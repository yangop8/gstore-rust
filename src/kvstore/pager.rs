//! A paged file with a CLOCK page cache, a free-list allocator, a write-ahead
//! log and per-operation brackets.
//!
//! Corresponds to the storage substrate beneath gStore's `KVstore`
//! (fixed-size disk blocks + buffer cache). The file is a sequence of
//! [`PAGE_SIZE`]-byte pages; page 0 is the header (its layout and every
//! version marker live in the [`format`](mod@super::format) registry). Higher
//! layers (the B+ tree, the IvArray) address data by [`PageId`] and read or
//! write whole pages through the cache.
//!
//! ## Cache
//! Cached pages are shared, immutable `Arc<[u8; PAGE_SIZE]>` buffers:
//! [`read_page_shared`](Pager::read_page_shared) hands out a reference-counted
//! handle without copying 4 KiB, and [`write_page`](Pager::write_page) copies on
//! write (a reader holding the old handle keeps the old bytes). Victims are
//! chosen by CLOCK (second chance). A dirty page never leaves the cache except
//! through a committed [`flush`](Pager::flush); on Unix a miss is a single
//! positioned read (`pread`), so readers do not serialize on a file cursor.
//!
//! ## Operation brackets (no-steal)
//! A logical write that touches several pages (a term: forward tree + reverse
//! array + counter; a triple: three indexes + statistics) runs between
//! [`begin_op`](Pager::begin_op) and [`end_op`](Pager::end_op), most easily
//! through the RAII [`PagerOp`]. While an operation is open the cache never
//! flushes to make room — it may grow past its capacity instead — so a commit
//! can never contain half an operation; `end_op` at depth 0 restores the
//! capacity (committing first if the dirty pages alone exceed it). Brackets
//! nest. An operation dropped without [`PagerOp::commit`] (an error or a panic
//! unwinding through it) is rolled back: every page it wrote returns to its
//! previous contents and the header (page count, free list, root slots) to its
//! state at `begin_op`, so a failed operation leaves no trace at all.
//!
//! ## Commit, write-ahead log and the failed state
//! A [`flush`](Pager::flush) stages the batch (header + dirty pages) in
//! `<file>.wal.tmp`, fsyncs it, renames it to `<file>.wal` and fsyncs the
//! directory — a log is never truncated or rewritten in place, so a log the
//! file still needs can only be replaced, atomically, by a complete newer
//! one. Then the batch is applied to the file, the file fsynced, the log
//! removed and the directory fsynced again.
//!
//! A failure before the rename leaves the file and the directory as they
//! were: the flush can simply be retried. Once the log has its name, it may
//! be durable, and any later error (applying a page, the fsyncs, removing the
//! log) leaves the file possibly half-applied: the pager becomes **failed**
//! ([`failure`](Pager::failure)). A failed pager refuses every write, page
//! allocation and commit with [`GStoreError::StorageFailed`] — reads still
//! work, every page of the batch is resident and dirty — and it never writes
//! the file again (it has no `Drop` flush either). Reopening replays the
//! complete log, so the file holds exactly the last batch whose log was
//! complete, never a mix. A store built on the pager may also mark it failed
//! when it finds itself out of step with it
//! ([`mark_failed`](Pager::mark_failed)).
//!
//! [`open`](Pager::open) replays a complete log and discards a torn one; a
//! log that exists but cannot be read fails the open (only "not found" means
//! "no log"), and so does a file shorter than its header's page count.
//!
//! A commit holds the pager **exclusively only to take its batch and to mark
//! it clean** ([`commit_shared`](Pager::commit_shared) /
//! [`finish_commit`](Pager::finish_commit)): checksumming and writing the log,
//! applying it and the fsyncs run under a shared guard, so readers of a
//! shared pager go on meanwhile (the batch's pages are resident and dirty,
//! never read from the file); writers wait as before. The checksum is a
//! table-driven CRC-32 and the log is streamed through a bounded buffer
//! instead of being copied whole first (review disk#10).
//!
//! ## One process per file, one log path
//! [`open`](Pager::open) takes an advisory exclusive lock on the file
//! (`flock` on Unix, `LockFileEx` on Windows) before it looks at the log, and
//! the process holds it while any of its pagers on the file lives: an open
//! from **another process** fails, touching nothing (not even the log).
//! Within one process the lock is shared, as the directory lock is
//! ([`dirlock`](crate::db::dirlock)): a second pager of the same process on
//! the same file is not refused — two pagers on one file would replay and
//! remove each other's log and overwrite each other's pages, so avoiding
//! them is the caller's job (share one pager instead: `Arc<RwLock<Pager>>`,
//! see [`DiskStore::shared_pager`](super::DiskStore::shared_pager); the
//! servers open a database once, under its directory lock). The process's
//! last pager on the file releases the lock explicitly, so a child process
//! being spawned at that moment (which briefly shares the descriptor) cannot
//! keep it. A file renamed over or unlinked while a pager has it open keeps
//! its lock; a new file at the path is another file. The log's path is
//! derived in one place,
//! [`wal_path`](Pager::wal_path), and [`remove_with_wal`](Pager::remove_with_wal)
//! removes a paged file with its log, failing on any error but "not found"
//! (review disk#9). The log records no identity of its file: the lock, the
//! single path rule and the checked removal are what keep a log from
//! meeting a file it does not belong to.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

use crate::error::{GStoreError, Result};

use super::format;
pub use super::format::NROOTS;

/// Fixed disk-block size (gStore: `STORAGE_BLOCK_SIZE = 1 << 12`).
pub const PAGE_SIZE: usize = 4096;

/// One page's bytes.
pub type Page = [u8; PAGE_SIZE];

/// Write-ahead-log framing: a committed batch is `WAL_MAGIC | n | (id,page)×n |
/// crc32(body) | COMMIT_MAGIC`. The trailing commit marker + checksum make a
/// torn or partial log recognisable: recovery replays it only when both are
/// intact, so a crash mid-commit loses the batch but never corrupts the store.
const WAL_MAGIC: &[u8; 8] = b"GSTOREWL";
const COMMIT_MAGIC: &[u8; 8] = b"GSTORECM";

/// A page index into the file. Page 0 is the header; 0 also means "null".
pub type PageId = u32;

/// The sticky failed state of a [`Pager`] (see the module docs), shareable
/// without the pager's lock: set once with the reason, never cleared.
pub type FailureFlag = Arc<OnceLock<String>>;

/// A cache frame: one resident page.
struct Frame {
    id: PageId,
    data: Arc<Page>,
    dirty: bool,
    /// CLOCK reference bit: set on every access, cleared as the hand passes.
    referenced: bool,
    /// The operation generation that last wrote this frame (see `Pager::op_gen`),
    /// so an operation records each page's previous state once.
    op_gen: u64,
}

/// A fast hasher for page ids (Fibonacci multiplication; the ids are dense
/// small integers, so SipHash's DoS resistance buys nothing here).
#[derive(Default)]
struct PageIdHasher(u64);

impl Hasher for PageIdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.0 = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// The buffer-cache state, behind a `Mutex` so cached reads (which only touch
/// the cache + the file) can run through `&self` and thus concurrently under a
/// shared [`std::sync::RwLock`] read guard at the layer above.
struct CacheState {
    map: HashMap<PageId, usize, BuildHasherDefault<PageIdHasher>>,
    frames: Vec<Frame>,
    /// CLOCK hand (an index into `frames`).
    hand: usize,
    /// Number of dirty frames.
    dirty: usize,
}

impl CacheState {
    fn new() -> CacheState {
        CacheState {
            map: HashMap::default(),
            frames: Vec::new(),
            hand: 0,
            dirty: 0,
        }
    }

    /// CLOCK: the index of a clean frame whose reference bit is clear, giving
    /// every referenced frame a second chance. `None` if every frame is dirty.
    /// Dirty frames are never victims.
    fn find_victim(&mut self) -> Option<usize> {
        let n = self.frames.len();
        if n == 0 || self.dirty >= n {
            return None;
        }
        for _ in 0..2 * n {
            if self.hand >= n {
                self.hand = 0;
            }
            let i = self.hand;
            self.hand += 1;
            let f = &mut self.frames[i];
            if f.dirty {
                continue;
            }
            if f.referenced {
                f.referenced = false;
                continue;
            }
            return Some(i);
        }
        None
    }

    /// Put `frame` into slot `i` (replacing a clean victim) or append it.
    fn place(&mut self, slot: Option<usize>, frame: Frame) {
        if frame.dirty {
            self.dirty += 1;
        }
        let id = frame.id;
        match slot {
            Some(i) => {
                let old = std::mem::replace(&mut self.frames[i], frame);
                debug_assert!(!old.dirty, "a dirty page must never be evicted");
                self.map.remove(&old.id);
                self.map.insert(id, i);
            }
            None => {
                self.frames.push(frame);
                self.map.insert(id, self.frames.len() - 1);
            }
        }
    }

    /// Drop frame `i` from the cache.
    fn remove_at(&mut self, i: usize) {
        let f = self.frames.swap_remove(i);
        if f.dirty {
            self.dirty -= 1;
        }
        self.map.remove(&f.id);
        if i < self.frames.len() {
            let moved = self.frames[i].id;
            self.map.insert(moved, i);
        }
        if self.hand >= self.frames.len() {
            self.hand = 0;
        }
    }

    /// Evict clean frames until at most `capacity` remain (as far as possible).
    fn trim(&mut self, capacity: usize) {
        while self.frames.len() > capacity {
            match self.find_victim() {
                Some(i) => self.remove_at(i),
                None => break,
            }
        }
    }

    /// Insert a clean page read from the file, evicting a clean victim if the
    /// cache is full; never commits (the `&self` read path cannot), so with no
    /// clean victim the cache overshoots until the next flush.
    fn insert_clean(&mut self, id: PageId, data: Arc<Page>, capacity: usize) {
        let slot = if self.frames.len() < capacity {
            None
        } else {
            self.find_victim()
        };
        self.place(
            slot,
            Frame {
                id,
                data,
                dirty: false,
                referenced: false,
                op_gen: 0,
            },
        );
    }
}

/// The header fields an operation may change (restored by a rollback).
#[derive(Clone)]
struct HeaderState {
    page_count: u32,
    free_head: PageId,
    roots: [u64; NROOTS],
}

/// A paged file with a write-back CLOCK cache (see the module docs).
///
/// ## Concurrency
/// `read_page`/`read_page_shared` take `&self`: the shared mutable state they
/// touch — the buffer cache — lives behind a short-held `Mutex`, and a miss
/// reads the file with a positioned read. So the layer above can hold the
/// `Pager` in an `Arc<RwLock<Pager>>` and let **many readers** resolve pages
/// concurrently (each `RwLock::read`), while mutations (`write_page`/`alloc`/
/// `free`/`flush`/operation brackets, all `&mut self`) take the exclusive
/// `RwLock::write`. Those `&mut self` methods reach the cache via `get_mut()`
/// (no locking needed — `&mut self` already proves exclusivity).
pub struct Pager {
    file: File,
    /// Serializes seek+read pairs where there is no positioned read.
    #[cfg(any(test, not(unix)))]
    seek_lock: Mutex<()>,
    page_count: u32,
    free_head: PageId,
    /// The header root slots; slot [`format::FEATURES`] holds the feature bits.
    roots: [u64; NROOTS],
    cache: Mutex<CacheState>,
    capacity: usize,
    /// Path of the write-ahead log beside the main file (`<file>.wal`).
    wal_path: PathBuf,
    /// Nesting depth of open operation brackets.
    op_depth: u32,
    /// Generation of the current (or last) outermost operation.
    op_gen: u64,
    /// Pages the open operation wrote, with their state before it: `None` =
    /// the page was clean or not cached (the file holds the old bytes), `Some`
    /// = the page was already dirty (these are its old bytes).
    op_undo: Vec<(PageId, Option<Arc<Page>>)>,
    /// The header at `begin_op`.
    op_header: Option<HeaderState>,
    /// A nested operation was aborted: the outermost one must roll back.
    op_failed: bool,
    /// Bumped by every change of a page's bytes (a write, an operation's
    /// rollback): what a reader holding a copy of a page checks before it
    /// trusts the copy ([`write_generation`](Self::write_generation)).
    write_gen: u64,
    /// Set once a commit failed after its log got its name (or a store on
    /// this pager found itself out of step with it): every write and commit
    /// is refused from then on (see the module docs).
    failed: FailureFlag,
    /// Flushes whose write-ahead log got its name (see
    /// [`wal_batches`](Self::wal_batches)); counted under a shared guard.
    wal_batches: AtomicU64,
    /// The file's entry in this process's advisory locks (see
    /// [`lock_file`]), given back when the pager is dropped.
    lock: FileKey,
    /// Serializes the commits running under a shared guard (see
    /// [`commit_shared`](Self::commit_shared)).
    commit_lock: Mutex<()>,
    /// Number of committed flushes (tests assert when commits happen).
    #[cfg(test)]
    flushes: u64,
}

