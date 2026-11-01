//! An exact transaction overlay over a [`TripleSource`] (Phase 6, R2-DISKLIB).
//!
//! An [`Overlay`] records the triples a transaction added to and removed from
//! a base store without touching the base; an [`OverlayView`] reads the base
//! *through* the overlay (read-your-writes), and [`Overlay::drain`] hands the
//! net changes to whoever applies them at commit.
//!
//! ## Invariants
//!
//! With `B` the base the overlay was built against:
//!
//! * `added ∩ B = ∅` — an added triple is new;
//! * `removed ⊆ B` — a removed triple existed;
//! * `added ∩ removed = ∅`.
//!
//! [`insert`](Overlay::insert) and [`remove`](Overlay::remove) keep them (they
//! probe the base for the triple when the overlay itself does not decide), so
//! their `bool` results are the **exact** "changed" flags a materialized store
//! would return, and every count of the view is plain arithmetic:
//! `|view range| = |base range| − |removed range| + |added range|`. The base
//! must not change while the overlay holds changes (a transaction's base is
//! frozen until it commits).
//!
//! ## The view
//!
//! [`OverlayView`] implements **all** of [`TripleSource`], including the C1
//! count methods, and answers exactly what a [`TripleStore`] holding
//! `(B − removed) ∪ added` answers, **in the same order** (the ordering
//! contract of the trait):
//!
//! * lists are sorted two-way merges of the base list (minus the removed
//!   entries) and the added list;
//! * counts are exact arithmetic over the three stores;
//! * the distinct counts (`distinct_subjects`, `pred_distinct_subj`,
//!   `num_predicates`, `distinct_p_of_*`, …) probe only the keys the overlay
//!   touches; the global and per-predicate ones are memoized until the
//!   overlay next changes;
//! * `iter_all` merges in `(p, s, o)` order;
//! * a key the overlay does not touch (no added or removed triple under it)
//!   is answered by the base directly — the untouched-key fast path — so an
//!   overlay costs one or two in-memory map probes per read.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;

use super::{read_scope, TripleSource, TripleStore};

/// The net changes of an [`Overlay`] (see [`Overlay::drain`]): each list in
/// `(s, p, o)` order, without duplicates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayChanges {
    /// Triples to delete from the base (each one is in the base).
    pub removed: Vec<IdTriple>,
    /// Triples to insert into the base (none is in the base).
    pub added: Vec<IdTriple>,
}

impl OverlayChanges {
    /// Whether there is nothing to apply.
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }

    /// Number of triple changes.
    pub fn len(&self) -> usize {
        self.removed.len() + self.added.len()
    }
}

/// A memoized difference between a view statistic and the base's, valid until
/// the overlay next changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DeltaKey {
    DistinctSubjects,
    DistinctObjects,
    NumPredicates,
    PredDistinctSubj(PredId),
    PredDistinctObj(PredId),
}

/// The changes of one transaction, kept apart from its base (see the module
/// docs for the invariants).
#[derive(Debug, Default)]
pub struct Overlay {
    added: TripleStore,
    removed: TripleStore,
    /// Memoized statistic deltas (cleared by every change).
    deltas: Mutex<HashMap<DeltaKey, i64>>,
}

impl Clone for Overlay {
    fn clone(&self) -> Overlay {
        Overlay {
            added: self.added.clone(),
            removed: self.removed.clone(),
            deltas: Mutex::new(HashMap::new()),
        }
    }
}

impl Overlay {
    /// An empty overlay.
    pub fn new() -> Overlay {
        Overlay::default()
    }

    /// Whether the overlay holds no change.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    /// Number of triple changes held.
    pub fn len(&self) -> usize {
        (self.added.triple_count() + self.removed.triple_count()) as usize
    }

    /// The added triples (none of them is in the base).
    pub fn added(&self) -> &TripleStore {
        &self.added
    }

    /// The removed triples (each of them is in the base).
    pub fn removed(&self) -> &TripleStore {
        &self.removed
    }

