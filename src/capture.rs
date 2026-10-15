//! Captura del monitor virtual a través de PipeWire.
//!
//! Corre en su propio hilo (los objetos de PipeWire no son `Send`). Cada búfer
//! recibido (memoria compartida, BGRx/RGBx) se copia empaquetado (sin padding)
//! a un [`FrameSlot`] que sólo guarda el frame más reciente: si el codificador
//! va atrasado, los frames viejos se descartan en lugar de acumular latencia.
//! La conversión de color la hace cada codificador (GPU en NVENC).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::VideoFormat;
use pw::spa::pod::Pod;

/// Frames recibidos de PipeWire (para estadísticas).
pub static CAPTURED: AtomicU64 = AtomicU64::new(0);
/// Tiempo acumulado copiando búferes de PipeWire, en microsegundos.
pub static COPY_US: AtomicU64 = AtomicU64::new(0);

/// Orden de canales de un píxel de 32 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelOrder {
    Bgrx,
    Rgbx,
}

impl PixelOrder {
    /// Índices (r, g, b) dentro del píxel.
    pub fn rgb_indices(self) -> (usize, usize, usize) {
        match self {
            PixelOrder::Bgrx => (2, 1, 0),
            PixelOrder::Rgbx => (0, 1, 2),
        }
    }

    pub fn gst_format(self) -> &'static str {
        match self {
            PixelOrder::Bgrx => "BGRx",
            PixelOrder::Rgbx => "RGBx",
        }
    }
}

/// Frame de 32 bits por píxel, filas contiguas (stride = width * 4).
pub struct RawFrame {
    pub width: usize,
    pub height: usize,
    pub order: PixelOrder,
    pub data: Vec<u8>,
    pub captured: Instant,
}

/// Frame en formato I420 (planos Y, U y V contiguos, sin padding).
pub struct YuvFrame {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl YuvFrame {
    pub fn planes(&self) -> (&[u8], &[u8], &[u8]) {
        let y = self.width * self.height;
        let c = y / 4;
        (
            &self.data[..y],
            &self.data[y..y + c],
            &self.data[y + c..y + 2 * c],
        )
    }
}

/// Buzón de un solo elemento: el productor sobrescribe, el consumidor espera.
#[derive(Default)]
pub struct FrameSlot {
    frame: Mutex<Option<RawFrame>>,
    ready: Condvar,
}

impl FrameSlot {
    pub fn put(&self, f: RawFrame) {
        *self.frame.lock().unwrap() = Some(f);
        self.ready.notify_one();
    }

    pub fn take_timeout(&self, timeout: Duration) -> Option<RawFrame> {
        let guard = self.frame.lock().unwrap();
        let (mut guard, _) = self
            .ready
            .wait_timeout_while(guard, timeout, |f| f.is_none())
            .unwrap();
        guard.take()
    }
}

/// Tamaño actual del monitor virtual (el negociado con Mutter). Lo leen la
/// inyección de entrada y `/info`.
#[derive(Default)]
pub struct Geometry {
    width: AtomicU32,
    height: AtomicU32,
}

impl Geometry {
    pub fn get(&self) -> (u32, u32) {
        (self.width.load(Ordering::Relaxed), self.height.load(Ordering::Relaxed))
    }

