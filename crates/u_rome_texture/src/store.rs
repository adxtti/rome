//! RDS5 texture stores (`.ttpf`) and their ETPF descriptor tables.
//!
//! # Layout
//!
//! Header (bcpack-rs `docs/BCPACK_SPEC.md` §1, Sentinel `ttpf-rds5-etpf.md`,
//! GTA6-Asset-Info §5, ttpf-studio FORMATS.md):
//!
//! | Offset | Field |
//! |---|---|
//! | 0x00 | `RDS5` |
//! | 0x04 | u32 n, chunk-table entries (the last one ends the last chunk) |
//! | 0x08 | u32 virtual size, low 32 bits; 0x0C bits 0-7 are the high bits |
//! | 0x10 | u32 ETPF offset / 256 |
//! | 0x18 | n entries of (u32 a, u32 b) |
//!
//! Entry i starts chunk i, which runs to entry i + 1 in both the file and the
//! virtual (unpacked) address space: file offset `(a & 0x3FFFFFF) << 8`, codec
//! `(a >> 24) & 0x3C`, virtual offset `((a >> 30) | (b & 0xFFFFF) << 2) << 12`,
//! `b >> 20` zero bytes of padding at the end of the stored chunk.
//!
//! **Disputed:** ttpf-studio and GTA6-Asset-Info read the file offset as 24
//! bits and the codec as bits 24-29; bcpack-rs (after its "4 GiB TTPF codec
//! mask" fix) reads 26 offset bits. The two agree whenever bits 24-25 are
//! zero. When they are not, [`IndexLayout`] records which reading gives a
//! consistent table and the store is marked provisional.
//!
//! GTA6-Asset-Info adds 0x1000 to both chunk and texture virtual offsets;
//! bcpack-rs adds it to neither. Only relative positions matter, so we follow
//! bcpack-rs.
//!
//! ETPF: `ETPF`, +4 u32 version (634), +8 u32 count t, +0xC u32 length, then t
//! 16-byte descriptors, then t u64 hashes (descriptor i belongs to hash i).
//!
//! Descriptor (d0, d1, d2, d3):
//! * d0 bits 0-13 width - 1, 14-27 height - 1, 28-31 mip field;
//! * d1 bits 0-2 flags (cube copies - 1), 3-9 DXGI format, 10-19 array size - 1
//!   (depth - 1 for 3D), 20-23 resource kind (2 2D, 4 cube, 6 3D, 10 2D array),
//!   24-27 tile mode;
//! * d2 bits 0-20 first 4 KiB virtual page.
//!
//! **Disputed:** the mip field is "mip count - 1" in bcpack-rs and
//! GTA6-Asset-Info but "mip count" in ttpf-studio; Sentinel reads the mip count
//! from d1 bits 24-31, which the other three sources call the tile mode. We use
//! field + 1 and apply GTA6-Asset-Info's spacing rule: when the chain does
//! not reach the next texture and exactly one mip count does, that count wins
//! (marked provisional). Cube maps use `field + copies - 1`. ttpf-studio and
//! Sentinel read the page as 20 bits; a set bit 20 is marked provisional.
//!
//! Tile modes: 0 is `SW_64KB_D`; 1-3 and the cube modes 9-12 are `SW_4KB_S`.
//! **Disputed:** ttpf-studio (and this task's brief) say mode-0 textures whose
//! mip 0 is under 64 KiB use 4 KiB tiles; bcpack-rs and GTA6-Asset-Info use
//! `SW_64KB_D` for every mode-0 texture. [`TileRule`] selects the reading.

use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;

use u_rome_core::{Image, ImageFormat, Reader};

use crate::bcn::decode_bcn;
use crate::error::{Error, Result};
use crate::export::RawTexture;
use crate::format::{Codec, DxgiFormat};
use crate::gfx10::{MipChain, Swizzle, detile_region, full_mip_count, mip_chain, reserve};

/// Largest unpacked chunk this crate will allocate.
const MAX_CHUNK_BYTES: u64 = 256 << 20;
/// How far past the chunk table to search for a misplaced ETPF table.
const ETPF_SEARCH_WINDOW: usize = 64 << 20;
/// Expected ETPF version (GTA6-Asset-Info §5, Sentinel: 634 in every store).
const ETPF_VERSION: u32 = 634;

/// What kind of input was parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    /// A full `RDS5` store with chunk data.
    Rds5,
    /// A bare `ETPF` table: descriptors only, nothing to decode.
    EtpfOnly,
}

/// How chunk-table entries were read (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexLayout {
    /// Bits 24-25 of every entry are zero, so all sources agree.
    Agreed,
    /// bcpack-rs / Sentinel: 26-bit file offset / 256, codec bits 26-29.
    WideOffset,
    /// ttpf-studio / GTA6-Asset-Info: 24-bit file offset / 256, codec bits 24-29.
    NarrowOffset,
}

/// Which swizzle a tile-mode-0 texture uses (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TileRule {
    /// bcpack-rs, GTA6-Asset-Info: tile mode 0 is always `SW_64KB_D`.
    #[default]
    ByTileMode,
    /// ttpf-studio and the task brief: `SW_64KB_D` when mip 0 is at least
    /// 64 KiB, `SW_4KB_S` otherwise.
    Mip0Size,
}

