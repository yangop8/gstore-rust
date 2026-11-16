//! Crash consistency of the library's redo and update logs and of the
//! incremental backups built on them (review durability#6, db#8, robust#10,
//! db#9, db#2, db#3, gap-doc-claims#1, db#13, db#14).
//!
//! The logs are library API only (`enable_redo_log*`, `enable_update_log*`,
//! `replay_*`, `backup_incremental`, `restore_incremental`); no server or
//! CLI turns them on. Crashes in the middle of an append are real ones where
//! it matters: a child process (a re-run of this test binary limited to one
//! `#[ignore]`d child test) whose `RLIMIT_FSIZE` stops the append part-way —
//! killed by `SIGXFSZ` for a crash, or with `SIGXFSZ` ignored for a write
//! that fails with `EFBIG`, like a full disk.

#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gstore::{Database, DiskQueryMode, OpenOptions, QueryResult, Term, Triple};

// ---- helpers -------------------------------------------------------------------

/// A temporary directory, removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_redo_durability_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The file-size limit and the signal it raises (Linux, 64-bit `rlim_t`).
mod rlimit {
    #[repr(C)]
    struct RLimit {
        cur: u64,
        max: u64,
    }

    extern "C" {
        fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
        fn getrlimit(resource: i32, rlim: *mut RLimit) -> i32;
        fn signal(sig: i32, handler: usize) -> usize;
    }

    const RLIMIT_FSIZE: i32 = 1;
    const RLIMIT_CORE: i32 = 4;
    pub const SIGXFSZ: i32 = 25;
    const SIG_IGN: usize = 1;

    /// Ignore `SIGXFSZ`, so exceeding the limit fails the write with `EFBIG`
    /// instead of killing the process.
    pub fn ignore_sigxfsz() {
        // SAFETY: installs the predefined SIG_IGN disposition.
        assert_ne!(unsafe { signal(SIGXFSZ, SIG_IGN) }, usize::MAX);
    }

    /// No core file when `SIGXFSZ` kills the process.
    pub fn no_core_dumps() {
        let zero = RLimit { cur: 0, max: 0 };
        // SAFETY: a plain syscall wrapper on a properly sized struct.
        assert_eq!(unsafe { setrlimit(RLIMIT_CORE, &zero) }, 0);
    }

    /// Set the soft file-size limit of this process to `bytes` (the hard
    /// limit stays, so the soft one can be raised again).
    pub fn limit_file_size(bytes: u64) {
        let mut cur = RLimit { cur: 0, max: 0 };
        // SAFETY: plain syscall wrappers on a properly sized struct.
        unsafe {
            assert_eq!(getrlimit(RLIMIT_FSIZE, &mut cur), 0);
            let new = RLimit {
                cur: bytes.min(cur.max),
                max: cur.max,
            };
            assert_eq!(setrlimit(RLIMIT_FSIZE, &new), 0);
        }
    }
}

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/c> <http://ex/p> <http://ex/d> .\n";

fn base() -> Database {
    Database::build_from_str("redo", BASE).unwrap()
}

/// `<http://ex/{tag}> <http://ex/p> <http://ex/o>`.
fn t(tag: &str) -> Triple {
    Triple::new(
        Term::iri(format!("http://ex/{tag}")),
        Term::iri("http://ex/p"),
        Term::iri("http://ex/o"),
    )
}

fn ins(tags: &[&str]) -> String {
    let body: String = tags
        .iter()
        .map(|tag| format!("<http://ex/{tag}> <http://ex/p> <http://ex/o> . "))
        .collect();
    format!("INSERT DATA {{ {body} }}")
}

fn ask(db: &mut Database, q: &str) -> bool {
    match db.query(q).unwrap() {
        QueryResult::Ask(b) => b,
        other => panic!("{other:?}"),
    }
}

fn has(db: &mut Database, tag: &str) -> bool {
    ask(
        db,
        &format!("ASK {{ <http://ex/{tag}> <http://ex/p> <http://ex/o> }}"),
    )
}

fn len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn cut(path: &Path, to: u64) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(to)
        .unwrap();
}

/// Replay a redo log onto a fresh base database.
fn replayed(log: &Path) -> (usize, Database) {
    let mut db = base();
    let n = db
        .replay_redo_log(log)
        .unwrap_or_else(|e| panic!("replaying {}: {e}", log.display()));
    (n, db)
}

