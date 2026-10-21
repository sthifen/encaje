//! Monitor virtual en KDE Plasma (KWin, Wayland).
//!
//! KWin no tiene API por D-Bus para esto, sino protocolos Wayland propios
//! (los que usa krfb-virtualmonitor):
//!
//! * `zkde_screencast_unstable_v1.stream_virtual_output` crea un monitor
//!   virtual y lo publica como nodo de PipeWire. Igual que Mutter, KWin le
//!   cambia el tamaño al que pida el consumidor (ver `capture.rs`).
//! * `org_kde_kwin_fake_input` mete ratón, táctil y teclado (por keysym) en
//!   coordenadas globales del escritorio.
//! * `wl_output` + `xdg_output` dicen dónde está cada pantalla, para pasar
//!   las coordenadas del visor a globales y para la animación.
//!
//! KWin 6.7 no cambia el tamaño del monitor al renegociar PipeWire (las
//! versiones nuevas sí): se cambia su modo con `kscreen-doctor`, y la
//! captura acepta el tamaño que venga (`capture_follows_output`).
//!
//! Los dos primeros están restringidos: KWin 6 sólo los da a programas con
//! un `.desktop` instalado cuyo `Exec` es su ejecutable y que los pide en
//! `X-KDE-Wayland-Interfaces` (así lo hace krfb). Como la ruta del AppImage
//! cambia en cada arranque, Encaje escribe ese archivo al empezar
//! (`register_desktop_file`).
//!
//! La conexión Wayland vive en su propio hilo. Al cerrarla (o si el proceso
//! muere) KWin quita el monitor virtual. KWin recuerda por el nombre dónde
//! lo pusiste (y su último modo) y al quitarlo repone las demás pantallas,
//! así que aquí no hace falta cuidar la disposición como en GNOME.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use tokio::sync::{oneshot, watch};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_callback, wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

use crate::capture::Capture;

use super::layout::{Rect, VirtualMonitor};
use super::{InputEvent, VirtualDisplay, button_code, clamp01};

#[allow(dead_code, unused_imports, non_upper_case_globals, non_camel_case_types, clippy::all)]
mod protocols {
    pub mod screencast {
        use wayland_client;
        use wayland_client::protocol::*;
        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
    }
    pub mod fake_input {
        use wayland_client;
        use wayland_client::protocol::*;
        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/fake-input.xml");
    }
}

use protocols::fake_input::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use protocols::screencast::zkde_screencast_stream_unstable_v1::{self as sc_stream, ZkdeScreencastStreamUnstableV1};
use protocols::screencast::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

/// Nombre del monitor virtual. Es fijo para que KWin recuerde la posición
/// entre sesiones. KWin le añade un prefijo: el conector es "Virtual-Encaje".
const OUTPUT_NAME: &str = "Encaje";

fn is_ours(connector: &str) -> bool {
    connector == OUTPUT_NAME || connector.strip_suffix(OUTPUT_NAME).is_some_and(|p| p.ends_with('-'))
}
const OUTPUT_DESCRIPTION: &str = "Tablet (Encaje)";
/// El cursor se dibuja dentro de la imagen.
const POINTER_EMBEDDED: u32 = 2;
/// `wl_pointer.axis`.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;
/// Una muesca de la rueda, en las unidades de `wl_pointer.axis`.
const WHEEL_STEP: f64 = 15.0;
/// Desde esta versión `fake_input` acepta keysyms (KWin 6.1).
const KEYSYM_SINCE: u32 = 6;
/// Lo que se pide a KWin en el `.desktop`.
const INTERFACES: &str = "zkde_screencast_unstable_v1,org_kde_kwin_fake_input";

/// Escribe (o actualiza) el `.desktop` oculto que da permiso a este
/// ejecutable para usar los protocolos restringidos, y refresca la base de
/// datos de aplicaciones de KDE (ksycoca), que es donde mira KWin.
fn register_desktop_file() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
        .join("applications");
    let path = dir.join("dev.encaje.kwin.desktop");
    // KWin parte el Exec como una línea de órdenes: comillas por si hay espacios.
    let content = format!(
        "[Desktop Entry]\nType=Application\nName=Encaje\nComment=Permisos de KWin para el monitor virtual\n\
         Exec=\"{}\"\nNoDisplay=true\nX-KDE-Wayland-Interfaces={INTERFACES}\n",
        exe.display()
    );
    if std::fs::read_to_string(&path).is_ok_and(|c| c == content) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, content)?;
    tracing::info!("permisos de KWin registrados en {}", path.display());
    let rebuilt = std::process::Command::new("kbuildsycoca6")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !rebuilt.is_ok_and(|s| s.success()) {
        tracing::warn!("no se pudo ejecutar kbuildsycoca6; KWin puede tardar en ver los permisos");
    }
    Ok(())
}

