//! `BNODE(str)` gives each solution of a multiset its own node, also when
//! another solution is equal to it (round-3 review of query#5).
//!
//! SPARQL 1.1 §17.4.2.9: `BNODE(str)` returns "the same blank node for calls
//! with the same simple literal within expressions for one solution
//! mapping", and nodes "distinct from all blank nodes created by calls to
//! this constructor for other query solutions". A solution sequence is a
//! multiset (§18.1.7), so a `VALUES` block with a repeated row, or a path
//! that reaches the same pair of nodes by two routes, has two solutions
//! with equal bindings. `BNODE(str)` told solutions apart by their bindings
//! only (leaving out the nodes `BNODE` itself minted), so such solutions
//! shared one node — and so did `SELECT (BNODE() AS ?a) (BNODE("x") AS ?c)`,
//! whose solutions differ only in a node `BNODE` minted. Every solution now
//! has an identity of its own while its expressions run, in both executors;
//! a later `BIND` or SELECT expression of the same solution keeps it.

use std::collections::HashSet;

use gstore::parser::sparql::parse;
use gstore::query::{Evaluator, QueryResult, TopKOptions};
use gstore::Database;

const PATHS: &str = "<http://ex/a> <http://ex/p> <http://ex/m1> .\n\
                     <http://ex/m1> <http://ex/q> <http://ex/z> .\n\
                     <http://ex/a> <http://ex/p> <http://ex/m2> .\n\
                     <http://ex/m2> <http://ex/q> <http://ex/z> .\n";

/// The rows of `q`, as `(variable, cell)` maps, through `options`.
fn rows(db: &Database, q: &str, options: TopKOptions) -> Vec<Vec<(String, Option<String>)>> {
    let query = parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    match Evaluator::new(db.dict(), db.store())
        .with_topk_options(options)
        .evaluate(&query)
    {
        Ok(QueryResult::Select(rs)) => rs
            .rows
            .iter()
            .map(|r| rs.vars.iter().cloned().zip(r.iter().cloned()).collect())
            .collect(),
        other => panic!("{q}: {other:?}"),
    }
}

fn cell<'r>(row: &'r [(String, Option<String>)], var: &str) -> &'r str {
    let cell = row
        .iter()
        .find(|(v, _)| v == var)
        .and_then(|(_, c)| c.as_deref())
        .unwrap_or_else(|| panic!("?{var} unbound in {row:?}"));
    assert!(cell.starts_with("_:"), "?{var} is a blank node: {cell}");
    cell
}

/// Every variant of `q` — the materializing executor; the streaming one
/// with a `LIMIT`, and with `ORDER BY` + `LIMIT` (the bounded heap) when
/// `order` names a variable — gives `n` rows whose `?{fresh}` nodes are all
/// different, and in each row every variable of `same` holds the row's
/// `?{fresh}` node.
fn assert_fresh_per_solution(
    db: &Database,
    q: &str,
    n: usize,
    fresh: &str,
    same: &[&str],
    order: Option<&str>,
) {
    let mut variants = vec![
        (q.to_string(), TopKOptions::NONE),
        (format!("{q} LIMIT 10"), TopKOptions::ALL),
    ];
    if let Some(v) = order {
        variants.push((format!("{q} ORDER BY ?{v} LIMIT 10"), TopKOptions::ALL));
    }
    for (q, options) in variants {
        let rows = rows(db, &q, options);
        assert_eq!(rows.len(), n, "{q}");
        let mut nodes = HashSet::new();
        for row in &rows {
            let node = cell(row, fresh);
            for v in same {
                assert_eq!(cell(row, v), node, "{q}: one node per string in {row:?}");
            }
            nodes.insert(node.to_string());
        }
        assert_eq!(
            nodes.len(),
            n,
            "{q} ({options:?}): equal solutions shared a node: {rows:?}"
        );
    }
}

/// Failing before: `VALUES ?v { 1 1 }` has two solutions, which got one
/// node — as a SELECT expression and as a `BIND`.
#[test]
fn a_repeated_values_row_is_two_solutions_with_two_nodes() {
    let db = Database::build_from_str("bnode_repeated", PATHS).unwrap();
    assert_fresh_per_solution(
        &db,
        "SELECT ?v (BNODE(\"x\") AS ?b) WHERE { VALUES ?v { 1 1 } }",
        2,
        "b",
        &[],
        Some("v"),
    );
    assert_fresh_per_solution(
        &db,
        "SELECT ?v ?b WHERE { VALUES ?v { 1 1 } BIND(BNODE(\"x\") AS ?b) }",
        2,
        "b",
        &[],
        Some("v"),
    );
    // Within each solution a BIND, a later BIND and a SELECT expression
    // still see one node for the string.
    assert_fresh_per_solution(
        &db,
        "SELECT ?v ?b1 ?b2 (BNODE(\"x\") AS ?b3) WHERE { VALUES ?v { 1 1 } \
         BIND(BNODE(\"x\") AS ?b1) BIND(BNODE(\"x\") AS ?b2) }",
        2,
        "b1",
        &["b2", "b3"],
        Some("v"),
    );
}

/// Failing before: a path reaching `<z>` from `<a>` by two routes is two
/// solutions (§18.4), which got one node.
#[test]
fn a_path_reached_by_two_routes_is_two_solutions_with_two_nodes() {
    let db = Database::build_from_str("bnode_routes", PATHS).unwrap();
    assert_fresh_per_solution(
        &db,
        "SELECT ?x ?y (BNODE(\"r\") AS ?route) WHERE { ?x <http://ex/p>/<http://ex/q> ?y }",
        2,
        "route",
        &[],
        Some("x"),
    );
    assert_fresh_per_solution(
        &db,
        "SELECT ?x ?y ?route WHERE { ?x <http://ex/p>/<http://ex/q> ?y \
         BIND(BNODE(\"r\") AS ?route) }",
        2,
        "route",
        &[],
        Some("x"),
    );
}

/// Failing before: solutions that differ only in a node `BNODE()` minted
/// were taken for one (the minted node is left out of the bindings that
/// identify a solution), so their `BNODE("x")` was one node.
#[test]
fn solutions_differing_only_in_a_minted_node_get_their_own_nodes() {
    let db = Database::build_from_str("bnode_minted", PATHS).unwrap();
    assert_fresh_per_solution(
        &db,
        "SELECT ?v (BNODE() AS ?a) (BNODE(\"x\") AS ?c) WHERE { VALUES ?v { 1 1 } }",
        2,
        "c",
        &[],
        Some("v"),
    );
}

/// A solution that holds a node minted earlier but was joined since is a
/// new solution: two equal ones made by the join get two nodes.
#[test]
fn equal_solutions_made_by_a_join_after_a_bind_get_their_own_nodes() {
    let db = Database::build_from_str("bnode_join", PATHS).unwrap();
    assert_fresh_per_solution(
        &db,
        "SELECT ?w ?b1 ?b2 WHERE { { BIND(BNODE(\"x\") AS ?b1) } VALUES ?w { 1 1 } \
         BIND(BNODE(\"y\") AS ?b2) }",
        2,
        "b2",
        &[],
        Some("w"),
    );
    // The node bound before the join is the one solution's, shared by both.
    let rows = rows(
        &db,
        "SELECT ?w ?b1 WHERE { { BIND(BNODE(\"x\") AS ?b1) } VALUES ?w { 1 1 } }",
        TopKOptions::NONE,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(cell(&rows[0], "b1"), cell(&rows[1], "b1"));
}
