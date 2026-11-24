//! Error type for every fallible operation in this crate.
//!
//! Game data is untrusted, so every error carries enough context (offsets,
//! indices, sizes) to diagnose a failure on a real install from a log line.

use thiserror::Error;
use u_rome_core::ByteError;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// The bytes are neither an `RDS5` store nor a bare `ETPF` table.
    #[error("not a texture store: {0}")]
    NotAStore(String),

    /// A structure is present but its fields are inconsistent.
    #[error("malformed {what} at offset {offset:#x}: {detail}")]
    Malformed {
        what: &'static str,
        offset: u64,
        detail: String,
    },

    /// A bounds-checked read failed.
    #[error(transparent)]
    Bytes(#[from] ByteError),

    #[error("texture index {index} out of range ({count} textures)")]
    IndexOutOfRange { index: usize, count: usize },

    #[error("mip {mip} out of range (texture has {count} mips)")]
    MipOutOfRange { mip: u32, count: u32 },

    #[error("slice {slice} out of range (texture has {count} slices)")]
    SliceOutOfRange { slice: u32, count: u32 },

    /// A codec or pixel format this crate cannot decode. Studio shows this
    /// message instead of rendering garbage.
    #[error("unsupported: codec {codec}, format {format}")]
    Unsupported { codec: String, format: String },

    /// A texture layout (3D volume, unknown tile mode, ...) this crate cannot detile.
    #[error("unsupported texture layout: {0}")]
    UnsupportedLayout(String),

    /// The store is a bare `ETPF` table without chunk data.
    #[error("store has no chunk data (bare ETPF table); open the RDS5 store instead")]
    NoData,

    /// A chunk failed to unpack.
    #[error("chunk {chunk} ({codec}) at file offset {offset:#x}: {detail}")]
    Chunk {
        chunk: usize,
        codec: String,
        offset: u64,
        detail: String,
    },

    /// A virtual byte range is not covered by the chunk table.
    #[error("virtual range {start:#x}..{end:#x} is not covered by any chunk")]
    NotCovered { start: u64, end: u64 },

    /// An allocation derived from header fields exceeds a safety cap.
    #[error("{what} needs {requested} bytes, above the limit of {limit}")]
    TooLarge {
        what: &'static str,
        requested: u64,
        limit: u64,
    },

    /// Input shorter than its declared size.
    #[error("{what}: need {needed} bytes, have {have}")]
    Truncated {
        what: &'static str,
        needed: u64,
        have: u64,
    },

    /// Bad arguments to a public helper.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// PNG or DDS encoding failed.
    #[error("encode failed: {0}")]
    Encode(String),
}
