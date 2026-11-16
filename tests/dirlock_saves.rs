//! Directory locks for saves and loads (reviews db#10, db#11, db#17).
//!
//! "Another process" is played by an independent open file description of
//! `<dir>/LOCK`: an advisory file lock taken through it excludes this
//! process's own locks exactly as another process's would, and this
//! process's lock registry does not know it.
//!
//! * db#10 — the lock registry is keyed by the locked `LOCK` file, not by
//!   the directory's path: a locked directory moved away keeps its lock, a
//!   new directory at the old path is locked afresh.
//! * db#11 — a save into a directory that is not the database's home takes
//!   that directory's lock whatever it holds (a new or empty directory
//!   too), so a save into a directory another saver holds is refused,
//!   touching nothing, and a save releases the lock when it returns. (Two
//!   saves of one process into the same new directory are serialized; the
//!   deterministic interleaving test is the unit test
//!   `db::dirlock::tests::two_saves_into_one_new_directory_never_interleave`,
//!   which parks a save through a crate-internal hook.)
//! * db#17 — `Database::load_disk` / `load_disk_with` take the directory
//!   lock, as `Database::open` does, and hold it for the database's life.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::db::dirlock::{DirLock, LOCK_FILE};
use gstore::{Database, DiskQueryMode, Term, Triple};

/// A temp directory (canonical path) removed when dropped, also when the
/// test fails.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_dirlock_saves_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(fs::canonicalize(&p).unwrap())
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open_lock_file(dir: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE))
        .unwrap()
}

/// Hold `<dir>/LOCK` as another process would.
fn foreign_lock(dir: &Path) -> File {
    let f = open_lock_file(dir);
    f.try_lock().expect("the directory is free");
    f
}

/// Whether another process could take `<dir>/LOCK` now (released at once).
fn free_elsewhere(dir: &Path) -> bool {
    match open_lock_file(dir).try_lock() {
        Ok(()) => true,
        Err(TryLockError::WouldBlock) => false,
        Err(TryLockError::Error(e)) => panic!("{e}"),
    }
}

fn small_db(name: &str, triples: usize) -> Database {
    let mut nt = String::new();
    for i in 0..triples {
        nt.push_str(&format!("<http://ex/{name}{i}> <http://ex/p> \"{i}\" .\n"));
    }
    Database::build_from_str(name, &nt).unwrap()
}

fn assert_refused(e: gstore::GStoreError) {
    let msg = e.to_string();
    assert!(msg.contains("locked by another process"), "{msg}");
}

#[test]
fn a_locked_directory_moved_away_keeps_its_lock_and_its_old_path_is_locked_afresh() {
    let base = TmpDir::new("moved");
    let d = base.0.join("x.db");
    let moved = base.0.join("moved.db");
    fs::create_dir(&d).unwrap();
    let first = DirLock::acquire(&d).unwrap();
    fs::rename(&d, &moved).unwrap();
    assert!(!DirLock::held_here(&d), "nothing is at the old path");
    assert!(DirLock::held_here(&moved), "the moved directory stays held");
    assert!(first.holds(&moved) && !first.holds(&d));
    fs::create_dir(&d).unwrap();
    assert!(
        !DirLock::held_here(&d),
        "nothing this process locked is at the old path now"
    );
    let second = DirLock::acquire(&d).unwrap();
    assert!(second.holds(&d) && !second.holds(&moved));
    assert!(
        !free_elsewhere(&d),
        "the new directory at the old path must be locked, not share the moved one's entry"
    );
    assert!(!free_elsewhere(&moved));
    drop(first);
    assert!(free_elsewhere(&moved), "released by its last holder");
    assert!(!free_elsewhere(&d));
    // Shared within the process, released by the last holder.
    let shared = DirLock::acquire(&d).unwrap();
    drop(second);
    assert!(!free_elsewhere(&d), "still held by `shared`");
    drop(shared);
    assert!(free_elsewhere(&d));
}

#[test]
fn a_database_reopened_after_restore_into_its_open_directory_holds_the_new_directory() {
    let base = TmpDir::new("restore");
    let (live, backup) = (base.0.join("live.db"), base.0.join("backup.db"));
    small_db("old", 3).save(&live).unwrap();
    small_db("new", 5).save(&backup).unwrap();
    let still_open = Database::open(&live).unwrap();
    Database::restore_into(&backup, &live).unwrap();
    let reopened = Database::open(&live).unwrap();
    assert_eq!(reopened.triple_num(), 5);
    assert!(
        !free_elsewhere(&live),
        "another process could open the restored database while this one serves it"
    );
    drop(still_open);
    assert!(!free_elsewhere(&live));
    drop(reopened);
    assert!(free_elsewhere(&live));
}

