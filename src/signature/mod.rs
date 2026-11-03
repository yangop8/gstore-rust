//! Signatures and the VS-tree — gStore's subgraph-matching filter index.
//!
//! This is a faithful port of gStore's `src/Signature` + the VSTree. Every
//! entity's neighbourhood (its incident edges: predicate + neighbour, in/out)
//! is hashed into a fixed-width bit signature ([`Signature`], 944 bits, exactly
//! gStore's `EntityBitSet`). A query node's *known* incident edges (constant
//! predicate and/or constant neighbour) hash into a query signature.
//!
//! Containment is the filter: if entity `e` can match query node `q`, then
//! `sig(e) ⊇ sig(q)` (every query bit is set in the entity). The [`VsTree`] is a
//! signature tree (S-tree): internal nodes hold the union of their children's
//! signatures, so a subtree can be pruned whenever its union does not contain
//! the query signature. Search returns a *superset* of the true matches — a
//! sound candidate filter the query engine intersects into the join.

mod disk_vstree;
mod vstree;

pub use vstree::VsTree;

use serde::{Deserialize, Serialize};

use crate::model::id::{is_literal_id, EntityLiteralId, PredId};

/// The VS-tree candidate-filter seam.
///
/// Given a query signature, return a *sound superset* of the entity ids whose
/// signature contains it — i.e. `{ e : sig(e) ⊇ query }` plus possibly a few
/// false positives the join later discards, never a false negative. `None`
/// means "no filtering" (e.g. an empty tree), so the caller scans unpruned.
///
/// This is the single interface the query/candidate path consults, implemented
/// by **both** backends so pruning is identical regardless of residency:
/// * the in-memory VS-tree ([`VsTree`]), which holds every node in RAM, and
/// * the out-of-core, streaming VS-tree (`disk_vstree::DiskTree`, reached
///   through a disk-backed [`VsTree`]), which pages in only the nodes a query
///   traverses.
///
/// Both return the same candidate set for the same entries (only tree shape and
/// residency differ), so a database can swap one for the other transparently —
/// e.g. a large in-memory store can opt into the streaming filter without
/// materializing the whole tree. The trait is object-safe: a caller can hold a
/// `&dyn SigFilter` and treat either backend uniformly.
pub trait SigFilter {
    /// Entity ids whose signature contains `query` (a sound superset), or `None`
    /// for "no filtering".
    fn candidates(&self, query: &Signature) -> Option<Vec<EntityLiteralId>>;
    /// Number of entities indexed by this filter.
    fn entity_count(&self) -> usize;
    /// Whether the filter is empty (no entities indexed).
    fn is_empty(&self) -> bool;
    /// Cumulative on-disk node pages read by candidate scans so far — `0` for a
    /// fully in-memory filter, positive once a streaming filter has paged nodes
    /// from disk. Lets a caller prove a query touched only part of the tree.
    fn pages_read(&self) -> u64 {
        0
    }
}

/// Edge direction, as in gStore (`EDGE_IN` / `EDGE_OUT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDir {
    /// The entity is the subject of the triple (edge points out).
    Out,
    /// The entity is the object of the triple (edge points in).
    In,
}

// --- gStore signature geometry (Signature.h), kept bit-for-bit identical -----
const STR_SIG_LITERAL: usize = 200;
const STR_SIG_ENTITY: usize = STR_SIG_LITERAL * 2; // 400
const STR_SIG_LENGTH: usize = STR_SIG_ENTITY + STR_SIG_LITERAL; // 600
const EDGE_SIG_INTERVAL_NUM_HALF: usize = 10;
const EDGE_SIG_INTERVAL_BASE: usize = 10;
const EDGE_SIG_LENGTH: usize = 2 * EDGE_SIG_INTERVAL_NUM_HALF * EDGE_SIG_INTERVAL_BASE; // 200
const STR_AND_EDGE_INTERVAL_BASE: usize = 48;
const STR_AND_EDGE_INTERVAL_NUM: usize = 3;
const STR_AND_EDGE_SIG_LENGTH: usize = STR_AND_EDGE_INTERVAL_BASE * STR_AND_EDGE_INTERVAL_NUM; // 144

/// Total signature width in bits (gStore `ENTITY_SIG_LENGTH` = 944).
pub const ENTITY_SIG_LENGTH: usize = STR_SIG_LENGTH + EDGE_SIG_LENGTH + STR_AND_EDGE_SIG_LENGTH;

