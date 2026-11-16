//! A transaction's commit is binary (review gap-txn-lifecycle#3, M2, and
//! gap-rocksdb-exec#5): when its commit point fails, it is rolled back —
//! none of its writes stays visible, none becomes durable later — unless it
//! can no longer be undone, which [`Database::commit_checked`] reports as
//! [`CommitFailure::NotDurable`] (its writes stay applied).
//!
//! The failures are injected with the crate's test-only failpoints: the
//! pager's (per thread: armed on the thread that commits) for both disk
//! modes — a write-ahead log that cannot be staged (nothing reached the
//! file: rolled back) or a batch that cannot be applied once its log has
//! its name (the next open replays it: not durable) — and RocksDB's
//! journal-deletion failpoint. Before the fix every one of these commits
//! left the transaction applied and visible (Materialize: until a later
//! save made it durable or a restart lost it; RocksDB: until the next write
//! committed it or a reopen undid it), and its redo records were logged.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::db::CommitFailure;
use crate::kvstore::pager::failpoints;
use crate::{ConcurrentDb, Database, DiskQueryMode, OpenOptions, QueryResult, Term, Triple};

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_txn_commit_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> <http://ex/c> .\n";

/// An INSERT DATA of `n` new triples about `tag`.
fn insert(tag: &str, n: usize) -> String {
    let mut s = String::from("INSERT DATA {\n");
    for i in 0..n {
        s.push_str(&format!(
            "<http://ex/{tag}/{i}> <http://ex/value> \"{tag} literal number {i}\" .\n"
        ));
    }
    s.push('}');
    s
}

fn ask(db: &Database, tag: &str) -> bool {
    let q = format!("ASK {{ <http://ex/{tag}/0> <http://ex/value> ?v }}");
    db.query_read(&q, None, None).unwrap() == QueryResult::Ask(true)
}

fn count(db: &Database) -> u64 {
    db.triple_num()
}

fn disk_db(dir: &Path) -> PathBuf {
    let src = dir.join("base.nt");
    fs::write(&src, BASE).unwrap();
    let db = dir.join("t.db");
    Database::build_disk(&db, &[&src]).unwrap();
    db
}

fn open(dir: &Path, mode: DiskQueryMode) -> Database {
    Database::open_with(dir, &OpenOptions::new().with_disk_query_mode(mode)).unwrap()
}

/// A write-ahead log that cannot be staged: nothing of the transaction
/// reached the file, so it is rolled back — invisible now and after a
/// reopen — and the store is broken until the reopen.
#[test]
fn a_commit_whose_log_cannot_be_written_is_rolled_back() {
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let tmp = TmpDir::new("wal");
        let dir = disk_db(&tmp.0);
        let mut db = open(&dir, mode);
        let before = count(&db);
        db.begin().unwrap();
        db.query(&insert("t", 50)).unwrap();
        let outcome = {
            let _fp = failpoints::fail_wal_write_after(100);
            db.commit_checked()
        };
        assert!(
            matches!(outcome, Err(CommitFailure::RolledBack(_))),
            "{mode:?}: {outcome:?}"
        );
        assert!(!db.in_transaction(), "{mode:?}");
        if mode == DiskQueryMode::Materialize {
            // Readers see the state before the transaction (Stream mode
            // refuses every read once its store is broken).
            assert!(!ask(&db, "t"), "{mode:?}: the failed commit is visible");
            assert_eq!(count(&db), before, "{mode:?}");
        }
        assert!(db.storage_failure().is_some(), "{mode:?}");
        drop(db);
        let db = open(&dir, mode);
        assert!(!ask(&db, "t"), "{mode:?}: the failed commit became durable");
        assert_eq!(count(&db), before, "{mode:?}");
    }
}

/// A batch that cannot be applied once its log has its name: the next open
/// replays the log, so the transaction can no longer be undone — it stays
/// applied, reported as not durable, and the reopen keeps it.
#[test]
fn a_commit_that_fails_after_its_log_is_complete_is_not_durable() {
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let tmp = TmpDir::new("apply");
        let dir = disk_db(&tmp.0);
        let mut db = open(&dir, mode);
        db.begin().unwrap();
        db.query(&insert("t", 50)).unwrap();
        let outcome = {
            let _fp = failpoints::fail_apply_after(1);
            db.commit_checked()
        };
        assert!(
            matches!(outcome, Err(CommitFailure::NotDurable(_))),
            "{mode:?}: {outcome:?}"
        );
        if mode == DiskQueryMode::Materialize {
            assert!(ask(&db, "t"), "{mode:?}: applied, as reported");
        }
        assert!(db.storage_failure().is_some(), "{mode:?}");
        drop(db);
        let db = open(&dir, mode);
        assert!(ask(&db, "t"), "{mode:?}: the replayed log holds it");
    }
}

/// A successful commit after a refused one works as before, and the plain
/// [`Database::commit`] reports a rolled-back commit as an error.
#[test]
fn commit_reports_the_rolled_back_commit_as_an_error() {
    let tmp = TmpDir::new("plain");
    let dir = disk_db(&tmp.0);
    let mut db = open(&dir, DiskQueryMode::Materialize);
    db.begin().unwrap();
    db.query(&insert("t", 5)).unwrap();
    let res = {
        let _fp = failpoints::fail_wal_write_after(10);
        db.commit()
    };
    assert!(res.is_err());
    assert!(!ask(&db, "t"));
    // A healthy database commits as before.
    let mut db = Database::build_from_str("m", BASE).unwrap();
    db.begin().unwrap();
    db.query(&insert("u", 3)).unwrap();
    db.commit_checked().unwrap();
    assert!(ask(&db, "u"));
    assert!(matches!(
        db.commit_checked(),
        Err(CommitFailure::NotCommitted(_))
    ));
}

