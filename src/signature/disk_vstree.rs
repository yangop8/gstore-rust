//! An **out-of-core** VS-tree: signature-tree nodes stored in paged disk blocks.
//!
//! The in-memory [`VsTree`](super::VsTree) holds the whole signature tree in RAM
//! (`bincode`-serialized to `vstree.bin`). This is the on-disk counterpart,
//! mirroring gStore's VSTree living in its own fixed-block file beneath the
//! buffer cache: each tree node is exactly one [`PAGE_SIZE`] pager page, so a
//! query-time candidate scan **reads only the nodes it actually traverses** —
//! the whole tree never has to be resident.
//!
//! ## Node page layout (one node per page)
//! ```text
//! [is_leaf: u8][count: u16 LE][node_sig: SIG_BYTES] then `count` entries of
//! [entry_sig: SIG_BYTES][value: u32 LE]
//! ```
//! For a **leaf**, `value` is an entity id and `entry_sig` is that entity's full
//! signature. For an **internal** node, `value` is a child page id and
//! `entry_sig` is the union signature of that child's subtree. Storing each
//! child's union *in the parent* lets the traversal prune a subtree without
//! reading the child page at all — the key to touching only traversed nodes.
//!
//! With `SIG_BYTES = 120` an entry is 124 bytes, so a 4 KiB page holds a header
//! plus [`DISK_FANOUT`] = 32 entries. The fan-out differs from the in-memory
//! tree's 64, but that only changes the tree *shape*, never the candidate set:
//! both return exactly `{ e : sig(e) ⊇ query }` because every leaf entry holds
//! the entity's exact signature and is tested exactly, and internal unions are
//! supersets, so pruning never drops a true match (see [`search`](DiskTree::search)).
//!
//! ## Soundness
//! A node is descended into only when its union signature contains the query, so
//! no true match is ever pruned; a leaf entry is reported only when the entity's
//! exact signature contains the query. The result is therefore a sound superset
//! of the true matches — identical (as a set) to the in-memory tree's, exactly
//! what the query engine's candidate filter requires.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Result;
use crate::kvstore::pager::{PageId, Pager, PAGE_SIZE};
use crate::model::id::EntityLiteralId;

use super::vstree::sort_entries;
use super::{SigFilter, Signature, SIG_BYTES};

/// Pager header slot holding the root node's page id (`0` = empty tree).
const ROOT_SLOT: usize = 0;
/// Pager header slot holding the entity count.
const COUNT_SLOT: usize = 1;

/// Page-cache size (in pages) for the VS-tree's pager (a path-only traversal
/// touches very few pages, so a modest cache more than covers the hot set).
const CACHE_PAGES: usize = 1024;

/// Byte offsets inside a node page.
const OFF_IS_LEAF: usize = 0;
const OFF_COUNT: usize = 1; // u16
const OFF_NODE_SIG: usize = 3;
const OFF_ENTRIES: usize = OFF_NODE_SIG + SIG_BYTES; // 123
/// Per-entry size: an entry signature plus a 4-byte id / child-page value.
const ENTRY_SIZE: usize = SIG_BYTES + 4; // 124

/// Max entries (leaf entities / internal children) that fit in one node page.
pub(super) const DISK_FANOUT: usize = (PAGE_SIZE - OFF_ENTRIES) / ENTRY_SIZE; // 32

/// Compile-time guarantee that a full node (header + node signature + entries)
/// fits within one page.
const _: () = assert!(OFF_ENTRIES + DISK_FANOUT * ENTRY_SIZE <= PAGE_SIZE);

/// An out-of-core signature tree backed by a dedicated paged file.
pub(super) struct DiskTree {
    /// Owns the VS-tree's paged file. Reads go through `read_page(&self)` (which
    /// only briefly latches the pager's own cache), so candidate scans run on
    /// `&self` and the type stays `Send + Sync`.
    pager: Pager,
    /// Root node page id (`0` = empty tree).
    root: PageId,
    entity_count: usize,
    /// Cumulative count of node pages read across all candidate scans, so a test
    /// can prove a query touched only a fraction of the tree (not fully resident).
    reads: AtomicU64,
}

impl std::fmt::Debug for DiskTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskTree")
            .field("root", &self.root)
            .field("entity_count", &self.entity_count)
            .field("pages_read", &self.reads.load(Ordering::Relaxed))
            .finish()
    }
}

