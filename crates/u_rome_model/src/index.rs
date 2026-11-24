//! Meshlet index buffer decoding (buffer descriptor flags 0x1380).
//!
//! Layout (GTA6-Asset-Info `formats/07-models.md` §7.3, facts only):
//!
//! | Off | Meaning |
//! |-----|---------|
//! | 0x00 | u16 meshlet count n |
//! | 0x02 | u16 unknown |
//! | 0x04 | u32 triangle count t (u32, not u16: u16 drops geometry) |
//! | 0x08 | u32 remap count r |
//! | 0x0C | n x 16-byte descriptors: u32 ?, u16 remap start, u16 triangle start, u32 ?, 2 x f16 ? |
//! | 0x0C + 16n | t x u32 triangles: local indices in bits 0-6, 7-13, 14-20; meshlet from bit 21 |
//! | ... + 4t | r x u16 remap (local -> vertex) |
//!
//! `vertex = remap[remap_start[meshlet] + local]`. The u16 remap start wraps
//! past 65 535 but ascends along meshlet order, so it is unwrapped here.

use u_rome_core::Reader;

use crate::error::{Context, ModelError, Result};

/// Size of the fixed header before the meshlet descriptors.
pub const INDEX_HEADER_LEN: usize = 0x0C;
/// Size of one meshlet descriptor.
pub const MESHLET_DESC_LEN: usize = 16;
/// Sanity cap on triangles per buffer (the largest known mesh has ~300 K).
pub const MAX_TRIANGLES: usize = 1 << 24;

/// One meshlet descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meshlet {
    /// Remap start, unwrapped past 65 535.
    pub remap_start: u32,
    /// Raw u16 remap start as stored.
    pub remap_start_raw: u16,
    /// Raw u16 triangle start as stored (not used for decoding).
    pub triangle_start_raw: u16,
}

/// A decoded meshlet index buffer with diagnostics.
#[derive(Clone, Debug, Default)]
pub struct IndexBuffer {
    /// Triangle list, three vertex indices per triangle.
    pub indices: Vec<u32>,
    pub meshlets: Vec<Meshlet>,
    /// Triangle count from the header.
    pub triangle_count: u32,
    /// Remap count from the header.
    pub remap_count: u32,
    /// Degenerate triangles dropped (normally a handful of padding
    /// triangles; tens of thousands means a decode problem).
    pub dropped_degenerate: usize,
    /// Triangles dropped for an out-of-range meshlet or remap index.
    pub dropped_invalid: usize,
}

/// Decode a meshlet index buffer to a flat triangle list.
///
/// Structural problems (truncation, counts that do not fit the buffer) are
/// errors. Individual triangles that reference a missing meshlet or remap
/// entry, and degenerate triangles, are dropped; use
/// [`decode_index_buffer_detailed`] to see how many.
pub fn decode_index_buffer(bytes: &[u8]) -> Result<Vec<u32>> {
    decode_index_buffer_detailed(bytes).map(|ib| ib.indices)
}

