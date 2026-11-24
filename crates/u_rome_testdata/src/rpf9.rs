//! Synthetic RPF9 archives.
//!
//! File layout produced by [`Rpf9Builder::build`]:
//!
//! ```text
//! 0x00            64-byte header (little-endian, see below)
//! 0x40            front table A (noise, 16 bytes per entry, min 16)
//! table_split     front table B (noise)
//! payload_start   entries, each starting on a 16-byte boundary
//! trailer_offset  trailer (noise), 16-byte aligned, `trailer_len` bytes, then EOF
//! ```
//!
//! Header:
//!
//! | off  | type | value                                   |
//! |------|------|-----------------------------------------|
//! | 0x00 | [u8;4] | `9FPR`                                |
//! | 0x04 | u32  | payload start (absolute)                |
//! | 0x08 | u32  | table split: absolute offset of table B |
//! | 0x0C | u32  | trailer length                          |
//! | 0x10 | u64  | archive id                              |
//! | 0x18 | u32  | generation `0x00047a00`                 |
//! | 0x1C | u32  | `0x150`                                 |
//! | 0x20 | u32  | version `0x10003`                       |
//! | 0x24 | u32  | entry count (top-level entries)         |
//! | 0x28 | u32  | key id                                  |
//! | 0x2C | u32  | flags `0x00090F01`                      |
//! | 0x30 | u32  | `0x10`                                  |
//! | 0x34 | -    | zero to 0x40                            |
//!
//! Entry encodings:
//!
//! - **Zlib**: zlib stream(s) at the entry offset. Payloads larger than
//!   [`PAGE_SIZE`] are split into pages: each page is an independent zlib
//!   stream that inflates to exactly 65536 bytes, except the last; after each
//!   page the file is zero-padded to the next 16-byte boundary where the next
//!   page starts.
//! - **Virtual**: 16 zero bytes, then the same (paged) zlib encoding at +0x10.
//! - **Stored**: raw bytes (e.g. `ADAT`, `KB2j`).
//! - **NestedStored**: a complete nested RPF9 archive, raw.
//! - **NestedZlib**: a complete nested RPF9 archive inside the (paged) zlib
//!   encoding.
//!
//! The gap between entries is zero padding only.

use u_rome_core::{AssetKind, detect};

use crate::{ALIGN, PAGE_SIZE, Prng, align_up, pad_to_align, zlib_compress};

/// Archive magic at offset 0.
pub const MAGIC: [u8; 4] = *b"9FPR";
/// Size of the fixed header.
pub const HEADER_LEN: usize = 64;
/// Header field at 0x18.
pub const GENERATION: u32 = 0x0004_7a00;
/// Header field at 0x1C.
pub const HEADER_0X1C: u32 = 0x150;
/// Header field at 0x20.
pub const VERSION: u32 = 0x0001_0003;
/// Header field at 0x2C.
pub const FLAGS: u32 = 0x0009_0F01;
/// Header field at 0x30.
pub const HEADER_0X30: u32 = 0x10;

/// How an entry was encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Zlib,
    Virtual,
    Stored,
    NestedStored,
    NestedZlib,
}

impl EntryKind {
    /// True for encodings that carry zlib pages.
    pub fn is_compressed(self) -> bool {
        matches!(
            self,
            EntryKind::Zlib | EntryKind::Virtual | EntryKind::NestedZlib
        )
    }
}

/// One zlib page (an independent zlib stream).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    /// Absolute offset of the zlib header within the archive.
    pub offset: u64,
    /// Length of the zlib stream, excluding padding.
    pub compressed_len: u64,
    /// Inflated length: 65536 for all but the last page.
    pub decoded_len: u64,
}

/// What the builder placed for one entry.
#[derive(Clone, Debug)]
pub struct PlacedEntry {
    /// Position in the order entries were added.
    pub index: usize,
    pub kind: EntryKind,
    /// Absolute, 16-byte-aligned start of the entry (for virtual resources,
    /// the start of the 16 zero bytes).
    pub offset: u64,
    /// Absolute offset of the first zlib byte or first stored byte.
    pub data_offset: u64,
    /// One past the last byte written for the entry (padding excluded).
    pub end: u64,
    /// Zlib pages in file order; empty for stored kinds.
    pub pages: Vec<Page>,
    /// Length of [`PlacedEntry::decoded`].
    pub decoded_len: u64,
    /// `u_rome_core::detect` on the decoded bytes.
    pub asset_kind: AssetKind,
    /// The decoded entry: inflated (and concatenated) pages, or the stored bytes.
    pub decoded: Vec<u8>,
    /// Manifest of the nested archive, with offsets relative to its own start.
    pub nested: Option<Box<Rpf9Manifest>>,
}

