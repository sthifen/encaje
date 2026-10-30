//! Guía de la primera vez: de «no sé qué es la depuración USB» a la tablet
//! conectada. La interfaz (`gui.rs`) la dibuja; aquí está qué paso toca.
//!
//! Los pasos avanzan solos según `adb devices`: el usuario sólo pulsa
//! «Siguiente» en el que no se puede detectar (opciones de desarrollador).

use std::process::Command;

/// Lo que dice `adb devices`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Adb {
    /// No hay `adb` en el PC.
    Missing,
    NoDevice,
    /// Conectada, pero sin aceptar el aviso de depuración USB.
    Unauthorized,
    Ready,
}

/// Qué hace el botón principal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Next {
    /// Pasar al siguiente paso.
    Go,
    /// Nada que pulsar: el paso termina solo (se espera a la tablet).
    Wait,
    /// Cerrar la guía.
    Finish,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Step {
    pub label: &'static str,
    pub title: &'static str,
    pub body: &'static str,
    pub hint: &'static str,
    /// Comando para copiar (instalar adb).
    pub cmd: Option<String>,
    pub next: Next,
    pub back: bool,
    /// Pasos completados, para los puntos de progreso (de 3).
    pub done: usize,
}

/// Lee `adb devices`. La tablet que más avanzó manda.
pub fn adb_state() -> Adb {
    match Command::new("adb").arg("devices").output() {
        Ok(out) => parse_devices(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => Adb::Missing,
    }
}

fn parse_devices(out: &str) -> Adb {
    let states: Vec<&str> = out.lines().skip(1).filter_map(|l| l.split_whitespace().nth(1)).collect();
    if states.contains(&"device") {
        Adb::Ready
    } else if states.contains(&"unauthorized") {
        Adb::Unauthorized
    } else {
        Adb::NoDevice
    }
}

/// Cómo instalar adb en esta distro (por `ID` / `ID_LIKE` de os-release).
pub fn install_cmd() -> String {
    let text = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .unwrap_or_default();
    install_cmd_for(&text)
}

fn install_cmd_for(os_release: &str) -> String {
    let get = |k: &str| {
        os_release
            .lines()
            .find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
            .map(|v| v.trim_matches('"').to_lowercase())
            .unwrap_or_default()
    };
    let ids = format!("{} {}", get("ID"), get("ID_LIKE"));
    let has = |id: &str| ids.split_whitespace().any(|w| w == id);
    if has("fedora") || has("rhel") {
        "sudo dnf install android-tools"
    } else if has("debian") || has("ubuntu") {
        "sudo apt install adb"
    } else if has("arch") {
        "sudo pacman -S android-tools"
    } else if has("suse") || has("opensuse") {
        "sudo zypper install android-tools"
    } else if has("nixos") {
        "nix-shell -p android-tools"
    } else {
        "instala el paquete android-tools (o adb) de tu distro"
    }
    .to_owned()
}

/// El paso que toca: el que eligió el usuario (`manual`, 0 o 1), salvo que
/// adb diga que la tablet ya va más adelante.
pub fn step(manual: usize, adb: Option<Adb>, install: &str) -> Step {
    let at = match adb {
        Some(Adb::Missing) => {
            return Step {
                label: "Antes de empezar",
                title: "Instala adb",
                body: "Encaje usa adb, la herramienta de Android, para hablar con la tablet por el cable. Instálalo con:",
                hint: "Cuando termine, la guía sigue sola.",
                cmd: Some(install.to_owned()),
                next: Next::Wait,
                back: false,
                done: 0,
            };
        }
        Some(Adb::Unauthorized) => 2,
        Some(Adb::Ready) => 3,
        Some(Adb::NoDevice) | None => manual.min(1),
    };
    let (label, title, body, hint, next) = match at {
        0 => (
            "Paso 1 de 3",
            "Activa las opciones de desarrollador",
            "En la tablet: Ajustes › Acerca de la tablet › Información de software. Toca 7 veces «Número de compilación».",
            "Si te pide el PIN, es normal. En Samsung está en «Información de software».",
            Next::Go,
        ),
        1 => (
            "Paso 2 de 3",
            "Activa la depuración USB",
            "Ajustes › Opciones de desarrollador › Depuración USB. Después conecta la tablet al PC con el cable.",
            "Esperando la tablet…",
            Next::Wait,
        ),
        2 => (
            "Paso 3 de 3",
            "Acepta el aviso en la tablet",
            "Sale «¿Permitir depuración USB?». Marca «Permitir siempre desde este equipo» y toca Permitir.",
            "¿No sale? Desconecta el cable y vuelve a conectarlo.",
            Next::Wait,
        ),
        _ => (
            "Listo",
            "La tablet está conectada",
            "Elige un modo: segundo monitor, o la tablet con el ratón y el teclado del PC.",
            "Puedes volver a ver esta guía con el botón ?.",
            Next::Finish,
        ),
    };
    Step { label, title, body, hint, cmd: None, next, back: at == 1, done: at }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_adb_devices() {
        assert_eq!(parse_devices("List of devices attached\n\n"), Adb::NoDevice);
        assert_eq!(parse_devices("List of devices attached\nR52W\tunauthorized\n"), Adb::Unauthorized);
        assert_eq!(
            parse_devices("List of devices attached\nHA25\tunauthorized\nR52W\tdevice usb:3-4 model:SM_X800\n"),
            Adb::Ready
        );
        assert_eq!(parse_devices("List of devices attached\nR52W\toffline\n"), Adb::NoDevice);
    }

    #[test]
    fn install_command_per_distro() {
        assert_eq!(install_cmd_for("ID=fedora\n"), "sudo dnf install android-tools");
        assert_eq!(install_cmd_for("ID=zorin\nID_LIKE=\"ubuntu debian\"\n"), "sudo apt install adb");
        assert_eq!(install_cmd_for("ID=endeavouros\nID_LIKE=arch\n"), "sudo pacman -S android-tools");
        assert_eq!(install_cmd_for("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n"), "sudo zypper install android-tools");
        assert!(install_cmd_for("").starts_with("instala"));
    }

    #[test]
    fn adb_moves_the_guide_forward() {
        // Sin tablet: manda el paso del usuario (el primero no se detecta).
        assert_eq!(step(0, None, "").title, "Activa las opciones de desarrollador");
        assert_eq!(step(1, Some(Adb::NoDevice), "").next, Next::Wait);
        assert!(step(1, Some(Adb::NoDevice), "").back);
        // La tablet aparece: se salta lo que ya hizo.
        assert_eq!(step(0, Some(Adb::Unauthorized), "").title, "Acepta el aviso en la tablet");
        let ready = step(0, Some(Adb::Ready), "");
        assert_eq!((ready.next, ready.done, ready.back), (Next::Finish, 3, false));
        // Sin adb no hay más pasos hasta instalarlo.
        let missing = step(1, Some(Adb::Missing), "sudo dnf install android-tools");
        assert_eq!(missing.cmd.as_deref(), Some("sudo dnf install android-tools"));
        assert_eq!(missing.next, Next::Wait);
    }
}
