//! Page-image writers for a **version 1** `kvstore.kv`.
//!
//! The bulk build writes the file's pages directly — no page cache, no WAL —
//! into an unpublished staging file, then the header (page 0) last, then one
//! `fsync`. Every page is in the exact format the existing readers and
//! writers use ([`crate::kvstore`]: B+ tree nodes, IvArray page tables,
//! directories and overflow chains, the header of the
//! [`format`](crate::kvstore::format) registry), so the file opens with
//! `DiskStore::open` and stays mutable by the incremental writers.
//!
//! * **Fixed-key trees** (SPO / POS / OSP: 12-byte keys, GSPO: 16-byte keys,
//!   empty values) are written bottom-up while the sorted keys stream in:
//!   leaves at the fill factor (90% by default), the last two leaves
//!   rebalanced when the last one would be underfull, leaves linked, then
//!   the internal levels. Page counts are a function of the key count
//!   ([`fixed_tree_pages`]).
//! * **Variable-key trees** (`entity2id` / `literal2id` / `predicate2id`)
//!   are planned first (greedy packing by bytes, last node rebalanced) and
//!   then written.
//! * **IvArrays** (`id2*`, the per-predicate statistics) are written as
//!   directories (each followed by its values' overflow chains), then the
//!   level-1 page-table pages, then the level-0 page.
//!
//! A node counts as underfull below a quarter page, as for the incremental
//! B+ tree's delete rebalancing; the bulk writer never leaves a non-root node
//! underfull when its neighbour can share.

use std::fs::File;

use crate::error::{GStoreError, Result};
use crate::kvstore::format::{self, NROOTS};
use crate::kvstore::ivarray::{CAPACITY, INLINE_CAP, PT_FANOUT, SLOTS_PER_DIR, SLOT_SIZE};
use crate::kvstore::overflow::CHAIN_PAYLOAD;
use crate::kvstore::{PageId, PAGE_SIZE};

/// Bytes of a B+ tree node header: `is_leaf` (1) + `link` (4) + count (2).
pub(crate) const NODE_HEADER: usize = 7;
/// Bytes of entries a node holds.
pub(crate) const NODE_ROOM: usize = PAGE_SIZE - NODE_HEADER;
/// Entry bytes below which a node is underfull (the incremental tree's
/// `UNDERFLOW_BYTES` = a quarter page, header included).
pub(crate) const UNDERFULL_ROOM: usize = PAGE_SIZE / 4 - NODE_HEADER;
/// Keys an IvArray addresses (`0 .. CAPACITY`).
pub(crate) const IVARRAY_CAPACITY: usize = CAPACITY;

/// IvArray slot tags (`ivarray.rs`).
const TAG_INLINE: u8 = 1;
const TAG_OVERFLOW: u8 = 2;

/// Bytes written to the file in one `pwrite` at most.
const COALESCE_BYTES: usize = 1 << 20;

/// One page.
pub(crate) type Page = [u8; PAGE_SIZE];

/// Write `buf` at byte offset `off` of `file`.
fn write_all_at(file: &File, buf: &[u8], off: u64) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(buf, off)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = file.seek_write(&buf[done..], off + done as u64)?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into());
            }
            done += n;
        }
    }
    Ok(())
}

/// Writes pages at their ids, coalescing consecutive ones into one write.
/// Several `PageOut`s may share one file (each owns its page range).
pub(crate) struct PageOut<'f> {
    file: &'f File,
    start: PageId,
    buf: Vec<u8>,
}

impl<'f> PageOut<'f> {
    pub(crate) fn new(file: &'f File) -> PageOut<'f> {
        PageOut {
            file,
            start: 0,
            buf: Vec::with_capacity(COALESCE_BYTES),
        }
    }

    /// Write `page` as page `id`.
    pub(crate) fn put(&mut self, id: PageId, page: &Page) -> Result<()> {
        let held = (self.buf.len() / PAGE_SIZE) as u64;
        if held > 0
            && (u64::from(id) != u64::from(self.start) + held || self.buf.len() >= COALESCE_BYTES)
        {
            self.flush()?;
        }
        if self.buf.is_empty() {
            self.start = id;
        }
        self.buf.extend_from_slice(page);
        Ok(())
    }

    /// Write out what is buffered.
    pub(crate) fn flush(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            write_all_at(
                self.file,
                &self.buf,
                u64::from(self.start) * PAGE_SIZE as u64,
            )?;
            self.buf.clear();
        }
        Ok(())
    }
}

impl Drop for PageOut<'_> {
    fn drop(&mut self) {
        // Callers flush explicitly (and see the error); this only keeps a
        // forgotten flush from losing pages.
        let _ = self.flush();
    }
}

