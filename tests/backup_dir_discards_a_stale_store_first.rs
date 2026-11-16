//! `Database::backup_dir` removes every database file the source lacks
//! before it copies anything (review durability#4, as its documentation
//! says) — also a `rocksdb/` store left in the destination by an earlier
//! (RocksDB-era) backup. Before the round-2 review fix that store was
//! discarded only after the copy, so a copy failing part-way left it in
//! place, and layout detection, which prefers `rocksdb/`, still saw the old
//! RocksDB database there.
//!
//! The copy failure is real: a directory stands where the copy must write
//! a file (as a full disk would stop it part-way).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::db::StorageKind;
use gstore::Database;

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_backup_stale_store_{tag}_{}_{n}",
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

#[test]
fn a_failed_backup_leaves_no_stale_rocksdb_store_in_the_destination() {
    let tmp = TmpDir::new("failed");
    let src = tmp.0.join("src.db");
    let dst = tmp.0.join("dst.db");
    let db = Database::build_from_str(
        "src",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .\n",
    )
    .unwrap();
    db.save(&src).unwrap();
    drop(db);
    assert_eq!(Database::storage_kind(&src), Some(StorageKind::Snapshot));

    // The destination: a RocksDB-era backup (its store, with a swap
    // sibling), and a directory where the copy must write `dict.bin`.
    fs::create_dir_all(dst.join("rocksdb")).unwrap();
    fs::write(dst.join("rocksdb/CURRENT"), b"MANIFEST-000001\n").unwrap();
    fs::create_dir_all(dst.join("rocksdb.old")).unwrap();
    fs::create_dir_all(dst.join("dict.bin/blocker")).unwrap();
    assert_eq!(Database::storage_kind(&dst), Some(StorageKind::RocksDb));

    let r = Database::backup_dir(&src, &dst);
    assert!(r.is_err(), "the copy fails part-way");
    assert!(
        !dst.join("rocksdb").exists() && !dst.join("rocksdb.old").exists(),
        "the stale store went before the copy began"
    );
    assert_ne!(
        Database::storage_kind(&dst),
        Some(StorageKind::RocksDb),
        "the destination no longer looks like the old RocksDB database"
    );

    // Once the obstacle is gone, the backup completes and opens.
    fs::remove_dir_all(dst.join("dict.bin")).unwrap();
    Database::backup_dir(&src, &dst).unwrap();
    assert_eq!(Database::storage_kind(&dst), Some(StorageKind::Snapshot));
    let copy = Database::open(&dst).unwrap();
    assert_eq!(copy.stats().triple_num, 2);
}
