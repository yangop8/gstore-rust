//! `IvArray` — a **dense, integer-keyed** value array (gStore's `IVArray` /
//! `ISArray`).
//!
//! gStore does not put its id-keyed stores in a B+ tree. Instead it keeps an
//! *array of entries* indexed directly by the id (`IVArray`/`ISArray`), where
//! each entry holds a small header plus a pointer (`store`) into a separate,
//! block-managed value file (`IVBlockManager`): fixed-size blocks chained by a
//! leading next-block pointer, so a value spans as many blocks as it needs and
//! the entry array stays compact. The id *is* the array index — no key bytes are
//! stored and no tree is walked, which is the natural layout for the densely
//! numbered id→value stores (`id2entity`, `id2literal`, `id2predicate`, and the
//! per-subject value lists).
//!
//! This is the Rust counterpart over the shared [`Pager`], differentiating those
//! integer-keyed stores from the generic variable-key [`BTree`](super::bptree)
//! that still serves the *string*-keyed forward dictionary (`entity2id`, …) and
//! the composite-key triple indexes (SPO/POS/OSP, which need ordered prefix
//! scans an array can't provide).
//!
//! ## Layout
//! The "entry array" is a set of **directory pages** of fixed-size [`SLOT_SIZE`]
//! slots; key `k` lives in slot `k % SLOTS_PER_DIR` of directory `k / SLOTS_PER_DIR`.
//! Directories are located through a two-level **page table** rooted at a pager
//! header slot (L0 → L1 → directory page id), so a lookup is a constant few page
//! reads — all served by the page cache. Each slot is:
//! ```text
//! [tag: u8][len: u32 LE][payload …]
//! ```
//! A value up to [`INLINE_CAP`] bytes is stored inline in the slot; a larger one
//! spills into an [`overflow`] page chain (the block-managed value file) and the
//! slot keeps only the chain head + length — exactly gStore's entry → block model.
//!
//! ## Capacity
//! The two-level table addresses `PT_FANOUT² × SLOTS_PER_DIR` keys
//! (≈ 33.5 M with the constants below); a key beyond that returns an error rather
//! than silently misbehaving. That covers the test/working sets here; a deeper
//! table (or a wider page) would raise the bound and is left as future work.

use crate::error::{GStoreError, Result};

use super::overflow;
use super::pager::{PageId, Pager, PAGE_SIZE};

/// A handle to one integer-keyed array, rooted at a [`Pager`] header slot. Like
/// [`BTree`](super::bptree::BTree) the structure lives in pager pages, so the
/// handle is just the slot index and is `Copy` — cheap to share (e.g. with the
/// out-of-core dictionary backend).
#[derive(Clone, Copy)]
pub struct IvArray {
    root_slot: usize,
}

/// Bytes per directory slot. Sized so typical dictionary strings (IRIs/literals)
/// fit inline; longer values spill to an overflow chain.
const SLOT_SIZE: usize = 128;
/// Slots per directory page.
const SLOTS_PER_DIR: usize = PAGE_SIZE / SLOT_SIZE; // 32
/// Page-table entries (u32 page ids) per page-table page.
const PT_FANOUT: usize = PAGE_SIZE / 4; // 1024
/// Max value bytes stored inline in a slot: `SLOT_SIZE - tag(1) - len(4)`.
const INLINE_CAP: usize = SLOT_SIZE - 5; // 123

/// Slot is empty (no value for this key).
const TAG_EMPTY: u8 = 0;
/// Slot stores its value inline (the bytes follow the length).
const TAG_INLINE: u8 = 1;
/// Slot stores an [`overflow`] chain head (4-byte page id) + the length.
const TAG_OVERFLOW: u8 = 2;

impl IvArray {
    pub fn new(root_slot: usize) -> IvArray {
        IvArray { root_slot }
    }

