//! The server's startup reconcile only forgets a database (its registration
//! and every grant on it) whose directory is confirmed gone — not one that is
//! merely unreachable for a moment (review gapi#9).
//!
//! Before the fix a `<db>.db` symlink to a data volume not mounted yet (or
//! any stat failure) read as "database dropped": the registry entry and its
//! grants were deleted for good, and when the volume came back the database
//! re-registered with creator `root` and no grants.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::http_users::Privilege;
use gstore::sysdb::open_system;

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_sysdb_reconcile_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_database_behind_a_dangling_symlink_keeps_its_registration_and_grants() {
    let tmp = TmpDir::new("dangling");
    let root = tmp.0.join("root");
    let volume = tmp.0.join("volume");
    let parked = tmp.0.join("volume-unmounted");
    fs::create_dir_all(volume.join("lubm.db")).unwrap();
    gstore::Database::build_from_str("lubm", "<http://ex/a> <http://ex/p> <http://ex/b> .\n")
        .unwrap()
        .save(volume.join("lubm.db"))
        .unwrap();
    fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(volume.join("lubm.db"), root.join("lubm.db")).unwrap();

    let (mut users, mut sys) = open_system(&root, "pw").unwrap();
    // Found on disk at the first start; give it its real creator and a grant.
    assert_eq!(sys.names(), ["lubm"]);
    sys.unregister("lubm").unwrap();
    sys.register("lubm", "alice").unwrap();
    users
        .add("alice", "alicepw", BTreeSet::from([Privilege::Load]))
        .unwrap();
    users
        .grant_on("alice", "lubm", &BTreeSet::from([Privilege::Query]))
        .unwrap();
    drop((users, sys));

    // The volume is not mounted at the next start: the symlink dangles.
    fs::rename(&volume, &parked).unwrap();
    let (users, sys) = open_system(&root, "pw").unwrap();
    assert_eq!(sys.names(), ["lubm"], "unregistered while unreachable");
    assert_eq!(sys.get("lubm").unwrap().creator, "alice");
    assert!(
        users.has_priv_on("alice", Privilege::Query, "lubm"),
        "the grant was deleted while the database was unreachable"
    );
    drop((users, sys));

    // The volume is back: nothing was lost.
    fs::rename(&parked, &volume).unwrap();
    let (users, sys) = open_system(&root, "pw").unwrap();
    assert_eq!(sys.names(), ["lubm"]);
    assert_eq!(sys.get("lubm").unwrap().creator, "alice");
    assert!(users.has_priv_on("alice", Privilege::Query, "lubm"));
    drop((users, sys));

    // A database whose directory really is gone is still forgotten.
    fs::remove_file(root.join("lubm.db")).unwrap();
    let (users, sys) = open_system(&root, "pw").unwrap();
    assert!(sys.names().is_empty());
    assert!(!users.has_priv_on("alice", Privilege::Query, "lubm"));
}
