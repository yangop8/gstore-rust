//! `restore_incremental` reads a base that is not a snapshot through a
//! private copy beside the base (round-3 review).
//!
//! The copy used to live in the system's temporary directory under the
//! predictable name `gstore-restore-<pid>-<n>`: the restore removed
//! whatever was there (silently failing on another user's entry in a
//! sticky `/tmp`) and copied the base into it with `create_dir_all`, so a
//! directory or symlink planted under that name was written into and read
//! back, and two containers with the same pid sharing `/tmp` mixed each
//! other's copies. It also put a copy of the whole base on `/tmp`, often a
//! small tmpfs. The scratch directory is now created exclusively (mode
//! `0700`, a fresh name) beside the base, on its file system, and only a
//! directory the restore created is removed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use gstore::{Database, QueryResult};

/// A directory removed when dropped — also when the test fails.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore_restore_private_{}_{}_{tag}",
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

/// Directories planted at the names the restore's copy used to take,
/// removed when dropped.
struct Planted(Vec<PathBuf>);

impl Drop for Planted {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = fs::remove_dir_all(p);
        }
    }
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_disk_base_is_copied_privately_beside_itself() {
    let tmp = TempDir::new("disk");
    let nt = tmp.path("base.nt");
    let data: String = (0..200)
        .map(|i| format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"))
        .collect();
    fs::write(&nt, data).unwrap();
    Database::build_disk(tmp.path("db"), &[&nt]).unwrap();
    let backups = tmp.path("backups");
    fs::create_dir_all(&backups).unwrap();
    {
        let mut db = Database::open(tmp.path("db")).unwrap();
        db.enable_redo_log(tmp.path("redo.log"));
        db.backup(backups.join("full")).unwrap();
        db.query("INSERT DATA { <http://ex/new> <http://ex/p> <http://ex/o> }")
            .unwrap();
        db.backup_incremental(backups.join("inc")).unwrap();
    }
    assert!(
        Database::storage_kind(backups.join("full"))
            .is_some_and(|k| !matches!(k, gstore::StorageKind::Snapshot)),
        "the base is not a snapshot (it is read through a copy)"
    );

    // Directories (one holding a file) at the copy's old names in the
    // shared temporary directory: none may be written into or removed.
    let pid = std::process::id();
    let planted = Planted(
        (0..4)
            .map(|n| std::env::temp_dir().join(format!("gstore-restore-{pid}-{n}")))
            .collect(),
    );
    for p in &planted.0 {
        fs::create_dir_all(p).unwrap();
        fs::write(p.join("planted"), b"not yours").unwrap();
    }
    let before = entries(&backups);

    let mut restored =
        Database::restore_incremental(backups.join("full"), &[backups.join("inc")]).unwrap();
    match restored.query("ASK { <http://ex/new> ?p ?o . <http://ex/s7> ?q ?r }") {
        Ok(QueryResult::Ask(true)) => {}
        other => panic!("restored database: {other:?}"),
    }

    for p in &planted.0 {
        assert!(
            p.join("planted").exists(),
            "'{}' was removed by the restore",
            p.display()
        );
        assert_eq!(
            entries(p),
            ["planted"],
            "'{}' was written into or emptied by the restore",
            p.display()
        );
    }
    // The copy beside the base is gone again.
    assert_eq!(
        entries(&backups),
        before,
        "a scratch copy was left beside the base"
    );
}
