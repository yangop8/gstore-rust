//! VList **overflow chains**: store a byte payload too large for a single
//! B+tree value across a linked list of pager pages.
//!
//! This is the Rust counterpart of gStore's VList large-value file
//! (`src/Util/VList.cpp`). There, value lists that exceed `LENGTH_BORDER` are
//! kept out of the tree in a separate file of fixed `BLOCK_SIZE` blocks; each
//! block begins with a 4-byte *next-block* pointer (`ReadAlign`/`WriteAlign`
//! follow/create it at every block boundary) and the chain terminates with a
//! `0` pointer. A tree value then holds just the head block id, not the bytes.
//!
//! Here a "block" is one [`PAGE_SIZE`] pager page and a chain is a singly-linked
//! list of such pages: the first 4 bytes of each page are the little-endian
//! [`PageId`] of the next page (`0` = end of chain), and the remaining
//! [`CHAIN_PAYLOAD`] bytes carry payload. Pages come from (and are returned to)
//! the ordinary [`Pager`] free list, so overflow blocks share the same file,
//! page cache, and crash-safe [`Pager::flush`](super::pager::Pager::flush) as
//! every other page.
//!
//! The payload itself is opaque to this module — callers pass the already
//! delta+varint-encoded VList (see [`vlist`](super::vlist)) and store the total
//! byte length alongside the head id so [`read_chain`] knows where to stop.

use crate::error::Result;

use super::pager::{PageId, Pager, PAGE_SIZE};

/// Payload bytes per chain page (the first 4 bytes hold the next-page pointer).
pub const CHAIN_PAYLOAD: usize = PAGE_SIZE - 4;

/// Write `data` across a freshly allocated chain of pages and return the head
/// page id. Always allocates at least one page (so the head id is never the
/// null page `0`, even for an empty payload). Pages are linked head→tail.
pub fn write_chain(pager: &mut Pager, data: &[u8]) -> Result<PageId> {
    let npages = data.len().div_ceil(CHAIN_PAYLOAD).max(1);
    // Allocate every page first so each can be linked to its successor.
    let mut ids = Vec::with_capacity(npages);
    for _ in 0..npages {
        ids.push(pager.alloc()?);
    }
    for i in 0..npages {
        let mut page = [0u8; PAGE_SIZE];
        let next: PageId = if i + 1 < npages { ids[i + 1] } else { 0 };
        page[0..4].copy_from_slice(&next.to_le_bytes());
        let start = i * CHAIN_PAYLOAD;
        let end = ((i + 1) * CHAIN_PAYLOAD).min(data.len());
        if start < end {
            page[4..4 + (end - start)].copy_from_slice(&data[start..end]);
        }
        pager.write_page(ids[i], &page)?;
    }
    Ok(ids[0])
}

/// Read `len` payload bytes by following the chain from `head`. Stops at the
/// terminating `0` pointer or once `len` bytes have been collected, whichever
/// comes first (a well-formed chain reaches both at the same point). Read-only
/// (`&Pager`), so it runs under a shared read guard like other reads.
pub fn read_chain(pager: &Pager, head: PageId, len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut page_id = head;
    while page_id != 0 && out.len() < len {
        let page = pager.read_page(page_id)?;
        let next = u32::from_le_bytes(page[0..4].try_into().unwrap());
        let take = (len - out.len()).min(CHAIN_PAYLOAD);
        out.extend_from_slice(&page[4..4 + take]);
        page_id = next;
    }
    Ok(out)
}

/// Return every page of the chain rooted at `head` to the pager's free list.
/// The next-pointer is read before each page is freed, so the whole chain is
/// reclaimed even though `free` overwrites the page.
pub fn free_chain(pager: &mut Pager, head: PageId) -> Result<()> {
    let mut page_id = head;
    while page_id != 0 {
        let page = pager.read_page(page_id)?;
        let next = u32::from_le_bytes(page[0..4].try_into().unwrap());
        pager.free(page_id)?;
        page_id = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvstore::pager::Pager;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gstore_overflow_{tag}.kv"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("kv.wal"));
        p
    }

    #[test]
    fn roundtrip_spanning_many_pages() {
        let path = tmp("roundtrip");
        let mut pg = Pager::open(&path, 16).unwrap();
        // ~10 pages worth of payload with a recognizable byte pattern.
        let data: Vec<u8> = (0..CHAIN_PAYLOAD * 10 + 123)
            .map(|i| (i % 251) as u8)
            .collect();
        let head = write_chain(&mut pg, &data).unwrap();
        assert_ne!(head, 0);
        let back = read_chain(&pg, head, data.len()).unwrap();
        assert_eq!(back, data);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn single_page_payload() {
        let path = tmp("single");
        let mut pg = Pager::open(&path, 16).unwrap();
        let data = vec![7u8; 100];
        let head = write_chain(&mut pg, &data).unwrap();
        assert_eq!(read_chain(&pg, head, data.len()).unwrap(), data);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn free_chain_reclaims_pages() {
        let path = tmp("free");
        let mut pg = Pager::open(&path, 64).unwrap();
        let data = vec![1u8; CHAIN_PAYLOAD * 5];
        let head = write_chain(&mut pg, &data).unwrap();
        let before = pg.page_count();
        free_chain(&mut pg, head).unwrap();
        // Re-allocating the same number of pages reuses the freed ones rather
        // than growing the file.
        let mut reused = Vec::new();
        for _ in 0..5 {
            reused.push(pg.alloc().unwrap());
        }
        assert!(
            pg.page_count() <= before,
            "freed chain pages must be reused (before {before}, now {})",
            pg.page_count()
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn persists_across_reopen() {
        let path = tmp("persist");
        let data: Vec<u8> = (0..CHAIN_PAYLOAD * 3 + 7).map(|i| (i % 97) as u8).collect();
        let head;
        {
            let mut pg = Pager::open(&path, 16).unwrap();
            head = write_chain(&mut pg, &data).unwrap();
            pg.flush().unwrap();
        }
        let pg = Pager::open(&path, 16).unwrap();
        assert_eq!(read_chain(&pg, head, data.len()).unwrap(), data);
        std::fs::remove_file(&path).ok();
    }
}
