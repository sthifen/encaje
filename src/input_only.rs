//! Modo "solo entrada" (`--input-only`): la tablet sigue mostrando su propio
//! Android y el ratón y el teclado del PC pasan a ella al entrar en el
//! monitor virtual, como si fuera una pantalla más a un lado.
//!
//! * Se crea el monitor virtual igual que en el modo normal, pero no se
//!   codifica ni se envía nada: sólo sirve para que GNOME sepa dónde está
//!   la tablet. `grab_window` pone encima una ventana que recoge la entrada.
//! * En la tablet, `android/server` (lanzado por adb con permisos de shell)
//!   crea un ratón y un teclado HID virtuales. Android los trata como
//!   periféricos de verdad: dibuja su propio puntero.
//!
//! Android ignora los ratones HID absolutos, así que el ratón es relativo.
//! El servidor desactiva la aceleración y mide cuántos píxeles avanza el
//! puntero por unidad. Con eso basta para llevar la cuenta de dónde está y
//! mandar en cada movimiento la diferencia hasta donde debería estar. Al
//! entrar en el monitor, el puntero se manda primero a la esquina (0, 0)
//! para partir de una posición conocida.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};

use crate::adb::{adb, close_app, list_devices};
use crate::grab_window::{self, GrabEvent};
use crate::clipboard::{self, Clip};
use crate::{backend, capture, events};

const SERVER_JAR: &[u8] = include_bytes!("../android/input-server.jar");
const REMOTE_JAR: &str = "/data/local/tmp/encaje-input.jar";
const SERVER_CLASS: &str = "dev.encaje.InputServer";
const SOCKET: &str = "localabstract:encaje-input";

const DEV_MOUSE: u8 = 0;
const DEV_KEYBOARD: u8 = 1;
const DEV_CLIPBOARD: u8 = 2;

/// Tamaño y escala del puntero que informa la tablet.
#[derive(Clone, Copy, Debug)]
struct TabletInfo {
    /// Píxeles que avanza el puntero de Android por unidad de movimiento HID.
    scale: f64,
    width: u32,
    height: u32,
}

impl TabletInfo {
    /// Tamaño del monitor virtual: el de la tablet en unidades de ratón.
    /// Así el puntero del PC recorre la tablet a la misma velocidad que un
    /// ratón conectado a ella.
    fn monitor_size(self) -> (u32, u32) {
        let w = (self.width as f64 / self.scale).round() as u32;
        let h = (self.height as f64 / self.scale).round() as u32;
        (w.max(320) & !1, h.max(240) & !1)
    }
}

/// Se pidió cerrar (Ctrl+C o SIGINT de la interfaz). Un `ctrl_c()` nuevo en
/// cada vuelta perdería la señal si llega mientras se hace otra cosa.
async fn quit_requested() {
    static QUIT: OnceLock<watch::Receiver<bool>> = OnceLock::new();
    let mut rx = QUIT
        .get_or_init(|| {
            let (tx, rx) = watch::channel(false);
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = tx.send(true);
            });
            rx
        })
        .clone();
    let _ = rx.wait_for(|q| *q).await;
}

/// Cuánto se mantiene el monitor virtual tras perder la tablet. Con un cable
/// flojo se reconecta antes y no cambia nada de sitio.
const GRACE: Duration = Duration::from_secs(15);

/// El monitor virtual y lo que cuelga de él.
struct Monitor {
    display: Box<dyn backend::VirtualDisplay>,
    capture: capture::Capture,
    size: (u32, u32),
}

impl Monitor {
    async fn create(env: &backend::Environment, size: (u32, u32)) -> Result<Self> {
        let display = backend::create(env, size.0, size.1).await?;
        // Hay que consumir el nodo de PipeWire para que el monitor exista. Con
        // el framerate variable de Mutter, y la ventana negra encima, casi no
        // llegan frames.
        let capture = capture::spawn(
            display.node_id(),
            size.0,
            size.1,
            5,
            display.capture_follows_output(),
            Arc::new(capture::FrameSlot::default()),
            false,
        );
        events::set_monitor(display.placed());
        Ok(Self { display, capture, size })
    }

    fn connector(&self) -> Option<String> {
        self.display.placed().borrow().as_ref().map(|m| m.connector.clone())
    }
}

enum End {
    Quit,
    /// Se fue la tablet (cable, reinicio de adb…).
    Lost,
    /// El escritorio cerró el monitor (en GNOME, "Dejar de compartir").
    Closed,
}

