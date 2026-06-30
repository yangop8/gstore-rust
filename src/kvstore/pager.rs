//! A paged file with an LRU page cache and free-list allocator.
//!
//! Corresponds to the storage substrate beneath gStore's `KVstore`
//! (fixed-size disk blocks + buffer cache). The file is a sequence of
//! [`PAGE_SIZE`]-byte pages; page 0 is the header. Higher layers (the B+ tree)
//! address data by [`PageId`] and read/write whole pages through the cache.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{GStoreError, Result};

/// Fixed disk-block size (gStore: `STORAGE_BLOCK_SIZE = 1 << 12`).
pub const PAGE_SIZE: usize = 4096;

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

/// A paged file with a write-back LRU cache.
pub struct Pager {
    file: File,
    page_count: u32,
    free_head: PageId,
    roots: [u64; NROOTS],
    cache: HashMap<PageId, CachedPage>,
    capacity: usize,
    clock: u64,
}

impl Pager {
    /// Open (or create) a paged file at `path` with a cache of `capacity` pages.
    pub fn open<P: AsRef<Path>>(path: P, capacity: usize) -> Result<Pager> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        let capacity = capacity.max(8);

        if len == 0 {
            // Fresh file: page 0 is the header, data starts at page 1.
            let mut p = Pager {
                file,
                page_count: 1,
                free_head: 0,
                roots: [0; NROOTS],
                cache: HashMap::new(),
                capacity,
                clock: 0,
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
                file,
                page_count,
                free_head,
                roots,
                cache: HashMap::new(),
                capacity,
                clock: 0,
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
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&hdr)?;
        Ok(())
    }

    /// Read a page (from cache, else from disk into the cache).
    pub fn read_page(&mut self, id: PageId) -> Result<[u8; PAGE_SIZE]> {
        self.clock += 1;
        let tick = self.clock;
        if let Some(cp) = self.cache.get_mut(&id) {
            cp.tick = tick;
            return Ok(*cp.data);
        }
        let mut buf = [0u8; PAGE_SIZE];
        self.file
            .seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
        self.file.read_exact(&mut buf)?;
        self.insert_cache(id, buf, false, tick)?;
        Ok(buf)
    }

    /// Write a page (into the cache, marked dirty; persisted on flush/eviction).
    pub fn write_page(&mut self, id: PageId, data: &[u8; PAGE_SIZE]) -> Result<()> {
        self.clock += 1;
        let tick = self.clock;
        if let Some(cp) = self.cache.get_mut(&id) {
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
        if self.cache.len() >= self.capacity {
            self.evict_one()?;
        }
        self.cache.insert(
            id,
            CachedPage {
                data: Box::new(data),
                dirty,
                tick,
            },
        );
        Ok(())
    }

    /// Evict the least-recently-used page, writing it back if dirty.
    fn evict_one(&mut self) -> Result<()> {
        let Some((&victim, _)) = self.cache.iter().min_by_key(|(_, cp)| cp.tick) else {
            return Ok(());
        };
        let cp = self.cache.remove(&victim).unwrap();
        if cp.dirty {
            self.write_to_disk(victim, &cp.data)?;
        }
        Ok(())
    }

    fn write_to_disk(&mut self, id: PageId, data: &[u8; PAGE_SIZE]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(id as u64 * PAGE_SIZE as u64))?;
        self.file.write_all(data)?;
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
            self.page_count += 1;
            self.write_page(id, &[0u8; PAGE_SIZE])?;
            Ok(id)
        }
    }

    /// Return a page to the free list.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(&self.free_head.to_le_bytes());
        self.write_page(id, &page)?;
        self.free_head = id;
        Ok(())
    }

    /// Flush all dirty pages and the header to disk.
    pub fn flush(&mut self) -> Result<()> {
        let dirty: Vec<PageId> = self
            .cache
            .iter()
            .filter(|(_, cp)| cp.dirty)
            .map(|(&id, _)| id)
            .collect();
        for id in dirty {
            let data = *self.cache.get(&id).unwrap().data;
            self.write_to_disk(id, &data)?;
            self.cache.get_mut(&id).unwrap().dirty = false;
        }
        self.write_header()?;
        self.file.flush()?;
        Ok(())
    }

    /// Number of allocated pages (including the header).
    pub fn page_count(&self) -> u32 {
        self.page_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gstore_pager_{tag}.kv"));
        let _ = std::fs::remove_file(&p);
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
            let mut pg = Pager::open(&path, 16).unwrap();
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
