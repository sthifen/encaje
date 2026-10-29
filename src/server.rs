//! Servidor HTTP: visor estático, señalización WebRTC y WebSockets.
//!
//! * `POST /offer`      – WebRTC (LAN, el navegador no necesita contexto seguro).
//! * `GET  /ws/video`   – H.264 Annex-B crudo por WebSocket para decodificar con
//!   WebCodecs. Funciona sobre TCP, así que pasa por `adb reverse` (USB).
//! * `GET  /ws/input`   – eventos de ratón/táctil/teclado en JSON.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::{BufMut, BytesMut};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use webrtc::peer_connection::RTCSessionDescription;

use crate::capture::Capture;
use crate::backend::{InputEvent, VirtualDisplay};
use crate::encoder::{BitrateControl, ClientRate, EncodedFrame, KeyframeRequest};

const INDEX_HTML: &str = include_str!("../web/index.html");

#[derive(Clone)]
pub struct AppState {
    pub frames: broadcast::Sender<EncodedFrame>,
    pub keyreq: KeyframeRequest,
    pub rate: Arc<BitrateControl>,
    pub input: mpsc::UnboundedSender<InputEvent>,
    pub capture: Capture,
    pub display: Arc<dyn VirtualDisplay>,
    /// La resolución sigue a los visores (si no se fijó con --width/--height).
    pub auto_size: bool,
    /// La frecuencia sigue a la pantalla del visor (si no se fijó con --fps),
    /// hasta este máximo.
    pub auto_fps: Option<u32>,
}

pub async fn serve(state: AppState, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/info", get(info))
        .route("/offer", post(offer))
        .route("/ws/video", get(ws_video))
        .route("/ws/input", get(ws_input))
        .with_state(Arc::new(state));

    // Sin Nagle: los frames P y las confirmaciones son paquetes pequeños que
    // TCP retendría hasta 40 ms esperando juntar más (sobre todo por adb).
    let listener = axum::serve::ListenerExt::tap_io(listener, |tcp| {
        let _ = tcp.set_nodelay(true);
    });
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")],
        INDEX_HTML,
    )
}

async fn info(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    // CORS abierto: el lanzador de la app Android consulta desde file://.
    (
        [(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")],
        Json(json!({ "width": s.capture.geometry.get().0, "height": s.capture.geometry.get().1, "fps": s.capture.fps.load(std::sync::atomic::Ordering::Relaxed), "auto_size": s.auto_size })),
    )
}

async fn offer(
    State(s): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(offer): Json<RTCSessionDescription>,
) -> Response {
    tracing::info!("oferta WebRTC de {peer}");
    match crate::rtc_peer::answer(offer, peer.ip(), s.frames.subscribe(), s.keyreq.clone()).await
    {
        Ok(answer) => {
            crate::events::connected();
            Json(answer).into_response()
        }
        Err(e) => {
            tracing::warn!("WebRTC {peer}: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
    }
}

async fn ws_video(
    ws: WebSocketUpgrade,
    State(s): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        tracing::info!("visor WebSocket conectado: {peer}");
        crate::events::connected();
        stream_video(socket, s).await;
        tracing::info!("visor WebSocket desconectado: {peer}");
        crate::events::emit("disconnected");
    })
}

/// Cada mensaje binario: [flags u8 (bit0 = keyframe)][pts µs u64 BE][Annex-B].
///
/// El visor responde `{"ack": pts}` al recibir cada frame. Con eso se mide lo
/// que de verdad atraviesa el enlace (adb por USB, WiFi…) y se evita encolar:
/// si hay demasiado en vuelo se saltan frames y se baja el bitrate, en lugar
/// de acumular segundos de retraso en búferes de TCP/adb.
async fn stream_video(mut socket: WebSocket, s: Arc<AppState>) {
    let (width, height) = s.capture.geometry.get();
    let hello = json!({ "type": "config", "width": width, "height": height, "fps": s.capture.fps.load(std::sync::atomic::Ordering::Relaxed) });
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }
    let mut frames = s.frames.subscribe();
    let mut flow = Flow::new(s.rate.register(), s.rate.max_kbps());
    s.keyreq.request();
    let mut synced = false;
    let mut key_requested = true;
    let mut last_stats = Instant::now() - Duration::from_secs(60);
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(Message::Text(t))) => {
                    if t.as_str() == "keyframe" {
                        tracing::debug!("el visor pide un keyframe (resincronizar)");
                        synced = false;
                        key_requested = true;
                        s.keyreq.request();
                    } else if let Ok(Ack { ack }) = serde_json::from_str::<Ack>(&t) {
                        flow.on_ack(ack);
                    } else if let Ok(ViewerStats { stats }) = serde_json::from_str::<ViewerStats>(&t) {
                        // Lo que mide el visor: decodificar, dibujar, resincronizaciones.
                        // Como mucho cada 2 s y corto: lo manda un cliente cualquiera.
                        let text = stats.to_string();
                        if text.len() < 512 && last_stats.elapsed() > Duration::from_secs(2) {
                            last_stats = Instant::now();
                            tracing::info!("visor: {text}");
                        }
                    }
                }
                _ => {}
            },
            frame = frames.recv() => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("visor atrasado {n} frames, resincronizando");
                        synced = false;
                        key_requested = false;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                flow.tick();
                if flow.congested() {
                    // Saltar este frame rompe la cadena de referencias: hará
                    // falta un keyframe, pero sólo cuando se vacíe el enlace.
                    flow.on_skip();
                    synced = false;
                    key_requested = false;
                    continue;
                }
                if !synced && !frame.keyframe {
                    if !key_requested && flow.drained() {
                        s.keyreq.request();
                        key_requested = true;
                    }
                    continue;
                }
                synced = true;
                let mut buf = BytesMut::with_capacity(frame.data.len() + 9);
                buf.put_u8(frame.keyframe as u8);
                buf.put_u64(frame.pts_us);
                buf.extend_from_slice(&frame.data);
                flow.on_send(frame.pts_us, buf.len());
                if socket.send(Message::Binary(buf.freeze())).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct Ack {
    ack: u64,
}