    /// Decompose `key` into (L0 index, L1 index, slot index), erroring if it is
    /// beyond the addressable capacity of the two-level page table.
    fn locate(key: u32) -> Result<(usize, usize, usize)> {
        let key = key as usize;
        let dir_idx = key / SLOTS_PER_DIR;
        let l0i = dir_idx / PT_FANOUT;
        let l1i = dir_idx % PT_FANOUT;
        let si = key % SLOTS_PER_DIR;
        if l0i >= PT_FANOUT {
            return Err(GStoreError::Database(format!(
                "IvArray key {key} exceeds addressable capacity"
            )));
        }
        Ok((l0i, l1i, si))
    }

    /// Resolve the directory page id holding `key`, reading the page table.
    /// Returns `None` if any level is unallocated (so the key was never set).
    fn dir_page(&self, pager: &Pager, l0i: usize, l1i: usize) -> Result<Option<PageId>> {
        let l0 = pager.root(self.root_slot) as PageId;
        if l0 == 0 {
            return Ok(None);
        }
        let l0buf = pager.read_page(l0)?;
        let l1 = read_u32(&l0buf, l0i * 4);
        if l1 == 0 {
            return Ok(None);
        }
        let l1buf = pager.read_page(l1)?;
        let dir = read_u32(&l1buf, l1i * 4);
        if dir == 0 {
            return Ok(None);
        }
        Ok(Some(dir))
    }

    /// Look up the value for integer `key`. Read-only (`&Pager`), so it runs
    /// under a shared read guard concurrently with other readers.
    pub fn get(&self, pager: &Pager, key: u32) -> Result<Option<Vec<u8>>> {
        let (l0i, l1i, si) = Self::locate(key)?;
        let Some(dir) = self.dir_page(pager, l0i, l1i)? else {
            return Ok(None);
        };
        let dbuf = pager.read_page(dir)?;
        let off = si * SLOT_SIZE;
        match dbuf[off] {
            TAG_EMPTY => Ok(None),
            TAG_INLINE => {
                let len = read_u32(&dbuf, off + 1) as usize;
                Ok(Some(dbuf[off + 5..off + 5 + len].to_vec()))
            }
            TAG_OVERFLOW => {
                let len = read_u32(&dbuf, off + 1) as usize;
                let head = read_u32(&dbuf, off + 5);
                Ok(Some(overflow::read_chain(pager, head, len)?))
            }
            other => Err(GStoreError::Database(format!(
                "IvArray: corrupt slot tag {other} for key {key}"
            ))),
        }
    }

    /// Insert or replace `key → val`. Allocates page-table / directory pages on
    /// demand and frees any previous overflow chain the slot referenced.
    pub fn insert(&self, pager: &mut Pager, key: u32, val: &[u8]) -> Result<()> {
        if val.len() > u32::MAX as usize {
            return Err(GStoreError::Database("IvArray value too large".into()));
        }
        let (l0i, l1i, si) = Self::locate(key)?;

        // Ensure the L0 page-table page exists.
        let mut l0 = pager.root(self.root_slot) as PageId;
        if l0 == 0 {
            l0 = pager.alloc()?;
            pager.set_root(self.root_slot, l0 as u64);
        }
        // Ensure the L1 page-table page for this directory exists.
        let mut l0buf = pager.read_page(l0)?;
        let mut l1 = read_u32(&l0buf, l0i * 4);
        if l1 == 0 {
            l1 = pager.alloc()?;
            write_u32(&mut l0buf, l0i * 4, l1);
            pager.write_page(l0, &l0buf)?;
        }
        // Ensure the directory page exists.
        let mut l1buf = pager.read_page(l1)?;
        let mut dir = read_u32(&l1buf, l1i * 4);
        if dir == 0 {
            dir = pager.alloc()?;
            write_u32(&mut l1buf, l1i * 4, dir);
            pager.write_page(l1, &l1buf)?;
        }

        let dbuf = pager.read_page(dir)?;
        let off = si * SLOT_SIZE;
        // Reclaim a previous overflow chain before overwriting the slot. The head
        // is read out first; the directory page is re-read fresh below.
        if dbuf[off] == TAG_OVERFLOW {
            let old_head = read_u32(&dbuf, off + 5);
            overflow::free_chain(pager, old_head)?;
        }

        // Compose the new slot (inline if small, else an overflow chain head).
        let head = if val.len() > INLINE_CAP {
            Some(overflow::write_chain(pager, val)?)
        } else {
            None
        };
        // free_chain / write_chain may have evicted/written pages; re-read the
        // directory page so we extend the *current* copy rather than a stale one.
        let mut dbuf = pager.read_page(dir)?;
        let slot = &mut dbuf[off..off + SLOT_SIZE];
        slot.fill(0);
        write_u32(slot, 1, val.len() as u32);
        match head {
            None => {
                slot[0] = TAG_INLINE;
                slot[5..5 + val.len()].copy_from_slice(val);
            }
            Some(h) => {
                slot[0] = TAG_OVERFLOW;
                write_u32(slot, 5, h);
            }
        }
        pager.write_page(dir, &dbuf)?;
        Ok(())
    }

