//! A paged file with an LRU page cache and free-list allocator.
//!
//! Corresponds to the storage substrate beneath gStore's `KVstore`
//! (fixed-size disk blocks + buffer cache). The file is a sequence of
//! [`PAGE_SIZE`]-byte pages; page 0 is the header. Higher layers (the B+ tree)
//! address data by [`PageId`] and read/write whole pages through the cache.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{GStoreError, Result};

/// Fixed disk-block size (gStore: `STORAGE_BLOCK_SIZE = 1 << 12`).
pub const PAGE_SIZE: usize = 4096;

/// Write-ahead-log framing: a committed batch is `WAL_MAGIC | n | (id,page)×n |
/// crc32(body) | COMMIT_MAGIC`. The trailing commit marker + checksum make a
/// torn or partial log recognisable: recovery replays it only when both are
/// intact, so a crash mid-commit loses the batch but never corrupts the store.
const WAL_MAGIC: &[u8; 8] = b"GSTOREWL";
const COMMIT_MAGIC: &[u8; 8] = b"GSTORECM";

/// A page index into the file. Page 0 is the header; 0 also means "null".
pub type PageId = u32;

/// Number of `u64` root slots in the header for higher layers (tree roots, etc.).
pub const NROOTS: usize = 16;

const MAGIC: &[u8; 8] = b"GSTOREKV";

/// A cached page plus its dirty flag and last-use tick (for LRU eviction).
struct CachedPage {
    data: Box<[u8; PAGE_SIZE]>,
    dirty: bool,
    tick: u64,
}

/// The mutable buffer-cache state, behind a `Mutex` so cached reads (which only
/// touch the cache + the file) can run through `&self` and thus concurrently
/// under a shared [`std::sync::RwLock`] read guard at the layer above.
struct CacheState {
    map: HashMap<PageId, CachedPage>,
    clock: u64,
}

/// A paged file with a write-back LRU cache.
///
/// ## Concurrency (task 4: fine-grained latching)
/// `read_page` takes `&self`: the page data it returns is an owned copy, and the
/// only shared mutable state it touches — the buffer cache and the file handle —
/// lives behind short-held `Mutex`es here. So the layer above can hold the
/// `Pager` in an `Arc<RwLock<Pager>>` and let **many readers** resolve pages
/// concurrently (each `RwLock::read`), while mutations (`write_page`/`alloc`/
/// `free`/`flush`, all `&mut self`) take the exclusive `RwLock::write`. Those
/// `&mut self` methods reach the cache/file via `get_mut()` (no locking needed —
/// `&mut self` already proves exclusivity), so the write path keeps the original
/// strict eviction + WAL commit protocol unchanged.
pub struct Pager {
    file: Mutex<File>,
    page_count: u32,
    free_head: PageId,
    roots: [u64; NROOTS],
    cache: Mutex<CacheState>,
    capacity: usize,
    /// Path of the write-ahead log beside the main file (`<file>.wal`).
    wal_path: PathBuf,
}

impl Pager {
    /// Open (or create) a paged file at `path` with a cache of `capacity` pages.
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

