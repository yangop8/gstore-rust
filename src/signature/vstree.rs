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
//!
//! ## Representation
//!
//! The in-memory tree is an arena of nodes (children and parents by index)
//! plus a **locator**, entity id → the leaf holding it. The locator is rebuilt
//! whenever a tree is built or loaded and kept current by every insert and
//! split; it is never serialized. It makes [`update`](VsTree::update)
//! O(height) instead of a scan of every entry, and lets
//! [`probe`](VsTree::probe) test one entity's stored signature in O(1)
//! (one leaf of at most [`FANOUT`] entries). The serialized form is the
//! nested tree the earlier representation derived, written field by field by
//! hand, so `vstree.bin` stays byte-identical.

use std::collections::HashMap;
use std::path::Path;

use serde::de::Deserializer;
use serde::ser::{SerializeSeq, SerializeStruct, SerializeStructVariant, Serializer};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::id::EntityLiteralId;

use super::disk_vstree::DiskTree;
use super::{SigFilter, Signature};

/// Max entries per leaf / children per internal node (tree fan-out).
const FANOUT: usize = 64;

/// "No node": the root's parent, an empty locator slot.
const NONE: u32 = u32::MAX;

/// A node's payload: a leaf's entries or an internal node's children.
#[derive(Debug)]
enum Kind {
    /// (entity id, its signature), in insertion / bulk-build order.
    Leaf(Vec<(EntityLiteralId, Signature)>),
    /// Child node indices, in order.
    Internal(Vec<u32>),
}

/// One arena node: its union signature, its parent and its payload.
#[derive(Debug)]
struct Node {
    sig: Signature,
    parent: u32,
    kind: Kind,
}

/// Entity id → index of the leaf that holds it. Entity ids are dense from 0,
/// so most live in a vector; an id far beyond the indexed population (only
/// possible through the raw [`VsTree::insert`] API) goes to a map instead of
/// growing the vector.
#[derive(Debug, Default)]
struct Locator {
    dense: Vec<u32>,
    sparse: HashMap<EntityLiteralId, u32>,
    /// Ids located so far (bounds how far `dense` may grow).
    len: usize,
}

impl Locator {
    fn get(&self, id: EntityLiteralId) -> Option<u32> {
        if let Some(&leaf) = self.dense.get(id as usize) {
            if leaf != NONE {
                return Some(leaf);
            }
        }
        if self.sparse.is_empty() {
            None
        } else {
            self.sparse.get(&id).copied()
        }
    }

    /// Point `id` at `leaf`.
    fn set(&mut self, id: EntityLiteralId, leaf: u32) {
        let i = id as usize;
        if i >= self.dense.len() && i <= 2 * self.len + (1 << 16) {
            self.dense.resize(i + 1, NONE);
        }
        match self.dense.get_mut(i) {
            Some(slot) => {
                if *slot == NONE {
                    self.len += 1;
                }
                *slot = leaf;
                if !self.sparse.is_empty() {
                    self.sparse.remove(&id);
                }
            }
            None => {
                if self.sparse.insert(id, leaf).is_none() {
                    self.len += 1;
                }
            }
        }
    }
}

