//! Disposición de monitores (org.gnome.Mutter.DisplayConfig).
//!
//! Mutter da al monitor virtual un número de serie nuevo cada vez que se
//! crea, así que nunca encuentra una configuración guardada para él y aplica
//! la de por defecto: lo pone a la derecha de todo y enciende los monitores
//! que tenías apagados. Para evitarlo:
//!
//! * Antes de crear el monitor se guarda la configuración actual (`snapshot`).
//! * Cuando aparece, se vuelve a aplicar esa configuración y se coloca el
//!   monitor virtual donde lo dejaste la última vez (`keep`).
//! * Si lo mueves en Configuración → Pantallas, la posición se guarda en
//!   `~/.config/spacedesk-linux/layout.json`, una por cada combinación de
//!   monitores físicos (no es lo mismo el portátil solo que con el HDMI).
//! * Al quitar el monitor virtual Mutter vuelve a su disposición por defecto,
//!   así que se repone la que tenías (`Keeper::restore`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zbus::zvariant::{OwnedValue, Value};

type Props = HashMap<String, OwnedValue>;
type MonitorId = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type Monitor = (MonitorId, Vec<Mode>, Props);
type Logical = (i32, i32, f64, u32, bool, Vec<MonitorId>, Props);
type LogicalConfig<'a> = (i32, i32, f64, u32, bool, Vec<(&'a str, &'a str, HashMap<&'a str, Value<'a>>)>);

const VIRTUAL_VENDOR: &str = "MetaVendor";

#[zbus::proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    #[allow(clippy::type_complexity)]
    fn get_current_state(&self) -> zbus::Result<(u32, Vec<Monitor>, Vec<Logical>, Props)>;
    fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<LogicalConfig<'_>>,
        properties: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    fn monitors_changed(&self) -> zbus::Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    fn right(&self) -> i32 {
        self.x + self.w
    }
    fn bottom(&self) -> i32 {
        self.y + self.h
    }
    fn overlaps(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    /// Longitud del borde que comparten (0 si no se tocan).
    fn shared_edge(&self, o: &Rect) -> i32 {
        let vertical = (self.right() == o.x || o.right() == self.x)
            .then(|| self.bottom().min(o.bottom()) - self.y.max(o.y));
        let horizontal = (self.bottom() == o.y || o.bottom() == self.y)
            .then(|| self.right().min(o.right()) - self.x.max(o.x));
        vertical.into_iter().chain(horizontal).max().unwrap_or(0).max(0)
    }
}

/// Un monitor encendido, con todo lo necesario para volver a aplicarlo.
#[derive(Clone, Debug)]
struct Placed {
    connector: String,
    vendor: String,
    rect: Rect,
    scale: f64,
    transform: u32,
    primary: bool,
}

#[derive(Clone, Debug)]
pub struct State {
    serial: u32,
    placed: Vec<Placed>,
    /// Modo actual de cada conector.
    modes: HashMap<String, String>,
}

impl State {
    fn find(&self, connector: &str) -> Option<&Placed> {
        self.placed.iter().find(|p| p.connector == connector)
    }

    /// Clave de la combinación de monitores físicos encendidos.
    fn layout_key(&self) -> String {
        let mut c: Vec<&str> = self
            .placed
            .iter()
            .filter(|p| p.vendor != VIRTUAL_VENDOR)
            .map(|p| p.connector.as_str())
            .collect();
        c.sort();
        c.join("+")
    }
}

async fn proxy() -> Result<DisplayConfigProxy<'static>> {
    let conn = zbus::Connection::session().await?;
    Ok(DisplayConfigProxy::new(&conn).await?)
}

pub async fn snapshot() -> Result<State> {
    read(&proxy().await?).await
}

