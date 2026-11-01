//! A lazily created scratch directory for spilled runs and spools, removed
//! (with everything in it) when dropped.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::error::Result;

/// A scratch directory under `parent`: created on the first
/// [`file`](Scratch::file), removed when the `Scratch` is dropped.
#[derive(Debug)]
pub(crate) struct Scratch {
    parent: PathBuf,
    /// The directory, once created.
    dir: Mutex<Option<PathBuf>>,
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
        self.dir.lock().unwrap().clone()
    }

    /// Create a new, empty scratch file named after `tag`; returns its path
    /// and a read-write handle.
    pub(crate) fn file(&self, tag: &str) -> Result<(PathBuf, File)> {
        let dir = {
            let mut guard = self.dir.lock().unwrap();
            match &*guard {
                Some(d) => d.clone(),
                None => {
                    let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
                    let d = self
                        .parent
                        .join(format!("gstore-bulk-scratch-{}-{seq}", std::process::id()));
                    if d.exists() {
                        fs::remove_dir_all(&d)?;
                    }
                    fs::create_dir_all(&d)?;
                    *guard = Some(d.clone());
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
        if let Some(d) = self.dir.get_mut().unwrap().take() {
            let _ = fs::remove_dir_all(d);
        }
    }
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
}