pub async fn run(env: &backend::Environment, fixed: Option<(u32, u32)>) -> Result<()> {
    if Command::new("adb").arg("version").output().await.is_err() {
        bail!("adb no encontrado: instala android-tools");
    }
    let (events_tx, mut events) = mpsc::unbounded_channel();
    // Registrar ya el manejador de la señal.
    let _ = tokio::time::timeout(Duration::ZERO, quit_requested()).await;
    let grab = grab_window::spawn(events_tx)?;
    let mut clip = match clipboard::spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("sin portapapeles compartido: {e:#}");
            None
        }
    };
    let mut monitor: Option<Monitor> = None;
    let mut lost_at: Option<Instant> = None;
    let mut waiting_logged = false;
    events::emit("waiting");

    let end = 'outer: loop {
        // 1. Esperar a la tablet. Si tarda más de GRACE, quitar el monitor.
        let serial = loop {
            if let Some(serial) = pick_device().await {
                break serial;
            }
            if !waiting_logged {
                tracing::info!("esperando a la tablet (USB con la depuración activada)…");
                waiting_logged = true;
            }
            if lost_at.is_some_and(|t| t.elapsed() > GRACE) {
                if let Some(m) = monitor.take() {
                    tracing::info!("la tablet no volvió: se quita el monitor virtual");
                    grab.detach();
                    m.display.stop().await;
                    events::emit("waiting");
                }
                lost_at = None;
            }
            let closed = async {
                match &monitor {
                    Some(m) => m.display.closed().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = quit_requested() => break 'outer End::Quit,
                _ = closed => break 'outer End::Closed,
            }
        };
        waiting_logged = false;

        // 2. Servidor en la tablet.
        let (mut server, port) = match start_server(&serial).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("{e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let result = session(env, fixed, port, &mut monitor, &grab, &mut events, &mut clip).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), server.wait()).await;
        let _ = adb(&["-s", &serial, "forward", "--remove", &format!("tcp:{port}")]).await;
        match result {
            Ok(End::Lost) => {
                tracing::warn!("se perdió la tablet; esperando a que vuelva…");
                events::emit("disconnected");
                lost_at = Some(Instant::now());
            }
            Ok(end) => break end,
            Err(e) => {
                tracing::warn!("{e:#}");
                events::emit("disconnected");
                lost_at.get_or_insert_with(Instant::now);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };

    grab.detach();
    if let Some(m) = monitor {
        m.display.stop().await;
    }
    match end {
        End::Closed => bail!("{}", backend::CLOSED_MESSAGE),
        _ => Ok(()),
    }
}

/// Una conexión con la tablet. Crea el monitor virtual si no existe (o le
/// cambia el tamaño) y reenvía la entrada hasta que algo termine.
async fn session(
    env: &backend::Environment,
    fixed: Option<(u32, u32)>,
    port: u16,
    monitor: &mut Option<Monitor>,
    grab: &grab_window::GrabWindow,
    events: &mut mpsc::UnboundedReceiver<GrabEvent>,
    clip: &mut Option<(clipboard::Clipboard, mpsc::UnboundedReceiver<Clip>)>,
) -> Result<End> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .context("no se pudo conectar con el servidor de la tablet")?;
    stream.set_nodelay(true)?;
    let (rx, tx) = stream.into_split();
    let mut infos = read_tablet(rx);

    let Some(TabletMsg::Info(info)) = infos.recv().await else {
        bail!("la tablet cerró la conexión al empezar");
    };
    tracing::info!(
        "tablet: {}x{}, el puntero avanza {:.3} px por unidad",
        info.width,
        info.height,
        info.scale
    );

    let size = fixed.unwrap_or_else(|| info.monitor_size());
    match monitor {
        Some(m) if m.size != size => {
            m.display.resize(size.0, size.1, &m.capture).await;
            m.size = size;
        }
        Some(_) => {}
        None => {
            *monitor = Some(Monitor::create(env, size).await?);
            tracing::info!(
                "listo: mueve el ratón hacia la tablet (colócala en Configuración → Pantallas). \
                 Para escribir en ella, haz clic en ella."
            );
        }
    }
    let m = monitor.as_mut().unwrap();
    let mut placed = m.display.placed();
    placed.mark_changed();

    // Eventos de la ventana que quedaran de antes de reconectar.
    while events.try_recv().is_ok() {}
    events::connected();
    // Lo copiado en el PC mientras no estaba la tablet: basta con lo de ahora.
    if let Some((board, changes)) = clip.as_mut() {
        while changes.try_recv().is_ok() {}
        board.resend();
    }
    let (board, mut changes) = match clip.as_mut() {
        Some((b, c)) => (Some(&*b), Some(c)),
        None => (None, None),
    };

    let mut out = Output { tx, pointer: Pointer::new(info), keys: Keys::default() };
    let end = loop {
        tokio::select! {
            ev = events.recv() => {
                let Some(ev) = ev else { break End::Quit };
                if out.handle(ev).await.is_err() {
                    break End::Lost;
                }
            }
            Some(c) = async {
                match changes.as_mut() {
                    Some(c) => c.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if out.clip(&c).await.is_err() {
                    break End::Lost;
                }
            }
            msg = infos.recv() => match msg {
                Some(TabletMsg::Clip(c)) => {
                    if let Some(b) = board {
                        b.set(c);
                    }
                }
                Some(TabletMsg::Info(info)) => {
                    tracing::info!("la tablet giró: {}x{}", info.width, info.height);
                    out.pointer.update(info);
                    if fixed.is_none() {
                        let (w, h) = info.monitor_size();
                        m.display.resize(w, h, &m.capture).await;
                        m.size = (w, h);
                    }
                }
                None => break End::Lost,
            },
            Ok(()) = placed.changed() => {
                if let Some(c) = m.connector() {
                    grab.attach(&c);
                }
            }
            _ = m.display.closed() => break End::Closed,
            _ = quit_requested() => {
                tracing::info!("cerrando…");
                break End::Quit;
            }
        }
    };
    // Al cerrar la conexión el servidor quita los dispositivos, restaura la
    // aceleración y sale solo; matarlo antes dejaría la tablet sin ella.
    let _ = out.keys.clear();
    drop(out);
    Ok(end)
}

enum TabletMsg {
    Info(TabletInfo),
    Clip(Clip),
}

/// Lee en otra tarea los mensajes de la tablet: `read_exact` no se puede
/// cancelar a medias dentro de un `select!` sin perder bytes.
fn read_tablet(mut rx: OwnedReadHalf) -> mpsc::Receiver<TabletMsg> {
    let (tx, out) = mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            match read_msg(&mut rx).await {
                Ok(Some(msg)) => {
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("{e:#}");
                    break;
                }
            }
        }
    });
    out
}