/// Number of 64-bit limbs backing a signature (⌈944/64⌉ = 15 → 960 bits).
const LIMBS: usize = ENTITY_SIG_LENGTH.div_ceil(64);

/// Per-limb masks of the bits that belong to the signature (the last limb is
/// padded past [`ENTITY_SIG_LENGTH`]).
const LIMB_MASK: [u64; LIMBS] = {
    let mut m = [u64::MAX; LIMBS];
    let used = ENTITY_SIG_LENGTH - (LIMBS - 1) * 64;
    if used < 64 {
        m[LIMBS - 1] = (1u64 << used) - 1;
    }
    m
};

/// The bits of the half-open bit range `[lo, hi)`, as limbs.
const fn range_mask(lo: usize, hi: usize) -> [u64; LIMBS] {
    let mut m = [0u64; LIMBS];
    let mut b = lo;
    while b < hi {
        m[b / 64] |= 1u64 << (b % 64);
        b += 1;
    }
    m
}

/// The predicate bits an in-edge sets ([`Signature::encode_predicate`] with
/// [`EdgeDir::In`]): seeds `0..EDGE_SIG_INTERVAL_NUM_HALF`.
const PRED_IN_MASK: [u64; LIMBS] = range_mask(
    STR_SIG_LENGTH,
    STR_SIG_LENGTH + EDGE_SIG_INTERVAL_BASE * EDGE_SIG_INTERVAL_NUM_HALF,
);

/// The predicate bits an out-edge sets: the upper half of the edge region.
const PRED_OUT_MASK: [u64; LIMBS] = range_mask(
    STR_SIG_LENGTH + EDGE_SIG_INTERVAL_BASE * EDGE_SIG_INTERVAL_NUM_HALF,
    STR_SIG_LENGTH + EDGE_SIG_LENGTH,
);

/// Raw byte width of a signature's limbs (`LIMBS × 8` = 120). Used by the
/// out-of-core VS-tree ([`vstree`]/[`disk_vstree`]) to (de)serialize a signature
/// into a fixed slot inside a paged node.
pub(crate) const SIG_BYTES: usize = LIMBS * 8;

/// A fixed-width bit signature (gStore `EntityBitSet`).
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    bits: [u64; LIMBS],
}

impl Default for Signature {
    fn default() -> Self {
        Signature { bits: [0; LIMBS] }
    }
}

impl std::fmt::Debug for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signature({} bits set)", self.popcount())
    }
}

impl Signature {
    pub fn new() -> Signature {
        Signature::default()
    }

    #[inline]
    fn set(&mut self, bit: usize) {
        debug_assert!(bit < ENTITY_SIG_LENGTH, "signature bit {bit} out of range");
        self.bits[bit / 64] |= 1u64 << (bit % 64);
    }

    #[inline]
    pub fn test(&self, bit: usize) -> bool {
        debug_assert!(bit < ENTITY_SIG_LENGTH, "signature bit {bit} out of range");
        self.bits[bit / 64] & (1u64 << (bit % 64)) != 0
    }

    /// Union (gStore `operator|=`).
    pub fn union_with(&mut self, other: &Signature) {
        for i in 0..LIMBS {
            self.bits[i] |= other.bits[i];
        }
    }

    /// Does `self` contain `other` (i.e. is `self ⊇ other`)? This is the
    /// VS-tree filter test: a candidate entity's signature must contain the
    /// query node's signature.
    #[inline]
    pub fn contains(&self, other: &Signature) -> bool {
        for i in 0..LIMBS {
            if other.bits[i] & !self.bits[i] != 0 {
                return false;
            }
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&w| w == 0)
    }

    /// The bits of `self` that `other` does not have (`self & !other`).
    pub fn and_not(&self, other: &Signature) -> Signature {
        let mut bits = self.bits;
        for (w, o) in bits.iter_mut().zip(&other.bits) {
            *w &= !o;
        }
        Signature { bits }
    }

    /// The predicate bits of `self` that edges in direction `dir` set (see
    /// [`encode_query_edge`](Self::encode_query_edge) with a predicate and
    /// no neighbour). The two directions' predicate bits are disjoint, and
    /// no neighbour or combined bit falls among them, so an entity's
    /// signature holds such a bit iff the entity has an edge in direction
    /// `dir` whose predicate encodes to it.
    pub fn predicate_bits(&self, dir: EdgeDir) -> Signature {
        let mask = match dir {
            EdgeDir::In => &PRED_IN_MASK,
            EdgeDir::Out => &PRED_OUT_MASK,
        };
        let mut bits = self.bits;
        for (w, m) in bits.iter_mut().zip(mask) {
            *w &= m;
        }
        Signature { bits }
    }

