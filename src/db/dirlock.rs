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
//! naming the holder's pid ([`GStoreError::Locked`], "… is locked by
//! another process …"); a failure to take the lock at all — the lock file
//! cannot be opened, the file system refuses the lock call — is an I/O
//! error ([`GStoreError::Io`]), so a server answers the conflict as one
//! (HTTP `409`) and the I/O failure as its own fault (`500`) (review
//! design#3). A RocksDB database holds both this lock and the
//! store's own `rocksdb/LOCK` (which RocksDB takes; older binaries take only
//! that one, which is why a bulk build also checks it).
//!
//! **The rule within one process.** The lock excludes *other processes*. Within
//! a process the lock is reference-counted per locked file: opening a
//! directory whose `LOCK` file this process already holds succeeds and shares
//! the lock, which is released when the last holder is dropped. Code that
//! opens the same database twice in one process (tests do, and the server
//! reopens a database after a restore) therefore keeps working; keeping two
//! *writers* of one directory apart inside a process is the caller's job — the
//! HTTP server does it with its registry of loaded databases and its per-name
//! lifecycle reservations — except for the one write a database makes into a
//! directory that is not its own: a [`save`](super::Database::save) (or
//! [`backup`](super::Database::backup)) into another directory also holds
//! [`DirLock::writing`], so two saves into the same directory, in this
//! process or in two, never interleave their files.
//!
//! **Bulk builds.** A directory a bulk build (or any other replacement of
//! a database's files: [`crate::bulk::build`], the incremental builds) is
//! rebuilding is claimed with [`BuildGuard::lock`], which takes the lock and
//! the claim in one step under the registry's mutex: it fails while any
//! holder of this process — a [`Database`](super::Database) that has the
//! directory open, another build — holds the directory, and while the claim
//! lives every [`DirLock::acquire`] of *another thread* of this process is
//! refused like another process's (review bulkcli#13: the "open in this
//! process?" check and the lock were two steps, and an open after them
//! shared the build's lock and wrote into the files the install then
//! replaced). The thread that holds the claim may still take the lock — the
//! save of the database it builds, a nested build, its own reopen — so a
//! tool can claim a directory before it prepares its input and keep the
//! claim through the build (`gbuild`).
//!
//! **Which file is locked.** The registry is keyed by the identity of the
//! locked `LOCK` file — its (device, inode) on Unix — not by the directory's
//! path: a restore or an `/api` rename moves a locked directory (its `LOCK`
//! file with it), and a new directory at the old path has a `LOCK` file of
//! its own, which a later [`DirLock::acquire`] locks afresh instead of
//! sharing the entry of the file that moved away (review db#10). (Off Unix,
//! where the standard library gives no file identity, the key is the
//! canonical directory path, as before.)
//!
//! On a file system without advisory-lock support the lock is skipped (the
//! database still opens): exclusion is a safety net, never a reason to refuse
//! service where it cannot be provided.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, ThreadId};

use crate::error::{GStoreError, Result};

/// The lock file inside a database directory.
pub const LOCK_FILE: &str = "LOCK";

/// A failure to take a lock at all (`what`: the attempt, with its path): an
/// I/O error of that kind ([`GStoreError::Io`], a storage failure), never
/// a lock conflict (review design#3).
fn lock_io_error(what: String, e: &dyn std::fmt::Display, kind: io::ErrorKind) -> GStoreError {
    GStoreError::Io(io::Error::new(kind, format!("{what}: {e}")))
}

/// The lock is held by another process (or by another build of this one):
/// a conflict, [`GStoreError::Locked`] (review design#3).
fn lock_conflict(msg: String) -> GStoreError {
    GStoreError::Locked(msg)
}

/// The directory is claimed by a bulk build of this process
/// ([`BuildGuard`]): a conflict, like another process's lock.
fn being_rebuilt(dir: &Path) -> GStoreError {
    lock_conflict(format!(
        "database directory '{}' is being rebuilt by another bulk build of this process",
        dir.display()
    ))
}

/// Another process keeps moving the directory (a restore, a rename):
/// contention, to retry once it is done.
fn keeps_being_replaced(dir: &Path) -> GStoreError {
    lock_conflict(format!(
        "cannot lock database directory '{}': its lock file keeps being replaced",
        dir.display()
    ))
}

