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

mod backend;
pub mod read_scope;

pub use backend::Backend;

/// A sorted, de-duplicated adjacency map `key → Vec<(a, b)>`.
type AdjMap<K> = BTreeMap<K, Vec<(u32, u32)>>;

/// The triple-access interface the query engine evaluates against. Implemented
/// by the in-memory [`TripleStore`] and the on-disk `DiskStore`, so the same
/// optimizer + executor can run either fully in memory or *streaming* from disk
/// (reading only the index ranges a query touches). Owned `Vec`s keep the disk
/// implementation simple; the in-memory one clones its (small per-key) slices.
///
/// ## Ordering contract
///
/// Every implementor returns its lists in this order, so that plans and the
/// order of solutions never depend on the backend:
///
/// * [`po_by_s`](Self::po_by_s) returns `(p, o)` pairs, [`ps_by_o`](Self::ps_by_o)
///   returns `(p, s)` pairs and [`so_by_p`](Self::so_by_p) returns `(s, o)`
///   pairs — each list sorted ascending by the pair, without duplicates;
/// * [`o_by_sp`](Self::o_by_sp), [`s_by_po`](Self::s_by_po),
///   [`p_by_so`](Self::p_by_so), [`subs_by_p`](Self::subs_by_p),
///   [`objs_by_p`](Self::objs_by_p) and the key lists
///   ([`subject_keys`](Self::subject_keys), [`object_keys`](Self::object_keys))
///   are ascending and duplicate-free;
/// * [`iter_all`](Self::iter_all) returns triples in `(p, s, o)` order.
///
/// ## Count methods (provided)
///
/// The `count_*`, `distinct_p_of_*` and `has_*` methods have defaults that are
/// correct for every implementor (they are defined through the list methods).
/// A backend may override them to answer without materializing the list —
/// [`TripleStore`] does with slice arithmetic, an on-disk store can count a key
/// prefix — but an override must return **exactly** what the default returns,
/// so that the cost model, and with it every plan and solution order, never
/// changes. On a read error (reported through
/// [`read_scope::note_read_error`]) the `has_*` methods answer `true`: an id is
/// never treated as unused when in doubt.
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

    // ---- provided counts (contract C1) -------------------------------------

    /// `s ? ?` — `po_by_s(sub).len()`.
    fn count_po_by_s(&self, sub: EntityLiteralId) -> u64 {
        self.po_by_s(sub).len() as u64
    }
    /// `s p ?` — `o_by_sp(sub, pred).len()`.
    fn count_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> u64 {
        self.o_by_sp(sub, pred).len() as u64
    }
    /// `s ? o` — `p_by_so(sub, obj).len()`.
    fn count_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> u64 {
        self.p_by_so(sub, obj).len() as u64
    }
    /// `? ? o` — `ps_by_o(obj).len()`.
    fn count_ps_by_o(&self, obj: EntityLiteralId) -> u64 {
        self.ps_by_o(obj).len() as u64
    }
    /// `? p o` — `s_by_po(pred, obj).len()`.
    fn count_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> u64 {
        self.s_by_po(pred, obj).len() as u64
    }
    /// `? p ?` — `so_by_p(pred).len()`.
    fn count_so_by_p(&self, pred: PredId) -> u64 {
        self.so_by_p(pred).len() as u64
    }
    /// The number of distinct predicates of subject `sub`: the runs of equal
    /// first components in [`po_by_s`](Self::po_by_s) (sorted by the ordering
    /// contract).
    fn distinct_p_of_s(&self, sub: EntityLiteralId) -> u64 {
        distinct_first(&self.po_by_s(sub))
    }
    /// The number of distinct predicates of object `obj`: the runs of equal
    /// first components in [`ps_by_o`](Self::ps_by_o).
    fn distinct_p_of_o(&self, obj: EntityLiteralId) -> u64 {
        distinct_first(&self.ps_by_o(obj))
    }
    /// Whether `sub` is the subject of some triple. `true` when the read
    /// failed (see the trait docs).
    fn has_subject(&self, sub: EntityLiteralId) -> bool {
        read_scope::true_on_read_error(|| !self.po_by_s(sub).is_empty())
    }
    /// Whether `obj` is the object of some triple. `true` when the read
    /// failed.
    fn has_object(&self, obj: EntityLiteralId) -> bool {
        read_scope::true_on_read_error(|| !self.ps_by_o(obj).is_empty())
    }
    /// Whether some triple has predicate `pred`. `true` when the read failed.
    fn has_predicate(&self, pred: PredId) -> bool {
        read_scope::true_on_read_error(|| !self.so_by_p(pred).is_empty())
    }
}

