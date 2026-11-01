//! A paged file with a CLOCK page cache, a free-list allocator, a write-ahead
//! log and per-operation brackets.
//!
//! Corresponds to the storage substrate beneath gStore's `KVstore`
//! (fixed-size disk blocks + buffer cache). The file is a sequence of
//! [`PAGE_SIZE`]-byte pages; page 0 is the header (its layout and every
//! version marker live in the [`format`](super::format) registry). Higher
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

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    /// Number of committed flushes (tests assert when commits happen).
    #[cfg(test)]
    flushes: u64,
}

impl Pager {
    /// Open (or create) a paged file at `path` with a cache of `capacity` pages.
    ///
    /// An existing file must carry a header this binary supports (see
    /// [`format::check_header`]); a new file is created as version 1.
    pub fn open<P: AsRef<Path>>(path: P, capacity: usize) -> Result<Pager> {
        let path = path.as_ref();
        let wal_path = wal_path_for(path);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        // Crash recovery: replay a fully-committed WAL into the main file (redo),
        // discard a torn one, then clear the log — before reading the header.
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
            #[cfg(test)]
            flushes: 0,
        };
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
        }
        Ok(p)
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
            let mut cache = self.cache.lock().unwrap();
            if let Some(&i) = cache.map.get(&id) {
                let f = &mut cache.frames[i];
                f.referenced = true;
                return Ok(Arc::clone(&f.data));
            }
        }
        // Miss: read from the file without holding the cache lock.
        let data = Arc::new(self.read_from_file(id)?);
        let mut cache = self.cache.lock().unwrap();
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
        let gen = (self.op_depth > 0).then_some(self.op_gen);
        // `&mut self` ⇒ exclusive access; reach the cache without locking.
        let cache = self.cache.get_mut().unwrap();
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
            let cache = self.cache.get_mut().unwrap();
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
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(&self.free_head.to_le_bytes());
        self.write_page(id, &page)?;
        self.free_head = id;
        Ok(())
    }

    /// Commit: durably persist the header and every dirty page as one atomic
    /// batch. The batch is first written (with a checksum + commit marker) to the
    /// write-ahead log and fsynced, then applied to the main file and fsynced,
    /// then the log is cleared. A crash at any point leaves either the old state
    /// (log torn/absent) or the new state (log replayed on next open) — never a
    /// partial mix *of this page batch*. Refused while an operation bracket is
    /// open, so a batch never holds part of an operation.
    pub fn flush(&mut self) -> Result<()> {
        if self.op_depth > 0 {
            return Err(GStoreError::Database(
                "cannot commit the page cache inside an open operation".into(),
            ));
        }
        // The committed batch: the header (page 0) plus all dirty pages.
        let header = Arc::new(self.header_bytes());
        let mut batch: Vec<(PageId, Arc<Page>)> = vec![(0, header)];
        {
            let cache = self.cache.get_mut().unwrap();
            batch.extend(
                cache
                    .frames
                    .iter()
                    .filter(|f| f.dirty)
                    .map(|f| (f.id, Arc::clone(&f.data))),
            );
        }

        // 1. Write-ahead: log the batch, then fsync the log.
        write_wal(&self.wal_path, &batch)?;
        // 2. Apply the batch to the main file, then fsync it.
        for (id, data) in &batch {
            self.file
                .seek(SeekFrom::Start(*id as u64 * PAGE_SIZE as u64))?;
            self.file.write_all(&data[..])?;
        }
        self.file.sync_all()?;
        // 3. Drop the log: the main file is now the durable state.
        let _ = std::fs::remove_file(&self.wal_path);
        drop(batch);

        // Every cached page is clean now; return to capacity.
        let cache = self.cache.get_mut().unwrap();
        for f in &mut cache.frames {
            f.dirty = false;
        }
        cache.dirty = 0;
        cache.trim(self.capacity);
        #[cfg(test)]
        {
            self.flushes += 1;
        }
        Ok(())
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
        self.cache.lock().unwrap().dirty
    }

    /// Number of pages currently cached.
    pub fn cached_pages(&self) -> usize {
        self.cache.lock().unwrap().frames.len()
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
        let undo = std::mem::take(&mut self.op_undo);
        let cache = self.cache.get_mut().unwrap();
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
        let cache = self.cache.get_mut().unwrap();
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

    /// Committed flushes since open (tests).
    #[cfg(test)]
    pub(crate) fn flush_count(&self) -> u64 {
        self.flushes
    }
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
        PagerOp {
            pager,
            done: false,
        }
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

/// The WAL path beside a database file: `<path>.wal`.
fn wal_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".wal");
    PathBuf::from(s)
}

