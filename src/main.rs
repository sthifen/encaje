//! Encaje: monitor virtual para GNOME y KDE Plasma (Wayland) transmitido a
//! un navegador o a la app Android, por WiFi (WebRTC) o por cable USB (ADB).
//!
//! Flujo: compositor (monitor virtual, ver `backend`) → PipeWire → H.264 (NVENC/VA-API/OpenH264) →
//! broadcast → { WebRTC | WebSocket+WebCodecs } → visor.
//!
//! Con `--input-only` no se transmite nada: la tablet sigue con su Android y
//! recibe el ratón y el teclado del PC (ver `input_only.rs`).

mod adb;
mod backend;
mod capture;
mod clipboard;
mod diagnose;
mod encoder;
mod events;
mod grab_window;
mod gui;
mod guide;
mod h264;
mod input_only;
mod memguard;
mod overlay;
mod rtc_peer;
mod server;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::{broadcast, mpsc};

#[derive(Parser)]
#[command(version, about = "Encaje: la tablet como segundo monitor o con el ratón y el teclado del PC (GNOME o KDE Plasma, Wayland)")]
struct Args {
    /// Ancho fijo del monitor virtual. Sin --width/--height la resolución
    /// se adapta a la pantalla de cada visor (y a su escala de rendimiento).
    #[arg(long, requires = "height")]
    width: Option<u32>,
    /// Alto fijo del monitor virtual
    #[arg(long, requires = "width")]
    height: Option<u32>,
    /// Fotogramas por segundo fijos. Sin --fps se usa la frecuencia de la
    /// pantalla del visor (60, 90, 120 Hz…), hasta --max-fps
    #[arg(long)]
    fps: Option<u32>,
    /// Máximo de fotogramas por segundo cuando siguen al visor
    #[arg(long, default_value_t = 120)]
    max_fps: u32,
    /// Bitrate máximo en kbit/s (baja solo si la red no da para más)
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
    /// No transmitir la pantalla: la tablet (por USB) sigue mostrando Android
    /// y el ratón y el teclado pasan a ella al entrar en el monitor virtual
    #[arg(long, visible_alias = "solo-entrada")]
    input_only: bool,
    /// Ventana para elegir el modo (la usa el acceso directo del menú)
    #[arg(long)]
    gui: bool,
    /// Escribe un informe para reportar un fallo (sistema, escritorio,
    /// codificadores, registro) y muestra el enlace para enviarlo
    #[arg(long, visible_alias = "diagnose")]
    diagnostico: bool,
    /// Sin escritorio: transmite una imagen sintética (pruebas de estrés y
    /// de rendimiento en contenedores)
    #[arg(long, visible_alias = "test-source", hide = true)]
    fuente_prueba: bool,
    /// Escribir eventos en JSON para la interfaz (líneas que empiezan por @)
    #[arg(long, hide = true)]
    events: bool,
    /// Animación de conexión (uso interno de la interfaz)
    #[arg(long, hide = true)]
    overlay: Option<String>,
}

fn main() -> Result<()> {
    // RUST_LOG permite más detalle, p. ej. RUST_LOG=debug. El registro va a
    // stderr: stdout queda para los eventos de la interfaz.
    //
    // Además, todo se añade a ~/.local/state/encaje/encaje.log: desde el menú
    // de aplicaciones nadie ve stderr, y es lo que lleva `--diagnostico`.
    let args = Args::parse();
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "error,encaje=info".into());
    // Las pruebas (`--fuente-prueba`) no ensucian el registro de verdad.
    let file = (!args.fuente_prueba).then(diagnose::open_log).flatten().map(|f| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_thread_names(true)
            .with_writer(std::sync::Mutex::new(f))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false).with_writer(std::io::stderr))
        .with(file)
        .init();
    diagnose::install_panic_hook();
    if args.diagnostico {
        return diagnose::run_cli();
    }
    tracing::info!(
        "Encaje {} ({}) pid {}",
        env!("CARGO_PKG_VERSION"),
        std::env::args().skip(1).collect::<Vec<_>>().join(" "),
        std::process::id()
    );
    // La interfaz y la animación usan su propio bucle (GTK / X11), sin tokio.
    if args.gui {
        return gui::run();
    }
    if let Some(spec) = &args.overlay {
        return overlay::run(spec);
    }
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(serve(args));
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    // Salir sin los destructores de las bibliotecas: el de NVENC (libnvcuvid)
    // se bloquea si el codificador sigue en marcha y el proceso no terminaría
    // nunca (la interfaz no podría cambiar de modo).
    let code = match &result {
        _ if memguard::tripped() => memguard::EXIT_CODE,
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("{e:#}");
            eprintln!("Error: {e:?}");
            1
        }
    };
    let _ = std::io::Write::flush(&mut std::io::stdout());
    unsafe { libc::_exit(code) }
}

