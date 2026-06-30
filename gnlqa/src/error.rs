//! The crate-wide error type.

use std::fmt;

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in gNLQA.
#[derive(Debug)]
pub enum Error {
    /// Missing or invalid configuration (e.g. no API key).
    Config(String),
    /// The LLM call failed or returned an unexpected shape.
    Llm(String),
    /// Talking to the gStore backend failed.
    GStore(String),
    /// A generated/!supplied SPARQL query is invalid.
    Sparql(String),
    /// Transport-level HTTP failure.
    Http(String),
    /// (De)serialization failure.
    Json(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(m) => write!(f, "config error: {m}"),
            Error::Llm(m) => write!(f, "LLM error: {m}"),
            Error::GStore(m) => write!(f, "gStore error: {m}"),
            Error::Sparql(m) => write!(f, "SPARQL error: {m}"),
            Error::Http(m) => write!(f, "HTTP error: {m}"),
            Error::Json(m) => write!(f, "JSON error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e.to_string())
    }
}
