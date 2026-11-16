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
        // Paths joined from an end point another pattern binds (review
        // sparqlconf#9): the subject side, the object side, both, a path
        // linking two otherwise unconnected patterns, two paths in a row,
        // and an end point OPTIONAL leaves unbound in some solutions.
        format!("SELECT ?s ?o WHERE {{ ?s {P} <http://ex/b3> . ?s {P}/{Q} ?o }} LIMIT 1000"),
        format!("SELECT ?s ?o WHERE {{ ?o ^{Q} <http://ex/b2> . ?s {P}/{Q} ?o }} LIMIT 1000"),
        format!("SELECT ?s ?o WHERE {{ ?s {P} ?b . ?b {Q} ?o . ?s ({P}/{Q})? ?o }} LIMIT 1000"),
        format!(
            "SELECT ?s ?o WHERE {{ ?s {P} <http://ex/b1> . ?o ^{Q} <http://ex/b1> . ?s {P}/{Q} ?o }} LIMIT 1000"
        ),
        format!("SELECT ?s ?m ?o WHERE {{ ?s {P} <http://ex/b4> . ?s {P} ?m . ?m {Q}* ?o }} LIMIT 1000"),
        format!("SELECT ?s ?b ?o WHERE {{ ?s {P} <http://ex/b5> . ?s ({P}|{Q})+ ?b . ?b {Q}+ ?o }} LIMIT 1000"),
        format!(
            "SELECT ?s ?t ?o WHERE {{ ?s {P} ?b OPTIONAL {{ ?s <http://ex/none> ?t }} ?t {Q}? ?o }} LIMIT 1000"
        ),
    ];
    for q in &queries {
        let full = rows(&db, q, TopKOptions::NONE);
        let (streamed, stats) = run(&db, q, TopKOptions::ALL);
        assert!(stats.streamed > 0, "{q}: {stats:?}");
        assert_eq!(streamed, full, "{q}");
    }
}

// ---- review sparqlconf#9: a path with a bound end point ---------------------

/// A chain of `n` nodes, `n_i :next n_(i+1)`, each with `n_i :id i`.
fn chain(n: usize) -> Database {
    let mut nt = String::new();
    for i in 0..n {
        nt.push_str(&format!("<http://ex/n{i}> <http://ex/id> \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n"));
        if i + 1 < n {
            nt.push_str(&format!(
                "<http://ex/n{i}> <http://ex/next> <http://ex/n{}> .\n",
                i + 1
            ));
        }
    }
    Database::build_from_str("chain", &nt).unwrap()
}

/// The queries of the scaling test over a chain of `n` nodes, with the rows
/// each must return: the end point bound by another pattern of the group,
/// subject side and object side, and both.
fn bound_end_point_queries(n: usize) -> Vec<(String, usize)> {
    let near_end = n - 10;
    vec![
        // The review's repro: `?s :id 2990 . ?s :next* ?x` on 3,000 nodes.
        (
            format!("SELECT ?x WHERE {{ ?s <http://ex/id> {near_end} . ?s <http://ex/next>* ?x }}"),
            10,
        ),
        (
            format!("SELECT ?x WHERE {{ ?s <http://ex/id> {near_end} . ?s <http://ex/next>+ ?x }}"),
            9,
        ),
        // The object end bound: walked backwards from it.
        (
            "SELECT ?s WHERE { ?x <http://ex/id> 10 . ?s <http://ex/next>* ?x }".to_string(),
            11,
        ),
        // Both ends bound, by two separate patterns.
        (
            format!(
                "SELECT ?s ?x WHERE {{ ?s <http://ex/id> {near_end} . ?x <http://ex/id> {} . \
                 ?s <http://ex/next>* ?x }}",
                n - 1
            ),
            1,
        ),
        // A pattern on the far end, connected to the rest only by the path.
        (
            format!(
                "SELECT ?x ?k WHERE {{ ?s <http://ex/id> {near_end} . ?s <http://ex/next>* ?x . \
                 ?x <http://ex/id> ?k }}"
            ),
            10,
        ),
    ]
}

#[test]
fn a_path_with_a_bound_end_point_costs_what_the_bound_side_reaches() {
    // Before review sparqlconf#9 the path ran from every node of the graph
    // (both ends free) and was joined afterwards: about n²/2 steps on a
    // chain, 4.5 million for the review's 3,000 nodes. Evaluated from the
    // bound end point it takes the same few steps whatever the chain's
    // length, in both executors.
    let small = chain(3000);
    let large = chain(6000);
    for (i, (q, expected_rows)) in bound_end_point_queries(3000).iter().enumerate() {
        let (q_large, _) = &bound_end_point_queries(6000)[i];
        for options in EXECUTORS {
            // The streaming executor runs for a LIMIT.
            let (q, q_large) = if options == TopKOptions::ALL {
                (
                    format!("{q} LIMIT 100000"),
                    format!("{q_large} LIMIT 100000"),
                )
            } else {
                (q.clone(), q_large.clone())
            };
            let (rows_small, stats_small) = run(&small, &q, options);
            let (rows_large, stats_large) = run(&large, &q_large, options);
            assert_eq!(rows_small.len(), *expected_rows, "{q} ({options:?})");
            assert_eq!(rows_large.len(), *expected_rows, "{q_large} ({options:?})");
            if options == TopKOptions::ALL {
                assert!(stats_small.streamed > 0, "{q}: {stats_small:?}");
            }
            assert!(
                stats_small.path_steps <= 64,
                "{q} ({options:?}): {} path steps on 3,000 nodes",
                stats_small.path_steps
            );
            assert_eq!(
                stats_small.path_steps, stats_large.path_steps,
                "{q} ({options:?}): the path's work grows with the graph"
            );
            eprintln!(
                "{q} ({options:?}): {} path steps on 3,000 and 6,000 nodes",
                stats_small.path_steps
            );
        }
    }
}

#[test]
fn counting_over_a_bound_path_on_the_reviews_chain() {
    let db = chain(3000);
    let q = "SELECT (COUNT(*) AS ?c) WHERE { ?s <http://ex/id> 2990 . ?s <http://ex/next>* ?x }";
    let (rows, stats) = run(&db, q, TopKOptions::NONE);
    assert_eq!(
        rows,
        vec![vec![Some(
            "\"10\"^^<http://www.w3.org/2001/XMLSchema#integer>".to_string()
        )]]
    );
    assert!(stats.path_steps <= 64, "{stats:?}");
}