    /// Whether every bit of `self` is a predicate bit (of either direction).
    pub fn only_predicate_bits(&self) -> bool {
        self.bits
            .iter()
            .zip(PRED_IN_MASK.iter().zip(&PRED_OUT_MASK))
            .all(|(w, (i, o))| w & !(i | o) == 0)
    }

    /// The positions of the set bits, ascending.
    pub fn set_bits(&self) -> impl Iterator<Item = usize> + '_ {
        self.bits.iter().enumerate().flat_map(|(i, &w)| {
            let mut w = w;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let b = w.trailing_zeros() as usize;
                w &= w - 1;
                Some(i * 64 + b)
            })
        })
    }

    /// The bit an edge with predicate `pre_id` in direction `dir` sets
    /// ([`encode_query_edge`](Self::encode_query_edge) with a predicate and
    /// no neighbour sets exactly this bit).
    pub fn predicate_bit(pre_id: PredId, dir: EdgeDir) -> usize {
        let id = pre_id as i64;
        let mut seed_num = (id % EDGE_SIG_INTERVAL_NUM_HALF as i64) as usize;
        if dir == EdgeDir::Out {
            seed_num += EDGE_SIG_INTERVAL_NUM_HALF;
        }
        let seed = id * 5003 % 49957;
        (seed as usize % EDGE_SIG_INTERVAL_BASE)
            + STR_SIG_LENGTH
            + EDGE_SIG_INTERVAL_BASE * seed_num
    }

    /// The predicate ids below `limit` whose edges in direction `dir` set
    /// `bit` ([`predicate_bit`](Self::predicate_bit)), ascending: the
    /// predicates an entity's signature can have that bit from. Empty for a
    /// bit that is not a predicate bit of `dir`. Visits a tenth of the ids
    /// (the bit fixes the id modulo [`EDGE_SIG_INTERVAL_NUM_HALF`]).
    pub fn predicates_with_bit(bit: usize, dir: EdgeDir, limit: PredId) -> Vec<PredId> {
        let half = EDGE_SIG_INTERVAL_BASE * EDGE_SIG_INTERVAL_NUM_HALF;
        let base = match dir {
            EdgeDir::In => STR_SIG_LENGTH,
            EdgeDir::Out => STR_SIG_LENGTH + half,
        };
        if bit < base || bit >= base + half {
            return Vec::new();
        }
        let first = ((bit - base) / EDGE_SIG_INTERVAL_BASE) as PredId;
        (first..limit)
            .step_by(EDGE_SIG_INTERVAL_NUM_HALF)
            .filter(|&p| Signature::predicate_bit(p, dir) == bit)
            .collect()
    }

    /// Compare the raw limbs, low limb first (each limb as a number, so bit
    /// 63 of a limb weighs most within it), ignoring the padding bits past
    /// [`ENTITY_SIG_LENGTH`] — the clustering order of the VS-tree bulk builds
    /// (the order of the limb vectors the builds once materialized per
    /// comparison).
    pub(crate) fn cmp_limbs(&self, other: &Signature) -> std::cmp::Ordering {
        let limbs = self.bits.iter().zip(&other.bits).zip(&LIMB_MASK);
        for ((&a, &b), &mask) in limbs {
            let (a, b) = (a & mask, b & mask);
            if a != b {
                return a.cmp(&b);
            }
        }
        std::cmp::Ordering::Equal
    }

    pub fn popcount(&self) -> u32 {
        self.bits.iter().map(|w| w.count_ones()).sum()
    }

    /// Hamming distance: popcount of `self XOR other`. Two signatures are "far
    /// apart" when they share few bits. Used by the VS-tree split heuristic to
    /// pick the two most dissimilar seeds (gStore's node-split seed selection).
    pub fn distance(&self, other: &Signature) -> u32 {
        let mut d = 0;
        for i in 0..LIMBS {
            d += (self.bits[i] ^ other.bits[i]).count_ones();
        }
        d
    }

    /// Number of bits set in `other` but not in `self` — the *enlargement* that
    /// unioning `other` into `self` would cause. Used by VS-tree insertion to
    /// descend into the child that grows least (minimal signature expansion).
    pub fn added_bits(&self, other: &Signature) -> u32 {
        let mut d = 0;
        for i in 0..LIMBS {
            d += (other.bits[i] & !self.bits[i]).count_ones();
        }
        d
    }

    /// Serialize the raw limbs little-endian into `out` (`out.len()` must be at
    /// least [`SIG_BYTES`]). The fixed-width encoding lets the out-of-core
    /// VS-tree store a signature in a constant-size slot inside a paged node.
    pub(crate) fn write_le(&self, out: &mut [u8]) {
        for (i, w) in self.bits.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
    }

    /// Reconstruct a signature from [`write_le`](Self::write_le) output (`b.len()`
    /// must be at least [`SIG_BYTES`]).
    pub(crate) fn read_le(b: &[u8]) -> Signature {
        let mut bits = [0u64; LIMBS];
        for (i, w) in bits.iter_mut().enumerate() {
            *w = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        }
        Signature { bits }
    }

    // --- encoding (ports of Signature.cpp) --------------------------------

    /// Encode a predicate bit (gStore `encodePredicate2Entity`, method 1).
    fn encode_predicate(&mut self, pre_id: PredId, dir: EdgeDir) {
        self.set(Signature::predicate_bit(pre_id, dir));
    }

    /// Encode a neighbour bit (gStore `encodeStr2Entity`).
    fn encode_str(&mut self, neighbor: EntityLiteralId, dir: EdgeDir) {
        let mut seed = neighbor as usize % STR_SIG_LITERAL;
        if is_literal_id(neighbor) {
            seed += STR_SIG_ENTITY;
        } else if dir == EdgeDir::Out {
            seed += STR_SIG_LITERAL;
        }
        self.set(seed);
    }

    /// Encode the combined predicate+neighbour bit (gStore `encodeEdge2Entity`
    /// tail), only valid when both predicate and neighbour are known.
    fn encode_combined(&mut self, pre_id: PredId, neighbor: EntityLiteralId, dir: EdgeDir) {
        let x = pre_id as usize % STR_AND_EDGE_INTERVAL_BASE;
        let y = neighbor as usize % STR_AND_EDGE_INTERVAL_BASE;
        // Cantor pairing, folded into the interval.
        let mut seed = x + (x + y + 1) * (x + y) / 2;
        seed %= STR_AND_EDGE_INTERVAL_BASE;
        seed += STR_SIG_LENGTH + EDGE_SIG_LENGTH;
        if is_literal_id(neighbor) {
            seed += STR_AND_EDGE_INTERVAL_BASE * 2;
        } else if dir == EdgeDir::Out {
            seed += STR_AND_EDGE_INTERVAL_BASE;
        }
        self.set(seed);
    }

    /// Encode a fully-known edge (used when building entity signatures):
    /// predicate + neighbour + combined (gStore `encodeEdge2Entity`).
    pub fn encode_edge(&mut self, pre_id: PredId, neighbor: EntityLiteralId, dir: EdgeDir) {
        self.encode_predicate(pre_id, dir);
        self.encode_str(neighbor, dir);
        self.encode_combined(pre_id, neighbor, dir);
    }

    /// Encode a *query* edge where the predicate and/or neighbour may be
    /// unknown (a variable). Only the known parts are set, which keeps the
    /// containment filter sound.
    pub fn encode_query_edge(
        &mut self,
        pre_id: Option<PredId>,
        neighbor: Option<EntityLiteralId>,
        dir: EdgeDir,
    ) {
        if let Some(p) = pre_id {
            self.encode_predicate(p, dir);
        }
        if let Some(n) = neighbor {
            self.encode_str(n, dir);
        }
        if let (Some(p), Some(n)) = (pre_id, neighbor) {
            self.encode_combined(p, n, dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::id::LITERAL_FIRST_ID;

    #[test]
    fn geometry_matches_gstore() {
        assert_eq!(STR_SIG_LENGTH, 600);
        assert_eq!(EDGE_SIG_LENGTH, 200);
        assert_eq!(STR_AND_EDGE_SIG_LENGTH, 144);
        assert_eq!(ENTITY_SIG_LENGTH, 944);
        assert_eq!(LIMBS, 15);
    }

    #[test]
    fn all_encoded_bits_are_in_range() {
        // Stress every encoder across a range of ids; must never panic / overflow.
        let mut sig = Signature::new();
        for p in 0..100u32 {
            for n in 0..300u32 {
                sig.encode_edge(p, n, EdgeDir::Out);
                sig.encode_edge(p, n, EdgeDir::In);
                sig.encode_edge(p, LITERAL_FIRST_ID + n, EdgeDir::Out);
            }
        }
        assert!(sig.popcount() > 0);
    }

    #[test]
    fn containment_is_reflexive_and_superset() {
        let mut a = Signature::new();
        a.encode_edge(1, 2, EdgeDir::Out);
        a.encode_edge(3, 4, EdgeDir::In);
        assert!(a.contains(&a));

        // a query sig with only one of a's edges is contained by a.
        let mut q = Signature::new();
        q.encode_query_edge(Some(1), Some(2), EdgeDir::Out);
        assert!(a.contains(&q));

        // an edge a does not have is not contained.
        let mut other = Signature::new();
        other.encode_query_edge(Some(9), Some(9), EdgeDir::Out);
        assert!(!a.contains(&other));
    }

    #[test]
    fn query_edge_is_subset_of_full_edge() {
        // Encoding the same edge as a query (known pred+neighbor) yields exactly
        // the bits of the full edge encoding ⇒ contained.
        let mut full = Signature::new();
        full.encode_edge(7, 42, EdgeDir::Out);
        let mut q = Signature::new();
        q.encode_query_edge(Some(7), Some(42), EdgeDir::Out);
        assert_eq!(full, q);
    }

    #[test]
    fn partial_query_edge_is_contained_in_full() {
        let mut full = Signature::new();
        full.encode_edge(7, 42, EdgeDir::Out);
        // predicate-only query (neighbor is a variable)
        let mut ponly = Signature::new();
        ponly.encode_query_edge(Some(7), None, EdgeDir::Out);
        assert!(full.contains(&ponly));
        assert!(ponly.popcount() < full.popcount());
    }

    #[test]
    fn predicate_bits_are_exactly_the_predicate_encodings() {
        for dir in [EdgeDir::In, EdgeDir::Out] {
            let other = match dir {
                EdgeDir::In => EdgeDir::Out,
                EdgeDir::Out => EdgeDir::In,
            };
            for p in 0..500u32 {
                let mut s = Signature::new();
                s.encode_query_edge(Some(p), None, dir);
                assert_eq!(
                    s.set_bits().collect::<Vec<_>>(),
                    [Signature::predicate_bit(p, dir)]
                );
                assert_eq!(s.predicate_bits(dir), s);
                assert!(s.predicate_bits(other).is_empty());
                assert!(s.only_predicate_bits());
                // A full edge adds neighbour and combined bits, none of them a
                // predicate bit.
                let mut full = Signature::new();
                full.encode_edge(p, p * 7 + 3, dir);
                full.encode_edge(p, LITERAL_FIRST_ID + p, dir);
                assert_eq!(full.predicate_bits(dir), s);
                assert!(full.predicate_bits(other).is_empty());
                assert!(!full.only_predicate_bits());
                assert!(full.and_not(&s).predicate_bits(dir).is_empty());
                assert_eq!(full.and_not(&s).popcount(), full.popcount() - 1);
            }
            // The inverse visits a tenth of the ids but finds them all.
            let limit = 1000;
            for bit in 0..ENTITY_SIG_LENGTH {
                let brute: Vec<PredId> = (0..limit)
                    .filter(|&p| Signature::predicate_bit(p, dir) == bit)
                    .collect();
                assert_eq!(
                    Signature::predicates_with_bit(bit, dir, limit),
                    brute,
                    "bit {bit}"
                );
            }
        }
    }

    #[test]
    fn set_bits_lists_every_set_bit_ascending() {
        let mut s = Signature::new();
        for b in [0, 1, 63, 64, 600, 799, 943] {
            s.set(b);
        }
        assert_eq!(
            s.set_bits().collect::<Vec<_>>(),
            [0, 1, 63, 64, 600, 799, 943]
        );
        assert_eq!(Signature::new().set_bits().count(), 0);
    }

    #[test]
    fn literal_and_entity_neighbors_differ() {
        let mut e = Signature::new();
        e.encode_edge(1, 5, EdgeDir::Out);
        let mut l = Signature::new();
        l.encode_edge(1, LITERAL_FIRST_ID + 5, EdgeDir::Out);
        assert_ne!(e, l);
    }
}