        if len == 0 {
            // Fresh file: page 0 is the header, data starts at page 1.
            let mut p = Pager {
                file: Mutex::new(file),
                page_count: 1,
                free_head: 0,
                roots: [0; NROOTS],
                cache: Mutex::new(CacheState {
                    map: HashMap::new(),
                    clock: 0,
                }),
                capacity,
                wal_path,
            };
            p.write_header()?;
            p.flush()?;
            Ok(p)
        } else {
            // Existing file: read the header from page 0.
            let mut hdr = [0u8; PAGE_SIZE];
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut hdr)?;
            if &hdr[0..8] != MAGIC {
                return Err(GStoreError::Database("not a gStore KV file".into()));
            }
            let page_count = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
            let free_head = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
            let mut roots = [0u64; NROOTS];
            for (i, r) in roots.iter_mut().enumerate() {
                let off = 16 + i * 8;
                *r = u64::from_le_bytes(hdr[off..off + 8].try_into().unwrap());
            }
            Ok(Pager {
                file: Mutex::new(file),
                page_count,
                free_head,
                roots,
                cache: Mutex::new(CacheState {
                    map: HashMap::new(),
                    clock: 0,
                }),
                capacity,
                wal_path,
            })
        }
    }

    /// A header root slot (higher layers store tree roots etc. here).
    pub fn root(&self, i: usize) -> u64 {
        self.roots[i]
    }

    /// Set a header root slot (persisted on [`flush`](Self::flush)).
    pub fn set_root(&mut self, i: usize, v: u64) {
        self.roots[i] = v;
    }

    fn header_bytes(&self) -> [u8; PAGE_SIZE] {
        let mut hdr = [0u8; PAGE_SIZE];
        hdr[0..8].copy_from_slice(MAGIC);
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
        let f = self.file.get_mut().unwrap();
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&hdr)?;
        Ok(())
    }

    /// Read a page (from cache, else from disk into the cache). Takes `&self`:
    /// the returned page is an owned copy and the shared mutable state (cache,
    /// file handle) is touched only under short-held internal `Mutex`es, so many
    /// readers can run this concurrently (see the type-level concurrency note).
    pub fn read_page(&self, id: PageId) -> Result<[u8; PAGE_SIZE]> {
        // Fast path: a cache hit, refreshing the LRU tick.
        {
            let mut cache = self.cache.lock().unwrap();
            cache.clock += 1;
            let tick = cache.clock;
            if let Some(cp) = cache.map.get_mut(&id) {
                cp.tick = tick;
                return Ok(*cp.data);
            }
        }
        // Miss: read from the file (brief file-lock), then populate the cache.
        let mut buf = [0u8; PAGE_SIZE];
        {
            let mut f = self.file.lock().unwrap();
            f.seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
            f.read_exact(&mut buf)?;
        }
        {
            let mut cache = self.cache.lock().unwrap();
            cache.clock += 1;
            let tick = cache.clock;
            // A concurrent reader may have inserted this page meanwhile; that is
            // harmless (same bytes), so just (re)insert. Evict only *clean* pages
            // under `&self`; if the cache is full of dirty pages we let it grow
            // slightly rather than commit on a read (writes flush under &mut).
            if cache.map.len() >= self.capacity {
                evict_one_clean(&mut cache);
            }
            cache.map.insert(
                id,
                CachedPage {
                    data: Box::new(buf),
                    dirty: false,
                    tick,
                },
            );
        }
        Ok(buf)
    }

    /// Write a page (into the cache, marked dirty; persisted on flush/eviction).
    pub fn write_page(&mut self, id: PageId, data: &[u8; PAGE_SIZE]) -> Result<()> {
        if id == 0 {
            return Err(GStoreError::Database(
                "page 0 is reserved for header".into(),
            ));
        }
        // `&mut self` ⇒ exclusive access; reach the cache without locking.
        let cache = self.cache.get_mut().unwrap();
        cache.clock += 1;
        let tick = cache.clock;
        if let Some(cp) = cache.map.get_mut(&id) {
            *cp.data = *data;
            cp.dirty = true;
            cp.tick = tick;
            return Ok(());
        }
        self.insert_cache(id, *data, true, tick)
    }

    fn insert_cache(
        &mut self,
        id: PageId,
        data: [u8; PAGE_SIZE],
        dirty: bool,
        tick: u64,
    ) -> Result<()> {
        if self.cache.get_mut().unwrap().map.len() >= self.capacity {
            self.evict_one()?;
        }
        self.cache.get_mut().unwrap().map.insert(
            id,
            CachedPage {
                data: Box::new(data),
                dirty,
                tick,
            },
        );
        Ok(())
    }

    /// Evict the least-recently-used *clean* page. Dirty pages reach the main
    /// file only through a committed [`flush`](Self::flush) (preserving the WAL
    /// invariant), so if every cached page is dirty we commit first, then evict.
    fn evict_one(&mut self) -> Result<()> {
        let clean_lru = {
            let cache = self.cache.get_mut().unwrap();
            cache
                .map
                .iter()
                .filter(|(_, cp)| !cp.dirty)
                .min_by_key(|(_, cp)| cp.tick)
                .map(|(&id, _)| id)
        };
        let victim = match clean_lru {
            Some(id) => id,
            None => {
                // All cached pages are dirty: commit them, then they're clean.
                self.flush()?;
                match self
                    .cache
                    .get_mut()
                    .unwrap()
                    .map
                    .iter()
                    .min_by_key(|(_, cp)| cp.tick)
                {
                    Some((&id, _)) => id,
                    None => return Ok(()),
                }
            }
        };
        self.cache.get_mut().unwrap().map.remove(&victim);
        Ok(())
    }

    /// Allocate a fresh page (reusing the free list when possible).
    pub fn alloc(&mut self) -> Result<PageId> {
        if self.free_head != 0 {
            let id = self.free_head;
            // The first 4 bytes of a free page hold the next free page id.
            let page = self.read_page(id)?;
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
    /// partial mix *of this page batch*. (A higher-level operation spanning
    /// several flushes is not itself atomic; see `DiskStore::insert_ids`.)
    pub fn flush(&mut self) -> Result<()> {
        // The committed batch: the header (page 0) plus all dirty pages.
        let mut batch: Vec<(PageId, [u8; PAGE_SIZE])> = Vec::new();
        batch.push((0, self.header_bytes()));
        {
            let cache = self.cache.get_mut().unwrap();
            let dirty: Vec<PageId> = cache
                .map
                .iter()
                .filter(|(_, cp)| cp.dirty)
                .map(|(&id, _)| id)
                .collect();
            for id in &dirty {
                batch.push((*id, *cache.map.get(id).unwrap().data));
            }
        }

        // 1. Write-ahead: log the batch, then fsync the log.
        write_wal(&self.wal_path, &batch)?;
        // 2. Apply the batch to the main file, then fsync it.
        {
            let f = self.file.get_mut().unwrap();
            for (id, data) in &batch {
                f.seek(SeekFrom::Start(*id as u64 * PAGE_SIZE as u64))?;
                f.write_all(data)?;
            }
            f.sync_all()?;
        }
        // 3. Drop the log: the main file is now the durable state.
        let _ = std::fs::remove_file(&self.wal_path);

        // Clear dirty flags on the pages we just committed.
        let cache = self.cache.get_mut().unwrap();
        for (_, cp) in cache.map.iter_mut() {
            cp.dirty = false;
        }
        Ok(())
    }

    /// Number of allocated pages (including the header).
    pub fn page_count(&self) -> u32 {
        self.page_count
    }
}

/// Evict the least-recently-used *clean* page from `cache`, if any. Used by the
/// `&self` read path, which must not commit dirty pages (that is the `&mut self`
/// write path's job via the WAL); if every cached page is dirty this is a no-op
/// and the cache grows slightly past capacity until the next flush.
fn evict_one_clean(cache: &mut CacheState) {
    if let Some(victim) = cache
        .map
        .iter()
        .filter(|(_, cp)| !cp.dirty)
        .min_by_key(|(_, cp)| cp.tick)
        .map(|(&id, _)| id)
    {
        cache.map.remove(&victim);
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
fn write_wal(path: &Path, batch: &[(PageId, [u8; PAGE_SIZE])]) -> Result<()> {
    let mut buf = Vec::with_capacity(8 + 4 + batch.len() * (4 + PAGE_SIZE) + 12);
    buf.extend_from_slice(WAL_MAGIC);
    buf.extend_from_slice(&(batch.len() as u32).to_le_bytes());
    for (id, data) in batch {
        buf.extend_from_slice(&id.to_le_bytes());
        buf.extend_from_slice(data);
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
        let p = std::env::temp_dir().join(format!("gstore_pager_{tag}.kv"));
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
        write_wal(&wal_path_for(&path), &[(1, marker)]).unwrap();

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
        write_wal(&wal, &[(1, bad)]).unwrap();
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
}
