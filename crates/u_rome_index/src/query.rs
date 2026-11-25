//! Read-only queries.

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Row, params, params_from_iter};
use u_rome_core::AssetKind;

use crate::error::{Ctx, IndexError, Result, to_u64};
use crate::types::{
    ArchiveId, ArchiveRow, Codec, DuplicateGroup, DuplicateStats, EntryId, EntryRow, KindStats,
    NameId, NameMatch, NameRow, NameSource, ScanState, SearchFilters, Stats, decode_chain,
    kind_code, kind_from_code, parse_kind,
};
use crate::{Index, MAX_HEAD_LEN};

/// Largest page any listing query returns.
pub const MAX_PAGE: u32 = 10_000;

const ENTRY_SELECT: &str = "SELECT e.id, e.archive_id, e.parent_id, e.chain, e.top_offset, \
     e.stored_len, e.size, e.codec, e.pages, e.kind, e.head, e.hash, e.provisional, \
     e.name_hash, e.name_id, n.name, e.name_match \
     FROM entries e LEFT JOIN names n ON n.id = e.name_id";

const ARCHIVE_SELECT: &str = "SELECT id, rel_path, size, mtime, header, scan_state, \
     last_offset, entry_count FROM archives";

/// Raw column values; validated by [`RawEntry::into_row`].
struct RawEntry {
    id: i64,
    archive_id: i64,
    parent_id: Option<i64>,
    chain: Vec<u8>,
    top_offset: i64,
    stored_len: i64,
    size: i64,
    codec: i64,
    pages: i64,
    kind: i64,
    head: Vec<u8>,
    hash: Option<Vec<u8>>,
    provisional: bool,
    name_hash: Option<i64>,
    name_id: Option<i64>,
    name: Option<String>,
    name_match: Option<i64>,
}

impl RawEntry {
    fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(RawEntry {
            id: r.get(0)?,
            archive_id: r.get(1)?,
            parent_id: r.get(2)?,
            chain: r.get(3)?,
            top_offset: r.get(4)?,
            stored_len: r.get(5)?,
            size: r.get(6)?,
            codec: r.get(7)?,
            pages: r.get(8)?,
            kind: r.get(9)?,
            head: r.get(10)?,
            hash: r.get(11)?,
            provisional: r.get(12)?,
            name_hash: r.get(13)?,
            name_id: r.get(14)?,
            name: r.get(15)?,
            name_match: r.get(16)?,
        })
    }

    fn into_row(self) -> Result<EntryRow> {
        let id = self.id;
        let corrupt = |detail: String| IndexError::Corrupt {
            table: "entries",
            id,
            detail,
        };
        let chain = decode_chain(&self.chain)
            .ok_or_else(|| corrupt(format!("chain blob of {} bytes", self.chain.len())))?;
        let hash = match self.hash {
            None => None,
            Some(h) => Some(
                <[u8; 32]>::try_from(h.as_slice())
                    .map_err(|_| corrupt(format!("hash of {} bytes", h.len())))?,
            ),
        };
        let codec = Codec::from_code(self.codec).unwrap_or_else(|| {
            log::warn!("index: entry {id} has unknown codec code {}", self.codec);
            Codec::Other(u8::MAX)
        });
        let mut head = self.head;
        head.truncate(MAX_HEAD_LEN);
        Ok(EntryRow {
            id: EntryId(id),
            archive_id: ArchiveId(self.archive_id),
            parent_id: self.parent_id.map(EntryId),
            chain,
            top_offset: to_u64(self.top_offset, "entries", id, "top_offset")?,
            stored_len: to_u64(self.stored_len, "entries", id, "stored_len")?,
            size: to_u64(self.size, "entries", id, "size")?,
            codec,
            pages: u32::try_from(self.pages)
                .map_err(|_| corrupt(format!("pages {}", self.pages)))?,
            kind: kind_from_code(self.kind),
            head,
            hash,
            provisional: self.provisional,
            name_hash: self.name_hash.and_then(|h| u32::try_from(h).ok()),
            name_id: self.name_id.map(NameId),
            name: self.name,
            name_match: self.name_match.and_then(NameMatch::from_code),
        })
    }
}

fn archive_from_row(r: &Row<'_>) -> rusqlite::Result<RawArchive> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
    ))
}

