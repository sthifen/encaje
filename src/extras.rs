//! Más monitores: cada tablet que pide «otro monitor» recibe un proceso de
//! Encaje propio, con su monitor virtual, su codificador y su puerto, y su
//! visor salta allí. Así un fallo en uno no se lleva a los demás.
//!
//! El proceso se va solo cuando su tablet deja de usarlo (ver `main.rs`) y
//! muere con este, que le pasa la señal de cierre para que todo quede en orden.

use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep};

/// Monitores además del principal. Con 4 en total la GPU de un portátil
/// corriente aún va sobrada; con 6 ya pierde frames.
pub const MAX: usize = 3;

/// Lo que tarda como mucho un monitor nuevo en estar listo.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Los puertos de los monitores extra. Se salta `port + 1`: en 8788 escucha
/// otra aplicación habitual en estas tablets.
pub fn ports(port: u16) -> [u16; MAX] {
    std::array::from_fn(|i| port + 3 + i as u16)
}

struct Slot {
    client: String,
    child: Child,
}

pub struct Extras {
    slots: Mutex<[Option<Slot>; MAX]>,
    ports: [u16; MAX],
    /// Argumentos con que arrancó este proceso, para que los hijos codifiquen igual.
    args: Vec<String>,
    supported: bool,
}

/// Quita lo que cada hijo debe decidir por su cuenta.
fn inherited_args(args: impl Iterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut args = args.peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                args.next();
            }
            "--events" | "--no-usb" => {}
            _ if a.starts_with("--port=") => {}
            _ => out.push(a),
        }
    }
    out
}

impl Extras {
    pub fn new(port: u16, supported: bool) -> Self {
        Self {
            slots: Mutex::new(std::array::from_fn(|_| None)),
            ports: ports(port),
            args: inherited_args(std::env::args().skip(1)),
            supported,
        }
    }

    pub fn supported(&self) -> bool {
        self.supported
    }

    /// Monitores extra vivos.
    pub async fn count(&self) -> usize {
        let mut slots = self.slots.lock().await;
        reap(&mut slots);
        slots.iter().flatten().count()
    }

    /// El puerto del monitor de este visor; si no tiene, lo crea. `None` si
    /// no caben más o no arrancó.
    pub async fn add(&self, client: &str) -> Option<u16> {
        if !self.supported {
            return None;
        }
        let mut slots = self.slots.lock().await;
        reap(&mut slots);
        if let Some(i) = slots.iter().position(|s| s.as_ref().is_some_and(|s| s.client == client)) {
            return Some(self.ports[i]);
        }
        let i = slots.iter().position(Option::is_none)?;
        let port = self.ports[i];
        let child = match self.spawn(i, port) {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!("no se pudo crear otro monitor: {e}");
                return None;
            }
        };
        slots[i] = Some(Slot { client: client.into(), child });
        drop(slots);
        ready(port).await.then_some(port)
    }

    fn spawn(&self, slot: usize, port: u16) -> std::io::Result<Child> {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(&self.args)
            .args(["--no-usb", "--port", &port.to_string(), "--extra-monitor", &(slot + 1).to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        // SAFETY: prctl es seguro entre fork y exec. Tokio crea el hijo desde
        // un hilo de trabajo, que vive tanto como este proceso.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT);
                Ok(())
            });
        }
        tracing::info!("otro monitor en el puerto {port}");
        cmd.spawn()
    }
}

fn reap(slots: &mut [Option<Slot>; MAX]) {
    for slot in slots.iter_mut() {
        if slot.as_mut().is_some_and(|s| !matches!(s.child.try_wait(), Ok(None))) {
            *slot = None;
        }
    }
}

/// Espera a que el monitor nuevo acepte conexiones.
async fn ready(port: u16) -> bool {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inherited(args: &[&str]) -> Vec<String> {
        inherited_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn children_keep_the_encoder_settings_but_not_the_port() {
        assert_eq!(
            inherited(&["--events", "--port", "9000", "--encoder", "nvenc", "--no-usb", "--port=9001"]),
            ["--encoder", "nvenc"]
        );
    }

    #[test]
    fn ports_avoid_the_one_next_to_the_server() {
        assert_eq!(ports(8787), [8790, 8791, 8792]);
    }
}