/// Everything [`Rpf9Builder::build`] placed, for assertions.
#[derive(Clone, Debug)]
pub struct Rpf9Manifest {
    pub archive_id: u64,
    pub key_id: u32,
    pub entry_count: u32,
    pub payload_start: u64,
    pub table_split: u64,
    pub trailer_offset: u64,
    pub trailer_len: u64,
    pub total_len: u64,
    pub entries: Vec<PlacedEntry>,
}

impl Rpf9Manifest {
    /// Absolute offsets of every zlib stream visible in the archive bytes:
    /// this archive's pages plus, recursively, those of nested archives that
    /// are stored raw (pages inside a compressed nested archive are not
    /// visible). Sorted ascending.
    pub fn visible_streams(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect_streams(0, &mut out);
        out.sort_unstable();
        out
    }

    fn collect_streams(&self, base: u64, out: &mut Vec<u64>) {
        for e in &self.entries {
            out.extend(e.pages.iter().map(|p| base + p.offset));
            if let (EntryKind::NestedStored, Some(n)) = (e.kind, &e.nested) {
                n.collect_streams(base + e.data_offset, out);
            }
        }
    }

    /// Absolute offsets and kinds of every stored blob visible in the archive
    /// bytes, including nested stored archives themselves and, recursively,
    /// their own stored entries. Sorted by offset.
    pub fn visible_blobs(&self) -> Vec<(u64, AssetKind)> {
        let mut out = Vec::new();
        self.collect_blobs(0, &mut out);
        out.sort_unstable_by_key(|b| b.0);
        out
    }

    fn collect_blobs(&self, base: u64, out: &mut Vec<(u64, AssetKind)>) {
        for e in &self.entries {
            if matches!(e.kind, EntryKind::Stored | EntryKind::NestedStored) {
                out.push((base + e.data_offset, e.asset_kind));
            }
            if let (EntryKind::NestedStored, Some(n)) = (e.kind, &e.nested) {
                n.collect_blobs(base + e.data_offset, out);
            }
        }
    }
}

/// Output of [`Rpf9Builder::build`].
#[derive(Clone, Debug)]
pub struct BuiltRpf9 {
    pub bytes: Vec<u8>,
    pub manifest: Rpf9Manifest,
}

#[derive(Clone, Debug)]
enum Pending {
    Zlib(Vec<u8>),
    Virtual(Vec<u8>),
    Stored(Vec<u8>),
    Nested(Box<Rpf9Builder>, bool),
}

/// Builds a synthetic RPF9 archive. See the module docs for the layout.
///
/// ```
/// use u_rome_testdata::{EntryKind, Rpf9Builder};
///
/// let built = Rpf9Builder::new(1)
///     .zlib(b"<?xml version=\"1.0\"?><x/>".to_vec())
///     .stored(b"ADAT\x01\x00\x00\xff".to_vec())
///     .build();
/// assert_eq!(&built.bytes[..4], b"9FPR");
/// assert_eq!(built.manifest.entries[0].kind, EntryKind::Zlib);
/// assert_eq!(built.manifest.entries[1].data_offset % 16, 0);
/// ```
#[derive(Clone, Debug)]
pub struct Rpf9Builder {
    seed: u64,
    archive_id: u64,
    key_id: u32,
    level: u32,
    trailer_len: usize,
    table_a_len: Option<usize>,
    table_b_len: usize,
    entries: Vec<Pending>,
}

impl Rpf9Builder {
    /// A builder whose noise, archive id and key id derive from `seed`.
    pub fn new(seed: u64) -> Self {
        let mut p = Prng::new(seed ^ 0x5250_4639);
        // Keep the low byte of the id clear of signature bytes so the header
        // field at 0x10 never looks like a zlib header to an aligned scan.
        let archive_id = (p.next_u64() & !0xFF) | 0x11;
        let key_id = p.next_u32();
        Self {
            seed,
            archive_id,
            key_id,
            level: 6,
            trailer_len: 0x40,
            table_a_len: None,
            table_b_len: 0x80,
            entries: Vec::new(),
        }
    }