#[derive(serde::Deserialize)]
struct ViewerStats {
    stats: serde_json::Value,
}

/// Control de congestión de un visor WebSocket, guiado por el retraso
/// (como el de WebRTC): el retraso envío→ack crece antes de perder nada, así
/// que se baja el bitrate en cuanto sube y sólo se saltan frames si se dispara.
struct Flow {
    rate: ClientRate,
    max_kbps: u32,
    /// (pts, bytes, enviado) de los frames sin confirmar.
    inflight: VecDeque<(u64, usize, Instant)>,
    inflight_bytes: usize,
    /// Activo desde el primer ack (un visor antiguo sin acks no se limita).
    acks_seen: bool,
    cap_kbps: u32,
    delivered: usize,
    window: Instant,
    goodput_kbps: f64,
    skipped_in_window: bool,
    /// Mayor retraso envío→confirmación en la ventana actual.
    max_delay: Duration,
}

impl Flow {
    /// Edad máxima del frame más antiguo sin confirmar antes de saltar frames.
    const MAX_DELAY: Duration = Duration::from_millis(300);
    /// Por encima: bajar el bitrate. Por debajo de la mitad: subirlo.
    const TARGET_DELAY: Duration = Duration::from_millis(80);

    fn new(rate: ClientRate, max_kbps: u32) -> Self {
        Self {
            rate,
            max_kbps,
            inflight: VecDeque::new(),
            inflight_bytes: 0,
            acks_seen: false,
            cap_kbps: max_kbps,
            delivered: 0,
            window: Instant::now(),
            goodput_kbps: 0.0,
            skipped_in_window: false,
            max_delay: Duration::ZERO,
        }
    }

    fn on_send(&mut self, pts: u64, bytes: usize) {
        self.inflight.push_back((pts, bytes, Instant::now()));
        self.inflight_bytes += bytes;
    }

    fn on_ack(&mut self, pts: u64) {
        self.acks_seen = true;
        while let Some(&(p, bytes, _)) = self.inflight.front() {
            if p > pts {
                break;
            }
            if let Some((_, _, sent)) = self.inflight.pop_front() {
                self.max_delay = self.max_delay.max(sent.elapsed());
            }
            self.inflight_bytes -= bytes;
            self.delivered += bytes;
        }
    }

    /// Bytes que caben en vuelo: lo que el enlace entrega en MAX_DELAY.
    fn budget(&self) -> usize {
        let kbps = self.cap_kbps.max(crate::encoder::MIN_KBPS) as f64;
        ((kbps * 1000.0 / 8.0 * Self::MAX_DELAY.as_secs_f64()) as usize).max(96 * 1024)
    }

    fn congested(&self) -> bool {
        self.acks_seen
            && (self.inflight_bytes > self.budget()
                || self.inflight.front().is_some_and(|f| f.2.elapsed() > Self::MAX_DELAY))
    }

    fn drained(&self) -> bool {
        !self.acks_seen || self.inflight_bytes < self.budget() / 2
    }

    /// Salto de frames = congestión fuerte: bajar a lo que de verdad se
    /// entrega (si se midió) y como mucho al 75 %.
    fn on_skip(&mut self) {
        if self.skipped_in_window {
            return; // una reducción por ventana
        }
        self.skipped_in_window = true;
        let cap = self.cap_kbps as f64;
        let new = (self.goodput_kbps * 0.85).max(cap * 0.6).min(cap * 0.75);
        self.set_cap(new as u32);
    }

