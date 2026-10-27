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

/// Lanza el hilo de captura conectado al nodo `node_id`. Si el flujo falla
/// (p. ej. Mutter cerró la sesión) termina el proceso, salvo con
/// `exit_on_error = false`, para quien vigila `VirtualDisplay::closed`.
///
/// Con `follow = false` se pide exactamente `width`x`height` y el compositor
/// pone el monitor a ese tamaño (Mutter). Con `follow = true` se acepta el
/// tamaño que ofrezca, que sigue al modo del monitor (KWin).
pub fn spawn(node_id: u32, width: u32, height: u32, fps: u32, follow: bool, slot: Arc<FrameSlot>, exit_on_error: bool) -> Capture {
    let (resize_tx, resize_rx) = pw::channel::channel();
    let geometry = Arc::new(Geometry::default());
    geometry.set(width, height);
    let geo = geometry.clone();
    std::thread::Builder::new()
        .name("pipewire-capture".into())
        .spawn(move || {
            if let Err(e) = run(node_id, (width, height), fps, follow, slot, geo, resize_rx) {
                if exit_on_error {
                    tracing::error!("captura PipeWire terminó con error: {e:#}");
                    std::process::exit(1);
                }
                tracing::debug!("captura PipeWire terminó: {e:#}");
            }
        })
        .expect("no se pudo crear el hilo de captura");
    Capture { resize_tx, geometry }
}

fn run(
    node_id: u32,
    (width, height): (u32, u32),
    fps: u32,
    follow: bool,
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
        "encaje-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let state = StreamState { format: Default::default(), slot, geometry };

    // Si Mutter cierra la sesión el flujo se desconecta: terminar el hilo.
    let quit = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(move |_, _, old, new| {
            tracing::info!("PipeWire: {old:?} -> {new:?}");
            if matches!(new, pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected) {
                quit.quit();
            }
        })
        .param_changed(|stream, state, id, param| {
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
            if let Ok(buf_bytes) = buffers_param_pod()
                && let Some(pod) = Pod::from_bytes(&buf_bytes)
            {
                let _ = stream.update_params(&mut [pod]);
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
            let s = state.format.size();
            let (w, h) = (s.width as usize & !1, s.height as usize & !1);
            let stride = if stride > 0 { stride as usize } else { s.width as usize * 4 };
            if w == 0 || h == 0 {
                return;
            }
            let Some(order) = channel_order(state.format.format()) else { return };
            let needed = offset + stride * (h - 1) + w * 4;
            let captured = Instant::now();
            let data = if let Some(bytes) = d.data() {
                let end = (offset + size).min(bytes.len());
                if end < needed {
                    return;
                }
                copy_frame(&bytes[offset..end], w, h, stride)
            } else if d.fd() >= 0 {
                let map_len = (offset + size).max(needed);
                let is_dmabuf = d.type_() == spa::buffer::DataType::DmaBuf;
                let Some(v) = copy_fd_frame(d.fd() as i32, map_len, offset, w, h, stride, is_dmabuf) else {
                    return;
                };
                v
            } else {
                return;
            };
            CAPTURED.fetch_add(1, Ordering::Relaxed);
            COPY_US.fetch_add(captured.elapsed().as_micros() as u64, Ordering::Relaxed);
            state.slot.put(RawFrame { width: w, height: h, order, data, captured });
        })
        .register()?;

    let format_dmabuf = video_format_pod(width, height, fps, follow, true)?;
    let format_shm = video_format_pod(width, height, fps, follow, false)?;
    let mut params = [
        Pod::from_bytes(&format_dmabuf).ok_or_else(|| anyhow!("pod dmabuf inválido"))?,
        Pod::from_bytes(&format_shm).ok_or_else(|| anyhow!("pod shm inválido"))?,
    ];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT,
        &mut params,
    )?;

    let resizer = stream.clone();
    let _resize = resize_rx.attach(mainloop.loop_(), move |(w, h)| {
        tracing::info!("cambiando el monitor virtual a {w}x{h}");
        let r = (|| -> Result<(), String> {
            let b_dma = video_format_pod(w, h, fps, follow, true).map_err(|e| e.to_string())?;
            let b_shm = video_format_pod(w, h, fps, follow, false).map_err(|e| e.to_string())?;
            let p1 = Pod::from_bytes(&b_dma).ok_or("pod inválido")?;
            let p2 = Pod::from_bytes(&b_shm).ok_or("pod inválido")?;
            resizer.update_params(&mut [p1, p2]).map_err(|e| e.to_string())
        })();
        if let Err(e) = r {
            tracing::warn!("no se pudo cambiar la resolución: {e}");
        }
    });

    mainloop.run();
    Ok(())
}

