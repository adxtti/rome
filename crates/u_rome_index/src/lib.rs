//! Persistent SQLite index of everything found in a game install, with name
//! recovery and search.
//!
//! # Model
//!
//! - **archives**: one row per file on disk, keyed by its path relative to the
//!   install root (always stored with `/` separators). Size and mtime detect
//!   changed files; scan state (`Pending`/`Partial`/`Done`) plus the last
//!   checkpointed offset make scans resumable.
//! - **entries**: everything a scanner found, mirroring `u_rome_rpf9` output
//!   ([`IndexEntry`]). Nested entries link to their parent, resolved from the
//!   location chain.
//! - **names**: recovered names with both JOAAT variants, searchable through
//!   an FTS5 trigram index (substring, case-insensitive).
//!
//! # Typical flow
//!
//! ```no_run
//! # use u_rome_index::*;
//! # fn scan() -> Result<()> {
//! let mut index = Index::open("studio-index.sqlite")?;
//! let reg = index.register_archive("x64/levels/gta6.rpf", 1_234_567, 1_700_000_000_000)?;
//! if reg.needs_scan {
//!     let mut w = index.writer(reg.id)?;
//!     let start = w.resume_offset();
//!     # let found: Vec<IndexEntry> = Vec::new();
//!     for (i, e) in found.iter().filter(|e| e.chain[0] >= start).enumerate() {
//!         w.push(e)?;
//!         if i % 4096 == 4095 {
//!             w.checkpoint(e.chain[0] + 1)?;
//!         }
//!     }
//!     w.finish()?;
//! }
//! let mut names = NameRegistry::new();
//! names.load_list("0xB779A091\tadder\n", NameSource::HashList);
//! index.import_names(&names)?;
//! index.attach_names_by_hash()?;
//! let hits = index.search("adder", &SearchFilters::default(), 100, 0)?;
//! # let _ = hits; Ok(()) }
//! ```
//!
//! # Threads and connections
//!
//! [`Index`] owns one SQLite connection; it is `Send` but not `Sync`. The
//! intended pattern for Studio is **one `Index` per thread on the same file**:
//!
//! - the background scan thread opens its own `Index` and writes through
//!   [`Writer`]s, checkpointing every few thousand entries;
//! - the UI thread opens a second `Index` and only queries.
//!
//! The database runs in WAL mode, so the UI keeps reading the last committed
//! state while a write transaction is open; it sees new entries after each
//! checkpoint. Only one writer can hold the write lock at a time; others wait
//! up to the busy timeout (5 s) before failing with `SQLITE_BUSY`. Do not
//! share one `Index` behind a mutex between UI and scanner: the scanner's
//! long transaction would block the UI. In-memory indexes
//! ([`Index::open_in_memory`]) are private to their connection and are meant
//! for tests.
//!
//! # Provisional data
//!
//! Hash-based names are guesses ([`NameMatch::provisional`]): sources disagree
//! on whether GTA VI lowercases names before JOAAT hashing (see
//! `u_rome_core::hash`), and 32-bit hashes collide. Entries carry the
//! scanner's own `provisional` flag through unchanged.

mod error;
mod names;
mod query;
mod schema;
mod types;
mod writer;
mod xml;

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use u_rome_core::{joaat, joaat_lower};

pub use error::{IndexError, Result};
pub use names::{LoadReport, MAX_NAME_LEN, MAX_REGISTRY_NAMES, NameRecord, NameRegistry};
pub use schema::SCHEMA_VERSION;
pub use types::{
    ALL_KINDS, ArchiveId, ArchiveRow, AttachReport, Codec, DuplicateGroup, DuplicateStats, EntryId,
    EntryRow, IndexEntry, KindStats, MAX_CHAIN_DEPTH, MAX_HEAD_LEN, NameId, NameMatch, NameRow,
    NameSource, Registration, ScanState, SearchFilters, Stats, kind_code, kind_from_code,
    kind_slug, parse_kind,
};
pub use writer::{WriteSummary, Writer};
pub use xml::names_from_xml;

