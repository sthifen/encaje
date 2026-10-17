//! Interfaz (`--gui`): dos tarjetas, sin texto. Una usa la tablet como
//! segundo monitor; la otra, como pantalla aparte que recibe el ratón y el
//! teclado. Un clic arranca el modo, otro clic lo para.
//!
//! La interfaz no hace el trabajo: lanza este mismo programa (`--events` y,
//! si toca, `--input-only`) y lo vigila. Si termina sin que se lo pidas (el
//! botón "Dejar de compartir" de GNOME, un fallo…) lo vuelve a lanzar. Lee
//! sus eventos para encender la luz de la tarjeta y lanzar la animación de
//! conexión (`--overlay`) en el borde donde está la tablet.
//!
//! Estilo de cristal: GNOME no deja a las aplicaciones desenfocar lo que hay
//! detrás de la ventana, así que el "cristal" se dibuja dentro: manchas de
//! color animadas bajo una capa translúcida, y tarjetas con borde de luz.

use std::cell::RefCell;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{cairo, gdk, glib};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Monitor,
    Input,
}

impl Mode {
    fn key(self) -> &'static str {
        match self {
            Mode::Monitor => "monitor",
            Mode::Input => "input",
        }
    }

    /// Azul y rojo neón, como los Joy-Con.
    fn color(self) -> (f64, f64, f64) {
        match self {
            Mode::Monitor => (0.0, 0.765, 0.89),
            Mode::Input => (1.0, 0.27, 0.33),
        }
    }

    fn overlay_color(self) -> &'static str {
        match self {
            Mode::Monitor => "blue",
            Mode::Input => "red",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Off,
    Waiting,
    Connected,
}

enum Msg {
    Event(u64, serde_json::Value),
}

struct Running {
    child: Child,
    mode: Mode,
    id: u64,
    started: Instant,
    /// Se le mandó SIGINT: cuándo, para rematarlo si no sale.
    stopping: Option<Instant>,
}

struct State {
    wanted: Option<Mode>,
    running: Option<Running>,
    status: Status,
    restart_at: Option<Instant>,
    /// Fallos seguidos al arrancar, para espaciar los reintentos.
    failures: u32,
    next_id: u64,
    quitting: bool,
    tx: mpsc::Sender<Msg>,
}

