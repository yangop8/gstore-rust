//! Bulk index build: RDF input → a complete database directory, sorted and
//! written bottom-up under a bounded memory budget, installed crash-safely
//! (contract C6 of docs/PHASE6_PLAN.md).
//!
//! ```text
//! sources ─▶ parse (per source, ordered replay) ─▶ encode (term maps, first-seen ids)
//!         ─▶ external sorts (SPO, then POS / OSP / GSPO, signatures)
//!         ─▶ page images (B+ trees at the fill factor, IvArrays, header) + vstree.kv
//!         ─▶ staged install into the database directory
//! ```
//!
//! [`build`] with [`BulkTarget::Disk`] writes a **version-1 `kvstore.kv`**
//! (the format of [`crate::kvstore`], byte-compatible with every reader and
//! writer; an older binary opens it too) and a fresh `vstree.kv`, the same
//! layout as `Database::build_disk_from_sources` — but in one pass over
//! sorted data instead of one B+ tree insert per index and term. Ids are
//! assigned exactly as the in-memory build assigns them (subject, predicate,
//! object, then graph, first seen first), so they equal the memory build's;
//! the tolerant / strict import semantics, the import report and the parse
//! error log are those of the sequential import.
//!
//! Memory: the sort buffers and spools stay within
//! [`BulkOptions::memory_budget`] (spilling sorted runs to scratch files when
//! they must); the term map (about `L + 26` bytes per term of `L` bytes) is
//! outside the budget and reported in [`BulkReport::term_map_bytes`].
//!
//! Crash safety: see [`commit`] for the install steps and what each crash
//! point leaves (always the old or the new database).

mod budget;
pub mod commit;
mod encode;
mod extsort;
mod kvimage;
mod pipeline;
mod scratch;
mod sigs;
mod source;
mod stats;
mod termmap;

use std::path::{Path, PathBuf};

use crate::error::{GStoreError, Result};
use crate::parser::import::{ErrorLog, ImportOptions, ImportReport, RdfFormat, Source};

pub use commit::{debug_crash_at, CRASH_ENV, CRASH_POINTS, STAGING_DIR};

/// The default memory budget: 1 GiB.
pub const DEFAULT_MEMORY_BUDGET: usize = 1024 << 20;
/// The default B+ tree fill factor, in percent.
pub const DEFAULT_FILL_PERCENT: u8 = 90;

/// What a bulk build reads.
#[derive(Debug, Clone, Copy)]
pub enum BulkInput<'a> {
    /// Files, directories and URLs, already expanded (see
    /// [`crate::parser::import::sources_from_paths`]); each source's format
    /// comes from its name, as for every other build.
    Sources(&'a [Source]),
    /// One in-memory document (no `begin` line is logged for it).
    Text {
        text: &'a str,
        format: RdfFormat,
        /// How the document is named in the error log.
        name: &'a str,
    },
}

/// Where a bulk build puts the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkTarget {
    /// The on-disk B+ tree layout (`kvstore.kv` + `vstree.kv`).
    Disk,
    /// RocksDB (SST ingest): not supported yet (Phase 6 round 3).
    RocksDb,
}

/// How a bulk build runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkOptions {
    /// Bytes the sort buffers, spools and merge buffers may use (default
    /// [`DEFAULT_MEMORY_BUDGET`]).
    pub memory_budget: usize,
    /// Threads: parser threads (one source each) and the parallel index
    /// tasks; `1` runs everything on the calling thread, `0` = the available
    /// parallelism (the default).
    pub threads: usize,
    /// Directory for spilled runs and spools (default: inside the database's
    /// staging directory).
    pub temp_dir: Option<PathBuf>,
    /// B+ tree node fill in percent, 50–100 (default
    /// [`DEFAULT_FILL_PERCENT`]).
    pub fill_percent: u8,
}

impl Default for BulkOptions {
    fn default() -> Self {
        BulkOptions {
            memory_budget: DEFAULT_MEMORY_BUDGET,
            threads: 0,
            temp_dir: None,
            fill_percent: DEFAULT_FILL_PERCENT,
        }
    }
}