async fn read(p: &DisplayConfigProxy<'_>) -> Result<State> {
    let (serial, monitors, logical, props) = p.get_current_state().await?;
    // layout-mode 1 = lógico (posiciones en píxeles lógicos), 2 = físico.
    let logical_mode = props
        .get("layout-mode")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1)
        == 1;
    let mut modes = HashMap::new();
    let mut sizes = HashMap::new();
    for ((connector, ..), mode_list, _) in &monitors {
        for (id, w, h, _, _, _, mprops) in mode_list {
            if mprops.get("is-current").and_then(|v| bool::try_from(v).ok()) == Some(true) {
                modes.insert(connector.clone(), id.clone());
                sizes.insert(connector.clone(), (*w, *h));
            }
        }
    }
    let mut placed = Vec::new();
    for (x, y, scale, transform, primary, ids, _) in logical {
        for (connector, vendor, ..) in ids {
            let Some(&(mut w, mut h)) = sizes.get(&connector) else { continue };
            if transform % 2 == 1 {
                std::mem::swap(&mut w, &mut h);
            }
            if logical_mode {
                w = (w as f64 / scale).round() as i32;
                h = (h as f64 / scale).round() as i32;
            }
            placed.push(Placed { connector, vendor, rect: Rect { x, y, w, h }, scale, transform, primary });
        }
    }
    Ok(State { serial, placed, modes })
}

/// Monitor virtual y el borde de la pantalla física con la que se toca: es
/// donde se dibuja la animación de conexión.
#[derive(Clone, Debug, Serialize)]
pub struct VirtualMonitor {
    pub connector: String,
    pub rect: Rect,
    /// Pantalla física vecina, el lado de esa pantalla donde está la tablet
    /// ("left", "right", "top", "bottom") y el tramo compartido.
    pub neighbor: Option<(Rect, &'static str, i32, i32)>,
}

fn describe(state: &State, connector: &str) -> Option<VirtualMonitor> {
    let v = state.find(connector)?;
    let neighbor = state
        .placed
        .iter()
        .filter(|p| p.vendor != VIRTUAL_VENDOR)
        .max_by_key(|p| p.rect.shared_edge(&v.rect))
        .filter(|p| p.rect.shared_edge(&v.rect) > 0)
        .map(|p| {
            let (r, t) = (p.rect, v.rect);
            if t.right() == r.x {
                (r, "left", r.y.max(t.y), r.bottom().min(t.bottom()))
            } else if t.x == r.right() {
                (r, "right", r.y.max(t.y), r.bottom().min(t.bottom()))
            } else if t.bottom() == r.y {
                (r, "top", r.x.max(t.x), r.right().min(t.right()))
            } else {
                (r, "bottom", r.x.max(t.x), r.right().min(t.right()))
            }
        });
    Some(VirtualMonitor { connector: connector.to_owned(), rect: v.rect, neighbor })
}

// ---- Posiciones guardadas ----

/// Posición del monitor virtual relativa a la pantalla principal.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
struct Offset {
    dx: i32,
    dy: i32,
}

fn layout_file() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("spacedesk-linux").join("layout.json")
}

fn load_offsets() -> HashMap<String, Offset> {
    std::fs::read(layout_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_offset(key: &str, offset: Offset) {
    let mut all = load_offsets();
    if all.get(key) == Some(&offset) {
        return;
    }
    all.insert(key.to_owned(), offset);
    let path = layout_file();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    match serde_json::to_vec_pretty(&all) {
        Ok(ref json) if std::fs::write(&path, json).is_ok() => {
            tracing::info!("posición de la tablet guardada ({key}: {}, {})", offset.dx, offset.dy)
        }
        _ => tracing::warn!("no se pudo guardar {}", path.display()),
    }
}

// ---- Colocación ----

/// Busca la posición válida (pegada a algún monitor, sin solaparse) más
/// cercana a `want`. Mutter rechaza disposiciones con huecos o solapes.
fn snap(others: &[Rect], w: i32, h: i32, want: (i32, i32)) -> (i32, i32) {
    let mut best = None;
    for r in others {
        let ys = (r.y - h + 1, r.bottom() - 1);
        let xs = (r.x - w + 1, r.right() - 1);
        let candidates = [
            (r.right(), want.1.clamp(ys.0, ys.1)),
            (r.x - w, want.1.clamp(ys.0, ys.1)),
            (want.0.clamp(xs.0, xs.1), r.bottom()),
            (want.0.clamp(xs.0, xs.1), r.y - h),
        ];
        for (x, y) in candidates {
            let rect = Rect { x, y, w, h };
            if others.iter().any(|o| o.overlaps(&rect)) {
                continue;
            }
            let d = (x - want.0).pow(2) as i64 + (y - want.1).pow(2) as i64;
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, (x, y)));
            }
        }
    }
    best.map(|(_, p)| p).unwrap_or(want)
}

