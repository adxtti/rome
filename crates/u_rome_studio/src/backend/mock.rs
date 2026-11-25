//! Synthetic install so the whole Studio UI works without game files.
//!
//! The tree, names, sizes and bytes are generated deterministically from a
//! seed. Entry bytes are synthesised on demand (no allocation), each starting
//! with the signature `u_rome_core::detect` recognises for its kind, so the
//! detection, Hex and Preview paths are exercised exactly as with real data.
//! Nothing here reflects real GTA VI file names or layouts.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use u_rome_core::{AssetKind, Hash32, Reader, joaat_lower};

use super::{
    Backend, BackendError, BackendProvider, BackendStats, ByteSource, Codec, EntryDetails, EntryId,
    EntryInfo, HeaderField, location_chain, read_vec,
};
use crate::jobs::JobContext;
use crate::util::mix64;

/// Mock generation parameters.
#[derive(Clone, Debug)]
pub struct MockConfig {
    pub seed: u64,
    /// Approximate total number of entries (all depths).
    pub target_entries: usize,
    /// Adds one 160 MiB video entry to exercise the virtualised Hex view.
    pub huge_entry: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            seed: 0x524F_4D45,
            target_entries: 120_000,
            huge_entry: true,
        }
    }
}

/// Size of the guaranteed large entry.
pub const HUGE_ENTRY_SIZE: u64 = 160 * 1024 * 1024 + 123;
/// Marker written at offset 0x1000 of every entry larger than 8 KiB, handy
/// for trying the Hex pattern search.
pub const SEARCH_MARKER: &[u8; 8] = b"UROMMOCK";
const MARKER_OFFSET: u64 = 0x1000;
const HEADER_LEN: u64 = 0x40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Theme {
    Common,
    Audio,
    Video,
    Level,
    Textures,
    Models,
    Data,
}

const LAYOUT: &[(&str, Theme, u32)] = &[
    ("common.rpf", Theme::Common, 3),
    ("update/update.rpf", Theme::Common, 4),
    ("x64/audio/sfx/weapons.rpf", Theme::Audio, 2),
    ("x64/audio/sfx/vehicles.rpf", Theme::Audio, 2),
    ("x64/audio/sfx/ambience.rpf", Theme::Audio, 2),
    ("x64/audio/dialogue.rpf", Theme::Audio, 3),
    ("x64/data/data.rpf", Theme::Data, 3),
    ("x64/data/shaders.rpf", Theme::Data, 1),
    ("x64/levels/mock_city/city_metadata.rpf", Theme::Level, 4),
    ("x64/levels/mock_city/city_downtown.rpf", Theme::Level, 6),
    ("x64/levels/mock_city/city_beach.rpf", Theme::Level, 6),
    ("x64/levels/mock_city/city_port.rpf", Theme::Level, 5),
    ("x64/levels/mock_city/city_wetlands.rpf", Theme::Level, 5),
    ("x64/levels/mock_city/city_keys.rpf", Theme::Level, 4),
    ("x64/levels/mock_city/terrain_00.rpf", Theme::Level, 3),
    ("x64/levels/mock_city/terrain_01.rpf", Theme::Level, 3),
    ("x64/models/peds.rpf", Theme::Models, 5),
    ("x64/models/vehicles.rpf", Theme::Models, 5),
    ("x64/models/props.rpf", Theme::Models, 6),
    ("x64/models/weapons.rpf", Theme::Models, 2),
    ("x64/textures/ui.rpf", Theme::Textures, 3),
    ("x64/textures/fx.rpf", Theme::Textures, 2),
    ("x64/movies/movies.rpf", Theme::Video, 1),
];

