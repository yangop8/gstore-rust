//! Error types for the gStore engine.
//!
//! gStore (C++) leans on `bool` return codes plus global/log state. The Rust
//! rewrite uses an explicit [`GStoreError`] propagated through [`Result`], so
//! failures carry context and cannot be silently ignored.
//!
//! **Classification.** [`GStoreError::kind`] says what *kind* of failure an
//! error is ([`ErrorKind`]): malformed input, a lock held elsewhere, a
//! storage or persistence failure, a cancellation, … The protocol layers
//! (the REST endpoints, the upstream `/api` layer, the single-database
//! server) derive their status from it in one place
//! (`crate::http_api::OpErrorKind::of`) instead of reading the message
//! (review design#3). Most construction sites still build the catch-all
//! [`GStoreError::Database`], which classifies as [`ErrorKind::Other`]
//! (each endpoint then answers as it did before errors were typed); the
//! directory locks ([`GStoreError::Locked`]) and the read errors of a
//! statement (contract C2, [`crate::store::read_scope::read_failure`]) are
//! typed. (A write applied in memory whose save failed is not an error of
//! the statement: the servers report it as their own persistence failure.)

use std::fmt;

/// The crate-wide result alias.
pub type Result<T> = std::result::Result<T, GStoreError>;

/// All error conditions the engine can surface.
#[derive(Debug)]
pub enum GStoreError {
    /// An I/O failure while reading RDF, or loading/saving a database.
    Io(std::io::Error),
    /// An RDF (e.g. N-Triples) parse failure: line number + reason.
    RdfParse { line: usize, msg: String },
    /// A SPARQL parse failure with a human-readable reason.
    SparqlParse(String),
    /// A query could not be evaluated (e.g. unbound variable in SELECT).
    Query(String),
    /// Persistence (de)serialization failure.
    Serialize(String),
    /// A database directory was malformed or missing required files.
    Database(String),
    /// An optimistic transaction aborted: a concurrent commit wrote a triple key
    /// this transaction also wrote (write-write conflict, first-committer-wins).
    Conflict(String),
    /// A read query was cooperatively cancelled (its cancel flag was set) and
    /// aborted at a pattern/phase boundary before completing.
    Cancelled,
    /// A statement ran longer than its time limit (`query_timeout`) and was
    /// stopped (review sec#10): the time it was given. Servers answer it
    /// as a refused request (REST `503`), not as a query error.
    Timeout(std::time::Duration),
    /// The on-disk store failed while committing a page batch (or an earlier
    /// write left it inconsistent with the session) and refuses every further
    /// write until the database is reopened: its write-ahead log holds the
    /// last batch that may have reached the file, so a reopen (`unload` +
    /// `load` on a server) restores a consistent state.
    StorageFailed(String),
    /// The database directory (or the lock file beside it) is locked by
    /// another process — a gserver, `gquery`, `gconsole`, a bulk build —
    /// or by a build of this process still running: a conflict to retry
    /// later, not a failure of the storage ([`ErrorKind::Locked`]; review
    /// design#3). Displayed as [`Database`](Self::Database) is, so the
    /// message reads as before.
    Locked(String),
}

/// What kind of failure a [`GStoreError`] is ([`GStoreError::kind`]): what
/// a protocol layer answers it as (review design#3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Malformed input: a SPARQL or RDF syntax error.
    Parse,
    /// A well-formed statement that could not be evaluated.
    Query,
    /// A lock another process (or a running build) holds: retry later.
    Locked,
    /// A write-write conflict between optimistic transactions.
    Conflict,
    /// A (de)serialization failure ([`GStoreError::Serialize`]): stored data
    /// that does not decode, or a value that does not encode.
    Corrupt,
    /// The storage failed: an I/O error, a read the store could not answer,
    /// a store broken by an earlier write error.
    Storage,
    /// Stopped by a cancellation request (`/cancel`, a shutdown).
    Cancelled,
    /// Stopped by its time limit (`query_timeout`).
    Timeout,
    /// Anything else (the untyped [`GStoreError::Database`] messages):
    /// each caller decides.
    Other,
}

impl fmt::Display for GStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GStoreError::Io(e) => write!(f, "I/O error: {e}"),
            GStoreError::RdfParse { line, msg } => {
                write!(f, "RDF parse error at line {line}: {msg}")
            }
            GStoreError::SparqlParse(msg) => write!(f, "SPARQL parse error: {msg}"),
            GStoreError::Query(msg) => write!(f, "query error: {msg}"),
            GStoreError::Serialize(msg) => write!(f, "serialization error: {msg}"),
            GStoreError::Database(msg) => write!(f, "database error: {msg}"),
            GStoreError::Conflict(msg) => write!(f, "transaction conflict: {msg}"),
            GStoreError::Cancelled => write!(f, "query cancelled"),
            GStoreError::Timeout(after) => f.write_str(&crate::deadline::timeout_message(*after)),
            GStoreError::StorageFailed(msg) => write!(
                f,
                "storage failed: {msg}; nothing more is written until the database is reopened"
            ),
            GStoreError::Locked(msg) => write!(f, "database error: {msg}"),
        }
    }
}

