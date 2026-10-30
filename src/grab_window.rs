//! Ventana de captura del modo "solo entrada".
//!
//! Una ventana negra a pantalla completa sobre el monitor virtual. Nadie ve
//! ese monitor, pero GNOME le entrega a la ventana todo lo que pasa en él:
//! la posición del puntero, los botones, la rueda y, tras un clic, el teclado.
//! Los eventos se traducen a uso HID y se mandan a `input_only`.
//!
//! Es una ventana X11 (XWayland) porque Wayland no deja a una aplicación
//! fijarse en todas las áreas de trabajo, y con `workspaces-only-on-primary`
//! desactivado la ventana desaparecería al cambiar de escritorio. En X11 se
//! marca como `_NET_WM_STATE_STICKY` y, de paso, fuera del dock y de Alt+Tab.
//!
//! El hilo vive mientras dure el programa (winit sólo permite un bucle de
//! eventos por proceso): `attach` pone la ventana sobre un monitor y `detach`
//! la quita. Cada medio segundo se comprueba que sigue en su monitor, porque
//! al reconectar o al cambiar la disposición Mutter puede moverla.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::platform::x11::EventLoopBuilderExtX11;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::window::{Window, WindowId};

#[derive(Debug)]
pub enum GrabEvent {
    /// El puntero entró en el monitor virtual.
    Enter,
    Leave,
    /// Posición normalizada 0..1 dentro del monitor virtual.
    Motion { u: f64, v: f64 },
    /// Bit HID del botón (0 izquierdo, 1 derecho, 2 central, 3 atrás, 4 adelante).
    Button { bit: u8, down: bool },
    /// Pasos de rueda (positivo: arriba / derecha).
    Wheel { v: i32, h: i32 },
    /// Uso HID del teclado (página 0x07).
    Key { usage: u8, down: bool },
    /// Soltar todas las teclas (la ventana perdió el foco).
    ReleaseKeys,
}

enum Command {
    /// Poner la ventana sobre el monitor con este conector (p. ej. "Meta-0").
    Attach(String),
    Detach,
}

#[derive(Clone)]
pub struct GrabWindow {
    proxy: EventLoopProxy<Command>,
}

impl GrabWindow {
    pub fn attach(&self, connector: &str) {
        let _ = self.proxy.send_event(Command::Attach(connector.to_owned()));
    }

    pub fn detach(&self) {
        let _ = self.proxy.send_event(Command::Detach);
    }
}

/// Lanza el bucle de eventos en un hilo propio.
pub fn spawn(events: UnboundedSender<GrabEvent>) -> anyhow::Result<GrabWindow> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("grab-window".into())
        .spawn(move || {
            let el = match EventLoop::<Command>::with_user_event().with_x11().with_any_thread(true).build() {
                Ok(el) => el,
                Err(e) => {
                    let _ = tx.send(Err(anyhow::anyhow!("no se pudo abrir XWayland: {e}")));
                    return;
                }
            };
            let _ = tx.send(Ok(el.create_proxy()));
            let mut app = App::new(events);
            if let Err(e) = el.run_app(&mut app) {
                tracing::error!("ventana de captura: {e}");
                std::process::exit(1);
            }
        })?;
    Ok(GrabWindow { proxy: rx.recv()?? })
}

struct Surface {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    /// Ya se pidió al gestor de ventanas que la fije en todas las áreas.
    stuck: bool,
    /// Intentos seguidos de recolocarla sin conseguirlo.
    moves: u8,
    /// Niri: id de la ventana ya puesta a pantalla completa.
    niri_fullscreen: Option<u64>,
}

struct App {
    events: UnboundedSender<GrabEvent>,
    target: Option<String>,
    surface: Option<Surface>,
    /// Cuándo se pidió el monitor, para avisar si no aparece.
    since: Instant,
    /// El teclado se reenvía sólo tras un clic en la tablet. GNOME da el foco
    /// a las ventanas nuevas, y sin esto lo primero que escribieras en el PC
    /// acabaría en la tablet.
    armed: bool,
    wheel: (f64, f64),
    x11: Option<X11>,
}

impl App {
    fn new(events: UnboundedSender<GrabEvent>) -> Self {
        Self {
            events,
            target: None,
            surface: None,
            since: Instant::now(),
            armed: false,
            wheel: (0.0, 0.0),
            x11: X11::connect().map_err(|e| tracing::warn!("X11: {e}")).ok(),
        }
    }

    fn send(&self, ev: GrabEvent) {
        let _ = self.events.send(ev);
    }