impl Pager {
    /// Open (or create) a paged file at `path` with a cache of `capacity` pages.
    ///
    /// An existing file must carry a header this binary supports (see
    /// [`format::check_header`]); a new file is created as version 1. The
    /// file's advisory lock is taken first (see the module docs): while
    /// another process has the file open, the open fails and touches
    /// nothing, not even the log.
    pub fn open<P: AsRef<Path>>(path: P, capacity: usize) -> Result<Pager> {
        let path = path.as_ref();
        let wal_path = wal_path_for(path);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let lock = lock_file(&file, path)?;
        // Until the pager owns it, an early return gives the share back.
        let share = LockShare(Some(lock.clone()));
        // Crash recovery: replay a fully-committed WAL into the main file (redo),
        // discard a torn one, then clear the log — before reading the header.
        // A log that exists but cannot be read fails the open.
        recover_wal(&mut file, &wal_path)?;
        let len = file.metadata()?.len();
        let capacity = capacity.max(8);
        let mut p = Pager {
            file,
            #[cfg(any(test, not(unix)))]
            seek_lock: Mutex::new(()),
            page_count: 1,
            free_head: 0,
            roots: [0; NROOTS],
            cache: Mutex::new(CacheState::new()),
            capacity,
            wal_path,
            op_depth: 0,
            op_gen: 0,
            op_undo: Vec::new(),
            op_header: None,
            op_failed: false,
            write_gen: 0,
            failed: Arc::new(OnceLock::new()),
            wal_batches: AtomicU64::new(0),
            commit_lock: Mutex::new(()),
            lock,
            #[cfg(test)]
            flushes: 0,
        };
        share.disarm();
        if len == 0 {
            // Fresh file: page 0 is the header, data starts at page 1.
            p.write_header()?;
            p.flush()?;
        } else {
            // Existing file: read the header from page 0.
            let mut hdr = [0u8; PAGE_SIZE];
            p.file.seek(SeekFrom::Start(0))?;
            p.file.read_exact(&mut hdr)?;
            p.page_count = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
            p.free_head = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
            for (i, r) in p.roots.iter_mut().enumerate() {
                let off = 16 + i * 8;
                *r = u64::from_le_bytes(hdr[off..off + 8].try_into().unwrap());
            }
            format::check_header(&hdr[0..8], p.features(), format::SUPPORTED_FEATURES)?;
            // Every committed batch writes all pages below its page count, so
            // a shorter file is truncated or torn: refuse it rather than fail
            // (or invent zero pages) on some later read.
            let need = u64::from(p.page_count) * PAGE_SIZE as u64;
            if len < need {
                return Err(GStoreError::Database(format!(
                    "'{}' is torn or truncated: {len} bytes, but its header counts {} pages \
                     ({need} bytes)",
                    path.display(),
                    p.page_count
                )));
            }
        }
        Ok(p)
    }

    /// Why this pager refuses every write and commit, if it does: a commit
    /// failed after its write-ahead log got its name, or a store built on the
    /// pager marked it failed (see the module docs). Reopen to recover.
    pub fn failure(&self) -> Option<&str> {
        self.failed.get().map(String::as_str)
    }

    /// How many flushes since the open got as far as naming their
    /// write-ahead log — from that step on a batch may be durable (the next
    /// open replays a complete log), whether or not the flush then
    /// succeeded. A caller compares it before and after a commit to tell a
    /// failed commit that left the file as it was (no new batch) from one
    /// that may survive in part or whole: a later step of the failed flush,
    /// or a flush the cache made in the middle of the work (the dirty pages
    /// of a large operation outgrew it).
    pub fn wal_batches(&self) -> u64 {
        self.wal_batches.load(Ordering::Acquire)
    }

    /// The write-ahead log of the paged file at `path`: `<path>.wal`. The one
    /// place this rule lives (review disk#9).
    pub fn wal_path(path: &Path) -> PathBuf {
        wal_path_for(path)
    }

