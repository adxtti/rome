//! Data access abstraction between the UI and the decoder crates.
//!
//! The UI never touches archive formats. It sees a tree of [`EntryInfo`]
//! (archives on disk, their entries, nested archives) and reads decoded bytes
//! through [`ByteSource`]. The real RPF9 backend implements [`Backend`] and is
//! constructed by a [`BackendProvider`]; until then [`mock::MockBackend`]
//! drives the whole UI with a synthetic install.

pub mod mock;
pub mod source;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use u_rome_core::{AssetKind, Hash32};

use crate::jobs::JobContext;

pub use source::{ByteSource, FileSource, MemorySource, read_vec};

/// Opaque, backend-defined entry identifier (stable for one backend instance).
pub type EntryId = u64;

/// Hard cap on tree depth followed when walking parents or children, so a
/// corrupt or cyclic archive cannot hang the UI.
pub const MAX_DEPTH: usize = 64;

/// Backend errors. Messages carry offsets and context so a failure on real
/// data can be diagnosed from a pasted log.
#[derive(Debug, Error)]
pub enum BackendError {
    #[error("entry {0:#x} not found")]
    NotFound(EntryId),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("read of {len} bytes at offset {offset:#x} past end ({size:#x} bytes)")]
    OutOfRange { offset: u64, len: u64, size: u64 },
    #[error("format error{}: {message}", offset.map(|o| format!(" at offset {o:#x}")).unwrap_or_default())]
    Format {
        offset: Option<u64>,
        message: String,
    },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("limit exceeded: {0}")]
    Limit(String),
    #[error("cancelled")]
    Cancelled,
}

impl BackendError {
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl From<crate::jobs::JobError> for BackendError {
    fn from(e: crate::jobs::JobError) -> Self {
        match e {
            crate::jobs::JobError::Cancelled => BackendError::Cancelled,
            other => BackendError::Unsupported(other.to_string()),
        }
    }
}

/// How an entry is stored in its parent container.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    Stored,
    Deflate,
    Zstd,
    /// Oodle (Kraken/Mermaid/...); decoding needs the user's own oo2core.
    Oodle,
    /// Anything else, labelled by the backend.
    Other(String),
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Codec::Stored => f.write_str("stored"),
            Codec::Deflate => f.write_str("deflate"),
            Codec::Zstd => f.write_str("zstd"),
            Codec::Oodle => f.write_str("oodle"),
            Codec::Other(s) => f.write_str(s),
        }
    }
}

/// One node in the install tree.
#[derive(Clone, Debug, PartialEq)]
pub struct EntryInfo {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    /// Resolved name, when known. RPF9 tables are encrypted and names come
    /// from hash dictionaries, so many entries have none.
    pub name: Option<String>,
    pub name_hash: Option<Hash32>,
    pub kind: AssetKind,
    pub codec: Codec,
    /// Offset of the stored data inside the parent (or 0 for files on disk).
    pub offset: u64,
    /// Bytes occupied in the parent.
    pub stored_size: u64,
    /// Decoded size.
    pub size: u64,
    /// Number of children (0 for leaves). Containers whose table has not been
    /// read yet may report `None`.
    pub child_count: Option<u32>,
    /// Any part of this entry's metadata is a guess (uncertain format facts).
    pub provisional: bool,
}

impl EntryInfo {
    /// Name if known, else the hash, else `@<offset>`.
    pub fn display_name(&self) -> String {
        match (&self.name, self.name_hash) {
            (Some(n), _) => n.clone(),
            (None, Some(h)) => format!("{h} @{:#010x}", self.offset),
            (None, None) => format!("@{:#010x}", self.offset),
        }
    }

    pub fn is_container(&self) -> bool {
        self.kind == AssetKind::Archive || self.child_count.is_some_and(|c| c > 0)
    }
}

/// A decoded header field shown in the Inspector.
#[derive(Clone, Debug, PartialEq)]
pub struct HeaderField {
    pub name: String,
    pub value: String,
    /// Byte offset inside the decoded entry, when meaningful.
    pub offset: Option<u64>,
    pub provisional: bool,
}

impl HeaderField {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            offset: None,
            provisional: false,
        }
    }

    pub fn at(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    pub fn provisional(mut self, p: bool) -> Self {
        self.provisional = p;
        self
    }
}

