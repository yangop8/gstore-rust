//! Regression tests for the cloud-review (ultrareview) findings on the query
//! core: COUNT(DISTINCT *), the all-predicate-variable planner panic, and
//! sub-SELECT predicate-variable dictionary routing.

use gstore::Database;

/// SPARQL counts render as typed `xsd:integer` literals.
fn int(n: i64) -> Option<String> {
    Some(format!("\"{n}\"^^<http://www.w3.org/2001/XMLSchema#integer>"))
}

/// bug_003: `COUNT(DISTINCT *)` must dedup whole solution mappings, not behave
/// like `COUNT(*)`.
#[test]
fn count_distinct_star_dedups_solutions() {
    let mut db = Database::build_from_str("cd", "<a> <p> <x> .\n<b> <p> <x> .\n").unwrap();

    // Two identical UNION arms ⇒ 4 solution mappings, 2 distinct.
    let q = "SELECT (COUNT(DISTINCT *) AS ?c) WHERE { { ?s <p> <x> } UNION { ?s <p> <x> } }";
    let rs = db.select(q).unwrap();
    assert_eq!(rs.rows[0][0], int(2), "COUNT(DISTINCT *) = distinct");

    // Control: plain COUNT(*) still counts every mapping.
    let q2 = "SELECT (COUNT(*) AS ?c) WHERE { { ?s <p> <x> } UNION { ?s <p> <x> } }";
    let rs2 = db.select(q2).unwrap();
    assert_eq!(rs2.rows[0][0], int(4), "COUNT(*) = all mappings");
}

/// bug_002: a BGP whose patterns are all `<const> ?p <const>` (only predicates
/// are variables) used to panic the planner on an empty node set. It must now
/// plan and answer correctly. >14 patterns forces the `planner::plan` fallback.
#[test]
fn all_predicate_variable_bgp_does_not_panic() {
    let n = 16;
    let mut data = String::new();
    for i in 0..n {
        data.push_str(&format!("<a{i}> <p{i}> <b{i}> .\n"));
    }
    let mut db = Database::build_from_str("pv", &data).unwrap();

    let mut pat = String::new();
    for i in 0..n {
        pat.push_str(&format!("<a{i}> ?p{i} <b{i}> . "));
    }
    let q = format!("SELECT * WHERE {{ {pat} }}");
    let rs = db.select(&q).unwrap();

    assert_eq!(rs.row_count(), 1, "the conjunctive match has one solution");
    // Predicate variables resolve via the predicate dictionary.
    let col = rs.vars.iter().position(|v| v == "p0").unwrap();
    assert_eq!(rs.rows[0][col], Some("<p0>".into()));
}

/// bug_001: a variable that appears only in predicate position inside a
/// sub-SELECT must resolve through the predicate dictionary in the outer query,
/// not the entity/literal one.
#[test]
fn subselect_predicate_variable_resolves_correctly() {
    let mut db = Database::build_from_str("ss", "<a> <knows> <b> .\n<a> <likes> <c> .\n").unwrap();

    let q = "SELECT ?p WHERE { { SELECT ?p WHERE { ?s ?p ?o } } }";
    let rs = db.select(q).unwrap();

    let mut got: Vec<String> = rs
        .rows
        .iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec!["<knows>".to_string(), "<likes>".to_string()],
        "predicate ids from a sub-SELECT must resolve to their IRIs"
    );
}

/// bug_001 (corollary): grouping by a sub-SELECT predicate variable groups by
/// the right values.
#[test]
fn subselect_predicate_variable_groups_correctly() {
    let mut db = Database::build_from_str(
        "ssg",
        "<a> <knows> <b> .\n<a> <knows> <c> .\n<a> <likes> <c> .\n",
    )
    .unwrap();

    let q = "SELECT ?p (COUNT(*) AS ?c) WHERE { { SELECT ?p WHERE { ?s ?p ?o } } } GROUP BY ?p";
    let rs = db.select(q).unwrap();
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
    assert_eq!(
        pairs,
        vec![
            ("<knows>".to_string(), int(2).unwrap()),
            ("<likes>".to_string(), int(1).unwrap()),
        ]
    );
}