/// La configuración de antes (con lo apagado apagado) más el monitor virtual
/// en su sitio.
fn desired(before: &State, now: &State, virt: &Placed, offset: Option<Offset>) -> Vec<Placed> {
    let mut out: Vec<Placed> = before
        .placed
        .iter()
        .filter(|p| now.modes.contains_key(&p.connector) && p.connector != virt.connector)
        .filter(|p| p.vendor != VIRTUAL_VENDOR || now.find(&p.connector).is_some())
        .cloned()
        .collect();
    if out.is_empty() {
        return now.placed.clone();
    }
    let rects: Vec<Rect> = out.iter().map(|p| p.rect).collect();
    let primary = out.iter().find(|p| p.primary).unwrap_or(&out[0]).rect;
    let want = match offset {
        Some(o) => (primary.x + o.dx, primary.y + o.dy),
        None => (rects.iter().map(Rect::right).max().unwrap_or(0), primary.y),
    };
    let (x, y) = snap(&rects, virt.rect.w, virt.rect.h, want);
    out.push(Placed { rect: Rect { x, y, ..virt.rect }, scale: 1.0, transform: 0, primary: false, ..virt.clone() });
    normalize(&mut out);
    out
}

/// Mutter exige que la disposición empiece en (0, 0).
fn normalize(out: &mut [Placed]) {
    let min_x = out.iter().map(|p| p.rect.x).min().unwrap_or(0);
    let min_y = out.iter().map(|p| p.rect.y).min().unwrap_or(0);
    for p in out {
        p.rect.x -= min_x;
        p.rect.y -= min_y;
    }
}

/// La disposición de referencia sin el monitor virtual, con lo que sigue
/// conectado.
fn without_virtual(baseline: &State, now: &State) -> Vec<Placed> {
    let mut out: Vec<Placed> = baseline
        .placed
        .iter()
        .filter(|p| now.modes.contains_key(&p.connector))
        .filter(|p| p.vendor != VIRTUAL_VENDOR || now.find(&p.connector).is_some())
        .cloned()
        .collect();
    normalize(&mut out);
    out
}

fn same_layout(a: &[Placed], b: &[Placed]) -> bool {
    a.len() == b.len()
        && a.iter().all(|p| b.iter().any(|q| q.connector == p.connector && q.rect == p.rect && q.primary == p.primary))
}

async fn apply(p: &DisplayConfigProxy<'_>, now: &State, layout: &[Placed]) -> Result<()> {
    let configs: Vec<LogicalConfig> = layout
        .iter()
        .filter_map(|m| {
            let mode = now.modes.get(&m.connector)?;
            Some((m.rect.x, m.rect.y, m.scale, m.transform, m.primary, vec![(m.connector.as_str(), mode.as_str(), HashMap::new())]))
        })
        .collect();
    // Método 1 = temporal: no se escribe en monitors.xml.
    p.apply_monitors_config(now.serial, 1, configs, HashMap::new())
        .await
        .context("Mutter rechazó la disposición de monitores")
}

/// Lo que `keep` comparte con `restore`.
struct Shared {
    /// Disposición de referencia (sin el monitor virtual).
    baseline: State,
    virt: Option<String>,
}

/// Vigila el monitor virtual mientras existe.
pub struct Keeper {
    /// El monitor y su vecino.
    pub placed: watch::Receiver<Option<VirtualMonitor>>,
    shared: Arc<Mutex<Shared>>,
    task: tokio::task::JoinHandle<()>,
}

impl Keeper {
    /// Llamar tras quitar el monitor virtual: Mutter aplica entonces su
    /// disposición por defecto (enciende lo apagado y lo recoloca todo), y se
    /// vuelve a poner la que tenías.
    pub async fn restore(self) {
        self.task.abort();
        let Shared { baseline, virt } = std::mem::replace(
            &mut *self.shared.lock().unwrap(),
            Shared { baseline: State { serial: 0, placed: Vec::new(), modes: HashMap::new() }, virt: None },
        );
        if let Err(e) = restore_inner(&baseline, virt.as_deref()).await {
            tracing::warn!("no se pudo reponer la disposición de monitores: {e:#}");
        }
    }
}