/// Like [`decode_index_buffer`] but returns meshlets and drop counts.
pub fn decode_index_buffer_detailed(bytes: &[u8]) -> Result<IndexBuffer> {
    let r = Reader::new(bytes);
    let n = r
        .u16_at(0)
        .ctx(|| "index buffer: meshlet count at 0x0".into())? as usize;
    let t = r
        .u32_at(4)
        .ctx(|| "index buffer: triangle count at 0x4".into())?;
    let rc = r
        .u32_at(8)
        .ctx(|| "index buffer: remap count at 0x8".into())?;
    if t as usize > MAX_TRIANGLES {
        return Err(ModelError::TooLarge {
            what: "index buffer triangle count",
            requested: u64::from(t),
            limit: MAX_TRIANGLES as u64,
        });
    }

    let desc_end = n
        .checked_mul(MESHLET_DESC_LEN)
        .and_then(|x| x.checked_add(INDEX_HEADER_LEN));
    let tri_end = desc_end.and_then(|d| (t as usize).checked_mul(4).and_then(|x| x.checked_add(d)));
    let remap_end =
        tri_end.and_then(|e| (rc as usize).checked_mul(2).and_then(|x| x.checked_add(e)));
    let (Some(desc_end), Some(tri_end), Some(remap_end)) = (desc_end, tri_end, remap_end) else {
        return Err(ModelError::IndexBuffer("counts overflow".into()));
    };
    if remap_end > bytes.len() {
        return Err(ModelError::IndexBuffer(format!(
            "{n} meshlets, {t} triangles and {rc} remap entries need 0x{remap_end:X} bytes \
             (descriptors 0x{INDEX_HEADER_LEN:X}..0x{desc_end:X}, triangles ..0x{tri_end:X}), \
             buffer is 0x{:X}",
            bytes.len()
        )));
    }
    if n == 0 && t > 0 {
        return Err(ModelError::IndexBuffer(format!(
            "{t} triangles but no meshlets"
        )));
    }

    // Meshlet descriptors; unwrap the ascending u16 remap start.
    let mut meshlets = Vec::with_capacity(n);
    let mut high: u32 = 0;
    let mut prev: u16 = 0;
    for i in 0..n {
        let off = INDEX_HEADER_LEN + i * MESHLET_DESC_LEN;
        let raw = r
            .u16_at(off + 4)
            .ctx(|| format!("index buffer: meshlet {i} remap start at 0x{:X}", off + 4))?;
        let tri = r.u16_at(off + 6).ctx(|| {
            format!(
                "index buffer: meshlet {i} triangle start at 0x{:X}",
                off + 6
            )
        })?;
        if i > 0 && raw < prev {
            high = high.saturating_add(0x1_0000);
        }
        prev = raw;
        meshlets.push(Meshlet {
            remap_start: high.saturating_add(u32::from(raw)),
            remap_start_raw: raw,
            triangle_start_raw: tri,
        });
    }

    let remap_bytes = bytes
        .get(tri_end..remap_end)
        .ok_or_else(|| ModelError::IndexBuffer("remap table out of bounds".into()))?;
    let remap: Vec<u16> = remap_bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let tri_bytes = bytes
        .get(desc_end..tri_end)
        .ok_or_else(|| ModelError::IndexBuffer("triangle table out of bounds".into()))?;

    let mut out = IndexBuffer {
        indices: Vec::with_capacity(t as usize * 3),
        meshlets,
        triangle_count: t,
        remap_count: rc,
        ..Default::default()
    };
    let mut first_bad: Option<String> = None;
    for (ti, c) in tri_bytes.as_chunks::<4>().0.iter().enumerate() {
        let w = u32::from_le_bytes(*c);
        let m = (w >> 21) as usize;
        let locals = [w & 0x7F, (w >> 7) & 0x7F, (w >> 14) & 0x7F];
        let Some(ml) = out.meshlets.get(m) else {
            out.dropped_invalid += 1;
            first_bad.get_or_insert_with(|| {
                format!("triangle {ti} (0x{w:08X}) names meshlet {m} of {n}")
            });
            continue;
        };
        let mut tri = [0u32; 3];
        let mut ok = true;
        for (dst, l) in tri.iter_mut().zip(locals) {
            let idx = ml.remap_start as usize + l as usize;
            match remap.get(idx) {
                Some(&v) => *dst = u32::from(v),
                None => {
                    ok = false;
                    first_bad.get_or_insert_with(|| {
                        format!("triangle {ti} (0x{w:08X}) meshlet {m}: remap index {idx} >= {rc}")
                    });
                }
            }
        }
        if !ok {
            out.dropped_invalid += 1;
            continue;
        }
        if tri[0] == tri[1] || tri[1] == tri[2] || tri[0] == tri[2] {
            out.dropped_degenerate += 1;
            continue;
        }
        out.indices.extend_from_slice(&tri);
    }
    if out.dropped_invalid > 0 {
        log::warn!(
            "index buffer: dropped {} of {t} triangles with invalid references; first: {}",
            out.dropped_invalid,
            first_bad.unwrap_or_default()
        );
    }
    if out.dropped_degenerate > 30 {
        // GTA6-Asset-Info §7.3: 1-12 degenerate triangles is normal, more
        // than ~30 on a large mesh points at a decode problem.
        log::warn!(
            "index buffer: dropped {} degenerate triangles of {t}",
            out.dropped_degenerate
        );
    } else if out.dropped_degenerate > 0 {
        log::debug!(
            "index buffer: dropped {} degenerate padding triangles",
            out.dropped_degenerate
        );
    }
    Ok(out)
}

