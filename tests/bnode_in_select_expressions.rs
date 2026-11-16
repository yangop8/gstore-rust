//! `BNODE(str)` in SELECT expressions under every evaluation path (round-3
//! integration: r3-sparql-functions' per-solution `BNODE(str)` meets
//! r3-sparql-joins' left-to-right SELECT expressions).
//!
//! SPARQL 1.1 §17.4.2.9: `BNODE(str)` is the same blank node for the same
//! string within one solution mapping and a different one in every other
//! solution. §18.2.4.4: the SELECT expressions extend the solution left to
//! right, so a later expression sees an earlier alias bound — the node
//! `BNODE` itself minted must not make the second call's solution look like
//! another one. The materializing path (no modifiers), ORDER BY, the
//! LIMIT fast path, the ORDER BY + LIMIT top-k path and DISTINCT must all
//! agree.

use std::collections::HashSet;

use gstore::{Database, QueryResult, ResultSet};

const DATA: &str = "<http://ex/s1> <http://ex/str> \"foo\" .\n\
                    <http://ex/s2> <http://ex/str> \"foo\" .\n\
                    <http://ex/s3> <http://ex/str> \"bar\" .\n";

fn select(db: &mut Database, q: &str) -> ResultSet {
    match db.query(q) {
        Ok(QueryResult::Select(rs)) => rs,
        other => panic!("{q}: expected a SELECT result, got {other:?}"),
    }
}

fn cell<'r>(rs: &'r ResultSet, row: usize, var: &str) -> Option<&'r str> {
    let col = rs
        .vars
        .iter()
        .position(|v| v == var)
        .unwrap_or_else(|| panic!("no column {var} in {:?}", rs.vars));
    rs.rows[row][col].as_deref()
}

/// Every row of `q` has `?b1 = ?b2`, a blank node, and the rows hold
/// `rows` different nodes.
fn assert_one_node_per_solution(db: &mut Database, q: &str, rows: usize) {
    let rs = select(db, q);
    assert_eq!(rs.row_count(), rows, "{q}");
    let mut nodes = HashSet::new();
    for row in 0..rs.row_count() {
        let (b1, b2) = (cell(&rs, row, "b1"), cell(&rs, row, "b2"));
        assert!(
            b1.is_some_and(|c| c.starts_with("_:")),
            "{q}: row {row}: {b1:?}"
        );
        assert_eq!(
            b1, b2,
            "{q}: row {row}: one node per string within a solution"
        );
        nodes.insert(b1.unwrap().to_string());
    }
    assert_eq!(
        nodes.len(),
        rows,
        "{q}: a new node in every solution: {nodes:?}"
    );
}

#[test]
fn bnode_of_a_string_in_select_expressions_is_one_node_per_solution() {
    let mut db = Database::build_from_str("bnode_select", DATA).expect("build");
    let base = "SELECT ?s (BNODE(?v) AS ?b1) (BNODE(?v) AS ?b2) WHERE { ?s <http://ex/str> ?v }";
    for (q, rows) in [
        (base.to_string(), 3),
        (format!("{base} ORDER BY ?s"), 3),
        (format!("{base} LIMIT 10"), 3),
        (format!("{base} ORDER BY DESC(?s) LIMIT 2"), 2),
        (
            "SELECT DISTINCT ?s (BNODE(?v) AS ?b1) (BNODE(?v) AS ?b2) \
             WHERE { ?s <http://ex/str> ?v }"
                .to_string(),
            3,
        ),
        // An unbound alias between the two leaves the solution as it is.
        (
            "SELECT ?s (BNODE(?v) AS ?b1) (?nowhere AS ?t) (BNODE(?v) AS ?b2) \
             WHERE { ?s <http://ex/str> ?v }"
                .to_string(),
            3,
        ),
        // A BIND in the pattern and a SELECT expression: one solution, one node.
        (
            "SELECT ?s ?b1 (BNODE(?v) AS ?b2) \
             WHERE { ?s <http://ex/str> ?v BIND(BNODE(?v) AS ?b1) }"
                .to_string(),
            3,
        ),
    ] {
        assert_one_node_per_solution(&mut db, &q, rows);
    }
}