async fn restore_inner(baseline: &State, virt: Option<&str>) -> Result<()> {
    let p = proxy().await?;
    // Esperar a que Mutter quite el monitor y rehaga la disposición.
    let mut now = read(&p).await?;
    for _ in 0..30 {
        if virt.is_none_or(|v| now.find(v).is_none()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        now = read(&p).await?;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let now = read(&p).await?;
    let layout = without_virtual(baseline, &now);
    if layout.is_empty() || same_layout(&layout, &now.placed) {
        return Ok(());
    }
    apply(&p, &now, &layout).await?;
    tracing::info!("disposición de monitores repuesta");
    Ok(())
}

/// Espera a que aparezca el monitor virtual nuevo, lo coloca y vigila los
/// cambios.
pub fn keep(before: State) -> Keeper {
    let (tx, rx) = watch::channel(None);
    let shared = Arc::new(Mutex::new(Shared { baseline: before.clone(), virt: None }));
    let s = shared.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = keep_inner(before, tx, s).await {
            tracing::warn!("disposición de monitores: {e:#}");
        }
    });
    Keeper { placed: rx, shared, task }
}

async fn keep_inner(mut before: State, tx: watch::Sender<Option<VirtualMonitor>>, shared: Arc<Mutex<Shared>>) -> Result<()> {
    let p = proxy().await?;
    let mut changes = p.receive_monitors_changed().await?;

    // 1. Esperar al monitor virtual nuevo (uno de Mutter que no estaba antes).
    let virt = loop {
        let now = read(&p).await?;
        if let Some(v) = now.placed.iter().find(|m| m.vendor == VIRTUAL_VENDOR && before.find(&m.connector).is_none()) {
            break v.connector.clone();
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), changes.next()).await;
    };

    // 2. Colocarlo, y volver a hacerlo cada vez que cambia de tamaño (Mutter
    //    rehace la disposición). Cualquier otro cambio lo hiciste tú: se guarda.
    shared.lock().unwrap().virt = Some(virt.clone());
    let mut applied: Option<Vec<Placed>> = None;
    let mut size = None;
    loop {
        let now = read(&p).await?;
        if let Some(v) = now.find(&virt).cloned() {
            let key = before.layout_key();
            let resized = size != Some((v.rect.w, v.rect.h));
            if resized {
                size = Some((v.rect.w, v.rect.h));
                let layout = desired(&before, &now, &v, load_offsets().get(&key).copied());
                if same_layout(&layout, &now.placed) {
                    applied = Some(layout);
                } else {
                    match apply(&p, &now, &layout).await {
                        Ok(()) => {
                            // Esperar a que Mutter la aplique de verdad: si no, una
                            // señal anterior haría leer la disposición por defecto
                            // y tomarla por un cambio tuyo.
                            for _ in 0..30 {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                if read(&p).await.is_ok_and(|s| same_layout(&layout, &s.placed)) {
                                    break;
                                }
                            }
                            applied = Some(layout);
                        }
                        Err(e) => {
                            // No tomar la disposición por defecto de Mutter como tuya.
                            tracing::warn!("{e:#}");
                            applied = Some(now.placed.clone());
                        }
                    }
                    // Descartar las señales de lo que acabamos de pedir.
                    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(150), changes.next()).await {}
                    continue;
                }
            } else if applied.as_deref().is_none_or(|a| !same_layout(a, &now.placed)) {
                // Lo cambiaste en Configuración: la nueva disposición pasa a
                // ser la de referencia (incluido lo que enciendas o apagues).
                before = State { placed: now.placed.iter().filter(|m| m.connector != virt).cloned().collect(), ..now.clone() };
                shared.lock().unwrap().baseline = before.clone();
                applied = Some(now.placed.clone());
            }
            if let Some(pr) = now.placed.iter().find(|m| m.primary && m.vendor != VIRTUAL_VENDOR) {
                save_offset(&before.layout_key(), Offset { dx: v.rect.x - pr.rect.x, dy: v.rect.y - pr.rect.y });
            }
            let info = describe(&now, &virt);
            tx.send_if_modified(|cur| {
                let changed = cur.as_ref().map(|c| (c.rect, c.neighbor)) != info.as_ref().map(|i| (i.rect, i.neighbor));
                if changed {
                    *cur = info;
                }
                changed
            });
        }
        if changes.next().await.is_none() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(connector: &str, x: i32, y: i32, w: i32, h: i32, primary: bool) -> Placed {
        let vendor = if connector.starts_with("Meta") { VIRTUAL_VENDOR } else { "X" };
        Placed { connector: connector.into(), vendor: vendor.into(), rect: Rect { x, y, w, h }, scale: 1.0, transform: 0, primary }
    }

    fn state(placed: Vec<Placed>) -> State {
        let modes = placed.iter().map(|p| (p.connector.clone(), "m".to_owned())).collect();
        State { serial: 1, placed, modes }
    }

    #[test]
    fn restores_saved_position_and_keeps_disabled_monitors_off() {
        // HDMI encima del portátil; DP-3 estaba apagado y Mutter lo encendió.
        let before = state(vec![placed("HDMI-1", 0, 0, 1920, 1080, true), placed("eDP-1", 0, 1080, 1920, 1080, false)]);
        let mut now = state(vec![
            placed("HDMI-1", 0, 0, 1920, 1080, true),
            placed("eDP-1", 0, 1080, 1920, 1080, false),
            placed("DP-3", 1920, 0, 640, 480, false),
            placed("Meta-0", 3840, 0, 1370, 858, false),
        ]);
        now.modes.insert("DP-3".into(), "m".into());
        let virt = now.find("Meta-0").unwrap().clone();
        // Guardado: a la izquierda del portátil (relativo a la principal).
        let layout = desired(&before, &now, &virt, Some(Offset { dx: -1370, dy: 1080 }));
        let get = |c: &str| layout.iter().find(|p| p.connector == c).map(|p| p.rect);
        assert_eq!(get("DP-3"), None);
        assert_eq!(get("Meta-0"), Some(Rect { x: 0, y: 1080, w: 1370, h: 858 }));
        assert_eq!(get("HDMI-1"), Some(Rect { x: 1370, y: 0, w: 1920, h: 1080 }));
        assert_eq!(get("eDP-1"), Some(Rect { x: 1370, y: 1080, w: 1920, h: 1080 }));
    }

    #[test]
    fn restore_drops_virtual_and_keeps_disabled_off() {
        // Tablet a la izquierda del portátil; al quitarla Mutter encendió DP-3.
        let baseline = state(vec![placed("HDMI-1", 1370, 0, 1920, 1080, true), placed("eDP-1", 1370, 1080, 1920, 1080, false)]);
        let now = state(vec![
            placed("HDMI-1", 0, 0, 1920, 1080, true),
            placed("eDP-1", 1920, 0, 1920, 1080, false),
            placed("DP-3", 3840, 0, 640, 480, false),
        ]);
        let layout = without_virtual(&baseline, &now);
        let get = |c: &str| layout.iter().find(|p| p.connector == c).map(|p| p.rect);
        assert_eq!(get("DP-3"), None);
        assert_eq!(get("HDMI-1"), Some(Rect { x: 0, y: 0, w: 1920, h: 1080 }));
        assert_eq!(get("eDP-1"), Some(Rect { x: 0, y: 1080, w: 1920, h: 1080 }));
    }

    #[test]
    fn invalid_saved_position_snaps_to_nearest_edge() {
        let others = [Rect { x: 0, y: 0, w: 1920, h: 1080 }];
        // Encima del monitor (solapado) → al borde más cercano.
        assert_eq!(snap(&others, 1370, 858, (1800, 100)), (1920, 100));
        // Lejos y con hueco → pegado.
        assert_eq!(snap(&others, 1370, 858, (-2000, 50)), (-1370, 50));
    }

    #[test]
    fn neighbor_side() {
        let s = state(vec![placed("eDP-1", 1370, 1080, 1920, 1080, false), placed("Meta-0", 0, 1080, 1370, 858, false)]);
        let v = describe(&s, "Meta-0").unwrap();
        let (_, side, a, b) = v.neighbor.unwrap();
        assert_eq!((side, a, b), ("left", 1080, 1938));
    }
}
