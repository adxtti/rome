//! Record types written to and read from the index.
//!
//! `IndexEntry` deliberately mirrors what `u_rome_rpf9` emits per found entry
//! (this crate does not depend on it), so the integrator maps one onto the
//! other field by field.

use u_rome_core::AssetKind;

/// Maximum nesting depth of a location chain. Real installs nest archives a
/// few levels deep at most; anything beyond this is treated as corrupt input.
pub const MAX_CHAIN_DEPTH: usize = 32;

/// Maximum number of leading decoded bytes kept per entry.
pub const MAX_HEAD_LEN: usize = 64;

/// Row id of an archive (a file on disk, relative to the install root).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchiveId(pub i64);

/// Row id of an entry found inside an archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(pub i64);

/// Row id of a recovered name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NameId(pub i64);

/// Compression used for an entry's stored bytes. Mirrors the rpf9 codec enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    Stored,
    Zlib,
    RawDeflate,
    /// A codec this build does not know; the raw code is preserved.
    Other(u8),
}

impl Codec {
    pub(crate) fn code(self) -> i64 {
        match self {
            Codec::Stored => 0,
            Codec::Zlib => 1,
            Codec::RawDeflate => 2,
            Codec::Other(n) => 256 + i64::from(n),
        }
    }

    pub(crate) fn from_code(code: i64) -> Option<Codec> {
        match code {
            0 => Some(Codec::Stored),
            1 => Some(Codec::Zlib),
            2 => Some(Codec::RawDeflate),
            256..=511 => u8::try_from(code - 256).ok().map(Codec::Other),
            _ => None,
        }
    }
}

/// Stable on-disk code for an [`AssetKind`].
///
/// The match is exhaustive on purpose: adding a variant to `AssetKind` must
/// come with a new, never-reused code here, otherwise existing indexes would
/// be misread.
pub fn kind_code(kind: AssetKind) -> i64 {
    match kind {
        AssetKind::Unknown => 0,
        AssetKind::Archive => 1,
        AssetKind::TextureStore => 2,
        AssetKind::TextureTable => 3,
        AssetKind::Pso => 4,
        AssetKind::MetaResource => 5,
        AssetKind::Audio => 6,
        AssetKind::Video => 7,
        AssetKind::MeshBlob => 8,
        AssetKind::Xml => 9,
    }
}

/// All kinds, in code order. Used for parsing and stats.
pub const ALL_KINDS: [AssetKind; 10] = [
    AssetKind::Unknown,
    AssetKind::Archive,
    AssetKind::TextureStore,
    AssetKind::TextureTable,
    AssetKind::Pso,
    AssetKind::MetaResource,
    AssetKind::Audio,
    AssetKind::Video,
    AssetKind::MeshBlob,
    AssetKind::Xml,
];

/// Inverse of [`kind_code`]. Unknown codes (from a newer build) read as `Unknown`.
pub fn kind_from_code(code: i64) -> AssetKind {
    ALL_KINDS
        .iter()
        .copied()
        .find(|k| kind_code(*k) == code)
        .unwrap_or_else(|| {
            log::warn!("index: unknown asset kind code {code}, reading as Unknown");
            AssetKind::Unknown
        })
}

/// Short machine name for a kind, accepted by search as `kind:<slug>`.
pub fn kind_slug(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Unknown => "unknown",
        AssetKind::Archive => "archive",
        AssetKind::TextureStore => "texture_store",
        AssetKind::TextureTable => "texture_table",
        AssetKind::Pso => "pso",
        AssetKind::MetaResource => "meta",
        AssetKind::Audio => "audio",
        AssetKind::Video => "video",
        AssetKind::MeshBlob => "mesh",
        AssetKind::Xml => "xml",
    }
}

/// Parse a kind from its slug or its human label (case-insensitive).
pub fn parse_kind(s: &str) -> Option<AssetKind> {
    let s = s.trim();
    ALL_KINDS
        .iter()
        .copied()
        .find(|k| kind_slug(*k).eq_ignore_ascii_case(s) || k.label().eq_ignore_ascii_case(s))
}

