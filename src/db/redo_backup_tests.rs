//! Backups and the locks they take (review conc#13), with access to the
//! database's private `persist_lock`.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::Database;
use crate::reason::rules::RULE_DIR;

/// A temporary directory, removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        let d =
            std::env::temp_dir().join(format!("gstore_redo_backup_{tag}_{}", std::process::id()));
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

/// conc#13: contract C8 orders a database's rule-directory lock before its
/// `persist_lock`; `backup` and `backup_incremental` took them the other
/// way round (a concurrent `reasonManage` disable, which takes the rule
/// lock and then saves, could deadlock against them). Now each copies the
/// rules before it takes `persist_lock`: with `persist_lock` held
/// elsewhere, the backup copies the rules and only then waits.
#[test]
fn backups_copy_the_rules_before_they_take_persist_lock() {
    let tmp = TmpDir::new("c8");
    let home = tmp.0.join("home");
    fs::create_dir_all(home.join(RULE_DIR)).unwrap();
    fs::write(home.join(RULE_DIR).join("r.json"), "{}").unwrap();
    let mut db =
        Database::build_from_str("c8", "<http://ex/a> <http://ex/p> <http://ex/b> .\n").unwrap();
    db.set_reason_db_dir(&home);
    db.enable_redo_log_in(tmp.0.join("live")).unwrap();
    let full = tmp.0.join("full");
    let inc = tmp.0.join("inc");
    let copied = |dir: &PathBuf| dir.join(RULE_DIR).join("r.json").is_file();
    for (dir, incremental) in [(&full, false), (&inc, true)] {
        let persist = db.persist_guard();
        let db = &db;
        std::thread::scope(|s| {
            let backup = s.spawn(move || {
                if incremental {
                    db.backup_incremental(dir)
                } else {
                    db.backup(dir)
                }
            });
            // A hang guard only: the rules appear while persist_lock is
            // still held here (before the fix the backup waited for it
            // first, so they never did, and the guard fails the test —
            // dropping `persist` on the way out lets the backup finish).
            let deadline = Instant::now() + Duration::from_secs(60);
            while !copied(dir) {
                assert!(
                    Instant::now() < deadline,
                    "the backup took persist_lock before the rule-directory lock (C8)"
                );
                std::thread::yield_now();
            }
            assert!(
                !backup.is_finished(),
                "the backup did not wait for persist_lock"
            );
            drop(persist);
            backup.join().unwrap().unwrap();
        });
    }
    assert!(copied(&full) && copied(&inc));
}