fn copy_frame(src: &[u8], w: usize, h: usize, stride: usize) -> Vec<u8> {
    let row = w * 4;
    if stride == row {
        src[..row * h].to_vec()
    } else {
        let mut v = Vec::with_capacity(row * h);
        for y in 0..h {
            v.extend_from_slice(&src[y * stride..y * stride + row]);
        }
        v
    }
}

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0 << 2;
const DMA_BUF_SYNC_END: u64 = 1 << 2;
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x40086200;

fn copy_fd_frame(
    fd: i32,
    map_len: usize,
    offset: usize,
    w: usize,
    h: usize,
    stride: usize,
    is_dmabuf: bool,
) -> Option<Vec<u8>> {
    if fd < 0 || map_len == 0 {
        return None;
    }
    unsafe {
        if is_dmabuf {
            let sync = DmaBufSync { flags: DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ };
            let _ = libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
        }
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            map_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            if is_dmabuf {
                let sync = DmaBufSync { flags: DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ };
                let _ = libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
            }
            return None;
        }
        let slice = std::slice::from_raw_parts(ptr as *const u8, map_len);
        let out = copy_frame(&slice[offset..], w, h, stride);
        libc::munmap(ptr, map_len);
        if is_dmabuf {
            let sync = DmaBufSync { flags: DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ };
            let _ = libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
        }
        Some(out)
    }
}

fn buffers_param_pod() -> Result<Vec<u8>> {
    use pw::spa::buffer::DataType;
    use pw::spa::pod::{ChoiceValue, Property};
    use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let mask = (1 << DataType::MemFd.as_raw())
        | (1 << DataType::MemPtr.as_raw())
        | (1 << DataType::DmaBuf.as_raw());
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamBuffers,
        pw::spa::param::ParamType::Buffers,
        Property::new(
            pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            pw::spa::pod::Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: mask,
                    flags: vec![mask],
                },
            ))),
        ),
    );
    Ok(pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner())
}

/// EnumFormat: vídeo crudo de 32 bits. Con tamaño fijo Mutter lo usa como
/// resolución del monitor virtual; con `follow` se acepta el tamaño y la
/// frecuencia de refresco que ofrezca el compositor (p. ej. 59.932 Hz CVT en
/// Niri o KWin). Si `with_linear_modifier` es `true`, ofrece el modificador
/// `DRM_FORMAT_MOD_LINEAR` (0) para compositores que exigen DMABUF (Niri).
fn video_format_pod(
    width: u32,
    height: u32,
    fps: u32,
    follow: bool,
    with_linear_modifier: bool,
) -> Result<Vec<u8>> {
    use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pw::spa::pod::{ChoiceValue, Property, PropertyFlags};
    use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Rectangle};
    let mut obj = if follow {
        pw::spa::pod::object!(
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
        )
    } else {
        pw::spa::pod::object!(
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
            pw::spa::pod::property!(FormatProperties::VideoSize, Rectangle, Rectangle { width, height }),
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
        )
    };
    if with_linear_modifier {
        obj.properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::from_bits_retain(0x18), // MANDATORY (0x08) | DONT_FIXATE (0x10)
            value: pw::spa::pod::Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: 0,
                    alternatives: vec![0],
                },
            ))),
        });
    }
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
