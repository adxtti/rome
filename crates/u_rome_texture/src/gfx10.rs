// Portions adapted from bcpack-rs by Ahmet Dedeler (https://github.com/Ahmet-Dedeler, https://ahmetdedeler.com)
// Source: https://github.com/Ahmet-Dedeler/bcpack-rs
// SPDX-License-Identifier: MIT OR Apache-2.0
// If you copy, vendor or port this file, keep this header and credit bcpack-rs in your README.
//! AMD Gfx10 (RDNA2) non-XOR swizzle patterns, tiling, detiling and mip chain layout.
//!
//! Adapted from bcpack-rs `src/ttpf.rs` (Ahmet Dedeler,
//! <https://github.com/Ahmet-Dedeler/bcpack-rs>, MIT OR Apache-2.0), which ports
//! paryzius' ttpf-viewer tiling and mip-tail code for 8-byte (BC1) elements.
//! This version generalises it to 1, 2, 4, 8 and 16-byte elements, addresses
//! elements forward instead of through a lookup of the whole surface, and
//! returns errors instead of panicking.
//!
//! Patterns (element address bits, lowest first) are from GTA6-Asset-Info
//! `formats/05-ttpf.md` (facts only) and agree with bcpack-rs for 8-byte
//! elements and with ttpf-studio FORMATS.md for 8 and 16-byte elements.
//!
//! Disagreement: ttpf-studio lists the 4-byte "standard" micro tile as
//! `x x y y y x`, GTA6-Asset-Info lists `SW_64KB_D` 4-byte as
//! `x0 x1 y0 y1 x2 y2`. We use the latter (it is stated as checked on 2,730
//! textures) and the store marks 4-byte tile-mode-0 textures provisional.
//!
//! Tiles (64 KiB or 4 KiB blocks) are laid out row-major; there is no pipe or
//! bank XOR.

use crate::error::{Error, Result};

/// A Gfx10 swizzle mode used by the stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Swizzle {
    /// `SW_64KB_D`: 64 KiB blocks, "display" micro tiles. Tile mode 0.
    Sw64KbD,
    /// `SW_4KB_S`: 4 KiB blocks, "standard" micro tiles. Tile modes 1-3 and cube maps.
    Sw4KbS,
}

impl Swizzle {
    /// Bytes in one block (tile).
    pub fn block_bytes(self) -> u32 {
        match self {
            Swizzle::Sw64KbD => 65536,
            Swizzle::Sw4KbS => 4096,
        }
    }

    /// Maximum number of mips in the packed mip tail.
    fn max_mips_in_tail(self) -> u32 {
        match self {
            Swizzle::Sw64KbD => 12,
            Swizzle::Sw4KbS => 8,
        }
    }
}

#[derive(Clone, Copy)]
enum Bit {
    X(u8),
    Y(u8),
}
use Bit::{X, Y};

const D64_1: &[Bit] = &[
    X(0),
    X(1),
    X(2),
    Y(1),
    Y(0),
    Y(2),
    X(3),
    Y(3),
    Y(4),
    X(4),
    Y(5),
    X(5),
    Y(6),
    X(6),
    Y(7),
    X(7),
];
const D64_2: &[Bit] = &[
    X(0),
    X(1),
    X(2),
    Y(0),
    Y(1),
    Y(2),
    X(3),
    Y(3),
    X(4),
    Y(4),
    X(5),
    Y(5),
    X(6),
    Y(6),
    X(7),
];
const D64_4: &[Bit] = &[
    X(0),
    X(1),
    Y(0),
    Y(1),
    X(2),
    Y(2),
    Y(3),
    X(3),
    Y(4),
    X(4),
    Y(5),
    X(5),
    Y(6),
    X(6),
];
const D64_8: &[Bit] = &[
    X(0),
    Y(0),
    X(1),
    X(2),
    Y(1),
    Y(2),
    X(3),
    Y(3),
    X(4),
    Y(4),
    X(5),
    Y(5),
    X(6),
];
const D64_16: &[Bit] = &[
    X(0),
    Y(0),
    X(1),
    Y(1),
    Y(2),
    X(2),
    Y(3),
    X(3),
    Y(4),
    X(4),
    Y(5),
    X(5),
];
const S4_1: &[Bit] = &[
    X(0),
    X(1),
    X(2),
    X(3),
    Y(0),
    Y(1),
    Y(2),
    Y(3),
    Y(4),
    X(4),
    Y(5),
    X(5),
];
const S4_2: &[Bit] = &[
    X(0),
    X(1),
    X(2),
    Y(0),
    Y(1),
    Y(2),
    X(3),
    Y(3),
    X(4),
    Y(4),
    X(5),
];
const S4_4: &[Bit] = &[X(0), X(1), Y(0), Y(1), Y(2), X(2), Y(3), X(3), Y(4), X(4)];
const S4_8: &[Bit] = &[X(0), Y(0), Y(1), X(1), X(2), Y(2), X(3), Y(3), X(4)];
const S4_16: &[Bit] = &[Y(0), Y(1), X(0), X(1), Y(2), X(2), Y(3), X(3)];