const CSS: &str = r#"
window.spacedesk { background: transparent; }
window.spacedesk.csd {
  border-radius: 28px;
  box-shadow: 0 22px 60px rgba(0,0,0,0.50), 0 0 0 1px rgba(255,255,255,0.10);
}
.glass {
  border-radius: 28px;
  background-color: rgba(24, 26, 36, 0.74);
  border: 1px solid rgba(255,255,255,0.16);
  box-shadow: inset 0 1px 0 rgba(255,255,255,0.22);
}
.frost {
  border-radius: 28px;
  background-image: linear-gradient(160deg, rgba(255,255,255,0.10), rgba(255,255,255,0.02) 45%, rgba(255,255,255,0.06));
}
button.card {
  background-color: rgba(255,255,255,0.07);
  background-image: linear-gradient(170deg, rgba(255,255,255,0.14), rgba(255,255,255,0.02));
  border: 1px solid rgba(255,255,255,0.14);
  border-radius: 24px;
  padding: 14px;
  box-shadow: inset 0 1px 0 rgba(255,255,255,0.20), 0 10px 26px rgba(0,0,0,0.28);
  transition: all 220ms cubic-bezier(0.2, 0.9, 0.3, 1.2);
}
button.card:hover {
  background-color: rgba(255,255,255,0.12);
  border-color: rgba(255,255,255,0.26);
}
button.card:active { background-color: rgba(255,255,255,0.18); }
button.card.active.blue {
  border-color: rgba(0,195,227,0.95);
  box-shadow: 0 0 0 1px rgba(0,195,227,0.55), 0 0 34px rgba(0,195,227,0.45), inset 0 1px 0 rgba(255,255,255,0.28);
}
button.card.active.red {
  border-color: rgba(255,69,84,0.95);
  box-shadow: 0 0 0 1px rgba(255,69,84,0.55), 0 0 34px rgba(255,69,84,0.45), inset 0 1px 0 rgba(255,255,255,0.28);
}
button.light {
  min-width: 13px; min-height: 13px; padding: 0; margin: 0;
  border-radius: 999px; border: none;
  box-shadow: inset 0 0 0 1px rgba(0,0,0,0.22);
}
button.light { background-image: none; outline: none; }
button.light.close { background-color: #ff5f57; }
button.light.min { background-color: #febc2e; }
button.light.zoom { background-color: rgba(255,255,255,0.18); }
button.light:hover { filter: brightness(1.15); }
"#;

pub fn run() -> Result<()> {
    let app = gtk::Application::builder().application_id("dev.spacedesk.linux").build();
    app.connect_activate(build);
    // Sin argumentos: los nuestros ya los leyó clap.
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build(app: &gtk::Application) {
    if let Some(w) = app.active_window() {
        w.present();
        return;
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("sin pantalla"),
        &provider,
        // Por encima del gtk.css del usuario: los temas suelen redefinir los botones.
        gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
    );

    let (tx, rx) = mpsc::channel();
    let state = Rc::new(RefCell::new(State {
        wanted: None,
        running: None,
        status: Status::Off,
        restart_at: None,
        failures: 0,
        next_id: 0,
        quitting: false,
        tx,
    }));

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .default_width(600)
        .default_height(320)
        .resizable(false)
        .icon_name("spacedesk-linux")
        .title("Spacedesk Linux")
        .build();
    window.add_css_class("spacedesk");
    // Sin barra de título: los semáforos van dentro del cristal.
    window.set_titlebar(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));

    let start = Instant::now();
    let glass = gtk::Overlay::new();
    glass.add_css_class("glass");
    glass.set_overflow(gtk::Overflow::Hidden);

    // Fondo: manchas de color que se mueven despacio.
    let aurora = gtk::DrawingArea::new();
    {
        let state = state.clone();
        aurora.set_draw_func(move |_, cr, w, h| {
            let s = state.borrow();
            let tint = s.wanted.map(Mode::color);
            draw_aurora(cr, w as f64, h as f64, start.elapsed().as_secs_f64(), tint);
        });
    }
    glass.set_child(Some(&aurora));
    let frost = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frost.add_css_class("frost");
    frost.set_can_target(false);
    glass.add_overlay(&frost);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(14);
    content.set_margin_start(16);
    content.set_margin_end(26);
    content.set_margin_bottom(26);
    glass.add_overlay(&content);

    let lights = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    for (class, action) in [("close", 0), ("min", 1), ("zoom", 2)] {
        let b = gtk::Button::new();
        b.add_css_class("light");
        b.add_css_class(class);
        b.set_valign(gtk::Align::Center);
        let w = window.clone();
        b.connect_clicked(move |_| match action {
            0 => w.close(),
            1 => w.minimize(),
            _ => {}
        });
        lights.append(&b);
    }
    content.append(&lights);

    let cards = gtk::Box::new(gtk::Orientation::Horizontal, 22);
    cards.set_homogeneous(true);
    cards.set_vexpand(true);
    cards.set_margin_start(10);
    let mut buttons = Vec::new();
    for mode in [Mode::Monitor, Mode::Input] {
        let icon = gtk::DrawingArea::new();
        icon.set_content_width(230);
        icon.set_content_height(170);
        {
            let state = state.clone();
            icon.set_draw_func(move |_, cr, w, h| {
                let s = state.borrow();
                let status = if s.wanted == Some(mode) { s.status } else { Status::Off };
                draw_icon(cr, mode, w as f64, h as f64, start.elapsed().as_secs_f64(), status);
            });
        }
        let button = gtk::Button::builder().child(&icon).build();
        button.add_css_class("card");
        button.add_css_class(match mode {
            Mode::Monitor => "blue",
            Mode::Input => "red",
        });
        // Para lectores de pantalla (no se ve).
        button.update_property(&[gtk::accessible::Property::Label(match mode {
            Mode::Monitor => "Tablet como segundo monitor",
            Mode::Input => "Tablet con el ratón y el teclado del PC",
        })]);
        {
            let state = state.clone();
            button.connect_clicked(move |_| {
                let mut s = state.borrow_mut();
                let new = if s.wanted == Some(mode) { None } else { Some(mode) };
                set_wanted(&mut s, new);
            });
        }
        cards.append(&button);
        buttons.push((mode, button, icon));
    }
    content.append(&cards);

    let handle = gtk::WindowHandle::new();
    handle.set_child(Some(&glass));
    window.set_child(Some(&handle));

    {
        let state = state.clone();
        window.connect_close_request(move |w| {
            let mut s = state.borrow_mut();
            if s.running.is_none() {
                return glib::Propagation::Proceed;
            }
            s.quitting = true;
            s.wanted = None;
            stop(&mut s);
            w.set_visible(false);
            glib::Propagation::Stop
        });
    }

    // Bucle de vigilancia: eventos del proceso, reinicios y animación.
    {
        let state = state.clone();
        let window = window.clone();
        glib::timeout_add_local(Duration::from_millis(33), move || {
            let mut s = state.borrow_mut();
            while let Ok(Msg::Event(id, ev)) = rx.try_recv() {
                handle_event(&mut s, id, &ev);
            }
            supervise(&mut s);
            if s.quitting && s.running.is_none() {
                window.destroy();
                return glib::ControlFlow::Break;
            }
            for (mode, button, icon) in &buttons {
                if s.wanted == Some(*mode) {
                    button.add_css_class("active");
                } else {
                    button.remove_css_class("active");
                }
                icon.queue_draw();
            }
            aurora.queue_draw();
            glib::ControlFlow::Continue
        });
    }

    // Arrancar con el último modo usado.
    {
        let mut s = state.borrow_mut();
        let last = load_last_mode();
        set_wanted(&mut s, last);
    }
    window.present();
}

// ---- Proceso vigilado ----

fn set_wanted(s: &mut State, mode: Option<Mode>) {
    s.wanted = mode;
    s.failures = 0;
    save_last_mode(mode);
    if s.running.as_ref().is_some_and(|r| Some(r.mode) != mode) {
        stop(s);
    }
    s.restart_at = Some(Instant::now());
    s.status = if mode.is_some() { Status::Waiting } else { Status::Off };
}

fn stop(s: &mut State) {
    if let Some(r) = s.running.as_mut() {
        if r.stopping.is_none() {
            // SIGINT: el programa cierra en orden (la tablet recupera su
            // configuración del puntero).
            unsafe { libc::kill(r.child.id() as i32, libc::SIGINT) };
            r.stopping = Some(Instant::now());
        }
    }
}

fn supervise(s: &mut State) {
    if let Some(r) = s.running.as_mut() {
        match r.child.try_wait() {
            Ok(Some(status)) => {
                let quick = r.started.elapsed() < Duration::from_secs(5);
                let wanted = s.wanted == Some(r.mode) && r.stopping.is_none();
                s.running = None;
                if wanted {
                    // Terminó solo: volver a lanzarlo, más despacio si falla al arrancar.
                    s.failures = if quick { s.failures + 1 } else { 0 };
                    let delay = Duration::from_millis(800 * (1 << s.failures.min(4)) as u64);
                    tracing::info!("el servidor terminó ({status}); se relanza en {delay:?}");
                    s.restart_at = Some(Instant::now() + delay);
                    s.status = Status::Waiting;
                }
            }
            Ok(None) => {
                if r.stopping.is_some_and(|t| t.elapsed() > Duration::from_secs(5)) {
                    let _ = r.child.kill();
                }
            }
            Err(_) => s.running = None,
        }
    }
    if s.running.is_none() && !s.quitting {
        if let (Some(mode), Some(at)) = (s.wanted, s.restart_at) {
            if Instant::now() >= at {
                s.restart_at = None;
                match spawn(mode, s.next_id, s.tx.clone()) {
                    Ok(child) => {
                        s.running = Some(Running { child, mode, id: s.next_id, started: Instant::now(), stopping: None });
                        s.next_id += 1;
                    }
                    Err(e) => {
                        tracing::error!("no se pudo lanzar el servidor: {e:#}");
                        s.restart_at = Some(Instant::now() + Duration::from_secs(5));
                    }
                }
            }
        }
    }
}

fn spawn(mode: Mode, id: u64, tx: mpsc::Sender<Msg>) -> Result<Child> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(config_args()).arg("--events");
    if mode == Mode::Input {
        cmd.arg("--input-only");
    }
    let mut child = cmd.stdout(Stdio::piped()).stdin(Stdio::null()).spawn()?;
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(json) = line.strip_prefix('@') {
                if let Ok(v) = serde_json::from_str(json) {
                    if tx.send(Msg::Event(id, v)).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok(child)
}

fn handle_event(s: &mut State, id: u64, ev: &serde_json::Value) {
    let Some(r) = s.running.as_ref().filter(|r| r.id == id) else { return };
    let mode = r.mode;
    match ev["t"].as_str() {
        Some("connected") => {
            s.status = Status::Connected;
            s.failures = 0;
            play_overlay(mode, &ev["monitor"]);
        }
        Some("waiting") | Some("disconnected") => s.status = Status::Waiting,
        _ => {}
    }
}

/// Lanza la animación de conexión en el borde que da a la tablet.
fn play_overlay(mode: Mode, monitor: &serde_json::Value) {
    let n = &monitor["neighbor"];
    let (Some(rect), Some(side)) = (n.get(0), n.get(1).and_then(|v| v.as_str())) else { return };
    let spec = serde_json::json!({
        "rect": [rect["x"], rect["y"], rect["w"], rect["h"]],
        "side": side,
        "from": n[2],
        "to": n[3],
        "color": mode.overlay_color(),
    });
    let Ok(exe) = std::env::current_exe() else { return };
    if let Ok(mut child) = Command::new(exe).arg("--overlay").arg(spec.to_string()).stdout(Stdio::null()).spawn() {
        std::thread::spawn(move || child.wait());
    }
}

// ---- Configuración ----

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
}

/// Opciones de ~/.config/spacedesk-linux.conf (las mismas que el lanzador).
fn config_args() -> Vec<String> {
    let conf = std::fs::read_to_string(config_dir().join("spacedesk-linux.conf")).unwrap_or_default();
    conf.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        .collect()
}

fn last_mode_file() -> PathBuf {
    config_dir().join("spacedesk-linux").join("gui.json")
}

fn load_last_mode() -> Option<Mode> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(last_mode_file()).ok()?).ok()?;
    match v["mode"].as_str()? {
        "monitor" => Some(Mode::Monitor),
        "input" => Some(Mode::Input),
        _ => None,
    }
}