    fn changed(&mut self) {
        self.deltas
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Insert `t` as seen through the overlay over `base`. Returns `true`
    /// exactly when `t` was not in `(base − removed) ∪ added` before.
    pub fn insert<B: TripleSource + ?Sized>(&mut self, base: &B, t: IdTriple) -> bool {
        match self.try_insert(t, |t| {
            Ok::<_, std::convert::Infallible>(base.exists(t.sub, t.pred, t.obj))
        }) {
            Ok(changed) => changed,
            Err(never) => match never {},
        }
    }

    /// Remove `t` as seen through the overlay over `base`. Returns `true`
    /// exactly when `t` was in `(base − removed) ∪ added` before.
    pub fn remove<B: TripleSource + ?Sized>(&mut self, base: &B, t: IdTriple) -> bool {
        match self.try_remove(t, |t| {
            Ok::<_, std::convert::Infallible>(base.exists(t.sub, t.pred, t.obj))
        }) {
            Ok(changed) => changed,
            Err(never) => match never {},
        }
    }

    /// [`insert`](Self::insert) with a fallible base probe: `base_exists(t)`
    /// is called only when the overlay itself cannot decide, and its error is
    /// returned unchanged (the overlay is then unchanged).
    pub fn try_insert<E>(
        &mut self,
        t: IdTriple,
        base_exists: impl FnOnce(IdTriple) -> Result<bool, E>,
    ) -> Result<bool, E> {
        if self.removed.exists(t.sub, t.pred, t.obj) {
            // A base triple removed earlier in the transaction comes back.
            self.removed.remove(t);
            self.changed();
            return Ok(true);
        }
        if self.added.exists(t.sub, t.pred, t.obj) || base_exists(t)? {
            return Ok(false);
        }
        self.added.insert(t);
        self.changed();
        Ok(true)
    }

    /// [`remove`](Self::remove) with a fallible base probe (see
    /// [`try_insert`](Self::try_insert)).
    pub fn try_remove<E>(
        &mut self,
        t: IdTriple,
        base_exists: impl FnOnce(IdTriple) -> Result<bool, E>,
    ) -> Result<bool, E> {
        if self.added.exists(t.sub, t.pred, t.obj) {
            self.added.remove(t);
            self.changed();
            return Ok(true);
        }
        if self.removed.exists(t.sub, t.pred, t.obj) || !base_exists(t)? {
            return Ok(false);
        }
        self.removed.insert(t);
        self.changed();
        Ok(true)
    }

    /// Take the net changes, each list in `(s, p, o)` order, leaving the
    /// overlay empty. Applying `removed` then `added` to the base yields
    /// exactly the view's contents.
    pub fn drain(&mut self) -> OverlayChanges {
        let changes = OverlayChanges {
            removed: spo_order(&self.removed),
            added: spo_order(&self.added),
        };
        self.clear();
        changes
    }

    /// Drop every change.
    pub fn clear(&mut self) {
        self.added = TripleStore::new();
        self.removed = TripleStore::new();
        self.changed();
    }

    /// Read `base` through this overlay.
    pub fn view<'a, B: TripleSource + ?Sized>(&'a self, base: &'a B) -> OverlayView<'a, B> {
        OverlayView::new(base, self)
    }

    /// The memoized delta under `key`, computing it with `f` once.
    fn delta(&self, key: DeltaKey, f: impl FnOnce() -> i64) -> i64 {
        if let Some(&d) = self
            .deltas
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return d;
        }
        // Computed outside the lock: `f` reads the base, which may take its
        // own locks. A racing reader computes the same value.
        let d = f();
        self.deltas
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, d);
        d
    }
}

/// Every triple of `store` in `(s, p, o)` order.
fn spo_order(store: &TripleStore) -> Vec<IdTriple> {
    let mut out = Vec::with_capacity(store.triple_count() as usize);
    for s in store.subject_keys() {
        out.extend(
            store
                .po_by_s(s)
                .iter()
                .map(|&(p, o)| IdTriple::new(s, p, o)),
        );
    }
    out
}

