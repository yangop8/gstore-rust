//! The staged, crash-safe install of a bulk build.
//!
//! The build holds the directory's [`DirLock`] (another process holding the
//! directory makes it fail) and a [`BuildGuard`] (another bulk build of this
//! process does), and writes everything into `<dir>/.bulk-tmp/`, which no
//! layout detection looks at. A stale `.bulk-tmp/` from a crashed build is
//! removed first. Once both files are complete and `fsync`ed, the install
//! runs; after each step a crash leaves a directory that an unmodified
//! binary opens as either the old database or the new one:
//!
//! | # | step | a crash after it leaves |
//! |---|------|-------------------------|
//! | 1 `staged` | `kvstore.kv` + `vstree.kv` complete and synced in `.bulk-tmp/` | the old database |
//! | 2 `marked` | `.bulk-tmp/COMMIT` written and synced | the old database |
//! | 3 `cleared` | a stale `kvstore.kv.wal` replayed into the old file (what any open does) and removed; the old `vstree.kv` and its WAL removed; synced | the old database (a missing `vstree.kv` is rebuilt at open) |
//! | 4 `kv-renamed` | `.bulk-tmp/kvstore.kv` renamed over `kvstore.kv` | the new database (no `vstree.kv` yet: rebuilt at open), or — under an existing `rocksdb/`, which detection prefers — still the old one |
//! | 5 `kv-synced` | directory synced | as 4, durably |
//! | 6 `vstree-placed` | `vstree.kv` renamed into place, synced | as 4, with the new VS-tree |
//! | 7 `layouts-removed` | the other layouts removed (snapshot files; `rocksdb/` renamed aside durably, then deleted) | the new database |
//! | 8 `cleaned` | `.bulk-tmp/` deleted, synced | the new database |
//!
//! The old `vstree.kv` goes before the new `kvstore.kv` arrives, so a new
//! store never meets an old VS-tree (or the reverse) marked fresh. A
//! snapshot left beside the new `kvstore.kv` by a crash between 4 and 7 is
//! older than it (the install makes sure of that), so the open treats it as
//! a stale legacy shadow and sets it aside. A strict parse failure, or any
//! other error before step 4, leaves the old database untouched.
//!
//! Crash tests: the `GSTORE_BULK_CRASH=<step>` environment variable aborts
//! the process right after that step (read once per install, never on the
//! hot path), and [`debug_crash_at`] makes an in-process build of one
//! directory stop there with an error, leaving its files as a crash would.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use crate::db::dirlock::{BuildGuard, DirLock};
use crate::db::fsutil::{remove_file_if_exists, sync_dir};
use crate::db::persist::remove_other_layouts;
use crate::db::StorageKind;
use crate::error::{GStoreError, Result};
use crate::kvstore::Pager;
use crate::parser::import::{ErrorLog, ImportOptions};

use super::budget::Budget;
use super::pipeline::{self, Ctx};
use super::scratch::Scratch;
use super::{BulkInput, BulkOptions, BulkReport, PhaseTime};

/// The staging directory inside the database directory.
pub const STAGING_DIR: &str = ".bulk-tmp";
/// The commit marker inside the staging directory.
const COMMIT_MARKER: &str = "COMMIT";
/// The disk layout's files (`db::KV_FILE`, `db::VSTREE_KV_FILE`).
const KV_FILE: &str = "kvstore.kv";
const VSTREE_KV_FILE: &str = "vstree.kv";
/// The snapshot layout's files (`db::persist::SNAPSHOT_FILES`).
const SNAPSHOT_FILES: [&str; 5] = [
    "dict.bin",
    "store.bin",
    "named.bin",
    "vstree.bin",
    "meta.bin",
];

/// The install steps, in order (see the module docs).
pub const CRASH_POINTS: [&str; 8] = [
    "staged",
    "marked",
    "cleared",
    "kv-renamed",
    "kv-synced",
    "vstree-placed",
    "layouts-removed",
    "cleaned",
];

/// The environment variable naming the step after which an install aborts
/// the process (crash tests).
#[doc(hidden)]
pub const CRASH_ENV: &str = "GSTORE_BULK_CRASH";

/// Per-directory simulated crash points (see [`debug_crash_at`]).
fn hooks() -> &'static Mutex<Vec<(PathBuf, String)>> {
    static HOOKS: OnceLock<Mutex<Vec<(PathBuf, String)>>> = OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Make the next bulk builds of directory `dir` in this process stop right
/// after install step `point` (one of [`CRASH_POINTS`]) with an error,
/// leaving the directory exactly as a crash there would; `None` clears it.
/// A test hook: it costs one lookup per install and nothing while unset.
#[doc(hidden)]
pub fn debug_crash_at(dir: &Path, point: Option<&str>) {
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut h = hooks().lock().unwrap_or_else(PoisonError::into_inner);
    h.retain(|(d, _)| *d != dir);
    if let Some(p) = point {
        h.push((dir, p.to_string()));
    }
}

/// The crash points armed for one install.
struct Failpoints {
    abort_at: Option<String>,
    fail_at: Option<String>,
    crashed: bool,
}

impl Failpoints {
    fn load(dir: &Path) -> Failpoints {
        let canonical = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let fail_at = {
            let h = hooks().lock().unwrap_or_else(PoisonError::into_inner);
            h.iter()
                .find(|(d, _)| *d == canonical)
                .map(|(_, p)| p.clone())
        };
        Failpoints {
            abort_at: std::env::var(CRASH_ENV).ok().filter(|s| !s.is_empty()),
            fail_at,
            crashed: false,
        }
    }