/// How [`DirLock::take`] takes a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// [`DirLock::acquire`]: shared with this process's other holders.
    Share,
    /// [`BuildGuard::lock`]: a bulk build's claim.
    Build,
}

/// A lock this process holds, shared by every [`SideLock`] on the file.
struct Held {
    /// The locked file (`None` where the file system has no advisory locks).
    file: Option<File>,
    holders: usize,
}

/// The identity of a directory's `LOCK` file: its (device, inode) on Unix —
/// stable while the directory is renamed, distinct from the `LOCK` file of a
/// new directory at the old path — and the canonical directory path
/// elsewhere, where the standard library gives no file identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LockKey(#[cfg(unix)] (u64, u64), #[cfg(not(unix))] PathBuf);

/// The key of the open lock file `file` of canonical directory `dir`.
fn key_of(file: &File, dir: &Path) -> io::Result<LockKey> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = dir;
        let meta = file.metadata()?;
        Ok(LockKey((meta.dev(), meta.ino())))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(LockKey(dir.to_path_buf()))
    }
}

/// The key of the file `<dir>/LOCK` names now (canonical `dir`), without
/// opening or creating it; `None` when there is none.
fn key_at(dir: &Path) -> Option<LockKey> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(dir.join(LOCK_FILE)).ok()?;
        Some(LockKey((meta.dev(), meta.ino())))
    }
    #[cfg(not(unix))]
    {
        Some(LockKey(dir.to_path_buf()))
    }
}

/// A directory lock this process holds, shared by every [`DirLock`] on the
/// same `LOCK` file.
struct DirHeld {
    /// The lock file, kept open for as long as the lock is held (also where
    /// the file system has no advisory locks: an open file keeps its inode,
    /// so no other file can take over its key meanwhile).
    file: File,
    /// Whether `file` is locked (`false` where the file system has no
    /// advisory locks).
    locked: bool,
    holders: usize,
    /// Serializes the writers of this process that hold
    /// [`DirLock::writing`].
    writer: Arc<Mutex<()>>,
    /// The thread whose bulk build claims the directory
    /// ([`BuildGuard::lock`]) and how many claims it holds: while set,
    /// [`DirLock::acquire`] on every other thread is refused.
    builder: Option<(ThreadId, usize)>,
}

/// The directory locks this process holds, by the identity of the locked
/// `LOCK` file (see the module docs).
fn registry() -> &'static Mutex<HashMap<LockKey, DirHeld>> {
    static REG: OnceLock<Mutex<HashMap<LockKey, DirHeld>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Exclusive (per process) ownership of a database directory; see the module
/// docs. Released when dropped.
#[derive(Debug)]
pub struct DirLock {
    /// The canonical directory, as it was named when the lock was taken.
    dir: PathBuf,
    /// The locked file's registry key.
    key: LockKey,
    /// The writer exclusion shared by every holder of the same file.
    writer: Arc<Mutex<()>>,
}

impl DirLock {
    /// Lock directory `dir` (which must exist) for this process: shares the
    /// lock when this process already holds the file `<dir>/LOCK` names (not
    /// merely a lock taken under the same path: see the module docs), else
    /// takes `<dir>/LOCK` exclusively — failing with a
    /// [`GStoreError::Locked`] that names the holder when another process
    /// has it, or when a bulk build on another thread of this process claims
    /// the directory ([`BuildGuard::lock`]), or with a [`GStoreError::Io`]
    /// when the lock cannot be taken at all (the directory or its lock file
    /// cannot be opened, the lock call fails).
    pub fn acquire(dir: &Path) -> Result<DirLock> {
        DirLock::take(dir, Mode::Share).map(|(lock, _)| lock)
    }

