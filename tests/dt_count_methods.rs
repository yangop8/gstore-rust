//! Contract C1: the provided count methods of `TripleSource`.
//!
//! * Every `count_*` equals the `len` of the list it counts, `distinct_p_of_*`
//!   the number of distinct predicates in it, and `has_*` its non-emptiness —
//!   for the in-memory `TripleStore` (which overrides them with slice
//!   arithmetic) and for `Backend` (which dispatches them).
//! * Plans are identical through a wrapper that keeps the default methods:
//!   the same ordered answers and the same number of partial solutions
//!   (`EvalStats::bgp_bindings`, which depends on the join order) on the
//!   LUBM workload and on hand-written shapes.

use std::path::PathBuf;

use gstore::model::id::{EntityLiteralId, PredId, LITERAL_FIRST_ID};
use gstore::query::{Evaluator, QueryResult};
use gstore::store::{Backend, TripleSource, TripleStore};
use gstore::{Database, IdTriple, MutableStore};

/// A small deterministic generator (SplitMix64).
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

fn random_store(seed: u64, n: usize) -> TripleStore {
    let mut rng = Rng(seed);
    let mut triples = Vec::with_capacity(n);
    for _ in 0..n {
        let s = rng.below(60);
        let p = rng.below(8);
        let o = if rng.below(4) == 0 {
            LITERAL_FIRST_ID + rng.below(30)
        } else {
            rng.below(80)
        };
        triples.push(IdTriple::new(s, p, o));
    }
    let mut store = TripleStore::new();
    MutableStore::bulk_load(&mut store, triples);
    // Some deletes, so pruned keys are covered too.
    let mut rng = Rng(seed ^ 0xABCD);
    for _ in 0..n / 10 {
        let t = IdTriple::new(rng.below(60), rng.below(8), rng.below(80));
        MutableStore::remove(&mut store, t);
    }
    store
}

fn distinct_firsts(pairs: &[(u32, u32)]) -> u64 {
    let mut firsts: Vec<u32> = pairs.iter().map(|&(a, _)| a).collect();
    firsts.sort_unstable();
    firsts.dedup();
    firsts.len() as u64
}

fn check_counts<S: TripleSource>(store: &S, label: &str) {
    let objects: Vec<EntityLiteralId> = (0..90)
        .chain((0..35).map(|i| LITERAL_FIRST_ID + i))
        .collect();
    for s in 0..70u32 {
        let po = store.po_by_s(s);
        assert_eq!(
            store.count_po_by_s(s),
            po.len() as u64,
            "{label} count_po_by_s({s})"
        );
        assert_eq!(
            store.distinct_p_of_s(s),
            distinct_firsts(&po),
            "{label} distinct_p_of_s({s})"
        );
        assert_eq!(
            store.has_subject(s),
            !po.is_empty(),
            "{label} has_subject({s})"
        );
        for p in 0..10u32 {
            assert_eq!(
                store.count_o_by_sp(s, p),
                store.o_by_sp(s, p).len() as u64,
                "{label} count_o_by_sp({s},{p})"
            );
        }
        for &o in &objects {
            assert_eq!(
                store.count_p_by_so(s, o),
                store.p_by_so(s, o).len() as u64,
                "{label} count_p_by_so({s},{o})"
            );
        }
    }
    for &o in &objects {
        let ps = store.ps_by_o(o);
        assert_eq!(
            store.count_ps_by_o(o),
            ps.len() as u64,
            "{label} count_ps_by_o({o})"
        );
        assert_eq!(
            store.distinct_p_of_o(o),
            distinct_firsts(&ps),
            "{label} distinct_p_of_o({o})"
        );
        assert_eq!(
            store.has_object(o),
            !ps.is_empty(),
            "{label} has_object({o})"
        );
        for p in 0..10u32 {
            assert_eq!(
                store.count_s_by_po(p, o),
                store.s_by_po(p, o).len() as u64,
                "{label} count_s_by_po({p},{o})"
            );
        }
    }
    for p in 0..10u32 {
        let so = store.so_by_p(p);
        assert_eq!(
            store.count_so_by_p(p),
            so.len() as u64,
            "{label} count_so_by_p({p})"
        );
        assert_eq!(
            store.has_predicate(p),
            !so.is_empty(),
            "{label} has_predicate({p})"
        );
    }
}