/// `(base − removed) ∪ added` over ascending, duplicate-free lists, compared
/// by `key`; the result is ascending and duplicate-free. Entries of `removed`
/// that are not in `base` are ignored, and an `added` entry equal to a kept
/// base entry appears once.
fn merge_by<T: Copy, K: Ord>(
    base: Vec<T>,
    removed: &[T],
    added: &[T],
    key: impl Fn(&T) -> K,
) -> Vec<T> {
    if removed.is_empty() && added.is_empty() {
        return base;
    }
    let mut out = Vec::with_capacity((base.len() + added.len()).saturating_sub(removed.len()));
    let (mut r, mut a) = (0usize, 0usize);
    for x in base {
        let kx = key(&x);
        while r < removed.len() && key(&removed[r]) < kx {
            r += 1;
        }
        if r < removed.len() && key(&removed[r]) == kx {
            r += 1;
            continue;
        }
        while a < added.len() {
            match key(&added[a]).cmp(&kx) {
                std::cmp::Ordering::Less => {
                    out.push(added[a]);
                    a += 1;
                }
                std::cmp::Ordering::Equal => a += 1,
                std::cmp::Ordering::Greater => break,
            }
        }
        out.push(x);
    }
    out.extend_from_slice(&added[a..]);
    out
}

/// [`merge_by`] on the values themselves.
fn merge<T: Copy + Ord>(base: Vec<T>, removed: &[T], added: &[T]) -> Vec<T> {
    merge_by(base, removed, added, |x| *x)
}

/// The ascending, duplicate-free union of two ascending, duplicate-free lists.
fn union<T: Copy + Ord>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// The distinct first components of a list sorted by them.
fn firsts(pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut out: Vec<u32> = pairs.iter().map(|&(a, _)| a).collect();
    out.dedup();
    out
}

/// `base − removed + added` for counts (saturating: a base that failed to
/// read reports 0 and its error through the read scope).
#[inline]
fn adjust(base: u64, removed: u64, added: u64) -> u64 {
    base.saturating_sub(removed) + added
}

/// How a key's presence changes: `+1` if it appears, `-1` if it disappears.
#[inline]
fn presence_delta(base: u64, removed: u64, added: u64) -> i64 {
    i64::from(adjust(base, removed, added) > 0) - i64::from(base > 0)
}

/// Apply a signed delta to a base statistic.
#[inline]
fn apply_delta(base: usize, delta: i64) -> usize {
    (base as i64 + delta).max(0) as usize
}

/// A base read through an [`Overlay`]: the triples of
/// `(base − removed) ∪ added`, in the trait's contract order (see the module
/// docs). Cheap to build (two references).
pub struct OverlayView<'a, B: TripleSource + ?Sized> {
    base: &'a B,
    overlay: &'a Overlay,
}

impl<B: TripleSource + ?Sized> Clone for OverlayView<'_, B> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<B: TripleSource + ?Sized> Copy for OverlayView<'_, B> {}