/// The number of runs of equal first components in `pairs` — the number of
/// distinct first components of a list sorted by them.
pub(crate) fn distinct_first(pairs: &[(u32, u32)]) -> u64 {
    let mut count = 0u64;
    let mut last: Option<u32> = None;
    for &(a, _) in pairs {
        if last != Some(a) {
            count += 1;
            last = Some(a);
        }
    }
    count
}

/// The triple-*mutation* interface — the write counterpart of [`TripleSource`].
/// Implemented by writable backends (the in-memory [`TripleStore`] today; a
/// future RocksDB / MySQL backend) so the [`Database`](crate::Database) facade
/// can insert and delete triples without depending on a concrete storage
/// representation. Kept object-safe (concrete argument/return types only) so a
/// backend can also be held behind `dyn`.
pub trait MutableStore {
    /// Insert one id-triple; returns `true` if it was newly added.
    fn insert(&mut self, t: IdTriple) -> bool;
    /// Remove one id-triple; returns `true` if it existed.
    fn remove(&mut self, t: IdTriple) -> bool;
    /// Bulk-load many id-triples (de-duplicated), rebuilding indexes once.
    fn bulk_load(&mut self, triples: Vec<IdTriple>);
}

/// A complete, writable storage backend: queryable ([`TripleSource`]) and
/// mutable ([`MutableStore`]). The query engine, the cost optimizer, the
/// VS-tree, and the graph-analytics layer all operate purely through these two
/// traits, so the byte-level storage — in-memory maps today, an on-disk B+ tree,
/// or a future RocksDB / MySQL backend — is pluggable without touching anything
/// above this seam. (This is gStore's real core value: the index + optimizer +
/// algorithms, decoupled from where triples physically live.)
pub trait StorageBackend: TripleSource + MutableStore {}
impl<T: TripleSource + MutableStore> StorageBackend for T {}

impl MutableStore for TripleStore {
    fn insert(&mut self, t: IdTriple) -> bool {
        TripleStore::insert(self, t)
    }
    fn remove(&mut self, t: IdTriple) -> bool {
        TripleStore::remove(self, t)
    }
    fn bulk_load(&mut self, triples: Vec<IdTriple>) {
        TripleStore::bulk_load(self, triples)
    }
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

    // C1: slice arithmetic, no list is materialized. Each equals the default.

    fn count_po_by_s(&self, sub: EntityLiteralId) -> u64 {
        TripleStore::po_by_s(self, sub).len() as u64
    }
    fn count_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> u64 {
        self.s2po
            .get(&sub)
            .map_or(0, |v| range_by_first(v, pred).len()) as u64
    }
    fn count_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> u64 {
        // `(p, o)` pairs are unique per subject, so with `o` fixed every
        // matching pair has its own predicate (the default's `dedup` is a no-op).
        self.s2po
            .get(&sub)
            .map_or(0, |v| v.iter().filter(|&&(_, o)| o == obj).count()) as u64
    }
    fn count_ps_by_o(&self, obj: EntityLiteralId) -> u64 {
        TripleStore::ps_by_o(self, obj).len() as u64
    }
    fn count_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> u64 {
        self.o2ps
            .get(&obj)
            .map_or(0, |v| range_by_first(v, pred).len()) as u64
    }
    fn count_so_by_p(&self, pred: PredId) -> u64 {
        TripleStore::so_by_p(self, pred).len() as u64
    }
    fn distinct_p_of_s(&self, sub: EntityLiteralId) -> u64 {
        distinct_first(TripleStore::po_by_s(self, sub))
    }
    fn distinct_p_of_o(&self, obj: EntityLiteralId) -> u64 {
        distinct_first(TripleStore::ps_by_o(self, obj))
    }
    fn has_subject(&self, sub: EntityLiteralId) -> bool {
        !TripleStore::po_by_s(self, sub).is_empty()
    }
    fn has_object(&self, obj: EntityLiteralId) -> bool {
        !TripleStore::ps_by_o(self, obj).is_empty()
    }
    fn has_predicate(&self, pred: PredId) -> bool {
        !TripleStore::so_by_p(self, pred).is_empty()
    }
}

