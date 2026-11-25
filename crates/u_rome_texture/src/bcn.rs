//! Standard BC1-BC7 and uncompressed texel decoding to RGBA8.
//!
//! Block decoding uses `bcdec_rs` (MIT), a port of iOrange's bcdec. Its BC6H
//! path avoids the three pitfalls listed in GTA6-Asset-Info §5 (unsigned
//! unquantize, 16-bit mask width, signed sign extension).
//!
//! Channel mapping for formats that are not plain colour:
//! * BC4, R8, R16, R32F: grey `(r, r, r, 255)`; A8: grey `(a, a, a, 255)` so masks are visible.
//! * BC5: `(x, y, z, 255)` with z reconstructed as `sqrt(1 - x^2 - y^2)` (tangent-space normal).
//! * RG8 / RG16: `(r, g, 0, 255)`.
//! * BC6H and float formats: linear values clamped to [0, 1] (no tone mapping).
//! * Signed formats map [-1, 1] to [0, 255].

use crate::error::{Error, Result};
use crate::format::{DxgiFormat, PixelKind};
use crate::gfx10::reserve;

fn to_u8(f: f32) -> u8 {
    // `as` saturates and maps NaN to 0.
    (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn snorm_to_u8(f: f32) -> u8 {
    to_u8(f * 0.5 + 0.5)
}

/// IEEE 754 half to f32.
pub(crate) fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = ((h >> 10) & 0x1F) as i32;
    let man = (h & 0x3FF) as f32;
    match exp {
        0 => sign * man * (1.0 / 16_777_216.0), // 2^-24
        31 => {
            if man == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        e => sign * (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    }
}

fn normal_z(x: f32, y: f32) -> u8 {
    let z = (1.0 - x * x - y * y).max(0.0).sqrt();
    snorm_to_u8(z)
}

/// Decode one 4x4 block to 16 RGBA8 texels (row-major).
fn decode_block(kind: PixelKind, block: &[u8], out: &mut [u8; 64]) {
    match kind {
        PixelKind::Bc1 => bcdec_rs::bc1(block, out, 16),
        PixelKind::Bc2 => bcdec_rs::bc2(block, out, 16),
        PixelKind::Bc3 => bcdec_rs::bc3(block, out, 16),
        PixelKind::Bc7 => bcdec_rs::bc7(block, out, 16),
        PixelKind::Bc4 { signed } => {
            let mut r = [0f32; 16];
            bcdec_rs::bc4_float(block, &mut r, 4, signed);
            for (px, &v) in out.as_chunks_mut::<4>().0.iter_mut().zip(r.iter()) {
                let g = if signed { snorm_to_u8(v) } else { to_u8(v) };
                px.copy_from_slice(&[g, g, g, 255]);
            }
        }
        PixelKind::Bc5 { signed } => {
            let mut rg = [0f32; 32];
            bcdec_rs::bc5_float(block, &mut rg, 8, signed);
            for (px, v) in out
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(rg.as_chunks::<2>().0)
            {
                let (x, y) = if signed {
                    (v[0], v[1])
                } else {
                    (v[0] * 2.0 - 1.0, v[1] * 2.0 - 1.0)
                };
                px.copy_from_slice(&[snorm_to_u8(x), snorm_to_u8(y), normal_z(x, y), 255]);
            }
        }
        PixelKind::Bc6h { signed } => {
            let mut rgb = [0f32; 48];
            bcdec_rs::bc6h_float(block, &mut rgb, 12, signed);
            for (px, v) in out
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(rgb.as_chunks::<3>().0)
            {
                px.copy_from_slice(&[to_u8(v[0]), to_u8(v[1]), to_u8(v[2]), 255]);
            }
        }
        _ => {}
    }
}

fn u16_at(t: &[u8], i: usize) -> u16 {
    match t.get(i..i + 2) {
        Some(b) => u16::from_le_bytes([b[0], b[1]]),
        None => 0,
    }
}

fn u32_at(t: &[u8], i: usize) -> u32 {
    match t.get(i..i + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

fn f32_at(t: &[u8], i: usize) -> f32 {
    f32::from_bits(u32_at(t, i))
}

/// Convert one uncompressed texel to RGBA8.
fn decode_texel(kind: PixelKind, t: &[u8]) -> [u8; 4] {
    let b = |i: usize| t.get(i).copied().unwrap_or(0);
    let h = |i: usize| to_u8(half_to_f32(u16_at(t, i)));
    let n16 = |i: usize| (u16_at(t, i) >> 8) as u8;
    match kind {
        PixelKind::Rgba8 => [b(0), b(1), b(2), b(3)],
        PixelKind::Bgra8 => [b(2), b(1), b(0), b(3)],
        PixelKind::Bgrx8 => [b(2), b(1), b(0), 255],
        PixelKind::Rgb10A2 => {
            let v = u32_at(t, 0);
            let c = |s: u32| ((v >> s) & 0x3FF) as f32 / 1023.0;
            [
                to_u8(c(0)),
                to_u8(c(10)),
                to_u8(c(20)),
                to_u8((v >> 30) as f32 / 3.0),
            ]
        }
        PixelKind::Rgba16Unorm => [n16(0), n16(2), n16(4), n16(6)],
        PixelKind::Rgba16Float => [h(0), h(2), h(4), h(6)],
        PixelKind::Rgba32Float => [
            to_u8(f32_at(t, 0)),
            to_u8(f32_at(t, 4)),
            to_u8(f32_at(t, 8)),
            to_u8(f32_at(t, 12)),
        ],
        PixelKind::Rg16Unorm => [n16(0), n16(2), 0, 255],
        PixelKind::Rg16Float => [h(0), h(2), 0, 255],
        PixelKind::Rg8 => [b(0), b(1), 0, 255],
        PixelKind::R32Float => {
            let g = to_u8(f32_at(t, 0));
            [g, g, g, 255]
        }
        PixelKind::R16Unorm => [n16(0), n16(0), n16(0), 255],
        PixelKind::R16Float => [h(0), h(0), h(0), 255],
        PixelKind::R8 | PixelKind::A8 => [b(0), b(0), b(0), 255],
        _ => [0, 0, 0, 255],
    }
}

/// Decode a linear (detiled) surface of `format` to row-major RGBA8.
///
/// `width` and `height` are in pixels. For block-compressed formats `data`
/// holds `ceil(w/4) * ceil(h/4)` blocks row-major; for uncompressed formats
/// it holds `w * h` texels. Despite the name this handles every format whose
/// [`DxgiFormat::layout`] is known.
pub fn decode_bcn(format: DxgiFormat, width: u32, height: u32, data: &[u8]) -> Result<Vec<u8>> {
    let layout = format.layout().ok_or_else(|| Error::Unsupported {
        codec: "none".into(),
        format: format.to_string(),
    })?;
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::InvalidArgument(format!(
            "image size {width}x{height}"
        )));
    }
    let need = layout.linear_size(width, height);
    if (data.len() as u64) < need {
        return Err(Error::Truncated {
            what: "linear texture data",
            needed: need,
            have: data.len() as u64,
        });
    }
    let (w, h) = (width as usize, height as usize);
    let mut out = Vec::new();
    reserve(&mut out, w as u64 * h as u64 * 4, "decoded RGBA image")?;
    out.resize(w * h * 4, 0);
    let bpe = layout.bytes_per_element as usize;
    if layout.is_block_compressed() {
        let (bw, bh) = layout.elements(width, height);
        let mut tmp = [0u8; 64];
        for by in 0..bh as usize {
            for bx in 0..bw as usize {
                let o = (by * bw as usize + bx) * bpe;
                let Some(block) = data.get(o..o + bpe) else {
                    continue;
                };
                decode_block(layout.kind, block, &mut tmp);
                for py in 0..4 {
                    let y = by * 4 + py;
                    if y >= h {
                        break;
                    }
                    let x0 = bx * 4;
                    let n = (w - x0).min(4);
                    let dst = (y * w + x0) * 4;
                    if let (Some(d), Some(s)) = (
                        out.get_mut(dst..dst + n * 4),
                        tmp.get(py * 16..py * 16 + n * 4),
                    ) {
                        d.copy_from_slice(s);
                    }
                }
            }
        }
    } else {
        for (px, t) in out
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(data.chunks_exact(bpe))
        {
            px.copy_from_slice(&decode_texel(layout.kind, t));
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A BC1 block: endpoints c0, c1 (RGB565) and 2-bit indices for the 16 texels.
    pub(crate) fn bc1_block(c0: u16, c1: u16, idx: [u8; 16]) -> [u8; 8] {
        let mut w = 0u32;
        for (i, &v) in idx.iter().enumerate() {
            w |= (v as u32 & 3) << (2 * i);
        }
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&c0.to_le_bytes());
        b[2..4].copy_from_slice(&c1.to_le_bytes());
        b[4..8].copy_from_slice(&w.to_le_bytes());
        b
    }

    fn px(img: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let o = (y * w + x) * 4;
        [img[o], img[o + 1], img[o + 2], img[o + 3]]
    }

    #[test]
    fn bc1_known_colours() {
        // c0 = pure red, c1 = pure blue, 4-colour mode (c0 > c1).
        let mut idx = [0u8; 16];
        idx[1] = 1;
        idx[2] = 2;
        idx[3] = 3;
        let b = bc1_block(0xF800, 0x001F, idx);
        let img = decode_bcn(DxgiFormat::BC1_UNORM, 4, 4, &b).unwrap();
        assert_eq!(px(&img, 4, 0, 0), [255, 0, 0, 255]);
        assert_eq!(px(&img, 4, 1, 0), [0, 0, 255, 255]);
        let c2 = px(&img, 4, 2, 0); // 2/3 red + 1/3 blue
        assert!(
            c2[0] >= 168 && c2[0] <= 171 && c2[2] >= 84 && c2[2] <= 86,
            "{c2:?}"
        );
        let c3 = px(&img, 4, 3, 0);
        assert!(
            c3[0] >= 84 && c3[0] <= 86 && c3[2] >= 168 && c3[2] <= 171,
            "{c3:?}"
        );
        assert_eq!(px(&img, 4, 3, 3), [255, 0, 0, 255]);
        // 3-colour mode: index 3 is transparent black.
        let b = bc1_block(0x001F, 0xF800, [3; 16]);
        let img = decode_bcn(DxgiFormat::BC1_UNORM, 4, 4, &b).unwrap();
        assert_eq!(px(&img, 4, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn bc1_crops_partial_blocks() {
        // 6x5 pixels = 2x2 blocks: green, red / blue, white.
        let blocks = [
            bc1_block(0x07E0, 0, [0; 16]),
            bc1_block(0xF800, 0, [0; 16]),
            bc1_block(0x001F, 0, [0; 16]),
            bc1_block(0xFFFF, 0, [0; 16]),
        ]
        .concat();
        let img = decode_bcn(DxgiFormat::BC1_UNORM_SRGB, 6, 5, &blocks).unwrap();
        assert_eq!(img.len(), 6 * 5 * 4);
        assert_eq!(px(&img, 6, 0, 0), [0, 255, 0, 255]);
        assert_eq!(px(&img, 6, 5, 0), [255, 0, 0, 255]);
        assert_eq!(px(&img, 6, 0, 4), [0, 0, 255, 255]);
        assert_eq!(px(&img, 6, 5, 4), [255, 255, 255, 255]);
        assert!(decode_bcn(DxgiFormat::BC1_UNORM, 6, 5, &blocks[..24]).is_err());
    }

    #[test]
    fn bc3_known_colours_and_alpha() {
        // Alpha: a0 = 255, a1 = 0 (8-value mode); index 0 -> 255, 1 -> 0, 7 -> 0 (1/7 a0 + 6/7 a1 = 36).
        let mut alpha_idx = 0u64;
        alpha_idx |= 1 << 3; // texel 1 -> index 1
        alpha_idx |= 7 << 6; // texel 2 -> index 7
        let mut blk = [0u8; 16];
        blk[0] = 255;
        blk[1] = 0;
        blk[2..8].copy_from_slice(&alpha_idx.to_le_bytes()[..6]);
        blk[8..16].copy_from_slice(&bc1_block(0x07E0, 0x0000, [0; 16]));
        let img = decode_bcn(DxgiFormat::BC3_UNORM, 4, 4, &blk).unwrap();
        assert_eq!(px(&img, 4, 0, 0), [0, 255, 0, 255]);
        assert_eq!(px(&img, 4, 1, 0), [0, 255, 0, 0]);
        let a = px(&img, 4, 2, 0)[3];
        assert!((35..=37).contains(&a), "{a}");
    }

    struct Bits {
        v: u128,
        n: u32,
    }
    impl Bits {
        fn put(&mut self, value: u32, bits: u32) {
            self.v |= (value as u128 & ((1u128 << bits) - 1)) << self.n;
            self.n += bits;
        }
    }

    /// BC7 mode 6 block (one subset, RGBA 7.7.7.7 + p-bit per endpoint), all indices 0.
    fn bc7_mode6(e0: [u32; 4], e1: [u32; 4], p0: u32, p1: u32) -> [u8; 16] {
        let mut b = Bits { v: 0, n: 0 };
        b.put(1 << 6, 7);
        for c in 0..4 {
            b.put(e0[c], 7);
            b.put(e1[c], 7);
        }
        b.put(p0, 1);
        b.put(p1, 1);
        b.put(0, 63);
        assert_eq!(b.n, 128);
        b.v.to_le_bytes()
    }

    #[test]
    fn bc7_known_colours() {
        let blk = bc7_mode6([0x7F, 0, 0x40, 0x7F], [0, 0x7F, 0, 0x7F], 1, 1);
        let img = decode_bcn(DxgiFormat::BC7_UNORM, 4, 4, &blk).unwrap();
        // Endpoint 0 = (0x7F<<1|1, 0|1, 0x40<<1|1, 0x7F<<1|1) = (255, 1, 129, 255).
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(px(&img, 4, x, y), [255, 1, 129, 255]);
            }
        }
        // Mode byte 0 is reserved: decodes as transparent black, no panic.
        let img = decode_bcn(DxgiFormat::BC7_UNORM, 4, 4, &[0u8; 16]).unwrap();
        assert_eq!(px(&img, 4, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn bc4_bc5_bc6h_do_not_panic() {
        let blk = [0xA5u8; 16];
        for f in [80, 81, 83, 84, 95, 96, 74] {
            let img = decode_bcn(DxgiFormat(f), 8, 8, &[blk, blk, blk, blk].concat()).unwrap();
            assert_eq!(img.len(), 256);
        }
        // BC4 solid: r0 = r1 = 200, all indices 0.
        let mut b4 = [0u8; 8];
        b4[0] = 200;
        b4[1] = 200;
        let img = decode_bcn(DxgiFormat::BC4_UNORM, 4, 4, &b4).unwrap();
        assert_eq!(px(&img, 4, 0, 0), [200, 200, 200, 255]);
        // BC5 flat normal (128, 128) -> z near 1.
        let mut b5 = [0u8; 16];
        b5[0] = 128;
        b5[1] = 128;
        b5[8] = 128;
        b5[9] = 128;
        let img = decode_bcn(DxgiFormat::BC5_UNORM, 4, 4, &b5).unwrap();
        let p = px(&img, 4, 0, 0);
        assert!(p[2] >= 254, "{p:?}");
    }

    #[test]
    fn uncompressed_formats() {
        let t = [10u8, 20, 30, 40];
        assert_eq!(
            decode_bcn(DxgiFormat::R8G8B8A8_UNORM, 1, 1, &t).unwrap(),
            [10, 20, 30, 40]
        );
        assert_eq!(
            decode_bcn(DxgiFormat::B8G8R8A8_UNORM, 1, 1, &t).unwrap(),
            [30, 20, 10, 40]
        );
        // R16G16B16A16_FLOAT: 1.0 = 0x3C00, 0.5 = 0x3800, 0 = 0, 2.0 (clamped) = 0x4000.
        let h: Vec<u8> = [0x3C00u16, 0x3800, 0, 0x4000]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(
            decode_bcn(DxgiFormat::R16G16B16A16_FLOAT, 1, 1, &h).unwrap(),
            [255, 128, 0, 255]
        );
        assert_eq!(
            decode_bcn(DxgiFormat::A8_UNORM, 1, 1, &[7]).unwrap(),
            [7, 7, 7, 255]
        );
        assert!(matches!(
            decode_bcn(DxgiFormat(126), 1, 1, &[0; 16]),
            Err(Error::Unsupported { .. })
        ));
        assert!(decode_bcn(DxgiFormat::R8G8B8A8_UNORM, 0, 1, &t).is_err());
    }

    #[test]
    fn half_conversion() {
        assert_eq!(half_to_f32(0x3C00), 1.0);
        assert_eq!(half_to_f32(0xC000), -2.0);
        assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
        assert!(half_to_f32(0x7E00).is_nan());
        assert_eq!(half_to_f32(0x7C00), f32::INFINITY);
    }
}
