//! Reescritura del SPS de H.264 para que los decodificadores no retengan frames.
//!
//! Sin `bitstream_restriction` en el VUI, un decodificador que sigue la norma
//! al pie de la letra debe suponer que puede haber frames reordenados y
//! guarda la salida hasta llenar el DPB (hasta 16 frames). Así lo hace el
//! decodificador por hardware de MediaTek de muchas tablets: con 90 fps eran
//! ~130 ms de retraso sólo por esperar. Ni NVENC ni OpenH264 escriben esos
//! campos, así que se añaden aquí: `max_num_reorder_frames = 0` y
//! `max_dec_frame_buffering = max_num_ref_frames` (lo mismo que hace WebRTC
//! en Chrome, `SpsVuiRewriter`). El resto del SPS se copia bit a bit.

use anyhow::{Result, bail};

// ── Bits ────────────────────────────────────────────────────────────────────

struct Reader<'a> {
    data: &'a [u8],
    pos: usize, // en bits
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Result<u32> {
        let Some(&byte) = self.data.get(self.pos / 8) else { bail!("SPS truncado") };
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(b as u32)
    }

    fn bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                bail!("exp-Golomb inválido");
            }
        }
        Ok((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    fn se(&mut self) -> Result<i32> {
        let k = self.ue()?;
        Ok(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    bits: usize,
}

impl Writer {
    fn bit(&mut self, b: u32) {
        if self.bits % 8 == 0 {
            self.out.push(0);
        }
        if b & 1 == 1 {
            *self.out.last_mut().unwrap() |= 1 << (7 - self.bits % 8);
        }
        self.bits += 1;
    }

    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit(v >> i);
        }
    }

    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        self.bits(0, len - 1);
        for i in (0..len).rev() {
            self.bit((x >> i) as u32);
        }
    }

    /// rbsp_trailing_bits: un 1 y ceros hasta completar el byte.
    fn trailing(&mut self) {
        self.bit(1);
        while self.bits % 8 != 0 {
            self.bit(0);
        }
    }
}

/// Lee del original y escribe lo mismo en la copia.
struct Copy<'a> {
    r: Reader<'a>,
    w: Writer,
}

impl Copy<'_> {
    fn bits(&mut self, n: u32) -> Result<u32> {
        let v = self.r.bits(n)?;
        self.w.bits(v, n);
        Ok(v)
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.bits(1)? == 1)
    }
    fn ue(&mut self) -> Result<u32> {
        let v = self.r.ue()?;
        self.w.ue(v);
        Ok(v)
    }
    fn se(&mut self) -> Result<i32> {
        let start = self.r.pos;
        let v = self.r.se()?;
        // Copia literal de los bits del se(v).
        let n = self.r.pos - start;
        let mut r = Reader { data: self.r.data, pos: start };
        for _ in 0..n {
            self.w.bit(r.bit()?);
        }
        Ok(v)
    }
}

// ── Emulation prevention (NAL ↔ RBSP) ──────────────────────────────────────

fn to_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn to_nal(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

// ── SPS ─────────────────────────────────────────────────────────────────────

fn copy_scaling_list(c: &mut Copy, size: usize) -> Result<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            let delta = c.se()?;
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Ok(())
}