    /// Override the archive id at 0x10. Choose a low byte for which
    /// [`crate::can_start_signature`] is false to keep the aligned-scan
    /// guarantee.
    pub fn archive_id(mut self, id: u64) -> Self {
        self.archive_id = id;
        self
    }

    /// Override the key id at 0x28.
    pub fn key_id(mut self, id: u32) -> Self {
        self.key_id = id;
        self
    }

    /// zlib level (0-9) for every compressed entry. 6 (default) yields `78 9C`
    /// headers, 1 yields `78 01`, 9 yields `78 DA`.
    pub fn level(mut self, level: u32) -> Self {
        self.level = level.min(9);
        self
    }

    /// Trailer length in bytes (default 0x40; 0 allowed).
    pub fn trailer_len(mut self, len: usize) -> Self {
        self.trailer_len = len;
        self
    }

    /// Front table sizes (each rounded up to 16). Default: table A is 16 bytes
    /// per entry (min 16), table B is 0x80.
    pub fn front_tables(mut self, table_a: usize, table_b: usize) -> Self {
        self.table_a_len = Some(table_a);
        self.table_b_len = table_b;
        self
    }

    /// Add a zlib entry (paged when larger than 64 KiB).
    pub fn zlib(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.entries.push(Pending::Zlib(data.into()));
        self
    }

    /// Add a virtual resource: 16 zero bytes, then zlib at +0x10.
    pub fn virtual_resource(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.entries.push(Pending::Virtual(data.into()));
        self
    }

    /// Add a stored (uncompressed) blob.
    pub fn stored(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.entries.push(Pending::Stored(data.into()));
        self
    }

    /// Add a nested archive stored raw.
    pub fn nested(mut self, inner: Rpf9Builder) -> Self {
        self.entries.push(Pending::Nested(Box::new(inner), false));
        self
    }

    /// Add a nested archive inside a zlib stream.
    pub fn nested_zlib(mut self, inner: Rpf9Builder) -> Self {
        self.entries.push(Pending::Nested(Box::new(inner), true));
        self
    }

    /// Number of top-level entries added so far.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// A representative archive exercising every entry kind:
    ///
    /// 0. zlib XML (small, single page)
    /// 1. zlib `RDS5` store of about 150 KiB (three pages)
    /// 2. virtual resource holding a `PSIN` document
    /// 3. stored `ADAT` bank
    /// 4. stored `KB2j` video stub
    /// 5. nested archive, stored, with two zlib entries
    /// 6. nested archive inside zlib, with a stored `ADAT` and a zlib entry
    pub fn sample(seed: u64) -> Self {
        let mut p = Prng::new(seed);
        let mut xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<CMapData>\n".to_vec();
        xml.extend(p.text(3000));
        xml.extend_from_slice(b"\n</CMapData>\n");

        let rds = crate::Rds5Builder::new(seed.wrapping_add(1))
            .noise_chunk(0x1111, 70_000)
            .noise_chunk(0x2222, 50_000)
            .noise_chunk(0x3333, 30_000)
            .build();
        let psin = crate::PsinBuilder::standard(seed.wrapping_add(2)).build();
        let adat = crate::AdatBuilder::new(seed.wrapping_add(3))
            .stream(crate::AdatStream::new(0x0ABC_DEF1, 4))
            .build();
        let kb2 = crate::kb2_stub(seed.wrapping_add(4), 1280, 720, 24, 6000);

        let inner_a = Rpf9Builder::new(seed.wrapping_add(5))
            .zlib(p.text(2000))
            .zlib(
                crate::PsinBuilder::standard(seed.wrapping_add(6))
                    .build()
                    .bytes,
            );
        let inner_adat = crate::AdatBuilder::new(seed.wrapping_add(7))
            .stream(crate::AdatStream::new(0x0123_4567, 2))
            .build();
        let inner_b = Rpf9Builder::new(seed.wrapping_add(8))
            .stored(inner_adat.bytes)
            .zlib(p.text(500));

        Rpf9Builder::new(seed)
            .zlib(xml)
            .zlib(rds.bytes)
            .virtual_resource(psin.bytes)
            .stored(adat.bytes)
            .stored(kb2)
            .nested(inner_a)
            .nested_zlib(inner_b)
    }

