//! Detección del entorno gráfico y creación del monitor virtual.
//!
//! En GNOME/Wayland, Mutter expone por D-Bus las APIs privadas que usa
//! gnome-remote-desktop:
//!
//! * `org.gnome.Mutter.RemoteDesktop`: sesión de control remoto (inyección de
//!   ratón, táctil y teclado).
//! * `org.gnome.Mutter.ScreenCast`: `RecordVirtual` crea un monitor virtual
//!   *headless* que extiende el escritorio y publica su contenido como un nodo
//!   de PipeWire. La resolución del monitor la decide el formato que pida el
//!   consumidor de PipeWire (ver `capture.rs`).
//!
//! Ambas sesiones están ligadas a la conexión D-Bus: si el proceso muere,
//! Mutter destruye el monitor virtual automáticamente.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use zbus::zvariant::{OwnedObjectPath, Value};

const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

#[zbus::proxy(
    interface = "org.gnome.Mutter.RemoteDesktop",
    default_service = "org.gnome.Mutter.RemoteDesktop",
    default_path = "/org/gnome/Mutter/RemoteDesktop"
)]
trait RemoteDesktop {
    fn create_session(&self) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.RemoteDesktop.Session",
    default_service = "org.gnome.Mutter.RemoteDesktop"
)]
trait RemoteDesktopSession {
    fn start(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn notify_pointer_motion_absolute(&self, stream: &str, x: f64, y: f64) -> zbus::Result<()>;
    fn notify_pointer_button(&self, button: i32, state: bool) -> zbus::Result<()>;
    fn notify_pointer_axis_discrete(&self, axis: u32, steps: i32) -> zbus::Result<()>;
    fn notify_touch_down(&self, stream: &str, slot: u32, x: f64, y: f64) -> zbus::Result<()>;
    fn notify_touch_motion(&self, stream: &str, slot: u32, x: f64, y: f64) -> zbus::Result<()>;
    fn notify_touch_up(&self, slot: u32) -> zbus::Result<()>;
    fn notify_keyboard_keysym(&self, keysym: u32, state: bool) -> zbus::Result<()>;
    #[zbus(signal)]
    fn closed(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn session_id(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.ScreenCast",
    default_service = "org.gnome.Mutter.ScreenCast",
    default_path = "/org/gnome/Mutter/ScreenCast"
)]
trait ScreenCast {
    fn create_session(&self, properties: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.ScreenCast.Session",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait ScreenCastSession {
    fn record_virtual(&self, properties: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.ScreenCast.Stream",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait ScreenCastStream {
    #[zbus(signal, name = "PipeWireStreamAdded")]
    fn pipewire_stream_added(&self, node_id: u32) -> zbus::Result<()>;
}

/// Sesión gráfica detectada a partir de las variables de entorno.
#[derive(Debug)]
pub struct Environment {
    pub session_type: String,
    pub desktop: String,
}

impl Environment {
    pub fn detect() -> Self {
        let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                "wayland".into()
            } else if std::env::var_os("DISPLAY").is_some() {
                "x11".into()
            } else {
                "tty".into()
            }
        });
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        Self { session_type, desktop }
    }

    pub fn is_gnome_wayland(&self) -> bool {
        self.session_type == "wayland" && self.desktop.to_uppercase().contains("GNOME")
    }
}

/// Evento de entrada enviado por el visor (coordenadas normalizadas 0..1).
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum InputEvent {
    Move { x: f64, y: f64 },
    Button { b: u8, down: bool },
    Wheel { dy: i32, #[serde(default)] dx: i32 },
    TouchDown { id: u32, x: f64, y: f64 },
    TouchMove { id: u32, x: f64, y: f64 },
    TouchUp { id: u32 },
    Key { keysym: u32, down: bool },
}

pub struct VirtualDisplay {
    remote: RemoteDesktopSessionProxy<'static>,
    stream_path: String,
    pub node_id: u32,
    closed: tokio::sync::watch::Receiver<bool>,
}

impl VirtualDisplay {
    /// Crea el monitor virtual y devuelve el id del nodo PipeWire que lo emite.
    pub async fn create(env: &Environment, width: u32, height: u32) -> Result<Self> {
        if !env.is_gnome_wayland() {
            bail!(
                "Sesión detectada: {} / {:?}. El monitor virtual se crea con la API \
                 RecordVirtual de Mutter, que sólo existe en GNOME sobre Wayland.\n\
                 En X11 haría falta un conector falso (driver dummy o evdi + xrandr), \
                 lo cual no está implementado.",
                env.session_type,
                env.desktop
            );
        }

        let conn = zbus::Connection::session()
            .await
            .context("no se pudo conectar al bus de sesión D-Bus")?;

        // 1. Sesión de escritorio remoto (necesaria para inyectar entrada).
        let rd = RemoteDesktopProxy::new(&conn).await?;
        let rd_path = rd
            .create_session()
            .await
            .context("Mutter rechazó CreateSession (¿está activo gnome-remote-desktop en modo exclusivo?)")?;
        let remote = RemoteDesktopSessionProxy::builder(&conn)
            .path(rd_path)?
            .build()
            .await?;
        let session_id = remote.session_id().await?;

        // 2. Sesión de screencast enlazada a la de escritorio remoto.
        let sc = ScreenCastProxy::new(&conn).await?;
        let sc_path = sc
            .create_session(HashMap::from([(
                "remote-desktop-session-id",
                Value::from(session_id.as_str()),
            )]))
            .await?;
        let sc_session = ScreenCastSessionProxy::builder(&conn)
            .path(sc_path)?
            .build()
            .await?;

        // 3. Monitor virtual. cursor-mode 1 = cursor incrustado en los frames.
        let stream_path = sc_session
            .record_virtual(HashMap::from([
                ("cursor-mode", Value::from(1u32)),
                ("is-platform", Value::from(true)),
            ]))
            .await
            .context("RecordVirtual falló")?;
        let stream = ScreenCastStreamProxy::builder(&conn)
            .path(stream_path.clone())?
            .build()
            .await?;

        // Mutter cierra la sesión si pulsas "Dejar de compartir" en la barra
        // superior. Hay que suscribirse a las señales antes de arrancarla.
        let mut closed_signal = remote.receive_closed().await?;
        let (closed_tx, closed) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            closed_signal.next().await;
            let _ = closed_tx.send(true);
        });
        let mut added = stream.receive_pipewire_stream_added().await?;
        remote.start().await.context("no se pudo iniciar la sesión")?;
        let signal = tokio::time::timeout(std::time::Duration::from_secs(10), added.next())
            .await
            .context("Mutter no publicó el nodo PipeWire a tiempo")?
            .context("la sesión se cerró antes de publicar el nodo")?;
        let node_id = signal.args()?.node_id;

        tracing::info!(node_id, %stream_path, "monitor virtual creado ({width}x{height})");
        Ok(Self {
            remote,
            stream_path: stream_path.to_string(),
            node_id,
            closed,
        })
    }