impl DiskTree {
    /// Bulk-build a fresh out-of-core VS-tree at `path` from `(id, signature)`
    /// pairs, writing every node as a page and returning a read-ready handle.
    /// Entities are clustered by [`sort_entries`] (as the in-memory bulk build
    /// does) so similar signatures share leaves and pruning stays effective.
    pub(super) fn build(
        path: &Path,
        mut entries: Vec<(EntityLiteralId, Signature)>,
    ) -> Result<DiskTree> {
        // Cluster signature-similar entities so leaf unions stay tight.
        sort_entries(&mut entries);
        DiskTree::build_sorted(path, entries)
    }

    /// [`build`](Self::build) from entries already in clustering order.
    pub(super) fn build_sorted(
        path: &Path,
        entries: Vec<(EntityLiteralId, Signature)>,
    ) -> Result<DiskTree> {
        let entity_count = entries.len();
        let mut pager = Pager::open(path, CACHE_PAGES)?;

        if entries.is_empty() {
            pager.set_root(ROOT_SLOT, 0);
            pager.set_root(COUNT_SLOT, 0);
            pager.flush()?;
            return Ok(DiskTree {
                pager,
                root: 0,
                entity_count: 0,
                reads: AtomicU64::new(0),
            });
        }

        // Build leaves: each chunk of DISK_FANOUT entities becomes one leaf page.
        let mut level: Vec<(Signature, PageId)> = Vec::new();
        for chunk in entries.chunks(DISK_FANOUT) {
            let mut nsig = Signature::new();
            for (_, s) in chunk {
                nsig.union_with(s);
            }
            let pid = pager.alloc()?;
            let page = build_node(true, &nsig, chunk.iter().map(|(id, s)| (*s, *id)));
            pager.write_page(pid, &page)?;
            level.push((nsig, pid));
        }

        // Build internal levels bottom-up until a single root remains.
        while level.len() > 1 {
            let mut next: Vec<(Signature, PageId)> = Vec::new();
            for chunk in level.chunks(DISK_FANOUT) {
                let mut nsig = Signature::new();
                for (s, _) in chunk {
                    nsig.union_with(s);
                }
                let pid = pager.alloc()?;
                let page = build_node(false, &nsig, chunk.iter().map(|(s, cp)| (*s, *cp)));
                pager.write_page(pid, &page)?;
                next.push((nsig, pid));
            }
            level = next;
        }

        let root = level[0].1;
        pager.set_root(ROOT_SLOT, root as u64);
        pager.set_root(COUNT_SLOT, entity_count as u64);
        pager.flush()?;
        Ok(DiskTree {
            pager,
            root,
            entity_count,
            reads: AtomicU64::new(0),
        })
    }

    /// Open a previously-[`build`](Self::build)t out-of-core VS-tree at `path`.
    pub(super) fn open(path: &Path) -> Result<DiskTree> {
        let pager = Pager::open(path, CACHE_PAGES)?;
        let root = pager.root(ROOT_SLOT) as PageId;
        let entity_count = pager.root(COUNT_SLOT) as usize;
        Ok(DiskTree {
            pager,
            root,
            entity_count,
            reads: AtomicU64::new(0),
        })
    }

    pub(super) fn entity_count(&self) -> usize {
        self.entity_count
    }

    pub(super) fn is_empty(&self) -> bool {
        self.root == 0
    }

    /// Cumulative number of node pages read across all candidate scans so far.
    pub(super) fn pages_read(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Return every entity id whose signature contains `query`, reading only the
    /// nodes the traversal actually visits. `Ok(None)` for an empty tree (so the
    /// caller falls back to "no filtering"); a sound superset otherwise.
    pub(super) fn candidates(&self, query: &Signature) -> Result<Option<Vec<EntityLiteralId>>> {
        if self.root == 0 {
            return Ok(None);
        }
        let mut out = Vec::new();
        self.search(self.root, query, &mut out)?;
        Ok(Some(out))
    }

    fn search(
        &self,
        page: PageId,
        query: &Signature,
        out: &mut Vec<EntityLiteralId>,
    ) -> Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let buf = self.pager.read_page(page)?;
        let is_leaf = buf[OFF_IS_LEAF] != 0;
        let count = u16::from_le_bytes(buf[OFF_COUNT..OFF_COUNT + 2].try_into().unwrap()) as usize;
        let nsig = Signature::read_le(&buf[OFF_NODE_SIG..OFF_NODE_SIG + SIG_BYTES]);
        // Prune: a subtree can hold a match only if its union contains the query.
        if !nsig.contains(query) {
            return Ok(());
        }
        for i in 0..count {
            let off = OFF_ENTRIES + i * ENTRY_SIZE;
            let sig = Signature::read_le(&buf[off..off + SIG_BYTES]);
            let value = u32::from_le_bytes(
                buf[off + SIG_BYTES..off + SIG_BYTES + 4]
                    .try_into()
                    .unwrap(),
            );
            if !sig.contains(query) {
                continue;
            }
            if is_leaf {
                out.push(value); // value = entity id; its exact sig contains query
            } else {
                self.search(value, query, out)?; // value = child page id
            }
        }
        Ok(())
    }
}

