//! Property-path evaluation: a fixed solution order (review
//! gap-accel-soundness#1) and the same rows, in the same order, from both
//! executors. The SPARQL semantics of paths — negated property sets, the
//! multisets of `/` and `|`, zero-length paths over terms that are not in
//! the data — are W3C-style cases in `tests/w3c/property-path` and
//! `tests/w3c/negated-property-set`.

use gstore::parser::sparql::parse;
use gstore::query::{EvalStats, Evaluator, QueryResult, TopKOptions};
use gstore::Database;

const P: &str = "<http://ex/p>";
const Q: &str = "<http://ex/q>";

/// The review's data: forty `a_i p b_(i mod 7)` and `b_(i mod 7) q c_i`, so
/// each `a_i` reaches six `c`s through `p/q`.
fn fan_out() -> Database {
    let mut nt = String::new();
    for i in 0..40 {
        nt.push_str(&format!("<http://ex/a{i}> {P} <http://ex/b{}> .\n", i % 7));
        nt.push_str(&format!("<http://ex/b{}> {Q} <http://ex/c{i}> .\n", i % 7));
    }
    Database::build_from_str("paths", &nt).unwrap()
}

/// The rows of `q` as one fresh evaluator over `db` returns them, with the
/// work it did.
fn run(db: &Database, q: &str, options: TopKOptions) -> (Vec<Vec<Option<String>>>, EvalStats) {
    let query = parse(q).unwrap();
    let ev = Evaluator::new(db.dict(), db.store()).with_topk_options(options);
    let rows = match ev.evaluate(&query).unwrap() {
        QueryResult::Select(rs) => rs.rows,
        other => panic!("{q}: not a SELECT result: {other:?}"),
    };
    (rows, ev.stats())
}

fn rows(db: &Database, q: &str, options: TopKOptions) -> Vec<Vec<Option<String>>> {
    run(db, q, options).0
}

/// Every executor setting the engine has (the materializing evaluator and
/// the streaming one).
const EXECUTORS: [TopKOptions; 2] = [TopKOptions::NONE, TopKOptions::ALL];

#[test]
fn path_solutions_come_in_the_same_order_on_every_evaluation() {
    let db = fan_out();
    let queries = [
        format!("SELECT ?o WHERE {{ <http://ex/a0> {P}/{Q} ?o }}"),
        format!("SELECT ?s ?o WHERE {{ ?s {P}/{Q} ?o }} LIMIT 25"),
        format!("SELECT ?s WHERE {{ ?s {P}/{Q} <http://ex/c14> }}"),
        format!("SELECT ?o WHERE {{ <http://ex/a0> ({P}|^{P})+ ?o }}"),
        format!("SELECT ?o WHERE {{ <http://ex/b0> !{P} ?o }}"),
        format!("SELECT ?s ?o WHERE {{ ?s ({P}/{Q})? ?o }} LIMIT 60"),
    ];
    for q in &queries {
        for options in EXECUTORS {
            let first = rows(&db, q, options);
            assert!(!first.is_empty(), "{q}");
            // A fresh evaluator each time: the old hash sets drew new
            // random keys per evaluation, so the order changed run by run.
            for run in 1..8 {
                assert_eq!(rows(&db, q, options), first, "{q}: run {run} ({options:?})");
            }
        }
    }
}

#[test]
fn limit_offset_pages_of_a_path_query_tile_its_solutions() {
    let db = fan_out();
    let all = format!("SELECT ?o WHERE {{ <http://ex/a0> {P}/{Q} ?o }}");
    for options in EXECUTORS {
        let full = rows(&db, &all, options);
        assert_eq!(full.len(), 6, "{full:?}");
        let mut paged = Vec::new();
        for offset in [0, 2, 4] {
            paged.extend(rows(
                &db,
                &format!("{all} LIMIT 2 OFFSET {offset}"),
                options,
            ));
        }
        // The review saw six paged rows with only four distinct ones.
        assert_eq!(paged, full, "{options:?}");
    }
}

#[test]
fn order_by_ties_over_path_solutions_are_repeatable() {
    let db = fan_out();
    // Every ?s has several ?o: the rows tied on ?s keep the solution
    // order, which is now fixed.
    let q = format!("SELECT ?s ?o WHERE {{ ?s {P}/{Q} ?o }} ORDER BY ?s LIMIT 20");
    for options in EXECUTORS {
        let first = rows(&db, &q, options);
        assert_eq!(first.len(), 20);
        for run in 1..8 {
            assert_eq!(rows(&db, &q, options), first, "run {run} ({options:?})");
        }
    }
}

#[test]
fn both_executors_return_path_solutions_in_the_same_order() {
    let db = fan_out();
    let queries = [
        format!("SELECT ?s ?o WHERE {{ ?s {P}/{Q} ?o }} LIMIT 1000"),
        format!("SELECT ?s ?o WHERE {{ ?s ({P}|{Q})* ?o }} LIMIT 1000"),
        format!("SELECT ?s ?o WHERE {{ ?s !{Q} ?o }} LIMIT 1000"),
        format!("SELECT ?s ?o WHERE {{ ?s !(^{P}|{Q}) ?o }} LIMIT 1000"),
    ];
    for q in &queries {
        let full = rows(&db, q, TopKOptions::NONE);
        let (streamed, stats) = run(&db, q, TopKOptions::ALL);
        assert!(stats.streamed > 0, "{q}: {stats:?}");
        assert_eq!(streamed, full, "{q}");
    }
}