fn pattern(swizzle: Swizzle, bpe: u32) -> Result<&'static [Bit]> {
    Ok(match (swizzle, bpe) {
        (Swizzle::Sw64KbD, 1) => D64_1,
        (Swizzle::Sw64KbD, 2) => D64_2,
        (Swizzle::Sw64KbD, 4) => D64_4,
        (Swizzle::Sw64KbD, 8) => D64_8,
        (Swizzle::Sw64KbD, 16) => D64_16,
        (Swizzle::Sw4KbS, 1) => S4_1,
        (Swizzle::Sw4KbS, 2) => S4_2,
        (Swizzle::Sw4KbS, 4) => S4_4,
        (Swizzle::Sw4KbS, 8) => S4_8,
        (Swizzle::Sw4KbS, 16) => S4_16,
        _ => {
            return Err(Error::InvalidArgument(format!(
                "bytes per element {bpe} (expected 1, 2, 4, 8 or 16)"
            )));
        }
    })
}

/// Precomputed address tables for one (swizzle, element size) pair.
#[derive(Clone, Debug)]
pub struct TileGeometry {
    pub swizzle: Swizzle,
    pub bytes_per_element: u32,
    /// Block size in elements.
    pub block_width: u32,
    pub block_height: u32,
    /// 256-byte unit size in elements (mip-tail placement granularity).
    pub unit_width: u32,
    pub unit_height: u32,
    xm: Vec<u32>,
    ym: Vec<u32>,
}

impl TileGeometry {
    pub fn new(swizzle: Swizzle, bytes_per_element: u32) -> Result<TileGeometry> {
        let pat = pattern(swizzle, bytes_per_element)?;
        let xs = pat.iter().filter(|b| matches!(b, X(_))).count() as u32;
        let ys = pat.len() as u32 - xs;
        let (bw, bh) = (1u32 << xs, 1u32 << ys);
        let mut xm = vec![0u32; bw as usize];
        let mut ym = vec![0u32; bh as usize];
        for (pos, b) in pat.iter().enumerate() {
            match *b {
                X(i) => {
                    for (x, m) in xm.iter_mut().enumerate() {
                        *m |= (((x as u32) >> i) & 1) << pos;
                    }
                }
                Y(i) => {
                    for (y, m) in ym.iter_mut().enumerate() {
                        *m |= (((y as u32) >> i) & 1) << pos;
                    }
                }
            }
        }
        // The first (8 - log2 bpe) bits address one 256-byte unit.
        let unit_bits = 8 - bytes_per_element.trailing_zeros() as usize;
        let ux = pat
            .iter()
            .take(unit_bits)
            .filter(|b| matches!(b, X(_)))
            .count() as u32;
        Ok(TileGeometry {
            swizzle,
            bytes_per_element,
            block_width: bw,
            block_height: bh,
            unit_width: 1 << ux,
            unit_height: 1 << (unit_bits as u32 - ux),
            xm,
            ym,
        })
    }