/// Mensajes de la tablet (big endian):
///   [u8 1] [f32 escala] [u16 ancho] [u16 alto]
///   [u8 2] [u8 tipo (0 texto, 1 imagen)] [u32 longitud] [datos]
async fn read_msg(rx: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<TabletMsg>> {
    let kind = match rx.read_u8().await {
        Ok(k) => k,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match kind {
        1 => {
            let mut buf = [0u8; 8];
            rx.read_exact(&mut buf).await?;
            Ok(Some(TabletMsg::Info(TabletInfo {
                scale: f32::from_be_bytes(buf[0..4].try_into().unwrap()) as f64,
                width: u16::from_be_bytes([buf[4], buf[5]]) as u32,
                height: u16::from_be_bytes([buf[6], buf[7]]) as u32,
            })))
        }
        DEV_CLIPBOARD => {
            let what = rx.read_u8().await?;
            let len = rx.read_u32().await? as usize;
            if len > clipboard::MAX_BYTES {
                bail!("portapapeles de la tablet demasiado grande ({len} bytes)");
            }
            let mut data = vec![0u8; len];
            rx.read_exact(&mut data).await?;
            Ok(Some(TabletMsg::Clip(match what {
                0 => Clip::Text(String::from_utf8_lossy(&data).into_owned()),
                _ => Clip::Image(data),
            })))
        }
        k => bail!("mensaje desconocido de la tablet: {k}"),
    }
}

// ---- Entrada → informes HID ----

struct Output {
    tx: OwnedWriteHalf,
    pointer: Pointer,
    keys: Keys,
}

impl Output {
    async fn handle(&mut self, ev: GrabEvent) -> Result<()> {
        match ev {
            GrabEvent::Enter => self.pointer.lost(),
            GrabEvent::Leave => {}
            GrabEvent::Motion { u, v } => {
                for (dx, dy) in self.pointer.move_to(u, v) {
                    self.mouse(dx, dy, 0, 0).await?;
                }
            }
            GrabEvent::Button { bit, down } => {
                if down {
                    self.pointer.buttons |= 1 << bit;
                } else {
                    self.pointer.buttons &= !(1 << bit);
                }
                self.mouse(0, 0, 0, 0).await?;
            }
            GrabEvent::Wheel { v, h } => {
                self.mouse(0, 0, v.clamp(-127, 127) as i8, h.clamp(-127, 127) as i8).await?;
            }
            GrabEvent::Key { usage, down } => {
                if self.keys.set(usage, down) {
                    self.keyboard().await?;
                }
            }
            GrabEvent::ReleaseKeys => {
                if self.keys.clear() {
                    self.keyboard().await?;
                }
            }
        }
        Ok(())
    }

    async fn mouse(&mut self, dx: i16, dy: i16, wheel: i8, pan: i8) -> Result<()> {
        let [x0, x1] = dx.to_le_bytes();
        let [y0, y1] = dy.to_le_bytes();
        let report = [self.pointer.buttons, x0, x1, y0, y1, wheel as u8, pan as u8];
        self.send(DEV_MOUSE, &report).await
    }

    async fn keyboard(&mut self) -> Result<()> {
        let report = self.keys.report();
        self.send(DEV_KEYBOARD, &report).await
    }

    /// [u8 2] [u8 tipo (0 texto, 1 imagen)] [u32 longitud] [datos]
    async fn clip(&mut self, clip: &Clip) -> Result<()> {
        let (what, data) = match clip {
            Clip::Text(t) => (0u8, t.as_bytes()),
            Clip::Image(png) => (1u8, png.as_slice()),
        };
        let mut msg = Vec::with_capacity(6 + data.len());
        msg.extend_from_slice(&[DEV_CLIPBOARD, what]);
        msg.extend_from_slice(&(data.len() as u32).to_be_bytes());
        msg.extend_from_slice(data);
        self.tx.write_all(&msg).await?;
        Ok(())
    }

    async fn send(&mut self, dev: u8, report: &[u8]) -> Result<()> {
        let mut msg = Vec::with_capacity(2 + report.len());
        msg.extend_from_slice(&[dev, report.len() as u8]);
        msg.extend_from_slice(report);
        self.tx.write_all(&msg).await?;
        Ok(())
    }
}

/// Posición estimada del puntero de Android, en píxeles de la tablet.
struct Pointer {
    info: TabletInfo,
    at: Option<(f64, f64)>,
    buttons: u8,
}

impl Pointer {
    fn new(info: TabletInfo) -> Self {
        Self { info, at: None, buttons: 0 }
    }

    fn update(&mut self, info: TabletInfo) {
        self.info = info;
        self.at = None;
    }

    /// Ya no se sabe dónde está (al volver a entrar: el puntero pudo moverse
    /// con otro ratón, o la pantalla girar).
    fn lost(&mut self) {
        self.at = None;
    }

    /// Movimientos relativos para llevar el puntero a (u, v) normalizado.
    fn move_to(&mut self, u: f64, v: f64) -> Vec<(i16, i16)> {
        let k = self.info.scale;
        let max = ((self.info.width - 1) as f64, (self.info.height - 1) as f64);
        let target = (u.clamp(0.0, 1.0) * max.0, v.clamp(0.0, 1.0) * max.1);
        let mut moves = Vec::new();
        let mut at = match self.at {
            Some(at) => at,
            None => {
                // Contra la esquina: Android recorta en el borde.
                moves.push((i16::MIN + 1, i16::MIN + 1));
                (0.0, 0.0)
            }
        };
        loop {
            let dx = ((target.0 - at.0) / k).round().clamp(-32767.0, 32767.0);
            let dy = ((target.1 - at.1) / k).round().clamp(-32767.0, 32767.0);
            if dx == 0.0 && dy == 0.0 {
                break;
            }
            moves.push((dx as i16, dy as i16));
            let next = ((at.0 + dx * k).clamp(0.0, max.0), (at.1 + dy * k).clamp(0.0, max.1));
            if next == at {
                break;
            }
            at = next;
        }
        self.at = Some(at);
        moves
    }
}

/// Estado del teclado HID: modificadores y hasta 6 teclas pulsadas.
#[derive(Default)]
struct Keys {
    modifiers: u8,
    pressed: Vec<u8>,
}

impl Keys {
    /// Devuelve si el estado cambió.
    fn set(&mut self, usage: u8, down: bool) -> bool {
        if (0xE0..=0xE7).contains(&usage) {
            let bit = 1 << (usage - 0xE0);
            let before = self.modifiers;
            if down {
                self.modifiers |= bit;
            } else {
                self.modifiers &= !bit;
            }
            return before != self.modifiers;
        }
        let pos = self.pressed.iter().position(|&k| k == usage);
        match (down, pos) {
            (true, None) if self.pressed.len() < 6 => self.pressed.push(usage),
            (false, Some(i)) => {
                self.pressed.remove(i);
            }
            _ => return false,
        }
        true
    }

    fn clear(&mut self) -> bool {
        let changed = self.modifiers != 0 || !self.pressed.is_empty();
        self.modifiers = 0;
        self.pressed.clear();
        changed
    }

    fn report(&self) -> [u8; 8] {
        let mut r = [0u8; 8];
        r[0] = self.modifiers;
        r[2..2 + self.pressed.len()].copy_from_slice(&self.pressed);
        r
    }
}

// ---- adb ----

async fn pick_device() -> Option<String> {
    if let Ok(serial) = std::env::var("ANDROID_SERIAL") {
        return list_devices().await.contains(&serial).then_some(serial);
    }
    let devices = list_devices().await;
    if devices.len() > 1 {
        tracing::warn!("hay varios dispositivos; se usa {} (elige otro con ANDROID_SERIAL)", devices[0]);
    }
    devices.into_iter().next()
}

/// Sube el servidor, lo arranca y abre un `adb forward` hacia su socket.
/// El proceso de adb se mata al soltar el `Child`.
async fn start_server(serial: &str) -> Result<(Child, u16)> {
    let local: PathBuf = std::env::temp_dir().join(format!("encaje-input-{}.jar", std::process::id()));
    std::fs::write(&local, SERVER_JAR)?;
    let pushed = adb(&["-s", serial, "push", &local.to_string_lossy(), REMOTE_JAR]).await;
    let _ = std::fs::remove_file(&local);
    pushed.context("adb push del servidor falló")?;

    // La app del visor (modo segundo monitor) no hace falta aquí.
    close_app(serial).await;

    // Un servidor de una ejecución anterior tendría ocupado el socket.
    let _ = adb(&["-s", serial, "shell", &format!("pkill -f {SERVER_CLASS}")]).await;

    let mut child = Command::new("adb")
        .args(["-s", serial, "shell", &format!("CLASSPATH={REMOTE_JAR} app_process / {SERVER_CLASS}")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // En su propio grupo, para que Ctrl+C no lo mate antes de que el
        // servidor restaure la tablet.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .context("no se pudo lanzar adb shell")?;
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line()).await {
            Ok(Ok(Some(l))) if l.trim() == "listo" => break,
            Ok(Ok(Some(l))) => tracing::info!("tablet: {l}"),
            Ok(_) => bail!("el servidor de la tablet terminó al arrancar"),
            Err(_) => bail!("el servidor de la tablet no respondió"),
        }
    }
    tokio::spawn(async move {
        while let Ok(Some(l)) = lines.next_line().await {
            tracing::info!("tablet: {l}");
        }
    });

    let port = adb(&["-s", serial, "forward", "tcp:0", SOCKET])
        .await
        .and_then(|p| p.trim().parse().ok())
        .context("adb forward falló")?;
    Ok((child, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB_S8: TabletInfo = TabletInfo { scale: 2.0416, width: 2800, height: 1752 };

    /// Simula el puntero de Android: suma cada movimiento por la escala y recorta.
    fn apply(at: &mut (f64, f64), moves: &[(i16, i16)]) {
        for &(dx, dy) in moves {
            at.0 = (at.0 + dx as f64 * TAB_S8.scale).clamp(0.0, 2799.0);
            at.1 = (at.1 + dy as f64 * TAB_S8.scale).clamp(0.0, 1751.0);
        }
    }

    #[test]
    fn android_pointer_follows_pc_pointer() {
        let mut p = Pointer::new(TAB_S8);
        let mut android = (1234.0, 567.0); // posición desconocida para el PC
        for &(u, v) in &[(0.5, 0.5), (0.0, 0.0), (1.0, 1.0), (0.25, 0.8), (0.2501, 0.8)] {
            apply(&mut android, &p.move_to(u, v));
            assert!((android.0 - u * 2799.0).abs() <= TAB_S8.scale, "{android:?} vs {u}");
            assert!((android.1 - v * 1751.0).abs() <= TAB_S8.scale, "{android:?} vs {v}");
            assert_eq!(p.at.unwrap(), android);
        }
    }

    #[test]
    fn monitor_matches_tablet_in_mouse_units() {
        assert_eq!(TAB_S8.monitor_size(), (1370, 858));
    }

    #[test]
    fn keyboard_report() {
        let mut k = Keys::default();
        assert!(k.set(0xE1, true)); // Shift
        assert!(k.set(0x0B, true)); // H
        assert!(!k.set(0x0B, true));
        assert_eq!(k.report(), [0x02, 0, 0x0B, 0, 0, 0, 0, 0]);
        assert!(k.set(0x0B, false));
        assert!(k.clear());
        assert_eq!(k.report(), [0; 8]);
        assert!(!k.clear());
    }
}
