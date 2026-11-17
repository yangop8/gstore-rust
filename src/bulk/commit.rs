//! The staged, crash-safe install of a bulk build.
//!
//! The build claims the directory with [`BuildGuard::lock`], which takes
//! its [`DirLock`](crate::db::dirlock::DirLock) in the same step (another
//! process holding the directory makes it fail, and so do a database of
//! this process that has it open and another bulk build of this process;
//! while the claim lives no other thread of this process can open it), and
//! writes everything into `<dir>/.bulk-tmp/`, which no layout detection
//! looks at. A stale `.bulk-tmp/` from a crashed build is
//! removed first. Once both files are complete and `fsync`ed, the install
//! runs; after each step a crash leaves a directory that an unmodified
//! binary opens as either the old database or the new one:
//!
//! | # | step | a crash after it leaves |
//! |---|------|-------------------------|
//! | 1 `staged` | `kvstore.kv` + `vstree.kv` complete and synced in `.bulk-tmp/` | the old database |
//! | 2 `marked` | `.bulk-tmp/COMMIT` written and synced (advisory, see below) | the old database |
//! | 3 `cleared` | a stale `kvstore.kv.wal` replayed into the old file (what any open does) and removed — a replay that fails stops the install with the log in place; the old `vstree.kv` and its WAL removed; synced | the old database (a missing `vstree.kv` is rebuilt at open) |
//! | 4 `kv-renamed` | `.bulk-tmp/kvstore.kv` renamed over `kvstore.kv` | the new database (no `vstree.kv` yet: rebuilt at open), or — under an existing `rocksdb/`, which detection prefers — still the old one |
//! | 5 `kv-synced` | directory synced | as 4, durably |
//! | 6 `vstree-placed` | the replaced database's reasoning rules (`reason_rule_files/`) removed, synced; `vstree.kv` renamed into place, synced | as 4, with the new VS-tree and without the old rules |
//! | 7 `layouts-removed` | the other layouts removed (snapshot files; `rocksdb/` renamed aside durably, then deleted) | the new database |
//! | 8 `cleaned` | `.bulk-tmp/` deleted, synced | the new database |
//!
//! The old `vstree.kv` goes before the new `kvstore.kv` arrives, so a new
//! store never meets an old VS-tree (or the reverse) marked fresh. A
//! snapshot left beside the new `kvstore.kv` by a crash between 4 and 7 is
//! older than it (the install makes sure of that), so the open treats it as
//! a stale legacy shadow and sets it aside. A strict parse failure, or any
//! other error before step 4, leaves the old database untouched. Step 4 is
//! the commit point: an error after it does not fail the build (the new
//! database is live) — the remaining steps still run and the errors come
//! back as [`BulkReport::warnings`] (a failed step-5 sync stops there: what
//! follows must not assume the rename durable). The old rules go right after
//! the new store is durable; a crash in that short window leaves them
//! beside the new database.
//!
//! **The `COMMIT` marker is advisory** (review durability#12, bulkcli#15):
//! it records that the staged files were complete and synced when the
//! install began — a forensic aid for whoever finds a `.bulk-tmp/` a crash
//! left behind — and nothing reads it. No recovery rolls an interrupted
//! install forward: before step 4 the old database is the database (the
//! next build, or `gbuild --force`, deletes the staging area, marked or
//! not), and from step 4 on the new store is in place without it. The
//! format is unchanged (D7); the step costs one fsync.
//!
//! Crash tests: the `GSTORE_BULK_CRASH=<step>` environment variable — a
//! test failpoint live in every build, reported on standard error whenever
//! it is set ([`CRASH_ENV`]) — aborts the process right after that step
//! (read once per install, never on the hot path), and [`debug_crash_at`]
//! makes an in-process build of one directory stop there with an error,
//! leaving its files as a crash would.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use crate::db::dirlock::BuildGuard;
use crate::db::fsutil::{remove_file_if_exists, sync_dir};
use crate::db::persist::remove_other_layouts;
use crate::db::StorageKind;
use crate::error::{GStoreError, Result};
use crate::kvstore::Pager;
use crate::parser::import::{ErrorLog, ImportOptions};