#[test]
fn a_save_into_an_empty_directory_another_process_holds_is_refused_and_touches_nothing() {
    let base = TmpDir::new("save_held");
    let target = base.0.join("new.db");
    fs::create_dir(&target).unwrap();
    let held = foreign_lock(&target);
    let db = small_db("s", 4);
    assert_refused(db.save(&target).unwrap_err());
    assert_refused(db.backup(&target).unwrap_err());
    assert_eq!(Database::storage_kind(&target), None, "nothing was written");
    let names: Vec<_> = fs::read_dir(&target)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec![LOCK_FILE.to_string()]);
    drop(held);
    db.save(&target).unwrap();
    assert_eq!(Database::open(&target).unwrap().triple_num(), 4);
}

#[test]
fn a_save_into_a_new_directory_releases_its_lock_when_it_returns() {
    let base = TmpDir::new("save_new");
    let target = base.0.join("a").join("new.db");
    let db = small_db("n", 2);
    db.save(&target).unwrap();
    assert!(!DirLock::held_here(&target));
    assert!(free_elsewhere(&target));
    // ... and a later save into it, over a database now, is refused while
    // another process holds it.
    let held = foreign_lock(&target);
    assert_refused(small_db("m", 1).save(&target).unwrap_err());
    drop(held);
    assert_eq!(Database::open(&target).unwrap().triple_num(), 2);
}

#[test]
fn a_disk_database_saved_into_another_directory_takes_that_directorys_lock() {
    let base = TmpDir::new("disk_copy");
    let (home, copy) = (base.0.join("home.db"), base.0.join("copy.db"));
    let nt = base.0.join("in.nt");
    fs::write(&nt, "<http://ex/s> <http://ex/p> <http://ex/o> .\n").unwrap();
    Database::build_disk(&home, &[&nt]).unwrap();
    let db = Database::open(&home).unwrap();
    fs::create_dir(&copy).unwrap();
    let held = foreign_lock(&copy);
    assert_refused(db.save(&copy).unwrap_err());
    assert_eq!(Database::storage_kind(&copy), None);
    drop(held);
    db.save(&copy).unwrap();
    assert!(free_elsewhere(&copy));
    drop(db);
    assert_eq!(Database::open(&copy).unwrap().triple_num(), 1);
}

#[test]
fn load_disk_takes_the_directory_lock_and_holds_it() {
    let base = TmpDir::new("load_disk");
    let dir = base.0.join("disk.db");
    let nt = base.0.join("in.nt");
    fs::write(&nt, "<http://ex/s> <http://ex/p> <http://ex/o> .\n").unwrap();
    Database::build_disk(&dir, &[&nt]).unwrap();

    let held = foreign_lock(&dir);
    assert_refused(Database::load_disk(&dir).unwrap_err());
    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let options = gstore::OpenOptions::new().with_disk_query_mode(mode);
        assert_refused(Database::load_disk_with(&dir, &options).unwrap_err());
    }
    drop(held);

    for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
        let options = gstore::OpenOptions::new().with_disk_query_mode(mode);
        let mut db = Database::load_disk_with(&dir, &options).unwrap();
        assert!(db.holds_dir_lock());
        assert!(!free_elsewhere(&dir), "held for the database's life");
        // A second load in this process shares the lock (as `open` does).
        let again = Database::load_disk(&dir).unwrap();
        drop(again);
        assert!(!free_elsewhere(&dir));
        assert!(db.insert_triple(&Triple::new(
            Term::iri(format!("http://ex/{mode}")),
            Term::iri("http://ex/p"),
            Term::iri("http://ex/o2"),
        )));
        db.sync().unwrap();
        drop(db);
        assert!(free_elsewhere(&dir));
    }
    assert_eq!(Database::open(&dir).unwrap().triple_num(), 3);
}

#[test]
fn hold_dir_lock_takes_the_directory_now_at_the_path() {
    let base = TmpDir::new("hold");
    let (d, moved) = (base.0.join("x.db"), base.0.join("moved.db"));
    let mut db = small_db("h", 1);
    db.save(&d).unwrap();
    db.hold_dir_lock(&d).unwrap();
    fs::rename(&d, &moved).unwrap();
    db.save(&d).unwrap();
    // The database's lock is on the moved directory; holding `d` must lock
    // the directory now there.
    db.hold_dir_lock(&d).unwrap();
    assert!(!free_elsewhere(&d));
    assert!(
        free_elsewhere(&moved),
        "the moved directory's lock was released"
    );
}
