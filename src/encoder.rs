//! Codificación H.264 del frame más reciente y difusión a los clientes.
//!
//! Backends:
//! * **NVENC** (GStreamer `nvh264enc`): la GPU NVIDIA convierte BGRx→YUV y
//!   codifica; unos pocos ms por frame incluso a 2340×1080.
//! * **VA-API** (GStreamer `vah264enc`, Intel/AMD): mismo esquema. Best-effort.
//! * **OpenH264** (CPU): siempre disponible, válido hasta ~720p a 60 fps.
//!
//! En modo `auto` se prueba cada backend de hardware codificando un frame
//! real antes de usarlo; si falla, se pasa al siguiente.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::YUVSlices;
use openh264::{OpenH264API, Timestamp};
use tokio::sync::broadcast;

use crate::capture::{self, FrameSlot, PixelOrder, RawFrame};

/// Unidad de acceso H.264 en formato Annex-B (con start codes).
#[derive(Clone, Debug)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub keyframe: bool,
    /// Microsegundos desde el inicio de la sesión.
    pub pts_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EncoderKind {
    /// Hardware si está disponible (NVENC → VA-API), si no OpenH264
    Auto,
    Nvenc,
    Vaapi,
    Openh264,
}

#[derive(Clone)]
pub struct EncoderSettings {
    pub kind: EncoderKind,
    pub fps: u32,
    /// Bitrate actual; lo ajusta [`BitrateControl`] según la red.
    pub bitrate_kbps: u32,
    pub rate: Arc<BitrateControl>,
}

impl EncoderSettings {
    /// Sin IDR periódico: por WebSocket (TCP) no hay pérdidas, y un keyframe
    /// a destiempo es un pico enorme en un enlace lento. Los visores WebRTC
    /// piden keyframes ellos mismos (ver `rtc_peer.rs`).
    fn gop(&self) -> u32 {
        self.fps * 600
    }
}

/// Bitrate objetivo compartido. Cada visor con control de congestión publica
/// cuánto aguanta su enlace; el codificador usa el mínimo (acotado al máximo
/// configurado). Por USB 1.1 o WiFi saturado baja solo, en vez de acumular
/// segundos de retraso en los búferes.
pub struct BitrateControl {
    max_kbps: u32,
    caps: Mutex<HashMap<u64, u32>>,
    next_id: AtomicU64,
}

pub const MIN_KBPS: u32 = 800;

impl BitrateControl {
    pub fn new(max_kbps: u32) -> Arc<Self> {
        Arc::new(Self { max_kbps, caps: Mutex::default(), next_id: AtomicU64::new(0) })
    }

    pub fn max_kbps(&self) -> u32 {
        self.max_kbps
    }

    /// Registra un visor; al soltar el [`ClientRate`] deja de contar.
    pub fn register(self: &Arc<Self>) -> ClientRate {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        ClientRate { ctl: self.clone(), id }
    }

    fn target_kbps(&self) -> u32 {
        let caps = self.caps.lock().unwrap();
        caps.values().copied().min().unwrap_or(self.max_kbps).clamp(MIN_KBPS, self.max_kbps)
    }
}

pub struct ClientRate {
    ctl: Arc<BitrateControl>,
    id: u64,
}

impl ClientRate {
    pub fn set(&self, kbps: u32) {
        self.ctl.caps.lock().unwrap().insert(self.id, kbps);
    }
}

impl Drop for ClientRate {
    fn drop(&mut self) {
        self.ctl.caps.lock().unwrap().remove(&self.id);
    }
}

/// Bandera compartida: cualquier cliente nuevo (o que perdió frames) la
/// activa para que el siguiente frame sea IDR.
#[derive(Clone, Default)]
pub struct KeyframeRequest(Arc<AtomicBool>);

