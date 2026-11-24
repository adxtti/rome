//! Error type for AWC parsing, packet extraction and decoding.

use thiserror::Error;
use u_rome_core::ByteError;

use crate::codec::Codec;

/// Errors produced by this crate. Every variant carries enough context
/// (offsets, stream and block indices) to diagnose a failure on real data
/// from a bug report alone.
#[derive(Debug, Error)]
pub enum AwcError {
    /// A bounds-checked read failed. `context` says what was being read.
    #[error("{context}: {source}")]
    Read {
        context: String,
        #[source]
        source: ByteError,
    },

    /// The buffer does not start with `ADAT`.
    #[error("not an AWC bank: expected magic \"ADAT\" at offset 0, found {found:02X?}")]
    BadMagic { found: Vec<u8> },

    /// Big-endian (`TADA`) banks come from PS3/Xbox 360 era titles.
    #[error("big-endian AWC bank (magic \"TADA\") is not supported")]
    BigEndian,

    /// Header version other than 0 or 1.
    #[error("unsupported AWC version {version} (flags 0x{flags:04X})")]
    UnsupportedVersion { version: u16, flags: u16 },

    /// Structurally invalid header or stream table.
    #[error("invalid AWC header at offset 0x{offset:X}: {reason}")]
    InvalidHeader { offset: usize, reason: String },

    /// A header field exceeds a sanity limit (protects allocations).
    #[error("{what} = {value} exceeds the limit of {max}")]
    Limit {
        what: &'static str,
        value: u64,
        max: u64,
    },

    /// A chunk table entry points outside the bank.
    #[error(
        "chunk tag 0x{tag:02X} at offset 0x{offset:X} with size 0x{size:X} lies outside the bank ({len} bytes)"
    )]
    ChunkOutOfBounds {
        tag: u8,
        offset: usize,
        size: usize,
        len: usize,
    },

    /// Requested stream index does not exist.
    #[error("stream index {index} out of range (bank has {count} streams)")]
    StreamIndex { index: usize, count: usize },

    /// The stream's codec cannot be handled by the requested operation.
    #[error(
        "stream {index} (id 0x{id:08X}) uses codec {codec}, which is not supported for {operation}"
    )]
    UnsupportedCodec {
        index: usize,
        id: u32,
        codec: Codec,
        operation: &'static str,
    },

    /// The stream's container layout is not understood.
    #[error("stream {index}: unsupported layout: {reason}")]
    UnsupportedLayout { index: usize, reason: String },

    /// The bank sets the encryption flag (0x0008 per vgmstream).
    #[error("bank sets the encrypted-data flag (0x0008); decryption is not supported")]
    Encrypted,

    /// A data block of a streamed (music) bank is malformed.
    #[error("stream {stream}, block {block} at offset 0x{offset:X}: {reason}")]
    Block {
        stream: usize,
        block: usize,
        offset: usize,
        reason: String,
    },

    /// Opus decoding failed for every packet.
    #[error("{context}: Opus decoding failed for all {packets} packets; first error: {message}")]
    Decode {
        context: String,
        packets: usize,
        message: String,
    },

    /// The stream holds no audio.
    #[error("stream {stream} contains no audio packets")]
    NoAudio { stream: usize },
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, AwcError>;