use error::{Ctx, to_i64, to_u64};

/// How long a connection waits for another connection's write lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection to the index database. See the crate docs for threading.
pub struct Index {
    conn: Connection,
}

impl Index {
    /// Open (or create) the index at `path` and migrate it to the current schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let conn =
            Connection::open(path).ctx_with(|| format!("opening index file {}", path.display()))?;
        Self::init(conn)
    }

    /// A private in-memory index (tests, throwaway sessions).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().ctx("opening in-memory index")?;
        Self::init(conn)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.busy_timeout(BUSY_TIMEOUT)
            .ctx("setting busy timeout")?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .ctx("enabling WAL")?;
        if !mode.eq_ignore_ascii_case("wal") && !mode.eq_ignore_ascii_case("memory") {
            log::warn!("index: journal_mode is {mode:?}, not WAL; concurrent reads may block");
        }
        conn.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -32768;",
        )
        .ctx("setting connection pragmas")?;
        schema::migrate(&mut conn)?;
        Ok(Index { conn })
    }

    /// Schema version of the open database.
    pub fn schema_version(&self) -> Result<i64> {
        schema::version(&self.conn)
    }

    // ---------------------------------------------------------------- archives

    /// Record an archive file and decide whether it needs (re)scanning.
    ///
    /// `rel_path` is relative to the install root; `\` is normalised to `/`.
    /// `mtime` is any stable modification-time value (Studio uses Unix
    /// milliseconds); it is only compared for equality. If size or mtime
    /// differ from the stored values, the archive's entries are deleted and
    /// it is reset to `Pending`.
    pub fn register_archive(
        &mut self,
        rel_path: &str,
        size: u64,
        mtime: i64,
    ) -> Result<Registration> {
        let path = normalize_path(rel_path)?;
        let size_i = to_i64(size, "archive size")?;
        let tx = self
            .conn
            .transaction()
            .ctx_with(|| format!("registering archive {path}"))?;
        let existing: Option<(i64, i64, i64, i64, i64)> = tx
            .query_row(
                "SELECT id, size, mtime, scan_state, last_offset FROM archives WHERE rel_path = ?1",
                [&path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .ctx_with(|| format!("looking up archive {path}"))?;
        let reg = match existing {
            None => {
                tx.execute(
                    "INSERT INTO archives (rel_path, size, mtime, scan_state, updated_at) \
                     VALUES (?1, ?2, ?3, 0, strftime('%s','now'))",
                    params![path, size_i, mtime],
                )
                .ctx_with(|| format!("inserting archive {path}"))?;
                Registration {
                    id: ArchiveId(tx.last_insert_rowid()),
                    needs_scan: true,
                    resume_offset: 0,
                    changed: false,
                }
            }
            Some((id, old_size, old_mtime, _, _)) if old_size != size_i || old_mtime != mtime => {
                log::debug!(
                    "index: archive {path} changed (size {old_size} -> {size_i}, \
                     mtime {old_mtime} -> {mtime}); invalidating"
                );
                tx.execute("DELETE FROM entries WHERE archive_id = ?1", [id])
                    .ctx_with(|| format!("clearing entries of changed archive {path}"))?;
                tx.execute(
                    "UPDATE archives SET size = ?2, mtime = ?3, header = NULL, scan_state = 0, \
                     last_offset = 0, entry_count = 0, updated_at = strftime('%s','now') \
                     WHERE id = ?1",
                    params![id, size_i, mtime],
                )
                .ctx_with(|| format!("resetting changed archive {path}"))?;
                Registration {
                    id: ArchiveId(id),
                    needs_scan: true,
                    resume_offset: 0,
                    changed: true,
                }
            }
            Some((id, _, _, state, last)) => {
                let state = ScanState::from_code(state).ok_or_else(|| IndexError::Corrupt {
                    table: "archives",
                    id,
                    detail: format!("scan_state {state}"),
                })?;
                Registration {
                    id: ArchiveId(id),
                    needs_scan: state != ScanState::Done,
                    resume_offset: if state == ScanState::Partial {
                        to_u64(last, "archives", id, "last_offset")?
                    } else {
                        0
                    },
                    changed: false,
                }
            }
        };
        tx.commit()
            .ctx_with(|| format!("committing registration of {path}"))?;
        Ok(reg)
    }

    /// Start (or resume) writing entries for an archive. See [`Writer`].
    pub fn writer(&mut self, archive: ArchiveId) -> Result<Writer<'_>> {
        Writer::new(&self.conn, archive)
    }

    /// Set an archive's header summary (free text, e.g. "RPF9 v2, 1234 entries").
    pub fn set_archive_header(&mut self, archive: ArchiveId, summary: &str) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE archives SET header = ?2 WHERE id = ?1",
                params![archive.0, summary],
            )
            .ctx_with(|| format!("setting header of archive {}", archive.0))?;
        if n == 0 {
            return Err(IndexError::ArchiveNotFound(archive.0));
        }
        Ok(())
    }

    /// Delete an archive and all its entries (e.g. the file disappeared).
    pub fn remove_archive(&mut self, archive: ArchiveId) -> Result<()> {
        let tx = self
            .conn
            .transaction()
            .ctx_with(|| format!("removing archive {}", archive.0))?;
        tx.execute("DELETE FROM entries WHERE archive_id = ?1", [archive.0])
            .ctx_with(|| format!("removing entries of archive {}", archive.0))?;
        let n = tx
            .execute("DELETE FROM archives WHERE id = ?1", [archive.0])
            .ctx_with(|| format!("removing archive {}", archive.0))?;
        if n == 0 {
            return Err(IndexError::ArchiveNotFound(archive.0));
        }
        tx.commit()
            .ctx_with(|| format!("committing removal of archive {}", archive.0))
    }

    // ------------------------------------------------------------------- names

    /// Add one name (or return the existing id).
    pub fn add_name(&mut self, name: &str, source: NameSource) -> Result<NameId> {
        upsert_name(&self.conn, name, None, source)
    }

    /// Persist every name in `registry`. Returns how many were new.
    pub fn import_names(&mut self, registry: &NameRegistry) -> Result<u64> {
        let tx = self.conn.transaction().ctx("starting name import")?;
        let before = count(&tx, "SELECT COUNT(*) FROM names")?;
        for rec in registry.iter() {
            upsert_name(&tx, &rec.name, rec.listed_hash, rec.source)?;
        }
        let after = count(&tx, "SELECT COUNT(*) FROM names")?;
        tx.commit().ctx("committing name import")?;
        let added = after.saturating_sub(before);
        log::debug!("index: imported {} names ({added} new)", registry.len());
        Ok(added)
    }

    /// Give an entry an explicit, authoritative name.
    pub fn attach_name(
        &mut self,
        entry: EntryId,
        name: &str,
        source: NameSource,
    ) -> Result<NameId> {
        let tx = self
            .conn
            .transaction()
            .ctx_with(|| format!("naming entry {}", entry.0))?;
        let id = upsert_name(&tx, name, None, source)?;
        let n = tx
            .execute(
                "UPDATE entries SET name_id = ?2, name_match = ?3 WHERE id = ?1",
                params![entry.0, id.0, NameMatch::Explicit.code()],
            )
            .ctx_with(|| format!("naming entry {}", entry.0))?;
        if n == 0 {
            return Err(IndexError::EntryNotFound(entry.0));
        }
        tx.commit()
            .ctx_with(|| format!("committing name of entry {}", entry.0))?;
        Ok(id)
    }

    /// Set (or clear) the embedded name hash of an entry, e.g. after a decoder
    /// read it from the resource header.
    pub fn set_name_hash(&mut self, entry: EntryId, hash: Option<u32>) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE entries SET name_hash = ?2 WHERE id = ?1",
                params![entry.0, hash.map(i64::from)],
            )
            .ctx_with(|| format!("setting name hash of entry {}", entry.0))?;
        if n == 0 {
            return Err(IndexError::EntryNotFound(entry.0));
        }
        Ok(())
    }

    /// Name every unnamed entry whose `name_hash` matches a known name.
    ///
    /// Tried in order: lowercase JOAAT (GTA V convention), case-sensitive
    /// JOAAT, then unverified listed hashes. On collisions the oldest name
    /// wins; [`Index::names_for_hash`] lists the alternatives. Explicit names
    /// are never overwritten. Safe to rerun after importing more names.
    pub fn attach_names_by_hash(&mut self) -> Result<AttachReport> {
        let tx = self.conn.transaction().ctx("starting name attach")?;
        let run = |column: &str, m: NameMatch| -> Result<u64> {
            let sql = format!(
                "UPDATE entries SET \
                   name_id = (SELECT n.id FROM names n WHERE n.{column} = entries.name_hash \
                              ORDER BY n.id LIMIT 1), \
                   name_match = ?1 \
                 WHERE name_id IS NULL AND name_hash IS NOT NULL \
                   AND EXISTS (SELECT 1 FROM names n WHERE n.{column} = entries.name_hash)"
            );
            let n = tx
                .execute(&sql, [m.code()])
                .ctx_with(|| format!("attaching names by {column}"))?;
            Ok(n as u64)
        };
        let report = AttachReport {
            joaat_lower: run("joaat_lower", NameMatch::JoaatLower)?,
            joaat: run("joaat", NameMatch::Joaat)?,
            listed_hash: run("alt_hash", NameMatch::ListedHash)?,
        };
        tx.commit().ctx("committing name attach")?;
        log::debug!("index: attached names by hash: {report:?}");
        Ok(report)
    }

    /// Run SQLite's planner statistics and merge FTS segments. Call after a
    /// large scan; cheap when nothing changed.
    pub fn optimize(&mut self) -> Result<()> {
        self.conn
            .execute_batch("INSERT INTO names_fts(names_fts) VALUES ('optimize'); PRAGMA optimize;")
            .ctx("optimizing index")
    }
}

