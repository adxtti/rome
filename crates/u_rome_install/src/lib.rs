//! A GTA VI install on disk: find its archives, scan them into the index, and
//! read entries back.
//!
//! This is the glue between `u_rome_rpf9` (carving) and `u_rome_index`
//! (storage) that both ROME Studio and the `rome` CLI use.
//!
//! The install root is stored in the index's sidecar file
//! (`<index>.root`), so commands that only get an index path can still
//! reopen archives.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

use thiserror::Error;
use u_rome_core::AssetKind;
use u_rome_index::{ArchiveId, EntryRow, Index, IndexEntry, NameRegistry, NameSource};
use u_rome_rpf9::{Archive, Found, Location, ScanOptions, ScanProgress};

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Index(#[from] u_rome_index::IndexError),
    #[error(transparent)]
    Rpf9(#[from] u_rome_rpf9::Rpf9Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, InstallError>;

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> InstallError + '_ {
    move |source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Files in an install that the scanner looks at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// An RPF9 archive (`.rpf`).
    Archive,
    /// A standalone texture database (`.ttpf`), indexed as one entry.
    TextureDb,
}

#[derive(Clone, Debug)]
pub struct InstallFile {
    pub rel_path: String,
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub kind: FileKind,
}

/// Walk an install folder for `.rpf` and `.ttpf` files, largest last so
/// quick wins show up first.
pub fn discover(root: &Path) -> Result<Vec<InstallFile>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = fs::read_dir(&dir).map_err(io_err(&dir))?;
        for ent in rd {
            let ent = ent.map_err(io_err(&dir))?;
            let path = ent.path();
            let ft = ent.file_type().map_err(io_err(&path))?;
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            let kind = match ext.as_deref() {
                Some("rpf") => FileKind::Archive,
                Some("ttpf") => FileKind::TextureDb,
                _ => continue,
            };
            let meta = ent.metadata().map_err(io_err(&path))?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(InstallFile {
                rel_path: rel,
                path,
                size: meta.len(),
                mtime,
                kind,
            });
        }
    }
    out.sort_by(|a, b| a.size.cmp(&b.size).then(a.rel_path.cmp(&b.rel_path)));
    Ok(out)
}

/// Default location for the index database.
pub fn default_index_path() -> PathBuf {
    data_dir().join("index.sqlite")
}

/// `%LOCALAPPDATA%\u_rome` on Windows, `~/Library/Application Support/u_rome`
/// on macOS, `$XDG_DATA_HOME/u_rome` elsewhere.
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    base.unwrap_or_else(|| PathBuf::from(".")).join("u_rome")
}

fn root_sidecar(index_path: &Path) -> PathBuf {
    let mut p = index_path.as_os_str().to_owned();
    p.push(".root");
    PathBuf::from(p)
}

/// Remember which install folder an index belongs to.
pub fn save_root(index_path: &Path, root: &Path) -> Result<()> {
    let side = root_sidecar(index_path);
    fs::write(&side, root.to_string_lossy().as_bytes()).map_err(io_err(&side))
}

/// The install folder recorded for an index, if any.
pub fn load_root(index_path: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(root_sidecar(index_path)).ok()?;
    let t = text.trim();
    (!t.is_empty()).then(|| PathBuf::from(t))
}

/// Progress events from [`scan_install`].
#[derive(Clone, Debug)]
pub enum ScanEvent {
    Discovered {
        files: usize,
        total_bytes: u64,
    },
    FileStarted {
        index: usize,
        rel_path: String,
        size: u64,
    },
    Progress {
        index: usize,
        progress: ScanProgress,
    },
    FileSkipped {
        index: usize,
        rel_path: String,
        reason: String,
    },
    FileDone {
        index: usize,
        rel_path: String,
        found: u64,
        seconds: f64,
    },
    FileFailed {
        index: usize,
        rel_path: String,
        error: String,
    },
    NamesImported {
        names: u64,
    },
}

