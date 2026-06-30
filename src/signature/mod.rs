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

mod vstree;

pub use vstree::VsTree;

use serde::{Deserialize, Serialize};

use crate::model::id::{is_literal_id, EntityLiteralId, PredId};

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

    pub fn popcount(&self) -> u32 {
        self.bits.iter().map(|w| w.count_ones()).sum()
    }

    // --- encoding (ports of Signature.cpp) --------------------------------

    /// Encode a predicate bit (gStore `encodePredicate2Entity`, method 1).
    fn encode_predicate(&mut self, pre_id: PredId, dir: EdgeDir) {
        let id = pre_id as i64;
        let mut seed_num = (id % EDGE_SIG_INTERVAL_NUM_HALF as i64) as usize;
        if dir == EdgeDir::Out {
            seed_num += EDGE_SIG_INTERVAL_NUM_HALF;
        }
        let seed = id * 5003 % 49957;
        let pos = (seed as usize % EDGE_SIG_INTERVAL_BASE)
            + STR_SIG_LENGTH
            + EDGE_SIG_INTERVAL_BASE * seed_num;
        self.set(pos);
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

