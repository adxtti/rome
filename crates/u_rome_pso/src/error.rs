//! Error type for PSIN parsing and value decoding.
//!
//! Every variant carries enough context (absolute file offsets, section tags,
//! block indices, raw reference values) to diagnose a failure on real game
//! data from a log line alone.

use thiserror::Error;
use u_rome_core::{ByteError, Hash32};

/// Result alias used throughout the crate.
pub type Result<T, E = PsoError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum PsoError {
    /// The buffer does not start with the `PSIN` magic.
    #[error("not a PSIN document: first bytes are {found:02X?}, expected \"PSIN\"")]
    NotPsin { found: Vec<u8> },

    /// A section header holds non-printable tag bytes.
    #[error("section header at offset {offset:#x}: invalid tag bytes {tag:02X?}")]
    BadSectionTag { offset: usize, tag: [u8; 4] },

    /// A section size is smaller than its header or runs past the buffer.
    #[error(
        "section {tag} at offset {offset:#x}: size {size:#x} is invalid ({reason}); \
         {remaining:#x} bytes remain in the buffer"
    )]
    BadSectionSize {
        tag: String,
        offset: usize,
        size: u64,
        remaining: usize,
        reason: &'static str,
    },

    /// More sections than any known layout uses.
    #[error("more than {max} sections; refusing to continue")]
    TooManySections { max: usize },

    /// A section body does not match its expected layout.
    #[error("{section} at offset {offset:#x}: {message}")]
    Malformed {
        section: &'static str,
        offset: usize,
        message: String,
    },

    /// A u64 pool reference points outside the block table or a block.
    #[error("bad pool reference {reference:#018x} at offset {offset:#x}: {message}")]
    BadReference {
        reference: u64,
        offset: usize,
        message: String,
    },

    /// A struct hash used by a member, block or pointer has no schema entry.
    #[error("struct {hash} is not in the schema (needed at offset {offset:#x}, {context})")]
    UnknownStruct {
        hash: Hash32,
        offset: usize,
        context: String,
    },

    /// A pool reference leads back to a struct that is already being decoded.
    #[error(
        "reference cycle: struct {type_hash} at offset {offset:#x} (block {block}) is its own ancestor"
    )]
    Cycle {
        type_hash: Hash32,
        block: u32,
        offset: usize,
    },

    /// The document has no usable root block.
    #[error("no root instance: {0}")]
    NoRoot(String),

    /// Decoding nested deeper than `DecodeOptions::max_depth`.
    #[error("depth limit {limit} exceeded at offset {offset:#x} (block {block})")]
    DepthLimit {
        limit: usize,
        block: u32,
        offset: usize,
    },

    /// Decoding produced more than `DecodeOptions::max_nodes` values.
    #[error("node limit {limit} exceeded")]
    NodeLimit { limit: usize },

    /// The document uses a layout this crate does not decode.
    #[error("unsupported layout: {0}")]
    Unsupported(String),

    /// A bounds-checked read failed.
    #[error(transparent)]
    Byte(#[from] ByteError),
}

impl PsoError {
    /// True for errors raised by the safety limits (depth, node count), which
    /// lenient decoding never swallows.
    pub fn is_limit(&self) -> bool {
        matches!(
            self,
            PsoError::DepthLimit { .. } | PsoError::NodeLimit { .. }
        )
    }
}