    fn hit(&mut self, point: &str) -> Result<()> {
        debug_assert!(CRASH_POINTS.contains(&point));
        if self.abort_at.as_deref() == Some(point) {
            eprintln!("{CRASH_ENV}: aborting after bulk install step '{point}'");
            std::process::abort();
        }
        if self.fail_at.as_deref() == Some(point) {
            self.crashed = true;
            return Err(GStoreError::Database(format!(
                "bulk build: simulated crash after install step '{point}'"
            )));
        }
        Ok(())
    }
}

/// `<path>.wal`, the pager's write-ahead log beside a paged file.
fn wal_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".wal");
    PathBuf::from(s)
}

fn sync_file(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

/// Build a disk database in `dir` (see [`crate::bulk::build`]).
pub(crate) fn build_disk(
    dir: &Path,
    input: &BulkInput<'_>,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    bulk: &BulkOptions,
) -> Result<BulkReport> {
    let started = Instant::now();
    fs::create_dir_all(dir)?;
    let _lock = DirLock::acquire(dir)?;
    let _guard = BuildGuard::enter(dir)?;
    let staging = dir.join(STAGING_DIR);
    if staging.exists() {
        // What a crashed build left: never part of a database.
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    if let Some(t) = &bulk.temp_dir {
        fs::create_dir_all(t)?;
    }
    let threads = bulk.effective_threads();
    let budget = Budget::new(bulk.memory_budget);
    let scratch = Scratch::new(bulk.temp_dir.as_deref().unwrap_or(&staging));
    let kv_new = staging.join(KV_FILE);
    let vs_new = staging.join(VSTREE_KV_FILE);
    let ctx = Ctx {
        budget: &budget,
        scratch: &scratch,
        fill_percent: bulk.fill_percent,
        threads,
    };
    let built = pipeline::build_image(&kv_new, &vs_new, input, opts, log, &ctx);
    drop(scratch);
    let img = match built {
        Ok(img) => img,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let t = Instant::now();
    let mut fp = Failpoints::load(dir);
    if let Err(e) = install(dir, &staging, &mut fp) {
        if !fp.crashed {
            let _ = fs::remove_dir_all(&staging);
        }
        return Err(e);
    }
    let mut phases = img.phases;
    phases.push(PhaseTime {
        name: "install",
        millis: t.elapsed().as_secs_f64() * 1e3,
    });
    Ok(BulkReport {
        import: img.import,
        triples: img.triples,
        quads: img.quads,
        entities: img.entities,
        literals: img.literals,
        predicates: img.predicates,
        vstree_entities: img.vstree_entities,
        kv_bytes: img.kv_pages * crate::kvstore::PAGE_SIZE as u64,
        vstree_bytes: img.vstree_pages * crate::kvstore::PAGE_SIZE as u64,
        term_map_bytes: img.term_map_bytes as u64,
        budget_bytes: budget.total() as u64,
        budget_peak_bytes: budget.peak() as u64,
        spilled_runs: budget.spilled_runs(),
        spilled_bytes: budget.spilled_bytes(),
        threads,
        phases,
        millis: started.elapsed().as_secs_f64() * 1e3,
    })
}

/// Make the staged `kvstore.kv` newer than any snapshot file in `dir`, so a
/// snapshot a crash leaves beside it reads as a stale legacy shadow.
fn newer_than_snapshots(dir: &Path, staged_kv: &Path) -> Result<()> {
    let newest = SNAPSHOT_FILES
        .iter()
        .filter_map(|n| fs::metadata(dir.join(n)).and_then(|m| m.modified()).ok())
        .max();
    let Some(newest) = newest else {
        return Ok(());
    };
    let mine = fs::metadata(staged_kv)?.modified()?;
    if mine <= newest {
        let later = newest + Duration::from_secs(1);
        let f = File::options().write(true).open(staged_kv)?;
        f.set_modified(later.max(SystemTime::now()))?;
        f.sync_all()?;
    }
    Ok(())
}

/// The install steps (see the module docs).
fn install(dir: &Path, staging: &Path, fp: &mut Failpoints) -> Result<()> {
    let kv_new = staging.join(KV_FILE);
    let vs_new = staging.join(VSTREE_KV_FILE);
    sync_file(&kv_new)?;
    sync_file(&vs_new)?;
    sync_dir(staging)?;
    fp.hit("staged")?;

    let marker = staging.join(COMMIT_MARKER);
    {
        let mut f = File::create(&marker)?;
        f.write_all(format!("{KV_FILE}\n{VSTREE_KV_FILE}\n").as_bytes())?;
        f.sync_all()?;
    }
    sync_dir(staging)?;
    fp.hit("marked")?;

    let kv = dir.join(KV_FILE);
    let vs = dir.join(VSTREE_KV_FILE);
    let kv_wal = wal_of(&kv);
    if kv_wal.exists() {
        // A committed batch the old store has not applied yet: replay it, as
        // the next open would, so removing the log loses nothing; a log
        // beside no store (or one this binary cannot open) is just removed.
        if kv.is_file() {
            drop(Pager::open(&kv, 8));
        }
        remove_file_if_exists(&kv_wal)?;
    }
    remove_file_if_exists(&wal_of(&vs))?;
    remove_file_if_exists(&vs)?;
    sync_dir(dir)?;
    fp.hit("cleared")?;

    newer_than_snapshots(dir, &kv_new)?;
    fs::rename(&kv_new, &kv)?;
    fp.hit("kv-renamed")?;
    sync_dir(dir)?;
    fp.hit("kv-synced")?;

    fs::rename(&vs_new, &vs)?;
    sync_dir(dir)?;
    fp.hit("vstree-placed")?;

    remove_other_layouts(dir, StorageKind::Disk)?;
    fp.hit("layouts-removed")?;

    fs::remove_dir_all(staging)?;
    sync_dir(dir)?;
    fp.hit("cleaned")?;
    Ok(())
}