/// Options for [`TextureStore::parse_with`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ParseOptions {
    pub tile_rule: TileRule,
}

/// ETPF resource kind (d1 bits 20-23, GTA6-Asset-Info §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Texture2D,
    Cube,
    Texture3D,
    Texture2DArray,
    Unknown(u8),
}

/// One chunk of an RDS5 store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkInfo {
    pub index: usize,
    pub file_offset: u64,
    /// Stored bytes including the trailing pad.
    pub file_len: u64,
    pub pad: u64,
    pub virtual_offset: u64,
    pub virtual_len: u64,
    pub codec: Codec,
}

/// One mip level of slice 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MipInfo {
    pub width: u32,
    pub height: u32,
    /// Absolute virtual offset of the level's blocks (slice 0).
    pub virtual_offset: u64,
    pub virtual_len: u64,
    /// Codecs of the chunks holding the level (empty when unknown or not covered).
    pub codecs: Vec<Codec>,
}

impl MipInfo {
    /// True when every chunk holding this level is stored or zlib.
    pub fn is_plain(&self) -> bool {
        !self.codecs.is_empty() && self.codecs.iter().all(|c| c.is_plain())
    }
}

/// One texture described by the ETPF table.
#[derive(Clone, Debug)]
pub struct TextureInfo {
    pub index: usize,
    /// 64-bit texture hash, the key drawable shaders reference.
    pub hash: u64,
    pub width: u32,
    pub height: u32,
    /// Mip count used for decoding (after the rules in the module docs).
    pub mip_count: u32,
    /// Raw d0 bits 28-31.
    pub mip_field: u32,
    pub format: DxgiFormat,
    /// Slices (2D arrays), faces (cubes) or depth (3D).
    pub array_size: u32,
    pub kind: ResourceKind,
    pub tile_mode: u32,
    /// Swizzle used for decoding, `None` when the tile mode is unknown.
    pub swizzle: Option<Swizzle>,
    pub virtual_offset: u64,
    /// The raw descriptor words, for diagnostics.
    pub descriptor: [u32; 4],
    /// Per-mip placement and codecs, largest first.
    pub mips: Vec<MipInfo>,
    /// True when a disputed field or rule affects this texture.
    pub provisional: bool,
    /// Human-readable reasons for `provisional` and other anomalies.
    pub notes: Vec<String>,
    chain: Option<MipChain>,
    layout_error: Option<String>,
}

impl TextureInfo {
    /// Why this texture cannot be decoded, if known at parse time.
    pub fn layout_error(&self) -> Option<&str> {
        self.layout_error.as_deref()
    }

    /// Largest mip whose chunks are all stored or zlib.
    pub fn first_plain_mip(&self) -> Option<u32> {
        self.mips
            .iter()
            .position(MipInfo::is_plain)
            .map(|i| i as u32)
    }

    fn note(&mut self, provisional: bool, msg: String) {
        log::debug!("texture {} ({:016x}): {msg}", self.index, self.hash);
        self.provisional |= provisional;
        self.notes.push(msg);
    }
}

/// Which mips [`TextureStore::decode`] returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MipSelection {
    /// Every mip, largest first.
    All,
    /// One mip.
    Level(u32),
    /// The given mip and every smaller one.
    FromLevel(u32),
    /// Start at the largest mip stored as plain zlib / stored data (exact),
    /// falling back to all mips when none is plain. Recommended for viewing,
    /// since BCPack output is provisional.
    #[default]
    PreferPlain,
}

/// Options for [`TextureStore::decode_with`] and [`TextureStore::decode_raw`].
#[derive(Clone, Copy, Debug)]
pub struct DecodeOptions {
    pub mips: MipSelection,
    /// Array slice or cube face.
    pub slice: u32,
    /// Decode BCPack BC1 chunks (provisional). When false they are `Unsupported`.
    pub allow_bcpack: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        DecodeOptions {
            mips: MipSelection::PreferPlain,
            slice: 0,
            allow_bcpack: true,
        }
    }
}

/// A parsed texture store. It holds metadata only; pass the same bytes to
/// [`TextureStore::decode`].
#[derive(Clone, Debug)]
pub struct TextureStore {
    pub kind: StoreKind,
    pub index_layout: IndexLayout,
    pub virtual_size: u64,
    pub etpf_offset: u64,
    pub etpf_version: u32,
    pub tile_rule: TileRule,
    /// True when a store-level field was disputed or recovered heuristically.
    pub provisional: bool,
    pub notes: Vec<String>,
    chunks: Vec<ChunkInfo>,
    /// Non-empty chunk indices sorted by virtual offset.
    by_virtual: Vec<usize>,
    textures: Vec<TextureInfo>,
    data_len: u64,
}

#[derive(Clone, Copy)]
struct Entry {
    off: u64,
    codec: u8,
    vstart: u64,
    pad: u64,
}

fn read_entry(a: u32, b: u32, layout: IndexLayout) -> Entry {
    let off = match layout {
        IndexLayout::NarrowOffset => (a & 0xFF_FFFF) as u64,
        _ => (a & 0x3FF_FFFF) as u64,
    } << 8;
    Entry {
        off,
        codec: ((a >> 24) & 0x3C) as u8,
        vstart: (((a >> 30) as u64) | (((b & 0xF_FFFF) as u64) << 2)) << 12,
        pad: (b >> 20) as u64,
    }
}

