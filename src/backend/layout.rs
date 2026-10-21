//! Disposición de monitores, lo común a todos los escritorios: rectángulos,
//! la pantalla física que toca al monitor virtual (para la animación) y la
//! posición guardada de la tablet en `~/.config/encaje/layout.json`, una por
//! cada combinación de monitores físicos (no es lo mismo el portátil solo
//! que con el HDMI).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn overlaps(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    /// Longitud del borde que comparten (0 si no se tocan).
    pub fn shared_edge(&self, o: &Rect) -> i32 {
        let vertical = (self.right() == o.x || o.right() == self.x)
            .then(|| self.bottom().min(o.bottom()) - self.y.max(o.y));
        let horizontal = (self.bottom() == o.y || o.bottom() == self.y)
            .then(|| self.right().min(o.right()) - self.x.max(o.x));
        vertical.into_iter().chain(horizontal).max().unwrap_or(0).max(0)
    }
}

/// Monitor virtual y el borde de la pantalla física con la que se toca: es
/// donde se dibuja la animación de conexión.
#[derive(Clone, Debug, Serialize)]
pub struct VirtualMonitor {
    /// Nombre del conector (el mismo que ve XWayland en RandR).
    pub connector: String,
    pub rect: Rect,
    /// Pantalla física vecina, el lado de esa pantalla donde está la tablet
    /// ("left", "right", "top", "bottom") y el tramo compartido.
    pub neighbor: Option<(Rect, &'static str, i32, i32)>,
}

impl VirtualMonitor {
    /// `physical`: las pantallas físicas encendidas.
    pub fn new(connector: &str, rect: Rect, physical: impl IntoIterator<Item = Rect>) -> Self {
        let neighbor = physical
            .into_iter()
            .map(|r| (r.shared_edge(&rect), r))
            .filter(|(edge, _)| *edge > 0)
            .max_by_key(|(edge, _)| *edge)
            .map(|(_, r)| {
                let t = rect;
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
        Self { connector: connector.to_owned(), rect, neighbor }
    }

    /// Lo que importa para la animación (sin el nombre).
    pub fn same_place(&self, o: &VirtualMonitor) -> bool {
        (self.rect, self.neighbor) == (o.rect, o.neighbor)
    }
}

/// Clave de una combinación de monitores físicos encendidos.
pub fn layout_key<'a>(connectors: impl IntoIterator<Item = &'a str>) -> String {
    let mut c: Vec<&str> = connectors.into_iter().collect();
    c.sort();
    c.join("+")
}

// ---- Posiciones guardadas ----

/// Posición del monitor virtual relativa a la pantalla principal.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Offset {
    pub dx: i32,
    pub dy: i32,
}

fn layout_file() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("encaje").join("layout.json")
}

pub fn load_offsets() -> HashMap<String, Offset> {
    std::fs::read(layout_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_offset(key: &str, offset: Offset) {
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

/// Busca la posición válida (pegada a algún monitor, sin solaparse) más
/// cercana a `want`. Los compositores rechazan disposiciones con huecos o
/// solapes.
pub fn snap(others: &[Rect], w: i32, h: i32, want: (i32, i32)) -> (i32, i32) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let v = VirtualMonitor::new(
            "Meta-0",
            Rect { x: 0, y: 1080, w: 1370, h: 858 },
            [Rect { x: 1370, y: 1080, w: 1920, h: 1080 }],
        );
        let (_, side, a, b) = v.neighbor.unwrap();
        assert_eq!((side, a, b), ("left", 1080, 1938));
    }
}