/// Serialize and durably write a committed batch to the WAL (truncating any
/// previous one), fsyncing before returning so the log is on disk before the
/// main file is touched.
fn write_wal<P: Deref<Target = Page>>(path: &Path, batch: &[(PageId, P)]) -> Result<()> {
    let mut buf = Vec::with_capacity(8 + 4 + batch.len() * (4 + PAGE_SIZE) + 12);
    buf.extend_from_slice(WAL_MAGIC);
    buf.extend_from_slice(&(batch.len() as u32).to_le_bytes());
    for (id, data) in batch {
        buf.extend_from_slice(&id.to_le_bytes());
        buf.extend_from_slice(&data[..]);
    }
    // Checksum covers the count + entries (everything after the magic).
    let crc = crc32(&buf[8..]);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(COMMIT_MAGIC);

    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(&buf)?;
    f.sync_all()?;
    Ok(())
}

/// Replay a committed WAL into the main file, then remove the log. A missing,
/// torn, or checksum-mismatched log is discarded (the main file is already the
/// last consistent state).
fn recover_wal(file: &mut File, path: &Path) -> Result<()> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return Ok(()), // no log ⇒ nothing to recover
    };
    if let Some(batch) = parse_wal(&data) {
        for (id, page) in batch {
            file.seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
            file.write_all(&page)?;
        }
        file.sync_all()?;
    }
    let _ = std::fs::remove_file(path);
    Ok(())
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

/// CRC-32 (IEEE 802.3, reflected) — a dependency-free integrity check for WAL
/// records. Not cryptographic; only detects accidental corruption / torn writes.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg(); // 0x0 or 0xFFFFFFFF
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
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
        assert_eq!(pg.read_page(a).unwrap()[0], 9, "uncommitted write must be lost");
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
        assert!(pg.cached_pages() <= 8, "clean pages are evicted to stay in capacity");
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
        assert!(op.cached_pages() > op.capacity(), "the cache overshoots instead");
        assert!(op.flush().is_err(), "an explicit commit mid-operation is refused");
        op.commit().unwrap();
        assert_eq!(pg.flush_count(), before + 1, "end_op commits the oversize op");
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
        assert_eq!(pg.flush_count(), before, "an op within capacity stays cached");
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
            assert!(outer.commit().is_err(), "a failed nested op fails the outer one");
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
            bytes[16 + format::NROOTS_V1 * 8..PAGE_SIZE].iter().all(|&b| b == 0),
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
        let e = Pager::open(&path, 16).err().expect("unknown bit refused").to_string();
        assert!(e.contains("feature bits"), "{e}");
        // V2 + a defined bit this binary does not implement yet.
        patch_header(&path, feat_off, &format::FEATURE_LONG_KEYS.to_le_bytes());
        assert!(Pager::open(&path, 16).is_err());
        // V2 without features.
        patch_header(&path, feat_off, &0u64.to_le_bytes());
        let e = Pager::open(&path, 16).err().expect("V2 without bits refused").to_string();
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
                pg.enable_feature(format::FEATURE_FREE_IDS).is_err(),
                "not implemented by this binary yet"
            );
            assert!(pg.enable_feature_with(1 << 20, u64::MAX).is_err(), "undefined bit");
            let a = pg.alloc().unwrap();
            pg.write_page(a, &page_with(5)).unwrap();
            pg.enable_feature_with(format::FEATURE_FREE_IDS, format::DEFINED_FEATURES)
                .unwrap();
            assert_eq!(pg.magic(), format::MAGIC_V2);
            assert!(pg.require_supported().is_err());
            // dropped without a commit
        }
        assert_eq!(&std::fs::read(&path).unwrap()[0..8], format::MAGIC_V1);
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            pg.enable_feature_with(format::FEATURE_FREE_IDS, format::DEFINED_FEATURES)
                .unwrap();
            pg.flush().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..8], format::MAGIC_V2, "the bit and V2 commit together");
        let feat_off = 16 + format::FEATURES * 8;
        assert_eq!(&bytes[feat_off..feat_off + 8], &format::FEATURE_FREE_IDS.to_le_bytes());
        assert!(Pager::open(&path, 16).is_err(), "this binary refuses the new feature");
        cleanup(&path);
    }
}
