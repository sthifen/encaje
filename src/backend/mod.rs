//! Lo que depende del escritorio: crear el monitor virtual (publicado como
//! un nodo de PipeWire), meter en él la entrada del visor y saber dónde está
//! colocado. Cada escritorio lo hace con sus propias APIs:
//!
//! * GNOME (`gnome.rs`): D-Bus privado de Mutter, el de gnome-remote-desktop.
//! * KDE Plasma (`kde.rs`): protocolos Wayland de KWin, los de krfb.
//!
//! El resto (captura, codificación, red, modo sólo entrada) es común.

mod gnome;
mod kde;
pub mod layout;
mod mutter_layout;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::watch;

use crate::capture::Capture;

pub use layout::VirtualMonitor;

/// Sesión gráfica detectada a partir de las variables de entorno.
#[derive(Debug)]
pub struct Environment {
    pub session_type: String,
    pub desktop: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    Gnome,
    Kde,
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

    /// El escritorio, si está soportado.
    pub fn kind(&self) -> Option<Desktop> {
        if self.session_type != "wayland" {
            return None;
        }
        // XDG_CURRENT_DESKTOP es una lista separada por ':' (p. ej. "ubuntu:GNOME").
        let names: Vec<String> = self.desktop.split(':').map(str::to_uppercase).collect();
        if names.iter().any(|d| d == "GNOME") {
            Some(Desktop::Gnome)
        } else if names.iter().any(|d| d == "KDE") {
            Some(Desktop::Kde)
        } else {
            None
        }
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

/// Botones del ratón (linux/input-event-codes.h).
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// Botón del visor (0 izquierdo, 1 central, 2 derecho) → código evdev.
pub fn button_code(b: u8) -> u32 {
    match b {
        1 => BTN_MIDDLE,
        2 => BTN_RIGHT,
        _ => BTN_LEFT,
    }
}

/// Un monitor virtual vivo. Al soltarlo sin `stop` lo quita el escritorio
/// cuando muere el proceso (las sesiones van ligadas a la conexión).
#[async_trait]
pub trait VirtualDisplay: Send + Sync {
    /// Nodo de PipeWire que emite la imagen del monitor. Su tamaño lo decide
    /// el formato que pida la captura (ver `capture.rs`).
    fn node_id(&self) -> u32;

    /// Si la captura acepta el tamaño que ofrezca el compositor (que lo
    /// cambia con `resize`) en lugar de imponerlo ella.
    fn capture_follows_output(&self) -> bool {
        false
    }

    /// Cambia el tamaño del monitor. En Mutter basta con que la captura
    /// pida el nuevo tamaño.
    async fn resize(&self, width: u32, height: u32, capture: &Capture) {
        capture.resize(width, height);
    }

    /// Dónde está el monitor y con qué pantalla física se toca (`None`
    /// hasta que el escritorio lo coloca).
    fn placed(&self) -> watch::Receiver<Option<VirtualMonitor>>;

    /// `size`: tamaño actual del monitor virtual (las coordenadas llegan 0..1).
    async fn inject(&self, ev: InputEvent, size: (u32, u32)) -> Result<()>;

    /// Termina cuando el escritorio cierra el monitor por su cuenta (p. ej.
    /// «Dejar de compartir» en la barra superior de GNOME).
    async fn closed(&self);

    /// Quita el monitor y repone la disposición de pantallas de antes.
    async fn stop(&self);
}

/// Para cuando `closed` termina.
pub const CLOSED_MESSAGE: &str =
    "el escritorio cerró el monitor virtual (en GNOME, se pulsó «Dejar de compartir»)";

/// Crea el monitor virtual con las APIs del escritorio actual.
pub async fn create(env: &Environment, width: u32, height: u32) -> Result<Box<dyn VirtualDisplay>> {
    match env.kind() {
        Some(Desktop::Gnome) => Ok(Box::new(gnome::GnomeDisplay::create(width, height).await?)),
        Some(Desktop::Kde) => Ok(Box::new(kde::KdeDisplay::create(width, height).await?)),
        None => bail!(
            "Sesión detectada: {} / {:?}. Encaje funciona en GNOME y en KDE Plasma, \
             las dos sobre Wayland.",
            env.session_type,
            env.desktop
        ),
    }
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(session: &str, desktop: &str) -> Environment {
        Environment { session_type: session.into(), desktop: desktop.into() }
    }

    #[test]
    fn detects_desktop() {
        assert_eq!(env("wayland", "GNOME").kind(), Some(Desktop::Gnome));
        assert_eq!(env("wayland", "ubuntu:GNOME").kind(), Some(Desktop::Gnome));
        assert_eq!(env("wayland", "KDE").kind(), Some(Desktop::Kde));
        assert_eq!(env("x11", "KDE").kind(), None);
        assert_eq!(env("wayland", "Hyprland").kind(), None);
    }
}
