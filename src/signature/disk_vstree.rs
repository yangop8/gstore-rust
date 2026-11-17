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
//!
//! ## Validation (review disk#4)
//! The file is a derived index and its pages carry no checksum, so nothing it
//! holds is trusted. Every node a scan reads is checked: its kind byte is 0 or
//! 1, its entry count at most [`DISK_FANOUT`], every child id lies in
//! `1..page_count` and below the parent's id (both writers write children
//! before their parent, so a tree is acyclic by construction), its node
//! signature is the union of its entries' and equals the union its
//! parent's entry stores for it (both writers store exactly those unions),
//! every leaf lies at the same depth, no path is longer than [`MAX_DEPTH`]
//! nodes, and no scan visits more nodes than the file holds (in a tree
//! every node has one parent, so a node reached twice is corruption).
//! [`open`](DiskTree::open) checks the header (root id, entity count). A
//! violation is a [`GStoreError::Serialize`] error ("corrupt"); as a
//! [`SigFilter`] the tree then degrades to "no filtering", which is sound.
//! Without a page checksum some damage stays invisible: an entity id
//! changed in a leaf, or a bit lost from one leaf entry that another entry
//! of the same leaf also sets, can still hide a match.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{GStoreError, Result};
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

/// Longest root-to-leaf path (in nodes) a valid tree can have: entity ids are
/// `u32`, so a tree holds at most 2^32 entities, 2^27 full leaves and
/// `ceil(27 / 5) = 6` internal levels above them — 7 nodes. One level of
/// slack; a deeper path is corruption (review disk#4: a cyclic or chained
/// file used to recurse until the stack overflowed).
pub(super) const MAX_DEPTH: usize = 8;

/// Compile-time guarantee that a full node (header + node signature + entries)
/// fits within one page.
const _: () = assert!(OFF_ENTRIES + DISK_FANOUT * ENTRY_SIZE <= PAGE_SIZE);