    /// [`acquire`](Self::acquire) (`Mode::Share`), or a bulk build's claim
    /// (`Mode::Build`, see [`BuildGuard::lock`]) — the lock and the claim
    /// taken in one step under the registry's mutex. With the lock, whether
    /// this call made the claim (`false` when the thread already held one
    /// and this one nests in it, and for `Mode::Share`).
    fn take(dir: &Path, mode: Mode) -> Result<(DirLock, bool)> {
        let cannot = |e: &io::Error| {
            lock_io_error(
                format!("cannot lock database directory '{}'", dir.display()),
                e,
                e.kind(),
            )
        };
        let canonical = fs::canonicalize(dir).map_err(|e| cannot(&e))?;
        let path = canonical.join(LOCK_FILE);
        let me = thread::current().id();
        let mut reg = registry().lock().unwrap_or_else(PoisonError::into_inner);
        // A few rounds: another process's restore or rename may move the
        // directory (its lock file with it) between our open and our lock.
        for _ in 0..8 {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| cannot(&e))?;
            let key = key_of(&file, &canonical).map_err(|e| cannot(&e))?;
            if let Some(held) = reg.get_mut(&key) {
                match (mode, &mut held.builder) {
                    // A bulk build of another thread is replacing the files:
                    // sharing its lock would write into files its install
                    // replaces, and a second build would share its staging
                    // area (review bulkcli#13).
                    (_, Some((t, _))) if *t != me => return Err(being_rebuilt(dir)),
                    // The claiming thread's own build nests in its claim.
                    (Mode::Build, Some((_, claims))) => *claims += 1,
                    // A holder of this process (an open database): replacing
                    // its files under it would strand its later writes (and
                    // a WAL of the old store could replay into the new one).
                    (Mode::Build, None) => {
                        return Err(GStoreError::Database(format!(
                            "database directory '{}' is open in this process; close it before \
                             rebuilding it",
                            dir.display()
                        )))
                    }
                    (Mode::Share, _) => {}
                }
                // This process holds the very file `<dir>/LOCK` names: share
                // it. (Dropping the probe descriptor keeps the lock: an
                // advisory file lock belongs to the descriptor that took it.)
                held.holders += 1;
                return Ok((
                    DirLock {
                        dir: canonical,
                        key,
                        writer: Arc::clone(&held.writer),
                    },
                    false,
                ));
            }
            // Not held here. A build's claim also enters the directory into
            // the set of directories being rebuilt (which an older-style
            // claim, [`BuildGuard::enter`], may hold already): checked and
            // entered under the registry's mutex (lock order: registry, then
            // the set).
            let mut set = match mode {
                Mode::Build => {
                    let set = building().lock().unwrap_or_else(PoisonError::into_inner);
                    if set.contains(&canonical) {
                        return Err(being_rebuilt(dir));
                    }
                    Some(set)
                }
                Mode::Share => None,
            };
            let locked = match file.try_lock() {
                // Moved away (or replaced) since the open: start over on the
                // file the path names now.
                Ok(()) if !still_named_by(&file, &path) => continue,
                Ok(()) => {
                    // Record the holder for the error message of a later
                    // contender (best effort: the lock, not the content, is
                    // what excludes).
                    let _ = record_pid(&mut file);
                    true
                }
                Err(TryLockError::WouldBlock) => {
                    let holder = read_pid(&mut file)
                        .map(|pid| format!(" (pid {pid})"))
                        .unwrap_or_default();
                    return Err(lock_conflict(format!(
                        "database directory '{}' is locked by another process{holder}: a gserver or \
                         another gStore tool has it open; stop it (or unload the database there) \
                         and retry",
                        dir.display()
                    )));
                }
                // No advisory locks on this file system: open without one.
                Err(TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => false,
                Err(TryLockError::Error(e)) => return Err(cannot(&e)),
            };
            let writer = Arc::new(Mutex::new(()));
            reg.insert(
                key.clone(),
                DirHeld {
                    file,
                    locked,
                    holders: 1,
                    writer: Arc::clone(&writer),
                    builder: (mode == Mode::Build).then_some((me, 1)),
                },
            );
            if let Some(set) = &mut set {
                set.insert(canonical.clone());
            }
            return Ok((
                DirLock {
                    dir: canonical,
                    key,
                    writer,
                },
                mode == Mode::Build,
            ));
        }
        // Another process keeps moving the directory (a restore, a
        // rename): contention, to retry once it is done.
        Err(keeps_being_replaced(dir))
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

    /// The canonical directory this lock was taken on (where its `LOCK` file
    /// was then; see [`holds`](Self::holds) for where it is now).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether this lock is on the file `<dir>/LOCK` names now — `false`
    /// once the locked directory was moved away (renamed, swapped out by a
    /// restore) and `dir` is another directory.
    pub fn holds(&self, dir: &Path) -> bool {
        fs::canonicalize(dir)
            .ok()
            .and_then(|canonical| key_at(&canonical))
            .is_some_and(|key| key == self.key)
    }

    /// Whether this process holds a lock on `dir` (on the file `<dir>/LOCK`
    /// names now).
    pub fn held_here(dir: &Path) -> bool {
        let Some(key) = fs::canonicalize(dir)
            .ok()
            .and_then(|canonical| key_at(&canonical))
        else {
            return false;
        };
        registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&key)
    }

