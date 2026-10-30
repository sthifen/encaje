//! Monitor virtual en Niri (Wayland).
//!
//! Niri implementa por D-Bus `org.gnome.Mutter.ScreenCast` (`RecordMonitor`) y
//! `org.gnome.Mutter.DisplayConfig`, pero no `RecordVirtual` ni `RemoteDesktop`.
//! Para crear un segundo monitor sin necesitar un adaptador HDMI físico (dummy
//! plug):
//!
//! * Se busca un conector DRM libre (`disconnected`) en `/sys/class/drm` (p. ej.
//!   `HDMI-A-1` o `DP-1`) y se fuerza su encendido escribiendo `on` en su
//!   archivo `status`. Un proceso vigía restaura `detect` si `encaje` termina
//!   abruptamente.
//! * Se fija el modo a medida (`niri msg output <conector> custom-mode`) y la
//!   posición guardada (`niri msg output <conector> position set`).
//! * Se publica el monitor como nodo de PipeWire mediante `RecordMonitor` en
//!   `org.gnome.Mutter.ScreenCast`.
//! * La entrada del visor (ratón, táctil, rueda y teclado) se inyecta con un
//!   dispositivo virtual `/dev/uinput` cuyas coordenadas absolutas se escalan
//!   al rectángulo global de Niri.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::watch;
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::capture::Capture;

use super::layout::{self, Offset, Rect, VirtualMonitor};
use super::{BTN_LEFT, InputEvent, VirtualDisplay, button_code, clamp01};

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
    fn start(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn record_monitor(
        &self,
        connector: &str,
        properties: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;
    #[zbus(signal)]
    fn closed(&self) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.ScreenCast.Stream",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait ScreenCastStream {
    #[zbus(signal, name = "PipeWireStreamAdded")]
    fn pipewire_stream_added(&self, node_id: u32) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    #[zbus(signal)]
    fn monitors_changed(&self) -> zbus::Result<()>;
}

// ── Salidas de Niri (`niri msg -j outputs`) ─────────────────────────────────

#[derive(Debug, Deserialize)]
struct NiriOutput {
    name: String,
    logical: Option<NiriLogical>,
}

#[derive(Debug, Deserialize)]
struct NiriLogical {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

async fn niri_msg(args: &[&str]) -> Result<Vec<u8>> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("niri")
            .arg("msg")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("niri msg no respondió")?
    .context("no se pudo ejecutar niri msg")?;
    if !out.status.success() {
        bail!(
            "niri msg {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

async fn query_outputs() -> Result<HashMap<String, NiriOutput>> {
    let raw = niri_msg(&["-j", "outputs"]).await?;
    Ok(serde_json::from_slice(&raw)?)
}

/// Configura el modo a medida (redondeando el ancho a múltiplo de 8 como exige
/// el generador CVT de Niri) respetando la escala que tenga el conector en Niri.
async fn set_mode(connector: &str, width: u32, height: u32) -> Result<(i32, i32)> {
    let w = (width & !7).max(320);
    let h = height.max(240);
    let mode = format!("{w}x{h}@60");
    niri_msg(&["output", connector, "custom-mode", &mode]).await?;
    Ok((w as i32, h as i32))
}

async fn set_position(connector: &str, x: i32, y: i32) -> Result<()> {
    niri_msg(&[
        "output",
        connector,
        "position",
        "set",
        &x.to_string(),
        &y.to_string(),
    ])
    .await?;
    Ok(())
}

// ── Conector DRM ────────────────────────────────────────────────────────────

fn is_usable_connector(name: &str) -> bool {
    !name.starts_with("eDP-")
        && !name.starts_with("LVDS-")
        && !name.starts_with("DSI-")
        && !name.starts_with("Writeback-")
        && !name.starts_with("Virtual-")
}

fn connector_priority(name: &str) -> (u8, &str) {
    if name.starts_with("HDMI-") {
        (0, name)
    } else if name.starts_with("DP-") {
        (1, name)
    } else {
        (2, name)
    }
}

/// Busca un conector DRM desconectado en `/sys/class/drm` (o el indicado en
/// `ENCAJE_OUTPUT`) y devuelve `(nombre_conector, ruta_status)`.
fn find_free_connector() -> Result<(String, PathBuf)> {
    let wanted = std::env::var("ENCAJE_OUTPUT").ok();
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir("/sys/class/drm").context("no se pudo leer /sys/class/drm")? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(dir_name) = file_name.to_str() else { continue };
        let Some((_, conn)) = dir_name.split_once('-') else { continue };
        if let Some(ref w) = wanted {
            if conn != w {
                continue;
            }
        } else if !is_usable_connector(conn) {
            continue;
        }
        let status_path = entry.path().join("status");
        let Ok(status) = std::fs::read_to_string(&status_path) else { continue };
        if status.trim() == "disconnected" || wanted.as_deref() == Some(conn) {
            candidates.push((conn.to_owned(), status_path));
        }
    }
    candidates.sort_by(|(a, _), (b, _)| connector_priority(a).cmp(&connector_priority(b)));
    candidates.into_iter().next().ok_or_else(|| {
        anyhow!(
            "no hay ningún conector DRM libre (HDMI/DP desconectado) en /sys/class/drm; \
             puedes forzar uno con ENCAJE_OUTPUT=<conector>"
        )
    })
}

fn write_connector_status(path: &Path, status: &str) -> Result<()> {
    std::fs::write(path, format!("{status}\n")).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow!(
                "sin permiso para escribir en {}: añade una regla udev \
                 (ACTION==\"add|change\", SUBSYSTEM==\"drm\", KERNEL==\"card[0-9]*-*\", \
                 RUN+=\"/bin/sh -c 'chgrp video /sys%p/status && chmod 0664 /sys%p/status'\")",
                path.display()
            )
        } else {
            anyhow!("no se pudo escribir en {}: {e}", path.display())
        }
    })
}