/// The out-of-core VS-tree as a [`SigFilter`]: identical candidate semantics to
/// the in-memory tree, but reading only the nodes a query traverses from disk.
/// An I/O error during a scan degrades to `None` ("no filtering") — still
/// correct (unpruned, never a false negative); a healthy store never hits it.
impl SigFilter for DiskTree {
    fn candidates(&self, query: &Signature) -> Option<Vec<EntityLiteralId>> {
        DiskTree::candidates(self, query).unwrap_or(None)
    }

    fn entity_count(&self) -> usize {
        DiskTree::entity_count(self)
    }

    fn is_empty(&self) -> bool {
        DiskTree::is_empty(self)
    }

    fn pages_read(&self) -> u64 {
        DiskTree::pages_read(self)
    }
}

/// What [`write_sorted_stream`] wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct DiskTreeStats {
    /// Entities indexed.
    pub entities: u64,
    /// Pages of the file, the header included.
    pub pages: u64,
}

/// Write a `vstree.kv` at `path` from `entries` **already in clustering
/// order** (the order [`sort_entries`] gives: signature limbs, then id),
/// streaming: one leaf is in memory at a time plus the `(union, page)` pair
/// of each node of the level being built. The file is byte-identical to the
/// one [`DiskTree::build_sorted`] (and so [`DiskTree::build`]) writes for the
/// same entries — the same pages in the same order under the same version-1
/// header — but written directly, without the page cache and its WAL, then
/// `fsync`ed once. An existing file at `path` is replaced.
pub(super) fn write_sorted_stream<I>(path: &Path, entries: I) -> Result<DiskTreeStats>
where
    I: IntoIterator<Item = Result<(EntityLiteralId, Signature)>>,
{
    use crate::kvstore::format;
    use std::io::{BufWriter, Seek, SeekFrom, Write};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    w.seek(SeekFrom::Start(PAGE_SIZE as u64))?;
    let mut next: PageId = 1;
    let mut count = 0u64;
    // Leaves, as the entries stream in.
    let mut level: Vec<(Signature, PageId)> = Vec::new();
    let mut chunk: Vec<(Signature, u32)> = Vec::with_capacity(DISK_FANOUT);
    let mut emit_leaf = |chunk: &mut Vec<(Signature, u32)>,
                         w: &mut BufWriter<std::fs::File>,
                         next: &mut PageId|
     -> Result<()> {
        let mut nsig = Signature::new();
        for (s, _) in chunk.iter() {
            nsig.union_with(s);
        }
        let page = build_node(true, &nsig, chunk.iter().copied());
        w.write_all(&page)?;
        level.push((nsig, *next));
        *next += 1;
        chunk.clear();
        Ok(())
    };
    for e in entries {
        let (id, sig) = e?;
        chunk.push((sig, id));
        count += 1;
        if chunk.len() == DISK_FANOUT {
            emit_leaf(&mut chunk, &mut w, &mut next)?;
        }
    }
    if !chunk.is_empty() {
        emit_leaf(&mut chunk, &mut w, &mut next)?;
    }
    // Internal levels, bottom-up, until a single root remains.
    while level.len() > 1 {
        let mut up = Vec::with_capacity(level.len().div_ceil(DISK_FANOUT));
        for children in level.chunks(DISK_FANOUT) {
            let mut nsig = Signature::new();
            for (s, _) in children {
                nsig.union_with(s);
            }
            let page = build_node(false, &nsig, children.iter().copied());
            w.write_all(&page)?;
            up.push((nsig, next));
            next += 1;
        }
        level = up;
    }
    let root = level.first().map_or(0, |&(_, p)| p);
    // The header the pager writes for this tree: version 1, no free list,
    // root and entity count in their slots.
    let mut hdr = [0u8; PAGE_SIZE];
    hdr[0..8].copy_from_slice(format::magic_for(0));
    hdr[8..12].copy_from_slice(&next.to_le_bytes());
    let slot = |i: usize| 16 + i * 8;
    hdr[slot(ROOT_SLOT)..slot(ROOT_SLOT) + 8].copy_from_slice(&u64::from(root).to_le_bytes());
    hdr[slot(COUNT_SLOT)..slot(COUNT_SLOT) + 8].copy_from_slice(&count.to_le_bytes());
    w.seek(SeekFrom::Start(0))?;
    w.write_all(&hdr)?;
    w.flush()?;
    let file = w.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(DiskTreeStats {
        entities: count,
        pages: u64::from(next),
    })
}

