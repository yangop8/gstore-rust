//! Property-path multiplicities are handed out lazily and bounded, and a
//! path under OPTIONAL is walked from the end point the left side binds
//! (round-3 review of query#9 / sparqlconf#4 and sparqlconf#9).
//!
//! A `/` or `|` path is a multiset (SPARQL 1.1 §18.4): a pair of nodes
//! reached by `k` routes is `k` solutions, and on one self-loop triple
//! `<a> (:p|:p)/…/(:p|:p) <a>` with `n` steps has `2ⁿ` routes. The walk
//! kept `k` as a count, but every pair was then pushed `k` times into a
//! vector before anything else ran, in both executors: `ASK`, `EXISTS` and
//! `LIMIT 1` could not stop early, 24 steps took 1.2 GB and 28 steps more
//! memory than the machine had. Now the copies are handed out one at a
//! time (the streaming executor stops after those it takes), only once per
//! pair where only existence or distinctness matters (`ASK`, `EXISTS`,
//! `SELECT DISTINCT`), and the copies past each pair's first are charged
//! to a per-query budget — a query that would hand out more fails with a
//! query error instead of exhausting the memory.
//!
//! `OPTIONAL { ?s :next* ?x }` with `?s` bound by the left side used to run
//! the path from every node of the graph (the bind join of sparqlconf#9
//! covered inner-join conjuncts only): 4.5 million path steps on a 3,000
//! node chain for 10 rows. The OPTIONAL's right side is now walked from
//! each left solution's value, in both executors.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use gstore::parser::sparql::parse;
use gstore::query::{EvalStats, Evaluator, QueryResult, TopKOptions};
use gstore::{Database, GStoreError};

/// Counts the bytes each thread holds, and its peak.
struct Counting;