fn save_last_mode(mode: Option<Mode>) {
    let path = last_mode_file();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(path, serde_json::json!({ "mode": mode.map(Mode::key) }).to_string());
}

// ---- Dibujo ----

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
    cr.close_path();
}

fn mix(a: (f64, f64, f64), b: (f64, f64, f64), k: f64) -> (f64, f64, f64) {
    (a.0 + (b.0 - a.0) * k, a.1 + (b.1 - a.1) * k, a.2 + (b.2 - a.2) * k)
}

/// Manchas grandes y difusas (gradientes radiales) que flotan despacio.
fn draw_aurora(cr: &cairo::Context, w: f64, h: f64, t: f64, tint: Option<(f64, f64, f64)>) {
    let base = [(0.0, 0.62, 0.95), (0.62, 0.32, 0.98), (1.0, 0.33, 0.45), (0.15, 0.85, 0.75)];
    let blobs = [(0.18, 0.25, 0.55, 0.13), (0.82, 0.30, 0.50, 0.17), (0.65, 0.95, 0.60, 0.11), (0.25, 0.90, 0.45, 0.15)];
    for (i, ((bx, by, r, speed), color)) in blobs.iter().zip(base).enumerate() {
        let phase = i as f64 * 1.7;
        let x = w * (bx + 0.08 * (t * speed + phase).sin());
        let y = h * (by + 0.10 * (t * speed * 1.3 + phase).cos());
        let radius = w.max(h) * r;
        let c = match tint {
            Some(tc) => mix(color, tc, 0.55),
            None => color,
        };
        let g = cairo::RadialGradient::new(x, y, 0.0, x, y, radius);
        g.add_color_stop_rgba(0.0, c.0, c.1, c.2, 0.55);
        g.add_color_stop_rgba(0.55, c.0, c.1, c.2, 0.18);
        g.add_color_stop_rgba(1.0, c.0, c.1, c.2, 0.0);
        let _ = cr.set_source(&g);
        let _ = cr.paint();
    }
}

