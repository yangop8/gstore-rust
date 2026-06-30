//! The triple store: the six-way index over id-triples.
//!
//! Mirrors gStore's `KVstore` value indexes (`subID2values`, `objID2values`,
//! `preID2values`). gStore stores, per key, a packed byte list; here we keep
//! three sorted adjacency maps that, between them, answer every triple-pattern
//! shape in `O(log n + k)`:
//!
//! | map     | key  | sorted values        | answers                       |
//! |---------|------|----------------------|-------------------------------|
//! | `s2po`  | sub  | `(pred, obj)`        | `s??`, `sp?`, `s?o`, `spo`    |
//! | `o2ps`  | obj  | `(pred, sub)`        | `??o`, `?po`                  |
//! | `p2so`  | pred | `(sub, obj)`         | `?p?`                         |
//!
//! Each triple is stored in all three maps (space-for-speed, as in gStore).
//! Value vectors are kept sorted and de-duplicated, enabling binary-search
//! range scans and sort-merge joins in the query engine.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;

/// A sorted, de-duplicated adjacency map `key → Vec<(a, b)>`.
type AdjMap<K> = BTreeMap<K, Vec<(u32, u32)>>;

/// The triple-access interface the query engine evaluates against. Implemented
/// by the in-memory [`TripleStore`] and the on-disk `DiskStore`, so the same
/// optimizer + executor can run either fully in memory or *streaming* from disk
/// (reading only the index ranges a query touches). Owned `Vec`s keep the disk
/// implementation simple; the in-memory one clones its (small per-key) slices.
pub trait TripleSource {
    /// `s p o` — does this exact triple exist?
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool;
    /// `s ? ?` — `(pred, obj)` pairs for a subject.
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)>;
    /// `s p ?` — objects of `(sub, pred)`.
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId>;
    /// `s ? o` — predicates linking a subject to an object.
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId>;
    /// `? ? o` — `(pred, sub)` pairs for an object.
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)>;
    /// `? p o` — subjects of `(pred, obj)`.
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId>;
    /// `? p ?` — `(sub, obj)` pairs for a predicate.
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)>;
    /// Distinct subjects appearing with a predicate.
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId>;
    /// Distinct objects appearing with a predicate.
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId>;
    /// All ids that appear as a subject.
    fn subject_keys(&self) -> Vec<EntityLiteralId>;
    /// All ids that appear as an object.
    fn object_keys(&self) -> Vec<EntityLiteralId>;
    /// Total triple count.
    fn triple_count(&self) -> u64;
    /// Number of distinct subject keys.
    fn distinct_subjects(&self) -> usize;
    /// Number of distinct object keys.
    fn distinct_objects(&self) -> usize;
    /// Number of distinct predicates present.
    fn num_predicates(&self) -> usize;
    /// Number of triples with a predicate (gStore `pre2num`).
    fn pred_card(&self, pred: PredId) -> usize;
    /// Distinct subjects of a predicate (gStore `pre2sub`).
    fn pred_distinct_subj(&self, pred: PredId) -> usize;
    /// Distinct objects of a predicate (gStore `pre2obj`).
    fn pred_distinct_obj(&self, pred: PredId) -> usize;
    /// Every triple (for the all-variable `? ? ?` scan).
    fn iter_all(&self) -> Vec<IdTriple>;
}