use super::budget::Budget;
use super::pipeline::{self, Ctx};
use super::scratch::{self, Scratch};
use super::{BulkInput, BulkOptions, BulkReport, PhaseTime};

/// The staging directory inside the database directory.
pub const STAGING_DIR: &str = ".bulk-tmp";
/// The install step whose warning leaves nothing an open reads (see
/// [`BulkReport::opens_as_built`]): the staging area not removed — no
/// layout detection reads it, and the next build removes it.
pub(crate) const STAGING_CLEANUP: &str = "removing the staging area";
/// The commit marker inside the staging directory: advisory, read by no
/// recovery (see the module docs).
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
/// the process — a **test failpoint** of the crash tests, live in every
/// build (review tests-a#19). Set, it makes every bulk install of the
/// process (`gbuild --disk` / `--rocksdb`, a server's bulk build) say so on
/// standard error and abort right after that step, leaving the old or the
/// new database; unset it outside crash tests. A value that names no
/// install step of the build is reported and ignored.
#[doc(hidden)]
pub const CRASH_ENV: &str = "GSTORE_BULK_CRASH";

/// The install step [`CRASH_ENV`] names among `points`, reported on standard
/// error whenever the variable is set (an operator who left it in an
/// environment sees why the build aborts, or that it is ignored).
pub(crate) fn crash_env(points: &[&str]) -> Option<String> {
    let v = std::env::var(CRASH_ENV).ok().filter(|s| !s.is_empty())?;
    if points.contains(&v.as_str()) {
        eprintln!(
            "{CRASH_ENV}={v} is set: this bulk build aborts right after its install step '{v}' \
             (a crash-test failpoint; unset it outside crash tests)"
        );
        Some(v)
    } else {
        eprintln!(
            "{CRASH_ENV}={v} is set but names no install step of this bulk build ({}); ignored",
            points.join(", ")
        );
        None
    }
}

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
            abort_at: crash_env(&CRASH_POINTS),
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

/// Remove the replaced database's reasoning rules from `dir` (durably),
/// under the rule directory's lock: no `reasonManage` file operation of
/// this process (a server's) is half-way through it meanwhile. Only this
/// step holds it — not the whole build, which a `reasonManage` request
/// would otherwise wait for. The directory is renamed aside before it is
/// deleted, so a crash part-way leaves no rule without its ledger
/// ([`crate::reason::store::remove_rule_dir`]).
pub(crate) fn remove_rule_dir(dir: &Path) -> Result<()> {
    crate::reason::store::remove_rule_dir(dir)
}

/// `<path>.wal`, the pager's write-ahead log beside a paged file (the rule
/// lives in [`Pager::wal_path`] only, review disk#9).
fn wal_of(path: &Path) -> PathBuf {
    Pager::wal_path(path)
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
    // The claim and the lock in one step (review bulkcli#13): refused while
    // a database of this process has the directory open (replacing its
    // files under it would strand its later writes, and a WAL of the old
    // store could replay into the new one), while another build of this
    // process rebuilds it, or while another process holds it — and while
    // the claim lives no other thread of this process opens it.
    let (_guard, _lock) = BuildGuard::lock(dir)?;
    // `Database::open` takes the directory lock for the RocksDB layout too,
    // but an older binary does not (it relies on RocksDB's own
    // `rocksdb/LOCK`): the lock above cannot see it serving the store.
    // Replacing its directory under it would lose every write it
    // acknowledges. Checked again right before the install.
    refuse_open_store(dir)?;
    let staging = dir.join(STAGING_DIR);
    if staging.exists() {
        // What a crashed build left: never part of a database.
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    if let Some(t) = &bulk.temp_dir {
        fs::create_dir_all(t)?;
        // Spill directories that crashed or killed builds left in a shared
        // temp directory (live builds' are locked and kept; the default
        // scratch inside the staging area went with it above).
        scratch::sweep_stale(t);
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
        free_ids: bulk.free_ids,
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
    let warnings = match install(dir, &staging, &mut fp) {
        Ok(w) => w,
        Err(e) => {
            if !fp.crashed {
                let _ = fs::remove_dir_all(&staging);
            }
            return Err(e);
        }
    };
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
        warnings,
    })
}

