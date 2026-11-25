//! Error type for the map crate.

use thiserror::Error;
use u_rome_core::ByteError;

/// Everything that can go wrong while decoding meta resources and map data.
///
/// Every variant carries enough context (offsets, hashes, counts) to diagnose
/// a failure on real data from a log line alone.
#[derive(Debug, Error)]
pub enum MapError {
    /// A bounds-checked read failed.
    #[error("{context}: {source}")]
    Bytes {
        context: String,
        #[source]
        source: ByteError,
    },

    /// The buffer does not carry the `0DRP` magic at +0x10.
    #[error("not a meta resource: expected \"0DRP\" at offset 0x10, found {found:02X?}")]
    BadMagic { found: Vec<u8> },

    /// The `0DRP` resource type byte at +0x14 is not 0x7a.
    #[error("unsupported PRD0 resource type 0x{found:02X} at offset 0x14 (expected 0x7A)")]
    BadResourceType { found: u8 },

    /// A count read from a header exceeds a sanity cap.
    #[error("{what}: count {count} exceeds limit {limit}")]
    TooLarge {
        what: &'static str,
        count: usize,
        limit: usize,
    },

    /// A pointer or pool reference does not resolve inside the resource.
    #[error("{context}: reference 0x{value:016X} does not resolve")]
    BadReference { context: String, value: u64 },

    /// A pointer resolves, but the data it describes runs past its page.
    #[error(
        "{context}: {len} bytes at {page:?}+0x{offset:X} run past the end of the page ({page_len} bytes)"
    )]
    OutOfPage {
        context: String,
        page: crate::meta::Page,
        offset: usize,
        len: usize,
        page_len: usize,
    },

    /// A member has a type code this reader cannot interpret for the request.
    #[error("{context}: member 0x{member:08X} has unsupported type code 0x{type_code:02X}")]
    UnsupportedType {
        context: String,
        member: u32,
        type_code: u8,
    },

    /// The resource holds no `CMapData` (struct `d3593fa6`) object.
    #[error("meta resource has no CMapData (struct 0xD3593FA6) instance")]
    NoMapData,
}

impl MapError {
    pub(crate) fn bytes(context: impl Into<String>, source: ByteError) -> Self {
        MapError::Bytes {
            context: context.into(),
            source,
        }
    }
}

/// Result alias for this crate.
pub type Result<T, E = MapError> = std::result::Result<T, E>;