/// The triple store. Holds three redundant sorted indexes plus a triple count.
///
/// It also keeps, per predicate, its number of distinct subjects and objects
/// (gStore's `pre2sub` / `pre2obj`), maintained exactly by every mutation, so
/// the cost model reads them in O(log P) instead of scanning the predicate's
/// triples on every query. They are derived data: not serialized (`store.bin`
/// holds exactly the four index fields) and recomputed when a store is read.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(from = "TripleStoreData")]
pub struct TripleStore {
    /// sub → sorted [(pred, obj)]
    s2po: AdjMap<EntityLiteralId>,
    /// obj → sorted [(pred, sub)]
    o2ps: AdjMap<EntityLiteralId>,
    /// pred → sorted [(sub, obj)]
    p2so: AdjMap<PredId>,
    triple_count: u64,
    /// pred → (distinct subjects, distinct objects); an entry per predicate
    /// that has triples.
    #[serde(skip)]
    pred_stats: BTreeMap<PredId, (usize, usize)>,
}

/// The serialized form of a [`TripleStore`] (its four index fields, as
/// derived before the statistics existed); read back through `From`, which
/// recomputes the statistics.
#[derive(Serialize, Deserialize)]
#[serde(rename = "TripleStore")]
struct TripleStoreData {
    s2po: AdjMap<EntityLiteralId>,
    o2ps: AdjMap<EntityLiteralId>,
    p2so: AdjMap<PredId>,
    triple_count: u64,
}

impl From<TripleStoreData> for TripleStore {
    fn from(d: TripleStoreData) -> TripleStore {
        let mut store = TripleStore {
            s2po: d.s2po,
            o2ps: d.o2ps,
            p2so: d.p2so,
            triple_count: d.triple_count,
            pred_stats: BTreeMap::new(),
        };
        store.recompute_pred_stats();
        store
    }
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

/// An adjacency map from `(key, value)` pairs sorted by key, then value, and
/// free of duplicates: one exactly sized list per run of equal keys.
fn group_sorted(pairs: impl Iterator<Item = (u32, (u32, u32))> + Clone) -> AdjMap<u32> {
    let mut runs: Vec<(u32, Vec<(u32, u32)>)> = Vec::new();
    let mut it = pairs.peekable();
    while let Some(&(key, _)) = it.peek() {
        // Count the run first, so its list is allocated once, at its size.
        let len = it.clone().take_while(|&(k, _)| k == key).count();
        let mut list = Vec::with_capacity(len);
        list.extend(it.by_ref().take(len).map(|(_, v)| v));
        runs.push((key, list));
    }
    // Sorted, unique keys: the map is built in one pass.
    runs.into_iter().collect()
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
        let po = self.s2po.entry(sub).or_default();
        let inserted = sorted_insert(po, (pred, obj));
        if !inserted {
            return false;
        }
        // The first `(sub, pred, _)` / `(_, pred, obj)` adds a distinct
        // subject / object to `pred`.
        let new_subject = range_by_first(po, pred).len() == 1;
        let ps = self.o2ps.entry(obj).or_default();
        sorted_insert(ps, (pred, sub));
        let new_object = range_by_first(ps, pred).len() == 1;
        sorted_insert(self.p2so.entry(pred).or_default(), (sub, obj));
        let stat = self.pred_stats.entry(pred).or_default();
        stat.0 += usize::from(new_subject);
        stat.1 += usize::from(new_object);
        self.triple_count += 1;
        true
    }

    /// Remove a triple. Returns `false` if it was not present.
    pub fn remove(&mut self, t: IdTriple) -> bool {
        let IdTriple { sub, pred, obj } = t;
        let (existed, last_subject) = match self.s2po.get_mut(&sub) {
            Some(v) => {
                let removed = sorted_remove(v, (pred, obj));
                (removed, removed && range_by_first(v, pred).is_empty())
            }
            None => (false, false),
        };
        if !existed {
            return false;
        }
        let mut last_object = false;
        if let Some(v) = self.o2ps.get_mut(&obj) {
            sorted_remove(v, (pred, sub));
            last_object = range_by_first(v, pred).is_empty();
        }
        if let Some(v) = self.p2so.get_mut(&pred) {
            sorted_remove(v, (sub, obj));
        }
        if let Some(stat) = self.pred_stats.get_mut(&pred) {
            stat.0 -= usize::from(last_subject);
            stat.1 -= usize::from(last_object);
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
            self.pred_stats.remove(&pred);
        }
    }

