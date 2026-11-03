//! The public types of the bulk build (contract C6): what it reads, where it
//! puts the database, how it runs and what it reports.

use std::path::PathBuf;

use crate::error::{GStoreError, Result};
use crate::parser::import::{ImportReport, RdfFormat, Source};

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
    /// RocksDB (`rocksdb/`, layout `"1"`): SST files written from the sorted
    /// streams and ingested into a fresh store (feature `rocksdb`).
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
    /// [`DEFAULT_FILL_PERCENT`]); the disk layout only.
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

    pub(crate) fn validate(&self) -> Result<()> {
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
    /// `finish` or `ingest`, `install`).
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
    /// Entities in `vstree.kv` (the disk layout; `0` for RocksDB).
    pub vstree_entities: u64,
    /// Bytes of the store: `kvstore.kv`, or the whole `rocksdb/` directory.
    pub kv_bytes: u64,
    /// Bytes of `vstree.kv` (the disk layout; `0` for RocksDB).
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
    /// Install steps after the commit point that failed (see
    /// [`commit`](super::commit)): the new database is in place, but its
    /// cleanup did not finish (an old layout, the staging area or the
    /// replaced database's reasoning rules may be left). Empty on a clean
    /// install.
    pub warnings: Vec<String>,
}

impl BulkReport {
    /// Whether the database the build installed opens as built: every one
    /// of its [`warnings`](Self::warnings) is about a leftover no open reads
    /// (the staging area). Otherwise the replaced layout or rules may still
    /// be there, or the new store may not be durably in place: opening it
    /// might read what was left behind.
    pub fn opens_as_built(&self) -> bool {
        self.warnings
            .iter()
            .all(|w| w.starts_with(super::commit::STAGING_CLEANUP))
    }

    /// A short multi-line summary (counts, phases, memory, spills) of a disk
    /// build, as `gbuild` prints it; [`summary_for`](Self::summary_for)
    /// names the files of either target.
    pub fn summary(&self) -> String {
        self.summary_for(BulkTarget::Disk)
    }

    /// [`summary`](Self::summary) for a build of `target`.
    pub fn summary_for(&self, target: BulkTarget) -> String {
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
        let files = match target {
            BulkTarget::Disk => format!(
                "kvstore.kv {:.1} MiB, vstree.kv {:.1} MiB",
                mib(self.kv_bytes),
                mib(self.vstree_bytes)
            ),
            BulkTarget::RocksDb => format!("rocksdb/ {:.1} MiB", mib(self.kv_bytes)),
        };
        s.push_str(&format!(
            "  spills: {} run(s), {:.1} MiB; {files}",
            self.spilled_runs,
            mib(self.spilled_bytes),
        ));
        for w in &self.warnings {
            s.push_str(&format!("\n  warning: the install did not finish: {w}"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a staging area left behind lets the installed database be
    /// opened as built; any other unfinished step (old rules or layouts, a
    /// rename not yet durable) does not.
    #[test]
    fn only_a_left_staging_area_opens_as_built() {
        let with = |w: &[&str]| BulkReport {
            warnings: w.iter().map(|s| s.to_string()).collect(),
            ..BulkReport::default()
        };
        assert!(with(&[]).opens_as_built());
        assert!(with(&["removing the staging area: I/O error"]).opens_as_built());
        for other in [
            "removing the replaced database's reasoning rules: Not a directory",
            "removing the other layouts: I/O error",
            "making the new kvstore.kv durable (the rest of the install was skipped; the next \
             build cleans up): I/O error",
        ] {
            assert!(!with(&[other]).opens_as_built(), "{other}");
            assert!(
                !with(&["removing the staging area: x", other]).opens_as_built(),
                "{other}"
            );
        }
    }
}
