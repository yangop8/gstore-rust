//! Data test: the cost-based optimizer's binary-join (bushy) execution path,
//! and the bounds of the large-BGP plan search.
//!
//! A "bow-tie" query — two stars (`?x` and `?y`), each pinned by two constant
//! edges, joined by a single bridge edge `?x → ?y` — is exactly the shape where
//! the DP optimizer prefers a *binary join* of the two independently-built
//! sub-results over any left-deep pipeline. These tests drive that query end to
//! end through the public `Database` API and check the result is correct (the
//! bushy executor must agree with the left-deep one).
//!
//! review tests-b#14: the tests used to compare only with hand-written
//! answers, so a cost-model change that stopped choosing the bushy plan (or
//! a plan never reaching the optimizer: the no-bridge data had no `:bridge`
//! triple at all, so the BGP was empty before planning) went unnoticed.
//! Each now checks through the optimizer's thread counters that a bushy plan
//! was chosen, and runs the query again with bushy plans forbidden on the
//! thread to compare with the left-deep pipeline.
//!
//! review query#13: a BGP of more than 14 patterns is planned by a greedy
//! order refined with 2-opt local search, which used to be cubic in the
//! pattern count and deaf to a cancel. Its work units
//! ([`optimizer::work_on_this_thread`]) stay within
//! [`optimizer::OPT_WORK_BUDGET`] however large the star, and a cancel
//! raised while it runs stops it at its next poll.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use gstore::query::optimizer;
use gstore::{Database, GStoreError, QueryResult};

/// 40 `?x` subjects (each `ta A`, `tb B`), 40 `?y` subjects (each `tc C`,
/// `td D`), and one bridge `x5 -> y7`. Only `x5`/`y7` survive the full join.
fn bowtie_data() -> String {
    let mut s = String::from("@prefix : <http://ex/> .\n");
    for x in 0..40 {
        s += &format!(":x{x} :ta :A ; :tb :B .\n");
    }
    for y in 0..40 {
        s += &format!(":y{y} :tc :C ; :td :D .\n");
    }
    s += ":x5 :bridge :y7 .\n";
    s
}

const BOWTIE_QUERY: &str = "\
SELECT ?x ?y WHERE {
  ?x <http://ex/ta> <http://ex/A> .
  ?x <http://ex/tb> <http://ex/B> .
  ?y <http://ex/tc> <http://ex/C> .
  ?y <http://ex/td> <http://ex/D> .
  ?x <http://ex/bridge> ?y .
}";

/// The sorted `(x, y)` pairs `BOWTIE_QUERY` returns on `db`.
fn pairs(db: &mut Database) -> Vec<(String, String)> {
    let rs = db.select(BOWTIE_QUERY).unwrap();
    let mut pairs: Vec<(String, String)> = rs
        .rows
        .iter()
        .map(|r| {
            (
                r[0].clone().unwrap_or_default(),
                r[1].clone().unwrap_or_default(),
            )
        })
        .collect();
    pairs.sort();
    pairs
}

/// Puts bushy plans back on this thread — also when the test fails.
struct AllowBushy;

impl Drop for AllowBushy {
    fn drop(&mut self) {
        optimizer::set_bushy_plans_on_this_thread(true);
    }
}

/// The bow-tie's answer on `data` — checked to come from a bushy plan — and
/// the same query's answer with bushy plans forbidden (the left-deep
/// pipeline); the two must agree (review tests-b#14).
fn bushy_and_left_deep(name: &str, data: &str) -> Vec<(String, String)> {
    let mut db = Database::build_from_str(name, data).unwrap();
    // The second run must evaluate again, not answer from the cache.
    db.set_query_cache_size(0);
    let before = optimizer::bushy_plans_on_this_thread();
    let bushy = pairs(&mut db);
    assert_eq!(
        optimizer::bushy_plans_on_this_thread(),
        before + 1,
        "the optimizer chose a bushy (binary-join) plan for the bow-tie"
    );
    let _restore = AllowBushy;
    assert!(optimizer::set_bushy_plans_on_this_thread(false));
    let left_deep = pairs(&mut db);
    assert_eq!(
        optimizer::bushy_plans_on_this_thread(),
        before + 1,
        "no bushy plan while they are forbidden"
    );
    assert_eq!(bushy, left_deep, "bushy and left-deep plans agree");
    bushy
}

#[test]
fn bushy_join_returns_exact_solution() {
    assert_eq!(
        bushy_and_left_deep("opt_bowtie", &bowtie_data()),
        vec![("<http://ex/x5>".to_string(), "<http://ex/y7>".to_string())],
        "exactly one bridged (x, y) pair"
    );
}