/// Pick the child whose signature grows least when `sig` is unioned into it
/// (minimal enlargement); ties go to the child with the smaller signature, which
/// keeps node signatures tight and pruning effective.
fn choose_child<'a>(children: impl Iterator<Item = &'a Signature>, sig: &Signature) -> usize {
    let mut best = 0;
    let mut best_added = u32::MAX;
    let mut best_pop = u32::MAX;
    for (i, c) in children.enumerate() {
        let added = c.added_bits(sig);
        let pop = c.popcount();
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

/// Sort bulk-build entries so that signature-similar entities are adjacent:
/// by the raw signature limbs (high bits first), then by id. Entries arrive in
/// ascending id order, so this is exactly the stable sort by limbs the build
/// used before — computed without materializing a key per comparison. Shared
/// with the out-of-core builder so both bulk routes cluster identically.
pub(super) fn sort_entries(entries: &mut [(EntityLiteralId, Signature)]) {
    entries.sort_unstable_by(|a, b| a.1.cmp_limbs(&b.1).then(a.0.cmp(&b.0)));
}

/// A signature tree over entity signatures.
///
/// By default the tree is fully in memory (an arena of nodes, see the module
/// docs), and `bincode`-serializes to `vstree.bin` for the in-memory database
/// path. It can instead be **out-of-core** (`disk`): the nodes then live in a
/// dedicated paged file and a candidate scan reads only the nodes it traverses
/// (see [`DiskTree`]). The `disk` backing is not serialized, so the serialized
/// form is the in-memory tree's — `save`/`load` are unaffected — and is
/// reconstructed from its own file via [`open_disk`](Self::open_disk).
#[derive(Debug, Default)]
pub struct VsTree {
    nodes: Vec<Node>,
    root: Option<u32>,
    entity_count: usize,
    /// Entity id → leaf (see the module docs); not serialized.
    locator: Locator,
    /// When set, the tree is backed by an on-disk paged node file instead of
    /// `nodes`; all reads ([`candidates`](Self::candidates)) go through it.
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
        sort_entries(&mut entries);

        let mut tree = VsTree {
            entity_count,
            ..VsTree::default()
        };

        // Build leaves, moving the entries into them.
        let mut level: Vec<u32> = Vec::with_capacity(entity_count.div_ceil(FANOUT));
        let mut it = entries.into_iter();
        loop {
            let chunk: Vec<(EntityLiteralId, Signature)> = it.by_ref().take(FANOUT).collect();
            if chunk.is_empty() {
                break;
            }
            let mut sig = Signature::new();
            for (_, s) in &chunk {
                sig.union_with(s);
            }
            level.push(tree.push(sig, Kind::Leaf(chunk)));
        }

        // Build internal levels until a single root remains.
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(FANOUT));
            for children in level.chunks(FANOUT) {
                let mut sig = Signature::new();
                for &c in children {
                    sig.union_with(&tree.nodes[c as usize].sig);
                }
                next.push(tree.push(sig, Kind::Internal(children.to_vec())));
            }
            level = next;
        }

        tree.root = level.pop();
        tree.link();
        tree
    }

    /// Append a node (its parent is set by [`link`](Self::link) or the
    /// caller).
    fn push(&mut self, sig: Signature, kind: Kind) -> u32 {
        let idx = u32::try_from(self.nodes.len()).expect("VS-tree node count overflows u32");
        self.nodes.push(Node {
            sig,
            parent: NONE,
            kind,
        });
        idx
    }

    /// Set every node's parent and rebuild the locator from the node arena.
    fn link(&mut self) {
        let mut locator = Locator::default();
        for idx in 0..self.nodes.len() {
            let kind = std::mem::replace(&mut self.nodes[idx].kind, Kind::Internal(Vec::new()));
            match &kind {
                Kind::Leaf(entries) => {
                    for &(id, _) in entries {
                        // The first entry of an id wins (as `update` finds it).
                        if locator.get(id).is_none() {
                            locator.set(id, idx as u32);
                        }
                    }
                }
                Kind::Internal(children) => {
                    for &c in children {
                        self.nodes[c as usize].parent = idx as u32;
                    }
                }
            }
            self.nodes[idx].kind = kind;
        }
        if let Some(r) = self.root {
            self.nodes[r as usize].parent = NONE;
        }
        self.locator = locator;
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
            disk: Some(disk),
            ..VsTree::default()
        })
    }

    /// Open an existing out-of-core VS-tree previously written by
    /// [`build_disk`](Self::build_disk).
    pub fn open_disk<P: AsRef<Path>>(path: P) -> Result<VsTree> {
        let disk = DiskTree::open(path.as_ref())?;
        Ok(VsTree {
            disk: Some(disk),
            ..VsTree::default()
        })
    }

    /// Whether this tree is backed by an on-disk paged node file (out-of-core)
    /// rather than in-memory nodes. Such a tree is read-only here: incremental
    /// [`insert`](Self::insert)/[`update`](Self::update) target the in-memory
    /// representation, so a disk-backed database invalidates and rebuilds its
    /// index on an insert instead (see `Database::insert_triple`).
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
        match self.root {
            None => {
                let leaf = self.push(sig, Kind::Leaf(vec![(id, sig)]));
                self.root = Some(leaf);
                self.locator.set(id, leaf);
            }
            Some(root) => {
                if let Some(sibling) = self.insert_at(root, id, sig) {
                    // Root split: a fresh internal root over the two halves.
                    let mut s = self.nodes[root as usize].sig;
                    s.union_with(&self.nodes[sibling as usize].sig);
                    let new_root = self.push(s, Kind::Internal(vec![root, sibling]));
                    self.nodes[root as usize].parent = new_root;
                    self.nodes[sibling as usize].parent = new_root;
                    self.root = Some(new_root);
                }
            }
        }
        self.entity_count += 1;
    }

    /// Insert `(id, sig)` into the subtree at `node`, unioning `sig` along the
    /// descent path so every node's signature stays a superset of its
    /// descendants. Returns the new right sibling if `node` overflowed
    /// [`FANOUT`] and split (the caller adopts it); `None` otherwise.
    fn insert_at(&mut self, node: u32, id: EntityLiteralId, sig: Signature) -> Option<u32> {
        let n = node as usize;
        self.nodes[n].sig.union_with(&sig);
        let overflow = if let Kind::Leaf(entries) = &mut self.nodes[n].kind {
            entries.push((id, sig));
            let len = entries.len();
            if self.locator.get(id).is_none() {
                self.locator.set(id, node);
            }
            len > FANOUT
        } else {
            let (ci, child) = match &self.nodes[n].kind {
                Kind::Internal(children) => {
                    let sigs = children.iter().map(|&c| &self.nodes[c as usize].sig);
                    let ci = choose_child(sigs, &sig);
                    (ci, children[ci])
                }
                Kind::Leaf(_) => return None,
            };
            let new_child = self.insert_at(child, id, sig)?;
            // Keep the split-off sibling next to its origin.
            self.nodes[new_child as usize].parent = node;
            match &mut self.nodes[n].kind {
                Kind::Internal(children) => {
                    children.insert(ci + 1, new_child);
                    children.len() > FANOUT
                }
                Kind::Leaf(_) => false,
            }
        };
        overflow.then(|| self.split(node))
    }

    /// Split overflowing `node` into two, keeping the left half in place and
    /// returning the (new) right half. Both halves' signatures are recomputed as
    /// the exact union of their members, preserving the superset invariant.
    fn split(&mut self, node: u32) -> u32 {
        let n = node as usize;
        let parent = self.nodes[n].parent;
        match std::mem::replace(&mut self.nodes[n].kind, Kind::Internal(Vec::new())) {
            Kind::Leaf(entries) => {
                let (left, lsig, right, rsig) = seed_split(entries, |(_, s)| *s);
                self.nodes[n].kind = Kind::Leaf(left);
                self.nodes[n].sig = lsig;
                let moved: Vec<EntityLiteralId> = right.iter().map(|&(id, _)| id).collect();
                let sibling = self.push(rsig, Kind::Leaf(right));
                self.nodes[sibling as usize].parent = parent;
                for id in moved {
                    if self.locator.get(id) == Some(node) {
                        self.locator.set(id, sibling);
                    }
                }
                sibling
            }
            Kind::Internal(children) => {
                let nodes = &self.nodes;
                let (left, lsig, right, rsig) = seed_split(children, |&c| nodes[c as usize].sig);
                self.nodes[n].kind = Kind::Internal(left);
                self.nodes[n].sig = lsig;
                let moved = right.clone();
                let sibling = self.push(rsig, Kind::Internal(right));
                self.nodes[sibling as usize].parent = parent;
                for c in moved {
                    self.nodes[c as usize].parent = sibling;
                }
                sibling
            }
        }
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
    /// one affected entity's leaf entry — found through the locator — is
    /// overwritten and the new bits unioned up its path, in O(height).
    ///
    /// Soundness: the target leaf entry is set to *exactly* `new_sig`, while every
    /// ancestor only ever *gains* bits (a union). So the tree invariant — each
    /// node's signature ⊇ the union of its descendant entity signatures — is
    /// preserved whether `new_sig` grew (edge added) or shrank (edge removed)
    /// relative to the old entry. The candidate filter therefore stays a sound
    /// superset of the true matches (never a false negative); a shrink merely
    /// leaves slightly looser ancestor unions, costing only some pruning power.
    pub fn update(&mut self, id: EntityLiteralId, new_sig: Signature) -> bool {
        if self.disk.is_some() {
            return false;
        }
        let Some(leaf) = self.locator.get(id) else {
            return false;
        };
        let Kind::Leaf(entries) = &mut self.nodes[leaf as usize].kind else {
            return false;
        };
        let Some(entry) = entries.iter_mut().find(|(eid, _)| *eid == id) else {
            return false;
        };
        entry.1 = new_sig;
        let mut n = leaf;
        while n != NONE {
            let node = &mut self.nodes[n as usize];
            node.sig.union_with(&new_sig);
            n = node.parent;
        }
        true
    }

    /// Whether `probe` is available: the tree is in memory (an out-of-core
    /// tree is only walked).
    pub fn can_probe(&self) -> bool {
        self.disk.is_none()
    }

    /// Whether entity `id`'s stored signature contains `query` — exactly
    /// whether [`candidates`](Self::candidates)`(query)` reports `id` (every
    /// ancestor's signature contains the entry's, so a walk reaches every
    /// entry that contains the query). `false` for an entity not in the tree
    /// and for an out-of-core tree (see [`can_probe`](Self::can_probe)).
    /// O(1): one leaf, found through the locator.
    pub fn probe(&self, id: EntityLiteralId, query: &Signature) -> bool {
        if self.disk.is_some() {
            return false;
        }
        let Some(leaf) = self.locator.get(id) else {
            return false;
        };
        match &self.nodes[leaf as usize].kind {
            Kind::Leaf(entries) => entries
                .iter()
                .find(|(eid, _)| *eid == id)
                .is_some_and(|(_, sig)| sig.contains(query)),
            Kind::Internal(_) => false,
        }
    }

    /// Height of the tree (0 = empty, 1 = a single leaf). For tests asserting
    /// that incremental insertion actually splits and grows the tree.
    #[cfg(test)]
    fn height(&self) -> usize {
        fn depth(tree: &VsTree, node: u32) -> usize {
            match &tree.nodes[node as usize].kind {
                Kind::Leaf(_) => 1,
                Kind::Internal(children) => {
                    1 + children.iter().map(|&c| depth(tree, c)).max().unwrap_or(0)
                }
            }
        }
        self.root.map_or(0, |r| depth(self, r))
    }

    /// Return all entity ids whose signature contains `query` (a superset of
    /// the true matches). If the tree is empty, returns `None` so the caller
    /// can fall back to "no filtering".
    pub fn candidates(&self, query: &Signature) -> Option<Vec<EntityLiteralId>> {
        if let Some(disk) = &self.disk {
            // Out-of-core: delegate to the streaming backend through the shared
            // [`SigFilter`] seam. Its impl degrades an I/O error to "no filtering"
            // (`None`) — the engine then scans without pruning, slower but still
            // correct (never a false negative). A healthy store never hits that.
            return SigFilter::candidates(disk, query);
        }
        let root = self.root?;
        let mut out = Vec::new();
        self.search(root, query, &mut out);
        Some(out)
    }

    fn search(&self, node: u32, query: &Signature, out: &mut Vec<EntityLiteralId>) {
        let node = &self.nodes[node as usize];
        // Prune: a subtree can hold a match only if its union contains the query.
        if !node.sig.contains(query) {
            return;
        }
        match &node.kind {
            Kind::Leaf(entries) => {
                for (id, sig) in entries {
                    if sig.contains(query) {
                        out.push(*id);
                    }
                }
            }
            Kind::Internal(children) => {
                for &child in children {
                    self.search(child, query, out);
                }
            }
        }
    }
}

