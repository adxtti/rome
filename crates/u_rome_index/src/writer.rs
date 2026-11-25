//! Batched, resumable entry writer for one archive.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Statement, params};
use u_rome_core::AssetKind;

use crate::error::{Ctx, IndexError, Result, to_i64, to_u64};
use crate::types::{
    ArchiveId, EntryId, IndexEntry, MAX_CHAIN_DEPTH, MAX_HEAD_LEN, ScanState, encode_chain,
};

/// Parent lookups cached per writer before the cache is cleared.
const PARENT_CACHE_MAX: usize = 65_536;

/// Summary returned by [`Writer::finish`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteSummary {
    /// Entries inserted by this writer.
    pub pushed: u64,
    /// Entries whose parent chain was not found (stored with no parent).
    pub orphans: u64,
    /// Total entries the archive now has in the index.
    pub entry_count: u64,
}

/// Writes the entries of one archive inside a single open transaction.
///
/// - [`Writer::push`] inserts into the open transaction (no fsync per row).
/// - [`Writer::checkpoint`] records progress and commits. After a crash or
///   cancellation the scan resumes from the last checkpointed offset.
/// - [`Writer::finish`] marks the archive `Done` and commits.
/// - Dropping a writer without `finish` rolls back everything since the last
///   checkpoint.
///
/// Checkpoint contract: `checkpoint(off)` promises that every entry whose
/// `chain[0] < off` has been pushed. A resumed writer deletes any committed
/// entries with `chain[0] >= off` so the scanner can simply re-emit them.
///
/// The transaction is `BEGIN IMMEDIATE`, so other connections can keep
/// reading (WAL) while it is open, but a second writer waits on
/// `busy_timeout`. Checkpoint every few thousand entries to keep the WAL
/// small and progress durable.
pub struct Writer<'a> {
    conn: &'a Connection,
    archive: ArchiveId,
    resume_offset: u64,
    last_checkpoint: u64,
    insert: Statement<'a>,
    find_chain: Statement<'a>,
    parents: HashMap<Vec<u64>, EntryId>,
    entry_count: u64,
    pushed: u64,
    orphans: u64,
    in_tx: bool,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(conn: &'a Connection, archive: ArchiveId) -> Result<Self> {
        let insert = conn
            .prepare(
                "INSERT INTO entries (archive_id, parent_id, chain, top_offset, depth, \
                 stored_len, size, codec, pages, kind, head, hash, provisional, name_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )
            .ctx("preparing entry insert")?;
        let find_chain = conn
            .prepare(
                "SELECT id FROM entries WHERE archive_id = ?1 AND top_offset = ?2 AND chain = ?3 \
                 ORDER BY id LIMIT 1",
            )
            .ctx("preparing chain lookup")?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .ctx_with(|| format!("starting write transaction for archive {}", archive.0))?;
        let mut w = Writer {
            conn,
            archive,
            resume_offset: 0,
            last_checkpoint: 0,
            insert,
            find_chain,
            parents: HashMap::new(),
            entry_count: 0,
            pushed: 0,
            orphans: 0,
            in_tx: true,
        };
        // On error, `w` drops here and rolls back.
        w.prepare_resume()?;
        Ok(w)
    }

    fn prepare_resume(&mut self) -> Result<()> {
        let id = self.archive.0;
        let (state, last): (i64, i64) = self
            .conn
            .query_row(
                "SELECT scan_state, last_offset FROM archives WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ctx_with(|| format!("reading scan state of archive {id}"))?
            .ok_or(IndexError::ArchiveNotFound(id))?;
        let state = ScanState::from_code(state).ok_or_else(|| IndexError::Corrupt {
            table: "archives",
            id,
            detail: format!("scan_state {state}"),
        })?;
        self.resume_offset = match state {
            ScanState::Partial => to_u64(last, "archives", id, "last_offset")?,
            ScanState::Pending | ScanState::Done => 0,
        };
        self.last_checkpoint = self.resume_offset;
        let resume = to_i64(self.resume_offset, "resume offset")?;
        let removed = self
            .conn
            .execute(
                "DELETE FROM entries WHERE archive_id = ?1 AND top_offset >= ?2",
                params![id, resume],
            )
            .ctx_with(|| format!("clearing entries of archive {id} from offset {resume:#x}"))?;
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE archive_id = ?1",
                [id],
                |r| r.get(0),
            )
            .ctx_with(|| format!("counting entries of archive {id}"))?;
        self.entry_count = to_u64(count, "archives", id, "entry count")?;
        self.conn
            .execute(
                "UPDATE archives SET scan_state = ?2, last_offset = ?3, entry_count = ?4, \
                 updated_at = strftime('%s','now') WHERE id = ?1",
                params![id, ScanState::Partial.code(), resume, count],
            )
            .ctx_with(|| format!("marking archive {id} partial"))?;
        log::debug!(
            "index: writer for archive {id} ({state:?}) resumes at {resume:#x}, \
             {count} entries kept, {removed} removed"
        );
        Ok(())
    }

    /// Archive this writer fills.
    pub fn archive(&self) -> ArchiveId {
        self.archive
    }

    /// Top-level offset the scanner should resume from. Entries before it are
    /// already in the index.
    pub fn resume_offset(&self) -> u64 {
        self.resume_offset
    }

    /// Entries pushed so far by this writer.
    pub fn pushed(&self) -> u64 {
        self.pushed
    }

    /// Insert one entry and return its id.
    ///
    /// The parent is resolved from `chain[..len - 1]`; if it has not been
    /// pushed the entry is stored without a parent and counted as an orphan.
    /// Pushing an entry that a resumed scan already committed (its
    /// `chain[0] < resume_offset`) returns the existing id instead of
    /// inserting a duplicate.
    pub fn push(&mut self, entry: &IndexEntry) -> Result<EntryId> {
        let Some(&top) = entry.chain.first() else {
            return Err(IndexError::Invalid(format!(
                "archive {}: entry with empty location chain",
                self.archive.0
            )));
        };
        if entry.chain.len() > MAX_CHAIN_DEPTH {
            return Err(IndexError::Invalid(format!(
                "archive {}: entry at {top:#x} nests {} levels deep (max {MAX_CHAIN_DEPTH})",
                self.archive.0,
                entry.chain.len()
            )));
        }
        if !self.in_tx {
            return Err(IndexError::Invalid("writer already finished".into()));
        }
        let chain_blob = encode_chain(&entry.chain);
        if top < self.resume_offset
            && let Some(id) = self.lookup_chain(top, &chain_blob)?
        {
            log::debug!(
                "index: archive {} entry {:?} already indexed as {}",
                self.archive.0,
                entry.chain,
                id.0
            );
            return Ok(id);
        }
        let parent = match entry.chain.split_last() {
            Some((_, prefix)) if !prefix.is_empty() => {
                let p = self.resolve_parent(prefix)?;
                if p.is_none() {
                    self.orphans += 1;
                    log::debug!(
                        "index: archive {} entry {:?} has no indexed parent",
                        self.archive.0,
                        entry.chain
                    );
                }
                p
            }
            _ => None,
        };
        let head = entry.head.get(..MAX_HEAD_LEN).unwrap_or(&entry.head);
        let archive = self.archive.0;
        let conv = |v: u64, what: &str| {
            i64::try_from(v).map_err(|_| {
                IndexError::Invalid(format!(
                    "archive {archive} entry {:?}: {what} {v:#x} exceeds i64::MAX",
                    entry.chain
                ))
            })
        };
        let top_i = conv(top, "top offset")?;
        let stored_i = conv(entry.stored_len, "stored_len")?;
        let size_i = conv(entry.size, "size")?;
        let row = self
            .insert
            .insert(params![
                archive,
                parent.map(|p| p.0),
                chain_blob,
                top_i,
                entry.chain.len() as i64,
                stored_i,
                size_i,
                entry.codec.code(),
                i64::from(entry.pages),
                crate::types::kind_code(entry.kind),
                head,
                entry.hash.as_ref().map(|h| &h[..]),
                entry.provisional,
                entry.name_hash.map(i64::from),
            ])
            .ctx_with(|| format!("archive {archive} entry {:?}: inserting", entry.chain))?;
        let id = EntryId(row);
        if entry.kind == AssetKind::Archive {
            self.remember(entry.chain.clone(), id);
        }
        self.pushed += 1;
        self.entry_count += 1;
        Ok(id)
    }

    fn remember(&mut self, chain: Vec<u64>, id: EntryId) {
        if self.parents.len() >= PARENT_CACHE_MAX {
            self.parents.clear();
        }
        self.parents.insert(chain, id);
    }

    fn lookup_chain(&mut self, top: u64, blob: &[u8]) -> Result<Option<EntryId>> {
        let top = to_i64(top, "top offset")?;
        self.find_chain
            .query_row(params![self.archive.0, top, blob], |r| r.get(0))
            .optional()
            .ctx("looking up entry by chain")
            .map(|o| o.map(EntryId))
    }

    fn resolve_parent(&mut self, chain: &[u64]) -> Result<Option<EntryId>> {
        if let Some(&id) = self.parents.get(chain) {
            return Ok(Some(id));
        }
        let Some(&top) = chain.first() else {
            return Ok(None);
        };
        let found = self.lookup_chain(top, &encode_chain(chain))?;
        if let Some(id) = found {
            self.remember(chain.to_vec(), id);
        }
        Ok(found)
    }

    /// Set the archive's header summary (free text shown in Studio).
    pub fn set_header(&mut self, summary: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE archives SET header = ?2 WHERE id = ?1",
                params![self.archive.0, summary],
            )
            .ctx_with(|| format!("setting header of archive {}", self.archive.0))?;
        Ok(())
    }

    /// Record that every entry with `chain[0] < last_offset` has been pushed,
    /// commit, and start a new transaction.
    pub fn checkpoint(&mut self, last_offset: u64) -> Result<()> {
        self.commit_state(ScanState::Partial, last_offset)?;
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .ctx_with(|| format!("restarting transaction for archive {}", self.archive.0))?;
        self.in_tx = true;
        Ok(())
    }

    /// Mark the archive fully scanned and commit.
    pub fn finish(mut self) -> Result<WriteSummary> {
        let last = self.last_checkpoint;
        self.commit_state(ScanState::Done, last)?;
        log::debug!(
            "index: archive {} done, {} pushed, {} orphans, {} total",
            self.archive.0,
            self.pushed,
            self.orphans,
            self.entry_count
        );
        Ok(WriteSummary {
            pushed: self.pushed,
            orphans: self.orphans,
            entry_count: self.entry_count,
        })
    }

    fn commit_state(&mut self, state: ScanState, last_offset: u64) -> Result<()> {
        if !self.in_tx {
            return Err(IndexError::Invalid("writer already finished".into()));
        }
        let id = self.archive.0;
        let last = to_i64(last_offset, "checkpoint offset")?;
        let count = to_i64(self.entry_count, "entry count")?;
        self.conn
            .execute(
                "UPDATE archives SET scan_state = ?2, last_offset = ?3, entry_count = ?4, \
                 updated_at = strftime('%s','now') WHERE id = ?1",
                params![id, state.code(), last, count],
            )
            .ctx_with(|| format!("recording progress of archive {id} at {last:#x}"))?;
        self.conn
            .execute_batch("COMMIT")
            .ctx_with(|| format!("committing archive {id} at {last:#x}"))?;
        self.in_tx = false;
        self.last_checkpoint = last_offset;
        Ok(())
    }
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        if self.in_tx {
            log::debug!(
                "index: writer for archive {} dropped without finish; rolling back to last checkpoint",
                self.archive.0
            );
            if let Err(e) = self.conn.execute_batch("ROLLBACK") {
                log::warn!("index: rollback for archive {} failed: {e}", self.archive.0);
            }
        }
    }
}
