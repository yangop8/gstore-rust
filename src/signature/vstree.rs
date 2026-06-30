//! The VS-tree: a signature tree over entity signatures.
//!
//! gStore's VSTree is a height-balanced S-tree built by incremental insertion
//! with node splitting. This module supports both routes:
//!
//! * [`VsTree::build`] **bulk loads**: cluster signature-similar entities into
//!   leaves, then build internal levels bottom-up, each internal node holding the
//!   union of its children's signatures.
//! * [`VsTree::insert`] adds **one entity at a time** after the tree exists,
//!   matching gStore's online `VSTree` insertion: descend into the child whose
//!   signature grows least (minimal-enlargement, [`Signature::added_bits`]),
//!   place the entity in a leaf, and **split** any node that overflows
//!   [`FANOUT`] — seeding the split with the two most dissimilar members
//!   ([`Signature::distance`]) and giving each remaining member to the closer
//!   group. A root split grows the tree by one level. So triples inserted after
//!   the initial build update the index without a full rebuild.
//!
//! Correctness (soundness): a node's signature is the union (superset) of every
//! descendant entity's signature — maintained on every insert (union along the
//! descent path) and split (each half's signature is the exact union of its
//! members). Pruning a node where `node.sig ⊉ query` can therefore never discard
//! a true match, so the returned candidate set stays a superset of the true
//! matches — exactly what the engine needs as a filter, whether the tree was
//! bulk-built, incrementally inserted, or both.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::id::EntityLiteralId;

use super::disk_vstree::DiskTree;
use super::Signature;

/// Max entries per leaf / children per internal node (tree fan-out).
const FANOUT: usize = 64;

#[derive(Debug, Serialize, Deserialize)]
enum Node {
    Leaf {
        sig: Signature,
        /// (entity id, its signature)
        entries: Vec<(EntityLiteralId, Signature)>,
    },
    Internal {
        sig: Signature,
        children: Vec<Node>,
    },
}

impl Node {
    fn sig(&self) -> &Signature {
        match self {
            Node::Leaf { sig, .. } | Node::Internal { sig, .. } => sig,
        }
    }

    /// Insert `(id, sig)` into this subtree, unioning `sig` along the descent
    /// path so every node's signature stays a superset of its descendants.
    /// Returns `Some(right_sibling)` if this node overflowed [`FANOUT`] and split
    /// (the caller adopts the sibling); `None` otherwise.
    fn insert(&mut self, id: EntityLiteralId, sig: Signature) -> Option<Node> {
        match self {
            Node::Leaf {
                sig: nsig,
                entries,
            } => {
                entries.push((id, sig));
                nsig.union_with(&sig);
                if entries.len() <= FANOUT {
                    return None;
                }
            }
            Node::Internal {
                sig: nsig,
                children,
            } => {
                nsig.union_with(&sig);
                let ci = choose_child(children, &sig);
                match children[ci].insert(id, sig) {
                    None => return None,
                    Some(new_child) => {
                        // Keep the split-off sibling next to its origin.
                        children.insert(ci + 1, new_child);
                        if children.len() <= FANOUT {
                            return None;
                        }
                    }
                }
            }
        }
        Some(self.split())
    }