    pub fn block_bytes(&self) -> u64 {
        self.swizzle.block_bytes() as u64
    }

    fn elements_per_block(&self) -> u64 {
        self.block_width as u64 * self.block_height as u64
    }

    /// Byte offset of element (x, y) in a surface `pitch_blocks` blocks wide.
    pub fn element_offset(&self, x: u32, y: u32, pitch_blocks: u32) -> u64 {
        let (bw, bh) = (self.block_width, self.block_height);
        let tile = (y / bh) as u64 * pitch_blocks as u64 + (x / bw) as u64;
        // The tables have exactly bw / bh entries and the indices are reduced mod bw / bh.
        let inner = self.xm.get((x % bw) as usize).copied().unwrap_or(0)
            | self.ym.get((y % bh) as usize).copied().unwrap_or(0);
        (tile * self.elements_per_block() + inner as u64) * self.bytes_per_element as u64
    }

    /// Blocks needed horizontally and vertically for `w x h` elements.
    pub fn blocks_for(&self, w: u32, h: u32) -> (u32, u32) {
        (
            w.div_ceil(self.block_width).max(1),
            h.div_ceil(self.block_height).max(1),
        )
    }

    /// Size in bytes of a surface of `w x h` elements padded to whole blocks.
    pub fn padded_size(&self, w: u32, h: u32) -> u64 {
        let (nx, ny) = self.blocks_for(w, h);
        nx as u64 * ny as u64 * self.block_bytes()
    }
}

/// Copy the `w x h` element region at origin (`ox`, `oy`) out of tiled `src`
/// (a surface `pitch_blocks` blocks wide) into row-major `out`.
///
/// Returns the byte offset of the first element that lies past `src` as an error.
pub(crate) fn detile_region(
    src: &[u8],
    g: &TileGeometry,
    pitch_blocks: u32,
    region: (u32, u32, u32, u32),
    out: &mut Vec<u8>,
) -> std::result::Result<(), u64> {
    let (ox, oy, w, h) = region;
    let bpe = g.bytes_per_element as usize;
    for y in 0..h {
        for x in 0..w {
            let off = g.element_offset(ox + x, oy + y, pitch_blocks);
            let elem = usize::try_from(off)
                .ok()
                .and_then(|o| src.get(o..o.checked_add(bpe)?))
                .ok_or(off)?;
            out.extend_from_slice(elem);
        }
    }
    Ok(())
}

fn check_dims(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::InvalidArgument(format!(
            "surface size {width}x{height} elements (must be 1..=65536)"
        )));
    }
    Ok(())
}

/// Detile one Gfx10 surface of `width x height` elements (4x4 blocks for BCn,
/// texels otherwise) stored from offset 0 of `data` in whole blocks, row-major
/// blocks. Returns `width * height * bytes_per_element` row-major bytes.
pub fn detile(
    data: &[u8],
    swizzle: Swizzle,
    bytes_per_element: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    check_dims(width, height)?;
    let g = TileGeometry::new(swizzle, bytes_per_element)?;
    let (pitch, _) = g.blocks_for(width, height);
    let mut out = Vec::new();
    let need = width as u64 * height as u64 * bytes_per_element as u64;
    reserve(&mut out, need, "detiled surface")?;
    detile_region(data, &g, pitch, (0, 0, width, height), &mut out).map_err(|off| {
        Error::Truncated {
            what: "tiled surface",
            needed: off + bytes_per_element as u64,
            have: data.len() as u64,
        }
    })?;
    Ok(out)
}

