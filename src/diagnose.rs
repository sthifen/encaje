//! Registro persistente e informe de errores (`--diagnostico`).
//!
//! Cada proceso de Encaje (la interfaz y los servidores que lanza) añade su
//! registro a `~/.local/state/encaje/encaje.log`. El informe junta ese
//! registro con lo que hace falta para entender un fallo en otro equipo:
//! distro, escritorio, versiones de glibc/GTK/GStreamer/PipeWire, si el
//! compositor ofrece las APIs que usa Encaje, codificadores disponibles,
//! GPU y móviles conectados por adb.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use gstreamer as gst;

pub const ISSUES_URL: &str = "https://github.com/sthifen/encaje/issues/new";
/// El registro se rota al pasar de este tamaño (queda una copia `.1`).
const LOG_MAX: u64 = 2 * 1024 * 1024;

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"))
        .join("encaje")
}

pub fn log_path() -> PathBuf {
    state_dir().join("encaje.log")
}

/// Abre el registro para añadir (rotándolo si es grande). `None` si no se
/// puede escribir: entonces sólo se registra en stderr.
pub fn open_log() -> Option<File> {
    let path = log_path();
    fs::create_dir_all(path.parent()?).ok()?;
    if fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX) {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Los pánicos también al registro (el gancho por defecto sólo usa stderr,
/// que desde el menú de aplicaciones no ve nadie).
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!("pánico en el hilo {:?}: {info}\n{bt}", std::thread::current().name().unwrap_or("?"));
        default(info);
    }));
}

/// Ejecuta un programa con límite de tiempo; su salida (stdout+stderr) o el motivo del fallo.
fn run(prog: &str, args: &[&str]) -> String {
    let child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return format!("({prog}: {e})"),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return format!("({prog}: sin respuesta en 4 s)");
            }
        }
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut out);
    }
    out.trim().to_owned()
}

fn os_release() -> String {
    let text = fs::read_to_string("/etc/os-release")
        .or_else(|_| fs::read_to_string("/usr/lib/os-release"))
        .unwrap_or_default();
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
            .map(|v| v.trim_matches('"').to_owned())
            .unwrap_or_default()
    };
    // Zorin, Mint, Pop!_OS…: la base (ID_LIKE / UBUNTU_CODENAME) explica las versiones.
    format!("{} (id={}, base={}, ubuntu={})", get("PRETTY_NAME"), get("ID"), get("ID_LIKE"), get("UBUNTU_CODENAME"))
}

fn glibc_version() -> String {
    unsafe extern "C" {
        fn gnu_get_libc_version() -> *const libc::c_char;
    }
    unsafe { std::ffi::CStr::from_ptr(gnu_get_libc_version()) }.to_string_lossy().into_owned()
}

/// Propiedad `Version` de una interfaz D-Bus de sesión, o por qué no está.
fn dbus_version(dest: &str, path: &str) -> String {
    let out = run(
        "gdbus",
        &["call", "--session", "--dest", dest, "--object-path", path, "--method",
          "org.freedesktop.DBus.Properties.Get", dest, "Version"],
    );
    if out.starts_with("(<") {
        format!("sí, versión {}", out.trim_matches(|c| "(<>,)".contains(c)))
    } else if out.contains("InvalidArgs") || out.contains("UnknownProperty") {
        "sí".into() // el servicio está, pero no publica versión
    } else {
        format!("NO ({})", out.lines().next().unwrap_or(""))
    }
}

fn gpus() -> String {
    let mut v = Vec::new();
    for e in fs::read_dir("/sys/class/drm").into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev = e.path().join("device");
        let vendor = fs::read_to_string(dev.join("vendor")).unwrap_or_default();
        let driver = fs::read_link(dev.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let vendor = match vendor.trim() {
            "0x10de" => "NVIDIA",
            "0x8086" => "Intel",
            "0x1002" => "AMD",
            other => other,
        };
        v.push(format!("{name}: {vendor} ({driver})"));
    }
    if v.is_empty() { "(ninguna en /sys/class/drm)".into() } else { v.join(", ") }
}

fn gst_elements() -> String {
    if let Err(e) = gst::init() {
        return format!("GStreamer no inicia: {e}");
    }
    let mut s = format!("{} — ", gst::version_string());
    for el in ["nvh264enc", "vah264enc", "vaapih264enc", "x264enc", "videoconvert", "h264parse"] {
        let ok = gst::ElementFactory::find(el).is_some();
        let _ = write!(s, "{el}:{} ", if ok { "sí" } else { "no" });
    }
    s
}