    /// Remove the paged file at `path` together with its write-ahead log (and
    /// a staged log), the log first, then make the removals durable: a log
    /// must never outlive its file, or it would replay into a new file
    /// created at the path. "Not found" counts as removed; every other error
    /// is returned — with the file still in place when the log could not be
    /// removed (review disk#9: callers used to ignore these errors).
    pub fn remove_with_wal(path: &Path) -> Result<()> {
        let wal = wal_path_for(path);
        let mut removed = false;
        for victim in [wal_tmp_path(&wal), wal, path.to_path_buf()] {
            match std::fs::remove_file(&victim) {
                Ok(()) => removed = true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(GStoreError::Io(std::io::Error::new(
                        e.kind(),
                        format!("cannot remove '{}': {e}", victim.display()),
                    )))
                }
            }
        }
        if removed {
            sync_parent_dir(path)?;
        }
        Ok(())
    }

    /// Read access to a pager shared behind `lock` (a
    /// [`DiskStore`](super::DiskStore), its views and its out-of-core
    /// dictionary). A lock poisoned by a panic under the write guard is
    /// recovered — the pages are consistent: an operation unwinding rolls
    /// back — and the pager is marked failed (see
    /// [`lock_write`](Self::lock_write)), so reads go on and writes are
    /// refused (review conc#8).
    pub fn lock_read(lock: &RwLock<Pager>) -> RwLockReadGuard<'_, Pager> {
        lock.read().unwrap_or_else(|poisoned| {
            let guard = poisoned.into_inner();
            guard.note_poisoned();
            guard
        })
    }

    /// Write access to a pager shared behind `lock`. On a lock poisoned by a
    /// panic under the write guard the pager is marked failed — the panic
    /// may have stopped a commit half-way — so every write and commit is
    /// refused with [`GStoreError::StorageFailed`] until the file is
    /// reopened (which replays a complete log), instead of panicking
    /// (review conc#8).
    pub fn lock_write(lock: &RwLock<Pager>) -> RwLockWriteGuard<'_, Pager> {
        lock.write().unwrap_or_else(|poisoned| {
            let guard = poisoned.into_inner();
            guard.note_poisoned();
            guard
        })
    }

    fn note_poisoned(&self) {
        self.mark_failed(
            "a panic happened while the pager was locked for writing; nothing more is \
             written until the database is reopened",
        );
    }

    /// The buffer cache (a poisoned cache lock is recovered: every critical
    /// section leaves the cache consistent).
    fn cache(&self) -> MutexGuard<'_, CacheState> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The flag behind [`failure`](Self::failure), for a store sharing this
    /// pager that checks (or sets) it without taking the pager's lock.
    pub fn failure_flag(&self) -> FailureFlag {
        Arc::clone(&self.failed)
    }

    /// Mark the pager failed (the first reason wins): from now on it refuses
    /// every write and commit, so nothing a store above it can no longer
    /// vouch for reaches the file. Reopening clears the state.
    pub fn mark_failed(&self, why: impl Into<String>) {
        let _ = self.failed.set(why.into());
    }

    /// `Err(StorageFailed)` once the pager failed.
    #[inline]
    fn check_writable(&self) -> Result<()> {
        match self.failed.get() {
            Some(why) => Err(GStoreError::StorageFailed(why.clone())),
            None => Ok(()),
        }
    }

    /// A header root slot (higher layers store tree roots etc. here).
    pub fn root(&self, i: usize) -> u64 {
        self.roots[i]
    }

    /// Set a header root slot (persisted on [`flush`](Self::flush)). The
    /// feature slot ([`format::FEATURES`]) is owned by the pager and only
    /// changes through [`enable_feature`](Self::enable_feature).
    pub fn set_root(&mut self, i: usize, v: u64) {
        if i == format::FEATURES {
            debug_assert!(false, "the feature slot is set through enable_feature");
            return;
        }
        self.roots[i] = v;
    }

    /// The feature bits of this file (0 for a version-1 file).
    pub fn features(&self) -> u64 {
        self.roots[format::FEATURES]
    }

    /// The magic the next header write carries (derived from the features).
    pub fn magic(&self) -> &'static [u8; 8] {
        format::magic_for(self.features())
    }

    /// Refuse a file using a feature this binary does not implement (`open`
    /// already checks; this re-checks an open pager, e.g. before trusting a
    /// structure behind a bit).
    pub fn require_supported(&self) -> Result<()> {
        format::check_header(self.magic(), self.features(), format::SUPPORTED_FEATURES)
    }

    /// Mark feature `bit` as in use. The next committed flush writes the bit
    /// and `MAGIC_V2` together with whatever structure needs the feature, so
    /// they reach the file atomically. Only bits this binary implements
    /// ([`format::SUPPORTED_FEATURES`]) are accepted.
    pub fn enable_feature(&mut self, bit: u64) -> Result<()> {
        self.enable_feature_with(bit, format::SUPPORTED_FEATURES)
    }

    fn enable_feature_with(&mut self, bit: u64, supported: u64) -> Result<()> {
        format::check_enable(bit, supported)?;
        self.roots[format::FEATURES] |= bit;
        Ok(())
    }

    fn header_bytes(&self) -> Page {
        let mut hdr = [0u8; PAGE_SIZE];
        hdr[0..8].copy_from_slice(self.magic());
        hdr[8..12].copy_from_slice(&self.page_count.to_le_bytes());
        hdr[12..16].copy_from_slice(&self.free_head.to_le_bytes());
        for (i, r) in self.roots.iter().enumerate() {
            let off = 16 + i * 8;
            hdr[off..off + 8].copy_from_slice(&r.to_le_bytes());
        }
        hdr
    }

    fn write_header(&mut self) -> Result<()> {
        let hdr = self.header_bytes();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&hdr)?;
        Ok(())
    }

    /// Read page `id` from the file (a cache miss).
    fn read_from_file(&self, id: PageId) -> Result<Page> {
        #[cfg(unix)]
        {
            self.read_at_pread(id)
        }
        #[cfg(not(unix))]
        {
            self.read_at_seek(id)
        }
    }

    #[cfg(unix)]
    fn read_at_pread(&self, id: PageId) -> Result<Page> {
        use std::os::unix::fs::FileExt;
        let mut buf = [0u8; PAGE_SIZE];
        self.file
            .read_exact_at(&mut buf, id as u64 * PAGE_SIZE as u64)?;
        Ok(buf)
    }

    #[cfg(any(test, not(unix)))]
    fn read_at_seek(&self, id: PageId) -> Result<Page> {
        let _cursor = self.seek_lock.lock().unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        let mut f = &self.file;
        f.seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Read a page as a shared, immutable handle (from the cache, else from
    /// disk into the cache) — no 4 KiB copy on a hit. Takes `&self`, so many
    /// readers can run this concurrently (see the type-level concurrency
    /// note). A later [`write_page`](Self::write_page) of the same page does
    /// not change the bytes behind a handle already returned.
    pub fn read_page_shared(&self, id: PageId) -> Result<Arc<Page>> {
        {
            let mut cache = self.cache();
            if let Some(&i) = cache.map.get(&id) {
                let f = &mut cache.frames[i];
                f.referenced = true;
                return Ok(Arc::clone(&f.data));
            }
        }
        // Miss: read from the file without holding the cache lock.
        let data = Arc::new(self.read_from_file(id)?);
        let mut cache = self.cache();
        if let Some(&i) = cache.map.get(&id) {
            // A concurrent reader inserted it meanwhile (same bytes).
            return Ok(Arc::clone(&cache.frames[i].data));
        }
        cache.insert_clean(id, Arc::clone(&data), self.capacity);
        Ok(data)
    }

    /// Read a page into an owned buffer (a copy of
    /// [`read_page_shared`](Self::read_page_shared)).
    pub fn read_page(&self, id: PageId) -> Result<Page> {
        Ok(*self.read_page_shared(id)?)
    }

    /// Write a page (into the cache, marked dirty; persisted on flush). Copy on
    /// write: readers holding the previous handle keep the previous bytes.
    pub fn write_page(&mut self, id: PageId, data: &Page) -> Result<()> {
        if id == 0 {
            return Err(GStoreError::Database(
                "page 0 is reserved for header".into(),
            ));
        }
        self.check_writable()?;
        self.write_gen += 1;
        let gen = (self.op_depth > 0).then_some(self.op_gen);
        // `&mut self` ⇒ exclusive access; reach the cache without locking.
        let cache = self.cache.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(&i) = cache.map.get(&id) {
            let f = &mut cache.frames[i];
            if let Some(g) = gen {
                if f.op_gen != g {
                    f.op_gen = g;
                    let before = f.dirty.then(|| Arc::clone(&f.data));
                    self.op_undo.push((id, before));
                }
            }
            match Arc::get_mut(&mut f.data) {
                Some(page) => *page = *data,
                None => f.data = Arc::new(*data),
            }
            f.referenced = true;
            if !f.dirty {
                f.dirty = true;
                cache.dirty += 1;
            }
            return Ok(());
        }
        if gen.is_some() {
            self.op_undo.push((id, None));
        }
        self.insert_dirty(id, Arc::new(*data), gen.unwrap_or(0))
    }

    /// Insert a newly dirtied page, making room per the module docs: evict a
    /// clean victim; with none, overshoot inside an operation, else commit
    /// first (then every page is clean).
    fn insert_dirty(&mut self, id: PageId, data: Arc<Page>, op_gen: u64) -> Result<()> {
        loop {
            let cache = self.cache.get_mut().unwrap_or_else(PoisonError::into_inner);
            let slot = if cache.frames.len() < self.capacity {
                Some(None)
            } else {
                cache.find_victim().map(Some)
            };
            match slot {
                Some(slot) => {
                    cache.place(
                        slot,
                        Frame {
                            id,
                            data,
                            dirty: true,
                            referenced: true,
                            op_gen,
                        },
                    );
                    return Ok(());
                }
                None if self.op_depth > 0 => {
                    cache.place(
                        None,
                        Frame {
                            id,
                            data,
                            dirty: true,
                            referenced: true,
                            op_gen,
                        },
                    );
                    return Ok(());
                }
                // Every cached page is dirty and no operation is open: commit.
                None => self.flush()?,
            }
        }
    }

    /// Allocate a fresh page (reusing the free list when possible).
    pub fn alloc(&mut self) -> Result<PageId> {
        self.check_writable()?;
        if self.free_head != 0 {
            let id = self.free_head;
            // The first 4 bytes of a free page hold the next free page id.
            let page = self.read_page_shared(id)?;
            self.free_head = u32::from_le_bytes(page[0..4].try_into().unwrap());
            // Zero the reused page.
            self.write_page(id, &[0u8; PAGE_SIZE])?;
            Ok(id)
        } else {
            let id = self.page_count;
            self.page_count = self
                .page_count
                .checked_add(1)
                .ok_or_else(|| GStoreError::Database("page count overflow".into()))?;
            self.write_page(id, &[0u8; PAGE_SIZE])?;
            Ok(id)
        }
    }

    /// Return a page to the free list.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        if id == 0 {
            return Err(GStoreError::Database(
                "page 0 is reserved for header".into(),
            ));
        }
        self.check_writable()?;
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(&self.free_head.to_le_bytes());
        self.write_page(id, &page)?;
        self.free_head = id;
        Ok(())
    }

    /// Commit: durably persist the header and every dirty page as one atomic
    /// batch. The batch is first written (with a checksum + commit marker) to
    /// the write-ahead log — staged under a temporary name, fsynced, renamed
    /// into place, the directory fsynced — then applied to the main file and
    /// fsynced, then the log is removed (and the directory fsynced). A crash
    /// at any point leaves either the old state (log torn/absent) or the new
    /// state (log replayed on next open) — never a partial mix *of this page
    /// batch*. Refused while an operation bracket is open, so a batch never
    /// holds part of an operation.
    ///
    /// An error before the log got its name changes nothing on disk (the
    /// pages stay dirty; the flush may be retried). An error after it leaves
    /// the pager **failed** and returns [`GStoreError::StorageFailed`]: the
    /// log holds the batch, the next open replays it, and until then nothing
    /// is written (see the module docs).
    pub fn flush(&mut self) -> Result<()> {
        let staged = self.commit_shared()?;
        self.finish_commit(staged)
    }

    /// The durable steps of a [`flush`](Self::flush) through `&self`, so a
    /// store sharing the pager runs them under a **shared** guard and its
    /// readers go on meanwhile (review disk#10): the batch (the header and
    /// every dirty page, as shared handles) is taken, then checksummed and
    /// written to the log, applied to the file and fsynced as `flush`
    /// describes — every page of the batch is resident and dirty, so no
    /// reader reads it from the file in between. A writer cannot run while
    /// the guard is held. The batch's pages stay dirty until
    /// [`finish_commit`](Self::finish_commit), under the exclusive guard,
    /// marks them clean. Concurrent calls are serialized.
    ///
    /// `Err` (nothing done) for a failed pager or an open operation; the
    /// outcome of the steps themselves comes back from `finish_commit`.
    pub fn commit_shared(&self) -> Result<StagedCommit> {
        self.check_writable()?;
        if self.op_depth > 0 {
            return Err(GStoreError::Database(
                "cannot commit the page cache inside an open operation".into(),
            ));
        }
        let _serial = self.commit_lock.lock().unwrap_or_else(PoisonError::into_inner);
        // The committed batch: the header (page 0) plus all dirty pages.
        let header = Arc::new(self.header_bytes());
        let mut batch: Vec<(PageId, Arc<Page>)> = vec![(0, header)];
        batch.extend(
            self.cache()
                .frames
                .iter()
                .filter(|f| f.dirty)
                .map(|f| (f.id, Arc::clone(&f.data))),
        );

        // 1. Write-ahead: stage the log under a temporary name and fsync it,
        //    then give it its name. Nothing on disk changed if this fails.
        if let Err(e) = write_wal(&self.wal_path, &batch) {
            return Ok(StagedCommit {
                batch,
                outcome: Err(e),
            });
        }
        self.wal_batches.fetch_add(1, Ordering::AcqRel);
        // From here the log may be durable: an error leaves the file possibly
        // half-applied, which only a replay of the log can finish.
        let outcome = self.apply_committed(&batch).map_err(|e| {
            let why = format!(
                "a committed page batch could not be applied to '{}' ({e}); it stays in the \
                 write-ahead log '{}', which the next open replays",
                self.wal_path.with_extension("").display(),
                self.wal_path.display()
            );
            self.mark_failed(why.clone());
            GStoreError::StorageFailed(self.failure().map_or(why, str::to_string))
        });
        Ok(StagedCommit { batch, outcome })
    }

    /// End a commit begun by [`commit_shared`](Self::commit_shared) (under
    /// the exclusive guard): its error, if a step failed — the pages stay
    /// dirty, and a failure after the log got its name left the pager failed
    /// — else every page of the batch is marked clean unless a writer
    /// changed it since (it then stays dirty for the next commit), and the
    /// cache returns to its capacity.
    pub fn finish_commit(&mut self, staged: StagedCommit) -> Result<()> {
        let StagedCommit { batch, outcome } = staged;
        outcome?;
        let capacity = self.capacity;
        let cache = self.cache.get_mut().unwrap_or_else(PoisonError::into_inner);
        for (id, data) in &batch[1..] {
            let Some(&i) = cache.map.get(id) else {
                continue;
            };
            let f = &mut cache.frames[i];
            if f.dirty && Arc::ptr_eq(&f.data, data) {
                f.dirty = false;
                cache.dirty -= 1;
            }
        }
        drop(batch);
        cache.trim(capacity);
        #[cfg(test)]
        {
            self.flushes += 1;
        }
        Ok(())
    }

    /// Steps 2 and 3 of a flush, once the batch's log has its name: make the
    /// name durable, apply the batch to the main file (positioned writes, so
    /// a shared guard suffices) and fsync it, remove the log and make the
    /// removal durable.
    fn apply_committed(&self, batch: &[(PageId, Arc<Page>)]) -> Result<()> {
        sync_parent_dir(&self.wal_path)?;
        for (i, (id, data)) in batch.iter().enumerate() {
            #[cfg(test)]
            failpoints::apply(i)?;
            #[cfg(not(test))]
            let _ = i;
            self.write_page_at(*id, data)?;
        }
        self.file.sync_all()?;
        remove_wal(&self.wal_path)?;
        sync_parent_dir(&self.wal_path)
    }

    /// Write page `id` of the file at its offset, through `&self`.
    fn write_page_at(&self, id: PageId, data: &Page) -> std::io::Result<()> {
        let offset = id as u64 * PAGE_SIZE as u64;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.write_all_at(&data[..], offset)
        }
        #[cfg(not(unix))]
        {
            let _cursor = self.seek_lock.lock().unwrap_or_else(PoisonError::into_inner);
            let mut f = &self.file;
            f.seek(SeekFrom::Start(offset))?;
            f.write_all(&data[..])
        }
    }

    /// Number of allocated pages (including the header).
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// The cache capacity in pages (the cache may exceed it inside an
    /// operation, see the module docs).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of dirty (uncommitted) pages in the cache.
    pub fn dirty_pages(&self) -> usize {
        self.cache().dirty
    }

    /// Number of pages currently cached.
    pub fn cached_pages(&self) -> usize {
        self.cache().frames.len()
    }

    // ---- operation brackets -------------------------------------------------

    /// Open an operation bracket (they nest). Prefer [`PagerOp::begin`], which
    /// ends the bracket on every path.
    pub fn begin_op(&mut self) {
        if self.op_depth == 0 {
            self.op_gen += 1;
            self.op_undo.clear();
            self.op_failed = false;
            self.op_header = Some(HeaderState {
                page_count: self.page_count,
                free_head: self.free_head,
                roots: self.roots,
            });
        }
        self.op_depth += 1;
    }

    /// Close an operation bracket. At depth 0 the operation is complete: the
    /// cache returns to its capacity (committing first if the dirty pages
    /// alone exceed it). If a nested bracket was aborted, the whole operation
    /// is rolled back instead and an error returned.
    pub fn end_op(&mut self) -> Result<()> {
        if self.op_depth == 0 {
            return Err(GStoreError::Database(
                "end_op without a matching begin_op".into(),
            ));
        }
        self.op_depth -= 1;
        if self.op_depth > 0 {
            return Ok(());
        }
        if self.op_failed {
            self.rollback_op();
            return Err(GStoreError::Database(
                "a nested page operation failed; the whole operation was rolled back".into(),
            ));
        }
        self.op_undo.clear();
        self.op_header = None;
        self.restore_capacity()
    }

    /// Close an operation bracket as failed. At depth 0 the operation is rolled
    /// back (see the module docs); a nested abort makes the outermost
    /// [`end_op`](Self::end_op) roll back.
    pub fn abort_op(&mut self) {
        if self.op_depth == 0 {
            return;
        }
        self.op_depth -= 1;
        if self.op_depth > 0 {
            self.op_failed = true;
        } else {
            self.rollback_op();
        }
    }

    /// Current nesting depth of operation brackets.
    pub fn op_depth(&self) -> u32 {
        self.op_depth
    }

    /// Undo every page write and header change of the current operation.
    fn rollback_op(&mut self) {
        self.write_gen += 1;
        let undo = std::mem::take(&mut self.op_undo);
        let cache = self.cache.get_mut().unwrap_or_else(PoisonError::into_inner);
        for (id, before) in undo.into_iter().rev() {
            let Some(&i) = cache.map.get(&id) else {
                continue;
            };
            match before {
                None => cache.remove_at(i),
                Some(data) => {
                    let f = &mut cache.frames[i];
                    f.data = data;
                    if !f.dirty {
                        f.dirty = true;
                        cache.dirty += 1;
                    }
                }
            }
        }
        cache.trim(self.capacity);
        if let Some(h) = self.op_header.take() {
            self.page_count = h.page_count;
            self.free_head = h.free_head;
            self.roots = h.roots;
        }
        self.op_failed = false;
    }

    /// Bring the cache back to its capacity after an operation that grew it.
    fn restore_capacity(&mut self) -> Result<()> {
        let cache = self.cache.get_mut().unwrap_or_else(PoisonError::into_inner);
        if cache.frames.len() <= self.capacity {
            return Ok(());
        }
        cache.trim(self.capacity);
        if cache.frames.len() > self.capacity {
            // The dirty pages alone exceed the capacity: commit them (the flush
            // then trims the now-clean cache).
            self.flush()?;
        }
        Ok(())
    }

    /// A counter that moves whenever the bytes of any page change (every
    /// [`write_page`](Self::write_page), and an operation's rollback): a
    /// copy of a page (an `Arc` from [`read_page_shared`](Self::read_page_shared))
    /// taken at one value is current while the value is unchanged — whoever
    /// wrote through this pager (every store and dictionary sharing it).
    pub fn write_generation(&self) -> u64 {
        self.write_gen
    }

    /// Committed flushes since open (tests).
    #[cfg(test)]
    pub(crate) fn flush_count(&self) -> u64 {
        self.flushes
    }
}

