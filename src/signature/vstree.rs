//! The VS-tree: a signature tree over entity signatures.
//!
//! gStore's VSTree is a height-balanced S-tree built by incremental insertion
//! with node splitting. We build the same shape by **bulk loading**: cluster
//! signature-similar entities into leaves, then build internal levels bottom-up,
//! each internal node holding the union of its children's signatures. Search
//! prunes any subtree whose union does not contain the query signature.
//!
//! Correctness (soundness): a node's signature is the union (superset) of every
//! descendant entity's signature, so pruning a node where `node.sig ⊉ query`
//! can never discard a true match. Therefore the returned candidate set is a
//! superset of the true matches — exactly what the engine needs as a filter.

use serde::{Deserialize, Serialize};

use crate::model::id::EntityLiteralId;

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
}

/// A signature tree over entity signatures.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VsTree {
    root: Option<Box<Node>>,
    entity_count: usize,
}

impl VsTree {
    pub fn new() -> VsTree {
        VsTree::default()
    }

    pub fn entity_count(&self) -> usize {
        self.entity_count
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
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
        }
    }

    /// Return all entity ids whose signature contains `query` (a superset of
    /// the true matches). If the tree is empty, returns `None` so the caller
    /// can fall back to "no filtering".
    pub fn candidates(&self, query: &Signature) -> Option<Vec<EntityLiteralId>> {
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
fn sig_key(sig: &Signature) -> Vec<u64> {
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
}
