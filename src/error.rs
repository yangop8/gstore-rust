//! Error types for the gStore engine.
//!
//! gStore (C++) leans on `bool` return codes plus global/log state. The Rust
//! rewrite uses an explicit [`GStoreError`] propagated through [`Result`], so
//! failures carry context and cannot be silently ignored.

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