#[test]
fn counts_equal_list_lengths_for_triple_store_and_backend() {
    for seed in 0..6u64 {
        let store = random_store(seed, 900);
        check_counts(&store, &format!("TripleStore seed {seed}"));
        let backend = Backend::Memory(store);
        check_counts(&backend, &format!("Backend seed {seed}"));
    }
    // The empty store.
    check_counts(&TripleStore::new(), "empty TripleStore");
    check_counts(&Backend::default(), "empty Backend");
}

#[test]
fn lists_follow_the_ordering_contract() {
    let store = random_store(42, 2000);
    let ascending_unique = |v: &[u32]| v.windows(2).all(|w| w[0] < w[1]);
    let ascending_pairs = |v: &[(u32, u32)]| v.windows(2).all(|w| w[0] < w[1]);
    for s in 0..60u32 {
        assert!(ascending_pairs(&TripleSource::po_by_s(&store, s)));
        for p in 0..8u32 {
            assert!(ascending_unique(&TripleSource::o_by_sp(&store, s, p)));
        }
        for o in 0..80u32 {
            assert!(ascending_unique(&TripleSource::p_by_so(&store, s, o)));
        }
    }
    for o in 0..80u32 {
        assert!(ascending_pairs(&TripleSource::ps_by_o(&store, o)));
        for p in 0..8u32 {
            assert!(ascending_unique(&TripleSource::s_by_po(&store, p, o)));
        }
    }
    for p in 0..8u32 {
        assert!(ascending_pairs(&TripleSource::so_by_p(&store, p)));
        assert!(ascending_unique(&TripleSource::subs_by_p(&store, p)));
        assert!(ascending_unique(&TripleSource::objs_by_p(&store, p)));
    }
    assert!(ascending_unique(&TripleSource::subject_keys(&store)));
    assert!(ascending_unique(&TripleSource::object_keys(&store)));
    let all = TripleSource::iter_all(&store);
    assert!(all
        .windows(2)
        .all(|w| (w[0].pred, w[0].sub, w[0].obj) < (w[1].pred, w[1].sub, w[1].obj)));
}

/// Delegates only the required methods: every C1 method runs its default.
struct Defaults<'a>(&'a TripleStore);

impl TripleSource for Defaults<'_> {
    fn exists(&self, s: EntityLiteralId, p: PredId, o: EntityLiteralId) -> bool {
        TripleSource::exists(self.0, s, p, o)
    }
    fn po_by_s(&self, s: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        TripleSource::po_by_s(self.0, s)
    }
    fn o_by_sp(&self, s: EntityLiteralId, p: PredId) -> Vec<EntityLiteralId> {
        TripleSource::o_by_sp(self.0, s, p)
    }
    fn p_by_so(&self, s: EntityLiteralId, o: EntityLiteralId) -> Vec<PredId> {
        TripleSource::p_by_so(self.0, s, o)
    }
    fn ps_by_o(&self, o: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        TripleSource::ps_by_o(self.0, o)
    }
    fn s_by_po(&self, p: PredId, o: EntityLiteralId) -> Vec<EntityLiteralId> {
        TripleSource::s_by_po(self.0, p, o)
    }
    fn so_by_p(&self, p: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        TripleSource::so_by_p(self.0, p)
    }
    fn subs_by_p(&self, p: PredId) -> Vec<EntityLiteralId> {
        TripleSource::subs_by_p(self.0, p)
    }
    fn objs_by_p(&self, p: PredId) -> Vec<EntityLiteralId> {
        TripleSource::objs_by_p(self.0, p)
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        TripleSource::subject_keys(self.0)
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        TripleSource::object_keys(self.0)
    }
    fn triple_count(&self) -> u64 {
        TripleSource::triple_count(self.0)
    }
    fn distinct_subjects(&self) -> usize {
        TripleSource::distinct_subjects(self.0)
    }
    fn distinct_objects(&self) -> usize {
        TripleSource::distinct_objects(self.0)
    }
    fn num_predicates(&self) -> usize {
        TripleSource::num_predicates(self.0)
    }
    fn pred_card(&self, p: PredId) -> usize {
        TripleSource::pred_card(self.0, p)
    }
    fn pred_distinct_subj(&self, p: PredId) -> usize {
        TripleSource::pred_distinct_subj(self.0, p)
    }
    fn pred_distinct_obj(&self, p: PredId) -> usize {
        TripleSource::pred_distinct_obj(self.0, p)
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        TripleSource::iter_all(self.0)
    }
}

