//! Escritorio de mentira para `--fuente-prueba`: no crea ningún monitor y
//! sólo cuenta la entrada recibida. Permite probar captura → codificación →
//! red con muchos visores en un contenedor sin GNOME ni KDE.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::watch;

use super::{InputEvent, VirtualDisplay, VirtualMonitor};

/// Eventos de entrada recibidos (los imprimen las estadísticas de prueba).
pub static INPUT_EVENTS: AtomicU64 = AtomicU64::new(0);

pub struct TestDisplay {
    placed: watch::Sender<Option<VirtualMonitor>>,
}

impl TestDisplay {
    pub fn new() -> Self {
        Self { placed: watch::channel(None).0 }
    }
}

#[async_trait]
impl VirtualDisplay for TestDisplay {
    fn node_id(&self) -> u32 {
        0
    }

    fn placed(&self) -> watch::Receiver<Option<VirtualMonitor>> {
        self.placed.subscribe()
    }

    async fn inject(&self, _ev: InputEvent, _size: (u32, u32)) -> Result<()> {
        INPUT_EVENTS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn closed(&self) {
        std::future::pending::<()>().await
    }

    async fn stop(&self) {}
}