impl TripleSource for TripleStore {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        TripleStore::exists(self, sub, pred, obj)
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        TripleStore::po_by_s(self, sub).to_vec()
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        TripleStore::o_by_sp(self, sub, pred)
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        TripleStore::p_by_so(self, sub, obj)
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        TripleStore::ps_by_o(self, obj).to_vec()
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        TripleStore::s_by_po(self, pred, obj)
    }
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        TripleStore::so_by_p(self, pred).to_vec()
    }
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        TripleStore::subs_by_p(self, pred)
    }
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        TripleStore::objs_by_p(self, pred)
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        TripleStore::subject_keys(self).collect()
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        TripleStore::object_keys(self).collect()
    }
    fn triple_count(&self) -> u64 {
        TripleStore::triple_count(self)
    }
    fn distinct_subjects(&self) -> usize {
        TripleStore::distinct_subjects(self)
    }
    fn distinct_objects(&self) -> usize {
        TripleStore::distinct_objects(self)
    }
    fn num_predicates(&self) -> usize {
        TripleStore::num_predicates(self)
    }
    fn pred_card(&self, pred: PredId) -> usize {
        TripleStore::pred_card(self, pred)
    }
    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        TripleStore::pred_distinct_subj(self, pred)
    }
    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        TripleStore::pred_distinct_obj(self, pred)
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        TripleStore::iter_all(self).collect()
    }
}

/// The triple store. Holds three redundant sorted indexes plus a triple count.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TripleStore {
    /// sub → sorted [(pred, obj)]
    s2po: AdjMap<EntityLiteralId>,
    /// obj → sorted [(pred, sub)]
    o2ps: AdjMap<EntityLiteralId>,
    /// pred → sorted [(sub, obj)]
    p2so: AdjMap<PredId>,
    triple_count: u64,
}

/// Insert `value` into a sorted vector if absent. Returns `true` if inserted.
fn sorted_insert(vec: &mut Vec<(u32, u32)>, value: (u32, u32)) -> bool {
    match vec.binary_search(&value) {
        Ok(_) => false,
        Err(pos) => {
            vec.insert(pos, value);
            true
        }
    }
}

/// Remove `value` from a sorted vector if present. Returns `true` if removed.
fn sorted_remove(vec: &mut Vec<(u32, u32)>, value: (u32, u32)) -> bool {
    match vec.binary_search(&value) {
        Ok(pos) => {
            vec.remove(pos);
            true
        }
        Err(_) => false,
    }
}

/// Return the slice of `vec` whose first component equals `first`.
/// `vec` must be sorted ascending by `(first, second)`.
fn range_by_first(vec: &[(u32, u32)], first: u32) -> &[(u32, u32)] {
    let lo = vec.partition_point(|&(a, _)| a < first);
    let hi = vec.partition_point(|&(a, _)| a <= first);
    &vec[lo..hi]
}

impl TripleStore {
    pub fn new() -> TripleStore {
        TripleStore::default()
    }

    pub fn triple_count(&self) -> u64 {
        self.triple_count
    }

    pub fn is_empty(&self) -> bool {
        self.triple_count == 0
    }

    // ---- mutation ---------------------------------------------------------

    /// Insert a triple. Returns `false` (and changes nothing) if it already
    /// exists. Maintains all three indexes in sorted order.
    pub fn insert(&mut self, t: IdTriple) -> bool {
        let IdTriple { sub, pred, obj } = t;
        let inserted = sorted_insert(self.s2po.entry(sub).or_default(), (pred, obj));
        if !inserted {
            return false;
        }
        sorted_insert(self.o2ps.entry(obj).or_default(), (pred, sub));
        sorted_insert(self.p2so.entry(pred).or_default(), (sub, obj));
        self.triple_count += 1;
        true
    }

    /// Remove a triple. Returns `false` if it was not present.
    pub fn remove(&mut self, t: IdTriple) -> bool {
        let IdTriple { sub, pred, obj } = t;
        let existed = match self.s2po.get_mut(&sub) {
            Some(v) => sorted_remove(v, (pred, obj)),
            None => false,
        };
        if !existed {
            return false;
        }
        if let Some(v) = self.o2ps.get_mut(&obj) {
            sorted_remove(v, (pred, sub));
        }
        if let Some(v) = self.p2so.get_mut(&pred) {
            sorted_remove(v, (sub, obj));
        }
        self.prune_empty(sub, pred, obj);
        self.triple_count -= 1;
        true
    }