/// Hands out page ids from 1 (page 0 is the header).
#[derive(Debug)]
pub(crate) struct PageAlloc {
    next: PageId,
}

impl PageAlloc {
    pub(crate) fn new() -> PageAlloc {
        PageAlloc { next: 1 }
    }

    /// Reserve `n` consecutive pages; returns the first.
    pub(crate) fn reserve(&mut self, n: u64) -> Result<PageId> {
        let base = self.next;
        let next = u64::from(base) + n;
        self.next =
            u32::try_from(next).map_err(|_| GStoreError::Database("page count overflow".into()))?;
        Ok(base)
    }

    /// The next free page id (= the page count so far).
    pub(crate) fn next(&self) -> PageId {
        self.next
    }
}

/// The fill target in entry bytes for `fill_percent`.
pub(crate) fn fill_target(fill_percent: u8) -> usize {
    NODE_ROOM * usize::from(fill_percent.clamp(50, 100)) / 100
}

fn put_u16(p: &mut [u8], off: usize, v: usize) {
    p[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes());
}

fn put_u32(p: &mut [u8], off: usize, v: u32) {
    p[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Start a node page: `[is_leaf][link][count]`.
fn node_header(p: &mut Page, leaf: bool, link: PageId, count: usize) {
    p[0] = leaf as u8;
    put_u32(p, 1, link);
    put_u16(p, 5, count);
}

// ---- fixed-key trees ---------------------------------------------------------

/// The node shape of a fixed-key tree.
#[derive(Clone, Copy, Debug)]
struct FixedShape {
    klen: usize,
    /// Entries per leaf at the fill target.
    per_leaf: usize,
    /// Fewest entries of a leaf that is not underfull.
    min_leaf: usize,
    /// Children per internal node at the fill target.
    per_node: usize,
    /// Fewest children of an internal node that is not underfull.
    min_node: usize,
}

impl FixedShape {
    fn new(klen: usize, fill_percent: u8) -> FixedShape {
        let target = fill_target(fill_percent);
        let leaf_entry = 2 + klen + 2;
        let node_entry = 2 + klen + 4;
        let per_leaf = (target / leaf_entry).clamp(2, NODE_ROOM / leaf_entry);
        let per_node = (target / node_entry + 1).clamp(4, NODE_ROOM / node_entry + 1);
        FixedShape {
            klen,
            per_leaf,
            min_leaf: UNDERFULL_ROOM.div_ceil(leaf_entry).min(per_leaf / 2).max(1),
            per_node,
            min_node: (UNDERFULL_ROOM.div_ceil(node_entry) + 1)
                .min(per_node / 2)
                .max(2),
        }
    }
}

/// Group sizes for `n` items in groups of `per`, the last two rebalanced
/// when the last group would hold fewer than `min`.
fn fixed_groups(n: u64, per: usize, min: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let per64 = per as u64;
    let full = (n / per64) as usize;
    let rest = (n % per64) as usize;
    let mut g = vec![per; full];
    if rest > 0 {
        g.push(rest);
    }
    let k = g.len();
    if k >= 2 && g[k - 1] < min {
        let total = g[k - 2] + g[k - 1];
        g[k - 2] = total.div_ceil(2);
        g[k - 1] = total / 2;
    }
    g
}

/// Pages of a fixed-key tree of `n` keys of `klen` bytes.
pub(crate) fn fixed_tree_pages(n: u64, klen: usize, fill_percent: u8) -> u64 {
    let shape = FixedShape::new(klen, fill_percent);
    let mut level = fixed_groups(n, shape.per_leaf, shape.min_leaf).len() as u64;
    let mut pages = level;
    while level > 1 {
        level = fixed_groups(level, shape.per_node, shape.min_node).len() as u64;
        pages += level;
    }
    pages
}

/// Streams sorted, unique fixed-size keys into a B+ tree written bottom-up
/// from page `base` on (see the module docs).
pub(crate) struct FixedTreeWriter {
    shape: FixedShape,
    base: PageId,
    /// The previous leaf (full), held back until the next one is complete so
    /// the last two can be rebalanced.
    held: Option<Vec<u8>>,
    /// Keys of the current leaf.
    cur: Vec<u8>,
    /// Leaves written so far.
    written: u32,
    /// First key of every written leaf.
    firsts: Vec<u8>,
    keys: u64,
}

impl FixedTreeWriter {
    pub(crate) fn new(base: PageId, klen: usize, fill_percent: u8) -> FixedTreeWriter {
        let shape = FixedShape::new(klen, fill_percent);
        FixedTreeWriter {
            shape,
            base,
            held: None,
            cur: Vec::with_capacity(shape.per_leaf * klen),
            written: 0,
            firsts: Vec::new(),
            keys: 0,
        }
    }

    /// Keys pushed so far.
    pub(crate) fn keys(&self) -> u64 {
        self.keys
    }

    /// Append the next key (strictly greater than the previous one).
    #[inline]
    pub(crate) fn push(&mut self, out: &mut PageOut<'_>, key: &[u8]) -> Result<()> {
        debug_assert_eq!(key.len(), self.shape.klen);
        if self.cur.len() == self.shape.per_leaf * self.shape.klen {
            if let Some(h) = self.held.take() {
                self.write_leaf(out, &h, true)?;
            }
            let next = Vec::with_capacity(self.cur.capacity());
            self.held = Some(std::mem::replace(&mut self.cur, next));
        }
        self.cur.extend_from_slice(key);
        self.keys += 1;
        Ok(())
    }

    fn write_leaf(&mut self, out: &mut PageOut<'_>, keys: &[u8], has_next: bool) -> Result<()> {
        let klen = self.shape.klen;
        let id = self.base + self.written;
        let n = keys.len() / klen;
        let mut p = [0u8; PAGE_SIZE];
        node_header(&mut p, true, if has_next { id + 1 } else { 0 }, n);
        let mut off = NODE_HEADER;
        for k in keys.chunks_exact(klen) {
            put_u16(&mut p, off, klen);
            p[off + 2..off + 2 + klen].copy_from_slice(k);
            // value length 0
            off += 4 + klen;
        }
        out.put(id, &p)?;
        self.firsts.extend_from_slice(&keys[..klen]);
        self.written += 1;
        Ok(())
    }

    /// Write the remaining leaves and the internal levels; returns the root
    /// (`0` for an empty tree) and the pages used.
    pub(crate) fn finish(mut self, out: &mut PageOut<'_>) -> Result<(PageId, u64)> {
        let klen = self.shape.klen;
        let cur = std::mem::take(&mut self.cur);
        match self.held.take() {
            None if cur.is_empty() => return Ok((0, 0)),
            None => self.write_leaf(out, &cur, false)?,
            Some(h) => {
                let n_cur = cur.len() / klen;
                if n_cur < self.shape.min_leaf {
                    let mut all = h;
                    all.extend_from_slice(&cur);
                    let total = all.len() / klen;
                    let cut = total.div_ceil(2) * klen;
                    self.write_leaf(out, &all[..cut], true)?;
                    self.write_leaf(out, &all[cut..], false)?;
                } else {
                    self.write_leaf(out, &h, true)?;
                    self.write_leaf(out, &cur, false)?;
                }
            }
        }
        // Internal levels.
        let mut firsts = std::mem::take(&mut self.firsts);
        let mut ids: Vec<PageId> = (0..self.written).map(|i| self.base + i).collect();
        let mut next_id = self.base + self.written;
        while ids.len() > 1 {
            let groups = fixed_groups(ids.len() as u64, self.shape.per_node, self.shape.min_node);
            let mut up_firsts = Vec::with_capacity(groups.len() * klen);
            let mut up_ids = Vec::with_capacity(groups.len());
            let mut at = 0;
            for g in groups {
                let mut p = [0u8; PAGE_SIZE];
                node_header(&mut p, false, ids[at], g - 1);
                let mut off = NODE_HEADER;
                for c in at + 1..at + g {
                    put_u16(&mut p, off, klen);
                    p[off + 2..off + 2 + klen].copy_from_slice(&firsts[c * klen..(c + 1) * klen]);
                    put_u32(&mut p, off + 2 + klen, ids[c]);
                    off += 6 + klen;
                }
                out.put(next_id, &p)?;
                up_firsts.extend_from_slice(&firsts[at * klen..(at + 1) * klen]);
                up_ids.push(next_id);
                next_id += 1;
                at += g;
            }
            firsts = up_firsts;
            ids = up_ids;
        }
        let pages = u64::from(next_id - self.base);
        debug_assert_eq!(
            pages,
            fixed_tree_pages(self.keys, klen, fill_from_shape(&self.shape))
        );
        Ok((ids[0], pages))
    }
}

/// Recover a fill percentage that reproduces `shape` (debug check only).
fn fill_from_shape(shape: &FixedShape) -> u8 {
    (50..=100u8)
        .find(|&f| FixedShape::new(shape.klen, f).per_leaf == shape.per_leaf)
        .unwrap_or(90)
}

// ---- variable-key trees ------------------------------------------------------

/// Plan groups of consecutive items: greedy up to `target` bytes, at least
/// `min_items` per group, the last group merged with or rebalanced against
/// the previous one when it is underfull. `cost(i, first)` is item `i`'s
/// bytes, `first` when it opens a group. Returns each group's first item.
fn plan_groups(
    n: usize,
    target: usize,
    min_items: usize,
    cost: impl Fn(usize, bool) -> usize,
) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut bytes = 0usize;
    let mut items = 0usize;
    for i in 0..n {
        if items > 0 {
            let c = cost(i, false);
            if items >= min_items && bytes + c > target {
                starts.push(i);
                bytes = cost(i, true);
                items = 1;
            } else {
                bytes += c;
                items += 1;
            }
        } else {
            starts.push(i);
            bytes = cost(i, true);
            items = 1;
        }
    }
    let k = starts.len();
    if k >= 2 {
        let (a, b) = (starts[k - 2], starts[k - 1]);
        let last: usize = cost(b, true) + (b + 1..n).map(|i| cost(i, false)).sum::<usize>();
        if n - b < min_items || last < UNDERFULL_ROOM {
            // Merge the last two groups when they fit one node …
            let merged: usize = cost(a, true) + (a + 1..n).map(|i| cost(i, false)).sum::<usize>();
            if merged <= NODE_ROOM {
                starts.pop();
            } else {
                // … else cut them where both fit and are most even.
                let side = |from: usize, to: usize| -> usize {
                    cost(from, true) + (from + 1..to).map(|i| cost(i, false)).sum::<usize>()
                };
                let best = (a + min_items..=n - min_items)
                    .filter(|&j| side(a, j) <= NODE_ROOM && side(j, n) <= NODE_ROOM)
                    .min_by_key(|&j| side(a, j).abs_diff(side(j, n)));
                if let Some(j) = best {
                    starts[k - 1] = j;
                }
            }
        }
    }
    starts
}

/// The plan of a variable-key tree: per level, each node's first entry.
pub(crate) struct VarTreePlan {
    /// `levels[0]` = leaves (start entry of each), then internal levels
    /// (start child index into the level below).
    levels: Vec<Vec<usize>>,
    n: usize,
}

impl VarTreePlan {
    /// Plan a tree over `n` sorted entries whose keys are `klen(i)` bytes
    /// with `vlen`-byte values.
    pub(crate) fn new(
        n: usize,
        vlen: usize,
        fill_percent: u8,
        klen: impl Fn(usize) -> usize,
    ) -> VarTreePlan {
        let target = fill_target(fill_percent);
        let mut levels = Vec::new();
        if n == 0 {
            return VarTreePlan { levels, n };
        }
        let leaves = plan_groups(n, target, 1, |i, _| 2 + klen(i) + 2 + vlen);
        // First entry (global) of each node at the current level.
        let mut first_entry: Vec<usize> = leaves.clone();
        levels.push(leaves);
        while first_entry.len() > 1 {
            let m = first_entry.len();
            let fe = &first_entry;
            let groups = plan_groups(
                m,
                target,
                2,
                |c, first| {
                    if first {
                        0
                    } else {
                        2 + klen(fe[c]) + 4
                    }
                },
            );
            let up: Vec<usize> = groups.iter().map(|&g| first_entry[g]).collect();
            levels.push(groups);
            first_entry = up;
        }
        VarTreePlan { levels, n }
    }

    /// Pages the tree takes.
    pub(crate) fn pages(&self) -> u64 {
        self.levels.iter().map(|l| l.len() as u64).sum()
    }

    /// Write the tree from page `base` on; `key(i)` / `val(i)` are entry
    /// `i`'s bytes. Returns the root (`0` when empty).
    pub(crate) fn write<'k>(
        &self,
        out: &mut PageOut<'_>,
        base: PageId,
        key: impl Fn(usize) -> &'k [u8],
        val: impl Fn(usize) -> [u8; 4],
    ) -> Result<PageId> {
        if self.levels.is_empty() {
            return Ok(0);
        }
        let leaves = &self.levels[0];
        let mut id = base;
        // Leaves.
        for (j, &start) in leaves.iter().enumerate() {
            let end = leaves.get(j + 1).copied().unwrap_or(self.n);
            let last = j + 1 == leaves.len();
            let mut p = [0u8; PAGE_SIZE];
            node_header(&mut p, true, if last { 0 } else { id + 1 }, end - start);
            let mut off = NODE_HEADER;
            for i in start..end {
                let k = key(i);
                put_u16(&mut p, off, k.len());
                p[off + 2..off + 2 + k.len()].copy_from_slice(k);
                off += 2 + k.len();
                put_u16(&mut p, off, 4);
                p[off + 2..off + 6].copy_from_slice(&val(i));
                off += 6;
            }
            out.put(id, &p)?;
            id += 1;
        }
        // Internal levels: children are the nodes of the level below.
        let mut child_ids: Vec<PageId> = (0..leaves.len() as u32).map(|j| base + j).collect();
        let mut child_first: Vec<usize> = leaves.clone();
        for groups in &self.levels[1..] {
            let mut up_ids = Vec::with_capacity(groups.len());
            let mut up_first = Vec::with_capacity(groups.len());
            for (g, &start) in groups.iter().enumerate() {
                let end = groups.get(g + 1).copied().unwrap_or(child_ids.len());
                let mut p = [0u8; PAGE_SIZE];
                node_header(&mut p, false, child_ids[start], end - start - 1);
                let mut off = NODE_HEADER;
                for c in start + 1..end {
                    let k = key(child_first[c]);
                    put_u16(&mut p, off, k.len());
                    p[off + 2..off + 2 + k.len()].copy_from_slice(k);
                    off += 2 + k.len();
                    put_u32(&mut p, off, child_ids[c]);
                    off += 4;
                }
                out.put(id, &p)?;
                up_ids.push(id);
                up_first.push(child_first[start]);
                id += 1;
            }
            child_ids = up_ids;
            child_first = up_first;
        }
        debug_assert_eq!(u64::from(id - base), self.pages());
        Ok(child_ids[0])
    }
}