thread_local! {
    static HELD: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn note(delta: isize) {
    let _ = HELD.try_with(|h| {
        let now = h.get() + delta;
        h.set(now);
        let _ = PEAK.try_with(|p| {
            if now > p.get() {
                p.set(now)
            }
        });
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            note(layout.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        note(-(layout.size() as isize));
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            note(layout.size() as isize);
        }
        p
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            note(new_size as isize - layout.size() as isize);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Run `f` and return its result with the most bytes this thread held at
/// once during it, beyond what it held before.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = HELD.with(Cell::get);
    PEAK.with(|p| p.set(base));
    let out = f();
    let peak = PEAK.with(Cell::get);
    (out, (peak - base).max(0) as usize)
}

/// Both executors: the materializing evaluator and the streaming one.
const EXECUTORS: [TopKOptions; 2] = [TopKOptions::NONE, TopKOptions::ALL];

/// The one self-loop triple.
fn self_loop() -> Database {
    Database::build_from_str("loop", "<http://ex/a> <http://ex/p> <http://ex/a> .\n").unwrap()
}

/// `(p|p)/(p|p)/…`, `steps` times: `2^steps` routes from `<a>` to `<a>`.
fn doubling(steps: usize) -> String {
    vec!["(<http://ex/p>|<http://ex/p>)"; steps].join("/")
}

fn evaluate(
    db: &Database,
    q: &str,
    options: TopKOptions,
    max: Option<u64>,
) -> (gstore::Result<QueryResult>, EvalStats) {
    let query = parse(q).unwrap();
    let mut ev = Evaluator::new(db.dict(), db.store()).with_topk_options(options);
    if let Some(max) = max {
        ev = ev.with_path_max_duplicates(max);
    }
    let r = ev.evaluate(&query);
    (r, ev.stats())
}

fn select_rows(r: gstore::Result<QueryResult>, q: &str) -> Vec<Vec<Option<String>>> {
    match r {
        Ok(QueryResult::Select(rs)) => rs.rows,
        other => panic!("{q}: {other:?}"),
    }
}

fn is_budget_error(r: &gstore::Result<QueryResult>) -> bool {
    matches!(r, Err(GStoreError::Query(m)) if m.contains("repeated solutions"))
}

/// Failing before: `ASK` and `EXISTS` over 22 doubling steps expanded all
/// 4,194,304 copies (hundreds of MB) before looking at the first; `LIMIT 1`
/// in the streaming executor did too. Now they take what they need.
#[test]
fn ask_exists_and_limit_take_only_the_copies_they_need() {
    let db = self_loop();
    let path = doubling(22);
    let ask = format!("ASK {{ <http://ex/a> {path} <http://ex/a> }}");
    let exists =
        format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o FILTER EXISTS {{ ?s {path} ?o }} }}");
    let limit = format!("SELECT ?x WHERE {{ <http://ex/a> {path} ?x }} LIMIT 1");
    for options in EXECUTORS {
        let ((r, _), peak) = peak_during(|| evaluate(&db, &ask, options, None));
        assert!(
            matches!(r, Ok(QueryResult::Ask(true))),
            "{options:?}: {r:?}"
        );
        assert!(peak < 4 << 20, "ASK ({options:?}): peak {peak} bytes");
        let ((r, _), peak) = peak_during(|| evaluate(&db, &exists, options, None));
        assert_eq!(select_rows(r, &exists).len(), 1, "{options:?}");
        assert!(peak < 4 << 20, "EXISTS ({options:?}): peak {peak} bytes");
    }
    let ((r, stats), peak) = peak_during(|| evaluate(&db, &limit, TopKOptions::ALL, None));
    assert_eq!(select_rows(r, &limit).len(), 1);
    assert!(stats.streamed > 0, "{stats:?}");
    assert!(peak < 4 << 20, "LIMIT 1 (streamed): peak {peak} bytes");
}

/// Failing before: a query that needs every copy (COUNT over 23 doubling
/// steps, 8,388,608 solutions) materialized them all; past the budget it
/// is now a query error, at once, in both executors. Within the budget the
/// multiset is kept exactly.
#[test]
fn a_query_needing_more_copies_than_the_budget_fails_at_once() {
    let db = self_loop();
    let count = format!(
        "SELECT (COUNT(*) AS ?c) WHERE {{ <http://ex/a> {} ?x }}",
        doubling(23)
    );
    for options in EXECUTORS {
        let ((r, _), peak) = peak_during(|| evaluate(&db, &count, options, None));
        assert!(is_budget_error(&r), "{options:?}: {r:?}");
        assert!(peak < 4 << 20, "COUNT ({options:?}): peak {peak} bytes");
    }
    // Within the budget: every route is a solution.
    let q = format!(
        "SELECT (COUNT(*) AS ?c) WHERE {{ <http://ex/a> {} ?x }}",
        doubling(10)
    );
    for options in EXECUTORS {
        let rows = select_rows(evaluate(&db, &q, options, None).0, &q);
        assert!(rows[0][0].as_deref().unwrap().contains("1024"), "{rows:?}");
    }
    // The budget is the evaluator's: 2^7 copies are 127 past the first.
    let q = format!("SELECT ?x WHERE {{ <http://ex/a> {} ?x }}", doubling(7));
    for options in EXECUTORS {
        assert!(is_budget_error(&evaluate(&db, &q, options, Some(100)).0));
        let rows = select_rows(evaluate(&db, &q, options, Some(127)).0, &q);
        assert_eq!(rows.len(), 128);
    }
}

/// The review's sizes: 40 doubling steps (2⁴⁰ routes) on the self-loop. A
/// count is never expanded beyond the budget, and existence or
/// distinctness takes one copy.
#[test]
fn forty_doubling_steps_are_answered_or_refused_without_expanding() {
    let db = self_loop();
    let path = doubling(40);
    for options in EXECUTORS {
        let q = format!("ASK {{ <http://ex/a> {path} <http://ex/a> }}");
        let ((r, stats), peak) = peak_during(|| evaluate(&db, &q, options, None));
        assert!(
            matches!(r, Ok(QueryResult::Ask(true))),
            "{options:?}: {r:?}"
        );
        assert!(stats.path_steps <= 200, "{stats:?}");
        assert!(peak < 4 << 20, "{options:?}: peak {peak} bytes");

        let q = format!("SELECT DISTINCT ?x WHERE {{ <http://ex/a> {path} ?x }}");
        let ((r, _), peak) = peak_during(|| evaluate(&db, &q, options, None));
        assert_eq!(select_rows(r, &q), vec![vec![Some("<http://ex/a>".into())]]);
        assert!(peak < 4 << 20, "{options:?}: peak {peak} bytes");

        for q in [
            format!("SELECT (COUNT(*) AS ?c) WHERE {{ <http://ex/a> {path} ?x }}"),
            format!("SELECT ?x WHERE {{ <http://ex/a> {path} ?x }}"),
            format!("SELECT ?x ?y WHERE {{ ?x {path} ?y }}"),
        ] {
            let ((r, _), peak) = peak_during(|| evaluate(&db, &q, options, None));
            assert!(is_budget_error(&r), "{q} ({options:?}): {r:?}");
            assert!(peak < 4 << 20, "{q} ({options:?}): peak {peak} bytes");
        }
    }
    // Through the database's own read path (streamed): LIMIT 1 answers.
    let mut db = db;
    let q = format!("SELECT ?x WHERE {{ <http://ex/a> {path} ?x }} LIMIT 1");
    match db.query(&q).unwrap() {
        QueryResult::Select(rs) => assert_eq!(rs.rows.len(), 1),
        other => panic!("{other:?}"),
    }
}

/// `SELECT DISTINCT` hands each pair out once only when that cannot change
/// its answer: a per-solution function (`BNODE()`) or an aggregate keeps
/// every copy.
#[test]
fn distinct_keeps_the_copies_a_per_solution_function_or_aggregate_sees() {
    let db = self_loop();
    let path = doubling(3);
    for options in EXECUTORS {
        let q = format!("SELECT DISTINCT ?x (BNODE() AS ?b) WHERE {{ <http://ex/a> {path} ?x }}");
        assert_eq!(
            select_rows(evaluate(&db, &q, options, None).0, &q).len(),
            8,
            "{options:?}"
        );
        let q = format!("SELECT DISTINCT (COUNT(*) AS ?c) WHERE {{ <http://ex/a> {path} ?x }}");
        let rows = select_rows(evaluate(&db, &q, options, None).0, &q);
        assert!(rows[0][0].as_deref().unwrap().contains('8'), "{rows:?}");
        // A sub-SELECT decides for itself.
        let q = format!(
            "SELECT DISTINCT ?c WHERE {{ {{ SELECT (COUNT(*) AS ?c) WHERE {{ <http://ex/a> {path} ?x }} }} }}"
        );
        let rows = select_rows(evaluate(&db, &q, options, None).0, &q);
        assert!(rows[0][0].as_deref().unwrap().contains('8'), "{rows:?}");
    }
}

/// A chain of `n` nodes linked by `:next`, each with an `:id`.
fn chain(n: usize) -> Database {
    let mut nt = String::new();
    for i in 0..n {
        nt.push_str(&format!("<http://ex/n{i}> <http://ex/id> \"{i}\" .\n"));
        if i + 1 < n {
            nt.push_str(&format!(
                "<http://ex/n{i}> <http://ex/next> <http://ex/n{}> .\n",
                i + 1
            ));
        }
    }
    Database::build_from_str("chain", &nt).unwrap()
}

/// Failing before: the OPTIONAL's path ran from every node (501,500 path
/// steps on 1,000 nodes, 2,003,000 on 2,000) for the same 10 rows; now it
/// is walked from the value the left side binds — the same few steps on
/// both sizes, in both executors, with the inner join's rows.
#[test]
fn an_optional_path_is_walked_from_the_end_point_the_left_side_binds() {
    let (small, large) = (chain(1000), chain(2000));
    let shapes = [
        // The review's shape, and a group inside the OPTIONAL.
        "SELECT ?s ?x WHERE { ?s <http://ex/id> \"{ID}\" OPTIONAL { ?s <http://ex/next>* ?x } }",
        "SELECT ?s ?x ?i WHERE { ?s <http://ex/id> \"{ID}\" \
         OPTIONAL { ?s <http://ex/next>+ ?x . ?x <http://ex/id> ?i } }",
        // A filter on the merged rows, and a left side that matches nothing
        // the path reaches from.
        "SELECT ?s ?x WHERE { ?s <http://ex/id> \"{ID}\" \
         OPTIONAL { ?s <http://ex/next>* ?x FILTER(?x != ?s) } }",
    ];
    for options in EXECUTORS {
        for shape in shapes {
            let mut steps = Vec::new();
            for (db, n) in [(&small, 1000), (&large, 2000)] {
                let mut q = shape.replace("{ID}", &(n - 10).to_string());
                if options == TopKOptions::ALL {
                    q.push_str(" LIMIT 100000");
                }
                let (r, stats) = evaluate(db, &q, options, None);
                let rows = select_rows(r, &q);
                // The same rows as the inner join, which the bind join of
                // sparqlconf#9 already evaluated from the bound side.
                let inner = q.replace("OPTIONAL {", "{");
                let expected = select_rows(evaluate(db, &inner, options, None).0, &inner);
                assert_eq!(rows, expected, "{q} ({options:?})");
                assert!(!rows.is_empty());
                steps.push(stats.path_steps);
            }
            assert!(
                steps[0] <= 64,
                "{shape} ({options:?}): {} path steps",
                steps[0]
            );
            assert_eq!(
                steps[0], steps[1],
                "{shape} ({options:?}): grows with the graph"
            );
        }
    }
    // A left solution the path cannot extend keeps its row, unextended.
    let q = "SELECT ?s ?x WHERE { ?s <http://ex/id> \"999\" OPTIONAL { ?s <http://ex/next>+ ?x } }";
    for options in EXECUTORS {
        let rows = select_rows(evaluate(&small, q, options, None).0, q);
        assert_eq!(
            rows,
            vec![vec![Some("<http://ex/n999>".into()), None]],
            "{options:?}"
        );
    }
}