    /// Drop now-empty adjacency entries so key-iteration stays tight.
    fn prune_empty(&mut self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) {
        if self.s2po.get(&sub).is_some_and(Vec::is_empty) {
            self.s2po.remove(&sub);
        }
        if self.o2ps.get(&obj).is_some_and(Vec::is_empty) {
            self.o2ps.remove(&obj);
        }
        if self.p2so.get(&pred).is_some_and(Vec::is_empty) {
            self.p2so.remove(&pred);
        }
    }

    /// Bulk-load triples efficiently: push everything, then sort + de-dup each
    /// adjacency list once. Use this for initial build; ~O(n log n) vs the
    /// O(n²)-ish cost of repeated [`insert`](Self::insert).
    pub fn bulk_load(&mut self, triples: impl IntoIterator<Item = IdTriple>) {
        for t in triples {
            let IdTriple { sub, pred, obj } = t;
            self.s2po.entry(sub).or_default().push((pred, obj));
            self.o2ps.entry(obj).or_default().push((pred, sub));
            self.p2so.entry(pred).or_default().push((sub, obj));
        }
        let mut total: u64 = 0;
        for v in self.s2po.values_mut() {
            v.sort_unstable();
            v.dedup();
            total += v.len() as u64;
        }
        for v in self.o2ps.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        for v in self.p2so.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        // `total` counts distinct triples (s2po dedup removes exact repeats).
        self.triple_count = total;
    }

    // ---- access patterns --------------------------------------------------