fn kind_weights(theme: Theme) -> &'static [(AssetKind, u32)] {
    use AssetKind::*;
    match theme {
        Theme::Common => &[
            (Xml, 10),
            (Pso, 20),
            (MetaResource, 10),
            (TextureTable, 10),
            (TextureStore, 10),
            (MeshBlob, 10),
            (Unknown, 15),
            (Archive, 2),
        ],
        Theme::Audio => &[(Audio, 90), (Pso, 5), (Unknown, 5)],
        Theme::Video => &[(Video, 95), (Xml, 5)],
        Theme::Level => &[
            (MetaResource, 25),
            (MeshBlob, 30),
            (TextureTable, 12),
            (TextureStore, 12),
            (Pso, 10),
            (Unknown, 8),
            (Archive, 3),
        ],
        Theme::Textures => &[(TextureTable, 45), (TextureStore, 50), (Unknown, 5)],
        Theme::Models => &[
            (MeshBlob, 45),
            (TextureTable, 15),
            (TextureStore, 15),
            (MetaResource, 10),
            (Pso, 5),
            (Unknown, 5),
            (Archive, 3),
        ],
        Theme::Data => &[(Xml, 40), (Pso, 40), (MetaResource, 10), (Unknown, 10)],
    }
}

fn size_range(kind: AssetKind) -> (u64, u64) {
    use AssetKind::*;
    match kind {
        Archive => (0, 0),
        TextureStore => (64 << 10, 32 << 20),
        TextureTable => (1 << 10, 64 << 10),
        Pso => (512, 256 << 10),
        MetaResource => (1 << 10, 2 << 20),
        Audio => (16 << 10, 24 << 20),
        Video => (4 << 20, 96 << 20),
        MeshBlob => (4 << 10, 8 << 20),
        Xml => (256, 128 << 10),
        Unknown => (16, 1 << 20),
    }
}

fn extension(kind: AssetKind) -> &'static str {
    use AssetKind::*;
    match kind {
        Archive => ".rpf",
        TextureStore => ".texstore",
        TextureTable => ".textable",
        Pso => ".pso",
        MetaResource => ".meta",
        Audio => ".awc",
        Video => ".bk2",
        MeshBlob => ".mesh",
        Xml => ".xml",
        Unknown => ".bin",
    }
}

const WORDS_A: &[&str] = &[
    "bldg", "road", "prop", "foliage", "water", "light", "dock", "pier", "palm", "car", "boat",
    "sign", "fence", "rail", "hotel", "mall", "bridge", "tower", "shack", "crate",
];
const WORDS_B: &[&str] = &[
    "downtown", "beach", "port", "wetlands", "keys", "airport", "harbor", "strip", "canal",
    "suburb", "highway", "marina",
];

/// Tiny deterministic RNG (SplitMix64 stream).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix64(self.0)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn log_uniform(&mut self, lo: u64, hi: u64) -> u64 {
        let lo = lo.max(1) as f64;
        let hi = (hi.max(1) as f64).max(lo);
        let t = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        (lo * (hi / lo).powf(t)) as u64
    }

    fn pick_kind(&mut self, weights: &[(AssetKind, u32)]) -> AssetKind {
        let total: u64 = weights.iter().map(|w| u64::from(w.1)).sum();
        let mut r = self.below(total);
        for (k, w) in weights {
            if r < u64::from(*w) {
                return *k;
            }
            r -= u64::from(*w);
        }
        AssetKind::Unknown
    }
}

struct Node {
    info: EntryInfo,
    children: Vec<EntryId>,
    seed: u64,
}

/// The mock backend.
pub struct MockBackend {
    nodes: Vec<Node>,
    roots: Vec<EntryId>,
    root_path: Option<PathBuf>,
    stats: BackendStats,
}

struct Builder<'a> {
    rng: Rng,
    nodes: Vec<Node>,
    budget: usize,
    ctx: &'a JobContext,
    target: usize,
}

