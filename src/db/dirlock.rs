//! Inter-process exclusion for database directories.
//!
//! A database directory may be open for writing by **one process** at a time:
//! two processes writing the same on-disk B+ tree (`kvstore.kv` and its WAL)
//! or saving the same snapshot over each other corrupt it or silently lose
//! one side's updates. [`Database::open`](super::Database::open) therefore
//! takes an exclusive advisory lock on `<dir>/LOCK` ([`LOCK_FILE`], an
//! operating-system file lock — `flock` on Unix, `LockFileEx` on Windows — so
//! it is released by the kernel even when the holder crashes) for the
//! snapshot and on-disk layouts, and holds it for as long as the
//! [`Database`](super::Database) lives. A second process opening the same
//! directory gets a clear error naming the holder's pid
//! ([`GStoreError::Database`], "… is locked by another process …"). RocksDB
//! stores are not locked here: RocksDB locks its own `rocksdb/LOCK`.
//!
//! **The rule within one process.** The lock excludes *other processes*. Within
//! a process the lock is reference-counted per (canonical) directory: opening
//! a directory this process already holds succeeds and shares the lock, which
//! is released when the last holder is dropped. Code that opens the same
//! database twice in one process (tests do, and the server reopens a database
//! after a restore) therefore keeps working; keeping two *writers* of one
//! directory apart inside a process is the caller's job — the HTTP server does
//! it with its registry of loaded databases and its per-name lifecycle
//! reservations.
//!
//! On a file system without advisory-lock support the lock is skipped (the
//! database still opens): exclusion is a safety net, never a reason to refuse
//! service where it cannot be provided.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::error::{GStoreError, Result};

/// The lock file inside a database directory.
pub const LOCK_FILE: &str = "LOCK";

/// A lock this process holds, shared by every [`DirLock`] on the directory.
struct Held {
    /// The locked file (`None` where the file system has no advisory locks).
    file: Option<File>,
    holders: usize,
}

/// The directories this process holds, by canonical path.
fn registry() -> &'static Mutex<HashMap<PathBuf, Held>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Held>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Exclusive (per process) ownership of a database directory; see the module
/// docs. Released when dropped.
#[derive(Debug)]
pub struct DirLock {
    /// The canonical directory.
    dir: PathBuf,
}

impl DirLock {
    /// Lock directory `dir` (which must exist) for this process: shares the
    /// lock when this process already holds it, else takes `<dir>/LOCK`
    /// exclusively — failing with a [`GStoreError::Database`] that names the
    /// holder when another process has it.
    pub fn acquire(dir: &Path) -> Result<DirLock> {
        let canonical = fs::canonicalize(dir).map_err(|e| {
            GStoreError::Database(format!(
                "cannot lock database directory '{}': {e}",
                dir.display()
            ))
        })?;
        let mut reg = registry().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = reg.get_mut(&canonical) {
            held.holders += 1;
            return Ok(DirLock { dir: canonical });
        }
        let path = canonical.join(LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let file = match file.try_lock() {
            Ok(()) => {
                // Record the holder for the error message of a later contender
                // (best effort: the lock, not the content, is what excludes).
                let _ = record_pid(&mut file);
                Some(file)
            }
            Err(TryLockError::WouldBlock) => {
                let holder = read_pid(&mut file)
                    .map(|pid| format!(" (pid {pid})"))
                    .unwrap_or_default();
                return Err(GStoreError::Database(format!(
                    "database directory '{}' is locked by another process{holder}: a gserver or \
                     another gStore tool has it open; stop it (or unload the database there) \
                     and retry",
                    dir.display()
                )));
            }
            // No advisory locks on this file system: open without one.
            Err(TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => None,
            Err(TryLockError::Error(e)) => {
                return Err(GStoreError::Database(format!(
                    "cannot lock database directory '{}': {e}",
                    dir.display()
                )))
            }
        };
        reg.insert(canonical.clone(), Held { file, holders: 1 });
        Ok(DirLock { dir: canonical })
    }

    /// The canonical directory this lock holds.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether this process holds a lock on `dir`.
    pub fn held_here(dir: &Path) -> bool {
        let Ok(canonical) = fs::canonicalize(dir) else {
            return false;
        };
        registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&canonical)
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let mut reg = registry().lock().unwrap_or_else(PoisonError::into_inner);
        let last = match reg.get_mut(&self.dir) {
            Some(held) => {
                held.holders = held.holders.saturating_sub(1);
                held.holders == 0
            }
            None => false,
        };
        if last {
            if let Some(Held {
                file: Some(file), ..
            }) = reg.remove(&self.dir)
            {
                // Closing the file releases the lock too; unlock explicitly so
                // it is gone before anyone can observe the registry change.
                let _ = file.unlock();
            }
        }
    }
}

fn record_pid(file: &mut File) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(64).read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gstore_dirlock_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn locks_are_shared_within_a_process_and_released_by_the_last_holder() {
        let d = tmp("shared");
        assert!(!DirLock::held_here(&d));
        let a = DirLock::acquire(&d).unwrap();
        assert!(DirLock::held_here(&d));
        let b = DirLock::acquire(&d.join(".")).unwrap();
        assert_eq!(a.dir(), b.dir());
        drop(a);
        assert!(DirLock::held_here(&d), "b still holds it");
        drop(b);
        assert!(!DirLock::held_here(&d));
        // The LOCK file records the holder's pid.
        let _c = DirLock::acquire(&d).unwrap();
        let pid = fs::read_to_string(d.join(LOCK_FILE)).unwrap();
        assert_eq!(pid.trim(), std::process::id().to_string());
        drop(_c);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_separate_open_file_description_is_refused_while_locked() {
        // What another process sees: an independent open of LOCK cannot take
        // the lock while this process holds it.
        let d = tmp("contend");
        let held = DirLock::acquire(&d).unwrap();
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(d.join(LOCK_FILE))
            .unwrap();
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
        drop(held);
        other.try_lock().expect("released");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let d = tmp("missing").join("nope");
        let e = DirLock::acquire(&d).unwrap_err().to_string();
        assert!(e.contains("cannot lock"), "{e}");
    }
}
