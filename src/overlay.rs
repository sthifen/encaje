//! Animación de conexión, al estilo de los Joy-Con de la Switch: en el borde
//! de la pantalla que da a la tablet entra un riel de cristal, "encaja" con
//! un destello y se encienden cuatro luces, como las de jugador.
//!
//! Es una ventana X11 (XWayland) override-redirect con canal alfa: se coloca
//! donde se quiere (Wayland no lo permite), no roba el foco, no oculta la
//! barra superior como haría una ventana a pantalla completa y deja pasar
//! los clics (región de entrada vacía con XShape). La interfaz la lanza con
//! `--overlay '{"rect":…,"side":…,"from":…,"to":…,"color":…}'`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, RadialGradient, Rect,
    SpreadMode, Transform,
};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ClipOrdering, ColormapAlloc, ConnectionExt as _, CreateWindowAux, EventMask, ImageFormat, VisualClass,
    WindowClass,
};

/// Profundidad de la franja (px hacia dentro de la pantalla).
const DEPTH: f32 = 300.0;
const DURATION: f32 = 1.9;

#[derive(Deserialize)]
struct Spec {
    /// Pantalla física vecina de la tablet (coordenadas lógicas).
    rect: [i32; 4],
    /// Lado de esa pantalla donde está la tablet.
    side: String,
    /// Tramo del borde compartido.
    from: i32,
    to: i32,
    /// "blue" (monitor) o "red" (entrada), como los Joy-Con neón.
    color: String,
}

pub fn run(spec: &str) -> Result<()> {
    let spec: Spec = serde_json::from_str(spec).context("especificación de la animación")?;
    let [rx, ry, rw, rh] = spec.rect;
    let len = (spec.to - spec.from).max(1);
    let depth = DEPTH as i32;
    let (x, y, w, h) = match spec.side.as_str() {
        "left" => (rx, spec.from, depth.min(rw), len),
        "right" => (rx + rw - depth.min(rw), spec.from, depth.min(rw), len),
        "top" => (spec.from, ry, len, depth.min(rh)),
        "bottom" => (spec.from, ry + rh - depth.min(rh), len, depth.min(rh)),
        s => bail!("lado desconocido: {s}"),
    };
    let color = match spec.color.as_str() {
        "red" => (255, 69, 84),
        _ => (0, 195, 227),
    };

    let (conn, screen_num) = x11rb::connect(None).context("no se pudo abrir XWayland")?;
    let screen = &conn.setup().roots[screen_num];
    let visual = screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| &d.visuals)
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .context("XWayland sin visual ARGB")?
        .visual_id;
    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)?;
    let win = conn.generate_id()?;
    conn.create_window(
        32,
        win,
        screen.root,
        x as i16,
        y as i16,
        w as u16,
        h as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE),
    )?;
    // Clics a través de la ventana.
    conn.shape_rectangles(shape::SO::SET, shape::SK::INPUT, ClipOrdering::UNSORTED, win, 0, 0, &[])?;
    conn.map_window(win)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &Default::default())?;
    conn.flush()?;

    let mut pixmap = Pixmap::new(w as u32, h as u32).context("tamaño de la animación")?;
    let edge = edge_transform(&spec.side, w as f32, h as f32);
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    // Filas por petición, para no pasar del tamaño máximo de X11.
    let rows_per_put = ((conn.maximum_request_bytes() - 64) / (w as usize * 4)).max(1);
    let start = Instant::now();
    loop {
        let t = start.elapsed().as_secs_f32();
        if t > DURATION {
            break;
        }
        pixmap.fill(Color::TRANSPARENT);
        draw(&mut pixmap, edge, len as f32, t, color);
        // tiny-skia: RGBA premultiplicado; X11 ARGB32: BGRA premultiplicado.
        for (src, dst) in pixmap.data().chunks_exact(4).zip(bgra.chunks_exact_mut(4)) {
            dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
        }
        for (i, chunk) in bgra.chunks(rows_per_put * w as usize * 4).enumerate() {
            let rows = chunk.len() / (w as usize * 4);
            conn.put_image(ImageFormat::Z_PIXMAP, win, gc, w as u16, rows as u16, 0, (i * rows_per_put) as i16, 0, 32, chunk)?;
        }
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(16));
    }
    conn.destroy_window(win)?;
    conn.flush()?;
    Ok(())
}

/// Se dibuja en "espacio del borde": x a lo largo del borde, y hacia dentro
/// de la pantalla (0 = el borde que da a la tablet).
fn edge_transform(side: &str, w: f32, h: f32) -> Transform {
    match side {
        "left" => Transform::from_row(0.0, 1.0, 1.0, 0.0, 0.0, 0.0),
        "right" => Transform::from_row(0.0, 1.0, -1.0, 0.0, w, 0.0),
        "top" => Transform::from_row(1.0, 0.0, 0.0, 1.0, 0.0, 0.0),
        _ => Transform::from_row(1.0, 0.0, 0.0, -1.0, 0.0, h),
    }
}

