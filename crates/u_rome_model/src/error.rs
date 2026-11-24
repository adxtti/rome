//! Error type for every decoder and exporter in this crate.

use thiserror::Error;
use u_rome_core::ByteError;

/// Everything that can go wrong while decoding or exporting a model.
///
/// Messages carry byte offsets and the structure being read so that a
/// failure on real game data can be diagnosed from a log line alone.
#[derive(Debug, Error)]
pub enum ModelError {
    /// A bounds-checked read failed.
    #[error("{context}: {source}")]
    Bytes {
        context: String,
        #[source]
        source: ByteError,
    },

    /// The vertex buffer blob does not start with u32 `0x4042`.
    #[error("vertex buffer: bad magic 0x{found:08X} at offset 0 (expected 0x00004042)")]
    BadVertexMagic { found: u32 },

    /// The vertex buffer header or one of its streams is inconsistent.
    #[error("vertex buffer: {0}")]
    VertexBuffer(String),

    /// The meshlet index buffer is inconsistent.
    #[error("index buffer: {0}")]
    IndexBuffer(String),

    /// A count read from game data exceeds a sanity cap.
    #[error("{what}: count {requested} exceeds limit {limit}")]
    TooLarge {
        what: &'static str,
        requested: u64,
        limit: u64,
    },

    /// The resource (drawable / fragment) structure could not be walked.
    #[error("resource: {0}")]
    Resource(String),

    /// glTF export failed.
    #[error("glb export: {0}")]
    Export(String),
}

pub type Result<T, E = ModelError> = std::result::Result<T, E>;

/// Attach context to a [`ByteError`].
pub(crate) trait Context<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> Context<T> for std::result::Result<T, ByteError> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| ModelError::Bytes {
            context: context(),
            source,
        })
    }
}