    /// Lay out the archive. Deterministic: the same builder always yields the
    /// same bytes.
    pub fn build(&self) -> BuiltRpf9 {
        let mut noise = Prng::new(self.seed);
        let table_a = align_up(
            self.table_a_len
                .unwrap_or(16 * self.entries.len())
                .max(ALIGN),
            ALIGN,
        );
        let table_b = align_up(self.table_b_len, ALIGN);

        let mut out = vec![0u8; HEADER_LEN];
        noise.push_noise(&mut out, table_a);
        let table_split = out.len();
        noise.push_noise(&mut out, table_b);
        let payload_start = out.len();

        let mut placed = Vec::with_capacity(self.entries.len());
        for (index, pending) in self.entries.iter().enumerate() {
            pad_to_align(&mut out);
            let offset = out.len() as u64;
            let (kind, data_offset, pages, decoded, nested) = match pending {
                Pending::Zlib(data) => {
                    let pages = write_paged(&mut out, data, self.level);
                    (EntryKind::Zlib, offset, pages, data.clone(), None)
                }
                Pending::Virtual(data) => {
                    out.extend_from_slice(&[0u8; 16]);
                    let pages = write_paged(&mut out, data, self.level);
                    (EntryKind::Virtual, offset + 16, pages, data.clone(), None)
                }
                Pending::Stored(data) => {
                    out.extend_from_slice(data);
                    (EntryKind::Stored, offset, Vec::new(), data.clone(), None)
                }
                Pending::Nested(inner, compressed) => {
                    let built = inner.build();
                    let (kind, pages) = if *compressed {
                        let pages = write_paged(&mut out, &built.bytes, self.level);
                        (EntryKind::NestedZlib, pages)
                    } else {
                        out.extend_from_slice(&built.bytes);
                        (EntryKind::NestedStored, Vec::new())
                    };
                    (
                        kind,
                        offset,
                        pages,
                        built.bytes,
                        Some(Box::new(built.manifest)),
                    )
                }
            };
            placed.push(PlacedEntry {
                index,
                kind,
                offset,
                data_offset,
                end: out.len() as u64,
                pages,
                decoded_len: decoded.len() as u64,
                asset_kind: detect(&decoded),
                decoded,
                nested,
            });
        }

        pad_to_align(&mut out);
        let trailer_offset = out.len();
        noise.push_noise(&mut out, self.trailer_len);

        let entry_count = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&to_u32(payload_start).to_le_bytes());
        header.extend_from_slice(&to_u32(table_split).to_le_bytes());
        header.extend_from_slice(&to_u32(self.trailer_len).to_le_bytes());
        header.extend_from_slice(&self.archive_id.to_le_bytes());
        for v in [
            GENERATION,
            HEADER_0X1C,
            VERSION,
            entry_count,
            self.key_id,
            FLAGS,
            HEADER_0X30,
        ] {
            header.extend_from_slice(&v.to_le_bytes());
        }
        out[..header.len()].copy_from_slice(&header);