impl Builder<'_> {
    fn push(&mut self, info: EntryInfo, seed: u64) -> EntryId {
        let id = self.nodes.len() as EntryId;
        self.nodes.push(Node {
            info: EntryInfo { id, ..info },
            children: Vec::new(),
            seed,
        });
        id
    }

    /// Fills container `id` with `count` children; returns its total size.
    fn fill(
        &mut self,
        id: EntryId,
        theme: Theme,
        count: usize,
        depth: usize,
        prefix: &str,
    ) -> Result<u64, BackendError> {
        let mut offset: u64 = 0x1000; // header + TOC
        let mut children = Vec::with_capacity(count);
        for i in 0..count {
            if self.budget == 0 {
                break;
            }
            if self.nodes.len().is_multiple_of(2048) {
                if self.ctx.is_cancelled() {
                    return Err(BackendError::Cancelled);
                }
                self.ctx
                    .set_progress(self.nodes.len() as u64, self.target as u64);
            }
            self.budget -= 1;
            let mut kind = self.rng.pick_kind(kind_weights(theme));
            if kind == AssetKind::Archive && depth >= 2 {
                kind = AssetKind::MetaResource;
            }
            let a = WORDS_A[self.rng.below(WORDS_A.len() as u64) as usize];
            let b = WORDS_B[self.rng.below(WORDS_B.len() as u64) as usize];
            let base = format!("{prefix}{a}_{b}_{i:04}{}", extension(kind));
            let hash = joaat_lower(&base);
            let named = kind == AssetKind::Archive || kind == AssetKind::Xml || self.rng.chance(70);
            let codec = match kind {
                AssetKind::Archive | AssetKind::Video | AssetKind::Audio => Codec::Stored,
                _ => match self.rng.below(10) {
                    0..=1 => Codec::Stored,
                    2..=3 => Codec::Deflate,
                    4..=5 => Codec::Zstd,
                    _ => Codec::Oodle,
                },
            };
            let provisional = matches!(kind, AssetKind::MeshBlob | AssetKind::MetaResource)
                && self.rng.chance(15);
            let seed = self.rng.next();
            let child = self.push(
                EntryInfo {
                    id: 0,
                    parent: Some(id),
                    name: named.then_some(base),
                    name_hash: Some(hash),
                    kind,
                    codec: codec.clone(),
                    offset,
                    stored_size: 0,
                    size: 0,
                    child_count: None,
                    provisional,
                },
                seed,
            );
            let size = if kind == AssetKind::Archive {
                let n = self.rng.below(280) as usize + 20;
                self.fill(child, theme, n, depth + 1, "")?
            } else {
                let (lo, hi) = size_range(kind);
                self.rng.log_uniform(lo, hi).max(HEADER_LEN)
            };
            let stored = match codec {
                Codec::Stored => size,
                _ => (size * (35 + self.rng.below(50)) / 100).max(16),
            };
            if let Some(n) = self.nodes.get_mut(child as usize) {
                n.info.size = size;
                n.info.stored_size = stored;
            }
            offset = (offset + stored).next_multiple_of(0x200);
            children.push(child);
        }
        let total = offset;
        if let Some(n) = self.nodes.get_mut(id as usize) {
            n.info.child_count = Some(children.len() as u32);
            n.info.size = total;
            n.info.stored_size = total;
            n.children = children;
        }
        Ok(total)
    }
}

