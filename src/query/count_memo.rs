//! A per-BGP memo of the store's exact counts (C1).
//!
//! Compiling one BGP asks the store for the same counts several times:
//! candidate generation orders a variable's constant edges by their list
//! lengths, and the optimizer's cardinality and distinct-value estimates
//! (`pattern_card`, `build_ndv`) ask for them again. An in-memory store
//! answers each in O(log n); an on-disk one counts a B+ tree key range, one
//! leaf per few hundred entries — at LUBM univ 38 `?x rdf:type
//! ub:GraduateStudent` is a 390-leaf range, asked three times per query.
//! [`CountMemo`] wraps the store for the duration of one `prepare_bgp` and
//! answers a repeated count from its memo. Counts are exact and the store
//! does not change while a statement compiles a BGP, so every answer equals
//! the store's — the plans are unchanged.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;
use crate::store::TripleSource;

use super::candidates::ConstEdge;
use super::engine::FxBuildHasher;

/// Which count a memo entry holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Count {
    PoByS,
    OBySp,
    PBySo,
    PsByO,
    SByPo,
    SoByP,
    DistinctPOfS,
    DistinctPOfO,
}

/// A [`TripleSource`] that forwards every call to `inner` and memoizes the
/// count methods (see the [module docs](self)).
pub(crate) struct CountMemo<'s, S: TripleSource> {
    inner: &'s S,
    counts: RefCell<HashMap<(Count, u32, u32), u64, FxBuildHasher>>,
}