    /// Recompute every predicate's distinct subject / object counts from the
    /// indexes (after a bulk load or a read).
    fn recompute_pred_stats(&mut self) {
        let mut stats: BTreeMap<PredId, (usize, usize)> = BTreeMap::new();
        for (&pred, pairs) in &self.p2so {
            if pairs.is_empty() {
                continue;
            }
            stats.insert(pred, (distinct_first(pairs) as usize, 0));
        }
        // Each object's list is sorted by predicate: one run per predicate.
        for ps in self.o2ps.values() {
            let mut last: Option<PredId> = None;
            for &(pred, _) in ps {
                if last != Some(pred) {
                    last = Some(pred);
                    if let Some(stat) = stats.get_mut(&pred) {
                        stat.1 += 1;
                    }
                }
            }
        }
        self.pred_stats = stats;
    }

    /// Bulk-load triples efficiently: push everything, then sort + de-dup each
    /// adjacency list once. Use this for initial build; ~O(n log n) vs the
    /// O(n²)-ish cost of repeated [`insert`](Self::insert).
    pub fn bulk_load(&mut self, triples: impl IntoIterator<Item = IdTriple>) {
        if self.s2po.is_empty() && self.o2ps.is_empty() && self.p2so.is_empty() {
            self.bulk_load_into_empty(triples.into_iter().collect());
            return;
        }
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
        self.recompute_pred_stats();
    }