fn layout_consistent(entries: &[(u32, u32)], layout: IndexLayout, len: u64) -> bool {
    let mut prev = 0u64;
    let last = entries.len().saturating_sub(1);
    for (i, &(a, b)) in entries.iter().enumerate() {
        let e = read_entry(a, b, layout);
        if i == last && e.off == 0 {
            break; // zero terminator
        }
        if e.off < prev || e.off > len {
            return false;
        }
        prev = e.off;
    }
    true
}

fn detect_layout(entries: &[(u32, u32)], len: u64) -> (IndexLayout, Option<String>) {
    let flagged = entries.iter().filter(|(a, _)| a & 0x0300_0000 != 0).count();
    if flagged == 0 {
        return (IndexLayout::Agreed, None);
    }
    let wide = layout_consistent(entries, IndexLayout::WideOffset, len);
    let narrow = layout_consistent(entries, IndexLayout::NarrowOffset, len);
    let (layout, how) = match (wide, narrow) {
        (true, _) => (
            IndexLayout::WideOffset,
            "26-bit offsets (bcpack-rs) are consistent",
        ),
        (false, true) => (
            IndexLayout::NarrowOffset,
            "only 24-bit offsets (ttpf-studio) are consistent",
        ),
        (false, false) => (
            IndexLayout::WideOffset,
            "neither reading is consistent; using 26-bit offsets",
        ),
    };
    let msg = format!("{flagged} chunk-table entries have bits 24-25 set: {how}");
    log::warn!("RDS5: {msg}");
    (layout, Some(msg))
}

fn find_magic(data: &[u8], from: usize, magic: &[u8; 4]) -> Option<usize> {
    let end = data.len().min(from.saturating_add(ETPF_SEARCH_WINDOW));
    data.get(from..end)?
        .windows(4)
        .position(|w| w == magic)
        .map(|p| p + from)
}

fn has_magic(data: &[u8], at: u64, magic: &[u8; 4]) -> bool {
    usize::try_from(at)
        .ok()
        .and_then(|a| data.get(a..a.checked_add(4)?))
        .is_some_and(|m| m == magic)
}

fn select_swizzle(tile_mode: u32, mip0_bytes: u64, rule: TileRule) -> Option<(Swizzle, bool)> {
    match tile_mode {
        0 => {
            let small = mip0_bytes < 65536;
            let sw = match rule {
                TileRule::Mip0Size if small => Swizzle::Sw4KbS,
                _ => Swizzle::Sw64KbD,
            };
            Some((sw, small))
        }
        1..=3 | 9..=12 => Some((Swizzle::Sw4KbS, false)),
        _ => None,
    }
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Byte offset of the full-resolution copy of a cube with `copies` stored copies.
///
/// GTA6-Asset-Info §5 (facts): units smallest first are the tail block and
/// each non-tail mip; copy k holds the smallest `u - c + 1 + k` units of every
/// face; a copy over 128 KiB is padded to a multiple of 64 KiB.
fn cube_full_copy_start(chain: &MipChain, faces: u32, copies: u32) -> Result<u64> {
    let units = chain.unit_sizes();
    let (u, c) = (units.len() as u32, copies);
    if c == 0 || c > u {
        return Err(Error::UnsupportedLayout(format!(
            "cube with {c} copies but only {u} mip units"
        )));
    }
    let mut start = 0u64;
    for k in 0..c - 1 {
        let n = (u - c + 1 + k) as usize;
        let face: u64 = units.iter().take(n).sum();
        let size = face * faces as u64;
        start += if size > 128 << 10 {
            align_up(size, 64 << 10)
        } else {
            size
        };
    }
    Ok(start)
}

fn bcpack_tables() -> &'static (bcpack::bc1::Window, bcpack::bc1::QuadTable) {
    static TABLES: OnceLock<(bcpack::bc1::Window, bcpack::bc1::QuadTable)> = OnceLock::new();
    TABLES.get_or_init(|| {
        (
            bcpack::bc1::Window::default_table(),
            bcpack::bc1::QuadTable::default_table(),
        )
    })
}

impl TextureStore {
    /// Parse an `RDS5` store or a bare `ETPF` table with default options.
    pub fn parse(bytes: &[u8]) -> Result<TextureStore> {
        Self::parse_with(bytes, ParseOptions::default())
    }

    /// Parse with explicit options (see [`TileRule`]).
    pub fn parse_with(bytes: &[u8], options: ParseOptions) -> Result<TextureStore> {
        let magic = bytes.get(..4).unwrap_or(bytes);
        match magic {
            b"RDS5" => Self::parse_rds5(bytes, options),
            b"ETPF" => {
                let mut store = TextureStore {
                    kind: StoreKind::EtpfOnly,
                    index_layout: IndexLayout::Agreed,
                    virtual_size: 0,
                    etpf_offset: 0,
                    etpf_version: 0,
                    tile_rule: options.tile_rule,
                    provisional: false,
                    notes: Vec::new(),
                    chunks: Vec::new(),
                    by_virtual: Vec::new(),
                    textures: Vec::new(),
                    data_len: bytes.len() as u64,
                };
                store.parse_etpf(bytes, 0)?;
                Ok(store)
            }
            _ => Err(Error::NotAStore(format!(
                "expected RDS5 or ETPF magic, found {magic:02x?}"
            ))),
        }
    }