impl<'a, B: TripleSource + ?Sized> OverlayView<'a, B> {
    /// Read `base` through `overlay`.
    pub fn new(base: &'a B, overlay: &'a Overlay) -> OverlayView<'a, B> {
        OverlayView { base, overlay }
    }

    /// The base store.
    pub fn base(&self) -> &'a B {
        self.base
    }

    /// The overlay.
    pub fn overlay(&self) -> &'a Overlay {
        self.overlay
    }

    #[inline]
    fn added(&self) -> &'a TripleStore {
        &self.overlay.added
    }

    #[inline]
    fn removed(&self) -> &'a TripleStore {
        &self.overlay.removed
    }

    /// Whether the overlay holds nothing (every read goes to the base).
    #[inline]
    fn plain(&self) -> bool {
        self.overlay.is_empty()
    }

    /// `+1`/`-1`/`0` for subject `s` gaining / losing / keeping its presence.
    fn subject_delta(&self, s: EntityLiteralId) -> i64 {
        presence_delta(
            self.base.count_po_by_s(s),
            self.removed().count_po_by_s(s),
            self.added().count_po_by_s(s),
        )
    }

    fn object_delta(&self, o: EntityLiteralId) -> i64 {
        presence_delta(
            self.base.count_ps_by_o(o),
            self.removed().count_ps_by_o(o),
            self.added().count_ps_by_o(o),
        )
    }

    fn predicate_delta(&self, p: PredId) -> i64 {
        presence_delta(
            self.base.count_so_by_p(p),
            self.removed().count_so_by_p(p),
            self.added().count_so_by_p(p),
        )
    }

    fn sp_delta(&self, s: EntityLiteralId, p: PredId) -> i64 {
        presence_delta(
            self.base.count_o_by_sp(s, p),
            self.removed().count_o_by_sp(s, p),
            self.added().count_o_by_sp(s, p),
        )
    }

    fn po_delta(&self, p: PredId, o: EntityLiteralId) -> i64 {
        presence_delta(
            self.base.count_s_by_po(p, o),
            self.removed().count_s_by_po(p, o),
            self.added().count_s_by_po(p, o),
        )
    }

    /// Whether the overlay holds a triple with predicate `p`.
    #[inline]
    fn touches_predicate(&self, p: PredId) -> bool {
        !self.added().so_by_p(p).is_empty() || !self.removed().so_by_p(p).is_empty()
    }
}