#[test]
fn bushy_join_with_no_bridge_is_empty() {
    // Same two stars, and no bridge between them ⇒ the binary join yields
    // nothing. review tests-b#14: the bridge predicate exists (between two
    // nodes outside both stars), so the BGP reaches the optimizer and the
    // binary-join executor instead of being empty before planning.
    let mut data = String::from("@prefix : <http://ex/> .\n");
    for x in 0..40 {
        data += &format!(":x{x} :ta :A ; :tb :B .\n");
    }
    for y in 0..40 {
        data += &format!(":y{y} :tc :C ; :td :D .\n");
    }
    data += ":z1 :bridge :z2 .\n";
    assert_eq!(bushy_and_left_deep("opt_nobridge", &data), Vec::new());
}

#[test]
fn bushy_join_multiple_bridges() {
    // Two bridges ⇒ two surviving pairs; the bushy executor must return both.
    let mut data = bowtie_data();
    data += ":x9 :bridge :y3 .\n";
    assert_eq!(
        bushy_and_left_deep("opt_twobridge", &data),
        vec![
            ("<http://ex/x5>".to_string(), "<http://ex/y7>".to_string()),
            ("<http://ex/x9>".to_string(), "<http://ex/y3>".to_string()),
        ]
    );
}

// ---- the large-BGP plan search (review query#13) ------------------------

/// 20 subjects, each with an object for every one of the predicates
/// `p0 … p{n-1}`, and the star `SELECT ?s { ?s :p_i ?o_i }` over them.
fn star(n: usize) -> (Database, String) {
    let mut data = String::from("@prefix : <http://ex/> .\n");
    for s in 0..20 {
        for p in 0..n {
            data += &format!(":s{s} :p{p} :o{} .\n", p % (s + 1));
        }
    }
    let mut q = String::from("PREFIX : <http://ex/>\nSELECT ?s WHERE {\n");
    for p in 0..n {
        q += &format!("  ?s :p{p} ?o{p} .\n");
    }
    q += "}";
    let mut db = Database::build_from_str("opt_star", &data).unwrap();
    db.set_query_cache_size(0);
    (db, q)
}

#[test]
fn large_star_plan_search_stays_within_its_work_budget() {
    // Unbounded, the 2-opt refinement scored up to 16 passes of n²/2 swaps
    // at n placements each: about 0.5M units at 40 patterns, 64M at 200.
    for n in [40, 120, 200] {
        let (mut db, q) = star(n);
        let before = optimizer::work_on_this_thread();
        let rs = db.select(&q).unwrap();
        let work = optimizer::work_on_this_thread() - before;
        assert_eq!(rs.row_count(), 20, "{n} patterns: every subject matches");
        assert!(work > 0, "{n} patterns: the search ran on this thread");
        assert!(
            work <= optimizer::OPT_WORK_BUDGET + n as u64,
            "{n} patterns: {work} work units, over the budget {}",
            optimizer::OPT_WORK_BUDGET
        );
    }
}

/// Removes the poll hook again — also when the test fails.
struct HookGuard;

impl Drop for HookGuard {
    fn drop(&mut self) {
        optimizer::set_poll_hook_on_this_thread(None);
    }
}

#[test]
fn a_cancel_raised_during_the_plan_search_stops_it() {
    let n = 200;
    let (mut db, q) = star(n);
    let flag = Arc::new(AtomicBool::new(false));
    // The search's first poll raises the query's cancel flag (as `/cancel`
    // from another thread would); the search must see it at that poll.
    let polls = Arc::new(AtomicU64::new(0));
    let _guard = HookGuard;
    {
        let (flag, polls) = (Arc::clone(&flag), Arc::clone(&polls));
        optimizer::set_poll_hook_on_this_thread(Some(Box::new(move || {
            polls.fetch_add(1, Ordering::SeqCst);
            flag.store(true, Ordering::SeqCst);
        })));
    }
    let before = optimizer::work_on_this_thread();
    let result = db.query_cancellable(&q, Arc::clone(&flag));
    let work = optimizer::work_on_this_thread() - before;
    assert_eq!(polls.load(Ordering::SeqCst), 1, "stopped at its first poll");
    assert!(
        work <= optimizer::POLL_EVERY + n as u64,
        "{work} work units: the search went on after the cancel"
    );
    match result {
        Err(GStoreError::Cancelled) => {}
        other => panic!("expected Cancelled, got {other:?}"),
    }
    // The cut-short plan was not cached: the same query runs in full now.
    optimizer::set_poll_hook_on_this_thread(None);
    let rs = match db.query(&q).unwrap() {
        QueryResult::Select(rs) => rs,
        other => panic!("not a SELECT result: {other:?}"),
    };
    assert_eq!(rs.row_count(), 20);
}
