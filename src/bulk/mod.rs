//! Bulk index build: RDF input → a complete database directory, sorted and
//! written bottom-up under a bounded memory budget, installed crash-safely
//! (contract C6 of docs/PHASE6_PLAN.md).
//!
//! ```text
//! sources ─▶ parse (per source, ordered replay) ─▶ encode (term maps, first-seen ids)
//!         ─▶ external sorts (SPO, then POS / OSP / GSPO, signatures)
//!         ─▶ page images (B+ trees at the fill factor, IvArrays, header) + vstree.kv
//!            or SST files ingested into a fresh RocksDB store
//!         ─▶ staged install into the database directory
//! ```
//!
//! [`build`] with [`BulkTarget::Disk`] writes a **version-1 `kvstore.kv`**
//! (the format of [`crate::kvstore`], byte-compatible with every reader and
//! writer; an older binary opens it too) and a fresh `vstree.kv`, the same
//! layout as `Database::build_disk_from_sources_incremental` — but in one pass over
//! sorted data instead of one B+ tree insert per index and term. Ids are
//! assigned exactly as the in-memory build assigns them (subject, predicate,
//! object, then graph, first seen first), so they equal the memory build's;
//! the tolerant / strict import semantics, the import report and the parse
//! error log are those of the sequential import.
//!
//! With [`BulkTarget::RocksDb`] (feature `rocksdb`) the same sorted streams
//! become SST files, one set per column family, ingested into a fresh store:
//! RocksDB layout `"1"` with every key and value byte-identical to what
//! `Database::build_rocksdb_*` (one write batch per triple) stores for the
//! same input — see `rocks_sink`.
//!
//! Memory: the sort buffers and spools stay within
//! [`BulkOptions::memory_budget`] (spilling sorted runs to scratch files when
//! they must); the term map (about `L + 26` bytes per term of `L` bytes) is
//! outside the budget and reported in [`BulkReport::term_map_bytes`].
//!
//! Crash safety: see [`commit`] for the install steps of the disk layout and
//! what each crash point leaves (always the old or the new database);
//! [`ROCKS_CRASH_POINTS`] are the RocksDB install's.

mod budget;
pub mod commit;
mod encode;
mod extsort;
mod kvimage;
mod pipeline;
#[cfg(feature = "rocksdb")]
mod rocks_sink;
mod scratch;
mod sigs;
mod source;
mod stats;
mod termmap;
mod types;

use std::path::Path;

use crate::error::Result;
use crate::parser::import::{ErrorLog, ImportOptions};

pub use commit::{CRASH_ENV, CRASH_POINTS, STAGING_DIR};
pub use types::{
    BulkInput, BulkOptions, BulkReport, BulkTarget, PhaseTime, DEFAULT_FILL_PERCENT,
    DEFAULT_MEMORY_BUDGET,
};

/// The install steps of a RocksDB bulk build, in order (the crash points of
/// [`CRASH_ENV`] and [`debug_crash_at`] for that target):
///
/// | step | a crash after it leaves |
/// |------|-------------------------|
/// | `staged` | the new store complete, closed and synced in `.bulk-tmp/rocksdb`: the old database |
/// | `swapped` | the new store renamed to `rocksdb/` (through `rocksdb.new`, which the next open completes): **the new database** — the commit point |
/// | `rules-removed` | the replaced database's reasoning rules removed |
/// | `layouts-removed` | the other layouts (snapshot files, `kvstore.kv`) removed |
/// | `cleaned` | `.bulk-tmp/` removed |
pub const ROCKS_CRASH_POINTS: [&str; 5] = [
    "staged",
    "swapped",
    "rules-removed",
    "layouts-removed",
    "cleaned",
];

/// Make the next bulk builds of directory `dir` in this process stop right
/// after install step `point` (one of [`CRASH_POINTS`], or of
/// [`ROCKS_CRASH_POINTS`] for the RocksDB target) with an error, leaving the
/// directory exactly as a crash there would; `None` clears it. A test hook:
/// it costs one lookup per install and nothing while unset.
#[doc(hidden)]
pub fn debug_crash_at(dir: &Path, point: Option<&str>) {
    commit::debug_crash_at(dir, point);
    #[cfg(feature = "rocksdb")]
    rocks_sink::debug_crash_at(dir, point);
}

/// Bulk-build the database directory `dir` (contract C6): parse `input` with
/// `opts` (failed statements of a tolerant import go to `log`), sort and
/// write every index, and install the result crash-safely, replacing
/// whatever layout `dir` held. `name` is the database name (stored by layouts
/// that keep one — RocksDB's `meta` column family; the disk layout derives
/// it from the directory).
///
/// Fails without touching an existing database when the input fails (a
/// strict import's first bad statement, an unreadable source, a term too
/// long for the disk layout — with the incremental build's error text),
/// when another process holds the directory, when a database of this
/// process has it open, when a RocksDB store in it is open (Linux: its
/// `rocksdb/LOCK` is held), or when another bulk build of this process is
/// rebuilding it. The install replaces the database's reasoning rules
/// (`reason_rule_files/`) too: they belong to the replaced data. Once the
/// install's commit point is passed the build succeeds, and a cleanup step
/// that failed after it is listed in [`BulkReport::warnings`].
///
/// [`BulkTarget::RocksDb`] needs the `rocksdb` feature (without it the
/// build fails before touching anything); it ignores
/// [`BulkOptions::fill_percent`].
pub fn build(
    dir: &Path,
    name: &str,
    input: BulkInput<'_>,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    target: BulkTarget,
    bulk: &BulkOptions,
) -> Result<BulkReport> {
    bulk.validate()?;
    match target {
        BulkTarget::Disk => commit::build_disk(dir, &input, opts, log, bulk),
        #[cfg(feature = "rocksdb")]
        BulkTarget::RocksDb => rocks_sink::build_rocksdb(dir, name, &input, opts, log, bulk),
        #[cfg(not(feature = "rocksdb"))]
        BulkTarget::RocksDb => {
            let _ = name;
            Err(crate::error::GStoreError::Database(
                "the RocksDB bulk build requires building with `--features rocksdb`".into(),
            ))
        }
    }
}
