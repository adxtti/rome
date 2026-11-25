//! Schema and migrations, versioned with `PRAGMA user_version`.
//!
//! `MIGRATIONS[i]` upgrades a database from version `i` to `i + 1`. Append new
//! migrations; never edit a released one. Each runs in its own transaction
//! together with the `user_version` bump, so a crash mid-migration leaves the
//! previous version intact.

use rusqlite::Connection;

use crate::error::{Ctx, IndexError, Result};

const V1: &str = r#"
CREATE TABLE archives (
    id          INTEGER PRIMARY KEY,
    rel_path    TEXT    NOT NULL UNIQUE,
    size        INTEGER NOT NULL,
    mtime       INTEGER NOT NULL,
    header      TEXT,
    scan_state  INTEGER NOT NULL DEFAULT 0,  -- 0 pending, 1 partial, 2 done
    last_offset INTEGER NOT NULL DEFAULT 0,
    entry_count INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER                      -- unix seconds of the last state change
);

CREATE TABLE names (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL UNIQUE,
    joaat       INTEGER NOT NULL,            -- case-sensitive JOAAT, as u32
    joaat_lower INTEGER NOT NULL,            -- JOAAT of ASCII-lowercased name, as u32
    alt_hash    INTEGER,                     -- unverified hash from an imported list
    source      INTEGER NOT NULL             -- NameSource code
);
CREATE INDEX names_by_joaat       ON names(joaat);
CREATE INDEX names_by_joaat_lower ON names(joaat_lower);
CREATE INDEX names_by_alt_hash    ON names(alt_hash) WHERE alt_hash IS NOT NULL;

CREATE TABLE entries (
    id          INTEGER PRIMARY KEY,
    archive_id  INTEGER NOT NULL REFERENCES archives(id) ON DELETE CASCADE,
    parent_id   INTEGER REFERENCES entries(id) ON DELETE CASCADE,
    chain       BLOB    NOT NULL,            -- u64 LE per level
    top_offset  INTEGER NOT NULL,            -- chain[0]
    depth       INTEGER NOT NULL,            -- chain length
    stored_len  INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    codec       INTEGER NOT NULL,
    pages       INTEGER NOT NULL,
    kind        INTEGER NOT NULL,            -- kind_code()
    head        BLOB    NOT NULL,
    hash        BLOB,                        -- BLAKE3 of decoded bytes
    provisional INTEGER NOT NULL,
    name_hash   INTEGER,                     -- embedded name hash, as u32
    name_id     INTEGER REFERENCES names(id) ON DELETE SET NULL,
    name_match  INTEGER                      -- NameMatch code
);
CREATE INDEX entries_by_archive   ON entries(archive_id, top_offset);
CREATE INDEX entries_by_kind      ON entries(kind);
CREATE INDEX entries_by_hash      ON entries(hash)      WHERE hash IS NOT NULL;
CREATE INDEX entries_by_parent    ON entries(parent_id) WHERE parent_id IS NOT NULL;
CREATE INDEX entries_by_name      ON entries(name_id)   WHERE name_id IS NOT NULL;
CREATE INDEX entries_by_name_hash ON entries(name_hash) WHERE name_hash IS NOT NULL;

-- Trigram tokenizer: substring search ("adde" finds "adder"), case-insensitive.
CREATE VIRTUAL TABLE names_fts USING fts5(
    name, content='names', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER names_ai AFTER INSERT ON names BEGIN
    INSERT INTO names_fts(rowid, name) VALUES (new.id, new.name);
END;
CREATE TRIGGER names_ad AFTER DELETE ON names BEGIN
    INSERT INTO names_fts(names_fts, rowid, name) VALUES ('delete', old.id, old.name);
END;
CREATE TRIGGER names_au AFTER UPDATE OF name ON names BEGIN
    INSERT INTO names_fts(names_fts, rowid, name) VALUES ('delete', old.id, old.name);
    INSERT INTO names_fts(rowid, name) VALUES (new.id, new.name);
END;
"#;

/// Ordered migrations; index `i` upgrades version `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[V1];

/// Schema version this build writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Read the database's schema version.
pub fn version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .ctx("reading schema version")
}

/// Bring the schema up to [`SCHEMA_VERSION`].
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let found = version(conn)?;
    if found > SCHEMA_VERSION {
        return Err(IndexError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found < 0 {
        return Err(IndexError::Corrupt {
            table: "pragma",
            id: 0,
            detail: format!("negative user_version {found}"),
        });
    }
    for (from, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let to = from as i64 + 1;
        let tx = crate::write_tx(conn).ctx_with(|| format!("starting migration {from} -> {to}"))?;
        // Another connection may have migrated while we waited for the lock.
        if version(&tx)? >= to {
            continue;
        }
        log::debug!("index: migrating schema {from} -> {to}");
        tx.execute_batch(sql)
            .ctx_with(|| format!("applying migration {from} -> {to}"))?;
        // PRAGMA does not accept bound parameters; `to` is a trusted integer.
        tx.execute_batch(&format!("PRAGMA user_version = {to}"))
            .ctx_with(|| format!("setting user_version {to}"))?;
        tx.commit()
            .ctx_with(|| format!("committing migration {from} -> {to}"))?;
    }
    Ok(())
}