/// Lanza un proceso vigía independiente que restaura el conector a `detect`
/// en cuanto el proceso principal de `encaje` muere (incluso por `_exit` o
/// `SIGKILL`).
fn spawn_connector_watchdog(status_path: &Path) {
    let pid = std::process::id().to_string();
    let path = status_path.to_string_lossy().into_owned();
    let _ = std::process::Command::new("sh")
        .args([
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 1; done; echo detect > \"$2\" 2>/dev/null",
            "encaje-drm-watchdog",
            &pid,
            &path,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ── Geometría para entrada y disposición ────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Bounds {
    /// Rectángulo lógico del monitor virtual.
    virt: Rect,
    /// Rectángulo envolvente global de todas las pantallas activas en Niri.
    global: Rect,
}

fn compute_state(
    outputs: &HashMap<String, NiriOutput>,
    virt_conn: &str,
) -> Option<(VirtualMonitor, Bounds, String, Rect)> {
    let mut virt_rect = None;
    let mut physical: Vec<(&str, Rect)> = Vec::new();
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_r = i32::MIN;
    let mut max_b = i32::MIN;

    for out in outputs.values() {
        let Some(l) = &out.logical else { continue };
        if l.width <= 0 || l.height <= 0 {
            continue;
        }
        let r = Rect { x: l.x, y: l.y, w: l.width, h: l.height };
        min_x = min_x.min(r.x);
        min_y = min_y.min(r.y);
        max_r = max_r.max(r.right());
        max_b = max_b.max(r.bottom());
        if out.name == virt_conn {
            virt_rect = Some(r);
        } else {
            physical.push((out.name.as_str(), r));
        }
    }

    let virt = virt_rect?;
    physical.sort_by_key(|(name, r)| (r.x.abs() + r.y.abs(), *name));
    let primary = physical.first().map(|(_, r)| *r).unwrap_or(virt);
    let key = layout::layout_key(physical.iter().map(|(n, _)| *n));
    let vm = VirtualMonitor::new(virt_conn, virt, physical.iter().map(|(_, r)| *r));
    let global = Rect {
        x: min_x,
        y: min_y,
        w: (max_r - min_x).max(1),
        h: (max_b - min_y).max(1),
    };
    Some((vm, Bounds { virt, global }, key, primary))
}

/// Coloca el monitor virtual según la posición guardada en `layout.json`.
async fn snap_initial_position(virt_conn: &str, fallback_w: i32, fallback_h: i32) -> Result<()> {
    let outputs = query_outputs().await?;
    let (w, h) = outputs
        .get(virt_conn)
        .and_then(|o| o.logical.as_ref())
        .filter(|l| l.width > 0 && l.height > 0)
        .map(|l| (l.width, l.height))
        .unwrap_or((fallback_w, fallback_h));
    let physical: Vec<(&str, Rect)> = outputs
        .values()
        .filter(|o| o.name != virt_conn)
        .filter_map(|o| {
            let l = o.logical.as_ref()?;
            (l.width > 0 && l.height > 0)
                .then_some((o.name.as_str(), Rect { x: l.x, y: l.y, w: l.width, h: l.height }))
        })
        .collect();
    if physical.is_empty() {
        return Ok(());
    }
    let mut sorted = physical.clone();
    sorted.sort_by_key(|(name, r)| (r.x.abs() + r.y.abs(), *name));
    let primary = sorted[0].1;
    let key = layout::layout_key(physical.iter().map(|(n, _)| *n));
    let rects: Vec<Rect> = physical.iter().map(|(_, r)| *r).collect();
    let want = match layout::load_offsets().get(&key).copied() {
        Some(o) => (primary.x + o.dx, primary.y + o.dy),
        None => (rects.iter().map(Rect::right).max().unwrap_or(0), primary.y),
    };
    let (x, y) = layout::snap(&rects, w, h, want);
    set_position(virt_conn, x, y).await
}

// ── Dispositivo virtual `/dev/uinput` ───────────────────────────────────────

const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c5504;
const UI_SET_EVBIT: libc::c_ulong = 0x40045564;
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565;
const UI_SET_RELBIT: libc::c_ulong = 0x40045566;
const UI_SET_ABSBIT: libc::c_ulong = 0x40045567;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0x00;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_MAX_VAL: i32 = 65535;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    axisinfo: InputAbsinfo,
}

#[repr(C)]
struct RawInputEvent {
    tv_sec: libc::time_t,
    tv_usec: libc::suseconds_t,
    type_: u16,
    code: u16,
    value: i32,
}

struct UinputDevice {
    file: File,
    active_touch: Option<u32>,
}

impl UinputDevice {
    fn create() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("no se pudo abrir /dev/uinput (¿permisos del grupo input?)")?;
        let fd = file.as_raw_fd();

        unsafe {
            for ev in [EV_SYN as libc::c_int, EV_KEY as _, EV_REL as _, EV_ABS as _] {
                if libc::ioctl(fd, UI_SET_EVBIT, ev) < 0 {
                    bail!("ioctl UI_SET_EVBIT falló");
                }
            }
            for btn in [0x110, 0x111, 0x112] {
                libc::ioctl(fd, UI_SET_KEYBIT, btn as libc::c_int);
            }
            for key in 1..=248 {
                libc::ioctl(fd, UI_SET_KEYBIT, key as libc::c_int);
            }
            for rel in [REL_WHEEL as libc::c_int, REL_HWHEEL as _] {
                libc::ioctl(fd, UI_SET_RELBIT, rel);
            }
            for axis in [ABS_X, ABS_Y] {
                libc::ioctl(fd, UI_SET_ABSBIT, axis as libc::c_int);
                let abs = UinputAbsSetup {
                    code: axis,
                    axisinfo: InputAbsinfo {
                        value: -1,
                        minimum: 0,
                        maximum: ABS_MAX_VAL,
                        fuzz: 0,
                        flat: 0,
                        resolution: 0,
                    },
                };
                if libc::ioctl(fd, UI_ABS_SETUP, &abs) < 0 {
                    bail!("ioctl UI_ABS_SETUP falló");
                }
            }

            let mut name = [0u8; 80];
            let label = b"Encaje Virtual Input";
            name[..label.len()].copy_from_slice(label);
            let setup = UinputSetup {
                id: InputId {
                    bustype: 0x03, // BUS_USB
                    vendor: 0x1d6b,
                    product: 0x0104,
                    version: 1,
                },
                name,
                ff_effects_max: 0,
            };
            if libc::ioctl(fd, UI_DEV_SETUP, &setup) < 0 {
                bail!("ioctl UI_DEV_SETUP falló");
            }
            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                bail!("ioctl UI_DEV_CREATE falló");
            }
        }

        Ok(Self { file, active_touch: None })
    }

    fn write_events(&mut self, events: &[(u16, u16, i32)]) -> Result<()> {
        let mut buf = Vec::with_capacity((events.len() + 1) * std::mem::size_of::<RawInputEvent>());
        for &(type_, code, value) in events {
            let ev = RawInputEvent { tv_sec: 0, tv_usec: 0, type_, code, value };
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &ev as *const RawInputEvent as *const u8,
                    std::mem::size_of::<RawInputEvent>(),
                )
            };
            buf.extend_from_slice(bytes);
        }
        let syn = RawInputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };
        let syn_bytes = unsafe {
            std::slice::from_raw_parts(
                &syn as *const RawInputEvent as *const u8,
                std::mem::size_of::<RawInputEvent>(),
            )
        };
        buf.extend_from_slice(syn_bytes);
        self.file.write_all(&buf)?;
        Ok(())
    }

    fn move_abs(&mut self, bounds: Bounds, x: f64, y: f64) -> Result<()> {
        let gx = bounds.virt.x as f64 + clamp01(x) * bounds.virt.w as f64;
        let gy = bounds.virt.y as f64 + clamp01(y) * bounds.virt.h as f64;
        let nx = ((gx - bounds.global.x as f64) / bounds.global.w as f64).clamp(0.0, 1.0);
        let ny = ((gy - bounds.global.y as f64) / bounds.global.h as f64).clamp(0.0, 1.0);
        let ax = (nx * ABS_MAX_VAL as f64).round() as i32;
        let ay = (ny * ABS_MAX_VAL as f64).round() as i32;
        self.write_events(&[(EV_ABS, ABS_X, ax), (EV_ABS, ABS_Y, ay)])
    }
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

