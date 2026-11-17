//! Data test: the gStore `num` dataset — FILTER with arithmetic & logic.
//!
//! `testdata/num/num.nt` is the FOAF-ish graph from gStore's `data/num`; the
//! `num*.sql` files are its FILTER-heavy queries. These assertions pin the exact
//! solutions, validating the typed-value comparison and arithmetic path.

use std::path::PathBuf;

use gstore::Database;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/num")
}

fn build() -> Database {
    Database::build_from_files("num", &[dir().join("num.nt")]).expect("build num.nt")
}

fn read(q: &str) -> String {
    std::fs::read_to_string(dir().join(q)).expect("read query")
}

/// Collect the value of a single projected column across all rows, sorted.
fn column(db: &mut Database, q: &str, col: usize) -> Vec<String> {
    let rs = db.select(q).expect("query ok");
    let mut v: Vec<String> = rs
        .rows
        .iter()
        .map(|r| r[col].clone().unwrap_or_default())
        .collect();
    v.sort();
    v
}

#[test]
fn num1_filter_abs_and_comparison() {
    // FILTER(?sx < ?sy && abs(?sx - ?sy) < 3000)
    // Only Alice(2500) knows Bob(5000): 2500<5000 and |2500-5000|=2500<3000.
    let mut db = build();
    let rs = db.select(&read("num1.sql")).expect("num1");
    assert_eq!(rs.row_count(), 1);
    // columns: ?nx ?ny ?sx ?sy
    assert_eq!(rs.rows[0][0], Some("\"Alice\"".to_string()));
    assert_eq!(rs.rows[0][1], Some("\"Bob\"".to_string()));
}

#[test]
fn num2_filter_nested_or() {
    // FILTER(?sx > ?sy && (?hx > ?hy || ?hx >= 170.0))
    let mut db = build();
    let rs = db.select(&read("num2.sql")).expect("num2");
    assert_eq!(rs.row_count(), 4);
    // The higher earner is always ?x; check each ?nx is David or Bob.
    let nx = column(&mut db, &read("num2.sql"), 0);
    for name in &nx {
        assert!(
            name == "\"David\"" || name == "\"Bob\"",
            "unexpected high earner {name}"
        );
    }
}

/// The subjects whose salary passes `filter` (a FILTER over `?s`), sorted.
fn earners(db: &mut Database, filter: &str) -> Vec<String> {
    column(
        db,
        &format!(
            "PREFIX foaf: <http://xmlns.com/foaf/0.1/>
             SELECT ?x WHERE {{ ?x foaf:salary ?s . FILTER({filter}) }}"
        ),
        0,
    )
}

#[test]
fn filter_strict_inequality_excludes_equal() {
    // A direct check that FILTER numeric comparison is strict. review
    // tests-b#12: the threshold used to be 100000, which no salary equals
    // (the largest is David's 10000), so a `>` evaluated as `>=` passed;
    // the thresholds are now the largest and the smallest salaries, each
    // compared strictly and non-strictly.
    let mut db = build();
    assert_eq!(earners(&mut db, "?s > 10000"), Vec::<String>::new());
    assert_eq!(
        earners(&mut db, "?s >= 10000"),
        vec!["<http://example/david>"]
    );
    assert_eq!(earners(&mut db, "?s < 2000"), Vec::<String>::new());
    assert_eq!(
        earners(&mut db, "?s <= 2000"),
        vec!["<http://example/carol>"]
    );
    assert_eq!(
        earners(&mut db, "?s != 2500"),
        vec![
            "<http://example/bob>",
            "<http://example/carol>",
            "<http://example/david>"
        ]
    );
}

#[test]
fn order_by_salary_desc() {
    let mut db = build();
    let rs = db
        .select(
            "PREFIX foaf: <http://xmlns.com/foaf/0.1/>
             SELECT ?x ?s WHERE { ?x foaf:salary ?s } ORDER BY DESC(?s) LIMIT 1",
        )
        .unwrap();
    assert_eq!(rs.row_count(), 1);
    // Highest salary in num.nt is David's 10000 (review tests-b#12: the
    // full typed term and the subject, not a substring of the cell).
    assert_eq!(rs.rows[0][0].as_deref(), Some("<http://example/david>"));
    assert_eq!(
        rs.rows[0][1].as_deref(),
        Some("\"10000\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
}