impl MockBackend {
    /// Generates the synthetic install (cancellable, reports progress).
    pub fn generate(
        config: &MockConfig,
        root_path: Option<PathBuf>,
        ctx: &JobContext,
    ) -> Result<Self, BackendError> {
        let target = config.target_entries.clamp(LAYOUT.len() * 2, 5_000_000);
        let mut b = Builder {
            rng: Rng(config.seed),
            nodes: Vec::with_capacity(target + LAYOUT.len() + 1),
            budget: target,
            ctx,
            target,
        };
        let weight_sum: u64 = LAYOUT.iter().map(|l| u64::from(l.2)).sum();
        let mut roots = Vec::new();
        for (path, theme, weight) in LAYOUT {
            let id = b.push(
                EntryInfo {
                    id: 0,
                    parent: None,
                    name: Some((*path).to_owned()),
                    name_hash: Some(joaat_lower(path)),
                    kind: AssetKind::Archive,
                    codec: Codec::Stored,
                    offset: 0,
                    stored_size: 0,
                    size: 0,
                    child_count: None,
                    provisional: false,
                },
                mix64(config.seed ^ u64::from(joaat_lower(path).0)),
            );
            roots.push(id);
            // Each root gets its own budget so nested archives in early roots
            // cannot starve later ones.
            let share = (target as u64 * u64::from(*weight) / weight_sum.max(1)) as usize;
            b.budget = share.max(1);
            b.fill(id, *theme, share.max(1), 0, "")?;
        }
        if config.huge_entry
            && let Some(&movies) = roots.last()
        {
            let huge = b.push(
                EntryInfo {
                    id: 0,
                    parent: Some(movies),
                    name: Some("intro_cinematic.bk2".into()),
                    name_hash: Some(joaat_lower("intro_cinematic.bk2")),
                    kind: AssetKind::Video,
                    codec: Codec::Stored,
                    offset: 0,
                    stored_size: HUGE_ENTRY_SIZE,
                    size: HUGE_ENTRY_SIZE,
                    child_count: None,
                    provisional: false,
                },
                0xB16_F11E,
            );
            if let Some(m) = b.nodes.get_mut(movies as usize) {
                let off = m.info.size.next_multiple_of(0x200);
                m.info.size = off + HUGE_ENTRY_SIZE;
                m.info.stored_size = m.info.size;
                m.info.child_count = Some(m.children.len() as u32 + 1);
                m.children.insert(0, huge);
                if let Some(h) = b.nodes.get_mut(huge as usize) {
                    h.info.offset = off;
                }
            }
        }
        ctx.set_fraction(1.0);
        let nodes = b.nodes;
        let stats = BackendStats {
            archives: nodes
                .iter()
                .filter(|n| n.info.kind == AssetKind::Archive)
                .count() as u64,
            entries: nodes.len() as u64,
            total_bytes: roots
                .iter()
                .filter_map(|r| nodes.get(*r as usize))
                .map(|n| n.info.size)
                .sum(),
        };
        log::info!(
            "mock install generated: {} entries, {} archives",
            stats.entries,
            stats.archives
        );
        Ok(Self {
            nodes,
            roots,
            root_path,
            stats,
        })
    }

    fn node(&self, id: EntryId) -> Result<&Node, BackendError> {
        usize::try_from(id)
            .ok()
            .and_then(|i| self.nodes.get(i))
            .ok_or(BackendError::NotFound(id))
    }

    fn source(&self, id: EntryId) -> Result<SynthSource, BackendError> {
        let n = self.node(id)?;
        Ok(SynthSource {
            kind: n.info.kind,
            seed: n.seed,
            len: n.info.size,
            count: n.info.child_count.unwrap_or(0),
            name_hash: n.info.name_hash.unwrap_or_default(),
        })
    }
}

impl Backend for MockBackend {
    fn label(&self) -> String {
        "Mock (synthetic data)".into()
    }

    fn root_path(&self) -> Option<PathBuf> {
        self.root_path.clone()
    }

    fn is_mock(&self) -> bool {
        true
    }

    fn stats(&self) -> BackendStats {
        self.stats.clone()
    }

    fn roots(&self) -> Result<Vec<EntryInfo>, BackendError> {
        self.roots
            .iter()
            .map(|id| self.node(*id).map(|n| n.info.clone()))
            .collect()
    }

    fn children(&self, id: EntryId) -> Result<Vec<EntryInfo>, BackendError> {
        let n = self.node(id)?;
        n.children
            .iter()
            .map(|c| self.node(*c).map(|n| n.info.clone()))
            .collect()
    }

    fn entry(&self, id: EntryId) -> Result<EntryInfo, BackendError> {
        self.node(id).map(|n| n.info.clone())
    }

