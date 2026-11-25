//! Error type for archive reading.

use std::path::PathBuf;
use thiserror::Error;

/// Everything that can go wrong while opening, scanning or reading an RPF9 archive.
///
/// Offsets are always byte offsets; `depth` is the index into a
/// [`Location::chain`](crate::Location) (0 = top-level archive bytes).
#[derive(Debug, Error)]
pub enum Rpf9Error {
    #[error("cannot open {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("bad RPF9 header at offset {offset:#x}: {reason}")]
    BadHeader { offset: u64, reason: String },
    #[error("location chain is empty")]
    EmptyLocation,
    #[error("chain depth {depth}: offset {offset:#x} is outside the {len}-byte buffer")]
    OutOfRange { depth: usize, offset: u64, len: u64 },
    #[error("chain depth {depth}: no recognisable entry at offset {offset:#x}: {reason}")]
    NoEntry {
        depth: usize,
        offset: u64,
        reason: String,
    },
    #[error(
        "chain depth {depth}: entry at offset {offset:#x} decodes to more than the {limit}-byte limit"
    )]
    TooLarge {
        depth: usize,
        offset: u64,
        limit: u64,
    },
    #[error("chain depth {depth}: entry at offset {offset:#x} is corrupt: {reason}")]
    Corrupt {
        depth: usize,
        offset: u64,
        reason: String,
    },
    #[error("cannot build scan thread pool: {0}")]
    ThreadPool(String),
}
