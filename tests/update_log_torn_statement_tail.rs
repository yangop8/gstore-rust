//! An update log whose last record was damaged by a crash stays replayable,
//! also when that record's statement holds lines that look like record
//! headers (round-3 review of durability#6).
//!
//! The statement text of an update-log record is stored as it is, so a
//! long literal may hold a line that starts with `REC `. When the last
//! record was damaged in place — its last bytes zero-filled because the
//! file grew but its last block never reached the disk, or a byte changed —
//! that text was taken for a record after the damaged one: replay reported
//! "update.log is corrupt", and the next session's append cut the file at
//! the fake header *inside* the damaged record and wrote behind the
//! leftover bytes, so replay refused the log for good and every later
//! commit was unreachable. The damaged record is now the torn tail unless a
//! whole record (complete, checksum right) follows it, and the appender
//! cuts at its start.

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
            "gstore_update_log_torn_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create a temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const BASE: &str = "<http://ex/base> <http://ex/p> <http://ex/o> .\n";

fn update(db: &mut Database, u: &str) {
    match db.query(u) {
        Ok(QueryResult::Update { .. }) => {}
        other => panic!("{u}: {other:?}"),
    }
}

fn has(db: &mut Database, s: &str) -> bool {
    match db.query(&format!("ASK {{ <http://ex/{s}> ?p ?o }}")) {
        Ok(QueryResult::Ask(b)) => b,
        other => panic!("ASK {s}: {other:?}"),
    }
}

/// Log `x1`, then a statement whose literal holds the line `REC 1 2 3`,
/// then damage the log's last bytes with `damage`.
fn torn_log(dir: &Path, damage: impl Fn(&mut Vec<u8>)) -> PathBuf {
    let log = dir.join("update.log");
    let mut db = Database::build_from_str("torn", BASE).unwrap();
    db.enable_update_log(&log);
    update(&mut db, "INSERT DATA { <http://ex/x1> <http://ex/p> 1 }");
    update(
        &mut db,
        &format!(
            "INSERT DATA {{ <http://ex/x2> <http://ex/p> \"\"\"x\nREC 1 2 3\ny\n{}\"\"\" }}",
            "z".repeat(200)
        ),
    );
    drop(db);
    let mut bytes = std::fs::read(&log).unwrap();
    damage(&mut bytes);
    std::fs::write(&log, &bytes).unwrap();
    log
}

fn check(tag: &str, damage: impl Fn(&mut Vec<u8>)) {
    let dir = TempDir::new(tag);
    let log = torn_log(dir.path(), damage);

    // Replay skips the torn statement and applies the commit before it.
    let mut replica = Database::build_from_str("replica", BASE).unwrap();
    let applied = replica
        .replay_update_log(&log)
        .unwrap_or_else(|e| panic!("{tag}: replay refused the torn tail: {e}"));
    assert_eq!(applied, 1, "{tag}");
    assert!(has(&mut replica, "x1") && !has(&mut replica, "x2"), "{tag}");

    // The next session's append cuts the torn record off at its start, so
    // its commit is replayable after the first one.
    let mut db = Database::build_from_str("next", BASE).unwrap();
    db.enable_update_log(&log);
    update(&mut db, "INSERT DATA { <http://ex/after> <http://ex/p> 3 }");
    drop(db);
    let mut replica = Database::build_from_str("replica", BASE).unwrap();
    let applied = replica
        .replay_update_log(&log)
        .unwrap_or_else(|e| panic!("{tag}: the log is unreplayable after the append: {e}"));
    assert_eq!(applied, 2, "{tag}");
    assert!(
        has(&mut replica, "x1") && has(&mut replica, "after"),
        "{tag}"
    );
    let text = String::from_utf8_lossy(&std::fs::read(&log).unwrap()).into_owned();
    assert!(
        !text.contains("REC 1 2 3"),
        "{tag}: the append left the torn record's bytes in the log:\n{text}"
    );
}

/// The last 40 bytes zero-filled: a crash after the file grew, before its
/// last block reached the disk.
#[test]
fn a_zero_filled_last_statement_holding_a_record_line_is_a_torn_tail() {
    check("zeroed", |b| {
        let n = b.len();
        b[n - 40..].fill(0);
    });
}

/// One byte of the last statement changed: its checksum fails.
#[test]
fn a_checksum_damaged_last_statement_holding_a_record_line_is_a_torn_tail() {
    check("flipped", |b| {
        let n = b.len();
        b[n - 10] ^= 0x01;
    });
}