#[derive(Default)]
struct OutputInfo {
    name: Option<String>,
    position: (i32, i32),
    size: (i32, i32),
}

/// Estado del hilo Wayland.
struct State {
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: HashMap<u32, (wl_output::WlOutput, OutputInfo)>,
    created: Option<oneshot::Sender<Result<u32>>>,
    closed: watch::Sender<bool>,
    placed: watch::Sender<Option<VirtualMonitor>>,
    quit: Arc<AtomicBool>,
}

impl State {
    fn add_output(&mut self, qh: &QueueHandle<Self>, registry: &wl_registry::WlRegistry, name: u32, version: u32) {
        let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, name);
        if let Some(m) = &self.xdg_manager {
            m.get_xdg_output(&output, qh, name);
        }
        self.outputs.insert(name, (output, OutputInfo::default()));
    }

    /// Vuelve a calcular dónde está el monitor virtual.
    fn publish(&self) {
        let rect = |i: &OutputInfo| Rect { x: i.position.0, y: i.position.1, w: i.size.0, h: i.size.1 };
        let infos = self.outputs.values().map(|(_, i)| i).filter(|i| i.size.0 > 0);
        let (virt, physical): (Vec<&OutputInfo>, Vec<&OutputInfo>) =
            infos.partition(|i| i.name.as_deref().is_some_and(is_ours));
        let info = virt.first().map(|v| {
            VirtualMonitor::new(v.name.as_deref().unwrap_or(OUTPUT_NAME), rect(v), physical.iter().map(|i| rect(i)))
        });
        self.placed.send_if_modified(|cur| {
            let changed = match (cur.as_ref(), info.as_ref()) {
                (Some(c), Some(i)) => !c.same_place(i),
                (c, i) => c.is_some() != i.is_some(),
            };
            if changed {
                tracing::debug!("monitor virtual en KWin: {info:?}");
                *cur = info;
            }
            changed
        });
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            // El monitor virtual aparece como un wl_output nuevo.
            wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
                state.add_output(qh, registry, name, version);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some((o, _)) = state.outputs.remove(&name) {
                    if o.version() >= 3 {
                        o.release();
                    }
                    state.publish();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some((_, info)) = state.outputs.get_mut(id) else { return };
        match event {
            wl_output::Event::Name { name } => info.name = Some(name),
            wl_output::Event::Done => state.publish(),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(state: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, event: zxdg_output_v1::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some((_, info)) = state.outputs.get_mut(id) else { return };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => info.position = (x, y),
            zxdg_output_v1::Event::LogicalSize { width, height } => info.size = (width, height),
            zxdg_output_v1::Event::Name { name } => info.name = Some(name),
            zxdg_output_v1::Event::Done => state.publish(),
            _ => {}
        }
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(state: &mut Self, _: &ZkdeScreencastStreamUnstableV1, event: sc_stream::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            sc_stream::Event::Created { node } => {
                if let Some(tx) = state.created.take() {
                    let _ = tx.send(Ok(node));
                }
            }
            sc_stream::Event::Failed { error } => {
                if let Some(tx) = state.created.take() {
                    let _ = tx.send(Err(anyhow!("KWin no pudo crear el monitor virtual: {error}")));
                }
            }
            sc_stream::Event::Closed => {
                let _ = state.closed.send(true);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(_: &mut Self, _: &wl_callback::WlCallback, _: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

wayland_client::delegate_noop!(State: ignore ZkdeScreencastUnstableV1);
wayland_client::delegate_noop!(State: ignore OrgKdeKwinFakeInput);
wayland_client::delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);

/// Ejecuta `kscreen-doctor` (de libkscreen, viene con Plasma).
async fn kscreen_doctor(args: &[String]) -> Result<Vec<u8>> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("kscreen-doctor").args(args).kill_on_drop(true).output(),
    )
    .await
    .context("kscreen-doctor no respondió")?
    .context("no se pudo ejecutar kscreen-doctor")?;
    if !out.status.success() {
        bail!("kscreen-doctor {args:?}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Modo actual y el id de un modo `width`x`height` del monitor, si lo hay.
async fn modes(connector: &str, width: u32, height: u32) -> Result<((u64, u64), Option<String>)> {
    let json: serde_json::Value = serde_json::from_slice(&kscreen_doctor(&["-j".into()]).await?)?;
    let output = json["outputs"]
        .as_array()
        .and_then(|o| o.iter().find(|o| o["name"] == connector))
        .ok_or_else(|| anyhow!("kscreen-doctor no ve {connector}"))?;
    let size = |v: &serde_json::Value| (v["width"].as_u64().unwrap_or(0), v["height"].as_u64().unwrap_or(0));
    let current = size(&output["size"]);
    let id = output["modes"].as_array().into_iter().flatten().find_map(|m| {
        (size(&m["size"]) == (width as u64, height as u64)).then(|| m["id"].as_str().unwrap_or_default().to_owned())
    });
    Ok((current, id))
}

/// Pone el monitor virtual a `width`x`height`, añadiendo el modo si no existe.
async fn set_mode(connector: &str, width: u32, height: u32) -> Result<()> {
    let (current, id) = modes(connector, width, height).await?;
    if current == (width as u64, height as u64) {
        return Ok(());
    }
    let id = match id {
        Some(id) => id,
        None => {
            kscreen_doctor(&[format!("output.{connector}.addCustomMode.{width}.{height}.60000.full")]).await?;
            modes(connector, width, height).await?.1.ok_or_else(|| anyhow!("KWin no aceptó el modo {width}x{height}"))?
        }
    };
    kscreen_doctor(&[format!("output.{connector}.mode.{id}")]).await?;
    Ok(())
}

pub struct KdeDisplay {
    conn: Connection,
    qh: QueueHandle<State>,
    stream: ZkdeScreencastStreamUnstableV1,
    fake: OrgKdeKwinFakeInput,
    node_id: u32,
    closed: watch::Receiver<bool>,
    placed: watch::Receiver<Option<VirtualMonitor>>,
    quit: Arc<AtomicBool>,
    /// Para avisar una sola vez de que el teclado no está disponible.
    warned_keys: Mutex<bool>,
}

impl KdeDisplay {
    pub async fn create(width: u32, height: u32) -> Result<Self> {
        if let Err(e) = register_desktop_file() {
            tracing::warn!("no se pudo registrar el acceso directo para KWin: {e:#}");
        }
        let conn = Connection::connect_to_env().context("no se pudo conectar con KWin (WAYLAND_DISPLAY)")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registro Wayland")?;
        let qh = queue.handle();

        let screencast: ZkdeScreencastUnstableV1 = globals.bind(&qh, 2..=6, ()).map_err(|_| {
            anyhow!(
                "KWin no da acceso a zkde_screencast_unstable_v1. Hace falta Plasma 5.26 o \
                 posterior; si lo es, cierra y vuelve a abrir la sesión para que KWin vea \
                 ~/.local/share/applications/dev.encaje.kwin.desktop"
            )
        })?;
        let fake: OrgKdeKwinFakeInput = globals
            .bind(&qh, 4..=6, ())
            .map_err(|_| anyhow!("KWin no ofrece org_kde_kwin_fake_input (entrada remota)"))?;
        fake.authenticate("Encaje".into(), "Usar la tablet como segundo monitor".into());
        if fake.version() < KEYSYM_SINCE {
            tracing::warn!("KWin antiguo: el teclado del visor no funcionará (hace falta Plasma 6.1)");
        }

        let (created_tx, created_rx) = oneshot::channel();
        let (closed_tx, closed) = watch::channel(false);
        let (placed_tx, placed) = watch::channel(None);
        let quit = Arc::new(AtomicBool::new(false));
        let mut state = State {
            xdg_manager: globals.bind(&qh, 2..=3, ()).ok(),
            outputs: HashMap::new(),
            created: Some(created_tx),
            closed: closed_tx,
            placed: placed_tx,
            quit: quit.clone(),
        };
        // Las pantallas que ya existían.
        let registry = globals.registry().clone();
        globals.contents().with_list(|list| {
            for g in list.iter().filter(|g| g.interface == "wl_output") {
                state.add_output(&qh, &registry, g.name, g.version);
            }
        });

        let stream = if screencast.version() >= 4 {
            screencast.stream_virtual_output_with_description(
                OUTPUT_NAME.into(),
                OUTPUT_DESCRIPTION.into(),
                width as i32,
                height as i32,
                1.0,
                POINTER_EMBEDDED,
                &qh,
                (),
            )
        } else {
            screencast.stream_virtual_output(OUTPUT_NAME.into(), width as i32, height as i32, 1.0, POINTER_EMBEDDED, &qh, ())
        };
        conn.flush()?;

        std::thread::Builder::new()
            .name("kwin".into())
            .spawn(move || {
                while !state.quit.load(Ordering::Relaxed) {
                    if let Err(e) = queue.blocking_dispatch(&mut state) {
                        tracing::warn!("conexión con KWin perdida: {e}");
                        let _ = state.closed.send(true);
                        break;
                    }
                }
            })
            .context("no se pudo crear el hilo de Wayland")?;

        let node_id = tokio::time::timeout(std::time::Duration::from_secs(10), created_rx)
            .await
            .context("KWin no publicó el nodo PipeWire a tiempo")?
            .context("la conexión con KWin se cerró antes de publicar el nodo")??;
        tracing::info!(node_id, "monitor virtual creado ({width}x{height})");

        // KWin le pone el último modo que tuvo (lo recuerda por el nombre).
        let mut rx = placed.clone();
        tokio::spawn(async move {
            let placed = match tokio::time::timeout(std::time::Duration::from_secs(5), rx.wait_for(Option::is_some)).await {
                Ok(Ok(p)) => p.clone(),
                _ => None,
            };
            let Some(m) = placed else { return };
            if (m.rect.w, m.rect.h) != (width as i32, height as i32)
                && let Err(e) = set_mode(&m.connector, width, height).await
            {
                tracing::warn!("no se pudo poner el monitor virtual a {width}x{height}: {e:#}");
            }
        });

        Ok(Self {
            conn,
            qh,
            stream,
            fake,
            node_id,
            closed,
            placed,
            quit,
            warned_keys: Mutex::new(false),
        })
    }

    /// Coordenadas 0..1 del visor → coordenadas globales de KWin.
    fn global(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let placed = self.placed.borrow();
        let r = placed.as_ref()?.rect;
        Some((r.x as f64 + clamp01(x) * r.w as f64, r.y as f64 + clamp01(y) * r.h as f64))
    }
}

#[async_trait]
impl VirtualDisplay for KdeDisplay {
    fn node_id(&self) -> u32 {
        self.node_id
    }

    fn placed(&self) -> watch::Receiver<Option<VirtualMonitor>> {
        self.placed.clone()
    }

    fn capture_follows_output(&self) -> bool {
        true
    }

    async fn resize(&self, width: u32, height: u32, _capture: &Capture) {
        let Some(connector) = self.placed.borrow().as_ref().map(|m| m.connector.clone()) else { return };
        tracing::info!("cambiando el monitor virtual a {width}x{height}");
        if let Err(e) = set_mode(&connector, width, height).await {
            tracing::warn!("no se pudo cambiar el tamaño del monitor virtual: {e:#}");
        }
    }

    async fn closed(&self) {
        let mut rx = self.closed.clone();
        let _ = rx.wait_for(|c| *c).await;
    }

    async fn stop(&self) {
        if self.quit.swap(true, Ordering::Relaxed) {
            return;
        }
        self.stream.close();
        // Despierta al hilo para que vea `quit` y termine.
        self.conn.display().sync(&self.qh, ());
        let _ = self.conn.flush();
    }

    // Las coordenadas de KWin son globales, así que el tamaño que importa es
    // el del monitor en el escritorio, no el de la imagen.
    async fn inject(&self, ev: InputEvent, _size: (u32, u32)) -> Result<()> {
        let f = &self.fake;
        match ev {
            InputEvent::Move { x, y } => {
                let Some((x, y)) = self.global(x, y) else { return Ok(()) };
                f.pointer_motion_absolute(x, y);
            }
            InputEvent::Button { b, down } => f.button(button_code(b), down as u32),
            InputEvent::Wheel { dy, dx } => {
                if dy != 0 {
                    f.axis(AXIS_VERTICAL, dy as f64 * WHEEL_STEP);
                }
                if dx != 0 {
                    f.axis(AXIS_HORIZONTAL, dx as f64 * WHEEL_STEP);
                }
            }
            InputEvent::TouchDown { id, x, y } => {
                let Some((x, y)) = self.global(x, y) else { return Ok(()) };
                f.touch_down(id, x, y);
                f.touch_frame();
            }
            InputEvent::TouchMove { id, x, y } => {
                let Some((x, y)) = self.global(x, y) else { return Ok(()) };
                f.touch_motion(id, x, y);
                f.touch_frame();
            }
            InputEvent::TouchUp { id } => {
                f.touch_up(id);
                f.touch_frame();
            }
            InputEvent::Key { keysym, down } => {
                if f.version() >= KEYSYM_SINCE {
                    f.keyboard_keysym(keysym, down as u32);
                } else {
                    let mut warned = self.warned_keys.lock().unwrap();
                    if !*warned {
                        *warned = true;
                        bail!("este KWin no acepta teclas por keysym");
                    }
                }
            }
        }
        self.conn.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_virtual_output_name() {
        assert!(is_ours("Virtual-Encaje"));
        assert!(is_ours("Encaje"));
        assert!(!is_ours("Virtual-0"));
        assert!(!is_ours("NoEncaje"));
    }
}
