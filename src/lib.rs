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
pub mod server;
pub mod signature;
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