/// Convierte un X11 keysym (enviado por el visor web) en código de tecla evdev.
fn keysym_to_evdev(keysym: u32) -> Option<(u16, bool)> {
    // Letras mayúsculas ASCII → tecla + Shift si hiciera falta, aunque el
    // visor ya envía Shift por separado cuando procede.
    match keysym {
        0x0061..=0x007a => {
            let code = match keysym as u8 {
                b'a' => 30, b'b' => 48, b'c' => 46, b'd' => 32, b'e' => 18, b'f' => 33,
                b'g' => 34, b'h' => 35, b'i' => 23, b'j' => 36, b'k' => 37, b'l' => 38,
                b'm' => 50, b'n' => 49, b'o' => 24, b'p' => 25, b'q' => 16, b'r' => 19,
                b's' => 31, b't' => 20, b'u' => 22, b'v' => 47, b'w' => 17, b'x' => 45,
                b'y' => 21, b'z' => 44,
                _ => return None,
            };
            Some((code, false))
        }
        0x0041..=0x005a => keysym_to_evdev(keysym + 32).map(|(c, _)| (c, true)),
        0x0031..=0x0039 => Some(((keysym - 0x0031 + 2) as u16, false)),
        0x0030 => Some((11, false)),
        0x0020 => Some((57, false)), // Space
        0x002d => Some((12, false)), // -
        0x003d => Some((13, false)), // =
        0x005b => Some((26, false)), // [
        0x005d => Some((27, false)), // ]
        0x005c => Some((43, false)), // \
        0x003b => Some((39, false)), // ;
        0x0027 => Some((40, false)), // '
        0x0060 => Some((41, false)), // `
        0x002c => Some((51, false)), // ,
        0x002e => Some((52, false)), // .
        0x002f => Some((53, false)), // /
        // Símbolos con Shift en teclado US/ES habitual
        0x0021 => Some((2, true)),  // !
        0x0040 => Some((3, true)),  // @
        0x0023 => Some((4, true)),  // #
        0x0024 => Some((5, true)),  // $
        0x0025 => Some((6, true)),  // %
        0x005e => Some((7, true)),  // ^
        0x0026 => Some((8, true)),  // &
        0x002a => Some((9, true)),  // *
        0x0028 => Some((10, true)), // (
        0x0029 => Some((11, true)), // )
        0x005f => Some((12, true)), // _
        0x002b => Some((13, true)), // +
        0x007b => Some((26, true)), // {
        0x007d => Some((27, true)), // }
        0x007c => Some((43, true)), // |
        0x003a => Some((39, true)), // :
        0x0022 => Some((40, true)), // "
        0x007e => Some((41, true)), // ~
        0x003c => Some((51, true)), // <
        0x003e => Some((52, true)), // >
        0x003f => Some((53, true)), // ?
        0x00f1 => Some((39, false)), // ñ (posición física de ';' en ES)
        0x00d1 => Some((39, true)),  // Ñ
        // Teclas especiales X11
        0xff08 => Some((14, false)),  // BackSpace
        0xff09 => Some((15, false)),  // Tab
        0xff0d => Some((28, false)),  // Return
        0xff1b => Some((1, false)),   // Escape
        0xffff => Some((111, false)), // Delete
        0xff50 => Some((102, false)), // Home
        0xff51 => Some((105, false)), // Left
        0xff52 => Some((103, false)), // Up
        0xff53 => Some((106, false)), // Right
        0xff54 => Some((108, false)), // Down
        0xff55 => Some((104, false)), // Page_Up
        0xff56 => Some((109, false)), // Page_Down
        0xff57 => Some((107, false)), // End
        0xff63 => Some((110, false)), // Insert
        0xffe1 => Some((42, false)),  // Shift_L
        0xffe2 => Some((54, false)),  // Shift_R
        0xffe3 => Some((29, false)),  // Control_L
        0xffe4 => Some((97, false)),  // Control_R
        0xffe5 => Some((58, false)),  // Caps_Lock
        0xffe9 => Some((56, false)),  // Alt_L
        0xffea | 0xfe03 => Some((100, false)), // Alt_R / ISO_Level3_Shift
        0xffeb => Some((125, false)), // Super_L
        0xffec => Some((126, false)), // Super_R
        0xffbe..=0xffc7 => Some(((keysym - 0xffbe + 59) as u16, false)), // F1..F10
        0xffc8 => Some((87, false)),  // F11
        0xffc9 => Some((88, false)),  // F12
        _ => None,
    }
}