/// Encode a triangle list into a meshlet index buffer. Used to build
/// synthetic test data; not a production-quality meshlet builder (it packs
/// triangles greedily into meshlets of at most 128 local vertices).
pub fn encode_index_buffer(indices: &[u32]) -> Result<Vec<u8>> {
    if !indices.len().is_multiple_of(3) {
        return Err(ModelError::IndexBuffer(
            "index count is not a multiple of 3".into(),
        ));
    }
    struct Building {
        remap: Vec<u16>,
        tris: Vec<[u8; 3]>,
    }
    let mut meshlets: Vec<Building> = Vec::new();
    for tri in indices.as_chunks::<3>().0 {
        let mut verts = [0u16; 3];
        for (d, &v) in verts.iter_mut().zip(tri) {
            *d = u16::try_from(v)
                .map_err(|_| ModelError::IndexBuffer(format!("vertex {v} above u16")))?;
        }
        let fits = |m: &Building| {
            let new = verts.iter().filter(|v| !m.remap.contains(v)).count();
            m.remap.len() + new <= 128
        };
        if meshlets.last().is_none_or(|m| !fits(m)) {
            meshlets.push(Building {
                remap: Vec::new(),
                tris: Vec::new(),
            });
        }
        let Some(m) = meshlets.last_mut() else {
            continue;
        };
        let mut local = [0u8; 3];
        for (l, v) in local.iter_mut().zip(verts) {
            let pos = match m.remap.iter().position(|&x| x == v) {
                Some(p) => p,
                None => {
                    m.remap.push(v);
                    m.remap.len() - 1
                }
            };
            *l = pos as u8;
        }
        m.tris.push(local);
    }
    let n = meshlets.len();
    if n >= 1 << 11 {
        return Err(ModelError::IndexBuffer(format!(
            "{n} meshlets do not fit the 11-bit meshlet field"
        )));
    }
    let t: usize = meshlets.iter().map(|m| m.tris.len()).sum();
    let rc: usize = meshlets.iter().map(|m| m.remap.len()).sum();
    let mut out = Vec::with_capacity(INDEX_HEADER_LEN + n * 16 + t * 4 + rc * 2);
    out.extend_from_slice(&(n as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(t as u32).to_le_bytes());
    out.extend_from_slice(&(rc as u32).to_le_bytes());
    let mut remap_start = 0usize;
    let mut tri_start = 0usize;
    for m in &meshlets {
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(remap_start as u16).to_le_bytes());
        out.extend_from_slice(&(tri_start as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 8]);
        remap_start += m.remap.len();
        tri_start += m.tris.len();
    }
    for (mi, m) in meshlets.iter().enumerate() {
        for l in &m.tris {
            let w =
                u32::from(l[0]) | u32::from(l[1]) << 7 | u32::from(l[2]) << 14 | (mi as u32) << 21;
            out.extend_from_slice(&w.to_le_bytes());
        }
    }
    for m in &meshlets {
        for v in &m.remap {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::lcg;

    #[test]
    fn round_trips_small_list() {
        let tris = vec![0, 1, 2, 2, 1, 3, 4, 5, 6];
        let buf = encode_index_buffer(&tris).unwrap();
        let ib = decode_index_buffer_detailed(&buf).unwrap();
        assert_eq!(ib.indices, tris);
        assert_eq!(ib.triangle_count, 3);
        assert_eq!(ib.meshlets.len(), 1);
        assert_eq!(ib.dropped_degenerate, 0);
    }

    #[test]
    fn hand_built_buffer() {
        // Two meshlets, the second starting at remap 3, one triangle each.
        let mut b = Vec::new();
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&6u32.to_le_bytes());
        for (rs, ts) in [(0u16, 0u16), (3, 1)] {
            b.extend_from_slice(&0xDEADu32.to_le_bytes());
            b.extend_from_slice(&rs.to_le_bytes());
            b.extend_from_slice(&ts.to_le_bytes());
            b.extend_from_slice(&[0xAA; 8]);
        }
        let tri = |a: u32, c: u32, d: u32, m: u32| a | c << 7 | d << 14 | m << 21;
        b.extend_from_slice(&tri(0, 1, 2, 0).to_le_bytes());
        b.extend_from_slice(&tri(2, 0, 1, 1).to_le_bytes());
        for v in [10u16, 11, 12, 500, 600, 700] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(
            decode_index_buffer(&b).unwrap(),
            vec![10, 11, 12, 700, 500, 600]
        );
    }

    #[test]
    fn large_counts_above_u16() {
        // 70 000 triangles and > 65 536 remap entries: the u16 reading of
        // the counts and of the remap start would lose geometry.
        let mut seed = 7;
        let nverts = 65_000u32;
        let mut tris = Vec::new();
        for i in 0..70_000u32 {
            // Mostly fresh vertices so the remap table grows past 65 536.
            let a = (i * 3) % nverts;
            let b = (a + 1 + lcg(&mut seed) % 5) % nverts;
            let c = (b + 1 + lcg(&mut seed) % 5) % nverts;
            if a == b || b == c || a == c {
                continue;
            }
            tris.extend_from_slice(&[a, b, c]);
        }
        let buf = encode_index_buffer(&tris).unwrap();
        let ib = decode_index_buffer_detailed(&buf).unwrap();
        assert!(ib.triangle_count > 65_535, "t = {}", ib.triangle_count);
        assert!(ib.remap_count > 65_535, "r = {}", ib.remap_count);
        assert!(ib.meshlets.iter().any(|m| m.remap_start > 65_535));
        assert_eq!(ib.dropped_invalid, 0);
        assert_eq!(ib.indices, tris);
    }

    #[test]
    fn drops_degenerate_and_invalid() {
        let mut buf = encode_index_buffer(&[0, 1, 2, 3, 4, 5]).unwrap();
        // Make the second triangle degenerate (locals 3,3,5 -> two equal).
        let t1 = INDEX_HEADER_LEN + 16 + 4;
        let w = u32::from_le_bytes(buf[t1..t1 + 4].try_into().unwrap());
        let w = (w & !0x3F80) | (3 << 7);
        buf[t1..t1 + 4].copy_from_slice(&w.to_le_bytes());
        let ib = decode_index_buffer_detailed(&buf).unwrap();
        assert_eq!(ib.indices, vec![0, 1, 2]);
        assert_eq!(ib.dropped_degenerate, 1);

        // Point the first triangle at a meshlet that does not exist.
        let t0 = INDEX_HEADER_LEN + 16;
        let w = u32::from_le_bytes(buf[t0..t0 + 4].try_into().unwrap()) | (5 << 21);
        buf[t0..t0 + 4].copy_from_slice(&w.to_le_bytes());
        let ib = decode_index_buffer_detailed(&buf).unwrap();
        assert_eq!(ib.dropped_invalid, 1);
        assert!(ib.indices.is_empty());

        // Local index past the remap table.
        let mut buf = encode_index_buffer(&[0, 1, 2]).unwrap();
        let t0 = INDEX_HEADER_LEN + 16;
        let w = u32::from_le_bytes(buf[t0..t0 + 4].try_into().unwrap()) | 0x7F;
        buf[t0..t0 + 4].copy_from_slice(&w.to_le_bytes());
        let ib = decode_index_buffer_detailed(&buf).unwrap();
        assert_eq!(ib.dropped_invalid, 1);
    }

    #[test]
    fn malformed_inputs_error() {
        assert!(decode_index_buffer(&[]).is_err());
        assert!(decode_index_buffer(&[1, 0, 0]).is_err());
        // Counts larger than the buffer.
        let mut b = vec![0u8; 0x0C];
        b[0] = 1;
        b[4..8].copy_from_slice(&1000u32.to_le_bytes());
        let e = decode_index_buffer(&b).unwrap_err().to_string();
        assert!(e.contains("1000 triangles"), "{e}");
        // Triangle count above the cap.
        b[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode_index_buffer(&b),
            Err(ModelError::TooLarge { .. })
        ));
        // Triangles without meshlets.
        let mut b = vec![0u8; 0x0C + 4];
        b[4] = 1;
        assert!(decode_index_buffer(&b).is_err());
        // Truncating a valid buffer anywhere must error, never panic.
        let good = encode_index_buffer(&[0, 1, 2, 2, 1, 3]).unwrap();
        for cut in 0..good.len() {
            assert!(decode_index_buffer(&good[..cut]).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut seed = 99;
        for _ in 0..2000 {
            let len = (lcg(&mut seed) % 256) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| lcg(&mut seed) as u8).collect();
            if b.len() >= 12 {
                // Keep counts small so some inputs parse.
                b[0] %= 4;
                b[1] = 0;
                let t = u32::from(b[4]) % 8;
                let r = u32::from(b[8]) % 16;
                b[4..8].copy_from_slice(&t.to_le_bytes());
                b[8..12].copy_from_slice(&r.to_le_bytes());
            }
            let _ = decode_index_buffer_detailed(&b);
        }
    }
}