// ---- IvArrays ----------------------------------------------------------------

/// Overflow-chain pages of a value of `len` bytes (`overflow::write_chain`).
fn chain_pages(len: usize) -> u64 {
    if len > INLINE_CAP {
        len.div_ceil(CHAIN_PAYLOAD).max(1) as u64
    } else {
        0
    }
}

/// Pages of an IvArray holding values of `lens` (`(key, value length)`,
/// keys ascending).
pub(crate) fn ivarray_pages(lens: impl Iterator<Item = (u32, usize)>) -> u64 {
    let mut pages = 0u64;
    let mut dirs = 0u64;
    let mut last_dir = None;
    let mut l1s = 0u64;
    let mut last_l0 = None;
    for (key, len) in lens {
        let dir = key as usize / SLOTS_PER_DIR;
        if last_dir != Some(dir) {
            last_dir = Some(dir);
            dirs += 1;
            let l0 = dir / PT_FANOUT;
            if last_l0 != Some(l0) {
                last_l0 = Some(l0);
                l1s += 1;
            }
        }
        pages += chain_pages(len);
    }
    if dirs == 0 {
        return 0;
    }
    pages + dirs + l1s + 1
}

/// Write an IvArray of `entries` (`(key, value)`, keys strictly ascending
/// and below [`IVARRAY_CAPACITY`]) from page `base` on. Returns the root
/// (the level-0 page; `0` when empty) and the pages used.
pub(crate) fn write_ivarray<'v>(
    out: &mut PageOut<'_>,
    base: PageId,
    entries: impl Iterator<Item = (u32, &'v [u8])>,
) -> Result<(PageId, u64)> {
    let mut id = base;
    // (dir index, dir page) of every directory written.
    let mut dirs: Vec<(usize, PageId)> = Vec::new();
    let mut dir_vals: Vec<(usize, &'v [u8])> = Vec::with_capacity(SLOTS_PER_DIR);
    let mut cur_dir: Option<usize> = None;
    let mut flush_dir = |dir: usize,
                         vals: &mut Vec<(usize, &'v [u8])>,
                         id: &mut PageId,
                         out: &mut PageOut<'_>|
     -> Result<()> {
        let dir_page = *id;
        *id += 1;
        let mut p = [0u8; PAGE_SIZE];
        let mut chains: Vec<(PageId, &[u8])> = Vec::new();
        for &(slot, v) in vals.iter() {
            let off = slot * SLOT_SIZE;
            put_u32(&mut p, off + 1, v.len() as u32);
            if v.len() > INLINE_CAP {
                p[off] = TAG_OVERFLOW;
                put_u32(&mut p, off + 5, *id);
                chains.push((*id, v));
                *id += chain_pages(v.len()) as PageId;
            } else {
                p[off] = TAG_INLINE;
                p[off + 5..off + 5 + v.len()].copy_from_slice(v);
            }
        }
        out.put(dir_page, &p)?;
        for (head, v) in chains {
            let n = chain_pages(v.len()) as usize;
            for k in 0..n {
                let mut c = [0u8; PAGE_SIZE];
                let next = if k + 1 < n { head + k as u32 + 1 } else { 0 };
                put_u32(&mut c, 0, next);
                let from = k * CHAIN_PAYLOAD;
                let to = ((k + 1) * CHAIN_PAYLOAD).min(v.len());
                if from < to {
                    c[4..4 + (to - from)].copy_from_slice(&v[from..to]);
                }
                out.put(head + k as u32, &c)?;
            }
        }
        dirs.push((dir, dir_page));
        vals.clear();
        Ok(())
    };
    for (key, v) in entries {
        let key = key as usize;
        if key >= IVARRAY_CAPACITY {
            return Err(GStoreError::Database(format!(
                "IvArray key {key} exceeds addressable capacity"
            )));
        }
        let dir = key / SLOTS_PER_DIR;
        if cur_dir != Some(dir) {
            if let Some(d) = cur_dir {
                flush_dir(d, &mut dir_vals, &mut id, out)?;
            }
            cur_dir = Some(dir);
        }
        dir_vals.push((key % SLOTS_PER_DIR, v));
    }
    if let Some(d) = cur_dir {
        flush_dir(d, &mut dir_vals, &mut id, out)?;
    }
    if dirs.is_empty() {
        return Ok((0, 0));
    }
    // Level-1 page-table pages, then the level-0 page.
    let mut l1s: Vec<(usize, PageId)> = Vec::new();
    let mut i = 0;
    while i < dirs.len() {
        let l0i = dirs[i].0 / PT_FANOUT;
        let mut p = [0u8; PAGE_SIZE];
        while i < dirs.len() && dirs[i].0 / PT_FANOUT == l0i {
            put_u32(&mut p, (dirs[i].0 % PT_FANOUT) * 4, dirs[i].1);
            i += 1;
        }
        out.put(id, &p)?;
        l1s.push((l0i, id));
        id += 1;
    }
    let mut p = [0u8; PAGE_SIZE];
    for (l0i, l1) in l1s {
        put_u32(&mut p, l0i * 4, l1);
    }
    out.put(id, &p)?;
    let root = id;
    id += 1;
    Ok((root, u64::from(id - base)))
}

// ---- header and free list ----------------------------------------------------

/// The header page of a new file (the pager's `header_bytes`). A bulk image
/// is a *new* database, so — like a `DiskStore` created by `open` — it keeps
/// a free-id list when the process's id-reclamation policy is on
/// ([`crate::db::default_id_reclaim`]): the `FREE_IDS` feature bit and
/// `MAGIC_V2`, with the list empty (its root and the `FREE_COUNT_*` roots
/// are 0). With the policy off it is a version-1 file, as before.
pub(crate) fn header_page(page_count: PageId, free_head: PageId, roots: &[u64; NROOTS]) -> Page {
    let free_ids = crate::db::default_id_reclaim() == crate::db::IdReclaim::Immediate;
    header_page_with(page_count, free_head, roots, free_ids)
}

/// [`header_page`] with the free-id list chosen explicitly.
pub(crate) fn header_page_with(
    page_count: PageId,
    free_head: PageId,
    roots: &[u64; NROOTS],
    free_ids: bool,
) -> Page {
    debug_assert_eq!(
        roots[format::FEATURES],
        0,
        "the image writers set no feature themselves"
    );
    let mut roots = *roots;
    if free_ids {
        // An empty list: no tree, no free id in any space.
        roots[format::FEATURES] = format::FEATURE_FREE_IDS;
        roots[format::FREE_IDS] = 0;
        roots[format::FREE_COUNT_E] = 0;
        roots[format::FREE_COUNT_L] = 0;
        roots[format::FREE_COUNT_P] = 0;
    }
    let mut h = [0u8; PAGE_SIZE];
    h[0..8].copy_from_slice(format::magic_for(roots[format::FEATURES]));
    put_u32(&mut h, 8, page_count);
    put_u32(&mut h, 12, free_head);
    for (i, r) in roots.iter().enumerate() {
        h[16 + i * 8..24 + i * 8].copy_from_slice(&r.to_le_bytes());
    }
    h
}

/// Chain `pages` (unused, reserved pages) into a free list as
/// `Pager::free` would: each page's first 4 bytes name the next free page.
/// Returns the list head (`0` when there is none). (The bulk build sizes
/// every range exactly, so it has no slack today; the writer and its test
/// keep the free-list format of a bulk image pinned.)
#[cfg(test)]
pub(crate) fn write_free_list(out: &mut PageOut<'_>, pages: &[PageId]) -> Result<PageId> {
    let mut next = 0;
    for &id in pages.iter().rev() {
        let mut p = [0u8; PAGE_SIZE];
        put_u32(&mut p, 0, next);
        out.put(id, &p)?;
        next = id;
    }
    Ok(next)
}

/// Write the header as page 0.
pub(crate) fn write_header(file: &File, header: &Page) -> Result<()> {
    write_all_at(file, header, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvstore::bptree::BTree;
    use crate::kvstore::{IvArray, Pager};
    use std::ops::ControlFlow;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("gstore_kvimage_{tag}_{}.kv", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut w = p.clone().into_os_string();
        w.push(".wal");
        let _ = std::fs::remove_file(std::path::PathBuf::from(w));
        p
    }

    fn create(path: &std::path::Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap()
    }

    fn key3(a: u32, b: u32, c: u32) -> [u8; 12] {
        let mut k = [0u8; 12];
        k[0..4].copy_from_slice(&a.to_be_bytes());
        k[4..8].copy_from_slice(&b.to_be_bytes());
        k[8..12].copy_from_slice(&c.to_be_bytes());
        k
    }

    /// Fixed trees of every interesting size read back through `BTree`, with
    /// the page count [`fixed_tree_pages`] predicts, and stay writable.
    #[test]
    fn fixed_trees_read_back_and_stay_writable() {
        for (n, fill) in [
            (0u32, 90u8),
            (1, 90),
            (229, 90),
            (230, 90),
            (231, 90),
            (250, 90),
            (5000, 90),
            (60_000, 90),
            (60_000, 50),
            (60_000, 100),
        ] {
            let path = tmp(&format!("fixed_{n}_{fill}"));
            let file = create(&path);
            let mut alloc = PageAlloc::new();
            let pages = fixed_tree_pages(u64::from(n), 12, fill);
            let base = alloc.reserve(pages).unwrap();
            let mut out = PageOut::new(&file);
            let mut want: Vec<Vec<u8>> = (0..n).map(|i| key3(i / 100, i % 7, i).to_vec()).collect();
            want.sort();
            let mut w = FixedTreeWriter::new(base, 12, fill);
            for k in &want {
                w.push(&mut out, k).unwrap();
            }
            let (root, used) = w.finish(&mut out).unwrap();
            assert_eq!(used, pages, "n {n}");
            out.flush().unwrap();
            let mut roots = [0u64; NROOTS];
            roots[0] = u64::from(root);
            write_header(&file, &header_page(alloc.next(), 0, &roots)).unwrap();
            drop(out);
            drop(file);

            let mut pager = Pager::open(&path, 64).unwrap();
            let tree = BTree::with_fixed_shape(0, 12, 0);
            let mut got = Vec::new();
            for (k, v) in tree.iter_all(&pager).unwrap() {
                assert!(v.is_empty());
                got.push(k);
            }
            assert!(got == want, "n {n}: the tree reads back other keys");
            if n > 0 {
                assert_eq!(
                    tree.count_prefix(&pager, &(0u32).to_be_bytes()).unwrap(),
                    n.min(100) as usize
                );
                // Writable: delete half, insert new keys, all through BTree.
                for i in (0..n).step_by(2) {
                    assert!(tree.delete(&mut pager, &key3(i / 100, i % 7, i)).unwrap());
                }
                for i in 0..n / 2 {
                    tree.insert(&mut pager, &key3(1_000_000 + i, 0, 0), b"")
                        .unwrap();
                }
                pager.flush().unwrap();
                let left = tree.iter_all(&pager).unwrap().len();
                assert_eq!(left as u32, n - n.div_ceil(2) + n / 2);
            }
            drop(pager);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn var_trees_and_ivarrays_read_back() {
        let path = tmp("var");
        let file = create(&path);
        // Keys of mixed lengths, a few near the key limit.
        let mut keys: Vec<Vec<u8>> = (0..3000u32)
            .map(|i| {
                let len = match i % 97 {
                    0 => 2038,
                    1 => 1500,
                    _ => 10 + (i as usize * 31) % 200,
                };
                let mut k = format!("<k{i:05}>").into_bytes();
                k.resize(len, b'x');
                k
            })
            .collect();
        let vals: Vec<Vec<u8>> = keys.clone();
        keys.sort();
        let plan = VarTreePlan::new(keys.len(), 4, 90, |i| keys[i].len());
        let mut alloc = PageAlloc::new();
        let base = alloc.reserve(plan.pages()).unwrap();
        let mut out = PageOut::new(&file);
        let root = plan
            .write(&mut out, base, |i| &keys[i], |i| (i as u32).to_be_bytes())
            .unwrap();
        // A dense IvArray with inline and overflow values, and a sparse one.
        let lens = vals.iter().enumerate().map(|(i, v)| (i as u32, v.len()));
        let iv_pages = ivarray_pages(lens);
        let iv_base = alloc.reserve(iv_pages).unwrap();
        let (iv_root, used) = write_ivarray(
            &mut out,
            iv_base,
            vals.iter()
                .enumerate()
                .map(|(i, v)| (i as u32, v.as_slice())),
        )
        .unwrap();
        assert_eq!(used, iv_pages);
        let sparse: Vec<(u32, Vec<u8>)> = vec![
            (3, vec![1; 12]),
            (40_000, vec![2; 12]),
            (40_001, vec![3; 12]),
        ];
        let sp_pages = ivarray_pages(sparse.iter().map(|(k, v)| (*k, v.len())));
        let sp_base = alloc.reserve(sp_pages).unwrap();
        let (sp_root, used) = write_ivarray(
            &mut out,
            sp_base,
            sparse.iter().map(|(k, v)| (*k, v.as_slice())),
        )
        .unwrap();
        assert_eq!(used, sp_pages);
        // Three slack pages on the free list.
        let slack = alloc.reserve(3).unwrap();
        let free = write_free_list(&mut out, &[slack, slack + 1, slack + 2]).unwrap();
        out.flush().unwrap();
        let mut roots = [0u64; NROOTS];
        roots[3] = u64::from(root);
        roots[6] = u64::from(iv_root);
        roots[15] = u64::from(sp_root);
        write_header(&file, &header_page(alloc.next(), free, &roots)).unwrap();
        drop(out);
        drop(file);

        let mut pager = Pager::open(&path, 64).unwrap();
        let tree = BTree::new(3);
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(
                tree.get(&pager, k).unwrap(),
                Some((i as u32).to_be_bytes().to_vec())
            );
        }
        assert_eq!(tree.iter_all(&pager).unwrap().len(), keys.len());
        let iv = IvArray::new(6);
        for (i, v) in vals.iter().enumerate() {
            assert_eq!(
                iv.get(&pager, i as u32).unwrap().as_deref(),
                Some(v.as_slice())
            );
        }
        let mut seen = 0;
        iv.for_each_range(&pager, 0, vals.len() as u32, |k, v| {
            assert_eq!(v, vals[k as usize].as_slice());
            seen += 1;
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(seen, vals.len());
        let sp = IvArray::new(15);
        assert_eq!(sp.get(&pager, 40_000).unwrap(), Some(vec![2; 12]));
        assert_eq!(sp.get(&pager, 4).unwrap(), None);
        // The writers keep working, and reuse the free list first.
        let a = pager.alloc().unwrap();
        assert_eq!(a, slack);
        tree.insert(&mut pager, b"<new>", &7u32.to_be_bytes())
            .unwrap();
        tree.delete(&mut pager, &keys[0]).unwrap();
        iv.insert(&mut pager, 5, &vec![9u8; 5000]).unwrap();
        iv.delete(&mut pager, 7).unwrap();
        pager.flush().unwrap();
        assert_eq!(
            tree.get(&pager, b"<new>").unwrap(),
            Some(7u32.to_be_bytes().to_vec())
        );
        assert_eq!(iv.get(&pager, 5).unwrap(), Some(vec![9u8; 5000]));
        assert_eq!(iv.get(&pager, 7).unwrap(), None);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn group_planning_respects_limits() {
        // Leaves of 2046-byte entries: one or two per node, never more.
        let starts = plan_groups(7, 3680, 1, |_, _| 2046);
        assert!(starts.windows(2).all(|w| w[1] - w[0] <= 1));
        // A small last group is merged or rebalanced, never left underfull.
        assert_eq!(plan_groups(240, 3680, 1, |_, _| 16), vec![0]);
        assert_eq!(plan_groups(260, 3680, 1, |_, _| 16), vec![0, 130]);
        assert_eq!(plan_groups(300, 3680, 1, |_, _| 16), vec![0, 230]);
        // Internal nodes keep at least two children.
        let starts = plan_groups(3, 3680, 2, |_, first| if first { 0 } else { 2044 });
        assert_eq!(starts, vec![0]);
        assert_eq!(fixed_groups(231, 230, 64), vec![116, 115]);
        assert_eq!(fixed_groups(460, 230, 64), vec![230, 230]);
        assert!(fixed_groups(0, 230, 64).is_empty());
    }
}