/// Install a disk database another builder staged in `staging`
/// (`<dir>/`[`STAGING_DIR`], holding a complete `kvstore.kv` and
/// `vstree.kv`) with this module's install steps — how the incremental
/// disk build replaces a database (review durability#5), so a failed or
/// interrupted rebuild leaves the old database as a bulk build does. The
/// caller holds `dir`'s [`DirLock`](crate::db::dirlock::DirLock) and
/// [`BuildGuard`]. An error before the commit point is returned with the
/// old database in place (the staging area removed); errors after it come
/// back as warnings.
pub(crate) fn install_staged_disk(dir: &Path, staging: &Path) -> Result<Vec<String>> {
    let mut fp = Failpoints::load(dir);
    install(dir, staging, &mut fp).inspect_err(|_| {
        if !fp.crashed {
            let _ = fs::remove_dir_all(staging);
        }
    })
}

/// Who holds the lock of a RocksDB store in `dir` (`rocksdb/LOCK`, or that of
/// a staged `rocksdb.new/`), if anyone does: RocksDB holds it — a POSIX
/// record lock — for as long as the store is open, in any process. Read from
/// the kernel's lock table (`/proc/locks`), so it sees every process on this
/// host whatever its pid namespace; `None` when there is no such store, its
/// lock is free, or the table cannot be read (a best-effort safety net, like
/// the directory lock on a file system without advisory locks). Shared by
/// the disk and the RocksDB installs.
#[cfg(target_os = "linux")]
pub(crate) fn rocksdb_lock_holder(dir: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let locks = ["rocksdb", "rocksdb.new"]
        .iter()
        .filter_map(|sub| fs::metadata(dir.join(sub).join("LOCK")).ok())
        .map(|m| {
            let dev = m.dev();
            let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
            let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
            (major, minor, m.ino())
        })
        .collect::<Vec<_>>();
    if locks.is_empty() {
        return None;
    }
    let table = fs::read_to_string("/proc/locks").ok()?;
    for line in table.lines() {
        // `1: POSIX  ADVISORY  WRITE 4242 08:01:1234 0 EOF` (a blocked
        // waiter's line has a `->` before its class).
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(at) = fields.iter().position(|f| f.matches(':').count() == 2) else {
            continue;
        };
        let mut id = fields[at].split(':');
        let (Some(major), Some(minor), Some(ino)) = (id.next(), id.next(), id.next()) else {
            continue;
        };
        let (Ok(major), Ok(minor), Ok(ino)) = (
            u64::from_str_radix(major, 16),
            u64::from_str_radix(minor, 16),
            ino.parse::<u64>(),
        ) else {
            continue;
        };
        if locks.contains(&(major, minor, ino)) {
            let pid = fields.get(at.wrapping_sub(1)).copied().unwrap_or("?");
            return Some(format!("its rocksdb/LOCK is held by process {pid}"));
        }
    }
    None
}

/// Elsewhere the kernel's lock table is not read: RocksDB's own lock still
/// refuses a second open, but an open store is not detected here.
#[cfg(not(target_os = "linux"))]
pub(crate) fn rocksdb_lock_holder(_dir: &Path) -> Option<String> {
    None
}