// ---- serialization: the nested form, byte for byte --------------------------
//
// `vstree.bin` holds what `#[derive(Serialize)]` produced for the earlier,
// nested representation:
//
//     struct VsTree { root: Option<Box<Node>>, entity_count: usize }
//     enum Node {
//         Leaf { sig: Signature, entries: Vec<(EntityLiteralId, Signature)> },
//         Internal { sig: Signature, children: Vec<Node> },
//     }
//
// It is written from the arena by `NodeRef` and read back through the
// `NestedNode` mirror (then converted into an arena).

/// A borrowed view of one arena node, serialized as the nested `Node`.
struct NodeRef<'a> {
    tree: &'a VsTree,
    node: u32,
}

impl Serialize for NodeRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let node = &self.tree.nodes[self.node as usize];
        match &node.kind {
            Kind::Leaf(entries) => {
                let mut v = serializer.serialize_struct_variant("Node", 0, "Leaf", 2)?;
                v.serialize_field("sig", &node.sig)?;
                v.serialize_field("entries", entries)?;
                v.end()
            }
            Kind::Internal(children) => {
                let mut v = serializer.serialize_struct_variant("Node", 1, "Internal", 2)?;
                v.serialize_field("sig", &node.sig)?;
                v.serialize_field(
                    "children",
                    &ChildrenRef {
                        tree: self.tree,
                        children,
                    },
                )?;
                v.end()
            }
        }
    }
}