    fn details(&self, id: EntryId) -> Result<EntryDetails, BackendError> {
        let chain = location_chain(self, id)?;
        let src = self.source(id)?;
        let head = read_vec(&src, 0, HEADER_LEN, HEADER_LEN as usize)?;
        let mut header = Vec::new();
        if src.kind == AssetKind::Xml {
            header.push(HeaderField::new("encoding", "UTF-8").at(0));
        } else {
            let r = Reader::new(&head);
            let magic = r.bytes_at(0, 4).map(printable).unwrap_or_default();
            header.push(HeaderField::new("magic", magic).at(0));
            let mut field =
                |name: &str, off: usize, v: Result<String, u_rome_core::ByteError>| match v {
                    Ok(v) => {
                        header.push(HeaderField::new(name, v).at(off as u64).provisional(true))
                    }
                    Err(e) => log::debug!("mock header field {name}: {e}"),
                };
            field("version", 4, r.u32_at(4).map(|v| v.to_string()));
            field("flags", 8, r.u32_at(8).map(|v| format!("{v:#010x}")));
            field("count", 12, r.u32_at(12).map(|v| v.to_string()));
            field("data_offset", 16, r.u32_at(16).map(|v| format!("{v:#x}")));
            field("name_hash", 20, r.u32_at(20).map(|v| Hash32(v).to_string()));
        }
        Ok(EntryDetails {
            chain,
            header,
            notes: vec![
                "Synthetic data from the mock backend; no game files are read.".into(),
                "Header layout is invented for the mock and marked provisional.".into(),
            ],
        })
    }

    fn open(&self, id: EntryId, ctx: &JobContext) -> Result<Arc<dyn ByteSource>, BackendError> {
        ctx.check()?;
        Ok(Arc::new(self.source(id)?))
    }
}

fn printable(b: &[u8]) -> String {
    let ascii: String = b
        .iter()
        .map(|&c| if c.is_ascii_graphic() { c as char } else { '.' })
        .collect();
    let hex: Vec<String> = b.iter().map(|c| format!("{c:02X}")).collect();
    format!("{ascii} ({})", hex.join(" "))
}

/// Deterministic on-demand bytes for one mock entry.
pub struct SynthSource {
    kind: AssetKind,
    seed: u64,
    len: u64,
    count: u32,
    name_hash: Hash32,
}

const XML_PROLOG: &[u8] = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<MockData>\n";
const XML_EPILOG: &[u8] = b"</MockData>\n";
const XML_LINE_LEN: u64 = 66;

impl SynthSource {
    fn magic(&self) -> [u8; 4] {
        use AssetKind::*;
        match self.kind {
            Archive => *b"9FPR",
            TextureStore => *b"RDS5",
            TextureTable => *b"ETPF",
            Pso => *b"PSIN",
            MetaResource => *b"0DRP",
            Audio => *b"ADAT",
            Video => *b"KB2j",
            MeshBlob => 0x4042u32.to_le_bytes(),
            Xml => *b"<?xm",
            Unknown => [0xFE, 0xED, (self.seed >> 8) as u8, self.seed as u8],
        }
    }

    fn header_byte(&self, off: u64) -> u8 {
        let le = |v: u32, i: u64| v.to_le_bytes()[(i % 4) as usize];
        match off {
            0..=3 => self.magic()[off as usize],
            4..=7 => le(1 + (self.seed % 4) as u32, off),
            8..=11 => le((self.seed >> 32) as u32 & 0x00FF_00FF, off),
            12..=15 => le(self.count.max((self.seed % 512) as u32), off),
            16..=19 => le(HEADER_LEN as u32, off),
            20..=23 => le(self.name_hash.0, off),
            _ => 0,
        }
    }