type RawArchive = (i64, String, i64, i64, Option<String>, i64, i64, i64);

fn archive_into_row(raw: RawArchive) -> Result<ArchiveRow> {
    let (id, rel_path, size, mtime, header, state, last, count) = raw;
    Ok(ArchiveRow {
        id: ArchiveId(id),
        rel_path,
        size: to_u64(size, "archives", id, "size")?,
        mtime,
        header,
        state: ScanState::from_code(state).ok_or_else(|| IndexError::Corrupt {
            table: "archives",
            id,
            detail: format!("scan_state {state}"),
        })?,
        last_offset: to_u64(last, "archives", id, "last_offset")?,
        entry_count: to_u64(count, "archives", id, "entry_count")?,
    })
}

/// What the free-text part of a search query means.
#[derive(Debug, PartialEq, Eq)]
enum Text {
    None,
    Names(Vec<String>),
    Hex(String),
}

/// Parsed search query.
#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    kind: Option<AssetKind>,
    archive: Option<i64>,
    text: Text,
}

fn parse_query(query: &str) -> Result<Parsed> {
    let mut kind = None;
    let mut archive = None;
    let mut rest: Vec<&str> = Vec::new();
    for tok in query.split_whitespace() {
        if let Some(k) = tok.strip_prefix("kind:") {
            kind = Some(
                parse_kind(k).ok_or_else(|| IndexError::Invalid(format!("unknown kind {k:?}")))?,
            );
        } else if let Some(a) = tok.strip_prefix("archive:") {
            archive = Some(
                a.parse::<i64>()
                    .map_err(|_| IndexError::Invalid(format!("bad archive id {a:?}")))?,
            );
        } else {
            rest.push(tok);
        }
    }
    let joined = rest.join(" ");
    let text = if joined.is_empty() {
        Text::None
    } else if joined.len() >= 2 && joined.starts_with('"') && joined.ends_with('"') {
        // Quoted: always a name search, even for "audio" or "0x1234".
        let inner = joined.get(1..joined.len() - 1).unwrap_or("");
        let terms: Vec<String> = inner.split_whitespace().map(str::to_owned).collect();
        if terms.is_empty() {
            Text::None
        } else {
            Text::Names(terms)
        }
    } else if let Some(hex) = rest
        .first()
        .filter(|_| rest.len() == 1)
        .and_then(|t| t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")))
        .filter(|h| (1..=64).contains(&h.len()) && h.chars().all(|c| c.is_ascii_hexdigit()))
    {
        Text::Hex(hex.to_ascii_lowercase())
    } else if let Some(k) = parse_kind(&joined) {
        if kind.is_some_and(|q| q != k) {
            return Err(IndexError::Invalid(format!(
                "query names two kinds: {joined:?} and kind:{}",
                crate::kind_slug(kind.unwrap_or(k))
            )));
        }
        kind = Some(k);
        Text::None
    } else {
        Text::Names(rest.iter().map(|s| (*s).to_owned()).collect())
    };
    Ok(Parsed {
        kind,
        archive,
        text,
    })
}

/// Lower and exclusive upper bound for blobs starting with the hex nibbles.
fn hex_prefix_range(hex: &str) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let nibbles: Vec<u8> = hex
        .chars()
        .map(|c| c.to_digit(16).and_then(|d| u8::try_from(d).ok()))
        .collect::<Option<_>>()?;
    let pack = |n: &[u8]| -> Vec<u8> {
        n.chunks(2)
            .map(|p| {
                let hi = p.first().copied().unwrap_or(0);
                let lo = p.get(1).copied().unwrap_or(0);
                (hi << 4) | lo
            })
            .collect()
    };
    let lo = pack(&nibbles);
    // Increment the nibble string; on overflow there is no upper bound.
    let mut up = nibbles.clone();
    let mut carried = true;
    for n in up.iter_mut().rev() {
        if *n == 15 {
            *n = 0;
        } else {
            *n += 1;
            carried = false;
            break;
        }
    }
    let hi = if carried { None } else { Some(pack(&up)) };
    Some((lo, hi))
}