/// A node's children, serialized as the nested `Vec<Node>`.
struct ChildrenRef<'a> {
    tree: &'a VsTree,
    children: &'a [u32],
}

impl Serialize for ChildrenRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.children.len()))?;
        for &node in self.children {
            seq.serialize_element(&NodeRef {
                tree: self.tree,
                node,
            })?;
        }
        seq.end()
    }
}

impl Serialize for VsTree {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut st = serializer.serialize_struct("VsTree", 2)?;
        st.serialize_field("root", &self.root.map(|node| NodeRef { tree: self, node }))?;
        st.serialize_field("entity_count", &self.entity_count)?;
        st.end()
    }
}

/// The nested node form, for reading `vstree.bin`.
#[derive(Serialize, Deserialize)]
#[serde(rename = "Node")]
enum NestedNode {
    Leaf {
        sig: Signature,
        entries: Vec<(EntityLiteralId, Signature)>,
    },
    Internal {
        sig: Signature,
        children: Vec<NestedNode>,
    },
}

/// The nested tree form, for reading `vstree.bin`.
#[derive(Serialize, Deserialize)]
#[serde(rename = "VsTree")]
struct NestedTree {
    root: Option<Box<NestedNode>>,
    entity_count: usize,
}

impl VsTree {
    /// Move a nested subtree into the arena; returns its index.
    fn adopt(&mut self, nested: NestedNode) -> u32 {
        match nested {
            NestedNode::Leaf { sig, entries } => self.push(sig, Kind::Leaf(entries)),
            NestedNode::Internal { sig, children } => {
                let idx = self.push(sig, Kind::Internal(Vec::new()));
                let kids: Vec<u32> = children.into_iter().map(|c| self.adopt(c)).collect();
                self.nodes[idx as usize].kind = Kind::Internal(kids);
                idx
            }
        }
    }
}