    /// Termina cuando Mutter cierra la sesión por su cuenta.
    pub async fn closed(&self) {
        let mut rx = self.closed.clone();
        let _ = rx.wait_for(|c| *c).await;
    }

    pub async fn stop(&self) {
        let _ = self.remote.stop().await;
    }

    /// `size`: tamaño actual del monitor virtual (las coordenadas llegan 0..1).
    pub async fn inject(&self, ev: InputEvent, size: (u32, u32)) -> Result<()> {
        let (w, h) = (size.0 as f64, size.1 as f64);
        let s = self.stream_path.as_str();
        match ev {
            InputEvent::Move { x, y } => {
                self.remote
                    .notify_pointer_motion_absolute(s, clamp01(x) * w, clamp01(y) * h)
                    .await?
            }
            InputEvent::Button { b, down } => {
                let code = match b {
                    1 => BTN_MIDDLE,
                    2 => BTN_RIGHT,
                    _ => BTN_LEFT,
                };
                self.remote.notify_pointer_button(code, down).await?
            }
            InputEvent::Wheel { dy, dx } => {
                if dy != 0 {
                    self.remote.notify_pointer_axis_discrete(0, dy).await?;
                }
                if dx != 0 {
                    self.remote.notify_pointer_axis_discrete(1, dx).await?;
                }
            }
            InputEvent::TouchDown { id, x, y } => {
                self.remote
                    .notify_touch_down(s, id, clamp01(x) * w, clamp01(y) * h)
                    .await?
            }
            InputEvent::TouchMove { id, x, y } => {
                self.remote
                    .notify_touch_motion(s, id, clamp01(x) * w, clamp01(y) * h)
                    .await?
            }
            InputEvent::TouchUp { id } => self.remote.notify_touch_up(id).await?,
            InputEvent::Key { keysym, down } => {
                self.remote.notify_keyboard_keysym(keysym, down).await?
            }
        }
        Ok(())
    }
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}