#[derive(Clone, Debug)]
pub struct ScanSettings {
    pub hash: bool,
    pub threads: Option<usize>,
    pub max_entry_size: u64,
    /// Rescan files even when the index says they are done.
    pub force: bool,
    /// Extra hash/name lists to import (TSV `0xHASH<TAB>name` or one name per line).
    pub name_lists: Vec<PathBuf>,
    /// Harvest names from XML entries after scanning.
    pub harvest_xml_names: bool,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            hash: true,
            threads: None,
            max_entry_size: 512 << 20,
            force: false,
            name_lists: Vec::new(),
            harvest_xml_names: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanReport {
    pub files: usize,
    pub scanned: usize,
    pub skipped: usize,
    pub failed: usize,
    pub entries: u64,
    pub names: u64,
    pub cancelled: bool,
}

fn to_index_entry(f: &Found) -> IndexEntry {
    let mut e = IndexEntry::new(f.location.chain.clone());
    e.stored_len = f.stored_len;
    e.size = f.size;
    e.codec = match f.codec {
        u_rome_rpf9::Codec::Zlib => u_rome_index::Codec::Zlib,
        u_rome_rpf9::Codec::RawDeflate => u_rome_index::Codec::RawDeflate,
        u_rome_rpf9::Codec::Stored => u_rome_index::Codec::Stored,
    };
    e.pages = f.pages;
    e.kind = f.kind;
    e.head = f.head.clone();
    e.hash = f.hash;
    e.provisional = f.provisional;
    e
}

fn header_summary(archive: &Archive) -> Option<String> {
    let h = archive.header()?;
    Some(format!(
        "version {}.{}, generation {:?}, declared entries {}, key id 0x{:08X}, flags 0x{:08X}, payload start 0x{:X}, trailer {} bytes",
        h.version_major(),
        h.version_minor(),
        h.generation_number(),
        h.entry_count,
        h.key_id,
        h.flags,
        h.payload_start,
        h.trailer_len
    ))
}

/// Scan every archive under `root` into the index at `index_path`.
///
/// Files already fully scanned (same size and mtime) are skipped unless
/// `settings.force` is set. Cancelling leaves completed files intact; the
/// cancelled file is rescanned next time.
pub fn scan_install(
    root: &Path,
    index_path: &Path,
    settings: &ScanSettings,
    cancel: &AtomicBool,
    on_event: &(dyn Fn(ScanEvent) + Sync),
) -> Result<ScanReport> {
    if let Some(dir) = index_path.parent() {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    save_root(index_path, root)?;
    let mut index = Index::open(index_path)?;
    let files = discover(root)?;
    let mut report = ScanReport {
        files: files.len(),
        ..Default::default()
    };
    on_event(ScanEvent::Discovered {
        files: files.len(),
        total_bytes: files.iter().map(|f| f.size).sum(),
    });

    let opts = ScanOptions {
        hash: settings.hash,
        threads: settings.threads,
        recurse_nested: true,
        max_entry_size: settings.max_entry_size,
    };

    for (i, file) in files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let reg = index.register_archive(&file.rel_path, file.size, file.mtime)?;
        if !reg.needs_scan && !settings.force {
            report.skipped += 1;
            on_event(ScanEvent::FileSkipped {
                index: i,
                rel_path: file.rel_path.clone(),
                reason: "already indexed".into(),
            });
            continue;
        }
        if settings.force && !reg.needs_scan {
            index.remove_archive(reg.id)?;
        }
        let reg = index.register_archive(&file.rel_path, file.size, file.mtime)?;
        on_event(ScanEvent::FileStarted {
            index: i,
            rel_path: file.rel_path.clone(),
            size: file.size,
        });
        let started = Instant::now();
        let result = match file.kind {
            FileKind::Archive => scan_archive(&mut index, reg.id, file, &opts, cancel, i, on_event),
            FileKind::TextureDb => index_texture_db(&mut index, reg.id, file),
        };
        match result {
            Ok((found, cancelled)) => {
                report.entries += found;
                if cancelled {
                    report.cancelled = true;
                    break;
                }
                report.scanned += 1;
                on_event(ScanEvent::FileDone {
                    index: i,
                    rel_path: file.rel_path.clone(),
                    found,
                    seconds: started.elapsed().as_secs_f64(),
                });
            }
            Err(e) => {
                report.failed += 1;
                log::warn!("scan of {} failed: {e}", file.rel_path);
                on_event(ScanEvent::FileFailed {
                    index: i,
                    rel_path: file.rel_path.clone(),
                    error: e.to_string(),
                });
            }
        }
    }

    // Names: user-supplied lists, then XML manifests found in the data.
    let mut names = NameRegistry::new();
    for list in &settings.name_lists {
        let bytes = fs::read(list).map_err(io_err(list))?;
        names.load_list_bytes(&bytes, NameSource::HashList);
    }
    if settings.harvest_xml_names && !report.cancelled {
        harvest_xml_names(&index, root, &mut names, cancel);
    }
    if !names.is_empty() {
        report.names = index.import_names(&names)?;
        index.attach_names_by_hash()?;
        on_event(ScanEvent::NamesImported {
            names: report.names,
        });
    }
    Ok(report)
}

fn scan_archive(
    index: &mut Index,
    id: ArchiveId,
    file: &InstallFile,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    file_index: usize,
    on_event: &(dyn Fn(ScanEvent) + Sync),
) -> Result<(u64, bool)> {
    let archive = Archive::open(&file.path)?;
    let mut writer = index.writer(id)?;
    if let Some(summary) = header_summary(&archive) {
        writer.set_header(&summary)?;
    }
    let (tx, rx) = mpsc::sync_channel::<Found>(16_384);
    let mut deferred: Vec<IndexEntry> = Vec::new();
    let mut pushed = 0u64;

    let scan_result = std::thread::scope(|s| {
        let archive = &archive;
        let handle = s.spawn(move || {
            // The sender lives in this thread; when the scan returns it is
            // dropped, which ends the receiving loop below.
            let on_found = |f: Found| {
                // A send only fails when the receiver is gone.
                let _ = tx.send(f);
            };
            let on_progress = |p: ScanProgress| {
                on_event(ScanEvent::Progress {
                    index: file_index,
                    progress: p,
                })
            };
            u_rome_rpf9::scan(archive, opts, cancel, &on_found, &on_progress)
        });
        for f in rx.iter() {
            let entry = to_index_entry(&f);
            // Parents must exist before children: push top-level entries as
            // they arrive, nested ones after the scan, shallowest first.
            if entry.chain.len() > 1 {
                deferred.push(entry);
            } else if writer.push(&entry).is_ok() {
                pushed += 1;
            }
        }
        handle.join()
    });
    let summary = match scan_result {
        Ok(r) => r?,
        Err(_) => {
            return Err(InstallError::Other(format!(
                "scanner thread panicked on {}",
                file.rel_path
            )));
        }
    };
    deferred.sort_by_key(|e| e.chain.len());
    for e in &deferred {
        writer.push(e)?;
        pushed += 1;
    }
    if summary.cancelled {
        // Leave the archive pending so it is rescanned.
        drop(writer);
        index.remove_archive(id)?;
        return Ok((pushed, true));
    }
    writer.checkpoint(file.size)?;
    writer.finish()?;
    Ok((pushed, false))
}

fn index_texture_db(index: &mut Index, id: ArchiveId, file: &InstallFile) -> Result<(u64, bool)> {
    let mut head = vec![0u8; 64];
    let n = {
        use std::io::Read;
        let mut f = fs::File::open(&file.path).map_err(io_err(&file.path))?;
        f.read(&mut head).map_err(io_err(&file.path))?
    };
    head.truncate(n);
    let mut entry = IndexEntry::new(vec![0]);
    entry.stored_len = file.size;
    entry.size = file.size;
    entry.codec = u_rome_index::Codec::Stored;
    entry.pages = 1;
    entry.kind = match u_rome_core::detect(&head) {
        AssetKind::Unknown => AssetKind::TextureStore,
        k => k,
    };
    entry.head = head;
    let mut w = index.writer(id)?;
    w.push(&entry)?;
    w.checkpoint(file.size)?;
    w.finish()?;
    Ok((1, false))
}

fn harvest_xml_names(index: &Index, root: &Path, names: &mut NameRegistry, cancel: &AtomicBool) {
    let filters = u_rome_index::SearchFilters {
        kind: Some(AssetKind::Xml),
        archive: None,
    };
    let reader = EntryReader::new(root);
    let mut offset = 0u32;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let rows = match index.search("", &filters, 1000, offset) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("xml name harvest query failed: {e}");
                return;
            }
        };
        if rows.is_empty() {
            return;
        }
        for row in &rows {
            if row.size > 32 << 20 {
                continue;
            }
            match reader.read(index, row) {
                Ok(bytes) => {
                    names.add_xml(&bytes);
                }
                Err(e) => log::debug!("xml entry {} unreadable: {e}", row.id.0),
            }
        }
        offset += rows.len() as u32;
    }
}