    fn parse_rds5(data: &[u8], options: ParseOptions) -> Result<TextureStore> {
        let r = Reader::new(data);
        if data.len() < 0x18 {
            return Err(Error::Truncated {
                what: "RDS5 header",
                needed: 0x18,
                have: data.len() as u64,
            });
        }
        let n = r.u32_at(4)? as u64;
        let vsize = r.u32_at(8)? as u64 | ((r.u32_at(0xC)? as u64 & 0xFF) << 32);
        let etpf_pages = r.u32_at(0x10)? as u64;
        let table_end = 0x18 + 8 * n;
        if table_end > data.len() as u64 {
            return Err(Error::Malformed {
                what: "RDS5 chunk table",
                offset: 4,
                detail: format!(
                    "{n} entries end at {table_end:#x}, past the end of the file ({:#x})",
                    data.len()
                ),
            });
        }
        let mut entries = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            let o = 0x18 + 8 * i;
            entries.push((r.u32_at(o)?, r.u32_at(o + 4)?));
        }
        let (index_layout, layout_note) = detect_layout(&entries, data.len() as u64);
        let mut store = TextureStore {
            kind: StoreKind::Rds5,
            index_layout,
            virtual_size: vsize,
            etpf_offset: 0,
            etpf_version: 0,
            tile_rule: options.tile_rule,
            provisional: layout_note.is_some(),
            notes: layout_note.into_iter().collect(),
            chunks: Vec::with_capacity(entries.len().saturating_sub(1)),
            by_virtual: Vec::new(),
            textures: Vec::new(),
            data_len: data.len() as u64,
        };
        store.build_chunks(&entries);

