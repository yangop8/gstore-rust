//! The redo and update logs follow what a reopen keeps, on RocksDB as on
//! the on-disk layout (round-3 review of durability#6 / db#8).
//!
//! An auto-commit UPDATE whose RocksDB commit point failed — its undo
//! journal could not be deleted, so the next open undoes the request unless
//! a later write deletes the journal first — was appended to both logs,
//! while a transaction in the same situation was not: the logs could hold a
//! statement the store no longer had. A commit point with an unknown
//! outcome now logs nothing, on every path, and marks the logs in doubt:
//! `backup_incremental` refuses until a full `backup` starts a new chain.
//! (The on-disk cases — replayed by the next open, or not applied at all —
//! are `tests/redo_log_durability.rs`'s
//! `the_logs_agree_with_the_reopened_database_after_a_failed_commit_point`.)

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use super::*;

/// A temp directory, removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tmp_dir(tag: &str) -> TmpDir {
    let d = std::env::temp_dir().join(format!("gstore_commit_fate_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    TmpDir(d)
}

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn a_rocksdb_update_whose_journal_stays_is_logged_nowhere_and_puts_the_logs_in_doubt() {
    let tmp = tmp_dir("rocks_update");
    let dir = tmp.0.join("r.db");
    drop(Database::build_rocksdb_from_str("r", &dir, BASE).unwrap());
    let mut db = Database::open(&dir).unwrap();
    let (redo, update) = (tmp.0.join("redo.log"), tmp.0.join("update.log"));
    db.enable_redo_log(&redo);
    db.enable_update_log(&update);
    db.query("INSERT DATA { <http://ex/before> <http://ex/p> 1 }")
        .unwrap();
    db.backup(tmp.0.join("full")).unwrap();

    let fp = &db.store.as_rocks().unwrap().inject_failures().journal_clear;
    fp.store(true, Ordering::SeqCst);
    let r = db.query("INSERT DATA { <http://ex/doubt> <http://ex/p> 2 }");
    let fp = &db.store.as_rocks().unwrap().inject_failures().journal_clear;
    fp.store(false, Ordering::SeqCst);
    assert!(r.is_err(), "the commit point must fail: {r:?}");
    assert!(
        !read(&redo).contains("ex/doubt") && !read(&update).contains("ex/doubt"),
        "an update whose outcome is unknown reached a log:\n{}\n{}",
        read(&redo),
        read(&update)
    );
    assert!(read(&redo).contains("ex/before") && read(&update).contains("ex/before"));

    // The next write deletes the stale journal first, which commits the
    // request after all: the logs and the store now differ, so no
    // increment is taken until a full backup starts a new chain.
    db.query("INSERT DATA { <http://ex/after> <http://ex/p> 3 }")
        .unwrap();
    let err = db
        .backup_incremental(tmp.0.join("inc1"))
        .expect_err("an increment was taken from logs in doubt");
    assert!(err.to_string().contains("full backup"), "{err}");
    db.backup(tmp.0.join("full2")).unwrap();
    db.query("INSERT DATA { <http://ex/later> <http://ex/p> 4 }")
        .unwrap();
    db.backup_incremental(tmp.0.join("inc2")).unwrap();
    let mut restored =
        Database::restore_incremental(tmp.0.join("full2"), &[tmp.0.join("inc2")]).unwrap();
    for tag in ["doubt", "after", "later"] {
        match restored
            .query(&format!("ASK {{ <http://ex/{tag}> ?p ?o }}"))
            .unwrap()
        {
            QueryResult::Ask(true) => {}
            other => panic!("{tag}: {other:?}"),
        }
    }
}

/// The same rule for a transaction: one whose journal could not be
/// deleted is rolled back (certainly not kept), so it is logged nowhere
/// and leaves the logs trusted.
#[test]
fn a_rocksdb_transaction_whose_journal_stays_is_rolled_back_and_the_logs_stay_trusted() {
    let tmp = tmp_dir("rocks_txn");
    let dir = tmp.0.join("r.db");
    drop(Database::build_rocksdb_from_str("r", &dir, BASE).unwrap());
    let mut db = Database::open(&dir).unwrap();
    let redo = tmp.0.join("redo.log");
    db.enable_redo_log(&redo);
    db.backup(tmp.0.join("full")).unwrap();
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/t> <http://ex/p> 1 }")
        .unwrap();
    let fp = &db.store.as_rocks().unwrap().inject_failures().journal_clear;
    fp.store(true, Ordering::SeqCst);
    let r = db.commit_checked();
    let fp = &db.store.as_rocks().unwrap().inject_failures().journal_clear;
    fp.store(false, Ordering::SeqCst);
    assert!(matches!(r, Err(CommitFailure::RolledBack(_))), "{r:?}");
    assert!(!read(&redo).contains("ex/t"));
    db.query("INSERT DATA { <http://ex/w> <http://ex/p> 2 }")
        .unwrap();
    db.backup_incremental(tmp.0.join("inc")).unwrap();
}
