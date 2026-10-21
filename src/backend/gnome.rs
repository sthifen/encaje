//! Monitor virtual en GNOME/Wayland.
//!
//! Mutter expone por D-Bus las APIs privadas que usa
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
//! Mutter destruye el monitor virtual automáticamente. La disposición de
//! pantallas la cuida `mutter_layout.rs`.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::sync::watch;
use zbus::zvariant::{OwnedObjectPath, Value};

use super::{InputEvent, VirtualDisplay, VirtualMonitor, button_code, clamp01, mutter_layout};

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

pub struct GnomeDisplay {
    remote: RemoteDesktopSessionProxy<'static>,
    stream_path: String,
    node_id: u32,
    closed: watch::Receiver<bool>,
    keeper: Mutex<Option<mutter_layout::Keeper>>,
    placed: watch::Receiver<Option<VirtualMonitor>>,
}

impl GnomeDisplay {
    pub async fn create(width: u32, height: u32) -> Result<Self> {
        // Antes de crear el monitor: Mutter rehace la disposición al añadirlo.
        let before = mutter_layout::snapshot().await?;

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
        let (closed_tx, closed) = watch::channel(false);
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
        let keeper = mutter_layout::keep(before);
        Ok(Self {
            remote,
            stream_path: stream_path.to_string(),
            node_id,
            closed,
            placed: keeper.placed.clone(),
            keeper: Mutex::new(Some(keeper)),
        })
    }
}

#[async_trait]
impl VirtualDisplay for GnomeDisplay {
    fn node_id(&self) -> u32 {
        self.node_id
    }

    fn placed(&self) -> watch::Receiver<Option<VirtualMonitor>> {
        self.placed.clone()
    }

    async fn closed(&self) {
        let mut rx = self.closed.clone();
        let _ = rx.wait_for(|c| *c).await;
    }

    async fn stop(&self) {
        let _ = self.remote.stop().await;
        let keeper = self.keeper.lock().unwrap().take();
        if let Some(k) = keeper {
            k.restore().await;
        }
    }

    async fn inject(&self, ev: InputEvent, size: (u32, u32)) -> Result<()> {
        let (w, h) = (size.0 as f64, size.1 as f64);
        let s = self.stream_path.as_str();
        match ev {
            InputEvent::Move { x, y } => {
                self.remote
                    .notify_pointer_motion_absolute(s, clamp01(x) * w, clamp01(y) * h)
                    .await?
            }
            InputEvent::Button { b, down } => {
                self.remote.notify_pointer_button(button_code(b) as i32, down).await?
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
