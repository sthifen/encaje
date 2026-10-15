//! spacedesk-linux: monitor virtual para GNOME/Wayland transmitido a un
//! navegador o a la app Android, por WiFi (WebRTC) o por cable USB (ADB).
//!
//! Flujo: Mutter (monitor virtual) → PipeWire → H.264 (NVENC/VA-API/OpenH264) →
//! broadcast → { WebRTC | WebSocket+WebCodecs } → visor.

mod adb;
mod capture;
mod display;
mod encoder;
mod rtc_peer;
mod server;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::{broadcast, mpsc};

#[derive(Parser)]
#[command(version, about = "Monitor virtual estilo Spacedesk para Linux (GNOME/Wayland)")]
struct Args {
    /// Ancho del monitor virtual (par)
    #[arg(long, default_value_t = 1920)]
    width: u32,
    /// Alto del monitor virtual (par)
    #[arg(long, default_value_t = 1080)]
    height: u32,
    /// Fotogramas por segundo
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Bitrate de vídeo en kbit/s
    #[arg(long, default_value_t = 12000)]
    bitrate: u32,
    /// Codificador H.264
    #[arg(long, value_enum, default_value_t = encoder::EncoderKind::Auto)]
    encoder: encoder::EncoderKind,
    /// Dirección de escucha
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,
    /// Puerto HTTP
    #[arg(long, default_value_t = 8787)]
    port: u16,
    /// No configurar `adb reverse` automáticamente para móviles por USB
    #[arg(long)]
    no_usb: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // RUST_LOG permite más detalle, p. ej. RUST_LOG=debug.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "error,linux_spacedesk_clone=info".into());
    tracing_subscriber::fmt().with_target(false).with_env_filter(filter).init();
    let args = Args::parse();
    let (width, height) = (args.width & !1, args.height & !1);

    // Reservar el puerto antes de crear el monitor virtual.
    let addr = SocketAddr::new(args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("no se pudo escuchar en {addr}"))?;

    let env = display::Environment::detect();
    tracing::info!("sesión: {} / escritorio: {}", env.session_type, env.desktop);

    let display = Arc::new(display::VirtualDisplay::create(&env, width, height).await?);

    // Captura → codificador (hilos nativos) → broadcast a clientes (tokio).
    let slot = Arc::new(capture::FrameSlot::default());
    capture::spawn(display.node_id, width, height, args.fps, slot.clone());
    let (frames_tx, _) = broadcast::channel(8);
    let keyreq = encoder::KeyframeRequest::default();
    encoder::spawn(
        slot,
        frames_tx.clone(),
        keyreq.clone(),
        encoder::EncoderSettings {
            kind: args.encoder,
            fps: args.fps,
            bitrate_kbps: args.bitrate,
        },
    );

    // Entrada remota: se procesa en orden en una sola tarea.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let d = display.clone();
    tokio::spawn(async move {
        while let Some(ev) = input_rx.recv().await {
            if let Err(e) = d.inject(ev).await {
                tracing::debug!("inyección de entrada falló: {e}");
            }
        }
    });

    if !args.no_usb {
        tokio::spawn(adb::watch(args.port));
    }

    let state = server::AppState {
        frames: frames_tx,
        keyreq,
        input: input_tx,
        width,
        height,
        fps: args.fps,
    };
    tracing::info!("visor en http://{addr} (WiFi: usa la IP de este equipo)");

    tokio::select! {
        r = server::serve(state, listener) => r?,
        _ = tokio::signal::ctrl_c() => tracing::info!("cerrando…"),
    }
    display.stop().await;
    Ok(())
}