impl KeyframeRequest {
    pub fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
    fn pending(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Destino de los frames codificados. Se comparte con el hilo de streaming de
/// GStreamer, que emite en cuanto la GPU termina (sin bloquear la captura).
#[derive(Clone)]
struct Output {
    tx: broadcast::Sender<EncodedFrame>,
    frames: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    keyframes: Arc<AtomicU64>,
}

impl Output {
    fn emit(&self, data: Vec<u8>, keyframe: bool, pts: Duration) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        if keyframe {
            self.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.tx.send(EncodedFrame {
            data: Bytes::from(data),
            keyframe,
            pts_us: pts.as_micros() as u64,
        });
    }
}

trait Backend {
    fn name(&self) -> &'static str;
    fn matches(&self, f: &RawFrame) -> bool;
    /// Envía el frame al codificador. `false` si se descartó y hay que reintentar.
    fn encode(&mut self, f: &RawFrame, force_key: bool, pts: Duration) -> Result<bool>;
    /// Cambia el bitrate sin reiniciar. `false` si el backend no lo admite.
    fn set_bitrate(&mut self, _kbps: u32) -> bool {
        false
    }
}

// ── OpenH264 (CPU) ──────────────────────────────────────────────────────────

struct OpenH264Backend {
    enc: Encoder,
    size: (usize, usize),
    out: Output,
}

impl OpenH264Backend {
    fn new(s: &EncoderSettings, w: usize, h: usize, out: Output) -> Result<Self> {
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            // No soportados con contenido de pantalla (OpenH264 los desactiva y avisa).
            .adaptive_quantization(false)
            .background_detection(false)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(s.bitrate_kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(s.fps as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(s.gop()))
            // Sin skip, OpenH264 ignora el bitrate objetivo.
            .skip_frames(true);
        let enc = Encoder::with_api_config(OpenH264API::from_source(), cfg)?;
        Ok(Self { enc, size: (w, h), out })
    }
}

impl Backend for OpenH264Backend {
    fn name(&self) -> &'static str {
        "OpenH264 (CPU)"
    }

    fn matches(&self, f: &RawFrame) -> bool {
        self.size == (f.width, f.height)
    }

    fn encode(&mut self, f: &RawFrame, force_key: bool, pts: Duration) -> Result<bool> {
        if force_key {
            self.enc.force_intra_frame();
        }
        let yuv = capture::to_i420(f);
        let (y, u, v) = yuv.planes();
        let (w, h) = (yuv.width, yuv.height);
        let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
        let bs = self
            .enc
            .encode_at(&src, Timestamp::from_millis(pts.as_millis() as u64))?;
        let key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
        let data = bs.to_vec();
        if data.is_empty() {
            return Ok(false);
        }
        self.out.emit(data, key, pts);
        Ok(true)
    }
}

// ── GStreamer (hardware) ────────────────────────────────────────────────────

struct GstBackend {
    name: &'static str,
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    /// Codificador NVENC, cuyo bitrate se puede cambiar en marcha.
    nvenc: Option<gst::Element>,
    vbv: u32,
    size: (usize, usize),
    order: PixelOrder,
}

impl GstBackend {
    fn new(kind: EncoderKind, s: &EncoderSettings, f: &RawFrame, out: Output) -> Result<Self> {
        gst::init()?;
        let (w, h, fps, kbps, gop) = (f.width, f.height, s.fps, s.bitrate_kbps, s.gop());
        let vbv = vbv_kbits(kbps);
        let va_gop = gop.min(1024);
        // Cola de 2 frames que descarta los más viejos si la GPU se atrasa.
        let input = format!(
            "appsrc name=src is-live=true format=time do-timestamp=false \
             max-buffers=2 leaky-type=downstream \
             caps=video/x-raw,format={},width={w},height={h},framerate={fps}/1",
            f.order.gst_format()
        );
        // Un único filtro de caps: gst-launch no acepta dos seguidos.
        let caps = "stream-format=byte-stream,alignment=au";
        let output = "appsink name=sink sync=false emit-signals=false";
        let (name, desc) = match kind {
            // Constrained Baseline: lo que anuncia la señalización WebRTC.
            EncoderKind::Nvenc => (
                "NVENC (GPU NVIDIA)",
                format!(
                    "{input} ! nvh264enc name=enc preset=p1 tune=ultra-low-latency rc-mode=cbr \
                     bitrate={kbps} vbv-buffer-size={vbv} gop-size={gop} bframes=0 zerolatency=true \
                     repeat-sequence-header=true \
                     ! video/x-h264,profile=constrained-baseline,{caps} ! {output}"
                ),
            ),
            EncoderKind::Vaapi => (
                "VA-API (GPU)",
                format!(
                    "{input} ! videoconvert ! video/x-raw,format=NV12 \
                     ! vah264enc rate-control=cbr bitrate={kbps} key-int-max={va_gop} b-frames=0 \
                     target-usage=7 ! h264parse config-interval=-1 ! video/x-h264,{caps} ! {output}"
                ),
            ),
            _ => bail!("no es un backend GStreamer"),
        };
        let pipeline = gst::parse::launch(&desc)
            .with_context(|| format!("{name}: no disponible"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("no es un pipeline"))?;
        let src = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| anyhow!("appsrc no encontrado"))?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("appsink no encontrado"))?;

        let got_key = Arc::new(AtomicBool::new(false));
        let got_key_cb = got_key.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let Some(buf) = sample.buffer() else { return Ok(gst::FlowSuccess::Ok) };
                    let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let key = !buf.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    let pts = Duration::from_nanos(buf.pts().map_or(0, |t| t.nseconds()));
                    if key {
                        got_key_cb.store(true, Ordering::Relaxed);
                    }
                    out.emit(map.as_slice().to_vec(), key, pts);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.set_state(gst::State::Playing)?;

        let nvenc = pipeline.by_name("enc");
        let mut me = Self { name, pipeline, src, nvenc, vbv, size: (w, h), order: f.order };
        // Prueba real: el pipeline debe producir un keyframe con este frame.
        me.encode(f, true, Duration::ZERO)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !got_key.load(Ordering::Relaxed) {
            me.check_bus()?;
            if Instant::now() > deadline {
                bail!("{name}: el codificador no produjo salida");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(me)
    }

    fn check_bus(&self) -> Result<()> {
        let Some(bus) = self.pipeline.bus() else { return Ok(()) };
        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(e) = msg.view() {
                bail!("{}: {} ({:?})", self.name, e.error(), e.debug());
            }
        }
        Ok(())
    }
}

impl Backend for GstBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn matches(&self, f: &RawFrame) -> bool {
        self.size == (f.width, f.height) && self.order == f.order
    }

    fn set_bitrate(&mut self, kbps: u32) -> bool {
        let Some(enc) = &self.nvenc else { return false };
        enc.set_property("bitrate", kbps);
        // Cambiar el VBV reinicia NVENC (= keyframe): sólo si el bitrate se
        // ha alejado más de 2× del que lo fijó.
        let vbv = vbv_kbits(kbps);
        if vbv > self.vbv * 2 || vbv * 2 < self.vbv {
            enc.set_property("vbv-buffer-size", vbv);
            self.vbv = vbv;
        }
        true
    }

    fn encode(&mut self, f: &RawFrame, force_key: bool, pts: Duration) -> Result<bool> {
        self.check_bus()?;
        if force_key {
            let ev = gstreamer_video::DownstreamForceKeyUnitEvent::builder()
                .all_headers(true)
                .build();
            self.src.send_event(ev);
        }
        let mut buf = gst::Buffer::from_slice(f.data.clone());
        buf.get_mut()
            .expect("búfer recién creado")
            .set_pts(gst::ClockTime::from_nseconds(pts.as_nanos() as u64));
        self.src
            .push_buffer(buf)
            .map_err(|e| anyhow!("{}: push_buffer: {e:?}", self.name))?;
        Ok(true)
    }
}

impl Drop for GstBackend {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Búfer VBV de ~250 ms: acota el tamaño de cada frame (incluidos los IDR)
/// para que un keyframe no bloquee un enlace lento durante medio segundo.
fn vbv_kbits(kbps: u32) -> u32 {
    (kbps / 4).max(200)
}

fn create_backend(s: &EncoderSettings, f: &RawFrame, out: &Output) -> Result<Box<dyn Backend>> {
    let candidates = match s.kind {
        EncoderKind::Auto => vec![EncoderKind::Nvenc, EncoderKind::Vaapi, EncoderKind::Openh264],
        k => vec![k],
    };
    let mut last_err = anyhow!("sin codificadores");
    for kind in candidates {
        let r: Result<Box<dyn Backend>> = match kind {
            EncoderKind::Openh264 => {
                OpenH264Backend::new(s, f.width, f.height, out.clone()).map(|b| Box::new(b) as _)
            }
            _ => GstBackend::new(kind, s, f, out.clone()).map(|b| Box::new(b) as _),
        };
        match r {
            Ok(b) => {
                tracing::info!("codificador: {} {}x{}", b.name(), f.width, f.height);
                return Ok(b);
            }
            Err(e) => {
                tracing::info!("{kind:?} no disponible: {e:#}");
                last_err = e;
            }
        }
    }
    Err(last_err)
}

// ── Bucle principal ─────────────────────────────────────────────────────────

pub fn spawn(
    slot: Arc<FrameSlot>,
    tx: broadcast::Sender<EncodedFrame>,
    keyreq: KeyframeRequest,
    settings: EncoderSettings,
) {
    std::thread::Builder::new()
        .name("h264-encoder".into())
        .spawn(move || {
            if let Err(e) = run(slot, tx, keyreq, settings) {
                tracing::error!("codificador terminó con error: {e:#}");
                std::process::exit(1);
            }
        })
        .expect("no se pudo crear el hilo del codificador");
}

fn run(
    slot: Arc<FrameSlot>,
    tx: broadcast::Sender<EncodedFrame>,
    keyreq: KeyframeRequest,
    mut settings: EncoderSettings,
) -> Result<()> {
    let start = Instant::now();
    let mut backend: Option<Box<dyn Backend>> = None;
    let mut last: Option<RawFrame> = None;
    let out = Output {
        tx: tx.clone(),
        frames: Arc::default(),
        bytes: Arc::default(),
        keyframes: Arc::default(),
    };
    let mut stats = Stats::new(&out);
    // Mutter entrega frames al ritmo del compositor (p. ej. 60 Hz) y sólo
    // cuando hay cambios. Se limita a `fps` sin perder nunca el último frame:
    // si llega uno demasiado pronto queda pendiente y se codifica al vencer
    // el intervalo.
    let interval = Duration::from_secs_f64(1.0 / settings.fps.max(1) as f64);
    let mut last_encode = Instant::now() - interval;
    let mut pending = false;
    let mut last_rate_change = Instant::now();

    loop {
        let wait = if pending {
            interval.saturating_sub(last_encode.elapsed())
        } else {
            Duration::from_millis(100)
        };
        if let Some(f) = slot.take_timeout(wait) {
            last = Some(f);
            pending = true;
        }
        // Si un cliente pide keyframe con la pantalla quieta, se recodifica
        // el último frame.
        let due = pending && last_encode.elapsed() >= interval;
        if !due && !keyreq.pending() {
            continue;
        }
        let Some(frame) = last.as_ref() else { continue };

        if tx.receiver_count() == 0 {
            // Sin visores no se codifica; el primero que llegue recibirá un
            // IDR del último frame gracias a la petición de keyframe.
            keyreq.request();
            pending = false;
            continue;
        }

        if !backend.as_ref().is_some_and(|b| b.matches(frame)) {
            drop(backend.take()); // liberar la GPU antes de crear otro
            backend = Some(create_backend(&settings, frame, &out)?);
            keyreq.request();
        }
        let target = settings.rate.target_kbps();
        let enc = backend.as_mut().unwrap();
        if target.abs_diff(settings.bitrate_kbps) * 8 > settings.bitrate_kbps {
            // Cambio > 12 %: en vivo si se puede; si no (CPU), recrear como
            // mucho cada 2 s (el reinicio cuesta un IDR).
            if enc.set_bitrate(target) {
                tracing::info!("bitrate → {target} kbit/s");
                settings.bitrate_kbps = target;
            } else if last_rate_change.elapsed() > Duration::from_secs(2) {
                tracing::info!("bitrate → {target} kbit/s (reiniciando codificador)");
                settings.bitrate_kbps = target;
                last_rate_change = Instant::now();
                backend = None;
                continue;
            }
        }
        let enc = backend.as_mut().unwrap();

        let forced = keyreq.take();
        let pts = start.elapsed();
        let t_encode = Instant::now();
        let sent = match enc.encode(frame, forced, pts) {
            Ok(sent) => sent,
            Err(e) if settings.kind != EncoderKind::Openh264 => {
                tracing::warn!("{e:#}; se pasa a OpenH264");
                settings.kind = EncoderKind::Openh264;
                backend = None;
                keyreq.request();
                continue;
            }
            Err(e) => return Err(e),
        };
        last_encode = Instant::now();
        stats.encode_us += t_encode.elapsed().as_micros() as u64;
        stats.calls += 1;
        if !sent {
            // Descartado por el control de bitrate: se reintenta.
            if forced {
                keyreq.request();
            }
            continue;
        }
        pending = false;
        stats.maybe_report(&out, frame.captured);
    }
}

/// Estadísticas periódicas: frames capturados vs. enviados y coste medio.
struct Stats {
    since: Instant,
    captured0: u64,
    copy0: u64,
    frames0: u64,
    bytes0: u64,
    calls: u64,
    encode_us: u64,
}

impl Stats {
    fn new(out: &Output) -> Self {
        Self {
            since: Instant::now(),
            captured0: capture::CAPTURED.load(Ordering::Relaxed),
            copy0: capture::COPY_US.load(Ordering::Relaxed),
            frames0: out.frames.load(Ordering::Relaxed),
            bytes0: out.bytes.load(Ordering::Relaxed),
            calls: 0,
            encode_us: 0,
        }
    }

    fn maybe_report(&mut self, out: &Output, captured_at: Instant) {
        let secs = self.since.elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        let captured = capture::CAPTURED.load(Ordering::Relaxed) - self.captured0;
        let copy = capture::COPY_US.load(Ordering::Relaxed) - self.copy0;
        let frames = out.frames.load(Ordering::Relaxed) - self.frames0;
        let bytes = out.bytes.load(Ordering::Relaxed) - self.bytes0;
        tracing::info!(
            "captura {:.1} fps (copia {:.1} ms) → envío {:.1} fps (codificar {:.1} ms), \
             {:.2} Mbit/s, {} keyframes, edad del frame {:.1} ms",
            captured as f64 / secs,
            copy as f64 / captured.max(1) as f64 / 1000.0,
            frames as f64 / secs,
            self.encode_us as f64 / self.calls.max(1) as f64 / 1000.0,
            bytes as f64 * 8.0 / secs / 1e6,
            out.keyframes.load(Ordering::Relaxed),
            captured_at.elapsed().as_secs_f64() * 1000.0,
        );
        *self = Self::new(out);
    }
}