/// A [`ConcurrentDb`] transaction whose commit point fails is rolled back
/// by the database it applies to, so it fails as a whole and publishes
/// nothing. Review (r2-txn): the binary commit reached `ConcurrentDb`, which
/// still treated such a failure as "applied but not durable" — it published
/// a new version and recorded the writes in its version chains, so a
/// version-pinned reader saw writes the database had rolled back.
#[test]
fn a_concurrent_commit_rolled_back_at_its_commit_point_publishes_nothing() {
    let tmp = TmpDir::new("concurrent");
    let dir = disk_db(&tmp.0);
    let cdb = ConcurrentDb::new(open(&dir, DiskQueryMode::Materialize));
    let v0 = cdb.version();
    let t = Triple::new(
        Term::iri("http://ex/t/0"),
        Term::iri("http://ex/value"),
        Term::plain_literal("t"),
    );
    let mut txn = cdb.begin();
    txn.insert(t.clone());
    let res = {
        let _fp = failpoints::fail_wal_write_after(10);
        txn.commit()
    };
    assert!(res.is_err(), "{res:?}");
    assert!(
        !cdb.version_view().contains(&t),
        "a version-pinned reader sees the rolled-back write"
    );
    assert!(!cdb.snapshot().contains(&t));
    assert_eq!(
        cdb.version(),
        v0,
        "a rolled-back commit published a version"
    );
}

/// RocksDB (review gap-rocksdb-exec#5): a commit whose journal cannot be
/// deleted is rolled back through the journal — not left for the next
/// write to commit or for the next open to undo — and its redo records are
/// never logged.
#[cfg(feature = "rocksdb")]
#[test]
fn a_rocksdb_commit_whose_journal_cannot_be_deleted_is_rolled_back() {
    let tmp = TmpDir::new("rocks");
    let dir = tmp.0.join("r.db");
    drop(Database::build_rocksdb_from_str("r", &dir, BASE).unwrap());
    let mut db = Database::open(&dir).unwrap();
    db.enable_redo_log_in(&tmp.0).unwrap();
    let redo = || {
        fs::read_dir(&tmp.0)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("redo"))
            .map(|e| fs::read_to_string(e.path()).unwrap_or_default())
            .collect::<String>()
    };
    let before = count(&db);
    db.begin().unwrap();
    db.query(&insert("t", 5)).unwrap();
    db.backend()
        .as_rocks()
        .unwrap()
        .inject_failures()
        .journal_clear
        .store(true, Ordering::SeqCst);
    let outcome = db.commit_checked();
    db.backend()
        .as_rocks()
        .unwrap()
        .inject_failures()
        .journal_clear
        .store(false, Ordering::SeqCst);
    assert!(
        matches!(outcome, Err(CommitFailure::RolledBack(_))),
        "{outcome:?}"
    );
    assert!(!db.in_transaction());
    assert!(!ask(&db, "t"), "the failed commit is visible");
    assert_eq!(count(&db), before);
    assert!(
        !redo().contains("http://ex/t/0"),
        "a rolled-back transaction reached the redo log: {}",
        redo()
    );
    // The replayed journal goes with the next write; nothing commits it.
    db.query(&insert("w", 1)).unwrap();
    assert!(!ask(&db, "t"));
    assert!(ask(&db, "w"));
    drop(db);
    let db = Database::open(&dir).unwrap();
    assert!(!ask(&db, "t"), "the failed commit became durable");
    assert!(ask(&db, "w"));
}

/// RocksDB: a commit whose journal cannot be deleted nor replayed yet
/// stays applied, reported as not durable, and the next open undoes it.
#[cfg(feature = "rocksdb")]
#[test]
fn a_rocksdb_commit_that_cannot_be_undone_yet_is_not_durable_and_undone_at_open() {
    let tmp = TmpDir::new("rocks2");
    let dir = tmp.0.join("r.db");
    drop(Database::build_rocksdb_from_str("r", &dir, BASE).unwrap());
    let mut db = Database::open(&dir).unwrap();
    db.begin().unwrap();
    db.query(&insert("t", 5)).unwrap();
    // Its newest write is a named-graph quad: undoing it is a `gspo` write,
    // which the side-write failpoint fails, so the replay stops there.
    db.query("INSERT DATA { GRAPH <http://ex/g> { <http://ex/q> <http://ex/p> <http://ex/r> } }")
        .unwrap();
    let fp = db.backend().as_rocks().unwrap().inject_failures();
    fp.journal_clear.store(true, Ordering::SeqCst);
    fp.side_writes.store(true, Ordering::SeqCst);
    let outcome = db.commit_checked();
    // Review (r2-txn): this test used to accept a rolled-back outcome as
    // well, and with default-graph writes only the replay always succeeded,
    // so the not-durable path it is named after never ran.
    assert!(
        matches!(outcome, Err(CommitFailure::NotDurable(_))),
        "{outcome:?}"
    );
    assert!(!db.in_transaction());
    assert!(ask(&db, "t"), "left applied, as reported");
    let fp = db.backend().as_rocks().unwrap().inject_failures();
    fp.journal_clear.store(false, Ordering::SeqCst);
    fp.side_writes.store(false, Ordering::SeqCst);
    drop(db);
    let db = Database::open(&dir).unwrap();
    assert!(!ask(&db, "t"), "the failed commit became durable");
    let q = "ASK { GRAPH <http://ex/g> { <http://ex/q> <http://ex/p> <http://ex/r> } }";
    assert_eq!(
        db.query_read(q, None, None).unwrap(),
        QueryResult::Ask(false),
        "the failed commit's quad became durable"
    );
}
