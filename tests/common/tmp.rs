//! Unique temporary paths that are removed again — also when the test fails.
//!
//! Every name is `<temp_dir>/gstore_<tag>_<pid>_<n>`: the pid keeps two test
//! processes that share `/tmp` (two checkouts, two agents, a test run next to
//! an IDE runner) apart, the process-wide counter `n` keeps the tests of one
//! process apart (review db#22, disk#13, tests-a#6, tests-b#9). A leftover of
//! an earlier run whose pid was the same is removed first: pids are reused.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Remove a directory tree or a file, ignoring "not found".
pub fn remove_any(p: &Path) {
    let _ = std::fs::remove_dir_all(p).or_else(|_| std::fs::remove_file(p));
}

/// A fresh, not yet existing path `<temp_dir>/gstore_<tag>_<pid>_<n>`. The
/// caller removes it (see [`TempDir`] and [`thread_path`] for paths that are
/// removed on their own).
pub fn unique_path(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("gstore_{tag}_{}_{n}", std::process::id()));
    remove_any(&p);
    p
}

/// A unique temporary directory, removed with everything in it when dropped
/// — also when the test panics (the guard is dropped while unwinding).
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A new, empty directory.
    pub fn new(tag: &str) -> TempDir {
        let dir = TempDir::reserve(tag);
        std::fs::create_dir_all(&dir.path)
            .unwrap_or_else(|e| panic!("create {}: {e}", dir.path.display()));
        dir
    }

    /// A unique path that does not exist yet, for an API that must create it
    /// itself (`gbuild` refuses an existing database path); removed on drop
    /// like a [`new`](Self::new) one.
    pub fn reserve(tag: &str) -> TempDir {
        TempDir {
            path: unique_path(tag),
        }
    }

    /// The path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The canonical path (the directory must exist).
    pub fn canonical(&self) -> PathBuf {
        std::fs::canonicalize(&self.path)
            .unwrap_or_else(|e| panic!("canonicalize {}: {e}", self.path.display()))
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        remove_any(&self.path);
    }
}

/// The paths the current thread registered, removed when the thread ends —
/// also when its test fails: a thread-local's destructor runs after the
/// unwinding, and libtest runs every test on a thread of its own.
struct Registry(RefCell<Vec<PathBuf>>);

impl Drop for Registry {
    fn drop(&mut self) {
        for p in self.0.borrow().iter().rev() {
            remove_any(p);
        }
    }
}

thread_local! {
    static REGISTRY: Registry = const { Registry(RefCell::new(Vec::new())) };
}

/// Remove `path` (a directory tree or a file) when the current thread ends.
pub fn remove_at_thread_exit(path: &Path) {
    REGISTRY.with(|r| r.0.borrow_mut().push(path.to_path_buf()));
}

/// A unique path that does not exist yet ([`unique_path`]), removed when
/// the current thread — the test — ends. For helpers that hand out a bare
/// `PathBuf` (a store file, a database directory a builder creates).
pub fn thread_path(tag: &str) -> PathBuf {
    let p = unique_path(tag);
    remove_at_thread_exit(&p);
    p
}

/// Like [`thread_path`], but the directory is created (empty) and the
/// canonical path returned.
pub fn thread_dir(tag: &str) -> PathBuf {
    let p = thread_path(tag);
    std::fs::create_dir_all(&p).unwrap_or_else(|e| panic!("create {}: {e}", p.display()));
    std::fs::canonicalize(&p).unwrap_or_else(|e| panic!("canonicalize {}: {e}", p.display()))
}