    /// Rectángulo del monitor buscado, preguntado a RandR en cada llamada
    /// (winit guarda la lista en caché y no ve los monitores nuevos).
    fn monitor(&self) -> Option<MonRect> {
        self.x11.as_ref()?.monitor(self.target.as_deref()?)
    }

    fn create(&mut self, el: &ActiveEventLoop, m: MonRect) -> anyhow::Result<()> {
        tracing::info!("ventana de captura en {} {}x{} ({}, {})", m.name, m.w, m.h, m.x, m.y);
        // Sin el fullscreen de winit (necesita su lista de monitores): se
        // crea encima del monitor y se pide pantalla completa al gestor.
        let attrs = Window::default_attributes()
            .with_title("Encaje (tablet)")
            .with_decorations(false)
            .with_position(PhysicalPosition::new(m.x, m.y))
            .with_inner_size(PhysicalSize::new(m.w, m.h));
        let window = Rc::new(el.create_window(attrs)?);
        let context = softbuffer::Context::new(window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let surface = softbuffer::Surface::new(&context, window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        window.request_redraw();
        self.surface = Some(Surface { window, surface, stuck: false, moves: 0, niri_fullscreen: None });
        self.armed = false;
        Ok(())
    }

    /// La ventana existe pero no ocupa su monitor: volver a ponerla.
    fn check_placement(&mut self) {
        let Some(m) = self.monitor() else { return };
        let (Some(s), Some(x11)) = (self.surface.as_mut(), self.x11.as_ref()) else { return };
        let Ok(RawWindowHandle::Xlib(h)) = s.window.window_handle().map(|h| h.as_raw()) else { return };
        let id = h.window as u32;
        if !s.stuck {
            x11.stick(id);
            s.stuck = true;
            return;
        }
        if std::env::var_os("NIRI_SOCKET").is_some() {
            place_on_niri(&m, &mut s.niri_fullscreen);
            return;
        }
        let pos = s.window.outer_position().unwrap_or_default();
        let size = s.window.inner_size();
        if (pos.x, pos.y, size.width, size.height) == (m.x, m.y, m.w, m.h) {
            s.moves = 0;
        } else if s.moves >= 2 {
            // Mutter no deja llevar una ventana ya mostrada a otro monitor
            // cuando cambia la disposición; una nueva sí cae en su sitio.
            tracing::info!("la ventana de captura no se deja mover; se vuelve a crear");
            self.surface = None;
        } else {
            tracing::info!(
                "la ventana de captura no está sobre {} ({}x{} en {}, {}); recolocando",
                m.name,
                size.width,
                size.height,
                pos.x,
                pos.y
            );
            x11.fullscreen(id, false);
            s.window.set_outer_position(PhysicalPosition::new(m.x, m.y));
            let _ = s.window.request_inner_size(PhysicalSize::new(m.w, m.h));
            s.stuck = false;
            s.moves += 1;
        }
    }

    fn redraw(&mut self) {
        let Some(s) = self.surface.as_mut() else { return };
        let size = s.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if s.surface.resize(w, h).is_err() {
            return;
        }
        if let Ok(mut buf) = s.surface.buffer_mut() {
            buf.fill(0);
            let _ = buf.present();
        }
    }
}

impl ApplicationHandler<Command> for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn user_event(&mut self, el: &ActiveEventLoop, cmd: Command) {
        match cmd {
            Command::Attach(connector) => {
                if self.target.as_deref() != Some(&connector) {
                    self.target = Some(connector);
                    self.since = Instant::now();
                }
            }
            Command::Detach => {
                self.target = None;
                self.surface = None;
            }
        }
        self.about_to_wait(el);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(target) = self.target.clone() else {
            el.set_control_flow(ControlFlow::Wait);
            return;
        };
        if self.surface.is_none() {
            match self.monitor() {
                Some(m) => {
                    if let Err(e) = self.create(el, m) {
                        tracing::error!("no se pudo crear la ventana de captura: {e:#}");
                    }
                }
                None if self.since.elapsed() > Duration::from_secs(10) => {
                    tracing::warn!("no aparece el monitor {target}");
                    self.since = Instant::now();
                }
                None => {}
            }
        } else {
            self.check_placement();
        }
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(500)));
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) => {
                if let Some(s) = &self.surface {
                    s.window.request_redraw();
                }
            }
            WindowEvent::CursorEntered { .. } => self.send(GrabEvent::Enter),
            WindowEvent::CursorLeft { .. } => self.send(GrabEvent::Leave),
            WindowEvent::CursorMoved { position, .. } => {
                let Some(s) = &self.surface else { return };
                let size = s.window.inner_size();
                if size.width == 0 || size.height == 0 {
                    return;
                }
                self.send(GrabEvent::Motion {
                    u: position.x / size.width as f64,
                    v: position.y / size.height as f64,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let bit = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                    MouseButton::Other(_) => return,
                };
                let down = state == ElementState::Pressed;
                if down {
                    self.armed = true;
                }
                self.send(GrabEvent::Button { bit, down });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // winit: y positivo = arriba, x positivo = izquierda.
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64),
                    MouseScrollDelta::PixelDelta(p) => (p.x / 40.0, p.y / 40.0),
                };
                self.wheel.0 += y;
                self.wheel.1 -= x;
                let v = self.wheel.0.trunc();
                let h = self.wheel.1.trunc();
                self.wheel.0 -= v;
                self.wheel.1 -= h;
                if v != 0.0 || h != 0.0 {
                    self.send(GrabEvent::Wheel { v: v as i32, h: h as i32 });
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if !self.armed || event.repeat {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else { return };
                if let Some(usage) = hid_usage(code) {
                    self.send(GrabEvent::Key { usage, down: event.state == ElementState::Pressed });
                }
            }
            WindowEvent::Focused(false) => {
                self.armed = false;
                self.send(GrabEvent::ReleaseKeys);
            }
            _ => {}
        }
    }
}