    fn xml_byte(&self, off: u64) -> u8 {
        let p = XML_PROLOG.len() as u64;
        let e = XML_EPILOG.len() as u64;
        if off < p {
            return XML_PROLOG[off as usize];
        }
        if self.len >= p + e && off >= self.len - e {
            return XML_EPILOG[(off - (self.len - e)) as usize];
        }
        let rel = off - p;
        let line = rel / XML_LINE_LEN;
        let col = (rel % XML_LINE_LEN) as usize;
        let h = mix64(self.seed ^ line) as u32;
        let text = format!(
            "  <Item name=\"item_{:06}\" hash=\"0x{h:08X}\" value=\"{:010}\" />\n",
            line % 1_000_000,
            h % 1_000_000_000
        );
        text.as_bytes().get(col).copied().unwrap_or(b' ')
    }

    fn body_byte(&self, off: u64) -> u8 {
        if (MARKER_OFFSET..MARKER_OFFSET + 8).contains(&off) && self.len > 8 * 1024 {
            return SEARCH_MARKER[(off - MARKER_OFFSET) as usize];
        }
        let block = off / 64;
        let r = mix64(self.seed ^ block.wrapping_mul(0x100_0000_01B3));
        match r % 8 {
            0 | 1 => 0,
            2 => {
                // Little-endian incrementing u32 table.
                let word = (off / 4) as u32;
                word.to_le_bytes()[(off % 4) as usize]
            }
            3 => {
                // f32 values in [-100, 100).
                let i = off / 4;
                let f = ((mix64(r ^ i) % 20_000) as f32 - 10_000.0) / 100.0;
                f.to_le_bytes()[(off % 4) as usize]
            }
            _ => mix64(r ^ (off / 8)).to_le_bytes()[(off % 8) as usize],
        }
    }

    fn byte(&self, off: u64) -> u8 {
        if self.kind == AssetKind::Xml {
            self.xml_byte(off)
        } else if off < HEADER_LEN {
            self.header_byte(off)
        } else {
            self.body_byte(off)
        }
    }
}

impl ByteSource for SynthSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, BackendError> {
        if offset >= self.len {
            return Ok(0);
        }
        let n = (self.len - offset).min(buf.len() as u64) as usize;
        for (i, b) in buf.iter_mut().take(n).enumerate() {
            *b = self.byte(offset + i as u64);
        }
        Ok(n)
    }
}

/// Provider that always succeeds with synthetic data.
#[derive(Default)]
pub struct MockProvider {
    pub config: MockConfig,
}

impl BackendProvider for MockProvider {
    fn name(&self) -> &str {
        "Mock"
    }

    fn accepts(&self, _root: Option<&Path>) -> bool {
        true
    }

