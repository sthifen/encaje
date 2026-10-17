//! Eventos para la interfaz (`--events`): una línea JSON por evento en la
//! salida estándar, precedida de `@` para distinguirla del registro.
//!
//!   @{"t":"waiting"}                      esperando a la tablet
//!   @{"t":"connected","monitor":{…}}      tablet o visor conectado; `monitor`
//!                                         dice dónde está (para la animación)
//!   @{"t":"disconnected"}

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::watch;

use crate::monitors::VirtualMonitor;

static ENABLED: AtomicBool = AtomicBool::new(false);
static MONITOR: Mutex<Option<watch::Receiver<Option<VirtualMonitor>>>> = Mutex::new(None);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Dónde está el monitor virtual, para adjuntarlo a `connected`.
pub fn set_monitor(rx: watch::Receiver<Option<VirtualMonitor>>) {
    *MONITOR.lock().unwrap() = Some(rx);
}

pub fn emit(kind: &str) {
    send(serde_json::json!({ "t": kind }));
}

/// Anuncia la conexión cuando se sepa dónde está el monitor virtual (puede
/// tardar un momento si se acaba de crear) y haya dejado de moverse: al
/// conectar el visor el monitor cambia de tamaño y Mutter recoloca las
/// pantallas, y la animación saldría donde estaba el borde antes.
pub fn connected() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let Some(rx) = MONITOR.lock().unwrap().clone() else {
        send(serde_json::json!({ "t": "connected" }));
        return;
    };
    tokio::spawn(async move {
        let mut rx = rx;
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.wait_for(Option::is_some)).await;
        let settle = async {
            while let Ok(Ok(())) = tokio::time::timeout(Duration::from_millis(700), rx.changed()).await {}
        };
        let _ = tokio::time::timeout(Duration::from_secs(4), settle).await;
        let monitor = rx.borrow().clone();
        send(serde_json::json!({ "t": "connected", "monitor": monitor }));
    });
}

fn send(v: serde_json::Value) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "@{v}");
    let _ = out.flush();
}