/// Quote a term for FTS5 (`"` doubled inside a string).
fn fts_quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// Escape `%`, `_` and `\` for `LIKE ... ESCAPE '\'`.
fn like_pattern(term: &str) -> String {
    let mut s = String::with_capacity(term.len() + 2);
    s.push('%');
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

/// SQL that selects name ids matching every term (substring, case-insensitive).
fn name_match_sql(terms: &[String], args: &mut Vec<Value>) -> String {
    // The trigram tokenizer needs 3+ characters per term; shorter terms fall
    // back to a LIKE scan over names.
    if terms.iter().all(|t| t.chars().count() >= 3) {
        let expr: Vec<String> = terms.iter().map(|t| fts_quote(t)).collect();
        args.push(Value::Text(expr.join(" AND ")));
        "SELECT rowid FROM names_fts WHERE names_fts MATCH ?".to_owned()
    } else {
        let mut conds = Vec::new();
        for t in terms {
            args.push(Value::Text(like_pattern(t)));
            conds.push("name LIKE ? ESCAPE '\\'");
        }
        format!("SELECT id FROM names WHERE {}", conds.join(" AND "))
    }
}

fn page(limit: u32, offset: u32) -> (i64, i64) {
    (i64::from(limit.min(MAX_PAGE)), i64::from(offset))
}

impl Index {
    /// Search entries.
    ///
    /// Query syntax (tokens separated by whitespace):
    /// - `kind:<slug>` / `archive:<id>`: filters, combined with `filters`.
    /// - `0x<1..64 hex digits>` (alone): entries whose embedded name hash, or
    ///   any name with that JOAAT (either variant) or listed hash, equals the
    ///   value (up to 8 digits), plus entries whose BLAKE3 hash starts with
    ///   those digits.
    /// - a kind slug or label alone (`audio`, `Texture store`): all entries of
    ///   that kind.
    /// - anything else: entries whose name contains every term
    ///   (case-insensitive substring). Wrap in double quotes to force a name
    ///   search for words that would otherwise be a kind or hash.
    /// - empty: all entries matching the filters.
    ///
    /// Results are ordered by entry id; `limit` is capped at 10 000.
    pub fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<EntryRow>> {
        let parsed = parse_query(query)?;
        let mut conds: Vec<String> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        for k in [filters.kind, parsed.kind].into_iter().flatten() {
            conds.push("e.kind = ?".into());
            args.push(Value::Integer(kind_code(k)));
        }
        for a in [filters.archive.map(|a| a.0), parsed.archive]
            .into_iter()
            .flatten()
        {
            conds.push("e.archive_id = ?".into());
            args.push(Value::Integer(a));
        }
        match &parsed.text {
            Text::None => {}
            Text::Names(terms) => {
                let sub = name_match_sql(terms, &mut args);
                conds.push(format!("e.name_id IN ({sub})"));
            }
            Text::Hex(hex) => {
                // Each branch uses its own index; UNION keeps that true.
                let mut branches = Vec::new();
                if hex.len() <= 8
                    && let Ok(h) = u32::from_str_radix(hex, 16)
                {
                    let h = i64::from(h);
                    branches.push("SELECT id FROM entries WHERE name_hash = ?".to_owned());
                    args.push(Value::Integer(h));
                    branches.push(
                        "SELECT e2.id FROM entries e2 WHERE e2.name_id IN \
                         (SELECT id FROM names WHERE joaat = ? \
                          UNION SELECT id FROM names WHERE joaat_lower = ? \
                          UNION SELECT id FROM names WHERE alt_hash = ?)"
                            .to_owned(),
                    );
                    args.extend([Value::Integer(h), Value::Integer(h), Value::Integer(h)]);
                }
                if let Some((lo, hi)) = hex_prefix_range(hex) {
                    match hi {
                        Some(hi) => {
                            branches.push(
                                "SELECT id FROM entries WHERE hash >= ? AND hash < ?".to_owned(),
                            );
                            args.push(Value::Blob(lo));
                            args.push(Value::Blob(hi));
                        }
                        None => {
                            branches.push("SELECT id FROM entries WHERE hash >= ?".to_owned());
                            args.push(Value::Blob(lo));
                        }
                    }
                }
                conds.push(format!("e.id IN ({})", branches.join(" UNION ")));
            }
        }
        let (limit, offset) = page(limit, offset);
        args.push(Value::Integer(limit));
        args.push(Value::Integer(offset));
        let where_sql = if conds.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conds.join(" AND "))
        };
        let sql = format!("{ENTRY_SELECT}{where_sql} ORDER BY e.id LIMIT ? OFFSET ?");
        self.entries_query(&sql, args, &format!("searching {query:?}"))
    }

    fn entries_query(&self, sql: &str, args: Vec<Value>, what: &str) -> Result<Vec<EntryRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(sql)
            .ctx_with(|| format!("preparing query for {what}"))?;
        let raws = stmt
            .query_map(params_from_iter(args), RawEntry::from_row)
            .ctx_with(|| format!("running query for {what}"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ctx_with(|| format!("reading rows for {what}"))?;
        raws.into_iter().map(RawEntry::into_row).collect()
    }

    /// Entries of one archive (all depths), by top-level offset then id.
    pub fn entries_in(&self, archive: ArchiveId, offset: u32, limit: u32) -> Result<Vec<EntryRow>> {
        let (limit, offset) = page(limit, offset);
        self.entries_query(
            &format!(
                "{ENTRY_SELECT} WHERE e.archive_id = ? ORDER BY e.top_offset, e.id LIMIT ? OFFSET ?"
            ),
            vec![
                Value::Integer(archive.0),
                Value::Integer(limit),
                Value::Integer(offset),
            ],
            &format!("entries of archive {}", archive.0),
        )
    }

    /// Top-level entries of one archive (no parent), for tree views.
    pub fn roots_in(&self, archive: ArchiveId, offset: u32, limit: u32) -> Result<Vec<EntryRow>> {
        let (limit, offset) = page(limit, offset);
        self.entries_query(
            &format!(
                "{ENTRY_SELECT} WHERE e.archive_id = ? AND e.parent_id IS NULL \
                 ORDER BY e.top_offset, e.id LIMIT ? OFFSET ?"
            ),
            vec![
                Value::Integer(archive.0),
                Value::Integer(limit),
                Value::Integer(offset),
            ],
            &format!("root entries of archive {}", archive.0),
        )
    }

    /// One entry by id.
    pub fn entry(&self, id: EntryId) -> Result<Option<EntryRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{ENTRY_SELECT} WHERE e.id = ?1"))
            .ctx("preparing entry lookup")?;
        stmt.query_row([id.0], RawEntry::from_row)
            .optional()
            .ctx_with(|| format!("reading entry {}", id.0))?
            .map(RawEntry::into_row)
            .transpose()
    }

    /// Direct children of an entry (e.g. the contents of a nested archive).
    pub fn children(&self, parent: EntryId) -> Result<Vec<EntryRow>> {
        self.entries_query(
            &format!("{ENTRY_SELECT} WHERE e.parent_id = ? ORDER BY e.id"),
            vec![Value::Integer(parent.0)],
            &format!("children of entry {}", parent.0),
        )
    }

    /// All archives, by path.
    pub fn archives(&self) -> Result<Vec<ArchiveRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{ARCHIVE_SELECT} ORDER BY rel_path"))
            .ctx("preparing archive list")?;
        let raws = stmt
            .query_map([], archive_from_row)
            .ctx("listing archives")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ctx("reading archives")?;
        raws.into_iter().map(archive_into_row).collect()
    }

    /// One archive by id.
    pub fn archive(&self, id: ArchiveId) -> Result<Option<ArchiveRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{ARCHIVE_SELECT} WHERE id = ?1"))
            .ctx("preparing archive lookup")?;
        stmt.query_row([id.0], archive_from_row)
            .optional()
            .ctx_with(|| format!("reading archive {}", id.0))?
            .map(archive_into_row)
            .transpose()
    }

    /// One archive by relative path (`\` normalised to `/`).
    pub fn archive_by_path(&self, rel_path: &str) -> Result<Option<ArchiveRow>> {
        let path = rel_path.replace('\\', "/");
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{ARCHIVE_SELECT} WHERE rel_path = ?1"))
            .ctx("preparing archive lookup")?;
        stmt.query_row([&path], archive_from_row)
            .optional()
            .ctx_with(|| format!("reading archive {path}"))?
            .map(archive_into_row)
            .transpose()
    }

    /// Names whose JOAAT (either variant) or listed hash equals `hash`.
    pub fn names_for_hash(&self, hash: u32) -> Result<Vec<NameRow>> {
        let h = i64::from(hash);
        self.names_query(
            "SELECT id, name, joaat, joaat_lower, alt_hash, source FROM names \
             WHERE joaat = ?1 UNION \
             SELECT id, name, joaat, joaat_lower, alt_hash, source FROM names \
             WHERE joaat_lower = ?1 UNION \
             SELECT id, name, joaat, joaat_lower, alt_hash, source FROM names \
             WHERE alt_hash = ?1 ORDER BY id",
            vec![Value::Integer(h)],
            &format!("names for hash {hash:#010x}"),
        )
    }

    /// Names containing every whitespace-separated term (case-insensitive),
    /// whether or not any entry uses them.
    pub fn search_names(&self, query: &str, limit: u32, offset: u32) -> Result<Vec<NameRow>> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_owned).collect();
        let mut args = Vec::new();
        let filter = if terms.is_empty() {
            String::new()
        } else {
            format!(" WHERE id IN ({})", name_match_sql(&terms, &mut args))
        };
        let (limit, offset) = page(limit, offset);
        args.push(Value::Integer(limit));
        args.push(Value::Integer(offset));
        self.names_query(
            &format!(
                "SELECT id, name, joaat, joaat_lower, alt_hash, source FROM names{filter} \
                 ORDER BY id LIMIT ? OFFSET ?"
            ),
            args,
            &format!("searching names {query:?}"),
        )
    }

    fn names_query(&self, sql: &str, args: Vec<Value>, what: &str) -> Result<Vec<NameRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(sql)
            .ctx_with(|| format!("preparing {what}"))?;
        let rows = stmt
            .query_map(params_from_iter(args), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .ctx_with(|| format!("running {what}"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ctx_with(|| format!("reading {what}"))?;
        rows.into_iter()
            .map(|(id, name, j, jl, alt, src)| {
                let h = |v: i64, what: &str| {
                    u32::try_from(v).map_err(|_| IndexError::Corrupt {
                        table: "names",
                        id,
                        detail: format!("{what} {v}"),
                    })
                };
                Ok(NameRow {
                    id: NameId(id),
                    name,
                    joaat: h(j, "joaat")?,
                    joaat_lower: h(jl, "joaat_lower")?,
                    listed_hash: alt.and_then(|a| u32::try_from(a).ok()),
                    source: NameSource::from_code(src),
                })
            })
            .collect()
    }

    /// Counts and bytes by kind, scan progress, and duplicate content.
    pub fn stats(&self) -> Result<Stats> {
        let c = &self.conn;
        let mut s = Stats::default();
        let u = |v: i64| u64::try_from(v).unwrap_or(0);

        let mut stmt = c
            .prepare_cached("SELECT scan_state, COUNT(*) FROM archives GROUP BY scan_state")
            .ctx("preparing archive stats")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .ctx("archive stats")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ctx("reading archive stats")?;
        for (state, n) in rows {
            let n = u(n);
            s.archives += n;
            match ScanState::from_code(state) {
                Some(ScanState::Pending) => s.archives_pending += n,
                Some(ScanState::Partial) => s.archives_partial += n,
                Some(ScanState::Done) => s.archives_done += n,
                None => log::warn!("index: {n} archives with unknown scan_state {state}"),
            }
        }

        let mut stmt = c
            .prepare_cached(
                "SELECT kind, COUNT(*), COALESCE(SUM(size), 0), COALESCE(SUM(stored_len), 0) \
                 FROM entries GROUP BY kind ORDER BY kind",
            )
            .ctx("preparing kind stats")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .ctx("kind stats")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ctx("reading kind stats")?;
        for (kind, n, size, stored) in rows {
            let ks = KindStats {
                kind: kind_from_code(kind),
                count: u(n),
                size: u(size),
                stored_len: u(stored),
            };
            s.entries += ks.count;
            s.total_size = s.total_size.saturating_add(ks.size);
            s.total_stored_len = s.total_stored_len.saturating_add(ks.stored_len);
            s.by_kind.push(ks);
        }

        let one = |sql: &str| -> Result<u64> {
            c.query_row(sql, [], |r| r.get::<_, i64>(0))
                .map(u)
                .ctx_with(|| format!("stats: {sql}"))
        };
        s.named_entries = one("SELECT COUNT(*) FROM entries WHERE name_id IS NOT NULL")?;
        s.provisional_entries = one("SELECT COUNT(*) FROM entries WHERE provisional != 0")?;
        s.names = one("SELECT COUNT(*) FROM names")?;

        let (groups, redundant, bytes) = c
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(c - 1), 0), COALESCE(SUM((c - 1) * sz), 0) FROM \
                 (SELECT COUNT(*) AS c, MAX(size) AS sz FROM entries \
                  WHERE hash IS NOT NULL GROUP BY hash HAVING COUNT(*) > 1)",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .ctx("duplicate stats")?;
        s.duplicates = DuplicateStats {
            groups: u(groups),
            redundant_entries: u(redundant),
            redundant_bytes: u(bytes),
        };
        Ok(s)
    }

    /// The duplicate-content groups wasting the most bytes. Find the entries
    /// of a group with `search("0x<hash hex>", ...)`.
    pub fn duplicates(&self, limit: u32) -> Result<Vec<DuplicateGroup>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT hash, COUNT(*) AS c, MAX(size) AS sz FROM entries \
                 WHERE hash IS NOT NULL GROUP BY hash HAVING c > 1 \
                 ORDER BY (c - 1) * sz DESC, hash LIMIT ?1",
            )
            .ctx("preparing duplicate list")?;
        let rows = stmt
            .query_map(params![i64::from(limit.min(MAX_PAGE))], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .ctx("listing duplicates")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ctx("reading duplicates")?;
        Ok(rows
            .into_iter()
            .filter_map(|(h, c, sz)| {
                Some(DuplicateGroup {
                    hash: <[u8; 32]>::try_from(h.as_slice()).ok()?,
                    count: u64::try_from(c).ok()?,
                    size: u64::try_from(sz).ok()?,
                })
            })
            .collect())
    }

    /// Query plan for a search, for diagnostics and tests.
    #[doc(hidden)]
    pub fn explain_search(&self, query: &str) -> Result<Vec<String>> {
        let _ = parse_query(query)?;
        let sql = format!(
            "EXPLAIN QUERY PLAN {ENTRY_SELECT} WHERE e.name_id IN \
             (SELECT rowid FROM names_fts WHERE names_fts MATCH ?1) ORDER BY e.id LIMIT 50"
        );
        let mut stmt = self.conn.prepare(&sql).ctx("explain")?;
        stmt.query_map([query], |r| r.get::<_, String>(3))
            .ctx("explain")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ctx("explain")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_queries() {
        let p = parse_query("kind:audio archive:3 vehicles adder").unwrap();
        assert_eq!(p.kind, Some(AssetKind::Audio));
        assert_eq!(p.archive, Some(3));
        assert_eq!(p.text, Text::Names(vec!["vehicles".into(), "adder".into()]));
        assert_eq!(
            parse_query("0xB779A091").unwrap().text,
            Text::Hex("b779a091".into())
        );
        let p = parse_query("Texture store").unwrap();
        assert_eq!(
            (p.kind, p.text),
            (Some(AssetKind::TextureStore), Text::None)
        );
        assert_eq!(
            parse_query("\"audio\"").unwrap().text,
            Text::Names(vec!["audio".into()])
        );
        assert!(parse_query("kind:nope").is_err());
        assert!(parse_query("kind:audio xml").is_err());
        assert_eq!(parse_query("  ").unwrap().text, Text::None);
        // Not hex: falls through to names.
        assert_eq!(
            parse_query("0xZZ").unwrap().text,
            Text::Names(vec!["0xZZ".into()])
        );
    }

    #[test]
    fn prefix_ranges() {
        assert_eq!(hex_prefix_range("ab"), Some((vec![0xAB], Some(vec![0xAC]))));
        assert_eq!(
            hex_prefix_range("abc"),
            Some((vec![0xAB, 0xC0], Some(vec![0xAB, 0xD0])))
        );
        assert_eq!(hex_prefix_range("ff"), Some((vec![0xFF], None)));
        assert_eq!(hex_prefix_range("1f"), Some((vec![0x1F], Some(vec![0x20]))));
        assert_eq!(like_pattern("a_b%"), "%a\\_b\\%%");
        assert_eq!(fts_quote("a\"b"), "\"a\"\"b\"");
    }
}