    fn open(
        &self,
        root: Option<&Path>,
        ctx: &JobContext,
    ) -> Result<Arc<dyn Backend>, BackendError> {
        ctx.set_status("generating synthetic install");
        Ok(Arc::new(MockBackend::generate(
            &self.config,
            root.map(Path::to_path_buf),
            ctx,
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use u_rome_core::detect;

    fn small() -> MockBackend {
        MockBackend::generate(
            &MockConfig {
                seed: 7,
                target_entries: 3_000,
                huge_entry: true,
            },
            None,
            &JobContext::detached(),
        )
        .unwrap()
    }

    #[test]
    fn deterministic_and_sized() {
        let a = small();
        let b = small();
        assert_eq!(a.nodes.len(), b.nodes.len());
        assert_eq!(a.roots().unwrap(), b.roots().unwrap());
        // Roots + target (+ the huge entry), never more.
        assert!(a.nodes.len() <= LAYOUT.len() + 3_000 + 1);
        assert!(a.nodes.len() > 2_000);
        assert_eq!(a.roots().unwrap().len(), LAYOUT.len());
    }

    #[test]
    fn every_kind_present_and_detectable() {
        let m = small();
        let mut seen = std::collections::HashSet::new();
        let ctx = JobContext::detached();
        m.walk(&ctx, &mut |e| {
            if seen.insert(e.kind) {
                let src = m.open(e.id, &ctx).unwrap();
                let head = read_vec(src.as_ref(), 0, 16, 16).unwrap();
                assert_eq!(detect(&head), e.kind, "entry {:?}", e.display_name());
            }
        })
        .unwrap();
        for k in [
            AssetKind::Archive,
            AssetKind::TextureStore,
            AssetKind::TextureTable,
            AssetKind::Pso,
            AssetKind::MetaResource,
            AssetKind::Audio,
            AssetKind::Video,
            AssetKind::MeshBlob,
            AssetKind::Xml,
            AssetKind::Unknown,
        ] {
            assert!(seen.contains(&k), "missing {k:?}");
        }
    }

    #[test]
    fn tree_is_consistent() {
        let m = small();
        let ctx = JobContext::detached();
        let mut count = 0usize;
        let mut nested = 0usize;
        let mut unnamed = 0usize;
        m.walk(&ctx, &mut |e| {
            count += 1;
            if let Some(p) = e.parent {
                let parent = m.entry(p).unwrap();
                assert!(parent.is_container());
                assert!(e.offset + e.stored_size <= parent.size, "{e:?}");
                if parent.parent.is_some() {
                    nested += 1;
                }
            }
            if e.name.is_none() {
                unnamed += 1;
                assert!(e.display_name().contains('@'));
            }
        })
        .unwrap();
        assert_eq!(count, m.nodes.len());
        assert!(nested > 0, "expected nested archive entries");
        assert!(unnamed > 0, "expected entries without names");
    }

    #[test]
    fn huge_entry_and_marker() {
        let m = small();
        let movies = *m.roots.last().unwrap();
        let kids = m.children(movies).unwrap();
        let huge = &kids[0];
        assert_eq!(huge.size, HUGE_ENTRY_SIZE);
        let src = m.open(huge.id, &JobContext::detached()).unwrap();
        assert_eq!(src.len(), HUGE_ENTRY_SIZE);
        let tail = read_vec(src.as_ref(), HUGE_ENTRY_SIZE - 4, 100, 100).unwrap();
        assert_eq!(tail.len(), 4);
        let marker = read_vec(src.as_ref(), MARKER_OFFSET, 8, 8).unwrap();
        assert_eq!(&marker, SEARCH_MARKER);
    }

    #[test]
    fn details_and_errors() {
        let m = small();
        let ctx = JobContext::detached();
        let mut deep = None;
        m.walk(&ctx, &mut |e| {
            if deep.is_none() && e.kind == AssetKind::Pso {
                deep = Some(e.id);
            }
        })
        .unwrap();
        let d = m.details(deep.unwrap()).unwrap();
        assert!(d.chain.len() >= 2);
        assert!(d.chain[0].parent.is_none());
        assert_eq!(d.entry().unwrap().id, deep.unwrap());
        assert!(
            d.header
                .iter()
                .any(|f| f.name == "magic" && f.value.starts_with("PSIN"))
        );
        assert!(matches!(m.entry(u64::MAX), Err(BackendError::NotFound(_))));
        assert!(m.children(u64::MAX).is_err());
    }

    #[test]
    fn xml_is_text() {
        let s = SynthSource {
            kind: AssetKind::Xml,
            seed: 1,
            len: 1000,
            count: 0,
            name_hash: Hash32(0),
        };
        let v = read_vec(&s, 0, 1000, 1000).unwrap();
        let text = String::from_utf8(v).unwrap();
        assert!(text.starts_with("<?xml"));
        assert!(text.ends_with("</MockData>\n"));
        assert!(text.contains("<Item name=\"item_000000\""));
    }

    #[test]
    fn generation_cancels() {
        let ctx = JobContext::detached();
        ctx.cancel();
        let r = MockBackend::generate(&MockConfig::default(), None, &ctx);
        assert!(matches!(r, Err(BackendError::Cancelled)));
    }
}
