//! A runaway evaluation stops soon after its cancel flag is raised — by a
//! `query_timeout` deadline or `/cancel` — instead of finishing the step it
//! is in (review robust#5): the left-deep join loop and the extension of a
//! binding poll the flag once per batch, the joins of a conjunction between
//! each other, and property paths per frontier step.
//!
//! Deterministic work counters, no clocks: a store wrapper raises the flag
//! at its `TRIP`-th index lookup and counts the lookups made after it; the
//! evaluator's `bgp_bindings` counts the partial solutions it produced.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gstore::model::id::{EntityLiteralId, PredId};
use gstore::query::Evaluator;
use gstore::store::{TripleSource, TripleStore};
use gstore::{Database, GStoreError, IdTriple};

/// Raises `flag` at the `trip`-th lookup (`0`: never) and counts every
/// lookup.
struct Tripwire<'a> {
    inner: &'a TripleStore,
    flag: Arc<AtomicBool>,
    trip: u64,
    calls: Cell<u64>,
}

impl Tripwire<'_> {
    /// Count a lookup; raise the flag at the `trip`-th (`0`: never).
    fn tick(&self) {
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if n == self.trip {
            self.flag.store(true, Ordering::SeqCst);
        }
    }

    fn after_trip(&self) -> u64 {
        self.calls.get().saturating_sub(self.trip)
    }
}

impl TripleSource for Tripwire<'_> {
    fn exists(&self, s: EntityLiteralId, p: PredId, o: EntityLiteralId) -> bool {
        self.tick();
        TripleSource::exists(self.inner, s, p, o)
    }
    fn po_by_s(&self, s: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        self.tick();
        TripleSource::po_by_s(self.inner, s)
    }
    fn o_by_sp(&self, s: EntityLiteralId, p: PredId) -> Vec<EntityLiteralId> {
        self.tick();
        TripleSource::o_by_sp(self.inner, s, p)
    }
    fn p_by_so(&self, s: EntityLiteralId, o: EntityLiteralId) -> Vec<PredId> {
        self.tick();
        TripleSource::p_by_so(self.inner, s, o)
    }
    fn ps_by_o(&self, o: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        self.tick();
        TripleSource::ps_by_o(self.inner, o)
    }
    fn s_by_po(&self, p: PredId, o: EntityLiteralId) -> Vec<EntityLiteralId> {
        self.tick();
        TripleSource::s_by_po(self.inner, p, o)
    }
    fn so_by_p(&self, p: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        self.tick();
        TripleSource::so_by_p(self.inner, p)
    }
    fn subs_by_p(&self, p: PredId) -> Vec<EntityLiteralId> {
        TripleSource::subs_by_p(self.inner, p)
    }
    fn objs_by_p(&self, p: PredId) -> Vec<EntityLiteralId> {
        TripleSource::objs_by_p(self.inner, p)
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        TripleSource::subject_keys(self.inner)
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        TripleSource::object_keys(self.inner)
    }
    fn triple_count(&self) -> u64 {
        TripleSource::triple_count(self.inner)
    }
    fn distinct_subjects(&self) -> usize {
        TripleSource::distinct_subjects(self.inner)
    }
    fn distinct_objects(&self) -> usize {
        TripleSource::distinct_objects(self.inner)
    }
    fn num_predicates(&self) -> usize {
        TripleSource::num_predicates(self.inner)
    }
    fn pred_card(&self, p: PredId) -> usize {
        TripleSource::pred_card(self.inner, p)
    }
    fn pred_distinct_subj(&self, p: PredId) -> usize {
        TripleSource::pred_distinct_subj(self.inner, p)
    }
    fn pred_distinct_obj(&self, p: PredId) -> usize {
        TripleSource::pred_distinct_obj(self.inner, p)
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        self.tick();
        TripleSource::iter_all(self.inner)
    }
}

/// Evaluate `q` over `db` with a flag the store raises at lookup `trip`
/// (`0`: never): the outcome, the lookups after the trip (all of them for
/// `0`) and the partial solutions.
fn run(db: &Database, q: &str, trip: u64) -> (Result<(), GStoreError>, u64, u64) {
    let flag = Arc::new(AtomicBool::new(false));
    let store = Tripwire {
        inner: db.store(),
        flag: Arc::clone(&flag),
        trip,
        calls: Cell::new(0),
    };
    let ev = Evaluator::new(db.dict(), &store).with_cancel(flag);
    let parsed = gstore::parser::sparql::parse(q).unwrap();
    let out = ev.evaluate(&parsed).map(|_| ());
    (out, store.after_trip(), ev.stats().bgp_bindings)
}

fn star(n: usize) -> Database {
    let nt: String = (0..n)
        .map(|i| {
            format!(
                "<http://ex/s{i}> <http://ex/p{}> <http://ex/o{i}> .\n",
                i % 3
            )
        })
        .collect();
    Database::build_from_str("star", &nt).unwrap()
}

/// Without the flag, how many lookups and partial solutions `q` takes.
fn full(db: &Database, q: &str) -> (u64, u64) {
    let (out, after, bindings) = run(db, q, 0);
    out.unwrap();
    (after, bindings)
}

#[test]
fn a_cross_product_stops_within_one_batch_of_the_flag() {
    let db = star(60);
    let q = "SELECT ?a ?d ?g WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i }";
    let (_, all) = full(&db, q);
    assert!(all > 200_000, "the cross product is large: {all}");
    // Trip early inside the last (largest) step.
    let (out, after, bindings) = run(&db, q, 200);
    assert!(matches!(out, Err(GStoreError::Cancelled)), "{out:?}");
    assert!(
        bindings < all / 10,
        "stopped within a batch: {bindings} of {all} partial solutions"
    );
    assert!(after <= 2048, "{after} lookups after the flag");
}

#[test]
fn a_property_path_closure_stops_within_one_frontier_step() {
    // A long chain: `*` from every node visits O(n²) nodes.
    let n = 400;
    let nt: String = (0..n)
        .map(|i| {
            format!(
                "<http://ex/n{i}> <http://ex/next> <http://ex/n{}> .\n",
                i + 1
            )
        })
        .collect();
    let db = Database::build_from_str("chain", &nt).unwrap();
    let q = "SELECT ?s ?o WHERE { ?s <http://ex/next>* ?o }";
    let (lookups, _) = full(&db, q);
    assert!(lookups > 10_000, "the closure is large: {lookups}");
    let (out, after, _) = run(&db, q, 100);
    assert!(matches!(out, Err(GStoreError::Cancelled)), "{out:?}");
    assert!(after <= 16, "{after} lookups after the flag (of {lookups})");
}
