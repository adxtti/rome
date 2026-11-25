//! Random-access byte sources. The Hex view and previewers read through this
//! trait so a 100 MB+ entry (or a multi-GB archive on disk) never needs to be
//! loaded in full.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::BackendError;

/// Positional, thread-safe reads over decoded bytes.
pub trait ByteSource: Send + Sync {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fills up to `buf.len()` bytes from `offset`; returns bytes read
    /// (short only at end of data). Reading at or past the end returns 0.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, BackendError>;

    /// Contiguous view when the data is already in memory.
    fn as_slice(&self) -> Option<&[u8]> {
        None
    }
}

/// Reads `[offset, offset + len)` clamped to the source, capped at `max` bytes.
pub fn read_vec(
    src: &dyn ByteSource,
    offset: u64,
    len: u64,
    max: usize,
) -> Result<Vec<u8>, BackendError> {
    let avail = src.len().saturating_sub(offset);
    let n = len.min(avail).min(max as u64) as usize;
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        let got = src.read_at(offset + filled as u64, &mut buf[filled..])?;
        if got == 0 {
            break;
        }
        filled += got;
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Bytes held in memory.
pub struct MemorySource {
    data: Arc<[u8]>,
}

impl MemorySource {
    pub fn new(data: impl Into<Arc<[u8]>>) -> Self {
        Self { data: data.into() }
    }
}

impl ByteSource for MemorySource {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, BackendError> {
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        let Some(src) = self.data.get(start..) else {
            return Ok(0);
        };
        let n = src.len().min(buf.len());
        buf[..n].copy_from_slice(&src[..n]);
        Ok(n)
    }

    fn as_slice(&self) -> Option<&[u8]> {
        Some(&self.data)
    }
}

/// A window `[base, base + len)` of a file on disk (a whole archive, or a
/// stored entry inside one).
pub struct FileSource {
    path: PathBuf,
    file: Mutex<File>,
    base: u64,
    len: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> Result<Self, BackendError> {
        let file = File::open(path)
            .map_err(|e| BackendError::io(format!("open {}", path.display()), e))?;
        let len = file
            .metadata()
            .map_err(|e| BackendError::io(format!("stat {}", path.display()), e))?
            .len();
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
            base: 0,
            len,
        })
    }

    /// Restricts the view to `[base, base + len)`, validated against the file size.
    pub fn window(mut self, base: u64, len: u64) -> Result<Self, BackendError> {
        let end = base.checked_add(len).ok_or(BackendError::OutOfRange {
            offset: base,
            len,
            size: self.len,
        })?;
        if end > self.len {
            return Err(BackendError::OutOfRange {
                offset: base,
                len,
                size: self.len,
            });
        }
        self.base = base;
        self.len = len;
        Ok(self)
    }
}

impl ByteSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, BackendError> {
        if offset >= self.len {
            return Ok(0);
        }
        let n = (self.len - offset).min(buf.len() as u64) as usize;
        let mut f = self
            .file
            .lock()
            .map_err(|_| BackendError::Unsupported("file lock poisoned".into()))?;
        let ctx = || format!("read {} at {:#x}", self.path.display(), self.base + offset);
        f.seek(SeekFrom::Start(self.base + offset))
            .map_err(|e| BackendError::io(ctx(), e))?;
        let mut filled = 0;
        while filled < n {
            match f.read(&mut buf[filled..n]) {
                Ok(0) => break,
                Ok(k) => filled += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(BackendError::io(ctx(), e)),
            }
        }
        Ok(filled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_source_bounds() {
        let s = MemorySource::new(vec![1u8, 2, 3, 4]);
        let mut b = [0u8; 3];
        assert_eq!(s.read_at(2, &mut b).unwrap(), 2);
        assert_eq!(&b[..2], &[3, 4]);
        assert_eq!(s.read_at(4, &mut b).unwrap(), 0);
        assert_eq!(s.read_at(u64::MAX, &mut b).unwrap(), 0);
        assert_eq!(read_vec(&s, 1, 100, 2).unwrap(), vec![2, 3]);
    }

    #[test]
    fn file_source_window() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        std::fs::write(&p, (0u8..=255).collect::<Vec<_>>()).unwrap();
        let s = FileSource::open(&p).unwrap().window(16, 32).unwrap();
        assert_eq!(s.len(), 32);
        assert_eq!(read_vec(&s, 0, 4, 64).unwrap(), vec![16, 17, 18, 19]);
        assert_eq!(read_vec(&s, 30, 10, 64).unwrap(), vec![46, 47]);
        assert!(FileSource::open(&p).unwrap().window(250, 10).is_err());
        assert!(FileSource::open(&p).unwrap().window(u64::MAX, 2).is_err());
        assert!(FileSource::open(&dir.path().join("missing")).is_err());
    }
}