/// Refuse to build over `dir` while a RocksDB store in it is open anywhere
/// ([`rocksdb_lock_holder`]). Checked when the build starts and again right
/// before its install replaces anything: a process that opened the store
/// while the input was parsed and sorted — an older binary takes only
/// `rocksdb/LOCK`, not the directory lock — is seen too, and the old
/// database is kept (only the few milliseconds of the install itself are
/// left uncovered).
pub(crate) fn refuse_open_store(dir: &Path) -> Result<()> {
    match rocksdb_lock_holder(dir) {
        None => Ok(()),
        Some(holder) => Err(GStoreError::Database(format!(
            "the RocksDB store in '{}' is in use ({holder}); close it before rebuilding",
            dir.display()
        ))),
    }
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

/// Record `r`'s error, if any, as a warning about the cleanup step `what`.
fn note(warnings: &mut Vec<String>, what: &str, r: Result<()>) -> bool {
    match r {
        Ok(()) => true,
        Err(e) => {
            warnings.push(format!("{what}: {e}"));
            false
        }
    }
}

/// The install steps (see the module docs). An error before the commit
/// point (step 4, `kv-renamed`) leaves the old database and is returned.
/// From there on the new database is in place: the remaining steps all run
/// and their errors come back as warnings, with `Ok` — the caller reports
/// the new database as built, with its cleanup incomplete.
fn install(dir: &Path, staging: &Path, fp: &mut Failpoints) -> Result<Vec<String>> {
    let kv_new = staging.join(KV_FILE);
    let vs_new = staging.join(VSTREE_KV_FILE);
    sync_file(&kv_new)?;
    sync_file(&vs_new)?;
    sync_dir(staging)?;
    fp.hit("staged")?;
    // Again, right before anything is replaced (see `refuse_open_store`).
    refuse_open_store(dir)?;

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
        // beside no store is just removed. A replay that fails part-way
        // (ENOSPC, EIO, …) may have written some of the batch: only the log
        // can finish it, so the install stops here with the log in place —
        // the old database stays recoverable (its next open replays the log
        // again). A store whose log was replayed but whose header this binary
        // cannot read is replaced anyway.
        if kv.is_file() {
            let opened = Pager::open(&kv, 8).map(drop);
            if let (Err(e), true) = (opened, kv_wal.exists()) {
                return Err(GStoreError::Database(format!(
                    "cannot replay the write-ahead log '{}' of the database being replaced \
                     (left in place: the old database's next open replays it): {e}",
                    kv_wal.display()
                )));
            }
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

    // The commit point is passed: the new database is in place.
    let mut warnings = Vec::new();
    if !note(
        &mut warnings,
        "making the new kvstore.kv durable (the rest of the install was skipped; \
         the next build cleans up)",
        sync_dir(dir),
    ) {
        // Nothing may assume the rename is durable: placing the new
        // `vstree.kv` (or dropping the old rules) could otherwise meet the
        // old store after a crash.
        return Ok(warnings);
    }
    fp.hit("kv-synced")?;

    // The replaced database's reasoning rules: their ledgers name the old
    // data's triples, and executing or removing such a rule would delete
    // triples the new data asserts. Removed as soon as the new store is
    // durably in place.
    note(
        &mut warnings,
        "removing the replaced database's reasoning rules",
        remove_rule_dir(dir),
    );
    note(
        &mut warnings,
        "placing the new vstree.kv (it is rebuilt at open)",
        fs::rename(&vs_new, &vs)
            .map_err(GStoreError::from)
            .and_then(|()| sync_dir(dir)),
    );
    fp.hit("vstree-placed")?;

    note(
        &mut warnings,
        "removing the other layouts",
        remove_other_layouts(dir, StorageKind::Disk),
    );
    fp.hit("layouts-removed")?;

    note(
        &mut warnings,
        STAGING_CLEANUP,
        fs::remove_dir_all(staging)
            .map_err(GStoreError::from)
            .and_then(|()| sync_dir(dir)),
    );
    fp.hit("cleaned")?;
    Ok(warnings)
}