/// Insert a name or return the existing id; records an unverified listed hash.
fn upsert_name(
    conn: &Connection,
    name: &str,
    listed_hash: Option<u32>,
    source: NameSource,
) -> Result<NameId> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(IndexError::Invalid(format!(
            "name of {} bytes (must be 1..={MAX_NAME_LEN})",
            name.len()
        )));
    }
    let j = joaat(name).0;
    let jl = joaat_lower(name).0;
    let listed = listed_hash.filter(|h| *h != j && *h != jl);
    let mut stmt = conn
        .prepare_cached(
            "INSERT INTO names (name, joaat, joaat_lower, alt_hash, source) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(name) DO UPDATE SET alt_hash = COALESCE(names.alt_hash, excluded.alt_hash) \
             RETURNING id",
        )
        .ctx("preparing name upsert")?;
    stmt.query_row(
        params![
            name,
            i64::from(j),
            i64::from(jl),
            listed.map(i64::from),
            source.code()
        ],
        |r| r.get(0),
    )
    .map(NameId)
    .ctx_with(|| format!("upserting name {name:?}"))
}

fn count(conn: &Connection, sql: &str) -> Result<u64> {
    let n: i64 = conn
        .query_row(sql, [], |r| r.get(0))
        .ctx_with(|| format!("counting: {sql}"))?;
    Ok(u64::try_from(n).unwrap_or(0))
}

fn normalize_path(p: &str) -> Result<String> {
    let p = p.replace('\\', "/");
    let p = p.trim_start_matches("./").to_owned();
    if p.is_empty() || p.len() > 4096 {
        return Err(IndexError::Invalid(format!(
            "archive path of {} bytes (must be 1..=4096)",
            p.len()
        )));
    }
    Ok(p)
}
