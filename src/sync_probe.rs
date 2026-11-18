//! A test probe of the `fsync`s behind every commit point (review tests-b#3,
//! tests-a#13).
//!
//! Every crash test kills its process (`SIGKILL`, `abort`, `mem::forget`),
//! which keeps the kernel's page cache: a write that was never `fsync`ed
//! survives such a "crash", so none of those tests can tell a commit point
//! that syncs from one that does not. This probe can. Each `fsync` /
//! `fdatasync` a commit point relies on goes through `sync_file`,
//! `sync_file_data` or `sync_dir` here, which sync and then record what
//! was synced — the path, the file's identity (device and inode), its length,
//! modification time and a digest of its content at that moment, and the
//! call site — for every live
//! [`Probe`] watching a directory the path lies in; RocksDB's write-ahead-log
//! sync reports itself through `rocks_wal_synced` with the lengths of the
//! store's log files. `tests/fsync_audit.rs` compares those records with
//! what each acknowledged commit point changed on disk: every file it wrote
//! must have been synced after its last write, and every directory whose
//! entries it changed must have been synced after the last change.
//!
//! No behaviour change: the sync happens exactly as before, and with no
//! probe alive recording costs one atomic load. Not part of the supported
//! API.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

/// What was synced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncKind {
    /// `fsync` of a regular file (`File::sync_all`).
    File,
    /// `fdatasync` of a regular file (`File::sync_data`).
    FileData,
    /// `fsync` of a directory (its entries).
    Dir,
    /// RocksDB's write-ahead log, synced (`flush_wal(true)`); the path is
    /// the store's directory.
    RocksWal,
}

/// A file's identity: device and inode number (stable across renames).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl FileId {
    /// The identity of the file `md` describes (`None` off unix).
    pub fn of(md: &fs::Metadata) -> Option<FileId> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(FileId {
                dev: md.dev(),
                ino: md.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = md;
            None
        }
    }
}

/// One recorded sync.
#[derive(Clone, Debug)]
pub struct SyncEvent {
    pub kind: SyncKind,
    /// The path the sync was made through (a staged file may have been
    /// renamed since: [`id`](Self::id) follows it).
    pub path: PathBuf,
    /// The synced file's or directory's identity, read right after the sync.
    pub id: Option<FileId>,
    /// Its length then.
    pub len: u64,
    /// Its modification time then: a later write (a directory: a later entry
    /// change) moves it on.
    pub modified: Option<SystemTime>,
    /// [`SyncKind::File`] / [`SyncKind::FileData`]: the [`content_digest`] of
    /// the file right after the sync. A later write that keeps the length
    /// and lands within the same timestamp tick (an in-place page write on
    /// a kernel without fine-grained timestamps) leaves `len` and
    /// `modified` as they were, but not this (round-5 review follow-up of
    /// tests-b#3).
    pub digest: Option<u64>,
    /// [`SyncKind::Dir`]: the directory's entries (name and identity) right
    /// after the sync — what a crash from then on keeps.
    pub entries: Vec<(OsString, Option<FileId>)>,
    /// [`SyncKind::RocksWal`]: every `*.log` file of the store and its length
    /// right after the sync.
    pub wal_files: Vec<(PathBuf, u64)>,
    /// The source location of the sync call.
    pub site: &'static Location<'static>,
    /// Process-wide order of the syncs.
    pub seq: u64,
}

impl SyncEvent {
    /// `file:line` of the sync call.
    pub fn site(&self) -> String {
        format!("{}:{}", self.site.file(), self.site.line())
    }
}

struct Watch {
    id: u64,
    root: PathBuf,
    canonical: Option<PathBuf>,
    events: Vec<SyncEvent>,
}

impl Watch {
    fn covers(&self, path: &Path, canonical: &mut Option<Option<PathBuf>>) -> bool {
        if path.starts_with(&self.root) {
            return true;
        }
        let Some(root) = &self.canonical else {
            return false;
        };
        if path.starts_with(root) {
            return true;
        }
        canonical
            .get_or_insert_with(|| canonical_lenient(path))
            .as_ref()
            .is_some_and(|p| p.starts_with(root))
    }
}

/// `path` resolved, or its parent resolved with the last component appended
/// (a file renamed away since the sync).
fn canonical_lenient(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok().or_else(|| {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
        Some(fs::canonicalize(parent).ok()?.join(path.file_name()?))
    })
}

/// How many probes are alive (recording is skipped while none is).
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static SEQ: AtomicU64 = AtomicU64::new(0);
static WATCHES: Mutex<Vec<Watch>> = Mutex::new(Vec::new());

fn watches() -> MutexGuard<'static, Vec<Watch>> {
    WATCHES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Records every sync of a path under one directory while it lives, from
/// any thread.
pub struct Probe {
    id: u64,
}