/// Reads entries back from disk, keeping archives memory-mapped between calls.
pub struct EntryReader {
    root: PathBuf,
    open: Mutex<HashMap<String, Arc<Archive>>>,
    /// Largest entry it will decode.
    pub max_size: u64,
}

impl EntryReader {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            open: Mutex::new(HashMap::new()),
            max_size: 1 << 30,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn archive(&self, rel_path: &str) -> Result<Arc<Archive>> {
        let mut open = self
            .open
            .lock()
            .map_err(|_| InstallError::Other("archive cache poisoned".into()))?;
        if let Some(a) = open.get(rel_path) {
            return Ok(a.clone());
        }
        let path = self.root.join(rel_path);
        let a = Arc::new(Archive::open(&path)?);
        open.insert(rel_path.to_owned(), a.clone());
        Ok(a)
    }

    /// Decode an indexed entry.
    pub fn read(&self, index: &Index, row: &EntryRow) -> Result<Vec<u8>> {
        let arch = index.archive(row.archive_id)?.ok_or_else(|| {
            InstallError::Other(format!("archive {} missing from index", row.archive_id.0))
        })?;
        let a = self.archive(&arch.rel_path)?;
        let lower = arch.rel_path.to_ascii_lowercase();
        if lower.ends_with(".ttpf") {
            let bytes = a.bytes();
            let n = bytes.len().min(self.max_size as usize);
            return Ok(bytes[..n].to_vec());
        }
        Ok(u_rome_rpf9::read(
            &a,
            &Location {
                chain: row.chain.clone(),
            },
            self.max_size,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use u_rome_testdata::Rpf9Builder;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("u_rome_install_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scan_index_and_read_back() {
        let dir = temp_dir("scan");
        let game = dir.join("game");
        fs::create_dir_all(game.join("x64")).unwrap();
        let built = Rpf9Builder::sample(7).build();
        fs::write(game.join("x64/sample.rpf"), &built.bytes).unwrap();
        fs::write(game.join("tex.ttpf"), b"TTPF-not-really-a-texture-db").unwrap();
        fs::write(game.join("readme.txt"), b"ignored").unwrap();

        let index_path = dir.join("index.sqlite");
        let cancel = AtomicBool::new(false);
        let report = scan_install(
            &game,
            &index_path,
            &ScanSettings::default(),
            &cancel,
            &|_| {},
        )
        .unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.scanned, 2);
        assert_eq!(report.failed, 0);
        assert!(report.entries > 3, "found {}", report.entries);
        assert_eq!(load_root(&index_path).as_deref(), Some(game.as_path()));

        // Every indexed entry decodes to its recorded size.
        let index = Index::open(&index_path).unwrap();
        let reader = EntryReader::new(&game);
        let mut checked = 0;
        for arch in index.archives().unwrap() {
            for row in index.entries_in(arch.id, 0, 10_000).unwrap() {
                let bytes = reader.read(&index, &row).unwrap();
                if !row.provisional {
                    assert_eq!(bytes.len() as u64, row.size, "entry {:?}", row.chain);
                }
                checked += 1;
            }
        }
        assert!(checked as u64 >= report.entries);

        // Second run skips unchanged files.
        let again = scan_install(
            &game,
            &index_path,
            &ScanSettings::default(),
            &cancel,
            &|_| {},
        )
        .unwrap();
        assert_eq!(again.skipped, 2);
        assert_eq!(again.scanned, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelled_scan_leaves_file_pending() {
        let dir = temp_dir("cancel");
        let game = dir.join("game");
        fs::create_dir_all(&game).unwrap();
        fs::write(game.join("a.rpf"), Rpf9Builder::sample(3).build().bytes).unwrap();
        let index_path = dir.join("index.sqlite");
        let cancel = AtomicBool::new(true);
        let report = scan_install(
            &game,
            &index_path,
            &ScanSettings::default(),
            &cancel,
            &|_| {},
        )
        .unwrap();
        assert!(report.cancelled);
        let cancel = AtomicBool::new(false);
        let report = scan_install(
            &game,
            &index_path,
            &ScanSettings::default(),
            &cancel,
            &|_| {},
        )
        .unwrap();
        assert_eq!(report.scanned, 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