    fn set(&self, w: u32, h: u32) {
        self.width.store(w, Ordering::Relaxed);
        self.height.store(h, Ordering::Relaxed);
    }
}

/// Permite pedir otra resolución desde fuera del hilo de PipeWire.
#[derive(Clone)]
pub struct Capture {
    resize_tx: pw::channel::Sender<(u32, u32)>,
    pub geometry: Arc<Geometry>,
}

impl Capture {
    /// Renegocia el formato con el nuevo tamaño; Mutter cambia el modo del
    /// monitor virtual en caliente (como hace gnome-remote-desktop).
    pub fn resize(&self, width: u32, height: u32) {
        let _ = self.resize_tx.send((width, height));
    }
}

struct StreamState {
    format: spa::param::video::VideoInfoRaw,
    slot: Arc<FrameSlot>,
    geometry: Arc<Geometry>,
}

/// Lanza el hilo de captura conectado al nodo `node_id`.
pub fn spawn(node_id: u32, width: u32, height: u32, fps: u32, slot: Arc<FrameSlot>) -> Capture {
    let (resize_tx, resize_rx) = pw::channel::channel();
    let geometry = Arc::new(Geometry::default());
    geometry.set(width, height);
    let geo = geometry.clone();
    std::thread::Builder::new()
        .name("pipewire-capture".into())
        .spawn(move || {
            if let Err(e) = run(node_id, width, height, fps, slot, geo, resize_rx) {
                tracing::error!("captura PipeWire terminó con error: {e:#}");
                std::process::exit(1);
            }
        })
        .expect("no se pudo crear el hilo de captura");
    Capture { resize_tx, geometry }
}

fn run(
    node_id: u32,
    width: u32,
    height: u32,
    fps: u32,
    slot: Arc<FrameSlot>,
    geometry: Arc<Geometry>,
    resize_rx: pw::channel::Receiver<(u32, u32)>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let stream = pw::stream::StreamRc::new(
        core,
        "spacedesk-linux-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let state = StreamState { format: Default::default(), slot, geometry };

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, old, new| {
            tracing::info!("PipeWire: {old:?} -> {new:?}");
        })
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            if state.format.parse(param).is_ok() {
                let s = state.format.size();
                state.geometry.set(s.width, s.height);
                tracing::info!(
                    "formato negociado: {:?} {}x{} @ {}/{}",
                    state.format.format(),
                    s.width,
                    s.height,
                    state.format.framerate().num,
                    state.format.framerate().denom
                );
            }
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let (offset, size, stride) = {
                let c = d.chunk();
                (c.offset() as usize, c.size() as usize, c.stride())
            };
            if size == 0 {
                return; // búfer sin contenido (frame no modificado / sólo metadatos)
            }
            let Some(bytes) = d.data() else { return };
            let s = state.format.size();
            let (w, h) = (s.width as usize & !1, s.height as usize & !1);
            let stride = if stride > 0 { stride as usize } else { s.width as usize * 4 };
            let end = (offset + size).min(bytes.len());
            if w == 0 || h == 0 || end < offset + stride * (h - 1) + w * 4 {
                return;
            }
            let Some(order) = channel_order(state.format.format()) else { return };
            let captured = Instant::now();
            let src = &bytes[offset..end];
            let row = w * 4;
            let data = if stride == row {
                src[..row * h].to_vec()
            } else {
                let mut v = Vec::with_capacity(row * h);
                for y in 0..h {
                    v.extend_from_slice(&src[y * stride..y * stride + row]);
                }
                v
            };
            CAPTURED.fetch_add(1, Ordering::Relaxed);
            COPY_US.fetch_add(captured.elapsed().as_micros() as u64, Ordering::Relaxed);
            state.slot.put(RawFrame { width: w, height: h, order, data, captured });
        })
        .register()?;

    let format = video_format_pod(width, height, fps)?;
    let mut params = [Pod::from_bytes(&format).ok_or_else(|| anyhow!("pod inválido"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    let resizer = stream.clone();
    let _resize = resize_rx.attach(mainloop.loop_(), move |(w, h)| {
        tracing::info!("cambiando el monitor virtual a {w}x{h}");
        let r = video_format_pod(w, h, fps).map_err(|e| e.to_string()).and_then(|bytes| {
            let pod = Pod::from_bytes(&bytes).ok_or("pod inválido")?;
            resizer.update_params(&mut [pod]).map_err(|e| e.to_string())
        });
        if let Err(e) = r {
            tracing::warn!("no se pudo cambiar la resolución: {e}");
        }
    });

    mainloop.run();
    Ok(())
}

/// EnumFormat: vídeo crudo de 32 bits con tamaño fijo. Mutter usa este tamaño
/// como resolución del monitor virtual.
fn video_format_pod(width: u32, height: u32, fps: u32) -> Result<Vec<u8>> {
    use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            pw::spa::utils::Rectangle { width, height }
        ),
        // Como gnome-remote-desktop: frames a ritmo variable (sólo cuando hay
        // cambios) con un máximo explícito.
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            pw::spa::utils::Fraction { num: 0, denom: 1 }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: fps, denom: 1 },
            pw::spa::utils::Fraction { num: 1, denom: 1 },
            pw::spa::utils::Fraction { num: fps, denom: 1 }
        ),
    );
    Ok(pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner())
}