impl Probe {
    /// Start recording the syncs of paths under `root` (an existing
    /// directory, or one about to be created).
    pub fn watch(root: impl AsRef<Path>) -> Probe {
        let root = root.as_ref();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        watches().push(Watch {
            id,
            root: root.to_path_buf(),
            canonical: canonical_lenient(root),
            events: Vec::new(),
        });
        ACTIVE.fetch_add(1, Ordering::AcqRel);
        Probe { id }
    }

    /// The syncs recorded so far, in the order they were made.
    pub fn events(&self) -> Vec<SyncEvent> {
        watches()
            .iter()
            .find(|w| w.id == self.id)
            .map(|w| w.events.clone())
            .unwrap_or_default()
    }

    /// The syncs recorded so far, removing them.
    pub fn take(&self) -> Vec<SyncEvent> {
        watches()
            .iter_mut()
            .find(|w| w.id == self.id)
            .map(|w| std::mem::take(&mut w.events))
            .unwrap_or_default()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        watches().retain(|w| w.id != self.id);
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The entries of directory `dir` (sorted by name).
fn entries_of(dir: &Path) -> Vec<(OsString, Option<FileId>)> {
    let mut out: Vec<(OsString, Option<FileId>)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let id = fs::symlink_metadata(e.path())
                .ok()
                .as_ref()
                .and_then(FileId::of);
            (e.file_name(), id)
        })
        .collect();
    out.sort();
    out
}

/// A digest of the content of the file at `path` (FNV-1a, 64 bit): what
/// [`SyncEvent::digest`] records, for comparing with the file's content
/// later. `None` when it cannot be read.
pub fn content_digest(path: &Path) -> Option<u64> {
    let bytes = fs::read(path).ok()?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(h)
}

fn record(
    kind: SyncKind,
    md: Option<fs::Metadata>,
    path: &Path,
    wal_files: Vec<(PathBuf, u64)>,
    site: &'static Location<'static>,
) {
    // Only for the probes watching `path`: a file's digest reads it whole.
    let mut canonical = None;
    let watchers: Vec<u64> = watches()
        .iter()
        .filter(|w| w.covers(path, &mut canonical))
        .map(|w| w.id)
        .collect();
    if watchers.is_empty() {
        return;
    }
    let entries = if kind == SyncKind::Dir {
        entries_of(path)
    } else {
        Vec::new()
    };
    let digest = match kind {
        SyncKind::File | SyncKind::FileData => content_digest(path),
        SyncKind::Dir | SyncKind::RocksWal => None,
    };
    let event = SyncEvent {
        kind,
        path: path.to_path_buf(),
        id: md.as_ref().and_then(FileId::of),
        len: md.as_ref().map_or(0, fs::Metadata::len),
        modified: md.as_ref().and_then(|m| m.modified().ok()),
        digest,
        entries,
        wal_files,
        site,
        seq: SEQ.fetch_add(1, Ordering::Relaxed),
    };
    for w in watches().iter_mut() {
        if watchers.contains(&w.id) {
            w.events.push(event.clone());
        }
    }
}

fn recording() -> bool {
    ACTIVE.load(Ordering::Acquire) > 0
}

/// `fsync` `file` (open at `path`), then record it.
#[track_caller]
pub(crate) fn sync_file(file: &File, path: &Path) -> io::Result<()> {
    file.sync_all()?;
    if recording() {
        record(
            SyncKind::File,
            file.metadata().ok(),
            path,
            Vec::new(),
            Location::caller(),
        );
    }
    Ok(())
}

/// `fdatasync` `file` (open at `path`), then record it.
#[track_caller]
pub(crate) fn sync_file_data(file: &File, path: &Path) -> io::Result<()> {
    file.sync_data()?;
    if recording() {
        record(
            SyncKind::FileData,
            file.metadata().ok(),
            path,
            Vec::new(),
            Location::caller(),
        );
    }
    Ok(())
}

/// `fsync` directory `dir`, so renames, creations and removals of its
/// entries are durable, then record it. A no-op off unix, where directories
/// cannot be opened for syncing.
#[track_caller]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let f = File::open(dir)?;
        f.sync_all()?;
        if recording() {
            record(
                SyncKind::Dir,
                f.metadata().ok(),
                dir,
                Vec::new(),
                Location::caller(),
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Record that RocksDB's write-ahead log in store directory `dir` was just
/// synced (call after a successful `flush_wal(true)`).
#[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
#[track_caller]
pub(crate) fn rocks_wal_synced(dir: &Path) {
    if !recording() {
        return;
    }
    let mut logs: Vec<(PathBuf, u64)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.len())))
        .collect();
    logs.sort();
    record(
        SyncKind::RocksWal,
        fs::metadata(dir).ok(),
        dir,
        logs,
        Location::caller(),
    );
}