/// A commit whose durable steps ran under a shared guard
/// ([`Pager::commit_shared`]); [`Pager::finish_commit`] ends it.
#[must_use = "a staged commit's pages stay dirty until finish_commit"]
pub struct StagedCommit {
    /// The batch written (page 0 is the header).
    batch: Vec<(PageId, Arc<Page>)>,
    /// The outcome of the log write, the apply and the fsyncs.
    outcome: Result<()>,
}

/// An open operation bracket on a [`Pager`] (see the module docs). Derefs to
/// the pager; [`commit`](Self::commit) ends the bracket, dropping it without
/// `commit` (an early `?` return or a panic) rolls the operation back.
pub struct PagerOp<'a> {
    pager: &'a mut Pager,
    done: bool,
}

impl<'a> PagerOp<'a> {
    /// Open a bracket on `pager`.
    pub fn begin(pager: &'a mut Pager) -> PagerOp<'a> {
        pager.begin_op();
        PagerOp { pager, done: false }
    }

    /// End the bracket (see [`Pager::end_op`]).
    pub fn commit(mut self) -> Result<()> {
        self.done = true;
        self.pager.end_op()
    }
}

impl Deref for PagerOp<'_> {
    type Target = Pager;
    fn deref(&self) -> &Pager {
        self.pager
    }
}

impl DerefMut for PagerOp<'_> {
    fn deref_mut(&mut self) -> &mut Pager {
        self.pager
    }
}

impl Drop for PagerOp<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.pager.abort_op();
        }
    }
}

/// The identity of a paged file in this process's lock registry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FileKey(#[cfg(unix)] (u64, u64), #[cfg(not(unix))] PathBuf);

/// One locked paged file of this process: the descriptor holding the
/// advisory lock (a duplicate of the first pager's) and how many pagers
/// share it.
struct HeldFile {
    lock: File,
    locked: bool,
    pagers: usize,
}

fn held_files() -> MutexGuard<'static, HashMap<FileKey, HeldFile>> {
    static HELD: OnceLock<Mutex<HashMap<FileKey, HeldFile>>> = OnceLock::new();
    HELD.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Take — or, when a pager of this process holds it already, share — the
/// advisory exclusive lock of the paged file `file` at `path` (see the
/// module docs). Another process holding it fails the open; a file system
/// without locks opens unlocked.
fn lock_file(file: &File, path: &Path) -> Result<FileKey> {
    #[cfg(unix)]
    let key = {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        FileKey((m.dev(), m.ino()))
    };
    #[cfg(not(unix))]
    let key = FileKey(std::fs::canonicalize(path)?);
    let mut held = held_files();
    if let Some(h) = held.get_mut(&key) {
        h.pagers += 1;
        return Ok(key);
    }
    let lock = file.try_clone()?;
    let locked = match lock.try_lock() {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(GStoreError::Database(format!(
                "'{}' is in use: another process has it open",
                path.display()
            )))
        }
        Err(std::fs::TryLockError::Error(e)) if e.kind() == std::io::ErrorKind::Unsupported => {
            false
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(GStoreError::Io(std::io::Error::new(
                e.kind(),
                format!("cannot lock '{}': {e}", path.display()),
            )))
        }
    };
    held.insert(
        key.clone(),
        HeldFile {
            lock,
            locked,
            pagers: 1,
        },
    );
    Ok(key)
}

/// Give back a pager's share of a file's lock; the last share unlocks it
/// explicitly before the descriptor is closed — a child process being
/// spawned meanwhile shares the descriptor until its `exec`, and would
/// otherwise keep the lock alive that long.
fn release_file(key: &FileKey) {
    let mut held = held_files();
    let last = match held.get_mut(key) {
        Some(h) => {
            h.pagers -= 1;
            h.pagers == 0
        }
        None => false,
    };
    if last {
        if let Some(h) = held.remove(key) {
            if h.locked {
                let _ = h.lock.unlock();
            }
        }
    }
}

/// A lock share that is given back on drop unless disarmed (the error
/// paths of [`Pager::open`] before the pager owns it).
struct LockShare(Option<FileKey>);

impl LockShare {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for LockShare {
    fn drop(&mut self) {
        if let Some(key) = self.0.take() {
            release_file(&key);
        }
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        release_file(&self.lock);
    }
}

/// How many pagers of this process share the lock of the file at `path`
/// (tests).
#[cfg(all(test, unix))]
pub(crate) fn lock_shares(path: &Path) -> usize {
    use std::os::unix::fs::MetadataExt;
    let Ok(m) = std::fs::metadata(path) else {
        return 0;
    };
    held_files()
        .get(&FileKey((m.dev(), m.ino())))
        .map_or(0, |h| h.pagers)
}

/// The WAL path beside a database file: `<path>.wal` (public as
/// [`Pager::wal_path`]; the only derivation of the rule).
fn wal_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".wal");
    PathBuf::from(s)
}

/// The staging name of a WAL (`<file>.wal.tmp`): a log is written here and
/// renamed into place, never rewritten in place. A staged log is not a commit
/// (open removes a leftover one).
fn wal_tmp_path(wal: &Path) -> PathBuf {
    let mut s = wal.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// `fsync` the directory holding `path`, so a rename or removal inside it is
/// durable. A no-op off unix, where directories cannot be opened for syncing.
fn sync_parent_dir(path: &Path) -> Result<()> {
    #[cfg(test)]
    failpoints::count_dir_sync();
    #[cfg(unix)]
    {
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Remove a WAL; "not found" counts as removed.
fn remove_wal(path: &Path) -> Result<()> {
    #[cfg(test)]
    failpoints::wal_delete()?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Read a WAL (`NotFound` when there is none).
fn read_wal(path: &Path) -> std::io::Result<Vec<u8>> {
    #[cfg(test)]
    failpoints::wal_read(path)?;
    std::fs::read(path)
}

/// The buffer a log is streamed through (the batch is never copied whole).
const WAL_BUFFER: usize = 1 << 20;

/// The staged log file; under test it carries the "fail after `n` bytes"
/// failpoint ([`failpoints::fail_wal_write_after`]).
struct WalFile {
    file: File,
    #[cfg(test)]
    budget: Option<usize>,
}

impl Write for WalFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        #[cfg(test)]
        if let Some(left) = self.budget {
            if left == 0 {
                self.file.sync_all()?;
                return Err(failpoints::wal_write_failed());
            }
            let n = self.file.write(&buf[..buf.len().min(left)])?;
            self.budget = Some(left - n);
            return Ok(n);
        }
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Serialize a committed batch and give it the WAL's name durably-ready:
/// written and fsynced under [`wal_tmp_path`], then renamed over `path` (an
/// existing log is replaced atomically, never truncated). The caller fsyncs
/// the directory before it touches the main file. On an error the staged
/// file is removed and `path` is as it was. The record is streamed through
/// a bounded buffer with its checksum computed on the way (review disk#10:
/// the whole batch used to be copied into one buffer first).
fn write_wal<P: Deref<Target = Page>>(path: &Path, batch: &[(PageId, P)]) -> Result<()> {
    let tmp = wal_tmp_path(path);
    let staged = (|| -> Result<()> {
        // The staging file is never a log anyone replays, so truncating a
        // leftover one is harmless.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        #[cfg(test)]
        failpoints::park_in_wal_write();
        let mut w = BufWriter::with_capacity(
            WAL_BUFFER,
            WalFile {
                file,
                #[cfg(test)]
                budget: failpoints::wal_write_budget(),
            },
        );
        let mut crc = Crc32::new();
        let mut body = |w: &mut BufWriter<WalFile>, bytes: &[u8]| -> std::io::Result<()> {
            crc.update(bytes);
            w.write_all(bytes)
        };
        w.write_all(WAL_MAGIC)?;
        // The checksum covers the count and the entries (everything after
        // the magic).
        body(&mut w, &(batch.len() as u32).to_le_bytes())?;
        for (id, data) in batch {
            body(&mut w, &id.to_le_bytes())?;
            body(&mut w, &data[..])?;
        }
        w.write_all(&crc.finish().to_le_bytes())?;
        w.write_all(COMMIT_MAGIC)?;
        let file = w.into_inner().map_err(|e| e.into_error())?.file;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if staged.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    staged
}

/// Replay a committed WAL into the main file, then remove the log (durably).
/// A torn or checksum-mismatched log is discarded (the main file is already
/// the last consistent state). Only a log that does not exist means "nothing
/// to recover": one that exists but cannot be read may hold a batch the file
/// needs, so the error is returned (and the open fails) rather than opening a
/// file that may be half-applied. A leftover staged log is not a commit and
/// is removed.
fn recover_wal(file: &mut File, path: &Path) -> Result<()> {
    let _ = std::fs::remove_file(wal_tmp_path(path));
    let data = match read_wal(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(GStoreError::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "cannot read the write-ahead log '{}' ({e}); it may hold a committed batch \
                     the file still needs, so the file is not opened without it",
                    path.display()
                ),
            )))
        }
    };
    if let Some(batch) = parse_wal(&data) {
        for (id, page) in batch {
            file.seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
            file.write_all(&page)?;
        }
        file.sync_all()?;
    }
    remove_wal(path)?;
    sync_parent_dir(path)
}

/// Parse a WAL buffer, returning the batch only if it is a complete, intact,
/// committed record (magic + length + checksum + commit marker all valid).
fn parse_wal(data: &[u8]) -> Option<Vec<(PageId, [u8; PAGE_SIZE])>> {
    let entry = 4 + PAGE_SIZE;
    if data.len() < 12 || &data[0..8] != WAL_MAGIC {
        return None;
    }
    let n = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
    let body_end = 12usize.checked_add(n.checked_mul(entry)?)?;
    let total = body_end.checked_add(12)?; // crc (4) + commit magic (8)
    if data.len() != total {
        return None;
    }
    let crc_stored = u32::from_le_bytes(data[body_end..body_end + 4].try_into().ok()?);
    if &data[body_end + 4..body_end + 12] != COMMIT_MAGIC {
        return None;
    }
    if crc32(&data[8..body_end]) != crc_stored {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let off = 12 + k * entry;
        let id = u32::from_le_bytes(data[off..off + 4].try_into().ok()?);
        let mut page = [0u8; PAGE_SIZE];
        page.copy_from_slice(&data[off + 4..off + 4 + PAGE_SIZE]);
        out.push((id, page));
    }
    Some(out)
}

/// The slicing-by-8 tables of the CRC-32 (IEEE 802.3, reflected polynomial
/// `0xEDB88320`): `T[0]` is the classic byte table, `T[k][b]` the CRC of
/// byte `b` followed by `k` zero bytes.
const CRC_TABLES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut b = 0;
    while b < 256 {
        let mut crc = b as u32;
        let mut k = 0;
        while k < 8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
            k += 1;
        }
        t[0][b] = crc;
        b += 1;
    }
    let mut b = 0;
    while b < 256 {
        let mut k = 1;
        while k < 8 {
            let prev = t[k - 1][b];
            t[k][b] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            k += 1;
        }
        b += 1;
    }
    t
};