/// Everything the Inspector shows for one entry.
#[derive(Clone, Debug, Default)]
pub struct EntryDetails {
    /// Root-first chain ending with the entry itself.
    pub chain: Vec<EntryInfo>,
    pub header: Vec<HeaderField>,
    /// Free-form notes (e.g. "TOC decrypted with key 3", "name from dictionary").
    pub notes: Vec<String>,
}

impl EntryDetails {
    pub fn entry(&self) -> Option<&EntryInfo> {
        self.chain.last()
    }
}

/// Aggregate numbers for the status bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendStats {
    pub archives: u64,
    pub entries: u64,
    pub total_bytes: u64,
}

/// Read access to an install. Implementations must be cheap to call from the
/// UI thread for `roots`/`entry`; anything slow belongs in `open` and `walk`,
/// which the UI runs on worker threads.
pub trait Backend: Send + Sync {
    /// Short label for the status bar and diagnostics (`Mock`, `RPF9`).
    fn label(&self) -> String;

    /// Install folder this backend reads, if any.
    fn root_path(&self) -> Option<PathBuf>;

    /// True for synthetic data; the UI shows a banner.
    fn is_mock(&self) -> bool {
        false
    }

    fn stats(&self) -> BackendStats;

    /// Top-level archives on disk.
    fn roots(&self) -> Result<Vec<EntryInfo>, BackendError>;

    /// Direct children of a container entry.
    fn children(&self, id: EntryId) -> Result<Vec<EntryInfo>, BackendError>;

    fn entry(&self, id: EntryId) -> Result<EntryInfo, BackendError>;

    /// Inspector details. The default builds the location chain from parents.
    fn details(&self, id: EntryId) -> Result<EntryDetails, BackendError> {
        Ok(EntryDetails {
            chain: location_chain(self, id)?,
            ..Default::default()
        })
    }

    /// Decoded bytes of an entry. May decompress or decrypt; runs on a worker.
    fn open(&self, id: EntryId, ctx: &JobContext) -> Result<Arc<dyn ByteSource>, BackendError>;

    /// Visits every entry (depth-first, parents before children). The default
    /// walks `roots`/`children`; an index-backed backend can override it.
    fn walk(
        &self,
        ctx: &JobContext,
        visit: &mut dyn FnMut(&EntryInfo),
    ) -> Result<(), BackendError> {
        let mut stack: Vec<(EntryInfo, usize)> =
            self.roots()?.into_iter().rev().map(|e| (e, 0)).collect();
        let mut seen: u64 = 0;
        while let Some((e, depth)) = stack.pop() {
            if seen.is_multiple_of(1024) {
                ctx.check()?;
            }
            seen += 1;
            visit(&e);
            if e.is_container() {
                if depth >= MAX_DEPTH {
                    log::warn!("walk: depth limit reached at entry {:#x}", e.id);
                    continue;
                }
                match self.children(e.id) {
                    Ok(kids) => stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1))),
                    Err(err) => log::warn!("walk: children of {:#x} failed: {err}", e.id),
                }
            }
        }
        Ok(())
    }
}

/// Root-first chain of entries ending at `id`.
pub fn location_chain<B: Backend + ?Sized>(
    backend: &B,
    id: EntryId,
) -> Result<Vec<EntryInfo>, BackendError> {
    let mut chain = vec![backend.entry(id)?];
    while let Some(parent) = chain.last().and_then(|e| e.parent) {
        if chain.len() > MAX_DEPTH {
            return Err(BackendError::Limit(format!(
                "parent chain of {id:#x} deeper than {MAX_DEPTH}"
            )));
        }
        chain.push(backend.entry(parent)?);
    }
    chain.reverse();
    Ok(chain)
}

/// Creates backends. The integrator registers the real RPF9 provider ahead of
/// the mock in `app::providers()`.
pub trait BackendProvider: Send + Sync {
    fn name(&self) -> &str;

    /// Quick check whether this provider understands `root` (UI thread).
    fn accepts(&self, root: Option<&Path>) -> bool;

    /// Opens or scans the install. Runs on a worker thread with progress.
    fn open(&self, root: Option<&Path>, ctx: &JobContext)
    -> Result<Arc<dyn Backend>, BackendError>;
}
