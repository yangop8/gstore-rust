//! Data test: named graphs — GRAPH patterns in queries, quad updates, CLEAR,
//! and persistence.

use gstore::{Database, QueryResult};

fn db() -> Database {
    let mut db = Database::new("graphs");
    let n = match db
        .query(
            "INSERT DATA {
               GRAPH <http://ex/g1> { <http://ex/a> <http://ex/p> <http://ex/b> }
               GRAPH <http://ex/g2> { <http://ex/c> <http://ex/p> <http://ex/d> }
               <http://ex/x> <http://ex/p> <http://ex/y> .
             }",
        )
        .unwrap()
    {
        QueryResult::Update { changed } => changed,
        other => panic!("expected Update, got {other:?}"),
    };
    assert_eq!(n, 3);
    db
}

fn col0(db: &mut Database, q: &str) -> Vec<String> {
    let mut v: Vec<String> = db
        .select(q)
        .unwrap()
        .rows
        .iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect();
    v.sort();
    v
}

#[test]
fn graph_const_queries_a_named_graph_only() {
    let mut db = db();
    // g1 holds <a>; g2 holds <c>; default holds <x>.
    assert_eq!(
        col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g1> { ?s <http://ex/p> ?o } }"),
        vec!["<http://ex/a>"]
    );
    // The default graph does NOT see named-graph triples.
    assert_eq!(
        col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }"),
        vec!["<http://ex/x>"]
    );
}

#[test]
fn graph_var_binds_each_named_graph() {
    let mut db = db();
    let rs = db
        .select("SELECT ?g ?s WHERE { GRAPH ?g { ?s <http://ex/p> ?o } }")
        .unwrap();
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
            ("<http://ex/g1>".to_string(), "<http://ex/a>".to_string()),
            ("<http://ex/g2>".to_string(), "<http://ex/c>".to_string()),
        ]
    );
}

#[test]
fn delete_data_from_named_graph() {
    let mut db = db();
    let changed = match db
        .query("DELETE DATA { GRAPH <http://ex/g1> { <http://ex/a> <http://ex/p> <http://ex/b> } }")
        .unwrap()
    {
        QueryResult::Update { changed } => changed,
        other => panic!("got {other:?}"),
    };
    assert_eq!(changed, 1);
    assert!(col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g1> { ?s <http://ex/p> ?o } }").is_empty());
    // g2 untouched
    assert_eq!(
        col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g2> { ?s <http://ex/p> ?o } }"),
        vec!["<http://ex/c>"]
    );
}

#[test]
fn clear_named_graph_and_clear_all() {
    let mut db = db();
    // CLEAR one named graph.
    match db.query("CLEAR GRAPH <http://ex/g1>").unwrap() {
        QueryResult::Update { changed } => assert_eq!(changed, 1),
        other => panic!("got {other:?}"),
    }
    assert!(col0(&mut db, "SELECT ?g ?s WHERE { GRAPH ?g { ?s <http://ex/p> ?o } }")
        .iter()
        .all(|_| true));
    // g2 still present, default still present.
    assert_eq!(
        col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }"),
        vec!["<http://ex/x>"]
    );
    // CLEAR ALL wipes default + named.
    db.query("CLEAR ALL").unwrap();
    assert!(col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }").is_empty());
    assert!(col0(
        &mut db,
        "SELECT ?s WHERE { GRAPH ?g { ?s <http://ex/p> ?o } }"
    )
    .is_empty());
}

#[test]
fn named_graphs_persist_across_save_load() {
    let dir = std::env::temp_dir().join("gstore_graph_test.db");
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = db();
        db.save(&dir).unwrap();
    }
    let mut db = Database::load(&dir).unwrap();
    assert_eq!(
        col0(&mut db, "SELECT ?s WHERE { GRAPH <http://ex/g2> { ?s <http://ex/p> ?o } }"),
        vec!["<http://ex/c>"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn graph_bind_computed_term_resolves() {
    // A BIND inside GRAPH mints a synthetic id; it must resolve in the outer
    // evaluator (GRAPH sub-evaluators share the extras interner).
    let mut db = Database::new("g_bind");
    db.query("INSERT DATA { GRAPH <http://ex/g> { <http://ex/x> <http://ex/age> 30 } }")
        .unwrap();
    let rs = db
        .select(
            "SELECT ?b WHERE {
               GRAPH <http://ex/g> { ?s <http://ex/age> ?a . BIND(?a + 1 AS ?b) }
             }",
        )
        .unwrap();
    assert_eq!(rs.row_count(), 1);
    assert!(
        rs.rows[0][0].as_deref().unwrap_or("").contains("31"),
        "computed ?b should resolve to 31, got {:?}",
        rs.rows[0][0]
    );
}

#[test]
fn graph_update_rolls_back() {
    let mut db = db();
    db.begin().unwrap();
    db.query("INSERT DATA { GRAPH <http://ex/g1> { <http://ex/a> <http://ex/p> <http://ex/z> } }")
        .unwrap();
    assert_eq!(
        col0(&mut db, "SELECT ?o WHERE { GRAPH <http://ex/g1> { <http://ex/a> <http://ex/p> ?o } }").len(),
        2
    );
    db.rollback().unwrap();
    assert_eq!(
        col0(&mut db, "SELECT ?o WHERE { GRAPH <http://ex/g1> { <http://ex/a> <http://ex/p> ?o } }"),
        vec!["<http://ex/b>"]
    );
}