impl<'de> Deserialize<'de> for VsTree {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let nested = NestedTree::deserialize(deserializer)?;
        let mut tree = VsTree {
            entity_count: nested.entity_count,
            ..VsTree::default()
        };
        if let Some(root) = nested.root {
            tree.root = Some(tree.adopt(*root));
        }
        tree.link();
        Ok(tree)
    }
}

/// The VS-tree as a [`SigFilter`]. This is the in-memory implementation of the
/// seam (and transparently delegates to the out-of-core backend when the tree is
/// disk-backed, via the inherent methods), so the query path can hold either an
/// in-memory or a streaming tree behind a single `&dyn SigFilter` and prune
/// candidates identically.
impl SigFilter for VsTree {
    fn candidates(&self, query: &Signature) -> Option<Vec<EntityLiteralId>> {
        VsTree::candidates(self, query)
    }

    fn entity_count(&self) -> usize {
        VsTree::entity_count(self)
    }

    fn is_empty(&self) -> bool {
        VsTree::is_empty(self)
    }

    fn pages_read(&self) -> u64 {
        VsTree::disk_pages_read(self)
    }
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

    /// Both backends behind a single `&dyn SigFilter`: a caller can hold the
    /// trait object and get *identical* candidate sets from the in-memory tree
    /// and the streaming (out-of-core) tree — the unified-seam guarantee that
    /// lets the query path use either uniformly. The streaming filter pages
    /// nodes from disk (`pages_read > 0`) while the in-memory one never reads a
    /// page, proving the tree need not be resident.
    #[test]
    fn sigfilter_seam_parity_in_memory_vs_streaming() {
        use crate::signature::SigFilter;
        let path = diskvs_tmp("seam_parity");
        let mut all: Vec<(u32, Signature)> = Vec::new();
        for i in 0..600u32 {
            let edges = [
                (i % 7, i, EdgeDir::Out),
                ((i % 3) + 1, (i * 2) % 50, EdgeDir::In),
            ];
            all.push((i, esig(&edges)));
        }
        let mem = VsTree::build(all.clone());
        let streaming = VsTree::build_disk(&path, all.clone()).unwrap();
        // Consume both purely through the trait object.
        let filters: [&dyn SigFilter; 2] = [&mem, &streaming];
        assert_eq!(filters[0].entity_count(), filters[1].entity_count());
        assert!(!filters[0].is_empty() && !filters[1].is_empty());

        for &(pred, nb, dir) in &[
            (2u32, 10u32, EdgeDir::Out),
            (1, 4, EdgeDir::In),
            (3, 250, EdgeDir::Out),
            (5, 5, EdgeDir::In),
        ] {
            let mut q = Signature::new();
            q.encode_query_edge(Some(pred), Some(nb), dir);
            let mut m = filters[0].candidates(&q).unwrap();
            let mut s = filters[1].candidates(&q).unwrap();
            m.sort_unstable();
            s.sort_unstable();
            assert_eq!(m, s, "seam candidates diverged for ({pred},{nb},{dir:?})");
        }
        // A predicate-only (variable-neighbour) query too.
        let mut q = Signature::new();
        q.encode_query_edge(Some(2), None, EdgeDir::Out);
        let mut m = filters[0].candidates(&q).unwrap();
        let mut s = filters[1].candidates(&q).unwrap();
        m.sort_unstable();
        s.sort_unstable();
        assert_eq!(m, s, "predicate-only seam candidates diverged");

        // The streaming filter actually paged nodes from disk; the in-memory
        // filter never reads a page.
        assert_eq!(filters[0].pages_read(), 0, "in-memory filter reads no pages");
        assert!(filters[1].pages_read() > 0, "streaming filter must page from disk");
        std::fs::remove_file(&path).ok();
    }
}