fn copy_hrd(c: &mut Copy) -> Result<()> {
    let cpb_cnt = c.ue()?;
    c.bits(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..=cpb_cnt.min(31) {
        c.ue()?;
        c.ue()?;
        c.bits(1)?;
    }
    c.bits(20)?; // cuatro campos de 5 bits
    Ok(())
}

fn write_restriction(w: &mut Writer, max_num_ref_frames: u32) {
    w.bit(1); // motion_vectors_over_pic_boundaries_flag
    w.ue(2); // max_bytes_per_pic_denom (valores por defecto de la norma)
    w.ue(1); // max_bits_per_mb_denom
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(max_num_ref_frames.max(1)); // max_dec_frame_buffering
}

/// Reescribe el RBSP de un SPS (sin la cabecera NAL). `None` si ya declara
/// que no reordena.
fn rewrite_sps_rbsp(rbsp: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut c = Copy { r: Reader::new(rbsp), w: Writer::default() };
    let profile = c.bits(8)?;
    c.bits(16)?; // constraint flags + level_idc
    c.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = c.ue()?;
        if chroma == 3 {
            c.bits(1)?;
        }
        c.ue()?;
        c.ue()?;
        c.bits(1)?;
        if c.flag()? {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if c.flag()? {
                    copy_scaling_list(&mut c, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    c.ue()?; // log2_max_frame_num_minus4
    match c.ue()? {
        0 => {
            c.ue()?;
        }
        1 => {
            c.bits(1)?;
            c.se()?;
            c.se()?;
            for _ in 0..c.ue()?.min(255) {
                c.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = c.ue()?;
    c.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    c.ue()?;
    c.ue()?; // tamaño en macrobloques
    if !c.flag()? {
        c.bits(1)?; // mb_adaptive_frame_field_flag
    }
    c.bits(1)?; // direct_8x8_inference_flag
    if c.flag()? {
        for _ in 0..4 {
            c.ue()?;
        }
    }
    // vui_parameters_present_flag: se escribe siempre a 1.
    let had_vui = c.r.bit()? == 1;
    c.w.bit(1);
    if !had_vui {
        c.w.bits(0, 8); // aspect, overscan, video_signal, chroma_loc, timing, nal_hrd, vcl_hrd, pic_struct
        c.w.bit(1);
        write_restriction(&mut c.w, max_num_ref_frames);
        c.w.trailing();
        return Ok(Some(c.w.out));
    }
    if c.flag()? && c.bits(8)? == 255 {
        c.bits(32)?; // sar_width, sar_height
    }
    if c.flag()? {
        c.bits(1)?;
    }
    if c.flag()? {
        c.bits(4)?;
        if c.flag()? {
            c.bits(24)?;
        }
    }
    if c.flag()? {
        c.ue()?;
        c.ue()?;
    }
    if c.flag()? {
        c.bits(32)?;
        c.bits(32)?;
        c.bits(1)?;
    }
    let nal_hrd = c.flag()?;
    if nal_hrd {
        copy_hrd(&mut c)?;
    }
    let vcl_hrd = c.flag()?;
    if vcl_hrd {
        copy_hrd(&mut c)?;
    }
    if nal_hrd || vcl_hrd {
        c.bits(1)?; // low_delay_hrd_flag
    }
    c.bits(1)?; // pic_struct_present_flag
    if c.r.bit()? == 1 {
        let mut r = Reader { data: c.r.data, pos: c.r.pos };
        r.bit()?;
        for _ in 0..4 {
            r.ue()?;
        }
        if r.ue()? == 0 {
            return Ok(None); // ya dice que no reordena
        }
    }
    c.w.bit(1);
    write_restriction(&mut c.w, max_num_ref_frames);
    c.w.trailing();
    Ok(Some(c.w.out))
}

/// Reescribe el SPS de una unidad de acceso Annex-B, si la lleva. Devuelve
/// `None` si no hay SPS o no hace falta tocarlo.
pub fn patch_access_unit(au: &[u8]) -> Option<Vec<u8>> {
    // Posiciones de los start codes (00 00 01, con o sin 00 delante).
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (k, &s) in starts.iter().enumerate() {
        if au.get(s).map(|h| h & 0x1f) != Some(7) {
            continue;
        }
        let mut end = starts.get(k + 1).map_or(au.len(), |&n| n - 3);
        while end > s && au[end - 1] == 0 {
            end -= 1; // el 00 de un start code de 4 bytes (o relleno)
        }
        let header = au[s];
        let rbsp = to_rbsp(&au[s + 1..end]);
        let new = match rewrite_sps_rbsp(&rbsp) {
            Ok(Some(n)) => n,
            Ok(None) => return None,
            Err(e) => {
                tracing::debug!("SPS no reescrito: {e}");
                return None;
            }
        };
        let mut out = Vec::with_capacity(au.len() + 8);
        out.extend_from_slice(&au[..s]);
        out.push(header);
        out.extend_from_slice(&to_nal(&new));
        out.extend_from_slice(&au[end..]);
        return Some(out);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_roundtrip() {
        let mut w = Writer::default();
        for v in [0, 1, 2, 3, 7, 8, 255, 65535] {
            w.ue(v);
        }
        w.trailing();
        let mut r = Reader::new(&w.out);
        for v in [0, 1, 2, 3, 7, 8, 255, 65535] {
            assert_eq!(r.ue().unwrap(), v);
        }
    }

    #[test]
    fn emulation_prevention_roundtrip() {
        let rbsp = [0u8, 0, 1, 0, 0, 0, 0, 3, 5];
        assert_eq!(to_rbsp(&to_nal(&rbsp)), rbsp);
    }

    /// SPS de NVENC (nvh264enc, 1920x1200): VUI con color y tiempos, sin
    /// bitstream_restriction.
    const NVENC_SPS: &str = "42c03395a01e0097b016a020202800001f400015f9042000";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn adds_restriction_to_nvenc_sps() {
        let mut au = vec![0, 0, 0, 1, 0x67];
        au.extend(hex(NVENC_SPS));
        au.extend([0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 1, 0x65, 0xb8, 0x00, 0x04]);
        let patched = patch_access_unit(&au).expect("debe reescribir el SPS de NVENC");
        // Releído, el SPS nuevo ya declara max_num_reorder_frames = 0 (no hace
        // falta tocarlo otra vez), conserva el VUI original (va al principio,
        // sólo cambia el final) y el resto de la unidad sigue igual.
        assert!(patch_access_unit(&patched).is_none());
        assert_eq!(patched[..5 + 18], au[..5 + 18]);
        let tail = |a: &[u8]| a[a.windows(5).position(|w| w == [0, 0, 0, 1, 0x68]).unwrap()..].to_vec();
        assert_eq!(tail(&au), tail(&patched));
    }

    #[test]
    fn leaves_openh264_sps_alone() {
        // OpenH264 ya escribe max_num_reorder_frames = 0.
        use openh264::encoder::{Encoder, EncoderConfig, UsageType};
        let cfg = EncoderConfig::new().usage_type(UsageType::ScreenContentRealTime);
        let mut enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), cfg).unwrap();
        let au = enc.encode(&openh264::formats::YUVBuffer::new(320, 240)).unwrap().to_vec();
        assert!(patch_access_unit(&au).is_none());
    }

    #[test]
    fn garbage_does_not_panic() {
        for n in 0..64 {
            let mut au = vec![0, 0, 1, 0x67];
            au.extend((0..n).map(|i| (i * 37 + n) as u8));
            let _ = patch_access_unit(&au);
        }
    }
}