/// An out-of-core signature tree backed by a dedicated paged file.
pub(super) struct DiskTree {
    /// Owns the VS-tree's paged file. Reads go through `read_page(&self)` (which
    /// only briefly latches the pager's own cache), so candidate scans run on
    /// `&self` and the type stays `Send + Sync`.
    pager: Pager,
    /// The file's path, for error messages.
    path: PathBuf,
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
    ///
    /// An existing file at `path` is replaced, with its write-ahead log
    /// (review disk#5: the pager used to open it and append the new tree
    /// after the old one, which leaked its pages, and a stale `<path>.wal`
    /// was replayed into the new file). The caller owns `path`: the old file
    /// is unlinked, so a handle another `DiskTree` holds on it keeps reading
    /// the old tree.
    pub(super) fn build_sorted(
        path: &Path,
        entries: Vec<(EntityLiteralId, Signature)>,
    ) -> Result<DiskTree> {
        let entity_count = entries.len();
        Pager::remove_with_wal(path)?;
        let mut pager = Pager::open(path, CACHE_PAGES)?;
        no_features(&pager, path)?;

        if entries.is_empty() {
            pager.set_root(ROOT_SLOT, 0);
            pager.set_root(COUNT_SLOT, 0);
            pager.flush()?;
            return Ok(DiskTree {
                pager,
                path: path.to_path_buf(),
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
            path: path.to_path_buf(),
            root,
            entity_count,
            reads: AtomicU64::new(0),
        })
    }

    /// Open a previously-[`build`](Self::build)t out-of-core VS-tree at `path`.
    /// A file using a pager feature bit is refused (see [`no_features`]).
    ///
    /// A missing or empty file is an error (review disk#15: the pager would
    /// create it and the caller got an empty tree instead of a failure), and
    /// so is a header whose root id lies outside the file or whose entity
    /// count no leaf layout of the file can hold (review disk#4).
    pub(super) fn open(path: &Path) -> Result<DiskTree> {
        let meta = std::fs::metadata(path).map_err(|e| {
            GStoreError::Io(std::io::Error::new(
                e.kind(),
                format!("cannot open VS-tree file '{}': {e}", path.display()),
            ))
        })?;
        if !meta.is_file() {
            return Err(GStoreError::Database(format!(
                "'{}' is not a VS-tree file: not a regular file",
                path.display()
            )));
        }
        if meta.len() == 0 {
            return Err(GStoreError::Serialize(format!(
                "VS-tree file '{}' is empty (corrupt; remove it to rebuild the index)",
                path.display()
            )));
        }
        let pager = Pager::open(path, CACHE_PAGES)?;
        no_features(&pager, path)?;
        let pages = u64::from(pager.page_count());
        let root = pager.root(ROOT_SLOT);
        let entity_count = pager.root(COUNT_SLOT);
        if root >= pages.max(1) {
            return Err(corrupt(
                path,
                &format!("its root node id {root} is outside its {pages} pages"),
            ));
        }
        if entity_count > pages.saturating_mul(DISK_FANOUT as u64) {
            return Err(corrupt(
                path,
                &format!("its entity count {entity_count} exceeds what {pages} pages can hold"),
            ));
        }
        Ok(DiskTree {
            pager,
            path: path.to_path_buf(),
            root: root as PageId,
            entity_count: entity_count as usize,
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
        let mut scan = Scan {
            leaf_depth: None,
            visited: 0,
            out: Vec::new(),
        };
        self.search(self.root, 1, None, query, &mut scan)?;
        Ok(Some(scan.out))
    }

    /// Visit node `page` at `depth` (the root is at 1); `expect` is the
    /// union signature its parent's entry stores for it (`None` for the
    /// root).
    fn search(
        &self,
        page: PageId,
        depth: usize,
        expect: Option<&Signature>,
        query: &Signature,
        scan: &mut Scan,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(self.corrupt_page(
                page,
                &format!("the path to it is longer than {MAX_DEPTH} nodes"),
            ));
        }
        // Every node but the header page has one parent, so a scan of a
        // tree visits each at most once. More visits mean some node is the
        // child of several entries: a damaged file whose scan could
        // otherwise grow exponentially with the depth (32 entries naming
        // one child, on each of 8 levels, is 32^8 visits).
        scan.visited += 1;
        if scan.visited >= u64::from(self.pager.page_count()) {
            return Err(self.corrupt_page(
                page,
                &format!(
                    "a node of a scan that visited more nodes than the file's {}",
                    self.pager.page_count().saturating_sub(1)
                ),
            ));
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        let buf = self.pager.read_page(page)?;
        let is_leaf = match buf[OFF_IS_LEAF] {
            0 => false,
            1 => true,
            other => return Err(self.corrupt_page(page, &format!("node kind byte {other}"))),
        };
        let count = u16::from_le_bytes([buf[OFF_COUNT], buf[OFF_COUNT + 1]]) as usize;
        if count > DISK_FANOUT {
            return Err(self.corrupt_page(
                page,
                &format!("{count} entries (at most {DISK_FANOUT} fit in a node)"),
            ));
        }
        if is_leaf && *scan.leaf_depth.get_or_insert(depth) != depth {
            return Err(self.corrupt_page(
                page,
                &format!(
                    "a leaf at depth {depth}, another at depth {}",
                    scan.leaf_depth.unwrap_or(depth)
                ),
            ));
        }
        let nsig = Signature::read_le(&buf[OFF_NODE_SIG..OFF_NODE_SIG + SIG_BYTES]);
        // The parent's entry stores this node's union: a child id redirected
        // to another node of the same depth would otherwise hide the
        // subtree it used to name (false negatives) and pass every check
        // above.
        if expect.is_some_and(|e| *e != nsig) {
            return Err(self.corrupt_page(
                page,
                "a node signature that differs from its parent's entry for it",
            ));
        }
        let entry_sig = |i: usize| {
            let off = OFF_ENTRIES + i * ENTRY_SIZE;
            Signature::read_le(&buf[off..off + SIG_BYTES])
        };
        // And the node signature is the union of the entries' (both writers
        // store exactly that): a bit lost from it would prune a subtree that
        // holds matches, and a bit flipped in an entry changes the union
        // unless another entry of the node has the same bit.
        let mut union = Signature::new();
        for i in 0..count {
            union.union_with(&entry_sig(i));
        }
        if union != nsig {
            return Err(self.corrupt_page(
                page,
                "a node signature that is not the union of its entries'",
            ));
        }
        // Prune: a subtree can hold a match only if its union contains the query.
        if !nsig.contains(query) {
            return Ok(());
        }
        for i in 0..count {
            let off = OFF_ENTRIES + i * ENTRY_SIZE;
            let sig = entry_sig(i);
            let at = off + SIG_BYTES;
            let value = u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
            if !sig.contains(query) {
                continue;
            }
            if is_leaf {
                scan.out.push(value); // value = entity id; its exact sig contains query
            } else {
                // value = child page id: never the header, inside the file,
                // and below its parent (children are written first).
                if value == 0 || value >= page || value >= self.pager.page_count() {
                    return Err(self.corrupt_page(
                        page,
                        &format!("child node id {value} (a child lies in 1..{page})"),
                    ));
                }
                self.search(value, depth + 1, Some(&sig), query, scan)?;
            }
        }
        Ok(())
    }

    /// The error for a malformed node `page` of this tree's file.
    fn corrupt_page(&self, page: PageId, what: &str) -> GStoreError {
        corrupt(&self.path, &format!("node page {page} holds {what}"))
    }
}

/// The state of one [`DiskTree::candidates`] scan.
struct Scan {
    /// The depth of the first leaf reached; every other leaf must lie there.
    leaf_depth: Option<usize>,
    /// Nodes visited so far (at most the file's node count in a tree).
    visited: u64,
    /// The candidates found so far.
    out: Vec<EntityLiteralId>,
}

/// A corruption error for the VS-tree file at `path` (review disk#4).
fn corrupt(path: &Path, what: &str) -> GStoreError {
    GStoreError::Serialize(format!(
        "VS-tree file '{}' is corrupt: {what} (remove it to rebuild the index)",
        path.display()
    ))
}

/// Refuse a `vstree.kv` whose pager header has feature bits: the format
/// registry (docs/PHASE6_PLAN.md §3.1) keeps `vstree.kv` at `FEATURES == 0`,
/// whatever bits the pager itself supports for `kvstore.kv` (the free-id list
/// of `kvstore.kv` is one), so such a file was not written as a VS-tree.
fn no_features(pager: &Pager, path: &Path) -> Result<()> {
    match pager.features() {
        0 => Ok(()),
        bits => Err(GStoreError::Database(format!(
            "{} is not a VS-tree file: it uses pager feature bits {bits:#x} (a VS-tree file has none)",
            path.display()
        ))),
    }
}

/// The out-of-core VS-tree as a [`SigFilter`]: identical candidate semantics to
/// the in-memory tree, but reading only the nodes a query traverses from disk.
/// An I/O error or a corrupt node (review disk#4) during a scan degrades to
/// `None` ("no filtering") — still correct (unpruned, never a false
/// negative); a healthy store never hits it.
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
/// `fsync`ed once. An existing file at `path` is replaced, with its
/// write-ahead log, as [`DiskTree::build_sorted`] replaces it (round-4 review
/// of disk#5: this writer truncated the file in place and left `<path>.wal`,
/// whose stale batch the next open replayed into the new file).
pub(super) fn write_sorted_stream<I>(path: &Path, entries: I) -> Result<DiskTreeStats>
where
    I: IntoIterator<Item = Result<(EntityLiteralId, Signature)>>,
{
    use crate::kvstore::format;
    use std::io::{BufWriter, Seek, SeekFrom, Write};

    Pager::remove_with_wal(path)?;
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

    /// Set feature bit `bit` in the header of the pager file at `path`, with
    /// the version-2 magic the registry pairs it with (a consistent header).
    fn set_feature_bit(path: &Path, bit: u64) {
        use crate::kvstore::format;
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut hdr = [0u8; PAGE_SIZE];
        f.read_exact(&mut hdr).unwrap();
        hdr[0..8].copy_from_slice(format::MAGIC_V2);
        let off = 16 + format::FEATURES * 8;
        let bits = u64::from_le_bytes(hdr[off..off + 8].try_into().unwrap()) | bit;
        hdr[off..off + 8].copy_from_slice(&bits.to_le_bytes());
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&hdr).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn a_vstree_file_with_feature_bits_is_refused() {
        use crate::kvstore::format;
        // Every defined bit, the one kvstore.kv supports from Round 4 on
        // (`FREE_IDS`) included: whether or not the pager accepts the bit,
        // a VS-tree file must not carry it (format registry: FEATURES == 0).
        for bit in [
            format::FEATURE_FREE_IDS,
            format::FEATURE_IVARRAY_WIDE,
            format::FEATURE_LONG_KEYS,
        ] {
            let path = tmp(&format!("features_{bit}_{}", std::process::id()));
            // Removed also when an assertion fails.
            struct Rm(std::path::PathBuf);
            impl Drop for Rm {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                    let mut wal = self.0.clone().into_os_string();
                    wal.push(".wal");
                    let _ = std::fs::remove_file(std::path::PathBuf::from(wal));
                }
            }
            let _rm = Rm(path.clone());
            let mut sig = Signature::new();
            sig.encode_edge(1, 2, EdgeDir::Out);
            DiskTree::build(&path, vec![(2, sig)]).unwrap();
            assert!(DiskTree::open(&path).is_ok(), "a plain file opens");
            set_feature_bit(&path, bit);
            assert!(DiskTree::open(&path).is_err(), "bit {bit:#x} accepted");
            assert!(super::super::VsTree::open_disk(&path).is_err());
            // review disk#5: a build replaces the file instead of opening it
            // (it used to be refused here because the pager kept the old
            // header and its bit), so the rebuilt file is a plain VS-tree.
            drop(DiskTree::build(&path, vec![(2, sig)]).unwrap());
            assert!(
                DiskTree::open(&path).is_ok(),
                "bit {bit:#x} survived a rebuild"
            );
        }
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
mod validation_tests {
    //! review disk#4 / disk#5 / disk#15: a damaged `vstree.kv` is an error,
    //! never a panic or a stack overflow; a build replaces the file; opening
    //! never creates one.
    use super::*;
    use crate::signature::EdgeDir;

    /// A scratch directory removed on drop (also when an assertion fails).
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir()
                .join(format!("gstore_diskvs_valid_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `n` entities whose signatures all contain the empty query.
    fn entries(n: u32) -> Vec<(EntityLiteralId, Signature)> {
        (0..n)
            .map(|i| {
                let mut s = Signature::new();
                s.encode_edge(i % 7, i, EdgeDir::Out);
                (i, s)
            })
            .collect()
    }

    /// Rewrite node page `page` of the file at `path` with `f`.
    fn patch_page(path: &Path, page: u32, f: impl FnOnce(&mut [u8])) {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let at = u64::from(page) * PAGE_SIZE as u64;
        let mut buf = vec![0u8; PAGE_SIZE];
        file.seek(SeekFrom::Start(at)).unwrap();
        file.read_exact(&mut buf).unwrap();
        f(&mut buf);
        file.seek(SeekFrom::Start(at)).unwrap();
        file.write_all(&buf).unwrap();
        file.sync_all().unwrap();
    }

    /// Set the child id of entry `i` of an internal node page.
    fn set_child(buf: &mut [u8], i: usize, child: u32) {
        let at = OFF_ENTRIES + i * ENTRY_SIZE + SIG_BYTES;
        buf[at..at + 4].copy_from_slice(&child.to_le_bytes());
    }

    /// A two-level tree of 40 entities: leaves on pages 1 and 2, the root
    /// (internal, two children) on page 3. Returns its file.
    fn two_level(tmp: &Tmp, name: &str) -> PathBuf {
        let path = tmp.path(name);
        let t = DiskTree::build(&path, entries(40)).unwrap();
        assert_eq!((t.root, t.pager.page_count()), (3, 4));
        path
    }

    /// Open the (patched) file and scan it with the empty query, which
    /// every node contains: the error, or the candidates.
    fn scan(path: &Path) -> Result<Option<Vec<EntityLiteralId>>> {
        DiskTree::open(path)?.candidates(&Signature::new())
    }

    fn assert_corrupt(r: Result<Option<Vec<EntityLiteralId>>>, what: &str) {
        match r {
            Err(e @ GStoreError::Serialize(_)) => {
                assert!(e.to_string().contains("corrupt"), "{what}: {e}")
            }
            other => panic!("{what}: expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn an_intact_tree_passes_validation() {
        let tmp = Tmp::new("intact");
        let path = two_level(&tmp, "t1.kv");
        let mut got = scan(&path).unwrap().unwrap();
        got.sort_unstable();
        assert_eq!(got, (0..40).collect::<Vec<_>>());
        // A tree deep enough to have three levels validates too.
        let deep = tmp.path("deep.kv");
        drop(DiskTree::build(&deep, entries(32 * 32 + 1)).unwrap());
        assert_eq!(scan(&deep).unwrap().unwrap().len(), 32 * 32 + 1);
        // And the file of the streaming (bulk-build) writer.
        let streamed = tmp.path("streamed.kv");
        let mut sorted = entries(32 * 32 + 1);
        sort_entries(&mut sorted);
        write_sorted_stream(&streamed, sorted.into_iter().map(Ok)).unwrap();
        assert_eq!(scan(&streamed).unwrap().unwrap().len(), 32 * 32 + 1);
    }

    #[test]
    fn an_entry_count_above_the_fanout_is_an_error() {
        let tmp = Tmp::new("count");
        for page in [1u32, 3] {
            let path = two_level(&tmp, &format!("count{page}.kv"));
            patch_page(&path, page, |b| {
                b[OFF_COUNT..OFF_COUNT + 2].copy_from_slice(&40u16.to_le_bytes())
            });
            assert_corrupt(scan(&path), &format!("count 40 on page {page}"));
        }
    }

    #[test]
    fn a_child_id_of_the_header_page_is_an_error() {
        let tmp = Tmp::new("child0");
        let path = two_level(&tmp, "t3.kv");
        patch_page(&path, 3, |b| set_child(b, 0, 0));
        assert_corrupt(scan(&path), "child 0");
    }

    #[test]
    fn a_child_id_past_the_file_is_an_error() {
        let tmp = Tmp::new("childpast");
        let path = two_level(&tmp, "t4.kv");
        patch_page(&path, 3, |b| set_child(b, 1, 1000));
        assert_corrupt(scan(&path), "child 1000");
    }

    #[test]
    fn a_cycle_is_an_error_not_a_stack_overflow() {
        let tmp = Tmp::new("cycle");
        // The root names itself.
        let path = two_level(&tmp, "t5.kv");
        patch_page(&path, 3, |b| set_child(b, 0, 3));
        assert_corrupt(scan(&path), "self cycle");
        // A one-leaf tree whose leaf turns into an internal node naming
        // itself (the review's reproduction: it aborted the process).
        let one = tmp.path("one.kv");
        drop(DiskTree::build(&one, entries(3)).unwrap());
        patch_page(&one, 1, |b| {
            b[OFF_IS_LEAF] = 0;
            set_child(b, 0, 1);
        });
        assert_corrupt(scan(&one), "leaf turned self cycle");
    }

    #[test]
    fn a_path_longer_than_the_maximum_depth_is_an_error() {
        // Build a chain: page 1 a leaf, pages 2..=k internal nodes with one
        // child each, the page below — acyclic, but deeper than any tree
        // of u32 entity ids can be.
        let tmp = Tmp::new("chain");
        let path = tmp.path("chain.kv");
        let k = (MAX_DEPTH + 2) as u32;
        {
            let mut pager = Pager::open(&path, 16).unwrap();
            let mut sig = Signature::new();
            sig.encode_edge(1, 1, EdgeDir::Out);
            for page in 1..=k {
                assert_eq!(pager.alloc().unwrap(), page);
                let node = if page == 1 {
                    build_node(true, &sig, std::iter::once((sig, 7u32)))
                } else {
                    build_node(false, &sig, std::iter::once((sig, page - 1)))
                };
                pager.write_page(page, &node).unwrap();
            }
            pager.set_root(ROOT_SLOT, u64::from(k));
            pager.set_root(COUNT_SLOT, 1);
            pager.flush().unwrap();
        }
        assert_corrupt(scan(&path), "chain");
    }

    #[test]
    fn leaves_at_different_depths_are_an_error() {
        // A three-level tree; the root's second child is redirected from an
        // internal node to a leaf one level too high.
        let tmp = Tmp::new("depth");
        let path = tmp.path("vstree.kv");
        let t = DiskTree::build(&path, entries(32 * 32 + 1)).unwrap();
        let root = t.root;
        drop(t);
        patch_page(&path, root, |b| set_child(b, 1, 1));
        assert_corrupt(scan(&path), "uneven leaves");
    }

    #[test]
    fn a_bad_node_kind_byte_is_an_error() {
        let tmp = Tmp::new("kind");
        let path = two_level(&tmp, "t6.kv");
        patch_page(&path, 1, |b| b[OFF_IS_LEAF] = 7);
        assert_corrupt(scan(&path), "kind 7");
    }

    #[test]
    fn a_bad_header_is_refused_at_open() {
        let tmp = Tmp::new("header");
        let path = two_level(&tmp, "t7.kv");
        let slot = |i: usize| 16 + i * 8;
        patch_page(&path, 0, |b| {
            b[slot(ROOT_SLOT)..slot(ROOT_SLOT) + 8].copy_from_slice(&99u64.to_le_bytes())
        });
        assert_corrupt(DiskTree::open(&path).map(|_| None), "root 99");
        let path = two_level(&tmp, "t8.kv");
        patch_page(&path, 0, |b| {
            b[slot(COUNT_SLOT)..slot(COUNT_SLOT) + 8].copy_from_slice(&u64::MAX.to_le_bytes())
        });
        assert_corrupt(DiskTree::open(&path).map(|_| None), "count u64::MAX");
    }

    #[test]
    fn a_corrupt_tree_filters_nothing_through_the_public_api() {
        // As a candidate filter the damaged tree degrades to "no filtering"
        // (`None`), which is sound, instead of panicking.
        let tmp = Tmp::new("public");
        let path = two_level(&tmp, "t9.kv");
        patch_page(&path, 3, |b| set_child(b, 0, 3));
        let tree = super::super::VsTree::open_disk(&path).unwrap();
        assert_eq!(tree.candidates(&Signature::new()), None);
        let mut q = Signature::new();
        q.encode_query_edge(Some(1), None, EdgeDir::Out);
        assert_eq!(tree.candidates(&q), None);
    }

    /// The union signature stored in node page `page` of the file at `path`.
    fn node_sig(path: &Path, page: u32) -> Signature {
        let bytes = std::fs::read(path).unwrap();
        let at = page as usize * PAGE_SIZE + OFF_NODE_SIG;
        Signature::read_le(&bytes[at..at + SIG_BYTES])
    }

    #[test]
    fn a_child_redirected_to_another_subtree_is_an_error_not_a_false_negative() {
        // The root's second entry names the first leaf instead of the
        // second: the id lies below the root, the leaf at the right depth,
        // so the other checks pass, and the scan used to answer the first
        // leaf's entities twice and none of the second leaf's (a false
        // negative, not the "no filtering" a damaged tree must give). The
        // parent's entry stores the second leaf's union, which differs.
        let tmp = Tmp::new("redirect");
        let path = two_level(&tmp, "t10.kv");
        assert_ne!(node_sig(&path, 1), node_sig(&path, 2), "precondition");
        patch_page(&path, 3, |b| set_child(b, 1, 1));
        assert_corrupt(scan(&path), "a child redirected to its sibling");
    }

    #[test]
    fn a_node_signature_that_is_not_the_union_of_its_entries_is_an_error() {
        // The root's signature loses every bit: a scan used to prune the
        // whole tree for any query with a bit (no candidates: false
        // negatives) and answer every entity for the empty query, without
        // noticing.
        let tmp = Tmp::new("nodesig");
        let path = two_level(&tmp, "t11.kv");
        patch_page(&path, 3, |b| {
            b[OFF_NODE_SIG..OFF_NODE_SIG + SIG_BYTES].fill(0);
        });
        assert_corrupt(scan(&path), "an empty root signature");
        // A leaf entry gains every bit: the leaf's signature no longer is
        // the union of its entries'.
        let path = two_level(&tmp, "t12.kv");
        patch_page(&path, 1, |b| {
            b[OFF_ENTRIES..OFF_ENTRIES + SIG_BYTES].fill(0xff);
        });
        assert_corrupt(scan(&path), "a leaf entry with every bit");
    }

    #[test]
    fn a_child_named_by_every_entry_is_an_error_not_an_exponential_scan() {
        // Pages 2..=4 are internal nodes whose 32 entries all name the page
        // below (with its own signature), page 1 a full leaf: acyclic, one
        // leaf depth, matching signatures — but the scan visits the leaf
        // 32^3 times. It used to answer 32^4 = 1,048,576 candidates from a
        // file of five pages; with 8 such levels it would be 32^8 (about
        // 10^12) and abort the process when the list cannot be allocated.
        let tmp = Tmp::new("dag");
        let path = tmp.path("dag.kv");
        {
            let mut pager = Pager::open(&path, 16).unwrap();
            let mut sig = Signature::new();
            sig.encode_edge(1, 1, EdgeDir::Out);
            for page in 1..=4u32 {
                assert_eq!(pager.alloc().unwrap(), page);
                let node = if page == 1 {
                    build_node(true, &sig, (0..DISK_FANOUT as u32).map(|e| (sig, e)))
                } else {
                    build_node(false, &sig, (0..DISK_FANOUT).map(|_| (sig, page - 1)))
                };
                pager.write_page(page, &node).unwrap();
            }
            pager.set_root(ROOT_SLOT, 4);
            pager.set_root(COUNT_SLOT, DISK_FANOUT as u64);
            pager.flush().unwrap();
        }
        match scan(&path) {
            Err(e @ GStoreError::Serialize(_)) => {
                assert!(e.to_string().contains("corrupt"), "{e}")
            }
            Err(e) => panic!("expected a corruption error, got {e}"),
            Ok(c) => panic!(
                "expected a corruption error, got {:?} candidates",
                c.map(|c| c.len())
            ),
        }
    }

    #[test]
    fn building_over_an_existing_file_replaces_it() {
        // review disk#5: three builds at one path used to grow the file
        // (671744, 1339392, 2007040 bytes for 5000 entities).
        let tmp = Tmp::new("rebuild");
        let path = tmp.path("vstree.kv");
        let mut sizes = Vec::new();
        for _ in 0..3 {
            drop(super::super::VsTree::build_disk(&path, entries(5000)).unwrap());
            sizes.push(std::fs::metadata(&path).unwrap().len());
        }
        assert!(sizes.iter().all(|&s| s == sizes[0]), "sizes {sizes:?}");
        // The rebuilt file is byte-identical to a fresh build elsewhere.
        let fresh = tmp.path("fresh.kv");
        drop(super::super::VsTree::build_disk(&fresh, entries(5000)).unwrap());
        assert!(std::fs::read(&path).unwrap() == std::fs::read(&fresh).unwrap());
        // A smaller tree over a larger one leaves no trace of it.
        drop(super::super::VsTree::build_disk(&path, entries(3)).unwrap());
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            2 * PAGE_SIZE as u64
        );
        let t = super::super::VsTree::open_disk(&path).unwrap();
        assert_eq!(t.entity_count(), 3);
    }

    #[test]
    fn building_drops_a_stale_write_ahead_log() {
        let tmp = Tmp::new("stalewal");
        let path = tmp.path("vstree.kv");
        let wal = Pager::wal_path(&path);
        std::fs::write(&wal, b"not a log of this file").unwrap();
        drop(super::super::VsTree::build_disk(&path, entries(10)).unwrap());
        assert!(!wal.exists(), "the stale log outlived the build");
        assert_eq!(
            super::super::VsTree::open_disk(&path)
                .unwrap()
                .entity_count(),
            10
        );
        // review disk#5 (reviewer): any open discards a torn log like the
        // one above, so it cannot show the replay. A complete log can: a
        // build of 5,000 entities whose log could not be removed leaves its
        // whole batch in `<path>.wal`; the file goes, the log stays. A build
        // at the path used to replay that batch into the new file and
        // append the new tree after it.
        {
            use crate::kvstore::pager::failpoints;
            let _fp = failpoints::fail_wal_delete();
            assert!(DiskTree::build(&path, entries(5000)).is_err());
        }
        assert!(wal.exists(), "the build left its complete log");
        std::fs::remove_file(&path).unwrap();
        drop(super::super::VsTree::build_disk(&path, entries(10)).unwrap());
        assert!(!wal.exists(), "the stale log outlived the build");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            2 * PAGE_SIZE as u64,
            "the stale batch was replayed into the new file"
        );
        assert_eq!(
            super::super::VsTree::open_disk(&path)
                .unwrap()
                .entity_count(),
            10
        );
        // Round-4 review of disk#5: the streaming writer behind the public
        // `VsTree::write_disk_sorted` truncated the file in place and left
        // the log beside it; the next open replayed the stale batch into the
        // new file (an empty tree).
        {
            use crate::kvstore::pager::failpoints;
            let _fp = failpoints::fail_wal_delete();
            assert!(DiskTree::build(&path, entries(5000)).is_err());
        }
        assert!(wal.exists(), "the build left its complete log");
        let mut sorted = entries(10);
        sort_entries(&mut sorted);
        let (n, pages) =
            super::super::VsTree::write_disk_sorted(&path, sorted.into_iter().map(Ok)).unwrap();
        assert_eq!((n, pages), (10, 2));
        assert!(!wal.exists(), "the stale log outlived the streaming write");
        assert_eq!(
            super::super::VsTree::open_disk(&path)
                .unwrap()
                .entity_count(),
            10,
            "the stale batch was replayed into the streamed file"
        );
    }

    #[test]
    fn opening_a_missing_or_empty_file_is_an_error_and_creates_nothing() {
        // review disk#15: the pager created the file, and the caller got an
        // empty tree (no filtering) instead of an error.
        let tmp = Tmp::new("missing");
        let path = tmp.path("vstree.kv");
        let e = super::super::VsTree::open_disk(&path).unwrap_err();
        assert!(
            matches!(&e, GStoreError::Io(io) if io.kind() == std::io::ErrorKind::NotFound),
            "{e}"
        );
        assert!(!path.exists(), "open_disk created the file");
        assert!(!Pager::wal_path(&path).exists());
        std::fs::write(&path, b"").unwrap();
        let e = super::super::VsTree::open_disk(&path).unwrap_err();
        assert!(e.to_string().contains("empty"), "{e}");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "the empty file was written"
        );
        // A directory is not a VS-tree file either.
        let e = super::super::VsTree::open_disk(&tmp.0).unwrap_err();
        assert!(e.to_string().contains("not a regular file"), "{e}");
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
