//! Vigilante de memoria del sistema.
//!
//! Crear un monitor virtual hace que otros programas trabajen para él, y
//! alguno pierde memoria sin parar: el fondo animado de Waywallen (extensión
//! de GNOME) se come ~1 GB/s de memoria compartida en cuanto aparece el
//! monitor, sin que cuente en el RSS de nadie. En pocos segundos el equipo se
//! queda sin RAM y no responde ni al teclado. Si la memoria disponible se
//! desploma mientras Encaje está en marcha, se cierra el monitor virtual (con
//! eso se libera) y se avisa, en vez de dejar que se cuelgue el equipo.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Código de salida cuando salta el vigilante: la interfaz no relanza el
/// servidor (volvería a pasar lo mismo).
pub const EXIT_CODE: i32 = 3;

static TRIPPED: AtomicBool = AtomicBool::new(false);

/// Si el vigilante cerró Encaje.
pub fn tripped() -> bool {
    TRIPPED.load(Ordering::Relaxed)
}

/// (MemTotal, MemAvailable) en KiB.
fn meminfo() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

/// Si la extensión Waywallen está activa en GNOME.
fn waywallen_active() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.shell", "enabled-extensions"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("waywallen"))
}

const WAYWALLEN_HINT: &str = "La extensión Waywallen (fondo de pantalla animado) pierde memoria \
     sin parar con el monitor virtual de Encaje. Desactívala en Extensiones mientras uses Encaje.";

/// Avisa al arrancar de lo que se sabe que acaba con la memoria.
pub fn warn_known_leaks() {
    if waywallen_active() {
        tracing::warn!("{WAYWALLEN_HINT}");
    }
}

/// Salta si la memoria disponible baja del 12 % del total habiendo perdido al
/// menos una cuarta parte de lo que había al arrancar (un equipo que ya iba
/// justo no es culpa del monitor virtual).
fn exhausted(total: u64, baseline: u64, available: u64) -> bool {
    available < total * 12 / 100 && baseline.saturating_sub(available) > baseline / 4
}

/// Vigila la memoria cada 250 ms (crece hasta 1 GB/s). Si se agota, pide el
/// cierre ordenado (SIGINT, como Ctrl+C) y, si no da tiempo, sale a la fuerza.
pub fn spawn() {
    let Some((total, baseline)) = meminfo() else { return };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            let Some((_, available)) = meminfo() else { return };
            if !exhausted(total, baseline, available) {
                continue;
            }
            TRIPPED.store(true, Ordering::Relaxed);
            let mut msg = format!(
                "El sistema se está quedando sin memoria (quedan {} MB de {} MB): Encaje cierra \
                 el monitor virtual para que el equipo no se cuelgue.",
                available / 1024,
                total / 1024
            );
            if waywallen_active() {
                msg = format!("{msg} {WAYWALLEN_HINT}");
            }
            tracing::error!("{msg}");
            let _ = std::process::Command::new("notify-send")
                .args(["-u", "critical", "-a", "Encaje", "Encaje se detuvo", &msg])
                .spawn();
            unsafe { libc::kill(libc::getpid(), libc::SIGINT) };
            tokio::time::sleep(Duration::from_secs(4)).await;
            tracing::error!("el cierre ordenado no terminó a tiempo; saliendo");
            unsafe { libc::_exit(EXIT_CODE) }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trips_only_on_a_collapse() {
        let gb = 1024 * 1024;
        // 16 GB con 11 libres al arrancar.
        assert!(!exhausted(16 * gb, 11 * gb, 9 * gb));
        assert!(exhausted(16 * gb, 11 * gb, gb));
        // Un equipo que ya arrancó justo y se queda igual.
        assert!(!exhausted(4 * gb, gb / 3, gb / 3));
    }

    #[test]
    fn reads_meminfo() {
        let (total, available) = meminfo().unwrap();
        assert!(total > 0 && available <= total);
    }
}
