//! The real backend: a scanned install in the shared SQLite index
//! (`u_rome_index`), with entries decoded on demand through
//! `u_rome_install::EntryReader` (`u_rome_rpf9` underneath).
//!
//! Entry ids: archives on disk are `ARCHIVE_BIT | archive id`, indexed
//! entries are their index row id.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use u_rome_core::{AssetKind, Hash32};
use u_rome_index::{ArchiveId, EntryRow, Index};
use u_rome_install::{EntryReader, ScanEvent, ScanSettings};

use super::{
    Backend, BackendError, BackendProvider, BackendStats, ByteSource, Codec, EntryDetails, EntryId,
    EntryInfo, FileSource, HeaderField, MemorySource, location_chain,
};
use crate::jobs::JobContext;

const ARCHIVE_BIT: u64 = 1 << 62;
/// Most children listed under one node; larger archives are browsed via Search.
const MAX_CHILDREN: u32 = 50_000;

fn err(e: impl std::fmt::Display) -> BackendError {
    BackendError::Format {
        offset: None,
        message: e.to_string(),
    }
}

fn codec(c: u_rome_index::Codec) -> Codec {
    match c {
        u_rome_index::Codec::Stored => Codec::Stored,
        u_rome_index::Codec::Zlib => Codec::Deflate,
        u_rome_index::Codec::RawDeflate => Codec::Other("raw deflate".into()),
        u_rome_index::Codec::Other(n) => Codec::Other(format!("codec {n}")),
    }
}

pub struct IndexBackend {
    root: PathBuf,
    index_path: PathBuf,
    index: Mutex<Index>,
    reader: EntryReader,
}

impl IndexBackend {
    pub fn open(index_path: &Path, root: &Path) -> Result<Self, BackendError> {
        let index = Index::open(index_path).map_err(err)?;
        Ok(Self {
            root: root.to_path_buf(),
            index_path: index_path.to_path_buf(),
            index: Mutex::new(index),
            reader: EntryReader::new(root),
        })
    }

    fn with_index<T>(
        &self,
        f: impl FnOnce(&Index) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        let guard = self
            .index
            .lock()
            .map_err(|_| BackendError::Unsupported("index lock poisoned".into()))?;
        f(&guard)
    }

    fn row_info(row: &EntryRow) -> EntryInfo {
        let parent = match row.parent_id {
            Some(p) => p.0 as u64,
            None => ARCHIVE_BIT | row.archive_id.0 as u64,
        };
        EntryInfo {
            id: row.id.0 as u64,
            parent: Some(parent),
            name: row.name.clone(),
            name_hash: row.name_hash.map(Hash32),
            kind: row.kind,
            codec: codec(row.codec),
            offset: row.chain.last().copied().unwrap_or(0),
            stored_size: row.stored_len,
            size: row.size,
            // Nested archives have children; everything else is a leaf.
            child_count: (row.kind == AssetKind::Archive).then_some(1),
            provisional: row.provisional || row.name_match.is_some_and(|m| m.provisional()),
        }
    }

    fn archive_info(a: &u_rome_index::ArchiveRow) -> EntryInfo {
        EntryInfo {
            id: ARCHIVE_BIT | a.id.0 as u64,
            parent: None,
            name: Some(a.rel_path.clone()),
            name_hash: None,
            kind: AssetKind::Archive,
            codec: Codec::Stored,
            offset: 0,
            stored_size: a.size,
            size: a.size,
            child_count: Some(u32::try_from(a.entry_count).unwrap_or(u32::MAX)),
            provisional: false,
        }
    }

    fn row(&self, id: EntryId) -> Result<EntryRow, BackendError> {
        self.with_index(|ix| {
            ix.entry(u_rome_index::EntryId(id as i64))
                .map_err(err)?
                .ok_or(BackendError::NotFound(id))
        })
    }
}

impl Backend for IndexBackend {
    fn label(&self) -> String {
        "RPF9 index".into()
    }

