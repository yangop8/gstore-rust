//! # gstore — a Rust rewrite of the gStore RDF triple store
//!
//! gStore (<https://github.com/pkumod/gStore>) is an RDF graph database that
//! answers SPARQL queries by subgraph matching. This crate re-implements its
//! *trunk* in idiomatic Rust:
//!
//! ```text
//! RDF file ─▶ parse ─▶ dictionary (string↔id) ─▶ six-way triple index ─▶ disk
//! SPARQL   ─▶ parse ─▶ BGP plan ─▶ index match + join ─▶ FILTER ─▶ result set
//! ```
//!
//! Module map (see `docs/DESIGN.md` for the mapping to gStore's C++ modules):
//!
//! * [`model`]  — RDF terms, triples, integer-id conventions
//! * [`dict`]   — bidirectional string↔id dictionaries
//! * [`store`]  — the s2xx / o2xx / p2xx triple indexes
//! * [`parser`] — N-Triples and SPARQL parsing
//! * [`query`]  — BGP evaluation, joins, FILTER, result sets
//! * [`db`]     — the [`db::Database`] facade: build / load / save / query / update

pub mod analytics;
/// Persistent storage backends (feature-gated). Currently the RocksDB-backed
/// [`backend::rocks::RocksStore`]; compiled only with `--features rocksdb`.
#[cfg(feature = "rocksdb")]
pub mod backend;
pub mod cluster;
pub mod concurrent;
pub mod dict;
pub mod error;
pub mod http_client;
pub mod kvstore;
pub mod model;
pub mod parser;
pub mod query;
pub mod reason;
pub mod rpc;
pub mod server;
pub mod signature;
pub mod sparql_results;
pub mod store;

pub mod db;

/// The gStore database-directory suffix (gStore C++ uses the same `.db`).
pub const DB_SUFFIX: &str = ".db";

/// Map a database name to its on-disk directory: `name` → `name.db` (idempotent
/// if the name already carries the suffix).
pub fn db_dir_for(name: &str) -> String {
    if name.ends_with(DB_SUFFIX) {
        name.to_string()
    } else {
        format!("{name}{DB_SUFFIX}")
    }
}

pub use db::Database;
pub use error::{GStoreError, Result};
pub use model::{IdTriple, ObjectType, Term, Triple};
pub use query::{QueryResult, ResultSet};
/// The pluggable-storage seam: read ([`TripleSource`]), write ([`MutableStore`]),
/// and the combined writable [`StorageBackend`]. The query/optimizer/VS-tree/
/// analytics layers run purely over these, so backends are swappable. [`Backend`]
/// is the runtime-selectable enum the [`Database`] holds.
pub use store::{Backend, MutableStore, StorageBackend, TripleSource};

/// The persistent RocksDB triple store (feature `rocksdb`).
#[cfg(feature = "rocksdb")]
pub use backend::rocks::RocksStore;
