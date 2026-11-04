//! Inter-process exclusion for database directories.
//!
//! A database directory may be open for writing by **one process** at a time:
//! two processes writing the same on-disk B+ tree (`kvstore.kv` and its WAL)
//! or saving the same snapshot over each other corrupt it or silently lose
//! one side's updates. [`Database::open`](super::Database::open) therefore
//! takes an exclusive advisory lock on `<dir>/LOCK` ([`LOCK_FILE`], an
//! operating-system file lock — `flock` on Unix, `LockFileEx` on Windows — so
//! it is released by the kernel even when the holder crashes) for every
//! layout, and holds it for as long as the [`Database`](super::Database)
//! lives. A second process opening the same directory gets a clear error
//! naming the holder's pid ([`GStoreError::Database`], "… is locked by
//! another process …"). A RocksDB database holds both this lock and the
//! store's own `rocksdb/LOCK` (which RocksDB takes; older binaries take only
//! that one, which is why a bulk build also checks it).
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

use std::collections::{HashMap, HashSet};
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

    /// Lock `dir` like [`acquire`](Self::acquire) when it is a directory;
    /// `Ok(None)` when there is nothing there (yet) to lock. The step every
    /// server operation that replaces, renames or deletes a database
    /// directory takes first — the REST endpoints of [`crate::http_api`] and
    /// the `/api` operations alike — so none of them pulls a directory from
    /// under another process.
    pub fn acquire_existing(dir: &Path) -> Result<Option<DirLock>> {
        if dir.is_dir() {
            DirLock::acquire(dir).map(Some)
        } else {
            Ok(None)
        }
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

/// The side locks this process holds, by lock-file path.
fn side_registry() -> &'static Mutex<HashMap<PathBuf, Held>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Held>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Exclusive (per process) ownership of the lock file *beside* a directory
/// that need not exist yet, `<dir><suffix>` — for an operation that creates
/// the directory, which has no `LOCK` of its own to take meanwhile (a
/// restore into a new database stages its copy beside a `<dir>` that does
/// not exist). Shared within a process like a [`DirLock`]. The last holder
/// removes the file while it still holds the lock (on Unix): a contender
/// that opened the old file can lock it only after that, sees that the path
/// no longer names it, and starts over on the current one. Released when
/// dropped.
#[derive(Debug)]
pub(crate) struct SideLock {
    path: PathBuf,
}

impl SideLock {
    /// Lock `<dir><suffix>` (`dir`'s parent must exist), failing with a
    /// [`GStoreError::Database`] naming the holder when another process has
    /// it.
    pub(crate) fn acquire(dir: &Path, suffix: &str) -> Result<SideLock> {
        let cannot = |e: &dyn std::fmt::Display| {
            GStoreError::Database(format!(
                "cannot take the lock beside '{}': {e}",
                dir.display()
            ))
        };
        let parent = dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = dir.file_name().ok_or_else(|| cannot(&"no file name"))?;
        let mut file_name = name.to_os_string();
        file_name.push(suffix);
        let path = fs::canonicalize(parent)
            .map_err(|e| cannot(&e))?
            .join(file_name);
        let mut reg = side_registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = reg.get_mut(&path) {
            held.holders += 1;
            return Ok(SideLock { path });
        }
        // A few rounds: the last holder may remove the file between our open
        // and our lock (the path then names another file, or none).
        for _ in 0..8 {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| cannot(&e))?;
            let file = match file.try_lock() {
                Ok(()) if !still_named_by(&file, &path) => continue,
                Ok(()) => {
                    let _ = record_pid(&mut file);
                    Some(file)
                }
                Err(TryLockError::WouldBlock) => {
                    let holder = read_pid(&mut file)
                        .map(|pid| format!(" (pid {pid})"))
                        .unwrap_or_default();
                    return Err(GStoreError::Database(format!(
                        "'{}' is locked by another process{holder}: another gStore tool is \
                         creating or restoring the database there; retry when it is done",
                        dir.display()
                    )));
                }
                // No advisory locks on this file system: proceed without one.
                Err(TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => None,
                Err(TryLockError::Error(e)) => return Err(cannot(&e)),
            };
            reg.insert(path.clone(), Held { file, holders: 1 });
            return Ok(SideLock { path });
        }
        Err(cannot(&"its lock file keeps being replaced"))
    }

    /// The lock file.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether `path` still names the file `file` has open (always `true` where
/// that cannot be told; the last holder then leaves the file in place).
fn still_named_by(file: &File, path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (file.metadata(), fs::metadata(path)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        true
    }
}

impl Drop for SideLock {
    fn drop(&mut self) {
        let mut reg = side_registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let last = match reg.get_mut(&self.path) {
            Some(held) => {
                held.holders = held.holders.saturating_sub(1);
                held.holders == 0
            }
            None => false,
        };
        if last {
            if let Some(Held {
                file: Some(file), ..
            }) = reg.remove(&self.path)
            {
                // Removed while still locked (see the type docs), then
                // released. Elsewhere the file stays.
                #[cfg(unix)]
                let _ = fs::remove_file(&self.path);
                let _ = file.unlock();
            }
        }
    }
}

/// The directories a bulk build of this process is rebuilding.
fn building() -> &'static Mutex<HashSet<PathBuf>> {
    static SET: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// This process's claim on a directory a bulk build is rebuilding
/// ([`crate::bulk::build`]). The [`DirLock`] keeps other processes out; this
/// keeps a second build in the same process out (the `DirLock` is shared
/// within a process). Released when dropped.
#[derive(Debug)]
pub struct BuildGuard {
    dir: PathBuf,
}

impl BuildGuard {
    /// Claim directory `dir` (which must exist); fails if a build of this
    /// process already rebuilds it.
    pub fn enter(dir: &Path) -> Result<BuildGuard> {
        let canonical = fs::canonicalize(dir).map_err(|e| {
            GStoreError::Database(format!(
                "cannot lock database directory '{}': {e}",
                dir.display()
            ))
        })?;
        let mut set = building().lock().unwrap_or_else(PoisonError::into_inner);
        if !set.insert(canonical.clone()) {
            return Err(GStoreError::Database(format!(
                "database directory '{}' is being rebuilt by another bulk build of this process",
                dir.display()
            )));
        }
        Ok(BuildGuard { dir: canonical })
    }

    /// Whether a build of this process is rebuilding `dir`.
    pub fn active(dir: &Path) -> bool {
        let Ok(canonical) = fs::canonicalize(dir) else {
            return false;
        };
        building()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&canonical)
    }
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        building()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.dir);
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
        // ... unless the caller only wants an existing directory locked.
        assert!(DirLock::acquire_existing(&d).unwrap().is_none());
        let here = DirLock::acquire_existing(d.parent().unwrap()).unwrap();
        assert!(here.is_some_and(|l| DirLock::held_here(l.dir())));
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }

    #[test]
    fn a_side_lock_is_shared_refused_elsewhere_and_removed_by_its_last_holder() {
        let d = tmp("side");
        let target = d.join("new.db");
        let a = SideLock::acquire(&target, ".restore-lock").unwrap();
        let file = a.path().to_path_buf();
        assert_eq!(
            file,
            fs::canonicalize(&d).unwrap().join("new.db.restore-lock")
        );
        assert!(!target.exists(), "the directory itself is not created");
        let b = SideLock::acquire(&target, ".restore-lock").unwrap();
        // Another process (an independent open) cannot take it ...
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file)
            .unwrap();
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
        drop(a);
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
        drop(b);
        // ... and once the last holder is gone the file is gone too: a
        // contender that had opened it locks a file no path names, which
        // `acquire` would see and start over.
        #[cfg(unix)]
        {
            assert!(!file.exists());
            other.try_lock().expect("released");
            assert!(!still_named_by(&other, &file));
        }
        drop(other);
        // Refused while another process holds it.
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&file)
            .unwrap();
        held.try_lock().unwrap();
        let e = SideLock::acquire(&target, ".restore-lock")
            .unwrap_err()
            .to_string();
        assert!(e.contains("locked by another process"), "{e}");
        drop(held);
        drop(SideLock::acquire(&target, ".restore-lock").unwrap());
        let _ = fs::remove_dir_all(&d);
    }
}