    fn root_path(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    fn stats(&self) -> BackendStats {
        self.with_index(|ix| {
            let s = ix.stats().map_err(err)?;
            Ok(BackendStats {
                archives: s.archives,
                entries: s.entries,
                total_bytes: s.total_size,
            })
        })
        .unwrap_or_default()
    }

    fn roots(&self) -> Result<Vec<EntryInfo>, BackendError> {
        self.with_index(|ix| {
            Ok(ix
                .archives()
                .map_err(err)?
                .iter()
                .map(Self::archive_info)
                .collect())
        })
    }

    fn children(&self, id: EntryId) -> Result<Vec<EntryInfo>, BackendError> {
        self.with_index(|ix| {
            let rows = if id & ARCHIVE_BIT != 0 {
                let aid = ArchiveId((id & !ARCHIVE_BIT) as i64);
                let rows = ix.roots_in(aid, 0, MAX_CHILDREN).map_err(err)?;
                if rows.len() as u32 >= MAX_CHILDREN {
                    log::warn!(
                        "archive {aid:?} has more than {MAX_CHILDREN} top-level entries; showing the first {MAX_CHILDREN}, use Search for the rest"
                    );
                }
                rows
            } else {
                ix.children(u_rome_index::EntryId(id as i64)).map_err(err)?
            };
            Ok(rows.iter().map(Self::row_info).collect())
        })
    }

    fn entry(&self, id: EntryId) -> Result<EntryInfo, BackendError> {
        if id & ARCHIVE_BIT != 0 {
            let aid = ArchiveId((id & !ARCHIVE_BIT) as i64);
            return self.with_index(|ix| {
                ix.archive(aid)
                    .map_err(err)?
                    .map(|a| Self::archive_info(&a))
                    .ok_or(BackendError::NotFound(id))
            });
        }
        Ok(Self::row_info(&self.row(id)?))
    }

    fn details(&self, id: EntryId) -> Result<EntryDetails, BackendError> {
        let chain = location_chain(self, id)?;
        let mut header = Vec::new();
        let mut notes = Vec::new();
        if id & ARCHIVE_BIT != 0 {
            let aid = ArchiveId((id & !ARCHIVE_BIT) as i64);
            if let Some(a) = self.with_index(|ix| ix.archive(aid).map_err(err))? {
                header.push(HeaderField::new("path", a.rel_path.clone()));
                header.push(HeaderField::new("scan state", format!("{:?}", a.state)));
                header.push(HeaderField::new("entries", a.entry_count.to_string()));
                if let Some(h) = a.header {
                    header.push(HeaderField::new("RPF9 header", h));
                }
            }
        } else {
            let row = self.row(id)?;
            header.push(HeaderField::new(
                "location",
                row.chain
                    .iter()
                    .map(|o| format!("{o:#x}"))
                    .collect::<Vec<_>>()
                    .join(" > "),
            ));
            header.push(HeaderField::new("pages", row.pages.to_string()));
            if let Some(h) = row.hash {
                header.push(HeaderField::new(
                    "blake3",
                    h.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                ));
            }
            if let Some(m) = row.name_match {
                header.push(
                    HeaderField::new("name source", format!("{m:?}")).provisional(m.provisional()),
                );
            }
            if row.provisional {
                notes.push("The scanner was unsure about this entry's extent or decode.".into());
            }
        }
        notes.push(format!("index: {}", self.index_path.display()));
        Ok(EntryDetails {
            chain,
            header,
            notes,
        })
    }

    fn open(&self, id: EntryId, ctx: &JobContext) -> Result<Arc<dyn ByteSource>, BackendError> {
        ctx.check()?;
        if id & ARCHIVE_BIT != 0 {
            let aid = ArchiveId((id & !ARCHIVE_BIT) as i64);
            let a = self
                .with_index(|ix| ix.archive(aid).map_err(err))?
                .ok_or(BackendError::NotFound(id))?;
            return Ok(Arc::new(FileSource::open(&self.root.join(&a.rel_path))?));
        }
        let row = self.row(id)?;
        ctx.set_status(format!("decoding entry {id} ({} bytes)", row.size));
        let bytes = self.with_index(|ix| self.reader.read(ix, &row).map_err(err))?;
        Ok(Arc::new(MemorySource::new(bytes)))
    }

    fn walk(
        &self,
        ctx: &JobContext,
        visit: &mut dyn FnMut(&EntryInfo),
    ) -> Result<(), BackendError> {
        // The index is flat: walk archives, then each archive's entries in
        // pages. Parents come before children because entries are stored in
        // scan order with nested ones pushed last.
        let archives = self.roots()?;
        for a in &archives {
            visit(a);
            let aid = ArchiveId((a.id & !ARCHIVE_BIT) as i64);
            let mut offset = 0u32;
            loop {
                ctx.check()?;
                let rows = self.with_index(|ix| ix.entries_in(aid, offset, 10_000).map_err(err))?;
                if rows.is_empty() {
                    break;
                }
                for r in &rows {
                    visit(&Self::row_info(r));
                }
                offset = offset.saturating_add(rows.len() as u32);
            }
        }
        Ok(())
    }
}

/// Scans the chosen folder into the shared index (skipping files already
/// indexed), then opens it.
pub struct IndexProvider {
    pub index_path: PathBuf,
}

impl Default for IndexProvider {
    fn default() -> Self {
        Self {
            index_path: u_rome_install::default_index_path(),
        }
    }
}

impl BackendProvider for IndexProvider {
    fn name(&self) -> &str {
        "RPF9"
    }

