//! Data test: the gStore `small.nt` graph — build, query, update, persist.
//!
//! Uses the bundled `testdata/small/small.nt` (the canonical gStore demo graph)
//! to exercise the full pipeline through the public [`gstore::Database`] API.

use std::path::PathBuf;

use gstore::{Database, QueryResult, Term, Triple};

fn small_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/small/small.nt")
}

fn build() -> Database {
    Database::build_from_files("small", &[small_nt()]).expect("build small.nt")
}

fn select_count(db: &mut Database, q: &str) -> usize {
    db.select(q).expect("query ok").row_count()
}

#[test]
fn loads_expected_triple_count() {
    let db = build();
    assert_eq!(db.triple_num(), 25, "small.nt has 25 triples");
}

#[test]
fn single_pattern_lookups() {
    let mut db = build();
    // root contains node0..node4
    assert_eq!(
        select_count(&mut db, "SELECT ?o WHERE { <root> <contain> ?o }"),
        5
    );
    // node1 owns point0, point1
    assert_eq!(
        select_count(&mut db, "SELECT ?o WHERE { <node1> <own> ?o }"),
        2
    );
}

#[test]
fn literal_object_roundtrips() {
    let mut db = build();
    let rs = db.select("SELECT ?n WHERE { <root> <name> ?n }").unwrap();
    assert_eq!(rs.rows[0][0], Some("\"Bookug Lobert\"".to_string()));
}

#[test]
fn two_hop_join_counts_owned_points() {
    let mut db = build();
    // every point owned by a node contained in root:
    // node1:2 + node2:3 + node3:1 + node4:2 = 8 (node0 owns none)
    let n = select_count(
        &mut db,
        "SELECT ?pt WHERE { <root> <contain> ?n . ?n <own> ?pt }",
    );
    assert_eq!(n, 8);
}

#[test]
fn ask_existing_and_missing() {
    let mut db = build();
    match db.query("ASK { <root> <contain> <node0> }").unwrap() {
        QueryResult::Ask(b) => assert!(b),
        other => panic!("got {other:?}"),
    }
    match db.query("ASK { <root> <contain> <nobody> }").unwrap() {
        QueryResult::Ask(b) => assert!(!b),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn update_then_query_reflects_change() {
    let mut db = build();
    let before = db.triple_num();

    // INSERT DATA adds a new contained node.
    db.query("INSERT DATA { <root> <contain> <node99> }")
        .unwrap();
    assert_eq!(db.triple_num(), before + 1);
    assert_eq!(
        select_count(&mut db, "SELECT ?o WHERE { <root> <contain> ?o }"),
        6
    );

    // DELETE DATA removes it again.
    db.query("DELETE DATA { <root> <contain> <node99> }")
        .unwrap();
    assert_eq!(db.triple_num(), before);
    assert_eq!(
        select_count(&mut db, "SELECT ?o WHERE { <root> <contain> ?o }"),
        5
    );
}

#[test]
fn programmatic_insert_remove() {
    let mut db = build();
    let t = Triple::new(Term::iri("node0"), Term::iri("own"), Term::iri("pointZ"));
    assert!(db.insert_triple(&t));
    assert_eq!(
        select_count(&mut db, "SELECT ?o WHERE { <node0> <own> ?o }"),
        1
    );
    assert!(db.remove_triple(&t));
    assert_eq!(
        select_count(&mut db, "SELECT ?o WHERE { <node0> <own> ?o }"),
        0
    );
}

#[test]
fn save_load_roundtrip_preserves_query_results() {
    let dir = std::env::temp_dir().join("gstore_dt_small_roundtrip");
    let _ = std::fs::remove_dir_all(&dir);

    let db = build();
    db.save(&dir).expect("save");

    let mut reloaded = Database::load(&dir).expect("load");
    assert_eq!(reloaded.triple_num(), 25);
    assert_eq!(
        select_count(
            &mut reloaded,
            "SELECT ?pt WHERE { <root> <contain> ?n . ?n <own> ?pt }"
        ),
        8
    );

    let _ = std::fs::remove_dir_all(&dir);
}