/// A running CRC-32 (IEEE 802.3, reflected) — a dependency-free integrity
/// check for WAL records. Not cryptographic; only detects accidental
/// corruption / torn writes. Table-driven, eight bytes per step (review
/// disk#10: the bit-at-a-time loop took ~90 ms per 16 MiB batch).
struct Crc32(u32);

impl Crc32 {
    fn new() -> Crc32 {
        Crc32(0xFFFF_FFFF)
    }

    fn update(&mut self, data: &[u8]) {
        let t = &CRC_TABLES;
        let mut crc = self.0;
        let mut chunks = data.chunks_exact(8);
        for c in &mut chunks {
            let lo = crc ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            crc = t[7][(lo & 0xFF) as usize]
                ^ t[6][((lo >> 8) & 0xFF) as usize]
                ^ t[5][((lo >> 16) & 0xFF) as usize]
                ^ t[4][(lo >> 24) as usize]
                ^ t[3][c[4] as usize]
                ^ t[2][c[5] as usize]
                ^ t[1][c[6] as usize]
                ^ t[0][c[7] as usize];
        }
        for &b in chunks.remainder() {
            crc = (crc >> 8) ^ t[0][((crc ^ u32::from(b)) & 0xFF) as usize];
        }
        self.0 = crc;
    }

    fn finish(&self) -> u32 {
        !self.0
    }
}

/// The CRC-32 of `data` (see [`Crc32`]).
fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

/// Test-only failpoints of the commit path and of recovery, standing in for
/// `ENOSPC` / `EIO` at exact steps. Per thread (a test arms them on the
/// thread that drives the pager) and cleared when the [`Armed`] guard is
/// dropped, also when the test fails.
#[cfg(test)]
pub(crate) mod failpoints {
    use std::cell::Cell;
    use std::io;
    use std::path::Path;

    thread_local! {
        static APPLY_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
        static WAL_WRITE_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
        static WAL_DELETE: Cell<bool> = const { Cell::new(false) };
        static WAL_UNREADABLE: Cell<bool> = const { Cell::new(false) };
        static DIR_SYNCS: Cell<u64> = const { Cell::new(0) };
        static PARK_WAL_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    fn injected(what: &str) -> io::Error {
        io::Error::other(format!(
            "injected failure: {what} (No space left on device)"
        ))
    }

    /// Clears every failpoint of this thread when dropped.
    #[must_use]
    pub(crate) struct Armed;

    impl Drop for Armed {
        fn drop(&mut self) {
            APPLY_AFTER.with(|c| c.set(None));
            WAL_WRITE_AFTER.with(|c| c.set(None));
            WAL_DELETE.with(|c| c.set(false));
            WAL_UNREADABLE.with(|c| c.set(false));
            PARK_WAL_WRITE.with(|c| c.borrow_mut().take());
        }
    }

    /// Applying a batch writes its first `k` pages (the header is page 0 of
    /// the batch), then fails.
    pub(crate) fn fail_apply_after(k: usize) -> Armed {
        APPLY_AFTER.with(|c| c.set(Some(k)));
        Armed
    }

    /// Staging a WAL writes its first `n` bytes, then fails.
    pub(crate) fn fail_wal_write_after(n: usize) -> Armed {
        WAL_WRITE_AFTER.with(|c| c.set(Some(n)));
        Armed
    }

    /// Removing a WAL fails.
    pub(crate) fn fail_wal_delete() -> Armed {
        WAL_DELETE.with(|c| c.set(true));
        Armed
    }

    /// Reading an existing WAL fails with `PermissionDenied`.
    pub(crate) fn wal_unreadable() -> Armed {
        WAL_UNREADABLE.with(|c| c.set(true));
        Armed
    }

    /// Directory fsyncs this thread made so far.
    pub(crate) fn dir_syncs() -> u64 {
        DIR_SYNCS.with(Cell::get)
    }

    pub(super) fn count_dir_sync() {
        DIR_SYNCS.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn apply(i: usize) -> io::Result<()> {
        match APPLY_AFTER.with(Cell::get) {
            Some(k) if i >= k => Err(injected("applying a page")),
            _ => Ok(()),
        }
    }

    /// The bytes a staged log may take before its write fails, if armed.
    pub(super) fn wal_write_budget() -> Option<usize> {
        WAL_WRITE_AFTER.with(Cell::get)
    }

    pub(super) fn wal_write_failed() -> io::Error {
        injected("writing the write-ahead log")
    }

    /// Park this thread's next log write (after its staging file is
    /// created) in `hook`, which runs once: a test holds a commit there
    /// while it checks what other threads can do meanwhile.
    pub(crate) fn park_next_wal_write(hook: impl FnOnce() + 'static) -> Armed {
        PARK_WAL_WRITE.with(|c| *c.borrow_mut() = Some(Box::new(hook)));
        Armed
    }

    pub(super) fn park_in_wal_write() {
        let hook = PARK_WAL_WRITE.with(|c| c.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(super) fn wal_delete() -> io::Result<()> {
        if WAL_DELETE.with(Cell::get) {
            return Err(injected("removing the write-ahead log"));
        }
        Ok(())
    }

    pub(super) fn wal_read(path: &Path) -> io::Result<()> {
        if WAL_UNREADABLE.with(Cell::get) && path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected failure: Permission denied",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gstore_pager_{tag}_{}.kv", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(wal_path_for(&p));
        p
    }

    /// The bit-at-a-time CRC-32 the log used before (review disk#10), as
    /// the reference for the table-driven one.
    fn crc32_bitwise(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
            }
        }
        !crc
    }

    #[test]
    fn the_table_driven_crc_equals_the_bitwise_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926, "the IEEE check value");
        assert_eq!(crc32(b""), 0);
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let data: Vec<u8> = (0..3 * PAGE_SIZE + 13)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        for len in [0, 1, 7, 8, 9, 63, 4096, 4100, data.len()] {
            assert_eq!(crc32(&data[..len]), crc32_bitwise(&data[..len]), "{len} bytes");
        }
        // Fed in pieces of any size, as the log writer feeds it.
        let mut c = Crc32::new();
        for piece in data.chunks(1000) {
            c.update(piece);
        }
        assert_eq!(c.finish(), crc32_bitwise(&data));
    }

    /// Review disk#9: the advisory lock is shared by the pagers of this
    /// process on one file and given back by the last of them (another
    /// process is refused meanwhile: `tests/storage_kernel_regressions.rs`).
    #[cfg(unix)]
    #[test]
    fn pagers_of_one_process_share_the_file_lock() {
        let path = tmp("lock_share");
        let _files = Files(path.clone());
        let a = Pager::open(&path, 16).unwrap();
        assert_eq!(lock_shares(&path), 1);
        let b = Pager::open(&path, 16).unwrap();
        assert_eq!(lock_shares(&path), 2);
        drop(a);
        assert_eq!(lock_shares(&path), 1, "the lock stays while a pager lives");
        drop(b);
        assert_eq!(lock_shares(&path), 0, "given back by the last pager");
        // An open that fails after taking its share gives it back.
        std::fs::write(&path, [0u8; 100]).unwrap();
        assert!(Pager::open(&path, 16).is_err(), "not a pager file");
        assert_eq!(lock_shares(&path), 0);
    }