struct MonRect {
    name: String,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// Conexión X11 propia para pedir al gestor de ventanas lo que winit no sabe.
struct X11 {
    conn: x11rb::rust_connection::RustConnection,
    root: u32,
    atoms: [u32; 6],
}

impl X11 {
    fn connect() -> anyhow::Result<Self> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let names = [
            "_NET_WM_STATE",
            "_NET_WM_STATE_STICKY",
            "_NET_WM_STATE_SKIP_TASKBAR",
            "_NET_WM_STATE_SKIP_PAGER",
            "_NET_WM_DESKTOP",
            "_NET_WM_STATE_FULLSCREEN",
        ];
        let mut atoms = [0; 6];
        for (a, n) in atoms.iter_mut().zip(names) {
            *a = conn.intern_atom(false, n.as_bytes())?.reply()?.atom;
        }
        Ok(Self { conn, root, atoms })
    }

    fn monitor(&self, name: &str) -> Option<MonRect> {
        use x11rb::protocol::randr::ConnectionExt as _;
        use x11rb::protocol::xproto::ConnectionExt as _;
        let reply = self.conn.randr_get_monitors(self.root, true).ok()?.reply().ok()?;
        reply.monitors.iter().find_map(|m| {
            let atom = self.conn.get_atom_name(m.name).ok()?.reply().ok()?;
            (atom.name == name.as_bytes()).then(|| MonRect {
                name: name.to_owned(),
                x: m.x as i32,
                y: m.y as i32,
                w: m.width as u32,
                h: m.height as u32,
            })
        })
    }

    /// Pantalla completa, en todas las áreas de trabajo y fuera del dock y
    /// de Alt+Tab (EWMH).
    fn stick(&self, window: u32) {
        let [state, sticky, skip_taskbar, skip_pager, desktop, _] = self.atoms;
        self.fullscreen(window, true);
        self.send(window, state, [1, sticky, 0, 1, 0]);
        self.send(window, state, [1, skip_taskbar, skip_pager, 1, 0]);
        self.send(window, desktop, [0xFFFF_FFFF, 1, 0, 0, 0]);
    }

    fn fullscreen(&self, window: u32, on: bool) {
        let [state, .., fullscreen] = self.atoms;
        self.send(window, state, [on as u32, fullscreen, 0, 1, 0]);
    }

    fn send(&self, window: u32, kind: u32, data: [u32; 5]) {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask};
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        let _ = self.conn.send_event(false, self.root, mask, ClientMessageEvent::new(32, window, kind, data));
        let _ = self.conn.flush();
    }
}