/// A redo record in the format written before checksums (and by every
/// version up to this one): `REDO <op> <glen> <tlen>[ G]\n<g><t>\n`.
fn legacy_redo(op: char, graph: Option<&str>, triple: &str) -> String {
    let g = graph.unwrap_or("");
    let flag = if graph.is_some() { " G" } else { "" };
    format!(
        "REDO {op} {} {}{flag}\n{g}{triple}\n",
        g.len(),
        triple.len()
    )
}

// ---- torn tails ----------------------------------------------------------------

/// durability#6 / db#8 / robust#10: a crash in the middle of an append
/// leaves a torn last record. Replay used to fail on it ("truncated
/// redo.log body"), and the next session appended after the torn bytes, so
/// every later record was unreachable too.
#[test]
fn a_torn_redo_tail_is_skipped_by_replay_and_cut_by_the_next_append() {
    let tmp = TmpDir::new("torn_redo");
    let log = tmp.path("redo.log");
    let mut db = base();
    db.enable_redo_log(&log);
    db.query(&ins(&["x1"])).unwrap();
    db.query(&ins(&["x2"])).unwrap();
    drop(db);
    cut(&log, len(&log) - 5);

    let (n, mut recovered) = replayed(&log);
    assert_eq!(n, 1, "the whole record replays, the torn one is skipped");
    assert!(has(&mut recovered, "x1") && !has(&mut recovered, "x2"));

    // The recovered database logs on; its first append starts on the last
    // whole record.
    recovered.enable_redo_log(&log);
    recovered.query(&ins(&["x3"])).unwrap();
    let (n, mut again) = replayed(&log);
    assert_eq!(n, 2);
    assert!(has(&mut again, "x1") && has(&mut again, "x3") && !has(&mut again, "x2"));
}

/// durability#6: the same for the statement-level update log.
#[test]
fn a_torn_update_log_tail_is_skipped_by_replay_and_cut_by_the_next_append() {
    let tmp = TmpDir::new("torn_update");
    let log = tmp.path("update.log");
    let mut db = base();
    db.enable_update_log(&log);
    db.query(&ins(&["x1"])).unwrap();
    db.query(&ins(&["x2"])).unwrap();
    drop(db);
    cut(&log, len(&log) - 4);

    let mut recovered = base();
    assert_eq!(recovered.replay_update_log(&log).unwrap(), 1);
    assert!(has(&mut recovered, "x1") && !has(&mut recovered, "x2"));
    recovered.enable_update_log(&log);
    recovered.query(&ins(&["x3"])).unwrap();
    let mut again = base();
    assert_eq!(again.replay_update_log(&log).unwrap(), 2);
    assert!(has(&mut again, "x3") && !has(&mut again, "x2"));
}

/// durability#6: the records of one commit replay all or nothing — a
/// transaction whose append was cut anywhere is not replayed in part (it
/// was never acknowledged).
#[test]
fn a_commit_cut_anywhere_replays_all_or_nothing() {
    let tmp = TmpDir::new("txn_cut");
    let log = tmp.path("redo.log");
    let mut db = base();
    db.enable_redo_log(&log);
    db.query(&ins(&["first"])).unwrap();
    let first = len(&log);
    db.begin().unwrap();
    db.query(&ins(&["t1", "t2", "t3"])).unwrap();
    db.query("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }")
        .unwrap();
    db.commit().unwrap();
    let full = fs::read(&log).unwrap();
    let (n, mut whole) = replayed(&log);
    assert_eq!(n, 5);
    assert!(has(&mut whole, "t3"));
    for at in first + 1..full.len() as u64 {
        fs::write(&log, &full[..at as usize]).unwrap();
        let (n, mut r) = replayed(&log);
        assert_eq!(n, 1, "cut at {at}: only the first commit replays");
        assert!(
            !has(&mut r, "t1"),
            "cut at {at}: a partial transaction replayed"
        );
        assert_eq!(r.triple_num(), 3, "cut at {at}");
    }
}

/// durability#6 (checksums): a record damaged in the middle of the log is
/// corruption, reported by replay — not applied, not skipped.
#[test]
fn a_damaged_record_in_the_middle_of_the_log_fails_the_replay() {
    let tmp = TmpDir::new("corrupt");
    let log = tmp.path("redo.log");
    let mut db = base();
    db.enable_redo_log(&log);
    db.query(&ins(&["x1"])).unwrap();
    let first = len(&log) as usize;
    db.query(&ins(&["x2"])).unwrap();
    drop(db);
    let mut bytes = fs::read(&log).unwrap();
    // `x1` -> `y1` in the first record's body.
    let at = bytes[..first]
        .windows(6)
        .position(|w| w == b"ex/x1>")
        .unwrap()
        + 3;
    bytes[at] = b'y';
    fs::write(&log, &bytes).unwrap();
    let err = base().replay_redo_log(&log).unwrap_err().to_string();
    assert!(err.contains("corrupt"), "{err}");
}