        // ETPF: header pointer first, then the 0x100-aligned end of the table
        // (both GTA6-Asset-Info variants), then a bounded search (ttpf-studio).
        let candidates = [
            etpf_pages << 8,
            align_up(table_end, 0x100),
            align_up(0x10 + 8 * n, 0x100),
        ];
        let etpf = match candidates.iter().position(|&o| has_magic(data, o, b"ETPF")) {
            Some(0) => candidates[0],
            Some(i) => {
                store.note_store(format!(
                    "ETPF not at header offset {:#x}; found at {:#x}",
                    candidates[0], candidates[i]
                ));
                candidates[i]
            }
            None => match find_magic(data, table_end as usize, b"ETPF") {
                Some(o) => {
                    store.note_store(format!(
                        "ETPF not at header offset {:#x}; found by search at {o:#x}",
                        candidates[0]
                    ));
                    o as u64
                }
                None => {
                    return Err(Error::Malformed {
                        what: "RDS5 header",
                        offset: 0x10,
                        detail: format!(
                            "no ETPF table at {:#x} or within {} MiB after the chunk table",
                            candidates[0],
                            ETPF_SEARCH_WINDOW >> 20
                        ),
                    });
                }
            },
        };
        store.parse_etpf(data, etpf)?;
        log::debug!(
            "RDS5: {} chunks, {} textures, virtual size {:#x}, ETPF at {:#x}, layout {:?}",
            store.chunks.len(),
            store.textures.len(),
            store.virtual_size,
            store.etpf_offset,
            store.index_layout
        );
        Ok(store)
    }

    fn note_store(&mut self, msg: String) {
        log::warn!("RDS5: {msg}");
        self.provisional = true;
        self.notes.push(msg);
    }

    fn build_chunks(&mut self, entries: &[(u32, u32)]) {
        let len = self.data_len;
        let count = entries.len().saturating_sub(1);
        for i in 0..count {
            let (Some(&(a, b)), Some(&(a2, b2))) = (entries.get(i), entries.get(i + 1)) else {
                break;
            };
            let e = read_entry(a, b, self.index_layout);
            let mut next = read_entry(a2, b2, self.index_layout);
            if i + 1 == count && (next.off < e.off || (a2 == 0 && b2 == 0)) {
                // Zero terminator: the last chunk runs to the end of the file / virtual space.
                log::debug!("RDS5: zero terminator, last chunk {i} runs to end of file");
                next.off = len;
                next.vstart = self.virtual_size.max(e.vstart);
            }
            if next.off < e.off || next.vstart < e.vstart {
                log::warn!(
                    "RDS5: chunk {i} at {:#x} runs backwards (next entry file {:#x}, virtual {:#x} -> {:#x})",
                    e.off,
                    next.off,
                    e.vstart,
                    next.vstart
                );
            }
            if e.off > len {
                log::warn!(
                    "RDS5: chunk {i} starts at {:#x}, past end of file ({len:#x})",
                    e.off
                );
            }
            self.chunks.push(ChunkInfo {
                index: i,
                file_offset: e.off,
                file_len: next.off.saturating_sub(e.off),
                pad: e.pad,
                virtual_offset: e.vstart,
                virtual_len: next.vstart.saturating_sub(e.vstart),
                codec: Codec::from_id(e.codec),
            });
        }
        let mut by: Vec<usize> = (0..self.chunks.len())
            .filter(|&i| self.chunks.get(i).is_some_and(|c| c.virtual_len > 0))
            .collect();
        by.sort_by_key(|&i| self.chunks.get(i).map_or(0, |c| c.virtual_offset));
        self.by_virtual = by;
    }

    fn parse_etpf(&mut self, data: &[u8], at: u64) -> Result<()> {
        let r = Reader::new(data);
        let o = usize::try_from(at).map_err(|_| Error::Malformed {
            what: "ETPF table",
            offset: at,
            detail: "offset does not fit in memory".into(),
        })?;
        r.bytes_at(o, 0x10)?;
        let version = r.u32_at(o + 4)?;
        let t = r.u32_at(o + 8)? as u64;
        let desc = at + 0x10;
        let hashes = desc + 16 * t;
        let end = hashes + 8 * t;
        if end > data.len() as u64 {
            return Err(Error::Malformed {
                what: "ETPF table",
                offset: at,
                detail: format!(
                    "{t} descriptors and hashes end at {end:#x}, past the end of the input ({:#x})",
                    data.len()
                ),
            });
        }
        self.etpf_offset = at;
        self.etpf_version = version;
        if version != ETPF_VERSION {
            self.note_store(format!(
                "ETPF version {version} (expected {ETPF_VERSION}); field layout may differ"
            ));
        }
        let mut raw = Vec::with_capacity(t as usize);
        for i in 0..t as usize {
            let d = desc as usize + 16 * i;
            let h = hashes as usize + 8 * i;
            raw.push((
                r.u64_at(h)?,
                [
                    r.u32_at(d)?,
                    r.u32_at(d + 4)?,
                    r.u32_at(d + 8)?,
                    r.u32_at(d + 12)?,
                ],
            ));
        }
        let mut starts: Vec<u64> = raw
            .iter()
            .map(|(_, d)| ((d[2] & 0x1F_FFFF) as u64) << 12)
            .collect();
        starts.sort_unstable();
        starts.dedup();
        let mut textures = Vec::with_capacity(raw.len());
        for (i, (hash, d)) in raw.into_iter().enumerate() {
            let v = ((d[2] & 0x1F_FFFF) as u64) << 12;
            let next = starts.get(starts.partition_point(|&s| s <= v)).copied();
            let mut info = self.build_info(i, hash, d, next);
            if self.provisional {
                info.provisional = true;
            }
            textures.push(info);
        }
        self.textures = textures;
        Ok(())
    }

    fn build_info(&self, index: usize, hash: u64, d: [u32; 4], next: Option<u64>) -> TextureInfo {
        let width = (d[0] & 0x3FFF) + 1;
        let height = ((d[0] >> 14) & 0x3FFF) + 1;
        let mip_field = d[0] >> 28;
        let format = DxgiFormat((d[1] >> 3) & 0x7F);
        let array_size = ((d[1] >> 10) & 0x3FF) + 1;
        let kind_raw = ((d[1] >> 20) & 0xF) as u8;
        let tile_mode = (d[1] >> 24) & 0xF;
        let page = d[2] & 0x1F_FFFF;
        let is_cube = (9..=12).contains(&tile_mode);
        let kind = match kind_raw {
            _ if is_cube => ResourceKind::Cube,
            2 => ResourceKind::Texture2D,
            4 => ResourceKind::Cube,
            6 => ResourceKind::Texture3D,
            10 => ResourceKind::Texture2DArray,
            k => ResourceKind::Unknown(k),
        };
        let mut info = TextureInfo {
            index,
            hash,
            width,
            height,
            mip_count: 1,
            mip_field,
            format,
            array_size,
            kind,
            tile_mode,
            swizzle: None,
            virtual_offset: (page as u64) << 12,
            descriptor: d,
            mips: Vec::new(),
            provisional: false,
            notes: Vec::new(),
            chain: None,
            layout_error: None,
        };
        if page & 0x10_0000 != 0 {
            info.note(
                true,
                "virtual page uses bit 20 (21-bit reading of bcpack-rs / GTA6-Asset-Info; ttpf-studio and Sentinel read 20 bits)".into(),
            );
        }
        if let ResourceKind::Unknown(k) = kind {
            info.note(true, format!("unknown resource kind {k}; treated as 2D"));
        }
        if kind_raw == 4 && !is_cube {
            info.note(
                true,
                format!("cube resource kind with tile mode {tile_mode}"),
            );
        }
        let max_mips = full_mip_count(width, height);
        let mut mips = if is_cube {
            let copies = tile_mode - 8;
            info.note(
                true,
                format!("cube map, {copies} stored copies; mip count = field + copies - 1 (GTA6-Asset-Info)"),
            );
            (mip_field + copies).saturating_sub(1).max(1)
        } else {
            mip_field + 1
        };
        if mips > max_mips {
            info.note(true, format!("mip count {mips} clamped to {max_mips}"));
            mips = max_mips;
        }
        info.mip_count = mips;

        let Some(layout) = format.layout() else {
            info.layout_error = Some(format!("pixel format {format} is not supported"));
            self.fill_mips(&mut info);
            return info;
        };
        if kind == ResourceKind::Texture3D {
            info.layout_error = Some("3D volume textures are not supported yet".into());
            self.fill_mips(&mut info);
            return info;
        }
        let mip0 = layout.linear_size(width, height);
        let Some((swizzle, disputed)) = select_swizzle(tile_mode, mip0, self.tile_rule) else {
            info.layout_error = Some(format!("tile mode {tile_mode} is not known"));
            self.fill_mips(&mut info);
            return info;
        };
        info.swizzle = Some(swizzle);
        if disputed {
            info.note(
                true,
                format!(
                    "tile mode 0 with mip 0 under 64 KiB ({mip0} bytes): sources disagree on SW_64KB_D vs SW_4KB_S; using {swizzle:?}"
                ),
            );
        }
        if swizzle == Swizzle::Sw64KbD && layout.bytes_per_element == 4 {
            info.note(
                true,
                "4-byte SW_64KB_D micro tile differs between ttpf-studio and GTA6-Asset-Info"
                    .into(),
            );
        }
        let (bpe, bd) = (layout.bytes_per_element, layout.block_dim);
        let slices = array_size as u64;
        let total = |m: u32| -> Option<u64> {
            mip_chain(swizzle, bpe, bd, width, height, m)
                .ok()
                .map(|c| c.slice_size * slices)
        };
        if let (Some(next), false) = (next, is_cube) {
            let v = info.virtual_offset;
            if total(mips).map(|t| v + t) != Some(next) {
                let fits: Vec<u32> = (1..=max_mips)
                    .filter(|&m| total(m).map(|t| v + t) == Some(next))
                    .collect();
                if let [m] = fits[..] {
                    info.note(
                        true,
                        format!(
                            "mip count corrected from {mips} to {m} by store spacing (mip field unreliable, GTA6-Asset-Info)"
                        ),
                    );
                    mips = m;
                    info.mip_count = m;
                }
            }
        }
        match mip_chain(swizzle, bpe, bd, width, height, mips) {
            Ok(c) => info.chain = Some(c),
            Err(e) => info.layout_error = Some(e.to_string()),
        }
        self.fill_mips(&mut info);
        info
    }

    /// Byte offset of (mip, slice) relative to the texture's virtual start.
    fn level_address(info: &TextureInfo, chain: &MipChain, mip: u32, slice: u32) -> Result<u64> {
        let l = chain.levels.get(mip as usize).ok_or(Error::MipOutOfRange {
            mip,
            count: chain.levels.len() as u32,
        })?;
        if slice >= info.array_size {
            return Err(Error::SliceOutOfRange {
                slice,
                count: info.array_size,
            });
        }
        if info.kind == ResourceKind::Cube {
            let copies = info.tile_mode.saturating_sub(8);
            let start = cube_full_copy_start(chain, info.array_size, copies)?;
            Ok(start + slice as u64 * chain.slice_size + l.offset)
        } else {
            // Arrays: address(m, s) = offset(m) * slices + s * size(m) (GTA6-Asset-Info §5).
            Ok(l.offset * info.array_size as u64 + slice as u64 * l.len)
        }
    }

    fn fill_mips(&self, info: &mut TextureInfo) {
        let mut mips = Vec::with_capacity(info.mip_count as usize);
        for m in 0..info.mip_count {
            let (w, h) = ((info.width >> m).max(1), (info.height >> m).max(1));
            let (voff, vlen, codecs) = match &info.chain {
                Some(chain) => match Self::level_address(info, chain, m, 0) {
                    Ok(addr) => {
                        let len = chain.levels.get(m as usize).map_or(0, |l| l.len);
                        let start = info.virtual_offset + addr;
                        (start, len, self.codecs_in(start, start + len))
                    }
                    Err(_) => (0, 0, Vec::new()),
                },
                None => (0, 0, Vec::new()),
            };
            mips.push(MipInfo {
                width: w,
                height: h,
                virtual_offset: voff,
                virtual_len: vlen,
                codecs,
            });
        }
        info.mips = mips;
    }

    /// Chunks covering [start, end): (chunk index, from, to) relative to the
    /// chunk. Stops at the first gap.
    fn locate(&self, start: u64, end: u64) -> Vec<(usize, u64, u64)> {
        let mut out = Vec::new();
        let mut pos = start;
        while pos < end {
            let i = self.by_virtual.partition_point(|&ci| {
                self.chunks.get(ci).is_some_and(|c| c.virtual_offset <= pos)
            });
            let Some(c) = i
                .checked_sub(1)
                .and_then(|i| self.by_virtual.get(i))
                .and_then(|&ci| self.chunks.get(ci))
            else {
                break;
            };
            let c_end = c.virtual_offset + c.virtual_len;
            if pos >= c_end {
                break;
            }
            let e = end.min(c_end);
            out.push((c.index, pos - c.virtual_offset, e - c.virtual_offset));
            pos = e;
        }
        out
    }

    fn codecs_in(&self, start: u64, end: u64) -> Vec<Codec> {
        let mut v: Vec<Codec> = Vec::new();
        for (ci, _, _) in self.locate(start, end) {
            if let Some(c) = self.chunks.get(ci)
                && !v.contains(&c.codec)
            {
                v.push(c.codec);
            }
        }
        v
    }

    /// Texture descriptors, in ETPF order.
    pub fn textures(&self) -> &[TextureInfo] {
        &self.textures
    }

    /// Chunk table, in file order (empty for a bare ETPF table).
    pub fn chunks(&self) -> &[ChunkInfo] {
        &self.chunks
    }

    /// Index of the texture with this 64-bit hash.
    pub fn find(&self, hash: u64) -> Option<usize> {
        // ETPF hashes are sorted (GTA6-Asset-Info §5); fall back to a scan if not.
        match self.textures.binary_search_by_key(&hash, |t| t.hash) {
            Ok(i) => Some(i),
            Err(_) => self.textures.iter().position(|t| t.hash == hash),
        }
    }

    /// Decode a texture to RGBA8 (slice 0, BCPack allowed).
    pub fn decode(&self, bytes: &[u8], index: usize, mips: MipSelection) -> Result<Image> {
        self.decode_with(
            bytes,
            index,
            &DecodeOptions {
                mips,
                ..DecodeOptions::default()
            },
        )
    }

    /// Decode a texture to RGBA8 with explicit options.
    pub fn decode_with(&self, bytes: &[u8], index: usize, opts: &DecodeOptions) -> Result<Image> {
        let raw = self.decode_raw(bytes, index, opts)?;
        let mut mips = Vec::with_capacity(raw.mips.len());
        for (i, data) in raw.mips.iter().enumerate() {
            let l = i as u32;
            let (w, h) = ((raw.width >> l).max(1), (raw.height >> l).max(1));
            mips.push(decode_bcn(raw.format, w, h, data)?);
        }
        Ok(Image {
            width: raw.width,
            height: raw.height,
            format: if raw.format.is_srgb() {
                ImageFormat::Rgba8Srgb
            } else {
                ImageFormat::Rgba8
            },
            mips,
            provisional: raw.provisional,
        })
    }

    /// Unpack and detile a texture without decoding the pixels: linear BCn
    /// blocks or texels per mip, ready for [`crate::export_dds_raw`].
    pub fn decode_raw(
        &self,
        bytes: &[u8],
        index: usize,
        opts: &DecodeOptions,
    ) -> Result<RawTexture> {
        let info = self.textures.get(index).ok_or(Error::IndexOutOfRange {
            index,
            count: self.textures.len(),
        })?;
        if self.kind == StoreKind::EtpfOnly {
            return Err(Error::NoData);
        }
        let Some(layout) = info.format.layout() else {
            let codec = info
                .mips
                .first()
                .map(|m| codec_list(&m.codecs))
                .unwrap_or_else(|| "unknown".into());
            return Err(Error::Unsupported {
                codec,
                format: info.format.to_string(),
            });
        };
        let chain = match (&info.chain, &info.layout_error) {
            (Some(c), _) => c,
            (None, e) => {
                return Err(Error::UnsupportedLayout(format!(
                    "texture {index} ({:016x}): {}",
                    info.hash,
                    e.as_deref().unwrap_or("no mip layout")
                )));
            }
        };
        if opts.slice >= info.array_size {
            return Err(Error::SliceOutOfRange {
                slice: opts.slice,
                count: info.array_size,
            });
        }
        let count = info.mip_count;
        let check = |m: u32| {
            if m < count {
                Ok(m)
            } else {
                Err(Error::MipOutOfRange { mip: m, count })
            }
        };
        let range = |m: u32| -> Result<(u64, u64)> {
            let addr = Self::level_address(info, chain, m, opts.slice)?;
            let len = chain.levels.get(m as usize).map_or(0, |l| l.len);
            let start = info.virtual_offset + addr;
            Ok((start, start + len))
        };
        let (first, last) = match opts.mips {
            MipSelection::All => (0, count),
            MipSelection::Level(m) => (check(m)?, m + 1),
            MipSelection::FromLevel(m) => (check(m)?, count),
            MipSelection::PreferPlain => {
                let mut first = 0;
                for m in 0..count {
                    let (s, e) = range(m)?;
                    let codecs = self.codecs_in(s, e);
                    if !codecs.is_empty() && codecs.iter().all(|c| c.is_plain()) {
                        first = m;
                        break;
                    }
                }
                (first, count)
            }
        };
        let mut raw = RawTexture {
            format: info.format,
            width: (info.width >> first).max(1),
            height: (info.height >> first).max(1),
            first_mip: first,
            mips: Vec::with_capacity((last - first) as usize),
            provisional: info.provisional,
            notes: info.notes.clone(),
        };
        let mut cache: HashMap<usize, Vec<u8>> = HashMap::new();
        let g = &chain.geometry;
        for m in first..last {
            let l = chain
                .levels
                .get(m as usize)
                .ok_or(Error::MipOutOfRange { mip: m, count })?;
            let (start, end) = range(m)?;
            let mut tiled = Vec::new();
            reserve(&mut tiled, end - start, "tiled mip")?;
            for (ci, from, to) in self.locate(start, end) {
                let data = match cache.entry(ci) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let (data, note) = self.unpack_chunk(bytes, ci, info, opts.allow_bcpack)?;
                        if let Some(n) = note {
                            raw.provisional = true;
                            raw.notes.push(n);
                        }
                        e.insert(data)
                    }
                };
                let part = data
                    .get(from as usize..to as usize)
                    .ok_or_else(|| Error::Chunk {
                        chunk: ci,
                        codec: self
                            .chunks
                            .get(ci)
                            .map_or("?".into(), |c| c.codec.to_string()),
                        offset: self.chunks.get(ci).map_or(0, |c| c.file_offset),
                        detail: format!("unpacked data shorter than {to} bytes"),
                    })?;
                tiled.extend_from_slice(part);
            }
            let mut linear = Vec::new();
            reserve(
                &mut linear,
                layout.linear_size(l.width, l.height),
                "linear mip",
            )?;
            detile_region(
                &tiled,
                g,
                l.pitch_blocks,
                (l.origin_x, l.origin_y, l.elem_width, l.elem_height),
                &mut linear,
            )
            .map_err(|off| {
                log::warn!(
                    "texture {index} ({:016x}) mip {m}: data ends at virtual {:#x}, needed {:#x}",
                    info.hash,
                    start + tiled.len() as u64,
                    start + off
                );
                if tiled.len() as u64 >= end - start {
                    Error::Truncated {
                        what: "mip data",
                        needed: off + g.bytes_per_element as u64,
                        have: tiled.len() as u64,
                    }
                } else {
                    Error::NotCovered {
                        start: start + tiled.len() as u64,
                        end,
                    }
                }
            })?;
            raw.mips.push(linear);
        }
        log::debug!(
            "decoded texture {index} ({:016x}) {} mips {first}..{last}, provisional {}",
            info.hash,
            info.format,
            raw.provisional
        );
        Ok(raw)
    }

    /// Unpack one chunk to its virtual bytes. Returns a provisional note for BCPack.
    fn unpack_chunk(
        &self,
        bytes: &[u8],
        ci: usize,
        info: &TextureInfo,
        allow_bcpack: bool,
    ) -> Result<(Vec<u8>, Option<String>)> {
        let c = self.chunks.get(ci).ok_or_else(|| Error::Malformed {
            what: "chunk index",
            offset: 0,
            detail: format!("chunk {ci} does not exist"),
        })?;
        let err = |detail: String| Error::Chunk {
            chunk: ci,
            codec: c.codec.to_string(),
            offset: c.file_offset,
            detail,
        };
        if c.virtual_len > MAX_CHUNK_BYTES {
            return Err(Error::TooLarge {
                what: "unpacked chunk",
                requested: c.virtual_len,
                limit: MAX_CHUNK_BYTES,
            });
        }
        let vlen = c.virtual_len as usize;
        let raw = usize::try_from(c.file_offset)
            .ok()
            .zip(usize::try_from(c.file_offset.saturating_add(c.file_len)).ok())
            .and_then(|(a, b)| bytes.get(a..b))
            .ok_or_else(|| {
                err(format!(
                    "{} stored bytes past the end of the input ({} bytes)",
                    c.file_len,
                    bytes.len()
                ))
            })?;
        let data_len = c.file_len.checked_sub(c.pad).ok_or_else(|| {
            err(format!(
                "padding {} exceeds stored length {}",
                c.pad, c.file_len
            ))
        })? as usize;
        let payload = raw.get(..data_len).unwrap_or(raw);
        match c.codec {
            Codec::Stored => {
                let d = raw.get(..vlen).ok_or_else(|| {
                    err(format!("{} stored bytes, virtual length {vlen}", raw.len()))
                })?;
                Ok((d.to_vec(), None))
            }
            Codec::Zlib => {
                let mut out = Vec::new();
                reserve(&mut out, vlen as u64, "inflated chunk")?;
                flate2::read::ZlibDecoder::new(payload)
                    .take(vlen as u64)
                    .read_to_end(&mut out)
                    .map_err(|e| err(format!("zlib: {e}")))?;
                if out.len() < vlen {
                    return Err(err(format!("inflated {} of {vlen} bytes", out.len())));
                }
                Ok((out, None))
            }
            Codec::BcPack(1) if allow_bcpack => {
                if !vlen.is_multiple_of(8) {
                    return Err(err(format!(
                        "virtual length {vlen} is not whole BC1 blocks"
                    )));
                }
                let (window, quads) = bcpack_tables();
                // bcpack-rs takes the stored bytes including the zero pad.
                let res =
                    std::panic::catch_unwind(|| bcpack::bc1::decode(raw, vlen / 8, window, quads));
                let out = match res {
                    Ok(Ok(o)) => o,
                    Ok(Err(e)) => return Err(err(format!("BCPack BC1: {e}"))),
                    Err(_) => return Err(err("BCPack BC1 decoder panicked".into())),
                };
                if out.blocks.len() != vlen {
                    return Err(err(format!(
                        "BCPack BC1 produced {} bytes, expected {vlen}",
                        out.blocks.len()
                    )));
                }
                let note = format!(
                    "chunk {ci} decoded from BCPack BC1 (fitted tables, provisional; {} blocks depend on unfitted window slots)",
                    out.unfitted_blocks
                );
                Ok((out.blocks, Some(note)))
            }
            other => Err(Error::Unsupported {
                codec: format!("{other} (chunk {ci} at file offset {:#x})", c.file_offset),
                format: info.format.to_string(),
            }),
        }
    }
}

fn codec_list(codecs: &[Codec]) -> String {
    if codecs.is_empty() {
        return "unknown".into();
    }
    codecs
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
