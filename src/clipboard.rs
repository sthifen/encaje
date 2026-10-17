//! Portapapeles del PC para el modo sólo entrada.
//!
//! GNOME no deja a un programa en segundo plano leer el portapapeles de
//! Wayland, pero Mutter lo mantiene sincronizado con el de X11 (XWayland), y
//! ése sí: se lee y se escribe con `arboard` y los cambios se detectan con
//! XFixes, sin necesidad de tener el foco.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use image::ImageEncoder as _;
use tokio::sync::mpsc;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};

/// Lo que se copia: texto o una imagen en PNG (la tablet puede mandar
/// también JPEG o WebP; se convierten al ponerlos en el PC).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Clip {
    Text(String),
    Image(Vec<u8>),
}

/// Más grande no se manda: por el cable tardaría demasiado.
pub const MAX_BYTES: usize = 32 << 20;

enum Cmd {
    /// Cambió el dueño del portapapeles de X11.
    Changed,
    Set(Clip),
}

pub struct Clipboard {
    cmds: std_mpsc::Sender<Cmd>,
}

impl Clipboard {
    /// Pone en el PC lo copiado en la tablet.
    pub fn set(&self, clip: Clip) {
        let _ = self.cmds.send(Cmd::Set(clip));
    }

    /// Manda otra vez lo que haya ahora (al conectar la tablet).
    pub fn resend(&self) {
        let _ = self.cmds.send(Cmd::Changed);
    }
}

/// Devuelve el portapapeles y lo que se va copiando en el PC.
pub fn spawn() -> Result<(Clipboard, mpsc::UnboundedReceiver<Clip>)> {
    let (cmds, rx) = std_mpsc::channel();
    let (tx, changes) = mpsc::unbounded_channel();
    let board = arboard::Clipboard::new().context("portapapeles de X11")?;
    let watch_cmds = cmds.clone();
    std::thread::Builder::new().name("clipboard-watch".into()).spawn(move || {
        if let Err(e) = watch(watch_cmds) {
            tracing::warn!("no se vigilan los cambios del portapapeles: {e:#}");
        }
    })?;
    std::thread::Builder::new().name("clipboard".into()).spawn(move || serve(board, rx, tx))?;
    Ok((Clipboard { cmds }, changes))
}

/// Avisa cada vez que alguien se adueña del portapapeles (al copiar algo en
/// cualquier aplicación, Mutter pasa a ser el dueño en X11).
fn watch(cmds: std_mpsc::Sender<Cmd>) -> Result<()> {
    let (conn, screen) = x11rb::connect(None)?;
    conn.xfixes_query_version(5, 0)?.reply()?;
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id()?;
    conn.create_window(0, win, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new())?;
    let atom = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    conn.xfixes_select_selection_input(
        win,
        atom,
        xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )?;
    conn.flush()?;
    loop {
        if let Event::XfixesSelectionNotify(_) = conn.wait_for_event()? {
            if cmds.send(Cmd::Changed).is_err() {
                return Ok(());
            }
        }
    }
}

fn serve(mut board: arboard::Clipboard, rx: std_mpsc::Receiver<Cmd>, tx: mpsc::UnboundedSender<Clip>) {
    let mut state = State { last: None, set_at: None };
    while let Ok(cmd) = rx.recv() {
        let Cmd::Changed = cmd else {
            if let Cmd::Set(clip) = cmd {
                state.apply(&mut board, clip);
            }
            continue;
        };
        // Copiar suele dar varios avisos seguidos.
        std::thread::sleep(Duration::from_millis(150));
        let pending: Vec<Clip> = rx
            .try_iter()
            .filter_map(|c| match c {
                Cmd::Set(clip) => Some(clip),
                Cmd::Changed => None,
            })
            .collect();
        // El aviso de nuestro propio cambio no cuenta.
        let ours = state.set_at.is_some_and(|t| t.elapsed() < Duration::from_millis(500));
        if !ours {
            if let Some((clip, print)) = read(&mut board) {
                if state.last != Some(print) {
                    state.last = Some(print);
                    tracing::info!("portapapeles: {} del PC", describe(&clip));
                    if tx.send(clip).is_err() {
                        return;
                    }
                }
            }
        }
        for clip in pending {
            state.apply(&mut board, clip);
        }
    }
}

struct State {
    /// Huella de lo último que pasó en cualquier sentido, para no devolver a
    /// la tablet lo que acaba de mandar.
    last: Option<u64>,
    set_at: Option<Instant>,
}

impl State {
    /// Pone en el PC lo copiado en la tablet.
    fn apply(&mut self, board: &mut arboard::Clipboard, clip: Clip) {
        let res = match &clip {
            Clip::Text(t) => {
                self.last = Some(fingerprint(t.as_bytes()));
                board.set_text(t.clone()).map_err(anyhow::Error::from)
            }
            Clip::Image(bytes) => decode(bytes).and_then(|img| {
                self.last = Some(fingerprint(&img.bytes));
                board.set_image(img).map_err(anyhow::Error::from)
            }),
        };
        match res {
            Ok(()) => {
                self.set_at = Some(Instant::now());
                tracing::info!("portapapeles: {} de la tablet", describe(&clip));
            }
            Err(e) => tracing::warn!("portapapeles: no se pudo poner lo de la tablet: {e:#}"),
        }
    }
}

/// Lo que hay en el portapapeles: una imagen si la hay (al copiar una imagen
/// del navegador viene también su HTML), si no, el texto.
fn read(board: &mut arboard::Clipboard) -> Option<(Clip, u64)> {
    if let Ok(img) = board.get_image() {
        let print = fingerprint(&img.bytes);
        match encode(&img) {
            Ok(png) if png.len() <= MAX_BYTES => return Some((Clip::Image(png), print)),
            Ok(_) => tracing::info!("portapapeles: imagen demasiado grande, no se manda"),
            Err(e) => tracing::warn!("portapapeles: {e:#}"),
        }
        return None;
    }
    let text = board.get_text().ok().filter(|t| !t.is_empty() && t.len() <= MAX_BYTES)?;
    let print = fingerprint(text.as_bytes());
    Some((Clip::Text(text), print))
}

fn fingerprint(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

fn encode(img: &arboard::ImageData) -> Result<Vec<u8>> {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(&img.bytes, img.width as u32, img.height as u32, image::ExtendedColorType::Rgba8)
    .context("no se pudo codificar la imagen")?;
    Ok(png)
}

fn decode(bytes: &[u8]) -> Result<arboard::ImageData<'static>> {
    let img = image::load_from_memory(bytes).context("imagen de la tablet ilegible")?.into_rgba8();
    let (w, h) = img.dimensions();
    Ok(arboard::ImageData { width: w as usize, height: h as usize, bytes: img.into_raw().into() })
}

fn describe(clip: &Clip) -> String {
    match clip {
        Clip::Text(t) => format!("texto ({} caracteres)", t.chars().count()),
        Clip::Image(b) => format!("imagen ({} KB)", b.len() / 1024),
    }
}