/// Pantalla de cristal: marco claro, interior con degradado del color.
fn screen(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64, color: (f64, f64, f64), lit: f64) {
    rounded(cr, x, y, w, h, r);
    let g = cairo::LinearGradient::new(x, y, x, y + h);
    g.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.30);
    g.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.12);
    let _ = cr.set_source(&g);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.55);
    cr.set_line_width(1.2);
    let _ = cr.stroke();
    let inset = r * 0.55;
    rounded(cr, x + inset, y + inset, w - 2.0 * inset, h - 2.0 * inset, r * 0.5);
    let g = cairo::LinearGradient::new(x, y, x + w, y + h);
    let c2 = mix(color, (1.0, 1.0, 1.0), 0.35);
    g.add_color_stop_rgba(0.0, c2.0, c2.1, c2.2, 0.55 + 0.35 * lit);
    g.add_color_stop_rgba(1.0, color.0, color.1, color.2, 0.35 + 0.45 * lit);
    let _ = cr.set_source(&g);
    let _ = cr.fill();
}

fn draw_icon(cr: &cairo::Context, mode: Mode, w: f64, h: f64, t: f64, status: Status) {
    let color = mode.color();
    let lit = if status == Status::Off { 0.0 } else { 1.0 };
    let s = (w / 230.0).min(h / 170.0);
    cr.translate((w - 230.0 * s) / 2.0, (h - 170.0 * s) / 2.0);
    cr.scale(s, s);
    match mode {
        Mode::Monitor => {
            // Monitor y tablet, con una ventana repartida entre los dos.
            screen(cr, 14.0, 30.0, 118.0, 78.0, 10.0, color, lit);
            rounded(cr, 62.0, 108.0, 22.0, 14.0, 3.0);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
            let _ = cr.fill();
            rounded(cr, 46.0, 120.0, 54.0, 6.0, 3.0);
            let _ = cr.fill();
            screen(cr, 142.0, 52.0, 76.0, 54.0, 9.0, color, lit);
            let wobble = 4.0 * (t * 1.6).sin() * lit;
            rounded(cr, 92.0 + wobble, 52.0, 86.0, 40.0, 6.0);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(color.0, color.1, color.2, 0.9);
            cr.set_line_width(1.5);
            let _ = cr.stroke();
            rounded(cr, 92.0 + wobble, 52.0, 86.0, 9.0, 4.0);
            cr.set_source_rgba(color.0, color.1, color.2, 0.75);
            let _ = cr.fill();
        }
        Mode::Input => {
            // Tablet con su propio Android, y el ratón y el teclado del PC.
            screen(cr, 104.0, 22.0, 112.0, 76.0, 11.0, color, lit);
            for i in 0..3 {
                for j in 0..2 {
                    rounded(cr, 124.0 + i as f64 * 26.0, 38.0 + j as f64 * 24.0, 16.0, 16.0, 5.0);
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.75);
                    let _ = cr.fill();
                }
            }
            // Puntero de Android moviéndose por la tablet.
            let px = 150.0 + 26.0 * (t * 1.2).sin() * lit;
            let py = 62.0 + 10.0 * (t * 1.9).cos() * lit;
            cr.move_to(px, py);
            cr.line_to(px, py + 16.0);
            cr.line_to(px + 4.5, py + 12.0);
            cr.line_to(px + 11.0, py + 12.0);
            cr.close_path();
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(0.1, 0.1, 0.15, 0.8);
            cr.set_line_width(1.2);
            let _ = cr.stroke();
            // Teclado.
            rounded(cr, 14.0, 112.0, 150.0, 44.0, 10.0);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.5);
            cr.set_line_width(1.2);
            let _ = cr.stroke();
            for row in 0..3 {
                for k in 0..9 {
                    let lit_key = lit > 0.0 && ((t * 4.0) as i32 + k * 3 + row * 5) % 23 == 0;
                    rounded(cr, 22.0 + k as f64 * 15.5, 119.0 + row as f64 * 11.0, 11.5, 7.5, 2.5);
                    if lit_key {
                        cr.set_source_rgba(color.0, color.1, color.2, 0.95);
                    } else {
                        cr.set_source_rgba(1.0, 1.0, 1.0, 0.55);
                    }
                    let _ = cr.fill();
                }
            }
            // Ratón.
            rounded(cr, 180.0, 110.0, 30.0, 48.0, 15.0);
            let g = cairo::LinearGradient::new(180.0, 110.0, 210.0, 158.0);
            g.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.45);
            g.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.15);
            let _ = cr.set_source(&g);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.6);
            let _ = cr.stroke();
            cr.move_to(195.0, 112.0);
            cr.line_to(195.0, 126.0);
            let _ = cr.stroke();
            // Flecha del PC a la tablet.
            cr.set_source_rgba(color.0, color.1, color.2, 0.55 + 0.4 * lit);
            cr.set_line_width(3.0);
            cr.set_line_cap(cairo::LineCap::Round);
            cr.move_to(40.0, 98.0);
            cr.curve_to(44.0, 60.0, 70.0, 52.0, 92.0, 56.0);
            let _ = cr.stroke();
            cr.move_to(84.0, 48.0);
            cr.line_to(94.0, 56.0);
            cr.line_to(84.0, 64.0);
            let _ = cr.stroke();
        }
    }
    // Luz de estado: ámbar esperando a la tablet, del color del modo conectada.
    let (lc, pulse) = match status {
        Status::Off => return,
        Status::Waiting => ((1.0, 0.74, 0.18), 0.5 + 0.5 * (t * 3.0).sin()),
        Status::Connected => (color, 1.0),
    };
    let (x, y) = (214.0, 12.0);
    let g = cairo::RadialGradient::new(x, y, 0.0, x, y, 14.0);
    g.add_color_stop_rgba(0.0, lc.0, lc.1, lc.2, 0.7 * pulse);
    g.add_color_stop_rgba(1.0, lc.0, lc.1, lc.2, 0.0);
    let _ = cr.set_source(&g);
    cr.arc(x, y, 14.0, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
    cr.set_source_rgba(lc.0, lc.1, lc.2, 0.6 + 0.4 * pulse);
    cr.arc(x, y, 4.5, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
}