    /// Review disk#9: `remove_with_wal` removes a file with its logs,
    /// "not found" is not an error, any other error is — and the file stays
    /// when its log could not be removed.
    #[test]
    fn remove_with_wal_removes_the_log_first_and_reports_errors() {
        let path = tmp("remove_with_wal");
        let _files = Files(path.clone());
        drop(Pager::open(&path, 8).unwrap());
        let wal = Pager::wal_path(&path);
        assert_eq!(wal, wal_path_for(&path));
        std::fs::write(&wal, b"log").unwrap();
        std::fs::write(wal_tmp_path(&wal), b"staged").unwrap();
        Pager::remove_with_wal(&path).unwrap();
        assert!(!path.exists() && !wal.exists() && !wal_tmp_path(&wal).exists());
        Pager::remove_with_wal(&path).expect("nothing there is not an error");
        // A log that cannot be removed (a directory): the error names it and
        // the file is kept.
        struct RmDir(PathBuf);
        impl Drop for RmDir {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
        drop(Pager::open(&path, 8).unwrap());
        let _dir = RmDir(wal.clone());
        std::fs::create_dir_all(wal.join("x")).unwrap();
        let e = Pager::remove_with_wal(&path).expect_err("the log stays");
        assert!(e.to_string().contains(".wal"), "{e}");
        assert!(path.exists(), "the file is not removed without its log");
    }

    #[test]
    fn alloc_write_read_roundtrip() {
        let path = tmp("rw");
        let mut pg = Pager::open(&path, 16).unwrap();
        let a = pg.alloc().unwrap();
        let mut data = [0u8; PAGE_SIZE];
        data[0] = 42;
        data[PAGE_SIZE - 1] = 7;
        pg.write_page(a, &data).unwrap();
        let back = pg.read_page(a).unwrap();
        assert_eq!(back[0], 42);
        assert_eq!(back[PAGE_SIZE - 1], 7);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn persists_across_reopen() {
        let path = tmp("persist");
        let a;
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            a = pg.alloc().unwrap();
            let mut d = [0u8; PAGE_SIZE];
            d[100] = 99;
            pg.write_page(a, &d).unwrap();
            pg.set_root(0, 0xDEAD_BEEF);
            pg.flush().unwrap();
        }
        {
            let pg = Pager::open(&path, 16).unwrap();
            assert_eq!(pg.root(0), 0xDEAD_BEEF);
            let d = pg.read_page(a).unwrap();
            assert_eq!(d[100], 99);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn free_list_reuses_pages() {
        let path = tmp("free");
        let mut pg = Pager::open(&path, 16).unwrap();
        let a = pg.alloc().unwrap();
        let b = pg.alloc().unwrap();
        assert_ne!(a, b);
        pg.free(a).unwrap();
        let c = pg.alloc().unwrap();
        assert_eq!(c, a, "freed page should be reused");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn crc32_matches_known_vector() {
        // The IEEE CRC-32 of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn flush_clears_the_wal() {
        let path = tmp("wal_clear");
        let mut pg = Pager::open(&path, 16).unwrap();
        let a = pg.alloc().unwrap();
        pg.write_page(a, &[7u8; PAGE_SIZE]).unwrap();
        pg.flush().unwrap();
        // After a clean commit the log is gone.
        assert!(!wal_path_for(&path).exists());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn committed_wal_is_replayed_on_open() {
        let path = tmp("wal_replay");
        // Initialise a valid main file.
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            let _ = pg.alloc().unwrap(); // page 1 exists, zeroed
            pg.flush().unwrap();
        }
        // Simulate a crash *after* the WAL was committed but *before* it was
        // applied: hand-write a committed WAL that sets page 1.
        let mut marker = [0u8; PAGE_SIZE];
        marker[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        write_wal(&wal_path_for(&path), &[(1, &marker)]).unwrap();

        // Opening must replay the log, then clear it.
        let pg = Pager::open(&path, 16).unwrap();
        let page = pg.read_page(1).unwrap();
        assert_eq!(&page[0..4], &0xDEAD_BEEFu32.to_le_bytes());
        assert!(!wal_path_for(&path).exists(), "replayed log is removed");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn torn_wal_is_discarded() {
        let path = tmp("wal_torn");
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            let a = pg.alloc().unwrap();
            pg.write_page(a, &[1u8; PAGE_SIZE]).unwrap();
            pg.flush().unwrap();
        }
        // Write a committed WAL, then truncate its commit marker (a torn write).
        let wal = wal_path_for(&path);
        let mut bad = [0u8; PAGE_SIZE];
        bad[0] = 0xAB;
        write_wal(&wal, &[(1, &bad)]).unwrap();
        let mut bytes = std::fs::read(&wal).unwrap();
        bytes.truncate(bytes.len() - 4); // chop part of the commit magic
        std::fs::write(&wal, &bytes).unwrap();

        // Recovery must reject the torn log and leave page 1 as it was (all 1s).
        let pg = Pager::open(&path, 16).unwrap();
        let page = pg.read_page(1).unwrap();
        assert_eq!(page[0], 1, "torn WAL must not be applied");
        assert!(!wal.exists());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unflushed_writes_are_not_durable() {
        let path = tmp("wal_nodurable");
        let a;
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            a = pg.alloc().unwrap();
            pg.write_page(a, &[9u8; PAGE_SIZE]).unwrap();
            pg.flush().unwrap(); // commit the allocation
            pg.write_page(a, &[5u8; PAGE_SIZE]).unwrap(); // dirty, never flushed
                                                          // drop without flushing ⇒ the second write is lost
        }
        let pg = Pager::open(&path, 16).unwrap();
        assert_eq!(
            pg.read_page(a).unwrap()[0],
            9,
            "uncommitted write must be lost"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn concurrent_reads_and_writes_via_rwlock() {
        // Many readers (shared `RwLock::read` → `read_page(&self)`) run alongside
        // a writer (`RwLock::write` → `write_page(&mut self)`). Each page's first
        // 4 bytes always equal its own id (every writer preserves that), so a
        // reader must never observe a torn page — proving the latching is correct.
        use std::sync::{Arc, RwLock};
        use std::thread;

        let path = tmp("concurrent_rw");
        let npages = 64u32;
        let mut pg = Pager::open(&path, 256).unwrap();
        let mut ids = Vec::new();
        for _ in 0..npages {
            let id = pg.alloc().unwrap();
            let mut d = [0u8; PAGE_SIZE];
            d[0..4].copy_from_slice(&id.to_le_bytes());
            pg.write_page(id, &d).unwrap();
            ids.push(id);
        }
        pg.flush().unwrap();
        let pager = Arc::new(RwLock::new(pg));
        let ids = Arc::new(ids);

        let mut handles = Vec::new();
        // Reader threads: assert the page-id invariant on every read.
        for _ in 0..6 {
            let pager = Arc::clone(&pager);
            let ids = Arc::clone(&ids);
            handles.push(thread::spawn(move || {
                for round in 0..2000usize {
                    let id = ids[round % ids.len()];
                    let page = pager.read().unwrap().read_page(id).unwrap();
                    let got = u32::from_le_bytes(page[0..4].try_into().unwrap());
                    assert_eq!(got, id, "reader saw a torn/incorrect page");
                }
            }));
        }
        // Writer thread: rewrite pages, bumping a counter byte but preserving the
        // id invariant.
        {
            let pager = Arc::clone(&pager);
            let ids = Arc::clone(&ids);
            handles.push(thread::spawn(move || {
                for round in 0..2000usize {
                    let id = ids[round % ids.len()];
                    let mut d = [0u8; PAGE_SIZE];
                    d[0..4].copy_from_slice(&id.to_le_bytes());
                    d[4] = (round % 251) as u8;
                    pager.write().unwrap().write_page(id, &d).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Final state is still consistent and durable.
        pager.write().unwrap().flush().unwrap();
        let pager = Arc::try_unwrap(pager).ok().unwrap().into_inner().unwrap();
        let pg = pager;
        for &id in ids.iter() {
            let page = pg.read_page(id).unwrap();
            assert_eq!(u32::from_le_bytes(page[0..4].try_into().unwrap()), id);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn small_cache_evicts_but_keeps_data_correct() {
        // Cache capacity smaller than the working set forces eviction; data must
        // still be correct (dirty pages written back before eviction).
        let path = tmp("evict");
        let mut pg = Pager::open(&path, 8).unwrap();
        let mut ids = Vec::new();
        for i in 0..100u32 {
            let id = pg.alloc().unwrap();
            let mut d = [0u8; PAGE_SIZE];
            d[0..4].copy_from_slice(&i.to_le_bytes());
            pg.write_page(id, &d).unwrap();
            ids.push(id);
        }
        for (i, &id) in ids.iter().enumerate() {
            let d = pg.read_page(id).unwrap();
            let got = u32::from_le_bytes(d[0..4].try_into().unwrap());
            assert_eq!(got, i as u32, "page {id} corrupted after eviction");
        }
        std::fs::remove_file(&path).ok();
    }

    // ---- Phase 6 R1-KV: shared pages, CLOCK, pread, op brackets, format ----

    fn page_with(tag: u32) -> Page {
        let mut d = [0u8; PAGE_SIZE];
        d[0..4].copy_from_slice(&tag.to_le_bytes());
        d[PAGE_SIZE - 4..].copy_from_slice(&tag.to_le_bytes());
        d
    }

    fn tag_of(p: &Page) -> u32 {
        u32::from_le_bytes(p[0..4].try_into().unwrap())
    }

    fn cleanup(path: &Path) {
        std::fs::remove_file(path).ok();
        std::fs::remove_file(wal_path_for(path)).ok();
    }

    #[test]
    fn shared_page_holder_keeps_old_bytes() {
        // Copy on write: a reader's handle is a snapshot of the page.
        let path = tmp("cow");
        let mut pg = Pager::open(&path, 16).unwrap();
        let a = pg.alloc().unwrap();
        pg.write_page(a, &page_with(1)).unwrap();
        let old = pg.read_page_shared(a).unwrap();
        pg.write_page(a, &page_with(2)).unwrap();
        assert_eq!(tag_of(&old), 1, "the old handle keeps the old bytes");
        assert_eq!(tag_of(&pg.read_page_shared(a).unwrap()), 2);
        // Also across a commit and a clean re-read.
        pg.flush().unwrap();
        let committed = pg.read_page_shared(a).unwrap();
        pg.write_page(a, &page_with(3)).unwrap();
        assert_eq!(tag_of(&committed), 2);
        assert_eq!(tag_of(&old), 1);
        assert_eq!(pg.read_page(a).unwrap()[0], 3);
        cleanup(&path);
    }

    #[test]
    fn clock_never_evicts_a_dirty_page() {
        let path = tmp("clock_dirty");
        let mut pg = Pager::open(&path, 8).unwrap();
        // 60 clean committed pages.
        let mut ids = Vec::new();
        for i in 0..60u32 {
            let id = pg.alloc().unwrap();
            pg.write_page(id, &page_with(i)).unwrap();
            ids.push(id);
        }
        pg.flush().unwrap();
        let flushes = pg.flush_count();
        // Dirty three pages, then stream every page through the 8-page cache
        // via the read path: the dirty ones must stay resident and dirty.
        for &id in &ids[..3] {
            pg.write_page(id, &page_with(1000 + id)).unwrap();
        }
        for _ in 0..3 {
            for &id in &ids {
                pg.read_page_shared(id).unwrap();
            }
        }
        assert_eq!(pg.dirty_pages(), 3);
        assert!(
            pg.cached_pages() <= 8,
            "clean pages are evicted to stay in capacity"
        );
        assert_eq!(pg.flush_count(), flushes, "reads never commit");
        for &id in &ids[..3] {
            let cache = pg.cache.lock().unwrap();
            let i = *cache.map.get(&id).expect("dirty page resident");
            assert!(cache.frames[i].dirty);
        }
        // Writes past capacity with no open operation evict clean pages first.
        for &id in &ids[3..8] {
            pg.write_page(id, &page_with(2000 + id)).unwrap();
        }
        assert_eq!(pg.dirty_pages(), 8);
        assert_eq!(pg.flush_count(), flushes, "clean victims existed");
        // Now every frame is dirty: the next new dirty page commits first.
        pg.write_page(ids[9], &page_with(9)).unwrap();
        assert_eq!(pg.flush_count(), flushes + 1);
        drop(pg);
        let pg = Pager::open(&path, 8).unwrap();
        for &id in &ids[..3] {
            assert_eq!(tag_of(&pg.read_page(id).unwrap()), 1000 + id);
        }
        cleanup(&path);
    }

    #[test]
    fn clock_gives_referenced_pages_a_second_chance() {
        let path = tmp("clock_ref");
        let mut pg = Pager::open(&path, 8).unwrap();
        let mut ids = Vec::new();
        for i in 0..40u32 {
            let id = pg.alloc().unwrap();
            pg.write_page(id, &page_with(i)).unwrap();
            ids.push(id);
        }
        pg.flush().unwrap();
        // Fill the cache with 8 clean pages, then keep re-touching one of them
        // while streaming others: it is referenced at every sweep and survives.
        let hot = ids[0];
        for &id in &ids[..8] {
            pg.read_page_shared(id).unwrap();
        }
        for &id in &ids[8..] {
            pg.read_page_shared(hot).unwrap();
            pg.read_page_shared(id).unwrap();
        }
        assert!(pg.cache.lock().unwrap().map.contains_key(&hot));
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn pread_equals_seek_and_read() {
        let path = tmp("pread");
        let mut pg = Pager::open(&path, 16).unwrap();
        let mut ids = Vec::new();
        for i in 0..20u32 {
            let id = pg.alloc().unwrap();
            let mut d = page_with(i);
            for (k, b) in d.iter_mut().enumerate().skip(4).take(PAGE_SIZE - 8) {
                *b = (k as u32 ^ i.wrapping_mul(31)) as u8;
            }
            pg.write_page(id, &d).unwrap();
            ids.push(id);
        }
        pg.flush().unwrap();
        for &id in ids.iter().chain(&[0]) {
            let a = pg.read_at_pread(id).unwrap();
            let b = pg.read_at_seek(id).unwrap();
            assert!(a == b, "page {id} differs between pread and seek+read");
        }
        // Interleaving the two never disturbs either (pread has no cursor).
        for &id in ids.iter().rev() {
            assert!(pg.read_at_seek(id).unwrap() == pg.read_at_pread(id).unwrap());
        }
        // Past the end of the file both fail rather than invent bytes.
        assert!(pg.read_at_pread(pg.page_count() + 5).is_err());
        assert!(pg.read_at_seek(pg.page_count() + 5).is_err());
        cleanup(&path);
    }

    /// Write `n` fresh pages (tags `tag..tag+n`) through `pg`.
    fn big_op(pg: &mut Pager, n: u32, tag: u32) -> Vec<PageId> {
        let mut ids = Vec::new();
        for i in 0..n {
            let id = pg.alloc().unwrap();
            pg.write_page(id, &page_with(tag + i)).unwrap();
            ids.push(id);
        }
        ids
    }

    #[test]
    fn op_bracket_never_flushes_mid_op_and_flushes_at_end() {
        let path = tmp("op_flush");
        let mut pg = Pager::open(&path, 8).unwrap();
        let before = pg.flush_count();
        let mut op = PagerOp::begin(&mut pg);
        let ids = big_op(&mut op, 50, 100);
        assert_eq!(op.flush_count(), before, "no commit inside an operation");
        assert!(
            op.cached_pages() > op.capacity(),
            "the cache overshoots instead"
        );
        assert!(
            op.flush().is_err(),
            "an explicit commit mid-operation is refused"
        );
        op.commit().unwrap();
        assert_eq!(
            pg.flush_count(),
            before + 1,
            "end_op commits the oversize op"
        );
        assert!(pg.cached_pages() <= pg.capacity(), "capacity restored");
        assert_eq!(pg.dirty_pages(), 0);
        drop(pg);
        let pg = Pager::open(&path, 8).unwrap();
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(tag_of(&pg.read_page(id).unwrap()), 100 + i as u32);
        }
        cleanup(&path);
    }

    #[test]
    fn small_op_does_not_flush_at_end() {
        let path = tmp("op_small");
        let mut pg = Pager::open(&path, 64).unwrap();
        let before = pg.flush_count();
        let mut op = PagerOp::begin(&mut pg);
        big_op(&mut op, 5, 0);
        op.commit().unwrap();
        assert_eq!(
            pg.flush_count(),
            before,
            "an op within capacity stays cached"
        );
        assert_eq!(pg.dirty_pages(), 5);
        cleanup(&path);
    }

    #[test]
    fn forgotten_op_is_entirely_absent_after_reopen() {
        // A crash mid-operation (modelled by never ending the bracket and
        // dropping the pager without a commit) loses the whole operation, even
        // though it wrote far more pages than the cache holds.
        let path = tmp("op_forget");
        let committed;
        let pages_before;
        {
            let mut pg = Pager::open(&path, 8).unwrap();
            committed = big_op(&mut pg, 3, 7);
            pg.set_root(0, 42);
            pg.flush().unwrap();
            pages_before = pg.page_count();
            let flushes = pg.flush_count();
            let mut op = PagerOp::begin(&mut pg);
            big_op(&mut op, 40, 500);
            for &id in &committed {
                op.write_page(id, &page_with(999)).unwrap();
            }
            op.set_root(0, 43);
            assert_eq!(op.flush_count(), flushes);
            std::mem::forget(op);
            assert_eq!(pg.flush_count(), flushes, "nothing of the op was committed");
            assert!(pg.flush().is_err(), "the bracket is still open");
        }
        let pg = Pager::open(&path, 8).unwrap();
        assert_eq!(pg.root(0), 42);
        assert_eq!(pg.page_count(), pages_before);
        for (i, &id) in committed.iter().enumerate() {
            assert_eq!(tag_of(&pg.read_page(id).unwrap()), 7 + i as u32);
        }
        cleanup(&path);
    }

    #[test]
    fn dropped_op_rolls_back_pages_and_header() {
        let path = tmp("op_rollback");
        let mut pg = Pager::open(&path, 8).unwrap();
        let committed = big_op(&mut pg, 3, 10);
        pg.flush().unwrap();
        // An uncommitted (but pre-op) change that must survive the rollback.
        pg.write_page(committed[0], &page_with(77)).unwrap();
        pg.set_root(1, 5);
        let (count, roots) = (pg.page_count(), pg.roots);
        {
            let mut op = PagerOp::begin(&mut pg);
            big_op(&mut op, 30, 300);
            op.write_page(committed[0], &page_with(78)).unwrap();
            op.write_page(committed[1], &page_with(79)).unwrap();
            op.free(committed[2]).unwrap();
            op.set_root(1, 6);
            // dropped without commit: rolled back
        }
        assert_eq!(pg.op_depth(), 0);
        assert_eq!(pg.page_count(), count);
        assert_eq!(pg.roots, roots);
        assert_eq!(tag_of(&pg.read_page(committed[0]).unwrap()), 77);
        assert_eq!(tag_of(&pg.read_page(committed[1]).unwrap()), 11);
        assert_eq!(tag_of(&pg.read_page(committed[2]).unwrap()), 12);
        assert_eq!(pg.dirty_pages(), 1, "only the pre-op change is dirty");
        // The pager stays usable and commits only the surviving state.
        let fresh = pg.alloc().unwrap();
        assert_eq!(fresh, count, "rolled-back allocations are reused");
        pg.flush().unwrap();
        drop(pg);
        let pg = Pager::open(&path, 8).unwrap();
        assert_eq!(pg.root(1), 5);
        assert_eq!(tag_of(&pg.read_page(committed[0]).unwrap()), 77);
        assert_eq!(tag_of(&pg.read_page(committed[2]).unwrap()), 12);
        cleanup(&path);
    }

    #[test]
    fn nested_ops_commit_once_and_nested_abort_rolls_back_all() {
        let path = tmp("op_nested");
        let mut pg = Pager::open(&path, 8).unwrap();
        let a = pg.alloc().unwrap();
        pg.write_page(a, &page_with(1)).unwrap();
        pg.flush().unwrap();
        let flushes = pg.flush_count();
        {
            let mut outer = PagerOp::begin(&mut pg);
            big_op(&mut outer, 20, 0);
            {
                let mut inner = PagerOp::begin(&mut outer);
                big_op(&mut inner, 20, 0);
                inner.commit().unwrap();
            }
            assert_eq!(outer.flush_count(), flushes, "inner end_op does not commit");
            outer.commit().unwrap();
        }
        assert_eq!(pg.flush_count(), flushes + 1);
        let count = pg.page_count();
        {
            let mut outer = PagerOp::begin(&mut pg);
            outer.write_page(a, &page_with(2)).unwrap();
            {
                let mut inner = PagerOp::begin(&mut outer);
                inner.write_page(a, &page_with(3)).unwrap();
                // inner dropped without commit
            }
            assert!(
                outer.commit().is_err(),
                "a failed nested op fails the outer one"
            );
        }
        assert_eq!(pg.page_count(), count);
        assert_eq!(tag_of(&pg.read_page(a).unwrap()), 1);
        assert!(pg.end_op().is_err(), "unbalanced end_op is an error");
        cleanup(&path);
    }

    #[test]
    fn format_v1_file_stays_v1_with_zero_padding() {
        let path = tmp("fmt_v1");
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            big_op(&mut pg, 4, 0);
            for i in 0..format::FEATURES {
                pg.set_root(i, 0x0101_0101 * (i as u64 + 1));
            }
            pg.flush().unwrap();
            assert_eq!(pg.features(), 0);
            assert_eq!(pg.magic(), format::MAGIC_V1);
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..8], format::MAGIC_V1);
        assert!(
            bytes[16 + format::NROOTS_V1 * 8..PAGE_SIZE]
                .iter()
                .all(|&b| b == 0),
            "a V1 header has nothing past the pre-Phase-6 root slots"
        );
        let pg = Pager::open(&path, 16).unwrap();
        assert_eq!(pg.features(), 0);
        assert!(pg.require_supported().is_ok());
        assert_eq!(pg.root(20), 0x0101_0101 * 21);
        cleanup(&path);
    }

    /// Overwrite header bytes of a closed pager file.
    fn patch_header(path: &Path, at: usize, bytes: &[u8]) {
        let mut data = std::fs::read(path).unwrap();
        data[at..at + bytes.len()].copy_from_slice(bytes);
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn format_refuses_unknown_bits_and_inconsistent_magic() {
        let path = tmp("fmt_refuse");
        Pager::open(&path, 16).unwrap();
        let feat_off = 16 + format::FEATURES * 8;
        // V2 + an unknown (undefined) bit.
        patch_header(&path, 0, format::MAGIC_V2);
        patch_header(&path, feat_off, &(1u64 << 33).to_le_bytes());
        let e = Pager::open(&path, 16)
            .err()
            .expect("unknown bit refused")
            .to_string();
        assert!(e.contains("feature bits"), "{e}");
        // V2 + a defined bit this binary does not implement yet.
        patch_header(&path, feat_off, &format::FEATURE_LONG_KEYS.to_le_bytes());
        assert!(Pager::open(&path, 16).is_err());
        // V2 without features.
        patch_header(&path, feat_off, &0u64.to_le_bytes());
        let e = Pager::open(&path, 16)
            .err()
            .expect("V2 without bits refused")
            .to_string();
        assert!(e.contains("without feature bits"), "{e}");
        // V1 with features.
        patch_header(&path, 0, format::MAGIC_V1);
        patch_header(&path, feat_off, &format::FEATURE_FREE_IDS.to_le_bytes());
        assert!(Pager::open(&path, 16).is_err());
        // A foreign magic.
        patch_header(&path, 0, b"NOTGSTOR");
        let e = Pager::open(&path, 16).err().unwrap().to_string();
        assert!(e.contains("not a gStore KV file"), "{e}");
        // Back to a valid V1 header: opens again.
        patch_header(&path, 0, format::MAGIC_V1);
        patch_header(&path, feat_off, &0u64.to_le_bytes());
        assert!(Pager::open(&path, 16).is_ok());
        cleanup(&path);
    }

    #[test]
    fn enabling_a_feature_writes_v2_only_with_a_commit() {
        let path = tmp("fmt_enable");
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            assert!(
                pg.enable_feature(format::FEATURE_LONG_KEYS).is_err(),
                "not implemented by this binary (FREE_IDS is, since R4)"
            );
            assert!(
                pg.enable_feature_with(1 << 20, u64::MAX).is_err(),
                "undefined bit"
            );
            let a = pg.alloc().unwrap();
            pg.write_page(a, &page_with(5)).unwrap();
            pg.enable_feature_with(format::FEATURE_LONG_KEYS, format::DEFINED_FEATURES)
                .unwrap();
            assert_eq!(pg.magic(), format::MAGIC_V2);
            assert!(pg.require_supported().is_err());
            // dropped without a commit
        }
        assert_eq!(&std::fs::read(&path).unwrap()[0..8], format::MAGIC_V1);
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            pg.enable_feature_with(format::FEATURE_LONG_KEYS, format::DEFINED_FEATURES)
                .unwrap();
            pg.flush().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            &bytes[0..8],
            format::MAGIC_V2,
            "the bit and V2 commit together"
        );
        let feat_off = 16 + format::FEATURES * 8;
        assert_eq!(
            &bytes[feat_off..feat_off + 8],
            &format::FEATURE_LONG_KEYS.to_le_bytes()
        );
        assert!(
            Pager::open(&path, 16).is_err(),
            "this binary refuses the new feature"
        );
        cleanup(&path);
    }

    // ---- review C-1 / M1 (durability#1, disk#1, disk#2, durability#7,
    // ---- robust#12, durability#8, disk#12, durability#16): the failed state

    /// Removes a test's pager file, its log and a staged log when dropped,
    /// also when the test fails.
    struct Files(PathBuf);

    impl Drop for Files {
        fn drop(&mut self) {
            cleanup(&self.0);
            std::fs::remove_file(wal_tmp_path(&wal_path_for(&self.0))).ok();
        }
    }

    fn is_storage_failed(r: &Result<impl std::fmt::Debug>) -> bool {
        matches!(r, Err(GStoreError::StorageFailed(_)))
    }

    /// 100 committed pages tagged with their index; their ids.
    fn committed_pages(pg: &mut Pager, n: u32) -> Vec<PageId> {
        let ids = big_op(pg, n, 0);
        pg.flush().unwrap();
        ids
    }

    /// Batch N: pages `ids[0]`, `ids[1]`, `ids[59]` rewritten (tags 1000+)
    /// and 20 fresh pages (tags 2000+); returns the fresh ids.
    fn dirty_batch(pg: &mut Pager, ids: &[PageId]) -> Vec<PageId> {
        pg.write_page(ids[0], &page_with(1000)).unwrap();
        pg.write_page(ids[1], &page_with(1001)).unwrap();
        pg.write_page(ids[59], &page_with(1002)).unwrap();
        big_op(pg, 20, 2000)
    }

    /// The pages of batch N, as a reopened pager must see them.
    fn assert_batch_applied(pg: &Pager, ids: &[PageId], fresh: &[PageId]) {
        assert_eq!(tag_of(&pg.read_page(ids[0]).unwrap()), 1000);
        assert_eq!(tag_of(&pg.read_page(ids[1]).unwrap()), 1001);
        assert_eq!(tag_of(&pg.read_page(ids[59]).unwrap()), 1002);
        for (i, &id) in ids.iter().enumerate().skip(2).filter(|&(i, _)| i != 59) {
            assert_eq!(tag_of(&pg.read_page(id).unwrap()), i as u32);
        }
        for (i, &id) in fresh.iter().enumerate() {
            assert_eq!(tag_of(&pg.read_page(id).unwrap()), 2000 + i as u32);
        }
        assert_eq!(pg.page_count(), 1 + ids.len() as u32 + fresh.len() as u32);
    }

    #[test]
    fn a_failed_apply_fails_the_pager_and_a_reopen_replays_the_whole_batch() {
        let path = tmp("fail_apply");
        let _files = Files(path.clone());
        let wal = wal_path_for(&path);
        let (ids, fresh) = {
            let mut pg = Pager::open(&path, 256).unwrap();
            let ids = committed_pages(&mut pg, 100);
            let fresh = dirty_batch(&mut pg, &ids);
            {
                // The header and two pages reach the file, then ENOSPC.
                let _fp = failpoints::fail_apply_after(3);
                let r = pg.flush();
                assert!(is_storage_failed(&r), "{r:?}");
            }
            let why = pg.failure().expect("the pager is failed").to_string();
            assert!(why.contains("write-ahead log"), "{why}");
            let log = std::fs::read(&wal).expect("the committed log stays");
            assert!(parse_wal(&log).is_some(), "and it is complete");
            let file = std::fs::read(&path).unwrap();
            // Every write and every commit is refused from now on.
            assert!(is_storage_failed(&pg.write_page(ids[3], &page_with(7))));
            assert!(is_storage_failed(&pg.alloc()));
            assert!(is_storage_failed(&pg.free(ids[4])));
            assert!(is_storage_failed(&pg.flush()));
            {
                let mut op = PagerOp::begin(&mut pg);
                assert!(is_storage_failed(&op.write_page(ids[5], &page_with(8))));
            }
            // Reads still answer batch N (its pages are resident and dirty).
            assert_eq!(tag_of(&pg.read_page(ids[59]).unwrap()), 1002);
            assert_eq!(
                std::fs::read(&wal).unwrap(),
                log,
                "the log is never rewritten"
            );
            assert_eq!(std::fs::read(&path).unwrap(), file, "nor the file");
            drop(pg); // a clean drop writes nothing either
            assert_eq!(std::fs::read(&wal).unwrap(), log);
            assert_eq!(std::fs::read(&path).unwrap(), file);
            (ids, fresh)
        };
        // The reopen replays the complete log: batch N whole, never a mix.
        let pg = Pager::open(&path, 16).unwrap();
        assert!(pg.failure().is_none(), "a reopen clears the failed state");
        assert!(!wal.exists());
        assert_batch_applied(&pg, &ids, &fresh);
        cleanup(&path);
    }

    #[test]
    fn a_retry_after_a_failed_apply_can_never_tear_the_log() {
        // The review's sequence (durability#16): flush #1 fails mid-apply,
        // the session writes on and flushes again, then exits cleanly. Before
        // the fix the retry truncated the only complete log and failed while
        // rewriting it, so the reopen found a torn log, a half-applied header
        // and missing pages.
        let path = tmp("fail_retry");
        let _files = Files(path.clone());
        let (ids, fresh) = {
            let mut pg = Pager::open(&path, 256).unwrap();
            let ids = committed_pages(&mut pg, 100);
            let fresh = dirty_batch(&mut pg, &ids);
            {
                let _fp = failpoints::fail_apply_after(2);
                assert!(pg.flush().is_err());
            }
            for &id in &ids[2..] {
                assert!(is_storage_failed(&pg.write_page(id, &page_with(3000))));
            }
            assert!(is_storage_failed(&pg.flush()));
            (ids, fresh)
        };
        let pg = Pager::open(&path, 16).unwrap();
        assert_batch_applied(&pg, &ids, &fresh);
        for id in 1..pg.page_count() {
            pg.read_page(id)
                .expect("every page below the page count reads");
        }
        cleanup(&path);
    }

    #[test]
    fn a_failed_wal_removal_fails_the_pager_and_a_reopen_is_consistent() {
        let path = tmp("fail_wal_rm");
        let _files = Files(path.clone());
        let wal = wal_path_for(&path);
        let (ids, fresh) = {
            let mut pg = Pager::open(&path, 256).unwrap();
            let ids = committed_pages(&mut pg, 100);
            let fresh = dirty_batch(&mut pg, &ids);
            {
                let _fp = failpoints::fail_wal_delete();
                assert!(is_storage_failed(&pg.flush()));
            }
            assert!(wal.exists(), "the log could not be removed");
            assert!(pg.failure().is_some());
            assert!(is_storage_failed(&pg.write_page(ids[3], &page_with(9))));
            assert!(is_storage_failed(&pg.flush()));
            (ids, fresh)
        };
        let pg = Pager::open(&path, 16).unwrap();
        assert!(!wal.exists(), "the reopen replays and removes it");
        assert_batch_applied(&pg, &ids, &fresh);
        cleanup(&path);
    }

    #[test]
    fn a_failed_wal_write_changes_nothing_and_the_flush_can_be_retried() {
        let path = tmp("fail_wal_write");
        let _files = Files(path.clone());
        let wal = wal_path_for(&path);
        // A cache that holds the whole batch: no commit before the failpoint.
        let mut pg = Pager::open(&path, 256).unwrap();
        let ids = committed_pages(&mut pg, 100);
        let before = std::fs::read(&path).unwrap();
        let fresh = dirty_batch(&mut pg, &ids);
        {
            let _fp = failpoints::fail_wal_write_after(PAGE_SIZE + 100);
            let r = pg.flush();
            assert!(matches!(r, Err(GStoreError::Io(_))), "{r:?}");
        }
        assert!(
            pg.failure().is_none(),
            "nothing reached the file: retryable"
        );
        assert!(!wal.exists(), "no log got its name");
        assert!(!wal_tmp_path(&wal).exists(), "the staged log is removed");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(pg.dirty_pages(), 23, "the batch is still pending");
        pg.flush().expect("the retry commits");
        drop(pg);
        let pg = Pager::open(&path, 16).unwrap();
        assert_batch_applied(&pg, &ids, &fresh);
        drop(pg);
        // Without the retry, a reopen finds the state before the batch.
        let mut pg = Pager::open(&path, 16).unwrap();
        pg.write_page(ids[7], &page_with(4000)).unwrap();
        {
            let _fp = failpoints::fail_wal_write_after(10);
            assert!(pg.flush().is_err());
        }
        drop(pg);
        let pg = Pager::open(&path, 16).unwrap();
        assert_eq!(tag_of(&pg.read_page(ids[7]).unwrap()), 7);
        cleanup(&path);
    }

    #[test]
    fn a_staged_log_never_replaces_a_complete_one_in_place() {
        // Review durability#1 / disk#1: the log used to be opened with
        // O_TRUNC, so rewriting it destroyed the complete one first. Now a
        // log is staged and renamed: a failure leaves the old log intact.
        let path = tmp("wal_no_trunc");
        let _files = Files(path.clone());
        let wal = wal_path_for(&path);
        let old = page_with(11);
        write_wal(&wal, &[(1, &old)]).unwrap();
        let complete = std::fs::read(&wal).unwrap();
        let big: Vec<(PageId, Box<Page>)> =
            (1..40).map(|i| (i, Box::new(page_with(500 + i)))).collect();
        {
            let _fp = failpoints::fail_wal_write_after(3 * PAGE_SIZE);
            assert!(write_wal(&wal, &big).is_err());
        }
        assert_eq!(std::fs::read(&wal).unwrap(), complete, "untouched");
        assert!(!wal_tmp_path(&wal).exists());
        write_wal(&wal, &big).unwrap();
        let replaced = parse_wal(&std::fs::read(&wal).unwrap()).expect("complete");
        assert_eq!(replaced.len(), 39, "replaced whole, atomically");
        cleanup(&path);
    }

    #[test]
    fn an_unreadable_wal_fails_the_open_and_only_a_missing_one_means_no_log() {
        // Review disk#2 / durability#7 / robust#12: any read error used to
        // mean "no log", so the file opened half-applied and the next flush
        // overwrote the log for good.
        let path = tmp("wal_unreadable");
        let _files = Files(path.clone());
        let wal = wal_path_for(&path);
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            committed_pages(&mut pg, 3);
        }
        // A crash mid-apply: a complete log, page 1 applied, page 2 not.
        write_wal(&wal, &[(1, &page_with(91)), (2, &page_with(92))]).unwrap();
        {
            let mut f = OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
            f.write_all(&page_with(91)).unwrap();
        }
        let file = std::fs::read(&path).unwrap();
        {
            let _fp = failpoints::wal_unreadable();
            let e = Pager::open(&path, 16).err().expect("the open fails");
            assert!(e.to_string().contains("write-ahead log"), "{e}");
        }
        assert!(wal.exists(), "the log is kept");
        assert_eq!(std::fs::read(&path).unwrap(), file, "the file untouched");
        let pg = Pager::open(&path, 16).unwrap();
        assert_eq!(tag_of(&pg.read_page(1).unwrap()), 91);
        assert_eq!(tag_of(&pg.read_page(2).unwrap()), 92);
        assert!(!wal.exists());
        cleanup(&path);
    }

    #[test]
    fn a_leftover_staged_log_is_not_replayed_and_is_removed() {
        let path = tmp("wal_tmp_left");
        let _files = Files(path.clone());
        let wal = wal_path_for(&path);
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            committed_pages(&mut pg, 3);
        }
        // A crash after staging, before the rename: even a complete staged
        // log is not a commit.
        write_wal(&wal, &[(1, &page_with(77))]).unwrap();
        std::fs::rename(&wal, wal_tmp_path(&wal)).unwrap();
        let pg = Pager::open(&path, 16).unwrap();
        assert_eq!(tag_of(&pg.read_page(1).unwrap()), 0);
        assert!(!wal_tmp_path(&wal).exists());
        cleanup(&path);
    }

    #[test]
    fn a_file_shorter_than_its_page_count_is_refused() {
        let path = tmp("short_file");
        let _files = Files(path.clone());
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            committed_pages(&mut pg, 10);
        }
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(6 * PAGE_SIZE as u64).unwrap();
        drop(f);
        let e = Pager::open(&path, 16).err().expect("refused").to_string();
        assert!(e.contains("torn or truncated"), "{e}");
        cleanup(&path);
    }

    #[test]
    fn every_commit_fsyncs_the_directory_after_naming_and_removing_the_log() {
        // Review durability#8 / robust#12.
        let path = tmp("dir_sync");
        let _files = Files(path.clone());
        let wal = wal_path_for(&path);
        let mut pg = Pager::open(&path, 16).unwrap();
        let ids = committed_pages(&mut pg, 4);
        pg.write_page(ids[0], &page_with(5)).unwrap();
        let before = failpoints::dir_syncs();
        pg.flush().unwrap();
        assert_eq!(failpoints::dir_syncs() - before, 2, "rename + removal");
        drop(pg);
        write_wal(&wal, &[(1, &page_with(6))]).unwrap();
        let before = failpoints::dir_syncs();
        let pg = Pager::open(&path, 16).unwrap();
        assert_eq!(
            failpoints::dir_syncs() - before,
            1,
            "the replayed log's removal"
        );
        assert_eq!(tag_of(&pg.read_page(1).unwrap()), 6);
        cleanup(&path);
    }

    #[test]
    fn a_pager_marked_failed_refuses_writes_and_commits() {
        let path = tmp("mark_failed");
        let _files = Files(path.clone());
        let mut pg = Pager::open(&path, 16).unwrap();
        let ids = committed_pages(&mut pg, 4);
        pg.write_page(ids[0], &page_with(5)).unwrap();
        let flag = pg.failure_flag();
        let _ = flag.set("the store above found itself out of step".into());
        assert_eq!(
            pg.failure(),
            Some("the store above found itself out of step")
        );
        assert!(is_storage_failed(&pg.flush()));
        assert!(is_storage_failed(&pg.write_page(ids[1], &page_with(6))));
        pg.mark_failed("a second reason is ignored");
        assert_eq!(
            pg.failure(),
            Some("the store above found itself out of step")
        );
        drop(pg);
        let pg = Pager::open(&path, 16).unwrap();
        assert_eq!(tag_of(&pg.read_page(ids[0]).unwrap()), 0, "never committed");
        cleanup(&path);
    }
}