fn lubm_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm")
}

/// Ordered rows plus the partial-solution count of one evaluation.
fn run<S: TripleSource>(eval: Evaluator<'_, S>, q: &str) -> (Vec<Vec<Option<String>>>, u64) {
    let parsed = gstore::parser::sparql::parse(q).unwrap();
    let rows = match eval.evaluate(&parsed).unwrap() {
        QueryResult::Select(rs) => rs.rows,
        QueryResult::Ask(b) => vec![vec![Some(b.to_string())]],
        other => panic!("unexpected result {other:?}"),
    };
    (rows, eval.stats().bgp_bindings)
}

#[test]
fn plans_are_identical_through_the_default_methods() {
    let db = Database::build_from_files("lubm", &[lubm_dir().join("lubm.nt")]).unwrap();
    let mut queries: Vec<String> = (1..=14)
        .map(|i| std::fs::read_to_string(lubm_dir().join(format!("lubm_q{i}.rq"))).unwrap())
        .collect();
    // Shapes whose cost model uses the distinct-predicate and per-key counts.
    let ub = "http://www.lehigh.edu/~zhp2/2004/0401/univ-bench.owl#";
    let d0 = "http://www.Department0.University0.edu/";
    let dept = "http://www.Department0.University0.edu";
    queries.extend([
        format!("SELECT ?s ?p WHERE {{ ?s ?p <{d0}GraduateCourse0> . ?s <{ub}advisor> ?a }}"),
        format!("SELECT ?p ?o WHERE {{ <{d0}GraduateStudent44> ?p ?o . ?o ?q ?r }} LIMIT 50"),
        format!(
            "SELECT ?x ?p WHERE {{ ?x <{ub}memberOf> <{dept}> . <{d0}GraduateStudent44> ?p ?x }}"
        ),
        format!(
            "SELECT ?x ?y ?z WHERE {{ ?x <{ub}advisor> ?y . ?y <{ub}teacherOf> ?z . \
             ?x <{ub}takesCourse> ?z . ?x ?p <{dept}> }}"
        ),
        format!("SELECT ?p WHERE {{ <{d0}GraduateStudent44> ?p <{dept}> }}"),
        format!("ASK {{ ?s ?p ?o . ?o ?q <{dept}> }}"),
    ]);
    let store = db.store();
    for q in &queries {
        let overridden = run(Evaluator::new(db.dict(), store), q);
        let defaults = run(Evaluator::new(db.dict(), &Defaults(store)), q);
        assert_eq!(overridden.0, defaults.0, "ordered answers differ for {q}");
        assert_eq!(
            overridden.1, defaults.1,
            "partial solutions (plan) differ for {q}"
        );
        // And through the Backend dispatch.
        let backend = Backend::Memory(store.clone());
        let dispatched = run(Evaluator::new(db.dict(), &backend), q);
        assert_eq!(overridden, dispatched, "Backend dispatch differs for {q}");
    }
}
