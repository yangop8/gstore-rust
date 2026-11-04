//! A lazily created scratch directory for spilled runs and spools, removed
//! (with everything in it) when dropped.
//!
//! The directory is created **exclusively** under a fresh name
//! (`gstore-bulk-scratch-<pid>-<seq>-<nonce>`, see [`SCRATCH_PREFIX`]) and
//! never by taking over an existing one: builds in different PID namespaces
//! (containers) or on different hosts that share a temp directory can have
//! the same pid. It holds a `LOCK` file, exclusively locked (an advisory
//! file lock, released by the kernel when the process dies) for as long as
//! the scratch lives, so [`sweep_stale`] can remove the directories that
//! crashed or killed builds left behind — whose lock nobody holds — and
//! leaves every live build's directory alone.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Result;

/// The name prefix of every scratch directory.
pub(crate) const SCRATCH_PREFIX: &str = "gstore-bulk-scratch-";

/// The lock file inside a scratch directory.
const LOCK_FILE: &str = "LOCK";

/// A scratch directory under `parent`: created on the first
/// [`file`](Scratch::file), removed when the `Scratch` is dropped.
#[derive(Debug)]
pub(crate) struct Scratch {
    parent: PathBuf,
    /// The directory, once created, and its locked `LOCK` file (`None` on a
    /// file system without advisory locks).
    dir: Mutex<Option<(PathBuf, Option<File>)>>,
    next: AtomicU64,
}

/// Distinguishes the scratch directories of concurrent builds in one process.
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    /// A scratch directory to be created inside `parent` when first needed.
    pub(crate) fn new(parent: &Path) -> Scratch {
        Scratch {
            parent: parent.to_path_buf(),
            dir: Mutex::new(None),
            next: AtomicU64::new(0),
        }
    }

    /// The directory, if it was created.
    #[cfg(test)]
    pub(crate) fn dir(&self) -> Option<PathBuf> {
        self.dir.lock().unwrap().as_ref().map(|(d, _)| d.clone())
    }

    /// Create a new, empty scratch file named after `tag`; returns its path
    /// and a read-write handle.
    pub(crate) fn file(&self, tag: &str) -> Result<(PathBuf, File)> {
        let dir = {
            let mut guard = self.dir.lock().unwrap();
            match &*guard {
                Some((d, _)) => d.clone(),
                None => {
                    let (d, lock) = create_locked(&self.parent)?;
                    *guard = Some((d.clone(), lock));
                    d
                }
            }
        };
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{tag}-{n}.run"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok((path, file))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some((d, lock)) = self.dir.get_mut().unwrap().take() {
            let _ = fs::remove_dir_all(d);
            drop(lock);
        }
    }
}