    /// Cada 500 ms: medir lo entregado y ajustar según el retraso.
    fn tick(&mut self) {
        let dt = self.window.elapsed().as_secs_f64();
        if dt < 0.5 {
            return;
        }
        let measured = self.delivered as f64 * 8.0 / dt / 1000.0;
        self.goodput_kbps = if self.goodput_kbps == 0.0 {
            measured
        } else {
            0.6 * self.goodput_kbps + 0.4 * measured
        };
        if self.acks_seen && !self.skipped_in_window {
            if self.max_delay > Self::TARGET_DELAY {
                self.set_cap(self.cap_kbps * 85 / 100);
            } else if self.max_delay < Self::TARGET_DELAY / 2 && self.cap_kbps < self.max_kbps {
                self.set_cap(self.cap_kbps + (self.cap_kbps / 10).max(250));
            }
        }
        tracing::debug!(
            "visor: entregado {measured:.0} kbit/s, límite {} kbit/s, retraso máx {:?}, en vuelo {} KB",
            self.cap_kbps,
            self.max_delay,
            self.inflight_bytes / 1024
        );
        self.delivered = 0;
        self.max_delay = Duration::ZERO;
        self.window = Instant::now();
        self.skipped_in_window = false;
    }

    fn set_cap(&mut self, kbps: u32) {
        let kbps = kbps.clamp(crate::encoder::MIN_KBPS, self.max_kbps);
        if kbps != self.cap_kbps {
            tracing::debug!("visor: límite {kbps} kbit/s (entregado {:.0})", self.goodput_kbps);
        }
        self.cap_kbps = kbps;
        self.rate.set(kbps);
    }
}

async fn ws_input(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if let Message::Text(t) = msg {
                if let Ok(Resize { w, h, hz }) = serde_json::from_str::<Resize>(&t) {
                    request_size(&s, w, h);
                    if let Some(hz) = hz {
                        request_fps(&s, hz);
                    }
                    continue;
                }
                match serde_json::from_str::<InputEvent>(&t) {
                    Ok(ev) => {
                        let _ = s.input.send(ev);
                    }
                    Err(e) => tracing::debug!("evento de entrada inválido {t}: {e}"),
                }
            }
        }
    })
}

/// `{"t":"resize","w":…,"h":…}`: el visor pide un monitor de ese tamaño (su
/// pantalla en píxeles físicos × la escala de rendimiento elegida).
#[derive(serde::Deserialize)]
#[serde(tag = "t", rename = "resize")]
struct Resize {
    w: u32,
    h: u32,
    /// Frecuencia de la pantalla del visor, medida con requestAnimationFrame.
    #[serde(default)]
    hz: Option<f64>,
}

/// Frecuencias habituales de pantallas: la medida (con ruido) se lleva a la
/// más cercana, para no recrear el codificador por 59 vs. 60.
const REFRESH_RATES: [u32; 7] = [30, 48, 60, 75, 90, 120, 144];

fn request_fps(s: &AppState, hz: f64) {
    let Some(max) = s.auto_fps else { return };
    if !hz.is_finite() || hz <= 0.0 {
        return;
    }
    let near = REFRESH_RATES
        .iter()
        .copied()
        .min_by(|a, b| (*a as f64 - hz).abs().total_cmp(&(*b as f64 - hz).abs()))
        .unwrap_or(60);
    let fps = near.min(max);
    // Cambiar de frecuencia recrea el codificador (un keyframe): como mucho
    // cada 2 s, aunque un visor mande medidas que bailan. Lo que llega antes
    // no se pierde: se aplica la última petición al vencer el plazo.
    let mut p = FPS_PACING.lock().unwrap_or_else(|e| e.into_inner());
    let wait = p.last.map_or(Duration::ZERO, |t| Duration::from_secs(2).saturating_sub(t.elapsed()));
    if wait.is_zero() {
        p.queued = None;
        apply_fps(&mut p, &s.capture, fps);
        return;
    }
    if p.queued.replace(fps).is_none() {
        let capture = s.capture.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let mut p = FPS_PACING.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(fps) = p.queued.take() {
                apply_fps(&mut p, &capture, fps);
            }
        });
    }
}

struct FpsPacing {
    /// Último cambio aplicado.
    last: Option<Instant>,
    /// Petición esperando a que pase el plazo.
    queued: Option<u32>,
}

static FPS_PACING: std::sync::Mutex<FpsPacing> = std::sync::Mutex::new(FpsPacing { last: None, queued: None });

fn apply_fps(p: &mut FpsPacing, capture: &Capture, fps: u32) {
    if capture.fps.load(std::sync::atomic::Ordering::Relaxed) != fps {
        p.last = Some(Instant::now());
        tracing::info!("el visor pide {fps} fps (su pantalla o lo que decodifica)");
        capture.set_fps(fps);
    }
}

/// Límites de NVENC H.264 y un mínimo usable.
const MAX_SIDE: u32 = 4096;
const MIN_SIDE: u32 = 320;

fn request_size(s: &AppState, w: u32, h: u32) {
    if !s.auto_size {
        tracing::debug!("resolución fija: se ignora la petición {w}x{h}");
        return;
    }
    // Escalar (manteniendo la proporción) si excede el máximo; tamaños pares.
    let k = (MAX_SIDE as f64 / w.max(h) as f64).min(1.0);
    let even = |v: f64| ((v as u32).clamp(MIN_SIDE, MAX_SIDE)) & !1;
    let (w, h) = (even(w as f64 * k), even(h as f64 * k));
    if s.capture.geometry.get() != (w, h) {
        let (display, capture) = (s.display.clone(), s.capture.clone());
        tokio::spawn(async move { display.resize(w, h, &capture).await });
    }
}