    /// `s p o` — does this exact triple exist?
    pub fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        self.s2po
            .get(&sub)
            .is_some_and(|v| v.binary_search(&(pred, obj)).is_ok())
    }

    /// `s ? ?` — all `(pred, obj)` pairs for a subject.
    pub fn po_by_s(&self, sub: EntityLiteralId) -> &[(PredId, EntityLiteralId)] {
        self.s2po.get(&sub).map_or(&[], Vec::as_slice)
    }

    /// `s p ?` — objects of `(sub, pred)`.
    pub fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        match self.s2po.get(&sub) {
            Some(v) => range_by_first(v, pred).iter().map(|&(_, o)| o).collect(),
            None => Vec::new(),
        }
    }

    /// `s ? o` — predicates linking a given subject to a given object (so2p).
    pub fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        match self.s2po.get(&sub) {
            Some(v) => {
                let mut ps: Vec<PredId> = v
                    .iter()
                    .filter(|&&(_, o)| o == obj)
                    .map(|&(p, _)| p)
                    .collect();
                ps.dedup();
                ps
            }
            None => Vec::new(),
        }
    }

    /// `? ? o` — all `(pred, sub)` pairs for an object.
    pub fn ps_by_o(&self, obj: EntityLiteralId) -> &[(PredId, EntityLiteralId)] {
        self.o2ps.get(&obj).map_or(&[], Vec::as_slice)
    }

    /// `? p o` — subjects of `(pred, obj)`.
    pub fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        match self.o2ps.get(&obj) {
            Some(v) => range_by_first(v, pred).iter().map(|&(_, s)| s).collect(),
            None => Vec::new(),
        }
    }

    /// `? p ?` — all `(sub, obj)` pairs for a predicate.
    pub fn so_by_p(&self, pred: PredId) -> &[(EntityLiteralId, EntityLiteralId)] {
        self.p2so.get(&pred).map_or(&[], Vec::as_slice)
    }

    /// Distinct subjects appearing with this predicate.
    pub fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut subs: Vec<EntityLiteralId> = self.so_by_p(pred).iter().map(|&(s, _)| s).collect();
        subs.dedup(); // already sorted by (s, o)
        subs
    }

    /// Distinct objects appearing with this predicate.
    pub fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut objs: Vec<EntityLiteralId> = self.so_by_p(pred).iter().map(|&(_, o)| o).collect();
        objs.sort_unstable();
        objs.dedup();
        objs
    }

    /// In + out degree of an entity (as subject or object).
    pub fn degree(&self, id: EntityLiteralId) -> usize {
        self.out_degree(id) + self.in_degree(id)
    }
    /// Out degree: number of triples where `id` is the subject.
    pub fn out_degree(&self, id: EntityLiteralId) -> usize {
        self.s2po.get(&id).map_or(0, Vec::len)
    }
    /// In degree: number of triples where `id` is the object.
    pub fn in_degree(&self, id: EntityLiteralId) -> usize {
        self.o2ps.get(&id).map_or(0, Vec::len)
    }

    /// `? ? ?` — iterate every triple (driven by the predicate index).
    pub fn iter_all(&self) -> impl Iterator<Item = IdTriple> + '_ {
        self.p2so.iter().flat_map(|(&pred, pairs)| {
            pairs
                .iter()
                .map(move |&(sub, obj)| IdTriple::new(sub, pred, obj))
        })
    }

    /// All predicate ids that occur in the store (sorted).
    pub fn predicates(&self) -> impl Iterator<Item = PredId> + '_ {
        self.p2so.keys().copied()
    }

    /// All ids that appear as a subject (always entities), sorted.
    pub fn subject_keys(&self) -> impl Iterator<Item = EntityLiteralId> + '_ {
        self.s2po.keys().copied()
    }

    /// All ids that appear as an object (entities or literals), sorted.
    pub fn object_keys(&self) -> impl Iterator<Item = EntityLiteralId> + '_ {
        self.o2ps.keys().copied()
    }

    // ---- statistics (for the cost-based optimizer) -----------------------

    /// Number of distinct subject keys (≈ distinct entities used as subjects).
    pub fn distinct_subjects(&self) -> usize {
        self.s2po.len()
    }

    /// Number of distinct object keys.
    pub fn distinct_objects(&self) -> usize {
        self.o2ps.len()
    }

    /// Number of distinct predicates present.
    pub fn num_predicates(&self) -> usize {
        self.p2so.len()
    }

    /// Number of triples with predicate `p` (gStore: `pre2num`).
    pub fn pred_card(&self, pred: PredId) -> usize {
        self.so_by_p(pred).len()
    }

    /// Distinct subjects appearing with predicate `p` (gStore: `pre2sub`).
    /// O(card) but allocation-free: `so_by_p` is sorted by `(sub, obj)`, so
    /// distinct subjects are the count of first-component transitions.
    pub fn pred_distinct_subj(&self, pred: PredId) -> usize {
        let pairs = self.so_by_p(pred);
        let mut count = 0usize;
        let mut last: Option<u32> = None;
        for &(s, _) in pairs {
            if last != Some(s) {
                count += 1;
                last = Some(s);
            }
        }
        count
    }

    /// Distinct objects appearing with predicate `p` (gStore: `pre2obj`).
    pub fn pred_distinct_obj(&self, pred: PredId) -> usize {
        // Not sorted by object, so collect distinct via a set.
        let pairs = self.so_by_p(pred);
        let mut objs: Vec<u32> = pairs.iter().map(|&(_, o)| o).collect();
        objs.sort_unstable();
        objs.dedup();
        objs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the classic gStore `small.nt` shape (subset) in id-space.
    /// root(0) -name-> "Bookug"(L0); root -contain-> node0..node4 (1..5)
    fn sample() -> TripleStore {
        // preds: name=0, contain=1, own=2
        let triples = vec![
            IdTriple::new(0, 0, 2_000_000_000), // root name "Bookug"
            IdTriple::new(0, 1, 1),             // root contain node0
            IdTriple::new(0, 1, 2),             // root contain node1
            IdTriple::new(0, 1, 3),             // root contain node2
            IdTriple::new(2, 2, 10),            // node1 own point0
            IdTriple::new(2, 2, 11),            // node1 own point1
            IdTriple::new(3, 2, 12),            // node2 own point2
        ];
        let mut s = TripleStore::new();
        s.bulk_load(triples);
        s
    }

    #[test]
    fn bulk_load_counts_distinct_triples() {
        let s = sample();
        assert_eq!(s.triple_count(), 7);
    }

    #[test]
    fn exists_checks_exact_spo() {
        let s = sample();
        assert!(s.exists(0, 1, 1));
        assert!(!s.exists(0, 1, 9));
        assert!(!s.exists(9, 9, 9));
    }

    #[test]
    fn s_pattern_returns_all_po() {
        let s = sample();
        let po = s.po_by_s(0);
        // root: name->L0, contain->1, contain->2, contain->3 (sorted by pred,obj)
        assert_eq!(po, &[(0, 2_000_000_000), (1, 1), (1, 2), (1, 3)]);
    }

    #[test]
    fn sp_pattern_returns_objects() {
        let s = sample();
        let mut objs = s.o_by_sp(0, 1); // root contain ?
        objs.sort_unstable();
        assert_eq!(objs, vec![1, 2, 3]);
        assert_eq!(s.o_by_sp(0, 0), vec![2_000_000_000]); // root name ?
        assert!(s.o_by_sp(0, 99).is_empty());
    }

    #[test]
    fn po_pattern_returns_subjects() {
        let s = sample();
        // who 'contain' node1(=2)? root(=0)
        assert_eq!(s.s_by_po(1, 2), vec![0]);
        // who own point0(=10)? node1(=2)
        assert_eq!(s.s_by_po(2, 10), vec![2]);
        assert!(s.s_by_po(2, 999).is_empty());
    }

    #[test]
    fn so_pattern_returns_predicates() {
        let s = sample();
        assert_eq!(s.p_by_so(0, 1), vec![1]); // root -> node0 via contain
        assert!(s.p_by_so(0, 999).is_empty());
    }

    #[test]
    fn p_pattern_returns_so_pairs() {
        let s = sample();
        let so = s.so_by_p(2); // 'own'
        assert_eq!(so, &[(2, 10), (2, 11), (3, 12)]);
        assert_eq!(s.subs_by_p(2), vec![2, 3]);
        assert_eq!(s.objs_by_p(2), vec![10, 11, 12]);
    }

    #[test]
    fn o_pattern_returns_ps_pairs() {
        let s = sample();
        // object node0(=1): (contain=1, root=0)
        assert_eq!(s.ps_by_o(1), &[(1, 0)]);
    }

    #[test]
    fn insert_then_remove_is_symmetric() {
        let mut s = sample();
        let before = s.triple_count();
        assert!(s.insert(IdTriple::new(5, 2, 99)));
        assert!(!s.insert(IdTriple::new(5, 2, 99))); // dup rejected
        assert_eq!(s.triple_count(), before + 1);
        assert!(s.exists(5, 2, 99));
        assert!(s.remove(IdTriple::new(5, 2, 99)));
        assert!(!s.remove(IdTriple::new(5, 2, 99))); // already gone
        assert_eq!(s.triple_count(), before);
        assert!(!s.exists(5, 2, 99));
    }

    #[test]
    fn remove_prunes_all_indexes() {
        let mut s = TripleStore::new();
        s.insert(IdTriple::new(7, 3, 8));
        assert!(s.remove(IdTriple::new(7, 3, 8)));
        assert!(s.po_by_s(7).is_empty());
        assert!(s.ps_by_o(8).is_empty());
        assert!(s.so_by_p(3).is_empty());
        assert_eq!(s.triple_count(), 0);
    }

    #[test]
    fn iter_all_yields_every_triple_once() {
        let s = sample();
        let mut all: Vec<IdTriple> = s.iter_all().collect();
        all.sort();
        assert_eq!(all.len(), 7);
        assert!(all.contains(&IdTriple::new(0, 1, 1)));
        assert!(all.contains(&IdTriple::new(3, 2, 12)));
    }

    #[test]
    fn degrees_count_in_and_out() {
        let s = sample();
        assert_eq!(s.out_degree(0), 4); // root has 4 outgoing
        assert_eq!(s.in_degree(0), 0);
        assert_eq!(s.in_degree(1), 1); // node0 is object once
        assert_eq!(s.degree(2), 2 + 1); // node1: 2 out (own), 1 in (contain)
    }
}