fn log_tail(lines: usize) -> String {
    let Ok(mut f) = File::open(log_path()) else { return "(sin registro todavía)".into() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Texto completo del informe.
pub fn report() -> String {
    let env = crate::backend::Environment::detect();
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| "(sin definir)".into());
    let mut r = String::new();
    let _ = writeln!(r, "# Informe de Encaje {}\n", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(r, "## Sistema");
    let _ = writeln!(r, "- Distro: {}", os_release());
    let _ = writeln!(r, "- Núcleo: {}", run("uname", &["-srm"]));
    let _ = writeln!(r, "- glibc: {}", glibc_version());
    let _ = writeln!(r, "- AppImage: {}", std::env::var("APPIMAGE").unwrap_or_else(|_| "no".into()));
    let _ = writeln!(r, "- GPU: {}", gpus());
    let _ = writeln!(r, "\n## Sesión");
    let _ = writeln!(r, "- XDG_SESSION_TYPE={} XDG_CURRENT_DESKTOP={} DESKTOP_SESSION={}",
        var("XDG_SESSION_TYPE"), var("XDG_CURRENT_DESKTOP"), var("DESKTOP_SESSION"));
    let _ = writeln!(r, "- WAYLAND_DISPLAY={} DISPLAY={}", var("WAYLAND_DISPLAY"), var("DISPLAY"));
    let _ = writeln!(r, "- Detectado: {:?}", env.kind());
    let _ = writeln!(r, "\n## Compositor");
    let _ = writeln!(r, "- gnome-shell: {}", run("gnome-shell", &["--version"]));
    let _ = writeln!(r, "- Mutter ScreenCast: {}", dbus_version("org.gnome.Mutter.ScreenCast", "/org/gnome/Mutter/ScreenCast"));
    let _ = writeln!(r, "- Mutter RemoteDesktop: {}", dbus_version("org.gnome.Mutter.RemoteDesktop", "/org/gnome/Mutter/RemoteDesktop"));
    let _ = writeln!(r, "- Mutter DisplayConfig: {}", dbus_version("org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig"));
    let _ = writeln!(r, "- KWin: {}", run("kwin_wayland", &["--version"]));
    let _ = writeln!(r, "- kscreen-doctor: {}", if run("kscreen-doctor", &["--version"]).starts_with('(') { "no" } else { "sí" });
    let _ = writeln!(r, "\n## Bibliotecas");
    let _ = writeln!(r, "- GTK: {}.{}.{}", gtk4::major_version(), gtk4::minor_version(), gtk4::micro_version());
    let _ = writeln!(r, "- GStreamer: {}", gst_elements());
    let _ = writeln!(r, "- PipeWire: {}", run("pipewire", &["--version"]).lines().last().unwrap_or(""));
    let _ = writeln!(r, "\n## Móviles (adb)");
    let _ = writeln!(r, "```\n{}\n```", run("adb", &["devices", "-l"]));
    let _ = writeln!(r, "\n## Registro (últimas líneas de {})", log_path().display());
    let _ = writeln!(r, "```\n{}\n```", log_tail(150));
    r
}

/// Guarda el informe y devuelve su ruta.
pub fn write_report() -> std::io::Result<PathBuf> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = dir.join(format!("informe-{secs}.md"));
    fs::write(&path, report())?;
    Ok(path)
}

/// Enlace para abrir una incidencia en GitHub con el resumen ya escrito
/// (el informe completo se adjunta a mano: no cabe en una URL).
pub fn issue_url(report: &str) -> String {
    let summary: String = report.split("\n## Registro").next().unwrap_or(report).chars().take(5000).collect();
    let body = format!(
        "**Qué pasó:**\n\n\n**Qué esperabas:**\n\n\n<details><summary>Informe automático</summary>\n\n{summary}\n</details>\n\n\
         (Arrastra aquí el archivo informe-*.md completo, que incluye el registro.)"
    );
    format!("{ISSUES_URL}?title={}&body={}", encode("Fallo: "), encode(&body))
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// `encaje --diagnostico`: escribe el informe, lo muestra y da el enlace.
pub fn run_cli() -> anyhow::Result<()> {
    let path = write_report()?;
    let text = fs::read_to_string(&path)?;
    println!("{text}");
    println!("Informe guardado en: {}", path.display());
    println!("Para reportar el fallo, abre este enlace y adjunta el archivo:\n{}", issue_url(&text));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encoding() {
        assert_eq!(encode("a b/ñ"), "a%20b%2F%C3%B1");
    }

    #[test]
    fn report_has_sections() {
        let r = report();
        for s in ["## Sistema", "## Sesión", "## Compositor", "## Bibliotecas", "## Registro"] {
            assert!(r.contains(s), "falta {s}");
        }
        assert!(issue_url(&r).len() < 20_000);
    }
}