/// Create a fresh scratch directory under `parent` — exclusively: an
/// existing directory of the same name (another build's) is never reused or
/// removed — and lock its `LOCK` file. The lock is taken on a temporary name
/// and then renamed into place, so [`sweep_stale`] never meets the `LOCK` of
/// a live directory unlocked.
fn create_locked(parent: &Path) -> Result<(PathBuf, Option<File>)> {
    fs::create_dir_all(parent)?;
    let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let stem = format!("{SCRATCH_PREFIX}{}-{seq}-{nonce:x}", std::process::id());
    let mut attempt = 0u32;
    let dir = loop {
        let d = match attempt {
            0 => parent.join(&stem),
            n => parent.join(format!("{stem}-{n}")),
        };
        match fs::create_dir(&d) {
            Ok(()) => break d,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 1000 => {
                attempt += 1;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let staged = dir.join("LOCK.new");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&staged)?;
    if file.try_lock().is_err() {
        // No advisory locks here: no `LOCK`, so no sweep ever removes it.
        let _ = fs::remove_file(&staged);
        return Ok((dir, None));
    }
    fs::rename(&staged, dir.join(LOCK_FILE))?;
    Ok((dir, Some(file)))
}

/// Remove the scratch directories under `parent` that builds no longer
/// running left behind (crashed or killed: their `LOCK` is not held by any
/// process). A live build's directory — its lock is held, by this process or
/// any other, whatever its pid — and a directory without a `LOCK` (being set
/// up, made by a build before locks were used, or on a file system without
/// advisory locks) are left alone. Best effort; returns how many were
/// removed.
pub(crate) fn sweep_stale(parent: &Path) -> usize {
    let Ok(entries) = fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let is_scratch = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(SCRATCH_PREFIX));
        if !is_scratch || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir = entry.path();
        let Ok(lock) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(LOCK_FILE))
        else {
            continue;
        };
        if lock.try_lock().is_ok() && fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_lazily_and_removed_on_drop() {
        let parent = std::env::temp_dir().join(format!("gstore_scratch_ut_{}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir_all(&parent).unwrap();
        let s = Scratch::new(&parent);
        assert!(s.dir().is_none());
        assert_eq!(
            fs::read_dir(&parent).unwrap().count(),
            0,
            "nothing created yet"
        );
        let (p1, _) = s.file("a").unwrap();
        let (p2, _) = s.file("a").unwrap();
        assert_ne!(p1, p2);
        let dir = s.dir().unwrap();
        assert!(p1.starts_with(&dir) && p1.is_file());
        drop(s);
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(&parent);
    }

    /// A scratch directory is created exclusively: an existing directory
    /// that a same-pid build elsewhere (another PID namespace) is using is
    /// never taken over or removed, and two scratches never share one.
    #[test]
    fn never_takes_over_an_existing_directory() {
        let parent =
            std::env::temp_dir().join(format!("gstore_scratch_excl_{}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir_all(&parent).unwrap();
        // What the old naming would have reused: `<prefix><pid>-<seq>`.
        let theirs = parent.join(format!(
            "{SCRATCH_PREFIX}{}-{}",
            std::process::id(),
            SCRATCH_SEQ.load(Ordering::Relaxed)
        ));
        fs::create_dir_all(&theirs).unwrap();
        fs::write(theirs.join("spo-0.run"), b"live").unwrap();
        let a = Scratch::new(&parent);
        let b = Scratch::new(&parent);
        let (pa, _) = a.file("spo").unwrap();
        let (pb, _) = b.file("spo").unwrap();
        assert_ne!(a.dir(), b.dir());
        assert!(pa != theirs.join("spo-0.run") && pb != theirs.join("spo-0.run"));
        assert_eq!(fs::read(theirs.join("spo-0.run")).unwrap(), b"live");
        drop((a, b));
        assert!(theirs.exists());
        let _ = fs::remove_dir_all(&parent);
    }

    /// The sweep removes what dead builds left (an unlocked `LOCK`) and
    /// keeps live directories (locked, in this process too) and directories
    /// without a lock file.
    #[test]
    fn sweep_removes_only_dead_builds_directories() {
        let parent =
            std::env::temp_dir().join(format!("gstore_scratch_sweep_{}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir_all(&parent).unwrap();
        let live = Scratch::new(&parent);
        let (run, _) = live.file("pos").unwrap();
        let live_dir = live.dir().unwrap();
        let locked = live_dir.join(LOCK_FILE).is_file();
        // A crashed build: its directory and LOCK, nobody holding the lock.
        let dead = parent.join(format!("{SCRATCH_PREFIX}999999-0-dead"));
        fs::create_dir_all(&dead).unwrap();
        fs::write(dead.join(LOCK_FILE), b"").unwrap();
        fs::write(dead.join("osp-3.run"), vec![0u8; 4096]).unwrap();
        // No lock file: left alone.
        let bare = parent.join(format!("{SCRATCH_PREFIX}999998-0-bare"));
        fs::create_dir_all(&bare).unwrap();
        // Not a scratch directory at all.
        let other = parent.join("unrelated");
        fs::create_dir_all(&other).unwrap();

        let removed = sweep_stale(&parent);
        if locked {
            assert_eq!(removed, 1);
            assert!(!dead.exists(), "the dead build's runs are removed");
        }
        assert!(run.is_file(), "the live build's directory is kept");
        assert!(bare.exists() && other.exists());
        drop(live);
        assert!(!live_dir.exists());
        if locked {
            // Once its owner is gone, nothing holds the old lock either.
            assert_eq!(sweep_stale(&parent), 0);
        }
        let _ = fs::remove_dir_all(&parent);
    }
}