    /// [`bulk_load`](Self::bulk_load) into an empty store: sort the triples
    /// once per index order and build each map from its sorted runs (exactly
    /// sized lists, maps built from sorted input) instead of a map lookup per
    /// triple and index. The result equals the general path's.
    fn bulk_load_into_empty(&mut self, mut triples: Vec<IdTriple>) {
        triples.sort_unstable_by_key(|t| (t.sub, t.pred, t.obj));
        triples.dedup();
        self.triple_count = triples.len() as u64;
        self.s2po = group_sorted(triples.iter().map(|t| (t.sub, (t.pred, t.obj))));
        triples.sort_unstable_by_key(|t| (t.obj, t.pred, t.sub));
        self.o2ps = group_sorted(triples.iter().map(|t| (t.obj, (t.pred, t.sub))));
        triples.sort_unstable_by_key(|t| (t.pred, t.sub, t.obj));
        self.p2so = group_sorted(triples.iter().map(|t| (t.pred, (t.sub, t.obj))));
        self.recompute_pred_stats();
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

    /// Visit every entity — each subject, and each object that is an entity
    /// (not a literal) — in ascending id order, with its outgoing `(p, o)` and
    /// incoming `(p, s)` lists (either may be empty). The VS-tree signs
    /// entities from exactly these lists; this walks both maps once, without
    /// a lookup or a copy per entity.
    pub fn for_each_entity_adjacency(
        &self,
        mut f: impl FnMut(EntityLiteralId, &[(PredId, EntityLiteralId)], &[(PredId, EntityLiteralId)]),
    ) {
        let mut outs = self.s2po.iter().peekable();
        let mut ins = self
            .o2ps
            .iter()
            .filter(|(&o, _)| crate::model::id::is_entity_id(o))
            .peekable();
        loop {
            match (outs.peek(), ins.peek()) {
                (None, None) => break,
                (Some(&(&s, out)), None) => {
                    f(s, out, &[]);
                    outs.next();
                }
                (None, Some(&(&o, inn))) => {
                    f(o, &[], inn);
                    ins.next();
                }
                (Some(&(&s, out)), Some(&(&o, inn))) => match s.cmp(&o) {
                    std::cmp::Ordering::Less => {
                        f(s, out, &[]);
                        outs.next();
                    }
                    std::cmp::Ordering::Greater => {
                        f(o, &[], inn);
                        ins.next();
                    }
                    std::cmp::Ordering::Equal => {
                        f(s, out, inn);
                        outs.next();
                        ins.next();
                    }
                },
            }
        }
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

    /// Distinct subjects appearing with predicate `p` (gStore: `pre2sub`),
    /// from the maintained statistics: O(log P).
    pub fn pred_distinct_subj(&self, pred: PredId) -> usize {
        self.pred_stats.get(&pred).map_or(0, |&(subjects, _)| subjects)
    }

    /// Distinct objects appearing with predicate `p` (gStore: `pre2obj`),
    /// from the maintained statistics: O(log P).
    pub fn pred_distinct_obj(&self, pred: PredId) -> usize {
        self.pred_stats.get(&pred).map_or(0, |&(_, objects)| objects)
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

    /// One generic function drives a backend's *write* face ([`MutableStore`])
    /// and *read* face ([`TripleSource`]) without naming a concrete type — the
    /// pluggability proof. The same `exercise` body would accept a future
    /// RocksDB / MySQL `StorageBackend` unchanged.
    #[test]
    fn storage_backend_seam_is_generic() {
        fn exercise<B: StorageBackend>(b: &mut B) {
            b.bulk_load(vec![IdTriple::new(1, 2, 3), IdTriple::new(1, 2, 4)]);
            assert!(b.insert(IdTriple::new(1, 2, 5))); // write via MutableStore
            assert!(!b.insert(IdTriple::new(1, 2, 5))); // duplicate
            assert!(b.exists(1, 2, 5)); // read via TripleSource
            assert_eq!(b.o_by_sp(1, 2).len(), 3);
            assert!(b.remove(IdTriple::new(1, 2, 5)));
            assert_eq!(b.triple_count(), 2);
        }
        let mut s = TripleStore::new();
        exercise(&mut s);
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

    /// The statistics and bulk-load paths the store keeps are exact: for
    /// random data under inserts, removes and bulk loads, the maintained
    /// per-predicate counts equal a brute-force count, the fast bulk load into
    /// an empty store equals the general path, and `store.bin` holds exactly
    /// the four index fields (the form derived before the statistics).
    #[test]
    fn statistics_bulk_load_and_bytes_stay_exact() {
        #[derive(Serialize)]
        struct Mirror<'a> {
            s2po: &'a AdjMap<EntityLiteralId>,
            o2ps: &'a AdjMap<EntityLiteralId>,
            p2so: &'a AdjMap<PredId>,
            triple_count: u64,
        }
        fn brute(s: &TripleStore, p: PredId) -> (usize, usize) {
            let pairs = s.so_by_p(p);
            let mut subs: Vec<u32> = pairs.iter().map(|&(a, _)| a).collect();
            let mut objs: Vec<u32> = pairs.iter().map(|&(_, b)| b).collect();
            subs.sort_unstable();
            subs.dedup();
            objs.sort_unstable();
            objs.dedup();
            (subs.len(), objs.len())
        }
        fn check(s: &TripleStore) {
            for p in 0..9u32 {
                assert_eq!((s.pred_distinct_subj(p), s.pred_distinct_obj(p)), brute(s, p));
            }
            let mut fresh = s.clone();
            fresh.recompute_pred_stats();
            assert_eq!(fresh.pred_stats, s.pred_stats);
            let bytes = bincode::serialize(s).unwrap();
            let mirror = Mirror {
                s2po: &s.s2po,
                o2ps: &s.o2ps,
                p2so: &s.p2so,
                triple_count: s.triple_count,
            };
            assert_eq!(bytes, bincode::serialize(&mirror).unwrap());
            let back: TripleStore = bincode::deserialize(&bytes).unwrap();
            assert_eq!(back.pred_stats, s.pred_stats);
            assert_eq!(bincode::serialize(&back).unwrap(), bytes);
        }
        let mut seed = 99u64;
        let mut next = move |n: u32| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 33) % u64::from(n)) as u32
        };
        let mut batch = || -> Vec<IdTriple> {
            (0..600)
                .map(|_| IdTriple::new(next(50), next(8), next(70)))
                .collect()
        };
        let first = batch();
        let mut fast = TripleStore::new();
        fast.bulk_load(first.clone());
        let mut general = TripleStore::new();
        general.insert(first[0]); // non-empty: the general path
        general.bulk_load(first.clone());
        assert_eq!(
            bincode::serialize(&fast).unwrap(),
            bincode::serialize(&general).unwrap()
        );
        assert_eq!(fast.pred_stats, general.pred_stats);
        check(&fast);
        let mut store = fast;
        for round in 0..5 {
            for t in batch() {
                if round % 2 == 0 {
                    store.insert(t);
                } else {
                    store.remove(t);
                }
            }
            check(&store);
            store.bulk_load(batch());
            check(&store);
        }
        for t in store.iter_all().collect::<Vec<_>>() {
            store.remove(t);
        }
        assert!(store.pred_stats.is_empty());
        check(&store);
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