async fn serve(args: Args) -> Result<()> {
    if args.events {
        events::enable();
    }
    memguard::spawn();
    let fixed = args.width.zip(args.height).map(|(w, h)| (w & !1, h & !1));
    let (width, height) = fixed.unwrap_or((1920, 1080));
    // Hasta que un visor diga su frecuencia, 60.
    let fps = args.fps.unwrap_or(60.min(args.max_fps)).clamp(1, 240);

    if args.input_only {
        let env = backend::Environment::detect();
        return input_only::run(&env, fixed).await;
    }

    // Reservar el puerto antes de crear el monitor virtual.
    let addr = SocketAddr::new(args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("no se pudo escuchar en {addr}"))?;

    let env = backend::Environment::detect();
    tracing::info!("sesión: {} / escritorio: {}", env.session_type, env.desktop);

    let display: Arc<dyn backend::VirtualDisplay> = if args.fuente_prueba {
        Arc::new(backend::test::TestDisplay::new())
    } else {
        backend::create(&env, width, height).await?.into()
    };
    events::set_monitor(display.placed());

    // Captura → codificador (hilos nativos) → broadcast a clientes (tokio).
    let slot = Arc::new(capture::FrameSlot::default());
    let capture = if args.fuente_prueba {
        capture::spawn_test(width, height, fps, slot.clone())
    } else {
        capture::spawn(display.node_id(), width, height, fps, display.capture_follows_output(), slot.clone(), true)
    };
    let (frames_tx, _) = broadcast::channel(8);
    let keyreq = encoder::KeyframeRequest::default();
    let rate = encoder::BitrateControl::new(args.bitrate);
    encoder::spawn(
        slot,
        frames_tx.clone(),
        keyreq.clone(),
        encoder::EncoderSettings {
            kind: args.encoder,
            fps: capture.fps.clone(),
            bitrate_kbps: args.bitrate,
            rate: rate.clone(),
        },
    );

    // Entrada remota: se procesa en orden en una sola tarea.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let d = display.clone();
    let geometry = capture.geometry.clone();
    tokio::spawn(async move {
        // Mutter aborta (y con él toda la sesión de GNOME) si recibe un
        // touch_down en un dedo que ya está abajo; pasa si se pierde un
        // touch_up (reconexión, dos clientes a la vez). Se lleva la cuenta
        // de los dedos abajo y se descarta o se completa lo que no cuadre.
        let mut touching = std::collections::HashSet::new();
        while let Some(ev) = input_rx.recv().await {
            let ev = match ev {
                backend::InputEvent::TouchDown { id, .. } if !touching.insert(id) => {
                    let _ = d.inject(backend::InputEvent::TouchUp { id }, geometry.get()).await;
                    ev
                }
                backend::InputEvent::TouchMove { id, .. } if !touching.contains(&id) => continue,
                backend::InputEvent::TouchUp { id } if !touching.remove(&id) => continue,
                ev => ev,
            };
            if let Err(e) = d.inject(ev, geometry.get()).await {
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
        rate,
        input: input_tx,
        capture,
        display: display.clone(),
        auto_size: fixed.is_none(),
        auto_fps: args.fps.is_none().then_some(args.max_fps),
    };
    tracing::info!("visor en http://{addr} (WiFi: usa la IP de este equipo)");

    let closed = tokio::select! {
        r = server::serve(state, listener) => r.map(|_| false),
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("cerrando…");
            Ok(false)
        }
        _ = display.closed() => Ok(true),
    };
    display.stop().await;
    if closed? {
        anyhow::bail!("{}", backend::CLOSED_MESSAGE);
    }
    Ok(())
}