// ---- real crashes and failed appends (child processes) -------------------------

const CHILD_SCENARIO: &str = "GSTORE_REDO_CHILD_SCENARIO";
const CHILD_DIR: &str = "GSTORE_REDO_CHILD_DIR";
const CHILD_MODE: &str = "GSTORE_REDO_CHILD_MODE";
const CHILD_OK: &str = "redo child finished";

/// Run `scenario` of [`child_redo_scenarios`] in a child process.
fn run_child(scenario: &str, dir: &Path, mode: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "child_redo_scenarios",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SCENARIO, scenario)
        .env(CHILD_DIR, dir)
        .env(CHILD_MODE, mode)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn child_ok(out: &std::process::Output, what: &str) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains(CHILD_OK),
        "child {what} failed: {:?}\n{stdout}\n{stderr}",
        out.status
    );
}

/// The child halves of the scenarios below (a no-op unless run by
/// [`run_child`]).
#[test]
#[ignore = "run in a child process by the scenarios of this file (RLIMIT_FSIZE)"]
fn child_redo_scenarios() {
    let Ok(scenario) = std::env::var(CHILD_SCENARIO) else {
        return;
    };
    let dir = PathBuf::from(std::env::var(CHILD_DIR).unwrap());
    match scenario.as_str() {
        "crash" => child_crash_mid_append(&dir),
        "efbig" => child_failed_append(&dir),
        "commit_point" => child_failed_commit_point(&dir),
        other => panic!("unknown scenario {other}"),
    }
    println!("{CHILD_OK}");
}

/// Ten statements, then a transaction whose append the file-size limit
/// stops part-way: `SIGXFSZ` kills the process there.
fn child_crash_mid_append(dir: &Path) {
    let log = dir.join("redo.log");
    let mut db = base();
    db.enable_redo_log(&log);
    for i in 0..10 {
        db.query(&ins(&[&format!("x{i}")])).unwrap();
    }
    db.begin().unwrap();
    db.query(&ins(&["t1", "t2", "t3", "t4"])).unwrap();
    rlimit::no_core_dumps();
    rlimit::limit_file_size(len(&log) + 60);
    let _ = db.commit();
    println!("not killed");
}

/// durability#6 / db#8: a crash (here `SIGXFSZ`) in the middle of an
/// append leaves a torn transaction at the end of the log: replay applies
/// every whole commit and none of the torn one, and the next session's
/// append starts after the last whole commit.
#[test]
fn a_crash_mid_append_leaves_a_log_whose_whole_commits_replay() {
    let tmp = TmpDir::new("crash");
    let log = tmp.path("redo.log");
    let out = run_child("crash", &tmp.0, "");
    assert_eq!(
        out.status.signal(),
        Some(rlimit::SIGXFSZ),
        "the child must die in the append: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout)
    );
    let (n, mut recovered) = replayed(&log);
    assert_eq!(
        n, 10,
        "the ten whole commits replay, the torn one not at all"
    );
    assert!(has(&mut recovered, "x9") && !has(&mut recovered, "t1"));
    recovered.enable_redo_log(&log);
    recovered.query(&ins(&["after"])).unwrap();
    let (n, mut again) = replayed(&log);
    assert_eq!(n, 11);
    assert!(has(&mut again, "after") && !has(&mut again, "t1"));
}

/// The redo log stops growing (`EFBIG`, `SIGXFSZ` ignored), then has room
/// again.
fn child_failed_append(dir: &Path) {
    rlimit::ignore_sigxfsz();
    let log = dir.join("redo.log");
    let mut db = base();
    db.enable_redo_log(&log);
    db.query(&ins(&["x1"])).unwrap();
    let whole = len(&log);
    rlimit::limit_file_size(whole + 20);
    // An UPDATE is committed, then its append fails: reported.
    let err = db
        .query(&ins(&["x2"]))
        .expect_err("a failed redo append is reported");
    assert!(err.to_string().contains("redo.log"), "{err}");
    assert!(has(&mut db, "x2"), "the update itself is committed");
    assert_eq!(len(&log), whole, "the partial append was cut off again");
    // A direct-API write cannot report it; the commit is owed to the log.
    assert!(db.insert_triple(&t("x3")));
    assert_eq!(len(&log), whole);
    // While commits are owed, an increment would miss them: refused; a
    // commit point reports the failure.
    let inc = dir.join("inc");
    let err = db
        .backup_incremental(&inc)
        .expect_err("an increment with a gap");
    assert!(err.to_string().contains("redo.log"), "{err}");
    assert!(db.sync().is_err());
    // Room again: the owed commits are written first, in order.
    rlimit::limit_file_size(u64::MAX);
    db.query(&ins(&["x4"])).unwrap();
    db.backup_incremental(&inc).unwrap();
    println!("live triples {}", db.triple_num());
}