/// Código físico de tecla → uso HID (USB HID Usage Tables, página 0x07).
fn hid_usage(code: KeyCode) -> Option<u8> {
    use KeyCode::*;
    Some(match code {
        KeyA => 0x04, KeyB => 0x05, KeyC => 0x06, KeyD => 0x07, KeyE => 0x08, KeyF => 0x09,
        KeyG => 0x0A, KeyH => 0x0B, KeyI => 0x0C, KeyJ => 0x0D, KeyK => 0x0E, KeyL => 0x0F,
        KeyM => 0x10, KeyN => 0x11, KeyO => 0x12, KeyP => 0x13, KeyQ => 0x14, KeyR => 0x15,
        KeyS => 0x16, KeyT => 0x17, KeyU => 0x18, KeyV => 0x19, KeyW => 0x1A, KeyX => 0x1B,
        KeyY => 0x1C, KeyZ => 0x1D,
        Digit1 => 0x1E, Digit2 => 0x1F, Digit3 => 0x20, Digit4 => 0x21, Digit5 => 0x22,
        Digit6 => 0x23, Digit7 => 0x24, Digit8 => 0x25, Digit9 => 0x26, Digit0 => 0x27,
        Enter => 0x28, Escape => 0x29, Backspace => 0x2A, Tab => 0x2B, Space => 0x2C,
        Minus => 0x2D, Equal => 0x2E, BracketLeft => 0x2F, BracketRight => 0x30,
        Backslash => 0x31, Semicolon => 0x33, Quote => 0x34, Backquote => 0x35,
        Comma => 0x36, Period => 0x37, Slash => 0x38, CapsLock => 0x39,
        F1 => 0x3A, F2 => 0x3B, F3 => 0x3C, F4 => 0x3D, F5 => 0x3E, F6 => 0x3F,
        F7 => 0x40, F8 => 0x41, F9 => 0x42, F10 => 0x43, F11 => 0x44, F12 => 0x45,
        PrintScreen => 0x46, ScrollLock => 0x47, Pause => 0x48, Insert => 0x49,
        Home => 0x4A, PageUp => 0x4B, Delete => 0x4C, End => 0x4D, PageDown => 0x4E,
        ArrowRight => 0x4F, ArrowLeft => 0x50, ArrowDown => 0x51, ArrowUp => 0x52,
        NumLock => 0x53, NumpadDivide => 0x54, NumpadMultiply => 0x55, NumpadSubtract => 0x56,
        NumpadAdd => 0x57, NumpadEnter => 0x58, Numpad1 => 0x59, Numpad2 => 0x5A,
        Numpad3 => 0x5B, Numpad4 => 0x5C, Numpad5 => 0x5D, Numpad6 => 0x5E, Numpad7 => 0x5F,
        Numpad8 => 0x60, Numpad9 => 0x61, Numpad0 => 0x62, NumpadDecimal => 0x63,
        IntlBackslash => 0x64, ContextMenu => 0x65, IntlRo => 0x87, IntlYen => 0x89,
        AudioVolumeMute => 0x7F, AudioVolumeUp => 0x80, AudioVolumeDown => 0x81,
        ControlLeft => 0xE0, ShiftLeft => 0xE1, AltLeft => 0xE2, SuperLeft => 0xE3,
        ControlRight => 0xE4, ShiftRight => 0xE5, AltRight => 0xE6, SuperRight => 0xE7,
        _ => return None,
    })
}

/// En Niri (`xwayland-satellite`), coloca la ventana `"Encaje (tablet)"` a
/// pantalla completa sobre el monitor virtual sin cambiar el foco del monitor
/// principal.
fn place_on_niri(m: &MonRect, fullscreened: &mut Option<u64>) {
    use std::process::Command;
    let Ok(win_out) = Command::new("niri").args(["msg", "-j", "windows"]).output() else { return };
    let Ok(ws_out) = Command::new("niri").args(["msg", "-j", "workspaces"]).output() else { return };
    let Ok(windows) = serde_json::from_slice::<Vec<serde_json::Value>>(&win_out.stdout) else { return };
    let Ok(workspaces) = serde_json::from_slice::<Vec<serde_json::Value>>(&ws_out.stdout) else { return };

    let Some(win) = windows.iter().find(|w| w["title"].as_str() == Some("Encaje (tablet)")) else {
        return;
    };
    let Some(id) = win["id"].as_u64() else { return };
    let id_str = id.to_string();

    let ws_id = win["workspace_id"].as_u64();
    let cur_output = ws_id.and_then(|wid| {
        workspaces
            .iter()
            .find(|ws| ws["id"].as_u64() == Some(wid))
            .and_then(|ws| ws["output"].as_str())
    });

    if cur_output != Some(m.name.as_str()) {
        let _ = Command::new("niri")
            .args(["msg", "action", "move-window-to-monitor", "--id", &id_str, &m.name])
            .status();
    }

    // `fullscreen-window` alterna: una sola vez por ventana. Comparar el
    // tamaño no basta (con escala o redondeo nunca coincide y la ventana
    // entraría y saldría de pantalla completa en cada comprobación).
    if *fullscreened != Some(id) {
        let _ = Command::new("niri")
            .args(["msg", "action", "fullscreen-window", "--id", &id_str])
            .status();
        *fullscreened = Some(id);
    }
}