impl super::VsTree {
    /// Write an out-of-core VS-tree file (`vstree.kv`) at `path` from
    /// `(entity id, signature)` entries **already in clustering order** —
    /// ascending by signature limbs, then id, the order the bulk builds sort
    /// into — streaming, without holding the entries in memory. The file is
    /// byte-identical to [`build_disk`](Self::build_disk) over the same
    /// entries; open it with [`open_disk`](Self::open_disk). An error from
    /// the iterator stops the write and is returned. Returns the number of
    /// entities written and the file's page count (header included).
    pub fn write_disk_sorted<P, I>(path: P, entries: I) -> Result<(u64, u64)>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = Result<(EntityLiteralId, Signature)>>,
    {
        let st = write_sorted_stream(path.as_ref(), entries)?;
        Ok((st.entities, st.pages))
    }
}

/// Serialize one node page from its kind, union signature, and `(entry_sig,
/// value)` entries (`value` = entity id for a leaf, child page id for internal).
fn build_node(
    is_leaf: bool,
    node_sig: &Signature,
    entries: impl ExactSizeIterator<Item = (Signature, u32)>,
) -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    page[OFF_IS_LEAF] = is_leaf as u8;
    let count = entries.len();
    page[OFF_COUNT..OFF_COUNT + 2].copy_from_slice(&(count as u16).to_le_bytes());
    node_sig.write_le(&mut page[OFF_NODE_SIG..OFF_NODE_SIG + SIG_BYTES]);
    for (i, (sig, value)) in entries.enumerate() {
        let off = OFF_ENTRIES + i * ENTRY_SIZE;
        sig.write_le(&mut page[off..off + SIG_BYTES]);
        page[off + SIG_BYTES..off + SIG_BYTES + 4].copy_from_slice(&value.to_le_bytes());
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::EdgeDir;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gstore_diskvs_{tag}.kv"));
        let _ = std::fs::remove_file(&p);
        let mut wal = p.clone().into_os_string();
        wal.push(".wal");
        let _ = std::fs::remove_file(std::path::PathBuf::from(wal));
        p
    }

    fn esig(edges: &[(u32, u32, EdgeDir)]) -> Signature {
        let mut s = Signature::new();
        for &(p, n, d) in edges {
            s.encode_edge(p, n, d);
        }
        s
    }

    #[test]
    fn fanout_is_as_expected() {
        // A full node fits in one page — checked at compile time by the `const _`
        // assertion above; this pins the resulting fan-out value.
        assert_eq!(DISK_FANOUT, 32);
    }

    #[test]
    fn empty_tree_returns_none() {
        let path = tmp("empty");
        let t = DiskTree::build(&path, Vec::new()).unwrap();
        assert!(t.is_empty());
        assert_eq!(t.entity_count(), 0);
        assert!(t.candidates(&Signature::new()).unwrap().is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn build_then_query_is_sound_and_persists() {
        let path = tmp("sound");
        let mut entries = Vec::new();
        for i in 0..400u32 {
            let mut edges = vec![(1u32, i, EdgeDir::Out)];
            if i == 123 {
                edges.push((2, 99, EdgeDir::Out));
            }
            entries.push((i, esig(&edges)));
        }
        let t = DiskTree::build(&path, entries).unwrap();
        assert_eq!(t.entity_count(), 400);

        let mut q = Signature::new();
        q.encode_query_edge(Some(2), Some(99), EdgeDir::Out);
        let cands = t.candidates(&q).unwrap().unwrap();
        assert!(cands.contains(&123), "true match must be a candidate");
        assert!(cands.len() < 400, "filter should prune");

        // Reopen: the persisted node pages still answer the same query.
        drop(t);
        let t2 = DiskTree::open(&path).unwrap();
        assert_eq!(t2.entity_count(), 400);
        let cands2 = t2.candidates(&q).unwrap().unwrap();
        assert!(cands2.contains(&123));
        std::fs::remove_file(&path).ok();
    }

    /// The streaming writer produces exactly the file `DiskTree::build`
    /// writes through the pager, for empty, one-leaf, exact-fan-out and
    /// multi-level trees.
    #[test]
    fn streaming_write_is_byte_identical_to_build() {
        for n in [0u32, 1, 31, 32, 33, 1024, 1025, 40_000] {
            let mut entries = Vec::new();
            for i in 0..n {
                let mut s = Signature::new();
                s.encode_edge(i % 11, i % 97, EdgeDir::Out);
                if i % 5 == 0 {
                    s.encode_edge(i % 3, i, EdgeDir::In);
                }
                entries.push((i * 3, s));
            }
            let a = tmp(&format!("stream_ref_{n}_{}", std::process::id()));
            let b = tmp(&format!("stream_new_{n}_{}", std::process::id()));
            drop(DiskTree::build(&a, entries.clone()).unwrap());
            let mut sorted = entries;
            sort_entries(&mut sorted);
            let (entities, pages) =
                super::super::VsTree::write_disk_sorted(&b, sorted.into_iter().map(Ok)).unwrap();
            assert_eq!(entities, u64::from(n));
            let (ra, rb) = (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
            assert_eq!(rb.len() as u64, pages * PAGE_SIZE as u64);
            assert!(ra == rb, "vstree.kv differs for {n} entities");
            let t = DiskTree::open(&b).unwrap();
            assert_eq!(t.entity_count(), n as usize);
            std::fs::remove_file(&a).ok();
            std::fs::remove_file(&b).ok();
        }
    }

    #[test]
    fn streaming_write_stops_at_an_iterator_error() {
        let p = tmp(&format!("stream_err_{}", std::process::id()));
        let items = vec![
            Ok((1u32, Signature::new())),
            Err(crate::error::GStoreError::Database("boom".into())),
        ];
        let e = super::super::VsTree::write_disk_sorted(&p, items).unwrap_err();
        assert!(e.to_string().contains("boom"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn query_reads_only_touched_nodes() {
        // A selective query must traverse far fewer pages than the whole tree
        // holds — proving the tree need not be fully resident.
        let path = tmp("touched");
        let n = 4000u32;
        let mut entries = Vec::new();
        for i in 0..n {
            // Distinctive edges per entity keep leaf unions sparse ⇒ good pruning.
            entries.push((i, esig(&[(1, i, EdgeDir::Out), (2, i % 50, EdgeDir::In)])));
        }
        let t = DiskTree::build(&path, entries).unwrap();
        let total_pages = t.pager.page_count();
        assert!(
            total_pages > 100,
            "expected a multi-page tree, got {total_pages}"
        );

        let mut q = Signature::new();
        q.encode_query_edge(Some(1), Some(2024), EdgeDir::Out);
        let before = t.pages_read();
        let _ = t.candidates(&q).unwrap().unwrap();
        let read = t.pages_read() - before;
        assert!(
            read < u64::from(total_pages),
            "candidate scan read {read} of {total_pages} pages (should be a fraction)"
        );
        std::fs::remove_file(&path).ok();
    }
}

#[cfg(test)]
mod sort_fix_tests {
    use super::*;
    use crate::signature::vstree::legacy;
    use crate::signature::EdgeDir;

    /// The sort fix leaves `vstree.kv` byte-identical: a build from entries in
    /// the legacy clustering order writes the same file as the new build.
    #[test]
    fn vstree_kv_bytes_are_unchanged_by_the_sort_fix() {
        let dir =
            std::env::temp_dir().join(format!("gstore_diskvs_sortfix_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut entries = Vec::new();
        for i in 0..3000u32 {
            let mut s = Signature::new();
            s.encode_edge(i % 5, i % 13, EdgeDir::Out);
            if i % 3 == 0 {
                s.encode_edge(i % 7, i % 4, EdgeDir::In);
            }
            entries.push((i, s));
        }
        let mut legacy_order = entries.clone();
        legacy_order.sort_by_key(|(_, s)| legacy::sig_key(s));
        let (a, b) = (dir.join("legacy.kv"), dir.join("new.kv"));
        drop(DiskTree::build_sorted(&a, legacy_order).unwrap());
        drop(DiskTree::build(&b, entries).unwrap());
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
