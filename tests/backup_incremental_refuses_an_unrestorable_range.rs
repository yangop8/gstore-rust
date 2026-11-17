//! `backup_incremental` refuses to write an increment that
//! `restore_incremental` could never apply (round-3 review).
//!
//! The appender's scan of the live redo log steps over a corrupt record
//! (corruption earlier in a log is replay's to report, the tail is the
//! appender's to repair), and the increment copied the log's bytes since
//! the last backup without reading them: a record damaged in the live log
//! went into the increment, `backup_incremental` answered `Ok`, and the
//! restore failed with "redo.log is corrupt … checksum mismatch" — for that
//! increment and every one after it. The copied range is now walked as the
//! restore walks it, and a damaged one is refused with the corruption's
//! offset; a full backup starts a new chain past the damage.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use gstore::{Database, QueryResult};

/// A directory under the system temp dir, removed when dropped — also when
/// the test fails.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore_incr_unrestorable_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create a temp dir");
        TempDir(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn insert(db: &mut Database, tag: &str) {
    match db.query(&format!(
        "INSERT DATA {{ <http://ex/{tag}> <http://ex/p> <http://ex/o> }}"
    )) {
        Ok(QueryResult::Update { .. }) => {}
        other => panic!("insert {tag}: {other:?}"),
    }
}

fn has(db: &mut Database, tag: &str) -> bool {
    match db.query(&format!("ASK {{ <http://ex/{tag}> ?p ?o }}")) {
        Ok(QueryResult::Ask(b)) => b,
        other => panic!("ASK {tag}: {other:?}"),
    }
}

/// Flip one byte of the record that logged `<http://ex/{tag}>`.
fn damage_record_of(log: &Path, tag: &str) {
    let mut bytes = fs::read(log).unwrap();
    let needle = format!("ex/{tag}>");
    let at = bytes
        .windows(needle.len())
        .position(|w| w == needle.as_bytes())
        .unwrap_or_else(|| panic!("{tag} is not in the redo log"));
    bytes[at + 3] ^= 0x01;
    fs::write(log, bytes).unwrap();
}

#[test]
fn an_increment_holding_a_damaged_commit_is_refused() {
    let tmp = TempDir::new("refused");
    let log = tmp.path("redo.log");
    let mut db =
        Database::build_from_str("incr", "<http://ex/base> <http://ex/p> <http://ex/o> .\n")
            .unwrap();
    db.enable_redo_log(&log);
    db.backup(tmp.path("full")).unwrap();
    for tag in ["x1", "x2", "x3"] {
        insert(&mut db, tag);
    }
    // A damaged middle commit, then one more commit (the appender's cached
    // boundary is the file's length, so it appends without a scan).
    damage_record_of(&log, "x2");
    insert(&mut db, "x4");

    let err = db
        .backup_incremental(tmp.path("inc"))
        .expect_err("an increment that cannot be restored was written");
    let msg = err.to_string();
    assert!(
        msg.contains("cannot be restored") && msg.contains("checksum mismatch"),
        "{msg}"
    );
    assert!(
        !tmp.path("inc").join("incr.log").exists(),
        "no increment is written"
    );

    // A full backup starts a new chain past the damage; its increments
    // restore.
    db.backup(tmp.path("full2")).unwrap();
    insert(&mut db, "x5");
    db.backup_incremental(tmp.path("inc2")).unwrap();
    let mut restored =
        Database::restore_incremental(tmp.path("full2"), &[tmp.path("inc2")]).unwrap();
    for tag in ["x1", "x3", "x4", "x5"] {
        assert!(has(&mut restored, tag), "{tag}");
    }
}

/// The check is on what is copied, not on the whole log: an increment of
/// healthy commits after a full backup is taken as before.
#[test]
fn increments_of_healthy_commits_are_taken_and_restored() {
    let tmp = TempDir::new("healthy");
    let mut db =
        Database::build_from_str("incr", "<http://ex/base> <http://ex/p> <http://ex/o> .\n")
            .unwrap();
    db.enable_redo_log(tmp.path("redo.log"));
    db.backup(tmp.path("full")).unwrap();
    insert(&mut db, "y1");
    db.backup_incremental(tmp.path("inc1")).unwrap();
    insert(&mut db, "y2");
    db.backup_incremental(tmp.path("inc2")).unwrap();
    let mut restored =
        Database::restore_incremental(tmp.path("full"), &[tmp.path("inc1"), tmp.path("inc2")])
            .unwrap();
    assert!(has(&mut restored, "y1") && has(&mut restored, "y2"));
}