    fn accepts(&self, root: Option<&Path>) -> bool {
        root.is_some_and(|r| crate::app::check_game_dir(r).rpf_files > 0)
    }

    fn open(
        &self,
        root: Option<&Path>,
        ctx: &JobContext,
    ) -> Result<Arc<dyn Backend>, BackendError> {
        let root =
            root.ok_or_else(|| BackendError::Unsupported("no install folder chosen".into()))?;
        let cancel = AtomicBool::new(false);
        let total = Mutex::new((0u64, 0u64)); // (bytes of finished files, all bytes)
        let current = Mutex::new(0u64); // size of the file being scanned
        let on_event = |ev: ScanEvent| {
            if ctx.is_cancelled() {
                cancel.store(true, Ordering::Relaxed);
            }
            match ev {
                ScanEvent::Discovered { files, total_bytes } => {
                    if let Ok(mut t) = total.lock() {
                        t.1 = total_bytes;
                    }
                    ctx.set_status(format!("{files} archives found"));
                }
                ScanEvent::FileStarted { rel_path, size, .. } => {
                    if let Ok(mut c) = current.lock() {
                        *c = size;
                    }
                    ctx.set_status(format!("scanning {rel_path}"));
                    log::info!("scanning {rel_path} ({size} bytes)");
                }
                ScanEvent::Progress { progress, .. } => {
                    if let Ok(t) = total.lock() {
                        ctx.set_progress(t.0 + progress.bytes_done, t.1.max(1));
                    }
                }
                ScanEvent::FileDone {
                    rel_path,
                    found,
                    seconds,
                    ..
                } => {
                    log::info!("{rel_path}: {found} entries in {seconds:.1}s");
                    let size = current.lock().map(|c| *c).unwrap_or(0);
                    if let Ok(mut t) = total.lock() {
                        t.0 += size;
                    }
                }
                ScanEvent::FileSkipped { rel_path, .. } => {
                    log::debug!("{rel_path}: already indexed");
                }
                ScanEvent::FileFailed {
                    rel_path, error, ..
                } => {
                    log::warn!("{rel_path}: scan failed: {error}");
                }
                ScanEvent::NamesImported { names } => log::info!("imported {names} names"),
            }
        };
        let report = u_rome_install::scan_install(
            root,
            &self.index_path,
            &ScanSettings::default(),
            &cancel,
            &on_event,
        )
        .map_err(err)?;
        if report.cancelled {
            return Err(BackendError::Cancelled);
        }
        log::info!(
            "scan done: {} scanned, {} skipped, {} failed, {} entries",
            report.scanned,
            report.skipped,
            report.failed,
            report.entries
        );
        Ok(Arc::new(IndexBackend::open(&self.index_path, root)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_synthetic_install_and_reads_entries() {
        let dir = std::env::temp_dir().join(format!("u_rome_studio_index_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let game = dir.join("game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(
            game.join("sample.rpf"),
            u_rome_testdata::Rpf9Builder::sample(5).build().bytes,
        )
        .unwrap();
        let provider = IndexProvider {
            index_path: dir.join("index.sqlite"),
        };
        assert!(provider.accepts(Some(&game)));
        let ctx = JobContext::detached();
        let backend = provider.open(Some(&game), &ctx).unwrap();
        let roots = backend.roots().unwrap();
        assert_eq!(roots.len(), 1);
        let kids = backend.children(roots[0].id).unwrap();
        assert!(!kids.is_empty());
        for k in &kids {
            let src = backend.open(k.id, &ctx).unwrap();
            if !k.provisional {
                assert_eq!(src.len(), k.size);
            }
            let d = backend.details(k.id).unwrap();
            assert_eq!(d.chain.first().map(|e| e.id), Some(roots[0].id));
        }
        let mut seen = 0;
        backend.walk(&ctx, &mut |_| seen += 1).unwrap();
        assert!(seen > kids.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