/// robust#10 / durability#6: an append that fails part-way (a full disk)
/// is reported and cut off, its records are kept and written first by the
/// next append, and no increment is taken across the gap.
#[test]
fn a_failed_append_is_reported_cut_off_and_retried_in_order() {
    let tmp = TmpDir::new("efbig");
    let log = tmp.path("redo.log");
    let out = run_child("efbig", &tmp.0, "");
    child_ok(&out, "efbig");
    let (n, mut recovered) = replayed(&log);
    assert_eq!(n, 4);
    for x in ["x1", "x2", "x3", "x4"] {
        assert!(has(&mut recovered, x), "{x} replays");
    }
    // The increment taken once the log caught up covers all of it.
    let mut restored = base();
    let inc = fs::read(tmp.path("inc/incr.log")).unwrap();
    let inc_log = tmp.path("inc.log");
    fs::write(&inc_log, inc).unwrap();
    assert_eq!(restored.replay_redo_log(&inc_log).unwrap(), 4);
}

const DISK_BASE: usize = 3000;
const DISK_UPDATE: usize = 300;

fn disk_update() -> String {
    let body: String = (0..DISK_UPDATE)
        .map(|i| format!("<http://ex/u/s{i}> <http://ex/u/p> \"update value {i}\" . "))
        .collect();
    format!("INSERT DATA {{ {body} }}")
}

/// An update whose commit point fails: `kvstore.kv` cannot grow.
fn child_failed_commit_point(dir: &Path) {
    rlimit::ignore_sigxfsz();
    let mode = match std::env::var(CHILD_MODE).unwrap().as_str() {
        "stream" => DiskQueryMode::Stream,
        _ => DiskQueryMode::Materialize,
    };
    let opts = OpenOptions {
        disk_query_mode: mode,
        ..OpenOptions::default()
    };
    let db_dir = dir.join("t.db");
    let mut db = Database::open_with(&db_dir, &opts).unwrap();
    db.enable_redo_log(dir.join("redo.log"));
    db.enable_update_log(dir.join("update.log"));
    db.query(&ins(&["before"])).unwrap();
    rlimit::limit_file_size(len(&db_dir.join("kvstore.kv")));
    let r = db.query(&disk_update());
    assert!(r.is_err(), "the update's commit point must fail: {r:?}");
    std::mem::forget(db);
}

