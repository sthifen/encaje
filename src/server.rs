//! Servidor HTTP: visor estático, señalización WebRTC y WebSockets.
//!
//! * `POST /offer`      – WebRTC (LAN, el navegador no necesita contexto seguro).
//! * `GET  /ws/video`   – H.264 Annex-B crudo por WebSocket para decodificar con
//!   WebCodecs. Funciona sobre TCP, así que pasa por `adb reverse` (USB).
//! * `GET  /ws/input`   – eventos de ratón/táctil/teclado en JSON.

use std::net::SocketAddr;
use std::sync::Arc;

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

use crate::display::InputEvent;
use crate::encoder::{EncodedFrame, KeyframeRequest};

const INDEX_HTML: &str = include_str!("../web/index.html");

#[derive(Clone)]
pub struct AppState {
    pub frames: broadcast::Sender<EncodedFrame>,
    pub keyreq: KeyframeRequest,
    pub input: mpsc::UnboundedSender<InputEvent>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub async fn serve(state: AppState, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/info", get(info))
        .route("/offer", post(offer))
        .route("/ws/video", get(ws_video))
        .route("/ws/input", get(ws_input))
        .with_state(Arc::new(state));

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
        Json(json!({ "width": s.width, "height": s.height, "fps": s.fps })),
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
        Ok(answer) => Json(answer).into_response(),
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
        stream_video(socket, s).await;
        tracing::info!("visor WebSocket desconectado: {peer}");
    })
}

/// Cada mensaje binario: [flags u8 (bit0 = keyframe)][pts µs u64 BE][Annex-B].
async fn stream_video(mut socket: WebSocket, s: Arc<AppState>) {
    let hello = json!({ "type": "config", "width": s.width, "height": s.height, "fps": s.fps });
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }
    let mut frames = s.frames.subscribe();
    s.keyreq.request();
    let mut synced = false;
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(Message::Text(t))) if t.as_str() == "keyframe" => {
                    synced = false;
                    s.keyreq.request();
                }
                _ => {}
            },
            frame = frames.recv() => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("visor atrasado {n} frames, resincronizando");
                        synced = false;
                        s.keyreq.request();
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                if !synced && !frame.keyframe {
                    continue;
                }
                synced = true;
                let mut buf = BytesMut::with_capacity(frame.data.len() + 9);
                buf.put_u8(frame.keyframe as u8);
                buf.put_u64(frame.pts_us);
                buf.extend_from_slice(&frame.data);
                if socket.send(Message::Binary(buf.freeze())).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn ws_input(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if let Message::Text(t) = msg {
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