fn channel_order(f: VideoFormat) -> Option<PixelOrder> {
    match f {
        VideoFormat::BGRx | VideoFormat::BGRA => Some(PixelOrder::Bgrx),
        VideoFormat::RGBx | VideoFormat::RGBA => Some(PixelOrder::Rgbx),
        _ => None,
    }
}

/// Conversión RGB → YUV 4:2:0 BT.601 rango limitado (lo que asumen los
/// decodificadores H.264 sin VUI). Se reparte por bandas entre varios hilos.
pub fn to_i420(frame: &RawFrame) -> YuvFrame {
    let (src, w, h) = (&frame.data[..], frame.width, frame.height);
    let stride = w * 4;
    let (ri, gi, bi) = frame.order.rgb_indices();
    let cw = w / 2;
    let mut data = vec![0u8; w * h + 2 * cw * (h / 2)];
    let (y_plane, uv) = data.split_at_mut(w * h);
    let (u_plane, v_plane) = uv.split_at_mut(cw * (h / 2));

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let pair_rows = h / 2;
    let rows_per_band = pair_rows.div_ceil(threads).max(1);

    std::thread::scope(|scope| {
        let y_bands = y_plane.chunks_mut(rows_per_band * 2 * w);
        let u_bands = u_plane.chunks_mut(rows_per_band * cw);
        let v_bands = v_plane.chunks_mut(rows_per_band * cw);
        for (band, ((yb, ub), vb)) in y_bands.zip(u_bands).zip(v_bands).enumerate() {
            scope.spawn(move || {
                let first_pair = band * rows_per_band;
                for (k, (urow, vrow)) in ub.chunks_mut(cw).zip(vb.chunks_mut(cw)).enumerate() {
                    let row = (first_pair + k) * 2;
                    let s0 = &src[row * stride..];
                    let s1 = &src[(row + 1) * stride..];
                    let (y0, y1) = yb[k * 2 * w..(k * 2 + 2) * w].split_at_mut(w);
                    for x in 0..cw {
                        let mut rs = 0i32;
                        let mut gs = 0i32;
                        let mut bs = 0i32;
                        for (srow, yrow) in [(s0, &mut *y0), (s1, &mut *y1)] {
                            for dx in 0..2 {
                                let p = &srow[(x * 2 + dx) * 4..];
                                let (r, g, b) = (p[ri] as i32, p[gi] as i32, p[bi] as i32);
                                yrow[x * 2 + dx] =
                                    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                                rs += r;
                                gs += g;
                                bs += b;
                            }
                        }
                        let (r, g, b) = (rs >> 2, gs >> 2, bs >> 2);
                        urow[x] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                        vrow[x] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
                    }
                }
            });
        }
    });

    YuvFrame { width: w, height: h, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_and_black_convert_to_limited_range() {
        let (w, h) = (4, 4);
        let mut src = vec![255u8; w * h * 4];
        src[..w * 2 * 4].fill(0); // dos primeras filas negras
        let raw = RawFrame { width: w, height: h, order: PixelOrder::Bgrx, data: src, captured: Instant::now() };
        let f = to_i420(&raw);
        let (y, u, v) = f.planes();
        assert_eq!(y[0], 16);
        assert_eq!(y[w * 3], 235);
        assert!(u.iter().chain(v).all(|&c| c == 128));
    }
}