// ── Backend NiriDisplay ─────────────────────────────────────────────────────

pub struct NiriDisplay {
    connector: String,
    status_path: PathBuf,
    sc_session: ScreenCastSessionProxy<'static>,
    node_id: u32,
    closed: watch::Receiver<bool>,
    placed: watch::Receiver<Option<VirtualMonitor>>,
    bounds: Arc<Mutex<Option<Bounds>>>,
    uinput: Mutex<UinputDevice>,
    stopped: AtomicBool,
    watcher: tokio::task::JoinHandle<()>,
}

impl NiriDisplay {
    pub async fn create(width: u32, height: u32) -> Result<Self> {
        let uinput = UinputDevice::create()?;
        let (connector, status_path) = find_free_connector()?;

        // 1. Encender el conector DRM y arrancar el vigía de limpieza.
        write_connector_status(&status_path, "on")?;
        spawn_connector_watchdog(&status_path);

        let setup_res: Result<_> = async {
            // 2. Esperar a que Niri detecte el conector.
            let mut detected = false;
            for _ in 0..50 {
                if let Ok(outputs) = query_outputs().await
                    && outputs.contains_key(&connector)
                {
                    detected = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !detected {
                bail!("Niri no detectó el conector {connector} tras activarlo");
            }

            // 3. Configurar modo y posición inicial.
            let (w, h) = set_mode(&connector, width, height).await?;
            let _ = snap_initial_position(&connector, w, h).await;

            // 4. Iniciar screencast de PipeWire por D-Bus en Niri.
            let conn = zbus::Connection::session()
                .await
                .context("no se pudo conectar al bus de sesión D-Bus")?;
            let sc = ScreenCastProxy::new(&conn).await?;
            let sc_path = sc
                .create_session(HashMap::new())
                .await
                .context("Niri rechazó ScreenCast.CreateSession")?;
            let sc_session = ScreenCastSessionProxy::builder(&conn)
                .path(sc_path)?
                .build()
                .await?;
            let stream_path = sc_session
                .record_monitor(&connector, HashMap::from([("cursor-mode", Value::from(1u32))]))
                .await
                .with_context(|| format!("RecordMonitor({connector}) falló en Niri"))?;
            let stream = ScreenCastStreamProxy::builder(&conn)
                .path(stream_path.clone())?
                .build()
                .await?;

            let mut closed_sig = sc_session.receive_closed().await?;
            let (closed_tx, closed) = watch::channel(false);
            tokio::spawn(async move {
                closed_sig.next().await;
                let _ = closed_tx.send(true);
            });

            let mut added = stream.receive_pipewire_stream_added().await?;
            sc_session
                .start()
                .await
                .context("no se pudo iniciar la sesión de ScreenCast en Niri")?;
            let signal = tokio::time::timeout(Duration::from_secs(10), added.next())
                .await
                .context("Niri no publicó el nodo PipeWire a tiempo")?
                .context("la sesión se cerró antes de publicar el nodo")?;
            let node_id = signal.args()?.node_id;

            Ok((conn, sc_session, node_id, closed))
        }
        .await;

        let (conn, sc_session, node_id, closed) = match setup_res {
            Ok(v) => v,
            Err(e) => {
                let _ = write_connector_status(&status_path, "detect");
                return Err(e);
            }
        };

        tracing::info!(
            node_id,
            connector,
            "monitor virtual en Niri creado ({width}x{height})"
        );

        let (placed_tx, placed) = watch::channel(None);
        let bounds = Arc::new(Mutex::new(None));
        if let Ok(outputs) = query_outputs().await
            && let Some((vm, b, _, _)) = compute_state(&outputs, &connector)
        {
            *bounds.lock().unwrap() = Some(b);
            let _ = placed_tx.send(Some(vm));
        }

        let bounds_task = bounds.clone();
        let conn_name = connector.clone();
        let watcher = tokio::spawn(async move {
            watch_layout(conn, conn_name, placed_tx, bounds_task).await;
        });

        Ok(Self {
            connector,
            status_path,
            sc_session,
            node_id,
            closed,
            placed,
            bounds,
            uinput: Mutex::new(uinput),
            stopped: AtomicBool::new(false),
            watcher,
        })
    }
}

async fn watch_layout(
    conn: zbus::Connection,
    virt_conn: String,
    placed_tx: watch::Sender<Option<VirtualMonitor>>,
    bounds: Arc<Mutex<Option<Bounds>>>,
) {
    let dc = DisplayConfigProxy::new(&conn).await.ok();
    let mut changes = match &dc {
        Some(p) => p.receive_monitors_changed().await.ok(),
        None => None,
    };
    loop {
        if let Ok(outputs) = query_outputs().await
            && let Some((vm, b, key, primary)) = compute_state(&outputs, &virt_conn)
        {
            *bounds.lock().unwrap() = Some(b);
            if !key.is_empty() {
                layout::save_offset(
                    &key,
                    Offset {
                        dx: b.virt.x - primary.x,
                        dy: b.virt.y - primary.y,
                    },
                );
            }
            placed_tx.send_if_modified(|cur| {
                let changed = match (cur.as_ref(), Some(&vm)) {
                    (Some(c), Some(i)) => !c.same_place(i),
                    (c, i) => c.is_some() != i.is_some(),
                };
                if changed {
                    *cur = Some(vm);
                }
                changed
            });
        }
        match &mut changes {
            Some(s) => {
                if let Ok(None) = tokio::time::timeout(Duration::from_secs(2), s.next()).await {
                    changes = None;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            None => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

#[async_trait]
impl VirtualDisplay for NiriDisplay {
    fn node_id(&self) -> u32 {
        self.node_id
    }

    fn capture_follows_output(&self) -> bool {
        true
    }

    async fn resize(&self, width: u32, height: u32, _capture: &Capture) {
        tracing::info!(
            "cambiando el monitor virtual {} a {width}x{height}",
            self.connector
        );
        match set_mode(&self.connector, width, height).await {
            Ok((w, h)) => {
                let _ = snap_initial_position(&self.connector, w, h).await;
            }
            Err(e) => {
                tracing::warn!("no se pudo cambiar el tamaño del monitor virtual: {e:#}");
            }
        }
    }

    fn placed(&self) -> watch::Receiver<Option<VirtualMonitor>> {
        self.placed.clone()
    }

    async fn closed(&self) {
        let mut rx = self.closed.clone();
        let _ = rx.wait_for(|c| *c).await;
    }

    async fn stop(&self) {
        if self.stopped.swap(true, Ordering::Relaxed) {
            return;
        }
        self.watcher.abort();
        let _ = self.sc_session.stop().await;
        if let Err(e) = write_connector_status(&self.status_path, "detect") {
            tracing::warn!("no se pudo restaurar {}: {e:#}", self.status_path.display());
        } else {
            tracing::info!("conector {} restaurado a detect", self.connector);
        }
    }

    async fn inject(&self, ev: InputEvent, _size: (u32, u32)) -> Result<()> {
        let bounds = *self.bounds.lock().unwrap();
        let mut dev = self.uinput.lock().unwrap();
        match ev {
            InputEvent::Move { x, y } => {
                if let Some(b) = bounds {
                    dev.move_abs(b, x, y)?;
                }
            }
            InputEvent::Button { b, down } => {
                dev.write_events(&[(EV_KEY, button_code(b) as u16, down as i32)])?;
            }
            InputEvent::Wheel { dy, dx } => {
                let mut evs = Vec::new();
                if dy != 0 {
                    evs.push((EV_REL, REL_WHEEL, dy));
                }
                if dx != 0 {
                    evs.push((EV_REL, REL_HWHEEL, dx));
                }
                if !evs.is_empty() {
                    dev.write_events(&evs)?;
                }
            }
            InputEvent::TouchDown { id, x, y } => {
                if dev.active_touch.is_none() {
                    dev.active_touch = Some(id);
                    if let Some(b) = bounds {
                        dev.move_abs(b, x, y)?;
                    }
                    dev.write_events(&[(EV_KEY, BTN_LEFT as u16, 1)])?;
                }
            }
            InputEvent::TouchMove { id, x, y } => {
                if dev.active_touch == Some(id)
                    && let Some(b) = bounds
                {
                    dev.move_abs(b, x, y)?;
                }
            }
            InputEvent::TouchUp { id } => {
                if dev.active_touch == Some(id) {
                    dev.active_touch = None;
                    dev.write_events(&[(EV_KEY, BTN_LEFT as u16, 0)])?;
                }
            }
            InputEvent::Key { keysym, down } => {
                if let Some((code, shift)) = keysym_to_evdev(keysym) {
                    if shift && down {
                        dev.write_events(&[(EV_KEY, 42, 1), (EV_KEY, code, 1), (EV_KEY, 42, 0)])?;
                    } else {
                        dev.write_events(&[(EV_KEY, code, down as i32)])?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for NiriDisplay {
    fn drop(&mut self) {
        if !self.stopped.swap(true, Ordering::Relaxed) {
            self.watcher.abort();
            let _ = write_connector_status(&self.status_path, "detect");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_connectors() {
        assert!(is_usable_connector("HDMI-A-1"));
        assert!(is_usable_connector("DP-1"));
        assert!(!is_usable_connector("eDP-1"));
        assert!(!is_usable_connector("Writeback-1"));
    }

    #[test]
    fn maps_keysyms() {
        assert_eq!(keysym_to_evdev(b'a' as u32), Some((30, false)));
        assert_eq!(keysym_to_evdev(b'A' as u32), Some((30, true)));
        assert_eq!(keysym_to_evdev(0xff0d), Some((28, false)));
    }
}
