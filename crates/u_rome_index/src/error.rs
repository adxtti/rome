//! Error type for the index.

use thiserror::Error;

/// Everything that can go wrong while opening, writing or querying the index.
///
/// SQLite errors always carry a short description of what the index was doing,
/// so a failure in a user's log names the operation (and, where relevant, the
/// archive or entry id) rather than only the raw SQLite message.
#[derive(Debug, Error)]
pub enum IndexError {
    #[error("sqlite error while {context}: {source}")]
    Sqlite {
        context: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "index schema version {found} is newer than this build supports ({supported}); \
         delete the index file or update u_rome"
    )]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("corrupt index row {table}#{id}: {detail}")]
    Corrupt {
        table: &'static str,
        id: i64,
        detail: String,
    },
    #[error("archive id {0} not found in index")]
    ArchiveNotFound(i64),
    #[error("entry id {0} not found in index")]
    EntryNotFound(i64),
}

pub type Result<T, E = IndexError> = std::result::Result<T, E>;

/// Attach a context string to a rusqlite result.
pub(crate) trait Ctx<T> {
    fn ctx(self, context: &str) -> Result<T>;
    fn ctx_with(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> Ctx<T> for rusqlite::Result<T> {
    fn ctx(self, context: &str) -> Result<T> {
        self.map_err(|source| IndexError::Sqlite {
            context: context.to_owned(),
            source,
        })
    }

    fn ctx_with(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| IndexError::Sqlite {
            context: context(),
            source,
        })
    }
}

/// Convert a u64 from game data into SQLite's signed INTEGER range.
pub(crate) fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v).map_err(|_| IndexError::Invalid(format!("{what} {v:#x} exceeds i64::MAX")))
}

/// Convert a stored INTEGER back to u64, flagging negative values as corruption.
pub(crate) fn to_u64(v: i64, table: &'static str, id: i64, what: &str) -> Result<u64> {
    u64::try_from(v).map_err(|_| IndexError::Corrupt {
        table,
        id,
        detail: format!("negative {what} {v}"),
    })
}