/// One entry found by a scan, as handed to [`crate::Writer::push`].
#[derive(Clone, Debug)]
pub struct IndexEntry {
    /// Location chain: `chain[0]` is the offset in the top-level archive,
    /// `chain[i]` the offset inside the decoded parent entry. Must be non-empty.
    /// The parent entry is resolved automatically from `chain[..len - 1]`.
    pub chain: Vec<u64>,
    pub stored_len: u64,
    pub size: u64,
    pub codec: Codec,
    pub pages: u32,
    pub kind: AssetKind,
    /// First bytes of the decoded entry; truncated to [`MAX_HEAD_LEN`].
    pub head: Vec<u8>,
    /// BLAKE3 of the decoded bytes, when the scanner computed it.
    pub hash: Option<[u8; 32]>,
    /// The scanner was unsure about this entry (e.g. heuristic boundary).
    pub provisional: bool,
    /// A name hash embedded in the entry or its parent's table, when known.
    /// [`crate::Index::attach_names_by_hash`] matches it against known names.
    pub name_hash: Option<u32>,
}

impl IndexEntry {
    /// An entry with only the location set; everything else empty/unknown.
    pub fn new(chain: Vec<u64>) -> Self {
        IndexEntry {
            chain,
            stored_len: 0,
            size: 0,
            codec: Codec::Stored,
            pages: 0,
            kind: AssetKind::Unknown,
            head: Vec::new(),
            hash: None,
            provisional: false,
            name_hash: None,
        }
    }
}

/// Per-archive scan progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanState {
    /// Never scanned, or invalidated because the file changed.
    Pending,
    /// A scan started; entries before `last_offset` are committed.
    Partial,
    /// The scan ran to completion.
    Done,
}

impl ScanState {
    pub(crate) fn code(self) -> i64 {
        match self {
            ScanState::Pending => 0,
            ScanState::Partial => 1,
            ScanState::Done => 2,
        }
    }

    pub(crate) fn from_code(code: i64) -> Option<ScanState> {
        match code {
            0 => Some(ScanState::Pending),
            1 => Some(ScanState::Partial),
            2 => Some(ScanState::Done),
            _ => None,
        }
    }
}

/// Result of [`crate::Index::register_archive`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registration {
    pub id: ArchiveId,
    /// True when the archive is new, changed on disk, or not fully scanned.
    pub needs_scan: bool,
    /// Top-level offset a scan should resume from (0 for a fresh scan).
    pub resume_offset: u64,
    /// True when an existing record was invalidated because size/mtime changed.
    pub changed: bool,
}

/// An archive row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveRow {
    pub id: ArchiveId,
    pub rel_path: String,
    pub size: u64,
    pub mtime: i64,
    pub header: Option<String>,
    pub state: ScanState,
    pub last_offset: u64,
    pub entry_count: u64,
}

/// How an entry's name was obtained.
///
/// Only `Explicit` is certain. Hash matches are provisional: community sources
/// disagree on whether GTA VI lowercases names before hashing (see
/// `u_rome_core::hash`), and 32-bit JOAAT collides across large name lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameMatch {
    /// Set directly via [`crate::Index::attach_name`].
    Explicit,
    /// `name_hash` equals the case-sensitive JOAAT of the name.
    Joaat,
    /// `name_hash` equals the JOAAT of the lowercased name (GTA V convention).
    JoaatLower,
    /// `name_hash` equals a hash given by an imported list that matched
    /// neither JOAAT variant (unverified, possibly another hash function).
    ListedHash,
}