/// Inverse of [`detile`]: tile row-major elements into whole Gfx10 blocks
/// (padding is zero). Useful for tests and for writing stores.
pub fn tile(
    linear: &[u8],
    swizzle: Swizzle,
    bytes_per_element: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    check_dims(width, height)?;
    let g = TileGeometry::new(swizzle, bytes_per_element)?;
    let bpe = bytes_per_element as usize;
    let need = width as usize * height as usize * bpe;
    if linear.len() < need {
        return Err(Error::Truncated {
            what: "linear surface",
            needed: need as u64,
            have: linear.len() as u64,
        });
    }
    let (pitch, _) = g.blocks_for(width, height);
    let size = g.padded_size(width, height);
    let mut out = Vec::new();
    reserve(&mut out, size, "tiled surface")?;
    out.resize(size as usize, 0);
    for y in 0..height {
        for x in 0..width {
            let src = (y as usize * width as usize + x as usize) * bpe;
            let dst = g.element_offset(x, y, pitch) as usize;
            if let (Some(d), Some(s)) = (out.get_mut(dst..dst + bpe), linear.get(src..src + bpe)) {
                d.copy_from_slice(s);
            }
        }
    }
    Ok(out)
}

/// Size in bytes of a `width x height` element surface padded to whole blocks.
pub fn tiled_size(
    swizzle: Swizzle,
    bytes_per_element: u32,
    width: u32,
    height: u32,
) -> Result<u64> {
    check_dims(width, height)?;
    Ok(TileGeometry::new(swizzle, bytes_per_element)?.padded_size(width, height))
}

pub(crate) fn reserve(v: &mut Vec<u8>, bytes: u64, what: &'static str) -> Result<()> {
    const LIMIT: u64 = 1 << 30;
    if bytes > LIMIT {
        return Err(Error::TooLarge {
            what,
            requested: bytes,
            limit: LIMIT,
        });
    }
    v.try_reserve_exact(bytes as usize)
        .map_err(|_| Error::TooLarge {
            what,
            requested: bytes,
            limit: LIMIT,
        })
}

/// Where one mip level sits inside a texture's (single-slice) tiled bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MipPlacement {
    /// Level size in pixels.
    pub width: u32,
    pub height: u32,
    /// Level size in elements.
    pub elem_width: u32,
    pub elem_height: u32,
    /// Byte offset of the level's blocks inside one slice's chain.
    pub offset: u64,
    /// Bytes of whole blocks holding the level (the full block for tail mips).
    pub len: u64,
    /// Surface width in blocks.
    pub pitch_blocks: u32,
    /// Element origin of the level inside its blocks (non-zero only in the tail).
    pub origin_x: u32,
    pub origin_y: u32,
    pub in_tail: bool,
}

/// The Gfx10 mip chain of one slice: levels largest first, stored smallest first.
#[derive(Clone, Debug)]
pub struct MipChain {
    pub geometry: TileGeometry,
    /// `levels[0]` is mip 0.
    pub levels: Vec<MipPlacement>,
    /// Bytes of one slice's chain (tail block plus padded levels).
    pub slice_size: u64,
    /// Index of the largest mip in the packed tail, if there is a tail.
    pub first_tail_mip: Option<u32>,
}

impl MipChain {
    /// Storage units smallest first: the tail block (if any), then each
    /// non-tail mip. Returns their byte sizes.
    pub fn unit_sizes(&self) -> Vec<u64> {
        let tail = self.first_tail_mip.unwrap_or(self.levels.len() as u32) as usize;
        let mut units = Vec::with_capacity(tail + 1);
        if self.first_tail_mip.is_some() {
            units.push(self.geometry.block_bytes());
        }
        for i in (0..tail).rev() {
            if let Some(l) = self.levels.get(i) {
                units.push(l.len);
            }
        }
        units
    }
}

/// Number of levels in a full chain down to 1x1.
pub fn full_mip_count(width: u32, height: u32) -> u32 {
    32 - width.max(height).max(1).leading_zeros()
}