    /// Remove `key`'s value if present; returns `true` if it existed. Frees an
    /// overflow chain the slot referenced. The slot (and page-table/directory
    /// pages) stay allocated for reuse, mirroring gStore keeping the entry array.
    pub fn delete(&self, pager: &mut Pager, key: u32) -> Result<bool> {
        let (l0i, l1i, si) = Self::locate(key)?;
        let Some(dir) = self.dir_page(pager, l0i, l1i)? else {
            return Ok(false);
        };
        let mut dbuf = pager.read_page(dir)?;
        let off = si * SLOT_SIZE;
        match dbuf[off] {
            TAG_EMPTY => Ok(false),
            tag => {
                if tag == TAG_OVERFLOW {
                    let head = read_u32(&dbuf, off + 5);
                    overflow::free_chain(pager, head)?;
                    dbuf = pager.read_page(dir)?;
                }
                dbuf[off..off + SLOT_SIZE].fill(0);
                pager.write_page(dir, &dbuf)?;
                Ok(true)
            }
        }
    }
}

#[inline]
fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

#[inline]
fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvstore::pager::Pager;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gstore_ivarray_{tag}.kv"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("kv.wal"));
        p
    }

    #[test]
    fn insert_get_basic_and_missing() {
        let path = tmp("basic");
        let mut pg = Pager::open(&path, 64).unwrap();
        let a = IvArray::new(0);
        a.insert(&mut pg, 0, b"<alice>").unwrap();
        a.insert(&mut pg, 5, b"<bob>").unwrap();
        a.insert(&mut pg, 100, b"\"hello world\"").unwrap();
        assert_eq!(a.get(&pg, 0).unwrap().as_deref(), Some(&b"<alice>"[..]));
        assert_eq!(a.get(&pg, 5).unwrap().as_deref(), Some(&b"<bob>"[..]));
        assert_eq!(a.get(&pg, 100).unwrap().as_deref(), Some(&b"\"hello world\""[..]));
        // Unset keys (in and out of allocated directories) are None.
        assert_eq!(a.get(&pg, 1).unwrap(), None);
        assert_eq!(a.get(&pg, 999_999).unwrap(), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replace_value_and_inline_to_overflow() {
        let path = tmp("replace");
        let mut pg = Pager::open(&path, 64).unwrap();
        let a = IvArray::new(0);
        a.insert(&mut pg, 7, b"short").unwrap();
        assert_eq!(a.get(&pg, 7).unwrap().as_deref(), Some(&b"short"[..]));
        // Replace with a value that must overflow (> INLINE_CAP), then back to small.
        let big = vec![b'x'; INLINE_CAP * 4 + 17];
        a.insert(&mut pg, 7, &big).unwrap();
        assert_eq!(a.get(&pg, 7).unwrap().unwrap(), big);
        a.insert(&mut pg, 7, b"tiny").unwrap();
        assert_eq!(a.get(&pg, 7).unwrap().as_deref(), Some(&b"tiny"[..]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn large_value_roundtrips_via_overflow_chain() {
        let path = tmp("overflow");
        let mut pg = Pager::open(&path, 64).unwrap();
        let a = IvArray::new(0);
        // Several pages worth of value, with a recognizable pattern.
        let val: Vec<u8> = (0..overflow::CHAIN_PAYLOAD * 3 + 99)
            .map(|i| (i % 251) as u8)
            .collect();
        a.insert(&mut pg, 42, &val).unwrap();
        assert_eq!(a.get(&pg, 42).unwrap().unwrap(), val);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn dense_many_keys_roundtrip_across_directories() {
        let path = tmp("dense");
        let mut pg = Pager::open(&path, 256).unwrap();
        let a = IvArray::new(0);
        // Well past one directory page (32 slots) ⇒ exercises the page table.
        let n = 5000u32;
        for i in 0..n {
            a.insert(&mut pg, i, format!("<node{i}>").as_bytes()).unwrap();
        }
        for i in 0..n {
            assert_eq!(
                a.get(&pg, i).unwrap().unwrap(),
                format!("<node{i}>").into_bytes(),
                "key {i}"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn sparse_keys_do_not_allocate_dense_space() {
        // A handful of widely-spaced keys must not require allocating a page per
        // key in between (only their directories exist).
        let path = tmp("sparse");
        let mut pg = Pager::open(&path, 64).unwrap();
        let a = IvArray::new(0);
        for &k in &[0u32, 1000, 1_000_000, 5_000_000] {
            a.insert(&mut pg, k, format!("v{k}").as_bytes()).unwrap();
        }
        for &k in &[0u32, 1000, 1_000_000, 5_000_000] {
            assert_eq!(a.get(&pg, k).unwrap().unwrap(), format!("v{k}").into_bytes());
        }
        // Far fewer pages than 5M slots would need if it were truly dense.
        assert!(pg.page_count() < 64, "sparse keys allocated too many pages: {}", pg.page_count());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_clears_and_frees() {
        let path = tmp("delete");
        let mut pg = Pager::open(&path, 64).unwrap();
        let a = IvArray::new(0);
        a.insert(&mut pg, 3, &vec![b'z'; INLINE_CAP * 2]).unwrap(); // overflow
        a.insert(&mut pg, 4, b"keep").unwrap();
        assert!(a.delete(&mut pg, 3).unwrap());
        assert_eq!(a.get(&pg, 3).unwrap(), None);
        // deleting again is a no-op false
        assert!(!a.delete(&mut pg, 3).unwrap());
        // unrelated key survives
        assert_eq!(a.get(&pg, 4).unwrap().as_deref(), Some(&b"keep"[..]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn persists_across_reopen() {
        let path = tmp("persist");
        {
            let mut pg = Pager::open(&path, 64).unwrap();
            let a = IvArray::new(0);
            for i in 0..1000u32 {
                a.insert(&mut pg, i, format!("s{i}").as_bytes()).unwrap();
            }
            // A big one too.
            a.insert(&mut pg, 2000, &vec![7u8; INLINE_CAP * 3]).unwrap();
            pg.flush().unwrap();
        }
        let pg = Pager::open(&path, 64).unwrap();
        let a = IvArray::new(0);
        assert_eq!(a.get(&pg, 500).unwrap().unwrap(), b"s500".to_vec());
        assert_eq!(a.get(&pg, 2000).unwrap().unwrap(), vec![7u8; INLINE_CAP * 3]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn two_arrays_share_a_pager_via_distinct_root_slots() {
        // Distinct root slots ⇒ independent arrays in one file (as DiskStore uses
        // one slot per id-keyed store).
        let path = tmp("two");
        let mut pg = Pager::open(&path, 64).unwrap();
        let id2ent = IvArray::new(0);
        let id2pred = IvArray::new(1);
        id2ent.insert(&mut pg, 1, b"<entity1>").unwrap();
        id2pred.insert(&mut pg, 1, b"<pred1>").unwrap();
        assert_eq!(id2ent.get(&pg, 1).unwrap().as_deref(), Some(&b"<entity1>"[..]));
        assert_eq!(id2pred.get(&pg, 1).unwrap().as_deref(), Some(&b"<pred1>"[..]));
        std::fs::remove_file(&path).ok();
    }
}