impl NameMatch {
    pub(crate) fn code(self) -> i64 {
        match self {
            NameMatch::Explicit => 0,
            NameMatch::Joaat => 1,
            NameMatch::JoaatLower => 2,
            NameMatch::ListedHash => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Option<NameMatch> {
        match code {
            0 => Some(NameMatch::Explicit),
            1 => Some(NameMatch::Joaat),
            2 => Some(NameMatch::JoaatLower),
            3 => Some(NameMatch::ListedHash),
            _ => None,
        }
    }

    /// True for every match that is a hash guess rather than ground truth.
    pub fn provisional(self) -> bool {
        !matches!(self, NameMatch::Explicit)
    }
}

/// Where a name came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NameSource {
    /// A user-supplied hash/name list.
    HashList,
    /// Extracted from an XML payload (content manifests, setup data).
    Xml,
    /// Set by hand or by a decoder that read the name from the data.
    Manual,
    /// Derived from another name (basename, device prefix stripped).
    Derived,
}

impl NameSource {
    pub(crate) fn code(self) -> i64 {
        match self {
            NameSource::HashList => 0,
            NameSource::Xml => 1,
            NameSource::Manual => 2,
            NameSource::Derived => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> NameSource {
        match code {
            0 => NameSource::HashList,
            1 => NameSource::Xml,
            3 => NameSource::Derived,
            _ => NameSource::Manual,
        }
    }
}

/// An entry row as returned by queries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryRow {
    pub id: EntryId,
    pub archive_id: ArchiveId,
    pub parent_id: Option<EntryId>,
    pub chain: Vec<u64>,
    pub top_offset: u64,
    pub stored_len: u64,
    pub size: u64,
    pub codec: Codec,
    pub pages: u32,
    pub kind: AssetKind,
    pub head: Vec<u8>,
    pub hash: Option<[u8; 32]>,
    pub provisional: bool,
    pub name_hash: Option<u32>,
    pub name_id: Option<NameId>,
    pub name: Option<String>,
    pub name_match: Option<NameMatch>,
}

/// A name row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameRow {
    pub id: NameId,
    pub name: String,
    pub joaat: u32,
    pub joaat_lower: u32,
    /// Hash given by an imported list that matched neither JOAAT variant.
    pub listed_hash: Option<u32>,
    pub source: NameSource,
}

/// Filters applied on top of a search query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchFilters {
    pub kind: Option<AssetKind>,
    pub archive: Option<ArchiveId>,
}

/// Count and bytes for one asset kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindStats {
    pub kind: AssetKind,
    pub count: u64,
    /// Sum of decoded sizes.
    pub size: u64,
    /// Sum of stored (on-disk) lengths.
    pub stored_len: u64,
}

/// Content duplicated across entries, by BLAKE3 hash.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DuplicateStats {
    /// Distinct hashes that occur more than once.
    pub groups: u64,
    /// Entries beyond the first copy in each group.
    pub redundant_entries: u64,
    /// Decoded bytes those redundant copies occupy.
    pub redundant_bytes: u64,
}

/// One group of entries with identical content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub hash: [u8; 32],
    pub count: u64,
    pub size: u64,
}

/// Index-wide statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub archives: u64,
    pub archives_pending: u64,
    pub archives_partial: u64,
    pub archives_done: u64,
    pub entries: u64,
    pub named_entries: u64,
    pub provisional_entries: u64,
    pub names: u64,
    pub total_size: u64,
    pub total_stored_len: u64,
    /// Only kinds with at least one entry, in kind-code order.
    pub by_kind: Vec<KindStats>,
    pub duplicates: DuplicateStats,
}

/// Counts from [`crate::Index::attach_names_by_hash`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttachReport {
    pub joaat_lower: u64,
    pub joaat: u64,
    pub listed_hash: u64,
}

impl AttachReport {
    pub fn total(&self) -> u64 {
        self.joaat_lower + self.joaat + self.listed_hash
    }
}

/// Encode a location chain as concatenated little-endian u64s.
pub(crate) fn encode_chain(chain: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chain.len() * 8);
    for v in chain {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Decode a chain blob; `None` if the length is not a multiple of 8 or too deep.
pub(crate) fn decode_chain(blob: &[u8]) -> Option<Vec<u64>> {
    let (chunks, rest) = blob.as_chunks::<8>();
    if chunks.is_empty() || !rest.is_empty() || chunks.len() > MAX_CHAIN_DEPTH {
        return None;
    }
    let out: Vec<u64> = chunks.iter().map(|c| u64::from_le_bytes(*c)).collect();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for k in ALL_KINDS {
            assert_eq!(kind_from_code(kind_code(k)), k);
            assert_eq!(parse_kind(kind_slug(k)), Some(k));
            assert_eq!(parse_kind(&k.label().to_uppercase()), Some(k));
        }
        for c in [
            Codec::Stored,
            Codec::Zlib,
            Codec::RawDeflate,
            Codec::Other(7),
        ] {
            assert_eq!(Codec::from_code(c.code()), Some(c));
        }
        assert_eq!(Codec::from_code(99), None);
        let chain = vec![1, u64::MAX, 0];
        assert_eq!(decode_chain(&encode_chain(&chain)), Some(chain));
        assert_eq!(decode_chain(&[1, 2, 3]), None);
        assert_eq!(decode_chain(&[]), None);
    }
}
