//! # gstore — a Rust rewrite of the gStore RDF triple store
//!
//! gStore (<https://github.com/pkumod/gStore>) is an RDF graph database that
//! answers SPARQL queries by subgraph matching. This crate re-implements it in
//! idiomatic Rust — the storage and query trunk, and the server, `/api`
//! protocol, tools and extensions around it:
//!
//! ```text
//! RDF files ─▶ parse ─▶ dictionary (string↔id) ─▶ six-way triple index ─▶ snapshot / disk / RocksDB
//! SPARQL    ─▶ parse ─▶ candidates + cost-based plan ─▶ index match + joins ─▶ FILTER … ─▶ results
//! ```
//!
//! Module map (`docs/DESIGN.md` describes the architecture and the mapping to
//! gStore's C++ modules; `README.md` has the same table with more detail):
//!
//! * **Data model and storage**
//!   * [`model`] — RDF terms, triples, integer-id conventions, the shared
//!     term validator ([`model::term`])
//!   * [`dict`] — bidirectional string↔id dictionaries (copy-on-write in
//!     memory; bounded caches over an out-of-core dictionary)
//!   * [`store`] — the in-memory s2xx / o2xx / p2xx index, the
//!     [`TripleSource`] / [`MutableStore`] seam every backend implements,
//!     [`Backend`], per-statement read errors, the transaction overlay
//!   * [`kvstore`] — the disk layout: pager, WAL, B+ trees, format registry,
//!     the Stream-mode `DiskTriples` / `DiskDict`; `backend` — RocksDB
//!     (feature `rocksdb`)
//!   * [`signature`] — entity signatures and the VS-tree candidate filter
//!   * [`bulk`] — bulk builds (external sort, bottom-up, crash-safe install)
//!   * [`db`] — the [`Database`] facade: build / open / save / query /
//!     update / transactions / backups over every layout, directory locks,
//!     id reclamation; [`concurrent`] — [`ConcurrentDb`], snapshot reads
//!     beside a writer (library API)
//! * **Languages**
//!   * [`parser`] — RDF readers (N-Triples, N-Quads, Turtle, TriG, RDF/XML,
//!     JSON-LD, RDFa) and SPARQL 1.1 Query + Update
//!   * [`query`] — the [`Evaluator`](query::Evaluator) with its two
//!     executors (materializing `eval_*` and streaming / Top-K `stream_*`),
//!     the cost-based optimizer, candidates, FILTER and aggregates, result
//!     sets; [`sparql_results`] — JSON / XML / CSV / TSV serializers
//! * **Extensions**
//!   * [`reason`] — upstream's `reasonManage` rules, RDFS materialization,
//!     textual rules; [`pfn`] — per-user PFN functions (sandboxed
//!     WebAssembly, feature `pfn-wasm`); [`analytics`] — CSR graph
//!     algorithms and upstream's advanced graph functions
//! * **Servers and tools**
//!   * [`server`] — the single-database SPARQL-protocol server (`gserver
//!     DIR`); [`http_api`] + [`gapi`] — the multi-database server (REST and
//!     upstream's `/api` protocol) with [`http_users`] / [`sysdb`] (users,
//!     privileges, logs), [`preauth`] and [`config`] (`conf.ini`);
//!     [`console`] — `gconsole`; `src/bin/` — `gbuild`, `gquery`, `gserver`,
//!     …
//!   * [`cluster`] / [`rpc`] — an experimental sharded store and its wire
//!     codec (library only)

pub mod analytics;
/// Persistent storage backends (feature-gated). Currently the RocksDB-backed
/// [`backend::rocks::RocksStore`]; compiled only with `--features rocksdb`.
#[cfg(feature = "rocksdb")]
pub mod backend;
// Bulk index build (sorted, bottom-up, bounded memory, crash-safe install).
pub mod bulk;
pub mod cluster;
pub mod concurrent;
pub mod config;
// `gconsole`: the interactive console, a client of gserver's `/api`.
pub mod console;
// Query deadlines: the timer behind `query_timeout`.
pub mod deadline;
pub mod dict;
pub mod error;
// A test hook: park a server's background thread right before it exits.
#[doc(hidden)]
pub mod exit_park;
// The upstream-compatible gStore `/api` operation layer (see `docs/GAP_PLAN.md` §2).
pub mod gapi;
pub mod http_api;
pub mod http_client;
pub mod http_users;
// A small dependency-free JSON value, parser and serializer.
pub mod json;
pub mod kvstore;
// Diagnostic logging (levels, standard error and a daily-rolling log file,
// the `gs_error!` … `gs_trace!` macros) — upstream's `SLOG_*` over log4cplus.
pub mod logging;
pub mod model;
pub mod parser;
// PFN: per-user graph functions run as sandboxed WebAssembly (`pfn-wasm`).
pub mod pfn;
// What a request may make the HTTP servers buffer before it is
// authenticated: body caps by route class, the body budget, the pre-body hook.
pub mod preauth;
pub mod query;
pub mod reason;
pub mod rpc;
pub mod server;
pub mod signature;
pub mod sparql_results;
/// Standard output for the command-line tools that a closed pipe ends
/// quietly (`outln!` / `out!` instead of `println!` / `print!`).
pub mod stdout;
pub mod store;
// A test probe recording the fsyncs behind every commit point (review
// tests-b#3; `tests/fsync_audit.rs`). Not part of the supported API.
#[doc(hidden)]
pub mod sync_probe;
// The system store: users, per-database privileges, the database registry.
pub mod sysdb;
// The decoders the protocol layers share (form / URL decoding, base64).
pub mod util;
// Zip packages (feature `zip`) and export files: RDF packages for `build` /
// `batchInsert`, zipped backups and exports, N-Triples / N-Quads exports.
pub mod zipio;

pub mod db;

// A transaction's commit outcome under injected storage failures (review
// gap-txn-lifecycle#3, gap-rocksdb-exec#5): the crate's test-only failpoints.
#[cfg(test)]
mod txn_commit_tests;

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

pub use concurrent::{ConcurrentDb, IsolationLevel};
pub use db::{
    default_open_options, set_default_open_options, Database, DiskQueryMode, IdReclaim, LoadPolicy,
    OpenOptions, QuerySettings, Statement, StorageKind,
};
pub use error::{GStoreError, Result};
pub use model::{IdTriple, ObjectType, Term, Triple};
pub use query::{FunctionRegistry, QueryResult, ResultSet, Value};
/// The pluggable-storage seam: read ([`TripleSource`]), write ([`MutableStore`]),
/// and the combined writable [`StorageBackend`]. The query/optimizer/VS-tree/
/// analytics layers run purely over these, so backends are swappable. [`Backend`]
/// is the runtime-selectable enum the [`Database`] holds.
pub use store::{Backend, MutableStore, StorageBackend, TripleSource};

/// The persistent RocksDB triple store (feature `rocksdb`).
#[cfg(feature = "rocksdb")]
pub use backend::rocks::RocksStore;