/// durability#6 / db#8: the logs are written after the commit point. An
/// update whose commit point fails is not known to be durable, so it is in
/// neither log (it used to be appended to both before its commit point).
#[test]
fn an_update_whose_commit_point_failed_is_in_neither_log() {
    for mode in ["memory", "stream"] {
        let tmp = TmpDir::new(&format!("commit_point_{mode}"));
        let nt = tmp.path("base.nt");
        let data: String = (0..DISK_BASE)
            .map(|i| format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"))
            .collect();
        fs::write(&nt, data).unwrap();
        Database::build_disk(tmp.path("t.db"), &[&nt]).unwrap();
        let out = run_child("commit_point", &tmp.0, mode);
        child_ok(&out, &format!("commit_point ({mode})"));
        let redo = fs::read_to_string(tmp.path("redo.log")).unwrap();
        let update = fs::read_to_string(tmp.path("update.log")).unwrap();
        assert!(
            redo.contains("ex/before") && update.contains("ex/before"),
            "{mode}"
        );
        assert!(
            !redo.contains("update value") && !update.contains("update value"),
            "{mode}: the failed update reached a log:\n{redo}\n{update}"
        );
    }
}

// ---- committed records only ----------------------------------------------------

/// db#2: a committed write whose append failed (the log path is a
/// directory) is kept: a later rollback or disable does not drop it.
#[test]
fn rollback_and_disable_keep_committed_records_whose_append_failed() {
    let tmp = TmpDir::new("db2");
    let log = tmp.path("redo.log");

    // Rollback.
    fs::create_dir(&log).unwrap();
    let mut db = base();
    db.enable_redo_log(&log);
    assert!(db.insert_triple(&t("k1")), "committed (its append fails)");
    fs::remove_dir(&log).unwrap();
    db.begin().unwrap();
    assert!(db.insert_triple(&t("k3")));
    db.rollback().unwrap();
    db.query(&ins(&["k2"])).unwrap();
    let (_, mut r) = replayed(&log);
    assert!(
        has(&mut r, "k1"),
        "the committed k1 was dropped by the rollback"
    );
    assert!(has(&mut r, "k2") && !has(&mut r, "k3"));
    drop(db);

    // Disable: the owed commit is written then.
    fs::remove_file(&log).unwrap();
    fs::create_dir(&log).unwrap();
    let mut db = base();
    db.enable_redo_log(&log);
    assert!(db.insert_triple(&t("k4")));
    fs::remove_dir(&log).unwrap();
    db.disable_redo_log();
    let (_, mut r) = replayed(&log);
    assert!(
        has(&mut r, "k4"),
        "disable_redo_log dropped a committed record"
    );

    // Disable while the log is still unwritable, then enable it again.
    fs::remove_file(&log).unwrap();
    fs::create_dir(&log).unwrap();
    db.enable_redo_log(&log);
    assert!(db.insert_triple(&t("k5")));
    db.disable_redo_log();
    fs::remove_dir(&log).unwrap();
    db.enable_redo_log(&log);
    db.query(&ins(&["k6"])).unwrap();
    let (n, mut r) = replayed(&log);
    assert_eq!(n, 2);
    assert!(has(&mut r, "k5") && has(&mut r, "k6"));
}

/// robust#10: no increment is taken while committed records are missing
/// from the redo log (an unreadable log also used to pass for an empty
/// one); once the log takes them, the increment has them.
#[test]
fn no_increment_is_taken_while_committed_records_are_missing() {
    let tmp = TmpDir::new("robust10");
    let log = tmp.path("redo.log");
    let mut db = base();
    db.enable_redo_log(&log);
    db.backup(tmp.path("full")).unwrap();
    fs::create_dir(&log).unwrap();
    assert!(db.insert_triple(&t("k1")), "committed (its append fails)");
    let err = db
        .backup_incremental(tmp.path("inc"))
        .expect_err("an increment without k1");
    assert!(err.to_string().contains("redo.log"), "{err}");
    fs::remove_dir(&log).unwrap();
    db.backup_incremental(tmp.path("inc")).unwrap();
    let mut r = Database::restore_incremental(tmp.path("full"), &[tmp.path("inc")]).unwrap();
    assert!(has(&mut r, "k1"));
}

/// Statements of a rolled-back transaction never reach the update log;
/// those of a committed one do, at the commit.
#[test]
fn only_committed_statements_reach_the_update_log() {
    let tmp = TmpDir::new("update_txn");
    let log = tmp.path("update.log");
    let mut db = base();
    db.enable_update_log(&log);
    db.begin().unwrap();
    db.query(&ins(&["rolled_back"])).unwrap();
    db.rollback().unwrap();
    db.begin().unwrap();
    db.query(&ins(&["committed"])).unwrap();
    assert_eq!(len(&log), 0, "logged before the commit");
    db.commit().unwrap();
    db.query(&ins(&["auto"])).unwrap();
    let mut r = base();
    assert_eq!(r.replay_update_log(&log).unwrap(), 2);
    assert!(has(&mut r, "committed") && has(&mut r, "auto"));
    assert!(!has(&mut r, "rolled_back"));
}

/// db#3: triples inferred by `materialize_rdfs` and `run_rules` are written
/// to the redo log (a restore used to lack them); inferred inside a
/// rolled-back transaction they are not.
#[test]
fn inferred_triples_are_in_the_redo_log() {
    const DATA: &str = "\
<http://ex/a> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/C> .
<http://ex/C> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <http://ex/D> .
<http://ex/a> <http://ex/knows> <http://ex/b> .
";
    let tmp = TmpDir::new("db3");
    let log = tmp.path("redo.log");
    let mut db = Database::build_from_str("rdfs", DATA).unwrap();
    db.enable_redo_log(&log);
    assert!(db.materialize_rdfs() > 0);
    db.add_rule("sym", "?x <http://ex/knows> ?y => ?y <http://ex/knows> ?x")
        .unwrap();
    assert!(db.run_rules() > 0);
    db.begin().unwrap();
    db.insert_triple(&Triple::new(
        Term::iri("http://ex/e"),
        Term::iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
        Term::iri("http://ex/C"),
    ));
    db.materialize_rdfs();
    db.rollback().unwrap();
    let mut r = Database::build_from_str("rdfs", DATA).unwrap();
    r.replay_redo_log(&log).unwrap();
    assert_eq!(r.triple_num(), db.triple_num());
    assert!(ask(
        &mut r,
        "ASK { <http://ex/a> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/D> }"
    ));
    assert!(ask(
        &mut r,
        "ASK { <http://ex/b> <http://ex/knows> <http://ex/a> }"
    ));
    assert!(!ask(&mut r, "ASK { <http://ex/e> ?p ?o }"));
}

// ---- malformed logs --------------------------------------------------------------

/// db#9: lengths that overflow are errors, never panics (replay used to
/// panic with "attempt to add with overflow", in restore_incremental too).
#[test]
fn crafted_log_lengths_are_errors_not_panics() {
    let tmp = TmpDir::new("db9");
    let redo = tmp.path("redo.log");
    for text in [
        format!("REDO I {} 1\nx\n", usize::MAX - 20),
        format!("REDO I 0 {}\nx\n", usize::MAX),
        format!("REDO I {} 0\nx\n", usize::MAX),
    ] {
        fs::write(&redo, &text).unwrap();
        let r = catch_unwind(AssertUnwindSafe(|| base().replay_redo_log(&redo)));
        assert!(matches!(r, Ok(Err(_))), "{text:?}: {r:?}");
    }
    let update = tmp.path("update.log");
    fs::write(&update, format!("REC 0 0 {}\nx\n", usize::MAX)).unwrap();
    let r = catch_unwind(AssertUnwindSafe(|| base().replay_update_log(&update)));
    assert!(matches!(r, Ok(Err(_))), "{r:?}");

    // A real increment whose log was crafted.
    let mut db = base();
    db.enable_redo_log_in(tmp.path("live")).unwrap();
    db.backup(tmp.path("full")).unwrap();
    db.query(&ins(&["x"])).unwrap();
    db.backup_incremental(tmp.path("inc")).unwrap();
    let crafted = format!("REDO I {} 1\nx\n", usize::MAX - 20);
    fs::write(tmp.path("inc/incr.log"), &crafted).unwrap();
    let marker = fs::read_to_string(tmp.path("inc/INCREMENTAL")).unwrap();
    let base_offset: u64 = marker
        .lines()
        .find_map(|l| l.strip_prefix("base_offset "))
        .unwrap()
        .parse()
        .unwrap();
    fs::write(
        tmp.path("inc/INCREMENTAL"),
        format!(
            "GSTORE-INCREMENTAL 1\nbase_offset {base_offset}\nend_offset {}\n",
            base_offset + crafted.len() as u64
        ),
    )
    .unwrap();
    let r = catch_unwind(|| {
        Database::restore_incremental(tmp.path("full"), &[tmp.path("inc")]).map(drop)
    });
    assert!(matches!(r, Ok(Err(_))), "{r:?}");
}

/// gap-doc-claims#1: an unknown op, or an insert/delete without a triple,
/// is refused (both used to replay as no-ops, so a record of a future op
/// was skipped silently).
#[test]
fn unknown_ops_and_empty_bodies_are_refused() {
    let tmp = TmpDir::new("ops");
    let log = tmp.path("redo.log");
    let good = legacy_redo('I', None, "<http://ex/x> <http://ex/p> <http://ex/o> .");
    for (bad, what) in [
        ("REDO X 0 0\n\n".to_string(), "unknown"),
        ("REDO X 0 3\nabc\n".to_string(), "unknown"),
        ("REDO I 0 0\n\n".to_string(), "triple"),
        ("REDO D 0 0\n\n".to_string(), "triple"),
        ("REDO C 0 5\nhello\n".to_string(), "clear"),
        ("REDO N 3 0 G\nabc\n".to_string(), "clear"),
    ] {
        fs::write(&log, format!("{good}{bad}")).unwrap();
        let err = base().replay_redo_log(&log).unwrap_err().to_string();
        assert!(err.contains(what), "{bad:?}: {err}");
    }
}

/// The redo-log reader of the versions before CLEAR records existed
/// (`eaf5320`, `6362b97^`), line for line: every record's body is parsed as
/// triples and every triple must be an insert or a delete — a record with
/// an empty body is applied as nothing.
fn pre_clear_reader(bytes: &[u8]) -> Result<usize, String> {
    let mut pos = 0usize;
    let mut applied = 0usize;
    while pos < bytes.len() {
        let nl = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .ok_or("truncated redo.log header")?;
        let header = std::str::from_utf8(&bytes[pos..nl]).map_err(|e| e.to_string())?;
        let mut it = header.split_whitespace();
        if it.next() != Some("REDO") {
            return Err(format!("malformed redo.log record header: {header:?}"));
        }
        let op = it
            .next()
            .and_then(|s| s.chars().next())
            .ok_or("missing op")?;
        let glen: usize = it.next().and_then(|s| s.parse().ok()).ok_or("glen")?;
        let tlen: usize = it.next().and_then(|s| s.parse().ok()).ok_or("tlen")?;
        let gstart = nl + 1;
        let gend = gstart + glen;
        let tend = gend + tlen;
        if tend > bytes.len() {
            return Err("truncated redo.log body".into());
        }
        let triple_str = std::str::from_utf8(&bytes[gend..tend]).map_err(|e| e.to_string())?;
        for _t in gstore::parser::turtle::parse_str(triple_str).map_err(|e| e.to_string())? {
            match op {
                'I' | 'D' => {}
                other => return Err(format!("unknown redo.log op '{other}'")),
            }
        }
        applied += 1;
        pos = (tend + 1).min(bytes.len());
    }
    Ok(applied)
}

/// gap-doc-claims#1 (downgrade): the CLEAR / DROP / MOVE / COPY records
/// this version writes are refused by readers older than those records
/// ("unknown redo.log op 'C'"), as docs/STORAGE_BACKEND.md says — the
/// empty-bodied ones written until now were skipped by them silently, so
/// the cleared data came back. Those old-form records still replay here.
#[test]
fn clear_records_are_refused_by_readers_that_predate_them() {
    let tmp = TmpDir::new("downgrade");
    let log = tmp.path("redo.log");
    let mut db = base();
    db.enable_redo_log(&log);
    db.query(
        "INSERT DATA { GRAPH <http://ex/g> { <http://ex/s> <http://ex/p> <http://ex/o> } \
         GRAPH <http://ex/h> { <http://ex/s> <http://ex/p> <http://ex/o> } }",
    )
    .unwrap();
    // The inserts, checksums and commit marks included, read as before.
    assert_eq!(pre_clear_reader(&fs::read(&log).unwrap()), Ok(2));
    for clear in ["CLEAR GRAPH <http://ex/g>", "CLEAR NAMED", "CLEAR DEFAULT"] {
        let before = fs::read(&log).unwrap();
        db.query(clear).unwrap();
        let record = &fs::read(&log).unwrap()[before.len()..];
        let err = pre_clear_reader(record).expect_err(clear);
        assert!(err.contains("unknown redo.log op"), "{clear}: {err}");
    }
    let (_, mut r) = replayed(&log);
    assert_eq!(r.triple_num(), 0);
    assert!(!ask(&mut r, "ASK { GRAPH ?g { ?s ?p ?o } }"));

    // A clear as written until now: old readers skip it (the documented
    // hole), this one replays it.
    let legacy = legacy_redo('C', None, "");
    assert_eq!(pre_clear_reader(legacy.as_bytes()), Ok(1));
    fs::write(&log, &legacy).unwrap();
    let (n, r) = replayed(&log);
    assert_eq!((n, r.triple_num()), (1, 0));
}

/// Logs written before checksums and commit marks replay as before.
#[test]
fn old_format_logs_still_replay() {
    let tmp = TmpDir::new("legacy");
    let log = tmp.path("redo.log");
    let x = "<http://ex/x> <http://ex/p> <http://ex/o> .";
    let y = "<http://ex/y> <http://ex/p> <http://ex/o> .";
    let text = [
        legacy_redo('I', None, x),
        legacy_redo('I', Some("http://ex/g"), y),
        legacy_redo('I', Some("http://ex/h"), y),
        legacy_redo('D', None, "<http://ex/a> <http://ex/p> <http://ex/b> ."),
        legacy_redo('C', Some("http://ex/g"), ""),
    ]
    .concat();
    fs::write(&log, text).unwrap();
    let (n, mut r) = replayed(&log);
    assert_eq!(n, 5);
    assert!(has(&mut r, "x"));
    assert!(!ask(&mut r, "ASK { <http://ex/a> ?p ?o }"));
    assert!(!ask(&mut r, "ASK { GRAPH <http://ex/g> { ?s ?p ?o } }"));
    assert!(ask(
        &mut r,
        "ASK { GRAPH <http://ex/h> { <http://ex/y> ?p ?o } }"
    ));

    let update = tmp.path("update.log");
    let q = ins(&["u"]);
    fs::write(&update, format!("REC 1 1 {}\n{q}\n", q.len())).unwrap();
    let mut r = base();
    assert_eq!(r.replay_update_log(&update).unwrap(), 1);
    assert!(has(&mut r, "u"));
}

// ---- backups ----------------------------------------------------------------------

/// Every file under `dir` with its content and modification time.
fn files(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, (Vec<u8>, SystemTime)>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let mtime = fs::metadata(&p).unwrap().modified().unwrap();
                out.insert(
                    p.strip_prefix(root).unwrap().to_path_buf(),
                    (fs::read(&p).unwrap(), mtime),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// Set the modification time of every file under `dir` to long ago, so
/// any later write shows (whatever the clock's granularity).
fn age(dir: &Path) {
    for (rel, _) in files(dir) {
        fs::OpenOptions::new()
            .write(true)
            .open(dir.join(rel))
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
    }
}

/// db#13: restoring from a base and its increments never writes the base
/// (it used to open it in place: a `LOCK` file, and on a disk base a
/// rewritten `kvstore.kv` and a new `vstree.kv`).
#[test]
fn restore_incremental_never_writes_the_base() {
    let tmp = TmpDir::new("db13");
    // A snapshot base.
    let mut db = base();
    db.enable_redo_log_in(tmp.path("live")).unwrap();
    db.backup(tmp.path("snap")).unwrap();
    db.query(&ins(&["x"])).unwrap();
    db.backup_incremental(tmp.path("snap_inc")).unwrap();
    age(&tmp.path("snap"));
    let before = files(&tmp.path("snap"));
    let mut r = Database::restore_incremental(tmp.path("snap"), &[tmp.path("snap_inc")]).unwrap();
    assert!(has(&mut r, "x"));
    drop(r);
    let after = files(&tmp.path("snap"));
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>()
    );
    assert!(after == before, "the snapshot base was written");

    // A disk base whose VS-tree is stale (not copied by the backup).
    let nt = tmp.path("base.nt");
    fs::write(&nt, BASE).unwrap();
    Database::build_disk(tmp.path("disk"), &[&nt]).unwrap();
    let mut db = Database::open(tmp.path("disk")).unwrap();
    db.insert_triple(&t("stale"));
    db.sync().unwrap();
    db.enable_redo_log_in(tmp.path("disk_live")).unwrap();
    db.backup(tmp.path("disk_base")).unwrap();
    db.query(&ins(&["y"])).unwrap();
    db.backup_incremental(tmp.path("disk_inc")).unwrap();
    drop(db);
    age(&tmp.path("disk_base"));
    let before = files(&tmp.path("disk_base"));
    let mut r =
        Database::restore_incremental(tmp.path("disk_base"), &[tmp.path("disk_inc")]).unwrap();
    assert!(has(&mut r, "y") && has(&mut r, "stale"));
    drop(r);
    let after = files(&tmp.path("disk_base"));
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>()
    );
    assert!(after == before, "the disk base was written");
}

/// db#14: a backup inside an open transaction is refused — it would capture
/// the transaction's uncommitted writes, which no later increment undoes
/// when it rolls back.
#[test]
fn a_backup_inside_a_transaction_is_refused() {
    let tmp = TmpDir::new("db14");
    let mut db = base();
    db.enable_redo_log_in(tmp.path("live")).unwrap();
    db.begin().unwrap();
    assert!(db.insert_triple(&t("uncommitted")));
    let err = db
        .backup(tmp.path("full"))
        .expect_err("a backup inside a transaction");
    assert!(err.to_string().contains("transaction"), "{err}");
    db.rollback().unwrap();
    db.backup(tmp.path("full")).unwrap();
    db.insert_triple(&t("later"));
    db.backup_incremental(tmp.path("inc")).unwrap();
    let mut r = Database::restore_incremental(tmp.path("full"), &[tmp.path("inc")]).unwrap();
    assert!(has(&mut r, "later") && !has(&mut r, "uncommitted"));
}
