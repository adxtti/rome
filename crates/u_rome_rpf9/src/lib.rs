//! RPF9 archive reading: header parsing, zlib stream carving, chunk stitching, nested archives.
//!
//! RPF9 tables of contents are encrypted and no key is public, so this crate
//! never reads them. Entries are found by carving the plaintext payload:
//!
//! - zlib streams at 16-byte aligned offsets (`78 01`, `78 5E`, `78 9C`,
//!   `78 DA`), validated by inflating fully and checking Adler-32;
//! - 64 KiB pages: a stream that inflates to exactly 65536 bytes continues with
//!   the stream at the next 16-byte boundary after zero padding, and the pages
//!   are stitched into one entry;
//! - stored ADAT audio banks and Bink 2 (`KB2`) videos;
//! - nested `9FPR` archives, both in raw bytes and inside decoded entries;
//! - optionally, headerless raw DEFLATE (provisional).
//!
//! Sources: SentinelInteractive/0x52504639 `docs/rpf9-container.md` (MIT),
//! lynxnathan/rage-tools-vi `README.md` and `format.md` (MIT),
//! JoshRob297/rpf9-research `docs/RPF9.md` (GPL documentation, facts only),
//! CodeWalker `AwcFile.cs` (MIT) for the ADAT chunk table.

mod archive;
mod carve;
mod error;
mod header;
mod inflate;
mod read;
mod scan;
mod stored;

use std::collections::BTreeMap;
use std::fmt;
use u_rome_core::AssetKind;

pub use archive::Archive;
pub use error::Rpf9Error;
pub use header::{HEADER_LEN, Header, MAGIC, parse_header};
pub use read::read;
pub use scan::{HEAD_LEN, ScanExtras, scan, scan_with};

/// How an entry's bytes are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Codec {
    /// zlib (RFC 1950), possibly several 64 KiB pages stitched together.
    Zlib,
    /// Headerless DEFLATE (RFC 1951). No checksum, so always provisional.
    RawDeflate,
    /// Uncompressed bytes (ADAT, Bink, nested archive front region).
    Stored,
}

/// Where an entry lives.
///
/// `chain[0]` is the offset in the top-level archive; `chain[i]` is an offset
/// inside the decoded bytes of the entry at `chain[..i]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Location {
    pub chain: Vec<u64>,
}

impl Location {
    /// A location in the top-level archive.
    pub fn top(offset: u64) -> Self {
        Self {
            chain: vec![offset],
        }
    }

    /// Number of nesting levels below the top-level archive.
    pub fn depth(&self) -> usize {
        self.chain.len().saturating_sub(1)
    }

    /// The location of the entry this one is nested in, if any.
    pub fn parent(&self) -> Option<Location> {
        (self.chain.len() > 1).then(|| Location {
            chain: self
                .chain
                .get(..self.chain.len() - 1)
                .unwrap_or_default()
                .to_vec(),
        })
    }
}

impl fmt::Display for Location {
    /// Formats as `0x1000/0x40` (outermost first).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, o) in self.chain.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            write!(f, "{o:#x}")?;
        }
        Ok(())
    }
}

/// One carved entry.
#[derive(Clone, Debug)]
pub struct Found {
    pub location: Location,
    /// Bytes occupied in the containing buffer, from the location to the end
    /// of the last page (inter-page padding included, trailing padding not).
    pub stored_len: u64,
    /// Decoded size. A lower bound when `provisional` because of the size limit.
    pub size: u64,
    pub codec: Codec,
    /// Number of zlib streams stitched into this entry (1 for unpaged entries).
    pub pages: u32,
    pub kind: AssetKind,
    /// First `<= 64` decoded bytes.
    pub head: Vec<u8>,
    /// BLAKE3 of the decoded bytes, when requested and the entry was decoded in full.
    pub hash: Option<[u8; 32]>,
    /// The extent or decode is uncertain: raw DEFLATE, an ADAT bank whose chunk
    /// table could not be used, a nested archive found in place (RPF9 stores no
    /// total size, so only its front region is known), or an entry cut off by
    /// `max_entry_size`.
    pub provisional: bool,
    /// The 16 bytes before a compressed stream are zero. Community notes
    /// describe "virtual" resources as 16 zero bytes followed by a zlib stream,
    /// but ordinary alignment padding looks the same, so this is only a hint.
    pub zero_prefix: bool,
}

/// Scan settings.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Compute a BLAKE3 hash of every entry's decoded bytes.
    pub hash: bool,
    /// Worker threads; `None` uses the current rayon pool.
    pub threads: Option<usize>,
    /// Scan inside decoded entries that contain nested RPF9 archives.
    pub recurse_nested: bool,
    /// Decoded-size cap per entry. Scanning streams decoded bytes without
    /// buffering them, except when an entry contains a nested archive: that
    /// entry is decoded into memory (at most this many bytes, per thread) to
    /// be scanned. Default 512 MiB.
    pub max_entry_size: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            hash: false,
            threads: None,
            recurse_nested: true,
            max_entry_size: 512 << 20,
        }
    }
}

/// Progress through the top-level archive. Nested scans do not add bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanProgress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub found: u64,
}

/// Totals for a finished (or cancelled) scan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub found: u64,
    pub by_kind: BTreeMap<AssetKind, u64>,
    pub bytes_scanned: u64,
    pub cancelled: bool,
}