    /// Exclusive use of the locked directory among this process's writers
    /// that ask for it: blocks while another holder of the same lock file
    /// holds it (the inter-process lock already keeps other processes out).
    /// [`Database::save`](super::Database::save) into a directory that is not
    /// the database's home holds it for the whole save, so two saves into
    /// one directory never interleave — the second waits for the first, and
    /// the snapshot recovery at the start of a save never deletes the files
    /// another save is staging (review db#11).
    pub fn writing(&self) -> MutexGuard<'_, ()> {
        match self.writer.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => {
                #[cfg(test)]
                writer_probe::waiting(&self.dir);
                self.writer.lock().unwrap_or_else(PoisonError::into_inner)
            }
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let mut reg = registry().lock().unwrap_or_else(PoisonError::into_inner);
        let last = match reg.get_mut(&self.key) {
            Some(held) => {
                held.holders = held.holders.saturating_sub(1);
                held.holders == 0
            }
            None => false,
        };
        if last {
            if let Some(held) = reg.remove(&self.key) {
                // Closing the file releases the lock too; unlock explicitly so
                // it is gone before anyone can observe the registry change.
                if held.locked {
                    let _ = held.file.unlock();
                }
            }
        }
    }
}

/// Test hook: who is told when a [`DirLock::writing`] has to wait.
#[cfg(test)]
pub(crate) mod writer_probe {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};

    /// What to call when a writer has to wait.
    pub(crate) type Hook = Arc<dyn Fn() + Send + Sync>;

    /// The watched directories (canonical paths) and what to call.
    static WATCHED: Mutex<Vec<(PathBuf, Hook)>> = Mutex::new(Vec::new());

    /// Call `hook` (on the waiting thread) whenever a writer of canonical
    /// directory `dir` has to wait.
    pub(crate) fn watch(dir: &Path, hook: Hook) {
        WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((dir.to_path_buf(), hook));
    }

    /// Stop watching `dir`.
    pub(crate) fn unwatch(dir: &Path) {
        WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(d, _)| d != dir);
    }

    /// Called by [`DirLock::writing`](super::DirLock::writing) before it
    /// blocks on canonical directory `dir`.
    pub(super) fn waiting(dir: &Path) {
        let hooks: Vec<Hook> = WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(d, _)| d == dir)
            .map(|(_, h)| Arc::clone(h))
            .collect();
        for hook in hooks {
            hook();
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
    /// [`GStoreError::Locked`] naming the holder when another process has
    /// it, or with a [`GStoreError::Io`] when the lock cannot be taken at
    /// all.
    pub(crate) fn acquire(dir: &Path, suffix: &str) -> Result<SideLock> {
        let what = || format!("cannot take the lock beside '{}'", dir.display());
        let cannot = |e: &io::Error| lock_io_error(what(), e, e.kind());
        let parent = dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = dir
            .file_name()
            .ok_or_else(|| lock_io_error(what(), &"no file name", io::ErrorKind::InvalidInput))?;
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
                    return Err(lock_conflict(format!(
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
        Err(lock_conflict(format!(
            "{}: its lock file keeps being replaced",
            what()
        )))
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
/// ([`crate::bulk::build`], the incremental builds). The [`DirLock`] keeps
/// other processes out; the claim keeps a second build of this process out
/// (the `DirLock` is shared within a process) and — when taken with
/// [`lock`](Self::lock), as every build takes it — every other thread's
/// [`DirLock::acquire`] too, so no database of this process opens the
/// directory while its files are replaced. Released when dropped.
#[derive(Debug)]
pub struct BuildGuard {
    /// The canonical directory.
    dir: PathBuf,
    /// Whether this guard put `dir` into [`building`] (the outermost claim).
    registered: bool,
    /// The claimed lock file's key and the claiming thread, for a guard from
    /// [`lock`](Self::lock).
    claim: Option<(LockKey, ThreadId)>,
}

impl BuildGuard {
    /// Claim directory `dir` (which must exist) by its path only, without
    /// its lock; fails with a [`GStoreError::Locked`] if a build of this
    /// process already rebuilds it (an [`GStoreError::Io`] if `dir` cannot be
    /// resolved). The builds use [`lock`](Self::lock), which also takes the
    /// directory's lock in the same step.
    pub fn enter(dir: &Path) -> Result<BuildGuard> {
        let canonical = fs::canonicalize(dir).map_err(|e| {
            lock_io_error(
                format!("cannot lock database directory '{}'", dir.display()),
                &e,
                e.kind(),
            )
        })?;
        let mut set = building().lock().unwrap_or_else(PoisonError::into_inner);
        if !set.insert(canonical.clone()) {
            return Err(being_rebuilt(dir));
        }
        Ok(BuildGuard {
            dir: canonical,
            registered: true,
            claim: None,
        })
    }

    /// Claim directory `dir` (which must exist) for a bulk build and take its
    /// [`DirLock`] — one atomic step under the lock registry's mutex (review
    /// bulkcli#13). Fails with a [`GStoreError::Locked`] when another process
    /// holds the directory, or a build on another thread of this process
    /// claims it; with a [`GStoreError::Database`] ("open in this process")
    /// when a holder of this process — an open database — has it; with a
    /// [`GStoreError::Io`] when the lock cannot be taken at all.
    ///
    /// While the claim lives, [`DirLock::acquire`] on any *other* thread of
    /// this process is refused like another process's. The claiming thread
    /// itself may still take the lock and nest further claims in this one:
    /// a tool claims the directory before it prepares its input and then
    /// runs the build (and the save, and its own reopen) under it.
    pub fn lock(dir: &Path) -> Result<(BuildGuard, DirLock)> {
        let (lock, outermost) = DirLock::take(dir, Mode::Build)?;
        let guard = BuildGuard {
            dir: lock.dir.clone(),
            registered: outermost,
            claim: Some((lock.key.clone(), thread::current().id())),
        };
        Ok((guard, lock))
    }

    /// Create `dir` and its missing ancestors durably — the parent of every
    /// directory created is `fsync`ed — and claim it as [`lock`](Self::lock)
    /// does: what a tool that claims a new database directory before it
    /// prepares its input and builds into it (`gbuild`) needs, so the
    /// directory survives a power loss like the files the build syncs into
    /// it (round-5 review follow-up of tests-b#3: `gbuild` created it with a
    /// plain `create_dir_all`).
    pub fn create_and_lock(dir: &Path) -> Result<(BuildGuard, DirLock)> {
        super::fsutil::create_dir_durable(dir)?;
        BuildGuard::lock(dir)
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
        if let Some((key, me)) = &self.claim {
            let mut reg = registry().lock().unwrap_or_else(PoisonError::into_inner);
            // The entry may be gone (its last `DirLock` dropped first) or be
            // another claim's by now: only this thread's claim count drops.
            if let Some(held) = reg.get_mut(key) {
                if let Some((t, claims)) = &mut held.builder {
                    if t == me {
                        *claims = claims.saturating_sub(1);
                        if *claims == 0 {
                            held.builder = None;
                        }
                    }
                }
            }
        }
        if self.registered {
            building()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.dir);
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

    /// A temp directory (canonical path) removed when dropped, also when the
    /// test fails.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> TmpDir {
            use std::sync::atomic::{AtomicU64, Ordering};
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "gstore_dirlock_ut_{tag}_{}_{n}",
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

    /// Unwatches the save probes when dropped (also when the test fails).
    struct Unwatch(PathBuf);

    impl Drop for Unwatch {
        fn drop(&mut self) {
            crate::db::fsutil::snapshot_probe::unwatch(&self.0);
            writer_probe::unwatch(&self.0);
        }
    }

    /// Review db#11: two saves into the same new directory. Save A is
    /// parked after it staged its first member; save B starts. Before the
    /// fix B took no lock: its snapshot recovery deleted A's staged member,
    /// B committed its own set, and A — resumed — committed its remaining
    /// members over B's, so the directory held B's dictionary with A's
    /// store and meta (unopenable). Now both hold the directory's lock and
    /// writer exclusion: B waits for A, and the directory ends as B's
    /// complete snapshot.
    #[test]
    fn two_saves_into_one_new_directory_never_interleave() {
        use crate::db::fsutil::snapshot_probe;
        use crate::db::Database;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        const HANG_GUARD: Duration = Duration::from_secs(120);
        let base = TmpDir::new("save_race");
        let target = base.0.join("new.db");
        let _unwatch = Unwatch(target.clone());
        let a = Database::build_from_str(
            "a",
            "<http://ex/a1> <http://ex/p> <http://ex/a2> .\n<http://ex/a3> <http://ex/p> <http://ex/a4> .\n",
        )
        .unwrap();
        let b = Database::build_from_str("b", "<http://ex/b1> <http://ex/q> \"b\" .\n").unwrap();

        // A parks after its first staged member until `go`.
        let (staged_tx, staged_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let parked = AtomicBool::new(false);
        snapshot_probe::watch(
            &target,
            Arc::new(move |_member| {
                if thread::current().name() == Some("saver-a")
                    && !parked.swap(true, Ordering::SeqCst)
                {
                    let _ = staged_tx.send(());
                    let _ = go_rx.lock().unwrap().recv_timeout(HANG_GUARD);
                }
            }),
        );
        // What B did while A was parked: wait for the directory, or finish.
        let (event_tx, event_rx) = mpsc::channel::<&'static str>();
        let waits = Mutex::new(event_tx.clone());
        writer_probe::watch(
            &target,
            Arc::new(move || {
                let _ = waits.lock().unwrap().send("b waits");
            }),
        );

        let ta = {
            let target = target.clone();
            thread::Builder::new()
                .name("saver-a".into())
                .spawn(move || a.save(&target))
                .unwrap()
        };
        staged_rx
            .recv_timeout(HANG_GUARD)
            .expect("save A staged a member");
        let tb = {
            let target = target.clone();
            thread::Builder::new()
                .name("saver-b".into())
                .spawn(move || {
                    let r = b.save(&target);
                    let _ = event_tx.send("b finished");
                    r
                })
                .unwrap()
        };
        let event = event_rx
            .recv_timeout(HANG_GUARD)
            .expect("save B progressed");
        go_tx.send(()).unwrap();
        let (ra, rb) = (ta.join().unwrap(), tb.join().unwrap());
        ra.expect("save A");
        rb.expect("save B");

        let saved = Database::open(&target).unwrap_or_else(|e| {
            panic!("the directory must hold one complete snapshot (B {event}): {e}")
        });
        let rows = Database::read_copy(&target).unwrap().triple_num();
        assert_eq!(rows, 1, "B saved last: its snapshot, complete");
        assert_eq!(saved.name(), "b");
        assert_eq!(event, "b waits", "B waited for A's save");
    }

    /// Review db#12: creating a database directory fsyncs the parent of
    /// every directory created, for a save and for a snapshot write on its
    /// own; a save into an existing directory syncs only that directory's
    /// parent.
    #[test]
    fn creating_a_database_directory_syncs_the_parent_of_each_new_directory() {
        use crate::db::fsutil::{SnapshotWriter, DURABLE_DIR_PARENT_SYNCS};
        use crate::db::Database;
        let take = || DURABLE_DIR_PARENT_SYNCS.with(|s| std::mem::take(&mut *s.borrow_mut()));
        let base = TmpDir::new("durable_mkdir");
        let db =
            Database::build_from_str("d", "<http://ex/s> <http://ex/p> <http://ex/o> .\n").unwrap();
        take();
        let target = base.0.join("a").join("b.db");
        db.save(&target).unwrap();
        let synced = take();
        assert!(
            synced.contains(&base.0.join("a")) && synced.contains(&base.0),
            "the parents of both new directories: {synced:?}"
        );
        // review tests-b#3 (round-5 follow-up): this asserted that a save
        // into an existing directory syncs no parent, which is the gap the
        // review found — a directory created before the save (`gbuild`'s
        // claim) never had its entry synced. A save into a directory that is
        // not the database's home now syncs that directory's parent, once.
        db.save(&target).unwrap();
        assert_eq!(
            take(),
            vec![base.0.join("a")],
            "the existing directory's own entry, nothing else"
        );

        let other = base.0.join("c.db");
        drop(SnapshotWriter::begin(&other, &["x.bin"]).unwrap());
        assert_eq!(take(), vec![base.0.clone()]);
    }
}