impl<B: TripleSource + ?Sized> TripleSource for OverlayView<'_, B> {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        if self.plain() {
            return self.base.exists(sub, pred, obj);
        }
        if self.added().exists(sub, pred, obj) {
            return true;
        }
        if self.removed().exists(sub, pred, obj) {
            return false;
        }
        self.base.exists(sub, pred, obj)
    }

    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let (a, r) = (self.added().po_by_s(sub), self.removed().po_by_s(sub));
        let base = self.base.po_by_s(sub);
        merge(base, r, a)
    }

    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        let base = self.base.o_by_sp(sub, pred);
        if self.plain() {
            return base;
        }
        let (a, r) = (
            self.added().o_by_sp(sub, pred),
            self.removed().o_by_sp(sub, pred),
        );
        merge(base, &r, &a)
    }

    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        let base = self.base.p_by_so(sub, obj);
        if self.plain() {
            return base;
        }
        let (a, r) = (
            self.added().p_by_so(sub, obj),
            self.removed().p_by_so(sub, obj),
        );
        merge(base, &r, &a)
    }

    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let (a, r) = (self.added().ps_by_o(obj), self.removed().ps_by_o(obj));
        let base = self.base.ps_by_o(obj);
        merge(base, r, a)
    }

    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        let base = self.base.s_by_po(pred, obj);
        if self.plain() {
            return base;
        }
        let (a, r) = (
            self.added().s_by_po(pred, obj),
            self.removed().s_by_po(pred, obj),
        );
        merge(base, &r, &a)
    }

    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        let (a, r) = (self.added().so_by_p(pred), self.removed().so_by_p(pred));
        let base = self.base.so_by_p(pred);
        merge(base, r, a)
    }

    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let base = self.base.subs_by_p(pred);
        if self.plain() || !self.touches_predicate(pred) {
            return base;
        }
        // A removed `(s, pred, _)` drops `s` only if no `(s, pred, _)` is left.
        let gone: Vec<u32> = firsts(self.removed().so_by_p(pred))
            .into_iter()
            .filter(|&s| self.count_o_by_sp(s, pred) == 0)
            .collect();
        merge(base, &gone, &self.added().subs_by_p(pred))
    }

    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let base = self.base.objs_by_p(pred);
        if self.plain() || !self.touches_predicate(pred) {
            return base;
        }
        let gone: Vec<u32> = self
            .removed()
            .objs_by_p(pred)
            .into_iter()
            .filter(|&o| self.count_s_by_po(pred, o) == 0)
            .collect();
        merge(base, &gone, &self.added().objs_by_p(pred))
    }

    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        let base = self.base.subject_keys();
        if self.plain() {
            return base;
        }
        let gone: Vec<u32> = self
            .removed()
            .subject_keys()
            .filter(|&s| self.count_po_by_s(s) == 0)
            .collect();
        let added: Vec<u32> = self.added().subject_keys().collect();
        merge(base, &gone, &added)
    }

    fn object_keys(&self) -> Vec<EntityLiteralId> {
        let base = self.base.object_keys();
        if self.plain() {
            return base;
        }
        let gone: Vec<u32> = self
            .removed()
            .object_keys()
            .filter(|&o| self.count_ps_by_o(o) == 0)
            .collect();
        let added: Vec<u32> = self.added().object_keys().collect();
        merge(base, &gone, &added)
    }

    fn triple_count(&self) -> u64 {
        adjust(
            self.base.triple_count(),
            self.removed().triple_count(),
            self.added().triple_count(),
        )
    }

    fn distinct_subjects(&self) -> usize {
        let base = self.base.distinct_subjects();
        if self.plain() {
            return base;
        }
        let delta = self.overlay.delta(DeltaKey::DistinctSubjects, || {
            let added: Vec<u32> = self.added().subject_keys().collect();
            let removed: Vec<u32> = self.removed().subject_keys().collect();
            union(&added, &removed)
                .into_iter()
                .map(|s| self.subject_delta(s))
                .sum()
        });
        apply_delta(base, delta)
    }

    fn distinct_objects(&self) -> usize {
        let base = self.base.distinct_objects();
        if self.plain() {
            return base;
        }
        let delta = self.overlay.delta(DeltaKey::DistinctObjects, || {
            let added: Vec<u32> = self.added().object_keys().collect();
            let removed: Vec<u32> = self.removed().object_keys().collect();
            union(&added, &removed)
                .into_iter()
                .map(|o| self.object_delta(o))
                .sum()
        });
        apply_delta(base, delta)
    }

    fn num_predicates(&self) -> usize {
        let base = self.base.num_predicates();
        if self.plain() {
            return base;
        }
        let delta = self.overlay.delta(DeltaKey::NumPredicates, || {
            let added: Vec<u32> = self.added().predicates().collect();
            let removed: Vec<u32> = self.removed().predicates().collect();
            union(&added, &removed)
                .into_iter()
                .map(|p| self.predicate_delta(p))
                .sum()
        });
        apply_delta(base, delta)
    }

    fn pred_card(&self, pred: PredId) -> usize {
        let base = self.base.pred_card(pred);
        if self.plain() {
            return base;
        }
        adjust(
            base as u64,
            self.removed().pred_card(pred) as u64,
            self.added().pred_card(pred) as u64,
        ) as usize
    }

    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        let base = self.base.pred_distinct_subj(pred);
        if self.plain() || !self.touches_predicate(pred) {
            return base;
        }
        let delta = self.overlay.delta(DeltaKey::PredDistinctSubj(pred), || {
            union(
                &firsts(self.added().so_by_p(pred)),
                &firsts(self.removed().so_by_p(pred)),
            )
            .into_iter()
            .map(|s| self.sp_delta(s, pred))
            .sum()
        });
        apply_delta(base, delta)
    }

    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        let base = self.base.pred_distinct_obj(pred);
        if self.plain() || !self.touches_predicate(pred) {
            return base;
        }
        let delta = self.overlay.delta(DeltaKey::PredDistinctObj(pred), || {
            union(
                &self.added().objs_by_p(pred),
                &self.removed().objs_by_p(pred),
            )
            .into_iter()
            .map(|o| self.po_delta(pred, o))
            .sum()
        });
        apply_delta(base, delta)
    }

    fn iter_all(&self) -> Vec<IdTriple> {
        let base = self.base.iter_all();
        if self.plain() {
            return base;
        }
        let removed: Vec<IdTriple> = self.removed().iter_all().collect();
        let added: Vec<IdTriple> = self.added().iter_all().collect();
        merge_by(base, &removed, &added, |t| (t.pred, t.sub, t.obj))
    }

    // ---- C1: exact arithmetic, probes over the touched keys only ---------

    fn count_po_by_s(&self, sub: EntityLiteralId) -> u64 {
        let base = self.base.count_po_by_s(sub);
        if self.plain() {
            return base;
        }
        adjust(
            base,
            self.removed().count_po_by_s(sub),
            self.added().count_po_by_s(sub),
        )
    }

    fn count_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> u64 {
        let base = self.base.count_o_by_sp(sub, pred);
        if self.plain() {
            return base;
        }
        adjust(
            base,
            self.removed().count_o_by_sp(sub, pred),
            self.added().count_o_by_sp(sub, pred),
        )
    }

    fn count_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> u64 {
        let base = self.base.count_p_by_so(sub, obj);
        if self.plain() {
            return base;
        }
        adjust(
            base,
            self.removed().count_p_by_so(sub, obj),
            self.added().count_p_by_so(sub, obj),
        )
    }

    fn count_ps_by_o(&self, obj: EntityLiteralId) -> u64 {
        let base = self.base.count_ps_by_o(obj);
        if self.plain() {
            return base;
        }
        adjust(
            base,
            self.removed().count_ps_by_o(obj),
            self.added().count_ps_by_o(obj),
        )
    }

    fn count_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> u64 {
        let base = self.base.count_s_by_po(pred, obj);
        if self.plain() {
            return base;
        }
        adjust(
            base,
            self.removed().count_s_by_po(pred, obj),
            self.added().count_s_by_po(pred, obj),
        )
    }

    fn count_so_by_p(&self, pred: PredId) -> u64 {
        let base = self.base.count_so_by_p(pred);
        if self.plain() {
            return base;
        }
        adjust(
            base,
            self.removed().count_so_by_p(pred),
            self.added().count_so_by_p(pred),
        )
    }

    fn distinct_p_of_s(&self, sub: EntityLiteralId) -> u64 {
        let base = self.base.distinct_p_of_s(sub);
        let (a, r) = (self.added().po_by_s(sub), self.removed().po_by_s(sub));
        if a.is_empty() && r.is_empty() {
            return base;
        }
        let delta: i64 = union(&firsts(a), &firsts(r))
            .into_iter()
            .map(|p| self.sp_delta(sub, p))
            .sum();
        (base as i64 + delta).max(0) as u64
    }

    fn distinct_p_of_o(&self, obj: EntityLiteralId) -> u64 {
        let base = self.base.distinct_p_of_o(obj);
        let (a, r) = (self.added().ps_by_o(obj), self.removed().ps_by_o(obj));
        if a.is_empty() && r.is_empty() {
            return base;
        }
        let delta: i64 = union(&firsts(a), &firsts(r))
            .into_iter()
            .map(|p| self.po_delta(p, obj))
            .sum();
        (base as i64 + delta).max(0) as u64
    }

    fn has_subject(&self, sub: EntityLiteralId) -> bool {
        if self.added().has_subject(sub) {
            return true;
        }
        let removed = self.removed().count_po_by_s(sub);
        if removed == 0 {
            return self.base.has_subject(sub);
        }
        // Never "absent" on a read error (C1).
        read_scope::true_on_read_error(|| self.base.count_po_by_s(sub) > removed)
    }

    fn has_object(&self, obj: EntityLiteralId) -> bool {
        if self.added().has_object(obj) {
            return true;
        }
        let removed = self.removed().count_ps_by_o(obj);
        if removed == 0 {
            return self.base.has_object(obj);
        }
        read_scope::true_on_read_error(|| self.base.count_ps_by_o(obj) > removed)
    }

    fn has_predicate(&self, pred: PredId) -> bool {
        if self.added().has_predicate(pred) {
            return true;
        }
        let removed = self.removed().count_so_by_p(pred);
        if removed == 0 {
            return self.base.has_predicate(pred);
        }
        read_scope::true_on_read_error(|| self.base.count_so_by_p(pred) > removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MutableStore;

    fn store(ts: &[(u32, u32, u32)]) -> TripleStore {
        let mut s = TripleStore::new();
        MutableStore::bulk_load(
            &mut s,
            ts.iter().map(|&(a, b, c)| IdTriple::new(a, b, c)).collect(),
        );
        s
    }

    #[test]
    fn merge_keeps_order_and_set_semantics() {
        assert_eq!(merge(vec![1, 3, 5, 7], &[3, 7], &[2, 8]), vec![1, 2, 5, 8]);
        // An added entry equal to a kept base entry appears once; a removed
        // entry absent from the base is ignored.
        assert_eq!(merge(vec![1, 3], &[2], &[3, 4]), vec![1, 3, 4]);
        // An added entry equal to a removed base entry is kept.
        assert_eq!(merge(vec![1, 3, 5], &[3], &[3]), vec![1, 3, 5]);
        assert_eq!(merge(Vec::<u32>::new(), &[], &[1, 2]), vec![1, 2]);
        assert_eq!(union(&[1, 3, 5], &[2, 3, 6]), vec![1, 2, 3, 5, 6]);
    }

    #[test]
    fn flags_are_exact_and_drain_applies() {
        let base = store(&[(1, 0, 2), (1, 1, 3), (2, 0, 2)]);
        let mut ov = Overlay::new();
        assert!(!ov.insert(&base, IdTriple::new(1, 0, 2))); // in base
        assert!(ov.insert(&base, IdTriple::new(3, 0, 2)));
        assert!(!ov.insert(&base, IdTriple::new(3, 0, 2))); // already added
        assert!(ov.remove(&base, IdTriple::new(1, 1, 3)));
        assert!(!ov.remove(&base, IdTriple::new(1, 1, 3))); // already removed
        assert!(!ov.remove(&base, IdTriple::new(9, 9, 9))); // never existed
        assert!(ov.insert(&base, IdTriple::new(1, 1, 3))); // re-added
        assert!(ov.remove(&base, IdTriple::new(3, 0, 2))); // added, then removed
        assert!(ov.remove(&base, IdTriple::new(2, 0, 2)));
        assert_eq!(ov.len(), 1);
        let view = ov.view(&base);
        assert_eq!(view.triple_count(), 2);
        assert_eq!(view.subject_keys(), vec![1]);
        assert_eq!(view.distinct_subjects(), 1);
        assert_eq!(view.object_keys(), vec![2, 3]);
        let changes = ov.drain();
        assert!(ov.is_empty());
        assert_eq!(changes.removed, vec![IdTriple::new(2, 0, 2)]);
        assert!(changes.added.is_empty());
    }

    #[test]
    fn deltas_are_recomputed_after_a_change() {
        let base = store(&[(1, 0, 2)]);
        let mut ov = Overlay::new();
        ov.insert(&base, IdTriple::new(5, 0, 2));
        assert_eq!(ov.view(&base).distinct_subjects(), 2);
        assert_eq!(ov.view(&base).pred_distinct_subj(0), 2);
        ov.remove(&base, IdTriple::new(1, 0, 2));
        assert_eq!(ov.view(&base).distinct_subjects(), 1);
        assert_eq!(ov.view(&base).pred_distinct_subj(0), 1);
        assert_eq!(ov.view(&base).num_predicates(), 1);
        ov.remove(&base, IdTriple::new(5, 0, 2));
        assert_eq!(ov.view(&base).num_predicates(), 0);
        assert!(!ov.view(&base).has_predicate(0));
    }
}