impl BulkOptions {
    /// The thread count actually used (`0` → the available parallelism).
    pub fn effective_threads(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        }
    }

    fn validate(&self) -> Result<()> {
        if !(50..=100).contains(&self.fill_percent) {
            return Err(GStoreError::Database(format!(
                "bulk build fill factor {}% is outside 50–100%",
                self.fill_percent
            )));
        }
        Ok(())
    }
}

/// How long one phase of a build took.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseTime {
    /// The phase (`parse+encode`, `spo`, `index` with its parallel tasks,
    /// `finish`, `install`).
    pub name: &'static str,
    pub millis: f64,
}

/// What a bulk build did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BulkReport {
    /// The import counts (statements, named, failed, documents).
    pub import: ImportReport,
    /// Default-graph triples stored (duplicates removed).
    pub triples: u64,
    /// Named-graph quads stored.
    pub quads: u64,
    pub entities: u64,
    pub literals: u64,
    pub predicates: u64,
    /// Entities in `vstree.kv`.
    pub vstree_entities: u64,
    pub kv_bytes: u64,
    pub vstree_bytes: u64,
    /// Bytes of the term maps at their largest (outside the budget).
    pub term_map_bytes: u64,
    pub budget_bytes: u64,
    /// The most budgeted bytes in use at once.
    pub budget_peak_bytes: u64,
    /// Sorted runs spilled to scratch files (`0`: everything sorted in memory).
    pub spilled_runs: u64,
    /// Bytes written to scratch files.
    pub spilled_bytes: u64,
    pub threads: usize,
    pub phases: Vec<PhaseTime>,
    /// Wall time of the whole build.
    pub millis: f64,
    /// Install steps after the commit point that failed (see [`commit`]):
    /// the new database is in place, but its cleanup did
    /// not finish (an old layout, the staging area or the replaced
    /// database's reasoning rules may be left). Empty on a clean install.
    pub warnings: Vec<String>,
}

impl BulkReport {
    /// A short multi-line summary (counts, phases, memory, spills), as
    /// `gbuild` prints it.
    pub fn summary(&self) -> String {
        let mib = |b: u64| b as f64 / (1u64 << 20) as f64;
        let mut s = format!(
            "  triples={}  entities={}  literals={}  predicates={}\n",
            self.triples, self.entities, self.literals, self.predicates
        );
        if self.quads > 0 {
            s.push_str(&format!("  named-graph quads={}\n", self.quads));
        }
        s.push_str("  phases:");
        for p in &self.phases {
            s.push_str(&format!(" {}={:.0}ms", p.name.trim(), p.millis));
        }
        s.push('\n');
        s.push_str(&format!(
            "  memory: budget {:.0} MiB, peak {:.1} MiB; term map {:.1} MiB; threads {}\n",
            mib(self.budget_bytes),
            mib(self.budget_peak_bytes),
            mib(self.term_map_bytes),
            self.threads
        ));
        s.push_str(&format!(
            "  spills: {} run(s), {:.1} MiB; kvstore.kv {:.1} MiB, vstree.kv {:.1} MiB",
            self.spilled_runs,
            mib(self.spilled_bytes),
            mib(self.kv_bytes),
            mib(self.vstree_bytes)
        ));
        for w in &self.warnings {
            s.push_str(&format!("\n  warning: the install did not finish: {w}"));
        }
        s
    }
}

/// Bulk-build the database directory `dir` (contract C6): parse `input` with
/// `opts` (failed statements of a tolerant import go to `log`), sort and
/// write every index, and install the result crash-safely, replacing
/// whatever layout `dir` held. `name` is the database name (stored by layouts
/// that keep one; the disk layout derives it from the directory).
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
pub fn build(
    dir: &Path,
    name: &str,
    input: BulkInput<'_>,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    target: BulkTarget,
    bulk: &BulkOptions,
) -> Result<BulkReport> {
    let _ = name;
    bulk.validate()?;
    match target {
        BulkTarget::Disk => commit::build_disk(dir, &input, opts, log, bulk),
        BulkTarget::RocksDb => Err(GStoreError::Database(
            "the bulk build does not support the RocksDB layout yet".into(),
        )),
    }
}