        let manifest = Rpf9Manifest {
            archive_id: self.archive_id,
            key_id: self.key_id,
            entry_count,
            payload_start: payload_start as u64,
            table_split: table_split as u64,
            trailer_offset: trailer_offset as u64,
            trailer_len: self.trailer_len as u64,
            total_len: out.len() as u64,
            entries: placed,
        };
        BuiltRpf9 {
            bytes: out,
            manifest,
        }
    }
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Write `data` as 64 KiB zlib pages starting at `out.len()` (which must be
/// 16-byte aligned). Empty data yields one page that inflates to nothing.
fn write_paged(out: &mut Vec<u8>, data: &[u8], level: u32) -> Vec<Page> {
    let mut pages = Vec::new();
    let mut chunks: Vec<&[u8]> = data.chunks(PAGE_SIZE).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    for (i, chunk) in chunks.into_iter().enumerate() {
        if i > 0 {
            pad_to_align(out);
        }
        let offset = out.len() as u64;
        let z = zlib_compress(chunk, level);
        out.extend_from_slice(&z);
        pages.push(Page {
            offset,
            compressed_len: z.len() as u64,
            decoded_len: chunk.len() as u64,
        });
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{inflate_prefix, is_zlib_header};
    use u_rome_core::Reader;

    /// A minimal aligned scanner, the way a carving reader would walk the
    /// payload: inflate at zlib headers and skip the stream, note magics.
    fn scan(bytes: &[u8], start: usize) -> (Vec<u64>, Vec<(u64, AssetKind)>) {
        let (mut streams, mut blobs) = (Vec::new(), Vec::new());
        let mut off = start;
        while off < bytes.len() {
            let rest = &bytes[off..];
            if is_zlib_header(rest)
                && let Some((_, used)) = inflate_prefix(rest, 1 << 24)
            {
                streams.push(off as u64);
                off = align_up(off + used, ALIGN);
                continue;
            }
            let kind = detect(rest);
            if kind != AssetKind::Unknown {
                blobs.push((off as u64, kind));
            }
            off += ALIGN;
        }
        (streams, blobs)
    }

    fn check_archive(bytes: &[u8], m: &Rpf9Manifest) {
        let r = Reader::new(bytes);
        assert_eq!(r.bytes_at(0, 4).unwrap(), b"9FPR");
        assert_eq!(r.u32_at(0x04).unwrap() as u64, m.payload_start);
        assert_eq!(r.u32_at(0x08).unwrap() as u64, m.table_split);
        assert_eq!(r.u32_at(0x0C).unwrap() as u64, m.trailer_len);
        assert_eq!(r.u64_at(0x10).unwrap(), m.archive_id);
        assert_eq!(r.u32_at(0x18).unwrap(), GENERATION);
        assert_eq!(r.u32_at(0x1C).unwrap(), 0x150);
        assert_eq!(r.u32_at(0x20).unwrap(), VERSION);
        assert_eq!(r.u32_at(0x24).unwrap() as usize, m.entries.len());
        assert_eq!(r.u32_at(0x28).unwrap(), m.key_id);
        assert_eq!(r.u32_at(0x2C).unwrap(), FLAGS);
        assert_eq!(r.u32_at(0x30).unwrap(), 0x10);
        assert!(bytes[0x34..0x40].iter().all(|&b| b == 0));
        assert_eq!(bytes.len() as u64, m.total_len);
        assert_eq!(m.trailer_offset + m.trailer_len, m.total_len);
        assert_eq!(m.trailer_offset % 16, 0);
        assert_eq!(m.payload_start % 16, 0);
        assert!(m.table_split > 0x40 && m.table_split <= m.payload_start);

        let mut prev_end = m.payload_start;
        for e in &m.entries {
            assert_eq!(e.offset % 16, 0, "entry {} aligned", e.index);
            assert!(e.offset >= prev_end);
            // Gap before the entry is zero padding shorter than 16 bytes.
            assert!(e.offset - prev_end < 16);
            assert!(
                bytes[prev_end as usize..e.offset as usize]
                    .iter()
                    .all(|&b| b == 0)
            );
            prev_end = e.end;

            match e.kind {
                EntryKind::Virtual => {
                    assert_eq!(e.data_offset, e.offset + 16);
                    let lead = &bytes[e.offset as usize..e.data_offset as usize];
                    assert!(lead.iter().all(|&b| b == 0));
                }
                _ => assert_eq!(e.data_offset, e.offset),
            }

            if e.kind.is_compressed() {
                assert!(!e.pages.is_empty());
                assert_eq!(e.pages[0].offset, e.data_offset);
                let mut joined = Vec::new();
                for (i, p) in e.pages.iter().enumerate() {
                    assert_eq!(p.offset % 16, 0);
                    let (dec, used) = inflate_prefix(&bytes[p.offset as usize..], 1 << 24)
                        .expect("page inflates");
                    assert_eq!(used as u64, p.compressed_len);
                    assert_eq!(dec.len() as u64, p.decoded_len);
                    let last = i + 1 == e.pages.len();
                    if !last {
                        assert_eq!(dec.len(), PAGE_SIZE);
                        let end = p.offset + p.compressed_len;
                        let next = e.pages[i + 1].offset;
                        assert_eq!(next, align_up(end as usize, 16) as u64);
                        assert!(bytes[end as usize..next as usize].iter().all(|&b| b == 0));
                    } else {
                        assert!(dec.len() <= PAGE_SIZE);
                        assert_eq!(p.offset + p.compressed_len, e.end);
                    }
                    joined.extend(dec);
                }
                assert_eq!(joined, e.decoded);
            } else {
                assert!(e.pages.is_empty());
                assert_eq!(
                    &bytes[e.data_offset as usize..e.end as usize],
                    &e.decoded[..]
                );
            }
            assert_eq!(e.decoded_len as usize, e.decoded.len());
            assert_eq!(e.asset_kind, detect(&e.decoded));

            if let Some(n) = &e.nested {
                assert!(matches!(
                    e.kind,
                    EntryKind::NestedStored | EntryKind::NestedZlib
                ));
                check_archive(&e.decoded, n);
            }
        }
        assert!(m.trailer_offset >= prev_end && m.trailer_offset - prev_end < 16);
    }

    #[test]
    fn sample_invariants() {
        let built = Rpf9Builder::sample(42).build();
        check_archive(&built.bytes, &built.manifest);

        let kinds: Vec<_> = built.manifest.entries.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [
                EntryKind::Zlib,
                EntryKind::Zlib,
                EntryKind::Virtual,
                EntryKind::Stored,
                EntryKind::Stored,
                EntryKind::NestedStored,
                EntryKind::NestedZlib,
            ]
        );
        let assets: Vec<_> = built
            .manifest
            .entries
            .iter()
            .map(|e| e.asset_kind)
            .collect();
        assert_eq!(
            assets,
            [
                AssetKind::Xml,
                AssetKind::TextureStore,
                AssetKind::Pso,
                AssetKind::Audio,
                AssetKind::Video,
                AssetKind::Archive,
                AssetKind::Archive,
            ]
        );
        assert_eq!(built.manifest.entries[1].pages.len(), 3);
    }

    #[test]
    fn aligned_scan_finds_exactly_the_manifest() {
        for seed in 0..8 {
            let built = Rpf9Builder::sample(seed).build();
            let m = &built.manifest;
            let (streams, blobs) = scan(&built.bytes, m.payload_start as usize);
            assert_eq!(streams, m.visible_streams(), "seed {seed}");
            assert_eq!(blobs, m.visible_blobs(), "seed {seed}");
        }
    }

    #[test]
    fn deterministic() {
        let a = Rpf9Builder::sample(9).build();
        let b = Rpf9Builder::sample(9).build();
        assert_eq!(a.bytes, b.bytes);
        assert_ne!(a.bytes, Rpf9Builder::sample(10).build().bytes);
    }

    #[test]
    fn paging_boundaries() {
        for len in [0, 1, PAGE_SIZE - 1, PAGE_SIZE, PAGE_SIZE + 1, 3 * PAGE_SIZE] {
            let data = Prng::new(len as u64).text(len);
            let built = Rpf9Builder::new(1).zlib(data.clone()).build();
            check_archive(&built.bytes, &built.manifest);
            let e = &built.manifest.entries[0];
            assert_eq!(e.pages.len(), len.div_ceil(PAGE_SIZE).max(1), "len {len}");
            assert_eq!(e.decoded, data);
        }
    }

    #[test]
    fn levels_change_headers() {
        let data = Prng::new(1).text(1000);
        for (level, flg) in [(1u32, 0x01u8), (6, 0x9C), (9, 0xDA)] {
            let built = Rpf9Builder::new(1).level(level).zlib(data.clone()).build();
            let off = built.manifest.entries[0].data_offset as usize;
            assert_eq!(&built.bytes[off..off + 2], &[0x78, flg]);
        }
    }

    #[test]
    fn empty_archive_and_options() {
        let built = Rpf9Builder::new(5)
            .trailer_len(0)
            .front_tables(0, 0)
            .key_id(0xDEAD_BEEF)
            .archive_id(0x1234_5611)
            .build();
        check_archive(&built.bytes, &built.manifest);
        assert_eq!(built.manifest.payload_start, 0x50);
        assert_eq!(built.manifest.total_len, 0x50);
        assert_eq!(built.manifest.key_id, 0xDEAD_BEEF);
        assert!(is_zlib_header(&[0x78, 0x9C]));
    }

    #[test]
    fn deep_nesting() {
        let inner = Rpf9Builder::new(3).zlib(b"leaf".to_vec());
        let mid = Rpf9Builder::new(2).nested_zlib(inner.clone()).nested(inner);
        let built = Rpf9Builder::new(1)
            .nested(mid.clone())
            .nested_zlib(mid)
            .build();
        check_archive(&built.bytes, &built.manifest);
        let (streams, blobs) = scan(&built.bytes, built.manifest.payload_start as usize);
        assert_eq!(streams, built.manifest.visible_streams());
        assert_eq!(blobs, built.manifest.visible_blobs());
    }
}