impl<'s, S: TripleSource> CountMemo<'s, S> {
    pub(crate) fn new(inner: &'s S) -> CountMemo<'s, S> {
        CountMemo {
            inner,
            counts: RefCell::new(HashMap::default()),
        }
    }

    /// Record the exact length of a list read whole (its C1 count).
    pub(crate) fn note(&self, edge: ConstEdge, len: u64) {
        let key = match edge {
            ConstEdge::SubjOf(p, o) => (Count::SByPo, p, o),
            ConstEdge::ObjOf(s, p) => (Count::OBySp, s, p),
            // Distinct values, not the counted pairs.
            ConstEdge::SubjOfAny(_) | ConstEdge::ObjOfAny(_) => return,
        };
        self.counts.borrow_mut().insert(key, len);
    }

    fn memo(&self, key: (Count, u32, u32), count: impl FnOnce() -> u64) -> u64 {
        if let Some(&n) = self.counts.borrow().get(&key) {
            return n;
        }
        let n = count();
        self.counts.borrow_mut().insert(key, n);
        n
    }
}

impl<S: TripleSource> TripleSource for CountMemo<'_, S> {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        self.inner.exists(sub, pred, obj)
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        self.inner.po_by_s(sub)
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        self.inner.o_by_sp(sub, pred)
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        self.inner.p_by_so(sub, obj)
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        self.inner.ps_by_o(obj)
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        self.inner.s_by_po(pred, obj)
    }
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        self.inner.so_by_p(pred)
    }
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        self.inner.subs_by_p(pred)
    }
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        self.inner.objs_by_p(pred)
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        self.inner.subject_keys()
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        self.inner.object_keys()
    }
    fn triple_count(&self) -> u64 {
        self.inner.triple_count()
    }
    fn distinct_subjects(&self) -> usize {
        self.inner.distinct_subjects()
    }
    fn distinct_objects(&self) -> usize {
        self.inner.distinct_objects()
    }
    fn num_predicates(&self) -> usize {
        self.inner.num_predicates()
    }
    fn pred_card(&self, pred: PredId) -> usize {
        self.inner.pred_card(pred)
    }
    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        self.inner.pred_distinct_subj(pred)
    }
    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        self.inner.pred_distinct_obj(pred)
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        self.inner.iter_all()
    }
    fn count_po_by_s(&self, sub: EntityLiteralId) -> u64 {
        self.memo((Count::PoByS, sub, 0), || self.inner.count_po_by_s(sub))
    }
    fn count_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> u64 {
        self.memo((Count::OBySp, sub, pred), || {
            self.inner.count_o_by_sp(sub, pred)
        })
    }
    fn count_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> u64 {
        self.memo((Count::PBySo, sub, obj), || {
            self.inner.count_p_by_so(sub, obj)
        })
    }
    fn count_ps_by_o(&self, obj: EntityLiteralId) -> u64 {
        self.memo((Count::PsByO, obj, 0), || self.inner.count_ps_by_o(obj))
    }
    fn count_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> u64 {
        self.memo((Count::SByPo, pred, obj), || {
            self.inner.count_s_by_po(pred, obj)
        })
    }
    fn count_so_by_p(&self, pred: PredId) -> u64 {
        self.memo((Count::SoByP, pred, 0), || self.inner.count_so_by_p(pred))
    }
    fn distinct_p_of_s(&self, sub: EntityLiteralId) -> u64 {
        self.memo((Count::DistinctPOfS, sub, 0), || {
            self.inner.distinct_p_of_s(sub)
        })
    }
    fn distinct_p_of_o(&self, obj: EntityLiteralId) -> u64 {
        self.memo((Count::DistinctPOfO, obj, 0), || {
            self.inner.distinct_p_of_o(obj)
        })
    }
    fn has_subject(&self, sub: EntityLiteralId) -> bool {
        self.inner.has_subject(sub)
    }
    fn has_object(&self, obj: EntityLiteralId) -> bool {
        self.inner.has_object(obj)
    }
    fn has_predicate(&self, pred: PredId) -> bool {
        self.inner.has_predicate(pred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TripleStore;
    use std::cell::Cell;

    /// Counts how often the wrapped store is asked for `count_s_by_po`.
    struct Counting {
        store: TripleStore,
        asked: Cell<u32>,
    }

    impl TripleSource for Counting {
        fn exists(&self, s: u32, p: u32, o: u32) -> bool {
            self.store.exists(s, p, o)
        }
        fn po_by_s(&self, s: u32) -> Vec<(u32, u32)> {
            TripleSource::po_by_s(&self.store, s)
        }
        fn o_by_sp(&self, s: u32, p: u32) -> Vec<u32> {
            TripleSource::o_by_sp(&self.store, s, p)
        }
        fn p_by_so(&self, s: u32, o: u32) -> Vec<u32> {
            TripleSource::p_by_so(&self.store, s, o)
        }
        fn ps_by_o(&self, o: u32) -> Vec<(u32, u32)> {
            TripleSource::ps_by_o(&self.store, o)
        }
        fn s_by_po(&self, p: u32, o: u32) -> Vec<u32> {
            TripleSource::s_by_po(&self.store, p, o)
        }
        fn so_by_p(&self, p: u32) -> Vec<(u32, u32)> {
            TripleSource::so_by_p(&self.store, p)
        }
        fn subs_by_p(&self, p: u32) -> Vec<u32> {
            TripleSource::subs_by_p(&self.store, p)
        }
        fn objs_by_p(&self, p: u32) -> Vec<u32> {
            TripleSource::objs_by_p(&self.store, p)
        }
        fn subject_keys(&self) -> Vec<u32> {
            TripleSource::subject_keys(&self.store)
        }
        fn object_keys(&self) -> Vec<u32> {
            TripleSource::object_keys(&self.store)
        }
        fn triple_count(&self) -> u64 {
            TripleSource::triple_count(&self.store)
        }
        fn distinct_subjects(&self) -> usize {
            TripleSource::distinct_subjects(&self.store)
        }
        fn distinct_objects(&self) -> usize {
            TripleSource::distinct_objects(&self.store)
        }
        fn num_predicates(&self) -> usize {
            TripleSource::num_predicates(&self.store)
        }
        fn pred_card(&self, p: u32) -> usize {
            TripleSource::pred_card(&self.store, p)
        }
        fn pred_distinct_subj(&self, p: u32) -> usize {
            TripleSource::pred_distinct_subj(&self.store, p)
        }
        fn pred_distinct_obj(&self, p: u32) -> usize {
            TripleSource::pred_distinct_obj(&self.store, p)
        }
        fn iter_all(&self) -> Vec<IdTriple> {
            TripleSource::iter_all(&self.store)
        }
        fn count_s_by_po(&self, p: u32, o: u32) -> u64 {
            self.asked.set(self.asked.get() + 1);
            TripleSource::count_s_by_po(&self.store, p, o)
        }
    }

    #[test]
    fn counts_equal_the_store_and_are_asked_once() {
        let mut store = TripleStore::new();
        store.bulk_load(vec![
            IdTriple::new(1, 0, 9),
            IdTriple::new(2, 0, 9),
            IdTriple::new(2, 1, 3),
            IdTriple::new(3, 1, 9),
        ]);
        let src = Counting {
            store,
            asked: Cell::new(0),
        };
        let memo = CountMemo::new(&src);
        for _ in 0..3 {
            assert_eq!(memo.count_s_by_po(0, 9), 2);
            assert_eq!(memo.count_s_by_po(1, 9), 1);
            assert_eq!(memo.count_s_by_po(1, 4), 0);
        }
        assert_eq!(src.asked.get(), 3, "each count asked once");
        // A noted list length answers without asking.
        memo.note(ConstEdge::SubjOf(0, 7), 0);
        assert_eq!(memo.count_s_by_po(0, 7), 0);
        assert_eq!(src.asked.get(), 3);
        // The other counts and every list equal the store's.
        let st = &src.store;
        for x in 0..10 {
            assert_eq!(memo.count_po_by_s(x), TripleSource::count_po_by_s(st, x));
            assert_eq!(memo.count_ps_by_o(x), TripleSource::count_ps_by_o(st, x));
            assert_eq!(
                memo.distinct_p_of_s(x),
                TripleSource::distinct_p_of_s(st, x)
            );
            assert_eq!(
                memo.distinct_p_of_o(x),
                TripleSource::distinct_p_of_o(st, x)
            );
            assert_eq!(memo.po_by_s(x), TripleSource::po_by_s(st, x));
            for y in 0..10 {
                assert_eq!(
                    memo.count_o_by_sp(x, y),
                    TripleSource::count_o_by_sp(st, x, y)
                );
                assert_eq!(
                    memo.count_p_by_so(x, y),
                    TripleSource::count_p_by_so(st, x, y)
                );
            }
        }
        assert_eq!(memo.count_so_by_p(1), 2);
    }
}