fn smooth(a: f32, b: f32, t: f32) -> f32 {
    let x = ((t - a) / (b - a)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Entrada con rebote (easeOutBack).
fn ease_out_back(x: f32) -> f32 {
    let (c1, c3) = (1.70158, 2.70158);
    1.0 + c3 * (x - 1.0).powi(3) + c1 * (x - 1.0).powi(2)
}

fn rgba(c: (u8, u8, u8), a: f32) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

fn capsule(x: f32, y: f32, w: f32, h: f32) -> Option<tiny_skia::Path> {
    let r = h.min(w) / 2.0;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn fill_shader(p: &mut Pixmap, path: &tiny_skia::Path, shader: Option<tiny_skia::Shader<'static>>, tf: Transform) {
    if let Some(shader) = shader {
        let paint = Paint { shader, anti_alias: true, ..Default::default() };
        p.fill_path(path, &paint, FillRule::Winding, tf, None);
    }
}

fn draw(p: &mut Pixmap, tf: Transform, len: f32, t: f32, color: (u8, u8, u8)) {
    let white = (255, 255, 255);
    let fade_out = 1.0 - smooth(1.4, DURATION, t);
    let center = len / 2.0;

    // 1. Resplandor del borde: una elipse de luz que nace en el borde.
    let glow = smooth(0.3, 0.6, t) * fade_out;
    if glow > 0.0 {
        let band = PathBuilder::from_rect(Rect::from_xywh(0.0, 0.0, len, DEPTH).unwrap());
        let shader = RadialGradient::new(
            Point::from_xy(0.0, 0.0),
            0.0,
            Point::from_xy(0.0, 0.0),
            1.0,
            vec![
                GradientStop::new(0.0, rgba(color, 0.55 * glow)),
                GradientStop::new(0.45, rgba(color, 0.18 * glow)),
                GradientStop::new(1.0, rgba(color, 0.0)),
            ],
            SpreadMode::Pad,
            Transform::from_row(len * 0.62, 0.0, 0.0, DEPTH * 0.9, center, 0.0),
        );
        fill_shader(p, &band, shader, tf);
    }

    // 2. Onda del "clic" al encajar.
    let ripple = ((t - 0.45) / 0.7).clamp(0.0, 1.0);
    if t > 0.45 && ripple < 1.0 {
        let r = 30.0 + ripple * DEPTH * 1.1;
        let a = (1.0 - ripple).powf(1.5) * 0.8;
        let w = 26.0 / r;
        let band = PathBuilder::from_rect(Rect::from_xywh(0.0, 0.0, len, DEPTH).unwrap());
        let shader = RadialGradient::new(
            Point::from_xy(center, 0.0),
            0.0,
            Point::from_xy(center, 0.0),
            r,
            vec![
                GradientStop::new((1.0 - 2.0 * w).max(0.0), rgba(white, 0.0)),
                GradientStop::new(1.0 - w, rgba(white, a)),
                GradientStop::new(1.0, rgba(color, 0.0)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        );
        fill_shader(p, &band, shader, tf);
    }

    // 3. El riel: entra desde fuera del borde con rebote.
    let slide = ease_out_back((t / 0.45).clamp(0.0, 1.0));
    let rail_len = (len * 0.5).clamp(80.0, 520.0);
    let rail_h = 16.0;
    let rail_y = -40.0 + slide * 54.0;
    let rail_x = center - rail_len / 2.0;
    let appear = smooth(0.0, 0.15, t) * fade_out;
    let flash = if t > 0.45 { (1.0 - (t - 0.45) / 0.35).clamp(0.0, 1.0) } else { 0.0 };
    // Halo difuso: varias cápsulas cada vez más grandes y más tenues.
    for i in (1..=6).rev() {
        let g = i as f32 * 5.0;
        if let Some(path) = capsule(rail_x - g, rail_y - g, rail_len + 2.0 * g, rail_h + 2.0 * g) {
            let a = (0.16 + 0.25 * flash) * appear / i as f32;
            fill_shader(p, &path, Some(tiny_skia::Shader::SolidColor(rgba(color, a))), tf);
        }
    }
    if let Some(path) = capsule(rail_x, rail_y, rail_len, rail_h) {
        // Cristal: blanco translúcido con brillo arriba y el color debajo.
        let shader = LinearGradient::new(
            Point::from_xy(0.0, rail_y),
            Point::from_xy(0.0, rail_y + rail_h),
            vec![
                GradientStop::new(0.0, rgba(white, (0.95) * appear)),
                GradientStop::new(0.5, rgba(blend(color, white, 0.35 + 0.6 * flash), 0.9 * appear)),
                GradientStop::new(1.0, rgba(color, 0.85 * appear)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        );
        fill_shader(p, &path, shader, tf);
    }

    // 4. Cuatro luces que se encienden en orden, como las de jugador.
    let leds_y = rail_y + rail_h + 22.0;
    for i in 0..4 {
        let on = smooth(0.62 + i as f32 * 0.11, 0.70 + i as f32 * 0.11, t) * fade_out;
        let x = center + (i as f32 - 1.5) * 22.0;
        if let Some(path) = PathBuilder::from_circle(x, leds_y, 4.5) {
            let base = rgba(white, 0.18 * appear);
            fill_shader(p, &path, Some(tiny_skia::Shader::SolidColor(base)), tf);
            if on > 0.0 {
                let lit = rgba(blend(color, white, 0.5), on);
                fill_shader(p, &path, Some(tiny_skia::Shader::SolidColor(lit)), tf);
                if let Some(halo) = PathBuilder::from_circle(x, leds_y, 11.0) {
                    let shader = RadialGradient::new(
                        Point::from_xy(x, leds_y),
                        0.0,
                        Point::from_xy(x, leds_y),
                        11.0,
                        vec![GradientStop::new(0.0, rgba(color, 0.6 * on)), GradientStop::new(1.0, rgba(color, 0.0))],
                        SpreadMode::Pad,
                        Transform::identity(),
                    );
                    fill_shader(p, &halo, shader, tf);
                }
            }
        }
    }
}

fn blend(a: (u8, u8, u8), b: (u8, u8, u8), k: f32) -> (u8, u8, u8) {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k.clamp(0.0, 1.0)) as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