impl GStoreError {
    /// What kind of failure this is (see [`ErrorKind`]). An untyped
    /// [`Database`](Self::Database) message is [`ErrorKind::Other`] — except
    /// the read error of a statement (contract C2), whose message every
    /// construction site starts with
    /// [`READ_FAILURE`](crate::store::read_scope::READ_FAILURE): that is a
    /// [`ErrorKind::Storage`] failure.
    pub fn kind(&self) -> ErrorKind {
        match self {
            GStoreError::Io(_) | GStoreError::StorageFailed(_) => ErrorKind::Storage,
            GStoreError::RdfParse { .. } | GStoreError::SparqlParse(_) => ErrorKind::Parse,
            GStoreError::Query(_) => ErrorKind::Query,
            GStoreError::Conflict(_) => ErrorKind::Conflict,
            GStoreError::Cancelled => ErrorKind::Cancelled,
            GStoreError::Timeout(_) => ErrorKind::Timeout,
            GStoreError::Locked(_) => ErrorKind::Locked,
            GStoreError::Database(msg)
                if msg.starts_with(crate::store::read_scope::READ_FAILURE) =>
            {
                ErrorKind::Storage
            }
            GStoreError::Serialize(_) => ErrorKind::Corrupt,
            GStoreError::Database(_) => ErrorKind::Other,
        }
    }

    /// A copy of this error (an I/O error keeps its kind and message) — for
    /// an error recorded once and reported wherever evaluation stops.
    pub(crate) fn duplicate(&self) -> GStoreError {
        match self {
            GStoreError::Io(e) => GStoreError::Io(std::io::Error::new(e.kind(), e.to_string())),
            GStoreError::RdfParse { line, msg } => GStoreError::RdfParse {
                line: *line,
                msg: msg.clone(),
            },
            GStoreError::SparqlParse(m) => GStoreError::SparqlParse(m.clone()),
            GStoreError::Query(m) => GStoreError::Query(m.clone()),
            GStoreError::Serialize(m) => GStoreError::Serialize(m.clone()),
            GStoreError::Database(m) => GStoreError::Database(m.clone()),
            GStoreError::Conflict(m) => GStoreError::Conflict(m.clone()),
            GStoreError::Cancelled => GStoreError::Cancelled,
            GStoreError::Timeout(after) => GStoreError::Timeout(*after),
            GStoreError::StorageFailed(m) => GStoreError::StorageFailed(m.clone()),
            GStoreError::Locked(m) => GStoreError::Locked(m.clone()),
        }
    }
}

impl std::error::Error for GStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GStoreError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GStoreError {
    fn from(e: std::io::Error) -> Self {
        GStoreError::Io(e)
    }
}

impl From<Box<bincode::ErrorKind>> for GStoreError {
    fn from(e: Box<bincode::ErrorKind>) -> Self {
        GStoreError::Serialize(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// review design#3: every variant has its kind, and an untyped message
    /// is `Other` unless it is a statement's read error.
    #[test]
    fn every_variant_has_its_kind() {
        let io = || std::io::Error::other("disk");
        let cases = [
            (GStoreError::Io(io()), ErrorKind::Storage),
            (
                GStoreError::RdfParse {
                    line: 1,
                    msg: "x".into(),
                },
                ErrorKind::Parse,
            ),
            (GStoreError::SparqlParse("x".into()), ErrorKind::Parse),
            (GStoreError::Query("x".into()), ErrorKind::Query),
            (GStoreError::Serialize("x".into()), ErrorKind::Corrupt),
            (GStoreError::Database("x".into()), ErrorKind::Other),
            (GStoreError::Conflict("x".into()), ErrorKind::Conflict),
            (GStoreError::Cancelled, ErrorKind::Cancelled),
            (
                GStoreError::Timeout(std::time::Duration::from_secs(1)),
                ErrorKind::Timeout,
            ),
            (GStoreError::StorageFailed("x".into()), ErrorKind::Storage),
            (GStoreError::Locked("x".into()), ErrorKind::Locked),
            (
                crate::store::read_scope::read_failure("evaluating the query", "page 7"),
                ErrorKind::Storage,
            ),
        ];
        for (e, kind) in cases {
            assert_eq!(e.kind(), kind, "{e}");
            assert_eq!(e.duplicate().kind(), kind, "{e}");
        }
    }

    /// The typed errors read as the untyped ones did.
    #[test]
    fn typed_variants_keep_their_messages() {
        assert_eq!(
            GStoreError::Locked("d is locked by another process".into()).to_string(),
            "database error: d is locked by another process"
        );
        assert_eq!(
            crate::store::read_scope::read_failure("exporting", "page 7").to_string(),
            "database error: a read from the store failed while exporting: page 7"
        );
    }
}