/// The nested VS-tree as it was before the arena and the sort fix, kept
/// verbatim as the oracle the mirror tests compare bytes and answers with.
#[cfg(test)]
pub(super) mod legacy {
    use serde::Serialize;

    use super::{choose_child_legacy, seed_split, FANOUT};
    use crate::model::id::EntityLiteralId;
    use crate::signature::Signature;

    #[derive(Debug, Serialize)]
    pub(crate) enum Node {
        Leaf {
            sig: Signature,
            entries: Vec<(EntityLiteralId, Signature)>,
        },
        Internal {
            sig: Signature,
            children: Vec<Node>,
        },
    }

    impl Node {
        pub(crate) fn sig(&self) -> &Signature {
            match self {
                Node::Leaf { sig, .. } | Node::Internal { sig, .. } => sig,
            }
        }

        fn insert(&mut self, id: EntityLiteralId, sig: Signature) -> Option<Node> {
            match self {
                Node::Leaf { sig: nsig, entries } => {
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
                    let ci = choose_child_legacy(children, &sig);
                    let new_child = children[ci].insert(id, sig)?;
                    children.insert(ci + 1, new_child);
                    if children.len() <= FANOUT {
                        return None;
                    }
                }
            }
            Some(self.split())
        }

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

        fn search(&self, query: &Signature, out: &mut Vec<EntityLiteralId>) {
            if !self.sig().contains(query) {
                return;
            }
            match self {
                Node::Leaf { entries, .. } => {
                    for (id, sig) in entries {
                        if sig.contains(query) {
                            out.push(*id);
                        }
                    }
                }
                Node::Internal { children, .. } => {
                    for child in children {
                        child.search(query, out);
                    }
                }
            }
        }
    }

    /// The old clustering key: one allocated limb vector per call.
    pub(crate) fn sig_key(sig: &Signature) -> Vec<u64> {
        let mut key = vec![0u64; crate::signature::LIMBS];
        for (i, word) in key.iter_mut().enumerate() {
            for b in 0..64 {
                let bit = i * 64 + b;
                if bit < crate::signature::ENTITY_SIG_LENGTH && sig.test(bit) {
                    *word |= 1u64 << b;
                }
            }
        }
        key
    }

    #[derive(Debug, Serialize)]
    pub(crate) struct VsTree {
        pub(crate) root: Option<Box<Node>>,
        pub(crate) entity_count: usize,
    }

