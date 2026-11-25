//! Archive bytes: memory-mapped file or owned buffer.

use crate::error::Rpf9Error;
use crate::header::{Header, parse_header};
use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};

enum Storage {
    Mmap(Mmap),
    Owned(Vec<u8>),
}

/// An RPF9 archive (or any byte blob to carve).
pub struct Archive {
    storage: Storage,
    header: Option<Header>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archive")
            .field("path", &self.path)
            .field("len", &self.bytes().len())
            .field("header", &self.header)
            .finish()
    }
}

impl Archive {
    /// Memory-map the file at `path`. Files are opened read-only.
    pub fn open(path: &Path) -> Result<Archive, Rpf9Error> {
        let io = |source| Rpf9Error::Io {
            path: path.to_path_buf(),
            source,
        };
        let file = File::open(path).map_err(io)?;
        let len = file.metadata().map_err(io)?.len();
        let storage = if len == 0 {
            // Mapping an empty file fails on some platforms.
            Storage::Owned(Vec::new())
        } else {
            // SAFETY: the map is read-only. If another process truncates or
            // rewrites the file while it is mapped, reads may observe changed
            // bytes or fault; this is the documented memmap2 caveat. Game
            // installs are not modified while Studio reads them.
            let map = unsafe { Mmap::map(&file) }.map_err(io)?;
            Storage::Mmap(map)
        };
        let mut a = Self::build(storage);
        a.path = Some(path.to_path_buf());
        log::debug!(
            "opened {} ({} bytes, header {})",
            path.display(),
            a.bytes().len(),
            if a.header.is_some() {
                "valid"
            } else {
                "absent"
            }
        );
        Ok(a)
    }

    /// Wrap bytes already in memory.
    pub fn from_vec(bytes: Vec<u8>) -> Archive {
        Self::build(Storage::Owned(bytes))
    }

    fn build(storage: Storage) -> Archive {
        let mut a = Archive {
            storage,
            header: None,
            path: None,
        };
        let data = a.bytes();
        a.header = match parse_header(data) {
            Ok(h) if h.payload_start as usize <= data.len() => {
                let issues = h.issues(Some(data.len() as u64));
                if !issues.is_empty() {
                    log::warn!("RPF9 header has unusual fields: {}", issues.join("; "));
                }
                Some(h)
            }
            Ok(h) => {
                log::warn!(
                    "RPF9 header payload_start {:#x} is past end of file ({} bytes); ignoring header",
                    h.payload_start,
                    data.len()
                );
                None
            }
            Err(e) => {
                log::debug!("no top-level RPF9 header: {e}");
                None
            }
        };
        a
    }

    /// The whole archive.
    pub fn bytes(&self) -> &[u8] {
        match &self.storage {
            Storage::Mmap(m) => m,
            Storage::Owned(v) => v,
        }
    }

    /// The top-level header, if the file starts with a valid one.
    pub fn header(&self) -> Option<&Header> {
        self.header.as_ref()
    }

    /// Path the archive was opened from, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}