/// Lay out a Gfx10 mip chain.
///
/// Rules (bcpack-rs `mip_chain_bc1`, generalised; GTA6-Asset-Info §5 facts):
/// mips are stored smallest first with mip 0 last; a mip is in the packed tail
/// when its element size is at most (block width / 2, block height) and it is
/// among the last `max_mips_in_tail` mips (8 for 4 KiB, 12 for 64 KiB blocks);
/// a single-mip surface has no tail; tail mip k sits in 256-byte unit
/// `m = max - 1 - k` at byte offset `m > 6 ? 16 << m : m << 8`, whose odd /
/// even Morton bits give the unit's x / y.
pub fn mip_chain(
    swizzle: Swizzle,
    bytes_per_element: u32,
    block_dim: u32,
    width: u32,
    height: u32,
    mips: u32,
) -> Result<MipChain> {
    if width == 0 || height == 0 || mips == 0 || mips > full_mip_count(width, height) {
        return Err(Error::InvalidArgument(format!(
            "mip chain {width}x{height} with {mips} mips"
        )));
    }
    if block_dim != 1 && block_dim != 4 {
        return Err(Error::InvalidArgument(format!(
            "block dimension {block_dim}"
        )));
    }
    let g = TileGeometry::new(swizzle, bytes_per_element)?;
    let (bw, bh) = (g.block_width, g.block_height);
    let px = |i: u32| ((width >> i).max(1), (height >> i).max(1));
    let el = |i: u32| {
        let (w, h) = px(i);
        (w.div_ceil(block_dim), h.div_ceil(block_dim))
    };
    let max_tail = swizzle.max_mips_in_tail();
    let mut tail = mips;
    if mips > 1 {
        for i in 0..mips {
            let (ew, eh) = el(i);
            if ew <= bw / 2 && eh <= bh && mips - i <= max_tail {
                tail = i;
                break;
            }
        }
    }
    let mut levels: Vec<Option<MipPlacement>> = vec![None; mips as usize];
    let mut pos: u64 = 0;
    if tail < mips {
        pos = g.block_bytes();
        for i in tail..mips {
            let m = max_tail - 1 - (i - tail);
            let off: u32 = if m > 6 { 16 << m } else { m << 8 };
            let morton = off >> 8;
            let (mut mx, mut my) = (0u32, 0u32);
            for bit in 0..8 {
                my |= ((morton >> (2 * bit)) & 1) << bit;
                mx |= ((morton >> (2 * bit + 1)) & 1) << bit;
            }
            let (ox, oy) = (mx * g.unit_width, my * g.unit_height);
            let (ew, eh) = el(i);
            if ox + ew > bw || oy + eh > bh {
                return Err(Error::UnsupportedLayout(format!(
                    "mip {i} ({ew}x{eh} elements) does not fit its tail slot at ({ox}, {oy})"
                )));
            }
            let (w, h) = px(i);
            levels[i as usize] = Some(MipPlacement {
                width: w,
                height: h,
                elem_width: ew,
                elem_height: eh,
                offset: 0,
                len: g.block_bytes(),
                pitch_blocks: 1,
                origin_x: ox,
                origin_y: oy,
                in_tail: true,
            });
        }
    }
    for i in (0..tail).rev() {
        let (ew, eh) = el(i);
        let (nx, ny) = g.blocks_for(ew, eh);
        let len = nx as u64 * ny as u64 * g.block_bytes();
        let (w, h) = px(i);
        levels[i as usize] = Some(MipPlacement {
            width: w,
            height: h,
            elem_width: ew,
            elem_height: eh,
            offset: pos,
            len,
            pitch_blocks: nx,
            origin_x: 0,
            origin_y: 0,
            in_tail: false,
        });
        pos += len;
    }
    let levels: Vec<MipPlacement> = levels.into_iter().flatten().collect();
    if levels.len() != mips as usize {
        return Err(Error::UnsupportedLayout("incomplete mip chain".into()));
    }
    Ok(MipChain {
        geometry: g,
        levels,
        slice_size: pos,
        first_tail_mip: (tail < mips).then_some(tail),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Swizzle; 2] = [Swizzle::Sw64KbD, Swizzle::Sw4KbS];
    const SIZES: [u32; 5] = [1, 2, 4, 8, 16];

    fn pattern_bytes(n: usize, bpe: u32) -> Vec<u8> {
        (0..n * bpe as usize)
            .map(|i| (i as u32).wrapping_mul(2_654_435_761).rotate_right(13) as u8)
            .collect()
    }

    #[test]
    fn geometry_matches_block_size() {
        for s in ALL {
            for bpe in SIZES {
                let g = TileGeometry::new(s, bpe).unwrap();
                assert_eq!(
                    g.block_width as u64 * g.block_height as u64 * bpe as u64,
                    s.block_bytes() as u64,
                    "{s:?} {bpe}"
                );
                assert_eq!(g.unit_width * g.unit_height * bpe, 256);
            }
        }
        // Unit sizes stated in GTA6-Asset-Info §5.
        let u = |bpe| {
            let g = TileGeometry::new(Swizzle::Sw64KbD, bpe).unwrap();
            (g.unit_width, g.unit_height)
        };
        assert_eq!(u(8), (8, 4));
        assert_eq!(u(16), (4, 4));
        assert_eq!(u(4), (8, 8));
        assert_eq!(u(2), (16, 8));
        assert_eq!(u(1), (16, 16));
        assert!(TileGeometry::new(Swizzle::Sw4KbS, 3).is_err());
    }

    #[test]
    fn full_block_is_a_permutation() {
        for s in ALL {
            for bpe in SIZES {
                let g = TileGeometry::new(s, bpe).unwrap();
                let n = g.elements_per_block() as usize;
                let mut seen = vec![false; n];
                for y in 0..g.block_height {
                    for x in 0..g.block_width {
                        let e = (g.element_offset(x, y, 1) / bpe as u64) as usize;
                        assert!(!seen[e], "{s:?} {bpe} duplicate at ({x},{y})");
                        seen[e] = true;
                    }
                }
            }
        }
    }

    #[test]
    fn tile_detile_round_trip() {
        let dims = [
            (1, 1),
            (4, 4),
            (37, 19),
            (64, 64),
            (130, 260),
            (300, 70),
            (257, 1),
        ];
        for s in ALL {
            for bpe in SIZES {
                for &(w, h) in &dims {
                    let lin = pattern_bytes((w * h) as usize, bpe);
                    let tiled = tile(&lin, s, bpe, w, h).unwrap();
                    assert_eq!(tiled.len() as u64, tiled_size(s, bpe, w, h).unwrap());
                    assert_eq!(tiled.len() as u64 % s.block_bytes() as u64, 0);
                    let back = detile(&tiled, s, bpe, w, h).unwrap();
                    assert_eq!(back, lin, "{s:?} bpe {bpe} {w}x{h}");
                }
            }
        }
    }

    #[test]
    fn swizzles_differ() {
        let lin = pattern_bytes(64 * 64, 8);
        let a = tile(&lin, Swizzle::Sw64KbD, 8, 64, 64).unwrap();
        let b = tile(&lin, Swizzle::Sw4KbS, 8, 64, 64).unwrap();
        assert_ne!(a[..4096], b[..4096]);
    }

    #[test]
    fn detile_rejects_short_and_bad_input() {
        assert!(matches!(
            detile(&[0u8; 100], Swizzle::Sw4KbS, 8, 32, 16),
            Err(Error::Truncated { .. })
        ));
        assert!(detile(&[], Swizzle::Sw4KbS, 8, 0, 16).is_err());
        assert!(detile(&[], Swizzle::Sw4KbS, 5, 1, 1).is_err());
        assert!(tile(&[0u8; 3], Swizzle::Sw4KbS, 4, 1, 1).is_err());
    }

    #[test]
    fn known_addresses() {
        // SW_4KB_S 8-byte: x0 y0 y1 x1 x2 ... => (1,0) -> elem 1, (0,1) -> elem 2, (2,0) -> elem 8.
        let g = TileGeometry::new(Swizzle::Sw4KbS, 8).unwrap();
        assert_eq!(g.element_offset(1, 0, 1), 8);
        assert_eq!(g.element_offset(0, 1, 1), 16);
        assert_eq!(g.element_offset(0, 2, 1), 32);
        assert_eq!(g.element_offset(2, 0, 1), 64);
        // Second block of a two-block-wide surface.
        assert_eq!(g.element_offset(32, 0, 2), 4096);
        // SW_64KB_D 1-byte: y1 comes before y0.
        let g = TileGeometry::new(Swizzle::Sw64KbD, 1).unwrap();
        assert_eq!(g.element_offset(0, 2, 1), 8);
        assert_eq!(g.element_offset(0, 1, 1), 16);
    }

    #[test]
    fn mip_chain_basics() {
        // 64x64 BC1 in SW_4KB_S: 16x16 elements fit the tail (block 32x16), so all 7 mips are in one block.
        let c = mip_chain(Swizzle::Sw4KbS, 8, 4, 64, 64, 7).unwrap();
        assert_eq!(c.first_tail_mip, Some(0));
        assert_eq!(c.slice_size, 4096);
        assert!(c.levels.iter().all(|l| l.in_tail));
        // Single mip: never a tail.
        let c = mip_chain(Swizzle::Sw4KbS, 8, 4, 64, 64, 1).unwrap();
        assert_eq!(c.first_tail_mip, None);
        assert_eq!(c.levels[0].offset, 0);
        // 512x512 BC1 SW_64KB_D: mip 0 is 128x128 elements = 2 blocks, last in the chain.
        let c = mip_chain(Swizzle::Sw64KbD, 8, 4, 512, 512, 10).unwrap();
        let l0 = &c.levels[0];
        assert_eq!(l0.len, 2 * 65536);
        assert_eq!(l0.offset + l0.len, c.slice_size);
        assert!(c.levels[1].offset < l0.offset);
        assert!(mip_chain(Swizzle::Sw64KbD, 8, 4, 512, 512, 11).is_err());
        assert!(mip_chain(Swizzle::Sw64KbD, 8, 4, 0, 512, 1).is_err());
    }

    #[test]
    fn tail_slots_do_not_overlap() {
        for s in ALL {
            for bpe in SIZES {
                for &(w, h, bd) in &[
                    (64u32, 64u32, 4u32),
                    (256, 128, 4),
                    (64, 32, 1),
                    (16, 16, 1),
                ] {
                    let mips = full_mip_count(w, h);
                    let c = mip_chain(s, bpe, bd, w, h, mips).unwrap();
                    let mut used = std::collections::HashSet::new();
                    for l in c.levels.iter().filter(|l| l.in_tail) {
                        for y in 0..l.elem_height {
                            for x in 0..l.elem_width {
                                assert!(
                                    used.insert((l.origin_x + x, l.origin_y + y)),
                                    "{s:?} {bpe} {w}x{h} overlap"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Cross-check against bcpack-rs (which ports paryzius' ttpf-viewer) for 8-byte elements.
    #[test]
    fn matches_bcpack_rs_for_bc1() {
        use bcpack::ttpf;
        for (tm, s) in [(0u32, Swizzle::Sw64KbD), (1, Swizzle::Sw4KbS)] {
            for &(w, h, mips) in &[
                (512u32, 512u32, 10u32),
                (256, 64, 9),
                (1024, 512, 11),
                (64, 64, 7),
                (128, 128, 1),
            ] {
                let (theirs, total) = ttpf::mip_chain_bc1(tm, w, h, mips).unwrap();
                let ours = mip_chain(s, 8, 4, w, h, mips).unwrap();
                assert_eq!(ours.slice_size, total, "tm {tm} {w}x{h}");
                for (a, b) in ours.levels.iter().zip(theirs.iter()) {
                    assert_eq!(
                        (a.width, a.height, a.offset, a.len),
                        (b.w, b.h, b.off, b.len)
                    );
                    assert_eq!((a.origin_x * 4, a.origin_y * 4), (b.ox, b.oy));
                }
            }
            // Element order of a whole 2x2-block surface.
            let g = TileGeometry::new(s, 8).unwrap();
            let (w, h) = (g.block_width as usize * 2, g.block_height as usize * 2);
            let order = ttpf::untile_order(w * h, w, h, ttpf::pattern_8b(tm));
            for (i, d) in order.iter().enumerate() {
                let d = d.unwrap();
                let (x, y) = ((d % w) as u32, (d / w) as u32);
                assert_eq!(g.element_offset(x, y, 2), i as u64 * 8);
            }
        }
    }
}