    impl VsTree {
        pub(crate) fn build(mut entries: Vec<(EntityLiteralId, Signature)>) -> VsTree {
            let entity_count = entries.len();
            if entries.is_empty() {
                return VsTree {
                    root: None,
                    entity_count: 0,
                };
            }
            entries.sort_by_key(|(_, s)| sig_key(s));
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
            }
        }

        pub(crate) fn insert(&mut self, id: EntityLiteralId, sig: Signature) {
            match self.root.take() {
                None => {
                    self.root = Some(Box::new(Node::Leaf {
                        sig,
                        entries: vec![(id, sig)],
                    }));
                }
                Some(mut root) => {
                    if let Some(sibling) = root.insert(id, sig) {
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

        pub(crate) fn update(&mut self, id: EntityLiteralId, new_sig: Signature) -> bool {
            match self.root.as_mut() {
                None => false,
                Some(root) => root.update(id, new_sig),
            }
        }

        pub(crate) fn candidates(&self, query: &Signature) -> Option<Vec<EntityLiteralId>> {
            let root = self.root.as_ref()?;
            let mut out = Vec::new();
            root.search(query, &mut out);
            Some(out)
        }
    }
}

/// [`choose_child`] over nested legacy nodes.
#[cfg(test)]
fn choose_child_legacy(children: &[legacy::Node], sig: &Signature) -> usize {
    choose_child(children.iter().map(legacy::Node::sig), sig)
}

#[cfg(test)]
mod arena_tests {
    use super::*;
    use crate::signature::EdgeDir;

    /// SplitMix64.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u32) -> u32 {
            (self.next() % u64::from(n)) as u32
        }
    }

    /// A random entity signature from a few random edges. Low edge variety
    /// makes many signatures equal, exercising the sort's tie-break.
    fn random_sig(rng: &mut Rng, variety: u32) -> Signature {
        let mut s = Signature::new();
        for _ in 0..=rng.below(3) {
            let dir = if rng.below(2) == 0 {
                EdgeDir::Out
            } else {
                EdgeDir::In
            };
            s.encode_edge(rng.below(variety), rng.below(variety * 3), dir);
        }
        s
    }

    fn random_entries(rng: &mut Rng, n: u32, variety: u32) -> Vec<(u32, Signature)> {
        (0..n).map(|i| (i, random_sig(rng, variety))).collect()
    }

    fn random_query(rng: &mut Rng, variety: u32) -> Signature {
        let mut q = Signature::new();
        let dir = if rng.below(2) == 0 {
            EdgeDir::Out
        } else {
            EdgeDir::In
        };
        let p = (rng.below(4) != 0).then(|| rng.below(variety));
        let n = (rng.below(3) != 0).then(|| rng.below(variety * 3));
        q.encode_query_edge(p, n, dir);
        q
    }

    fn bytes<T: Serialize>(t: &T) -> Vec<u8> {
        bincode::serialize(t).unwrap()
    }

    #[test]
    fn sort_fix_keeps_the_legacy_order_and_bytes() {
        for (seed, n, variety) in [
            (1u64, 0u32, 4u32),
            (2, 1, 4),
            (3, 300, 2),
            (4, 5000, 5),
            (5, 9000, 40),
        ] {
            let mut rng = Rng(seed);
            let entries = random_entries(&mut rng, n, variety);
            let mut legacy_order = entries.clone();
            legacy_order.sort_by_key(|(_, s)| legacy::sig_key(s));
            let mut new_order = entries.clone();
            sort_entries(&mut new_order);
            assert_eq!(
                new_order, legacy_order,
                "seed {seed}: clustering order changed"
            );
            let old = legacy::VsTree::build(entries.clone());
            let new = VsTree::build(entries);
            assert_eq!(
                bytes(&new),
                bytes(&old),
                "seed {seed}: vstree.bin bytes changed"
            );
        }
    }

    #[test]
    fn online_inserts_and_updates_mirror_the_legacy_bytes() {
        let mut rng = Rng(77);
        let base = random_entries(&mut rng, 700, 9);
        let mut old = legacy::VsTree::build(base.clone());
        let mut new = VsTree::build(base);
        // Inserts that split leaves and internal nodes, interleaved with
        // updates of existing and new entities.
        for i in 700..6000u32 {
            let s = random_sig(&mut rng, 9);
            old.insert(i, s);
            new.insert(i, s);
            if i % 3 == 0 {
                let id = rng.below(i + 1);
                let s = random_sig(&mut rng, 9);
                assert_eq!(old.update(id, s), new.update(id, s), "update {id}");
            }
        }
        assert!(!new.update(1_000_000, Signature::new()));
        assert!(!old.update(1_000_000, Signature::new()));
        assert!(new.height() > 2, "inserts should have grown the tree");
        let encoded = bytes(&new);
        assert_eq!(
            encoded,
            bytes(&old),
            "arena and nested trees serialize differently"
        );
        // A round trip through the nested form keeps the bytes and answers.
        let back: VsTree = bincode::deserialize(&encoded).unwrap();
        assert_eq!(bytes(&back), encoded);
        for _ in 0..200 {
            let q = random_query(&mut rng, 9);
            let want = old.candidates(&q);
            assert_eq!(new.candidates(&q), want);
            assert_eq!(back.candidates(&q), want);
        }
        // The empty tree, too.
        assert_eq!(
            bytes(&VsTree::new()),
            bytes(&legacy::VsTree {
                root: None,
                entity_count: 0
            })
        );
        let empty: VsTree = bincode::deserialize(&bytes(&VsTree::new())).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn probe_equals_walk_intersected_with_the_candidates() {
        let mut rng = Rng(9);
        let mut tree = VsTree::build(random_entries(&mut rng, 3000, 12));
        for i in 3000..4500u32 {
            tree.insert(i, random_sig(&mut rng, 12));
        }
        for _ in 0..800 {
            let id = rng.below(4500);
            tree.update(id, random_sig(&mut rng, 12));
        }
        assert!(tree.can_probe());
        for _ in 0..300 {
            let q = random_query(&mut rng, 12);
            let mut walk = tree.candidates(&q).unwrap();
            walk.sort_unstable();
            // An existing exact candidate set: entities in and out of the
            // tree, and a literal id.
            let mut existing: Vec<u32> = (0..rng.below(400)).map(|_| rng.below(5000)).collect();
            existing.push(crate::model::id::LITERAL_FIRST_ID + 3);
            existing.sort_unstable();
            existing.dedup();
            let probed: Vec<u32> = existing
                .iter()
                .copied()
                .filter(|&x| tree.probe(x, &q))
                .collect();
            let intersected: Vec<u32> = existing
                .iter()
                .copied()
                .filter(|x| walk.binary_search(x).is_ok())
                .collect();
            assert_eq!(probed, intersected);
        }
        assert!(!VsTree::new().probe(0, &Signature::new()));
    }

    #[test]
    fn updates_through_the_locator_equal_a_rebuild() {
        let mut rng = Rng(31);
        let mut current = random_entries(&mut rng, 2500, 10);
        let mut tree = VsTree::build(current.clone());
        for i in 2500..3200u32 {
            let s = random_sig(&mut rng, 10);
            tree.insert(i, s);
            current.push((i, s));
        }
        // Grow and shrink signatures.
        for _ in 0..3000 {
            let id = rng.below(3200);
            let s = if rng.below(2) == 0 {
                let mut g = current[id as usize].1;
                g.union_with(&random_sig(&mut rng, 10));
                g
            } else {
                random_sig(&mut rng, 10)
            };
            assert!(tree.update(id, s));
            current[id as usize].1 = s;
        }
        let rebuilt = VsTree::build(current);
        for _ in 0..300 {
            let q = random_query(&mut rng, 10);
            let mut a = tree.candidates(&q).unwrap();
            let mut b = rebuilt.candidates(&q).unwrap();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b);
        }
    }

    #[test]
    fn sparse_ids_are_located_too() {
        let mut tree = VsTree::new();
        let mut q = Signature::new();
        q.encode_query_edge(Some(2), Some(9), EdgeDir::Out);
        let mut s = Signature::new();
        s.encode_edge(2, 9, EdgeDir::Out);
        for id in [5u32, 1_900_000_000, 7, 3_000_000] {
            tree.insert(id, s);
        }
        for id in [5u32, 1_900_000_000, 7, 3_000_000] {
            assert!(tree.probe(id, &q), "{id}");
            assert!(tree.update(id, s));
        }
        assert!(!tree.probe(6, &q));
    }
}
