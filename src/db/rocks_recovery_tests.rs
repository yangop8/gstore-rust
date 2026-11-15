//! RocksDB crash recovery at open is bounded and resumable at the
//! `Database` level (round-1 review of r1-rocksdb: every auto-commit import
//! is journaled since that round, and the open read the whole journal into
//! memory, collected its candidates in memory twice, and deleted the
//! journal only after the whole replay and term check — an open that ran
//! out of memory did so again at every open).
//!
//! A transaction left open when its `Database` is dropped leaves its undo
//! journal on disk, as a process that died would.

use std::path::PathBuf;

use super::*;
use crate::backend::rocks::{REPLAY_CHUNK_FOR_TEST, REPLAY_MAX_HELD, REPLAY_STOP_AFTER_CHUNKS};

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        let d = std::env::temp_dir().join(format!(
            "gstore_rocks_recovery_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Puts the replay knobs of this thread back, also when the test fails.
struct Knobs;

impl Drop for Knobs {
    fn drop(&mut self) {
        REPLAY_CHUNK_FOR_TEST.with(|c| c.set(0));
        REPLAY_STOP_AFTER_CHUNKS.with(|c| c.set(None));
    }
}

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> \"kept\" .\n";

#[test]
fn a_crashed_transaction_is_recovered_in_chunks_without_orphan_terms() {
    let tmp = TmpDir::new("txn");
    let dir = tmp.0.join("r.db");
    drop(Database::build_rocksdb_from_str("r", &dir, BASE).unwrap());
    {
        let mut db = Database::open(&dir).unwrap();
        db.begin().unwrap();
        for i in 0..300 {
            db.query(&format!(
                "INSERT DATA {{ <http://ex/new{i}> <http://ex/q{}> \"literal {i}\" }}",
                i % 7
            ))
            .unwrap();
        }
        // Dropped with the transaction open: its journal stays on disk.
    }
    let _knobs = Knobs;
    // The first recovery stops after two chunks of 64 records (the process
    // died again); the next open resumes it.
    REPLAY_CHUNK_FOR_TEST.with(|c| c.set(64));
    REPLAY_STOP_AFTER_CHUNKS.with(|c| c.set(Some(2)));
    assert!(Database::open(&dir).is_err(), "the injected stop");
    REPLAY_STOP_AFTER_CHUNKS.with(|c| c.set(None));
    REPLAY_MAX_HELD.with(|m| m.set(0));
    let mut db = Database::open(&dir).unwrap();
    let held = REPLAY_MAX_HELD.with(std::cell::Cell::get);
    assert!((1..=64).contains(&held), "held {held} records at once");
    assert_eq!(db.triple_num(), 2, "the transaction is undone");
    let rocks = db.store.as_rocks().unwrap();
    assert!(
        rocks.pending_reclaim(usize::MAX).unwrap().is_empty(),
        "drained at open"
    );
    assert!(rocks.journal_error().is_none());
    // Every term the transaction minted was freed: none is left as an
    // orphan (a reclamation pass finds nothing to free).
    assert_eq!(db.reclaim_unused().unwrap(), 0, "no orphan term");
    // The database serves writes again.
    db.query("INSERT DATA { <http://ex/after> <http://ex/p> <http://ex/b> }")
        .unwrap();
    assert_eq!(db.triple_num(), 3);
}