    /// Replace the stored signature of entity `id` (somewhere in this subtree)
    /// with `new_sig`, unioning `new_sig` into every node on the descent path so
    /// each node's signature stays a superset of its descendants. Returns `true`
    /// if `id` was found here. The first matching entry is updated (ids are
    /// unique in a tree built by [`VsTree::build`] / maintained by
    /// [`VsTree::update`]).
    fn update(&mut self, id: EntityLiteralId, new_sig: Signature) -> bool {
        match self {
            Node::Leaf { sig, entries } => {
                for (eid, esig) in entries.iter_mut() {
                    if *eid == id {
                        *esig = new_sig;
                        sig.union_with(&new_sig);
                        return true;
                    }
                }
                false
            }
            Node::Internal { sig, children } => {
                for child in children.iter_mut() {
                    if child.update(id, new_sig) {
                        sig.union_with(&new_sig);
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Split this overflowing node into two, keeping the left half in place and
    /// returning the right half. Both halves' signatures are recomputed as the
    /// exact union of their members, preserving the superset invariant.
    fn split(&mut self) -> Node {
        match self {
            Node::Leaf { sig, entries } => {
                let (left, lsig, right, rsig) =
                    seed_split(std::mem::take(entries), |(_, s)| *s);
                *entries = left;
                *sig = lsig;
                Node::Leaf {
                    sig: rsig,
                    entries: right,
                }
            }
            Node::Internal { sig, children } => {
                let (left, lsig, right, rsig) =
                    seed_split(std::mem::take(children), |c| *c.sig());
                *children = left;
                *sig = lsig;
                Node::Internal {
                    sig: rsig,
                    children: right,
                }
            }
        }
    }
}

/// Pick the child whose signature grows least when `sig` is unioned into it
/// (minimal enlargement); ties go to the child with the smaller signature, which
/// keeps node signatures tight and pruning effective.
fn choose_child(children: &[Node], sig: &Signature) -> usize {
    let mut best = 0;
    let mut best_added = u32::MAX;
    let mut best_pop = u32::MAX;
    for (i, c) in children.iter().enumerate() {
        let added = c.sig().added_bits(sig);
        let pop = c.sig().popcount();
        if added < best_added || (added == best_added && pop < best_pop) {
            best = i;
            best_added = added;
            best_pop = pop;
        }
    }
    best
}

/// Partition `items` into two groups for a node split. Seeds are the two most
/// dissimilar members (max [`Signature::distance`]); each remaining member joins
/// whichever group its signature enlarges less. Returns each group with its
/// union signature. This mirrors gStore's VSTree split seed selection.
fn seed_split<T>(
    items: Vec<T>,
    sig_of: impl Fn(&T) -> Signature,
) -> (Vec<T>, Signature, Vec<T>, Signature) {
    let n = items.len();
    let sigs: Vec<Signature> = items.iter().map(&sig_of).collect();

    // Seeds: the pair with the greatest Hamming distance.
    let (mut si, mut sj, mut best) = (0usize, 1usize, 0u32);
    for i in 0..n {
        for j in (i + 1)..n {
            let d = sigs[i].distance(&sigs[j]);
            if d >= best {
                best = d;
                si = i;
                sj = j;
            }
        }
    }

    let mut left_idx = vec![si];
    let mut right_idx = vec![sj];
    let mut lsig = sigs[si];
    let mut rsig = sigs[sj];
    for (idx, sigi) in sigs.iter().enumerate() {
        if idx == si || idx == sj {
            continue;
        }
        let la = lsig.added_bits(sigi);
        let ra = rsig.added_bits(sigi);
        if la < ra || (la == ra && left_idx.len() <= right_idx.len()) {
            lsig.union_with(sigi);
            left_idx.push(idx);
        } else {
            rsig.union_with(sigi);
            right_idx.push(idx);
        }
    }

    // Move items into their groups (each taken exactly once).
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    let left: Vec<T> = left_idx.iter().map(|&i| slots[i].take().unwrap()).collect();
    let right: Vec<T> = right_idx.iter().map(|&i| slots[i].take().unwrap()).collect();
    (left, lsig, right, rsig)
}

/// A signature tree over entity signatures.
///
/// By default the tree is fully in memory (`root`), and `bincode`-serializes to
/// `vstree.bin` for the in-memory database path. It can instead be **out-of-core**
/// (`disk`): the nodes then live in a dedicated paged file and a candidate scan
/// reads only the nodes it traverses (see [`DiskTree`]). The `disk` backing is
/// `#[serde(skip)]`, so the serialized form is byte-identical to the in-memory
/// tree's — `save`/`load` are unaffected — and is reconstructed from its own file
/// via [`open_disk`](Self::open_disk).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VsTree {
    root: Option<Box<Node>>,
    entity_count: usize,
    /// When set, the tree is backed by an on-disk paged node file instead of
    /// `root`; all reads ([`candidates`](Self::candidates)) go through it.
    #[serde(skip)]
    disk: Option<DiskTree>,
}

impl VsTree {
    pub fn new() -> VsTree {
        VsTree::default()
    }

    pub fn entity_count(&self) -> usize {
        match &self.disk {
            Some(d) => d.entity_count(),
            None => self.entity_count,
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.disk {
            Some(d) => d.is_empty(),
            None => self.root.is_none(),
        }
    }

    /// Build a VS-tree from `(entity_id, signature)` pairs.
    pub fn build(mut entries: Vec<(EntityLiteralId, Signature)>) -> VsTree {
        let entity_count = entries.len();
        if entries.is_empty() {
            return VsTree::new();
        }

        // Cluster signature-similar entities by sorting on the raw signature
        // limbs; adjacent entries then share high-order bits, which keeps leaf
        // union signatures tighter and improves pruning. (gStore achieves this
        // via split heuristics; bulk sort is a simpler route to the same end.)
        entries.sort_by_key(|(_, s)| sig_key(s));

        // Build leaves.
        let mut level: Vec<Node> = entries
            .chunks(FANOUT)
            .map(|chunk| {
                let mut sig = Signature::new();
                for (_, s) in chunk {
                    sig.union_with(s);
                }
                Node::Leaf {
                    sig,
                    entries: chunk.to_vec(),
                }
            })
            .collect();

        // Build internal levels until a single root remains.
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(FANOUT));
            let mut iter = level.into_iter();
            loop {
                let children: Vec<Node> = iter.by_ref().take(FANOUT).collect();
                if children.is_empty() {
                    break;
                }
                let mut sig = Signature::new();
                for c in &children {
                    sig.union_with(c.sig());
                }
                next.push(Node::Internal { sig, children });
            }
            level = next;
        }

        VsTree {
            root: Some(Box::new(level.pop().unwrap())),
            entity_count,
            disk: None,
        }
    }

    /// Bulk-build an **out-of-core** VS-tree at `path` from `(entity_id, signature)`
    /// pairs: the nodes are written to a dedicated paged file and never held whole
    /// in memory. The returned tree filters via the on-disk nodes (see
    /// [`candidates`](Self::candidates)), reading only the nodes a query traverses.
    /// Equivalent (as a candidate filter) to [`build`](Self::build) over the same
    /// entries — the result set is identical; only the tree shape and residency
    /// differ. An existing file at `path` is overwritten.
    pub fn build_disk<P: AsRef<Path>>(
        path: P,
        entries: Vec<(EntityLiteralId, Signature)>,
    ) -> Result<VsTree> {
        let disk = DiskTree::build(path.as_ref(), entries)?;
        Ok(VsTree {
            root: None,
            entity_count: 0,
            disk: Some(disk),
        })
    }

    /// Open an existing out-of-core VS-tree previously written by
    /// [`build_disk`](Self::build_disk).
    pub fn open_disk<P: AsRef<Path>>(path: P) -> Result<VsTree> {
        let disk = DiskTree::open(path.as_ref())?;
        Ok(VsTree {
            root: None,
            entity_count: 0,
            disk: Some(disk),
        })
    }

    /// Whether this tree is backed by an on-disk paged node file (out-of-core)
    /// rather than an in-memory `root`. Such a tree is read-only here: incremental
    /// [`insert`](Self::insert)/[`update`](Self::update) target the in-memory
    /// representation, so a disk-backed database invalidates and rebuilds its
    /// index on mutation instead (see `Database::insert_triple`).
    pub fn is_disk_backed(&self) -> bool {
        self.disk.is_some()
    }

    /// Cumulative number of on-disk node pages read across candidate scans (0 for
    /// an in-memory tree). A value far below the tree's page count demonstrates
    /// that a query touches only the nodes it traverses (not the whole tree).
    pub fn disk_pages_read(&self) -> u64 {
        self.disk.as_ref().map_or(0, DiskTree::pages_read)
    }

    /// Insert one `(entity_id, signature)` after the tree exists, splitting any
    /// node that overflows [`FANOUT`] and growing a new root on a root split.
    /// Equivalent in effect to having included the entity in [`build`](Self::build):
    /// the candidate filter stays sound (see the module docs). Callers must use
    /// unique ids (as [`build`](Self::build) assumes); re-inserting an id leaves a
    /// duplicate entry, which a candidate scan would simply report twice.
    pub fn insert(&mut self, id: EntityLiteralId, sig: Signature) {
        match self.root.take() {
            None => {
                self.root = Some(Box::new(Node::Leaf {
                    sig,
                    entries: vec![(id, sig)],
                }));
            }
            Some(mut root) => {
                if let Some(sibling) = root.insert(id, sig) {
                    // Root split: a fresh internal root over the two halves.
                    let mut s = *root.sig();
                    s.union_with(sibling.sig());
                    self.root = Some(Box::new(Node::Internal {
                        sig: s,
                        children: vec![*root, sibling],
                    }));
                } else {
                    self.root = Some(root);
                }
            }
        }
        self.entity_count += 1;
    }

    /// Insert many `(entity_id, signature)` pairs one by one (see [`insert`](Self::insert)).
    pub fn insert_all(&mut self, entries: impl IntoIterator<Item = (EntityLiteralId, Signature)>) {
        for (id, sig) in entries {
            self.insert(id, sig);
        }
    }

    /// Replace the stored signature of an existing entity with `new_sig` — the
    /// entity's *current* full signature, recomputed from the store after a
    /// mutation added or removed one of its incident edges. Returns `true` if
    /// `id` was present (and updated), `false` if it is not in the tree (the
    /// caller may then [`insert`](Self::insert) it).
    ///
    /// This is the incremental-maintenance counterpart of a full rebuild: rather
    /// than re-signing every entity and rebuilding the tree on each mutation, the
    /// one affected entity's leaf entry is overwritten and the new bits unioned
    /// up its path.
    ///
    /// Soundness: the target leaf entry is set to *exactly* `new_sig`, while every
    /// ancestor only ever *gains* bits (a union). So the tree invariant — each
    /// node's signature ⊇ the union of its descendant entity signatures — is
    /// preserved whether `new_sig` grew (edge added) or shrank (edge removed)
    /// relative to the old entry. The candidate filter therefore stays a sound
    /// superset of the true matches (never a false negative); a shrink merely
    /// leaves slightly looser ancestor unions, costing only some pruning power.
    pub fn update(&mut self, id: EntityLiteralId, new_sig: Signature) -> bool {
        match self.root.as_mut() {
            None => false,
            Some(root) => root.update(id, new_sig),
        }
    }

    /// Height of the tree (0 = empty, 1 = a single leaf). For tests asserting
    /// that incremental insertion actually splits and grows the tree.
    #[cfg(test)]
    fn height(&self) -> usize {
        fn depth(node: &Node) -> usize {
            match node {
                Node::Leaf { .. } => 1,
                Node::Internal { children, .. } => {
                    1 + children.iter().map(depth).max().unwrap_or(0)
                }
            }
        }
        self.root.as_ref().map_or(0, |r| depth(r))
    }

    /// Return all entity ids whose signature contains `query` (a superset of
    /// the true matches). If the tree is empty, returns `None` so the caller
    /// can fall back to "no filtering".
    pub fn candidates(&self, query: &Signature) -> Option<Vec<EntityLiteralId>> {
        if let Some(disk) = &self.disk {
            // An I/O error degrades to "no filtering" (`None`): the engine then
            // scans without pruning — slower but still correct (never a false
            // negative). A healthy store does not hit this path.
            return disk.candidates(query).unwrap_or(None);
        }
        let root = self.root.as_ref()?;
        let mut out = Vec::new();
        Self::search(root, query, &mut out);
        Some(out)
    }

    fn search(node: &Node, query: &Signature, out: &mut Vec<EntityLiteralId>) {
        // Prune: a subtree can hold a match only if its union contains the query.
        if !node.sig().contains(query) {
            return;
        }
        match node {
            Node::Leaf { entries, .. } => {
                for (id, sig) in entries {
                    if sig.contains(query) {
                        out.push(*id);
                    }
                }
            }
            Node::Internal { children, .. } => {
                for child in children {
                    Self::search(child, query, out);
                }
            }
        }
    }
}

/// Sort key clustering similar signatures together (raw limbs, high bits first).
/// Shared with the out-of-core builder so both bulk routes cluster identically.
pub(super) fn sig_key(sig: &Signature) -> Vec<u64> {
    // `Signature` exposes `test`; reconstruct limb words for ordering.
    let mut key = vec![0u64; super::LIMBS];
    for (i, word) in key.iter_mut().enumerate() {
        for b in 0..64 {
            let bit = i * 64 + b;
            if bit < super::ENTITY_SIG_LENGTH && sig.test(bit) {
                *word |= 1u64 << b;
            }
        }
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::EdgeDir;

    /// Make an entity signature from a list of (pred, neighbor, dir) edges.
    fn esig(edges: &[(u32, u32, EdgeDir)]) -> Signature {
        let mut s = Signature::new();
        for &(p, n, d) in edges {
            s.encode_edge(p, n, d);
        }
        s
    }

    #[test]
    fn empty_tree_returns_none() {
        let t = VsTree::new();
        assert!(t.is_empty());
        assert!(t.candidates(&Signature::new()).is_none());
    }

    #[test]
    fn candidates_are_a_sound_superset() {
        // Entities 0..200, each with a distinct out-edge (pred 1 -> neighbor i),
        // plus entity i==42 also has edge (pred 2 -> neighbor 99).
        let mut entries = Vec::new();
        for i in 0..200u32 {
            let mut edges = vec![(1u32, i, EdgeDir::Out)];
            if i == 42 {
                edges.push((2, 99, EdgeDir::Out));
            }
            entries.push((i, esig(&edges)));
        }
        let tree = VsTree::build(entries);
        assert_eq!(tree.entity_count(), 200);

        // Query for "has out-edge pred 2 -> 99" must include entity 42.
        let mut q = Signature::new();
        q.encode_query_edge(Some(2), Some(99), EdgeDir::Out);
        let cands = tree.candidates(&q).unwrap();
        assert!(cands.contains(&42), "true match 42 must be a candidate");
        // And it must be a strict filter (far fewer than all 200).
        assert!(cands.len() < 200, "filter should prune: {}", cands.len());
    }

    #[test]
    fn empty_query_matches_everything() {
        let entries: Vec<_> = (0..50u32)
            .map(|i| (i, esig(&[(1, i, EdgeDir::Out)])))
            .collect();
        let tree = VsTree::build(entries);
        // An all-zero query is contained by every signature.
        let cands = tree.candidates(&Signature::new()).unwrap();
        assert_eq!(cands.len(), 50);
    }

    // ---- incremental insertion (task 2) -----------------------------------

    #[test]
    fn pure_incremental_insert_is_sound_and_filters() {
        // Build the whole tree by one-at-a-time insertion (no bulk build), which
        // forces leaf and internal splits, then verify the candidate filter.
        let n = 600u32;
        let mut all: Vec<(u32, Signature)> = Vec::new();
        let mut tree = VsTree::new();
        for i in 0..n {
            let sig = esig(&[(1, i, EdgeDir::Out), ((i % 5) + 2, i % 40, EdgeDir::In)]);
            all.push((i, sig));
            tree.insert(i, sig);
        }
        assert_eq!(tree.entity_count(), n as usize);
        // Splitting must have grown the tree past a single leaf.
        assert!(tree.height() > 1, "incremental inserts should split the tree");

        // Soundness: for several queries the candidate set is a superset of the
        // brute-force matches over every inserted entity. Each query is a real
        // edge of some entity (entity 7's & 500's out-edge `(1, i)`, entity 6's
        // in-edge `((6%5)+2, 6%40) = (3, 6)`), so a true match is guaranteed.
        for &(pred, nb, dir) in &[
            (1u32, 7u32, EdgeDir::Out),
            (3, 6, EdgeDir::In),
            (1, 500, EdgeDir::Out),
        ] {
            let mut q = Signature::new();
            q.encode_query_edge(Some(pred), Some(nb), dir);
            let cands: std::collections::HashSet<u32> =
                tree.candidates(&q).unwrap().into_iter().collect();
            let mut truth = 0;
            for (id, sig) in &all {
                if sig.contains(&q) {
                    truth += 1;
                    assert!(cands.contains(id), "entity {id} missing from candidates");
                }
            }
            // The filter is selective: it prunes far below the full set.
            assert!(truth >= 1, "query should have at least one true match");
            assert!(cands.len() < n as usize, "filter should prune some entities");
        }
    }

    #[test]
    fn build_then_insert_no_false_negatives() {
        // Bulk-build a base set, then add many more incrementally; queries must
        // still find true matches among *both* the built and inserted entities.
        let n_base = 80u32;
        let n_more = 420u32;
        let mut all: Vec<(u32, Signature)> = Vec::new();
        for i in 0..n_base {
            let sig = esig(&[(i % 7, i, EdgeDir::Out)]);
            all.push((i, sig));
        }
        let mut tree = VsTree::build(all.clone());
        assert_eq!(tree.entity_count(), n_base as usize);

        for i in n_base..(n_base + n_more) {
            // A distinctive edge on one inserted entity we can later query for.
            let mut edges = vec![(i % 7, i, EdgeDir::Out)];
            if i == 300 {
                edges.push((11, 9999, EdgeDir::In));
            }
            let sig = esig(&edges);
            all.push((i, sig));
            tree.insert(i, sig);
        }
        assert_eq!(tree.entity_count(), (n_base + n_more) as usize);

        // The distinctive inserted entity must be a candidate.
        let mut q = Signature::new();
        q.encode_query_edge(Some(11), Some(9999), EdgeDir::In);
        let cands = tree.candidates(&q).unwrap();
        assert!(cands.contains(&300), "inserted true match 300 must be a candidate");
        assert!(cands.len() < (n_base + n_more) as usize, "filter should prune");

        // Full soundness sweep over a few queries.
        for &(pred, nb, dir) in &[(3u32, 250u32, EdgeDir::Out), (0, 5, EdgeDir::Out)] {
            let mut q = Signature::new();
            q.encode_query_edge(Some(pred), Some(nb), dir);
            let cands: std::collections::HashSet<u32> =
                tree.candidates(&q).unwrap().into_iter().collect();
            for (id, sig) in &all {
                if sig.contains(&q) {
                    assert!(cands.contains(id), "entity {id} missing after incremental insert");
                }
            }
        }
    }

    #[test]
    fn update_grows_signature_and_stays_sound() {
        // Build a tree, then grow one entity's signature with a new edge via
        // `update`; the new edge must become findable and the entity count must
        // not change (update is in-place, not an insert).
        let mut tree = VsTree::new();
        for i in 0..120u32 {
            tree.insert(i, esig(&[(1, i, EdgeDir::Out)]));
        }
        assert_eq!(tree.entity_count(), 120);

        // Updating an absent entity reports "not found".
        assert!(!tree.update(9999, esig(&[(7, 7, EdgeDir::Out)])));

        // Grow entity 42 to also have edge (2 -> 99).
        let grown = esig(&[(1, 42, EdgeDir::Out), (2, 99, EdgeDir::Out)]);
        assert!(tree.update(42, grown));
        assert_eq!(tree.entity_count(), 120, "update must not change the count");

        let mut q = Signature::new();
        q.encode_query_edge(Some(2), Some(99), EdgeDir::Out);
        assert!(
            tree.candidates(&q).unwrap().contains(&42),
            "the freshly-added edge must be findable after update"
        );
    }

    #[test]
    fn update_shrinks_signature_without_false_negatives() {
        // Every entity starts with two edges; shrink one entity to drop an edge.
        // The shrunk entity must fall out of that edge's candidate set (the leaf
        // entry is checked exactly), while every other entity is still found.
        let mut tree = VsTree::new();
        for i in 0..60u32 {
            tree.insert(i, esig(&[(1, i, EdgeDir::Out), (2, 99, EdgeDir::Out)]));
        }
        assert!(tree.update(7, esig(&[(1, 7, EdgeDir::Out)])));

        let mut q = Signature::new();
        q.encode_query_edge(Some(2), Some(99), EdgeDir::Out);
        let cands: std::collections::HashSet<u32> =
            tree.candidates(&q).unwrap().into_iter().collect();
        assert!(!cands.contains(&7), "shrunk entity drops out of the candidate set");
        for i in 0..60u32 {
            if i != 7 {
                assert!(cands.contains(&i), "entity {i} must remain a candidate");
            }
        }
    }

    #[test]
    fn insert_into_empty_tree_then_query() {
        let mut tree = VsTree::new();
        assert!(tree.is_empty());
        tree.insert(7, esig(&[(2, 99, EdgeDir::Out)]));
        assert!(!tree.is_empty());
        assert_eq!(tree.entity_count(), 1);
        let mut q = Signature::new();
        q.encode_query_edge(Some(2), Some(99), EdgeDir::Out);
        assert_eq!(tree.candidates(&q).unwrap(), vec![7]);
    }

    #[test]
    fn candidates_never_miss_true_matches_random() {
        // Build a graph-ish set and verify, for several queries, that the
        // candidate set is a superset of a brute-force scan.
        let mut all: Vec<(u32, Signature)> = Vec::new();
        for i in 0..500u32 {
            let edges = [
                (i % 7, i, EdgeDir::Out),
                ((i % 3) + 1, (i * 2) % 50, EdgeDir::In),
            ];
            all.push((i, esig(&edges)));
        }
        let tree = VsTree::build(all.clone());

        for &(pred, nb, dir) in &[(2u32, 10u32, EdgeDir::Out), (1, 4, EdgeDir::In)] {
            let mut q = Signature::new();
            q.encode_query_edge(Some(pred), Some(nb), dir);
            let cands: std::collections::HashSet<u32> =
                tree.candidates(&q).unwrap().into_iter().collect();
            // Brute force: every entity whose full signature contains q.
            for (id, sig) in &all {
                if sig.contains(&q) {
                    assert!(cands.contains(id), "entity {id} missing from candidates");
                }
            }
        }
    }

    // ---- out-of-core VS-tree (task 1) -------------------------------------

    fn diskvs_tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gstore_vstree_disk_{tag}.kv"));
        let _ = std::fs::remove_file(&p);
        let mut wal = p.clone().into_os_string();
        wal.push(".wal");
        let _ = std::fs::remove_file(std::path::PathBuf::from(wal));
        p
    }

    /// The on-disk VS-tree must return *identical* candidate sets to the
    /// in-memory tree built from the same entries — the core soundness/parity
    /// guarantee of task 1 (different tree shape, same `{ e : sig(e) ⊇ query }`).
    #[test]
    fn disk_candidates_identical_to_in_memory() {
        let path = diskvs_tmp("parity");
        let mut all: Vec<(u32, Signature)> = Vec::new();
        for i in 0..600u32 {
            let edges = [
                (i % 7, i, EdgeDir::Out),
                ((i % 3) + 1, (i * 2) % 50, EdgeDir::In),
            ];
            all.push((i, esig(&edges)));
        }
        let mem = VsTree::build(all.clone());
        let disk = VsTree::build_disk(&path, all.clone()).unwrap();
        assert!(disk.is_disk_backed());
        assert_eq!(disk.entity_count(), mem.entity_count());

        let queries = [
            (2u32, 10u32, EdgeDir::Out),
            (1, 4, EdgeDir::In),
            (3, 250, EdgeDir::Out),
            (5, 5, EdgeDir::In),
        ];
        for &(pred, nb, dir) in &queries {
            let mut q = Signature::new();
            q.encode_query_edge(Some(pred), Some(nb), dir);
            let mut m = mem.candidates(&q).unwrap();
            let mut d = disk.candidates(&q).unwrap();
            m.sort_unstable();
            d.sort_unstable();
            assert_eq!(m, d, "candidate sets diverged for ({pred},{nb},{dir:?})");
            // And both are a sound superset of the brute-force truth.
            for (id, sig) in &all {
                if sig.contains(&q) {
                    assert!(d.binary_search(id).is_ok(), "disk missed true match {id}");
                }
            }
        }
        // A predicate-only (variable-neighbour) query too.
        let mut q = Signature::new();
        q.encode_query_edge(Some(2), None, EdgeDir::Out);
        let mut m = mem.candidates(&q).unwrap();
        let mut d = disk.candidates(&q).unwrap();
        m.sort_unstable();
        d.sort_unstable();
        assert_eq!(m, d, "predicate-only candidate sets diverged");
        std::fs::remove_file(&path).ok();
    }
}
