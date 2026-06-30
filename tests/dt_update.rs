//! Data test: SPARQL 1.1 UPDATE — INSERT/DELETE DATA, DELETE/INSERT … WHERE,
//! DELETE WHERE, LOAD, CLEAR/DROP, and `;`-separated sequences, end to end.

use gstore::{Database, QueryResult};
use std::io::Write;

const DATA: &str = r#"
@prefix : <http://ex/> .
:alice :age 30 ; :city :paris .
:bob   :age 25 ; :city :paris .
:carol :age 40 ; :city :rome .
"#;

fn db() -> Database {
    Database::build_from_str("upd", DATA).expect("build")
}

fn count(db: &mut Database, q: &str) -> usize {
    db.select(q).unwrap().row_count()
}

fn changed(db: &mut Database, q: &str) -> usize {
    match db.query(q).unwrap() {
        QueryResult::Update { changed } => changed,
        other => panic!("expected Update, got {other:?}"),
    }
}

#[test]
fn insert_and_delete_data() {
    let mut db = db();
    let n = changed(
        &mut db,
        "INSERT DATA { <http://ex/dave> <http://ex/age> 50 }",
    );
    assert_eq!(n, 1);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/age> 50 }"), 1);

    let n = changed(
        &mut db,
        "DELETE DATA { <http://ex/dave> <http://ex/age> 50 }",
    );
    assert_eq!(n, 1);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/age> 50 }"), 0);
}

#[test]
fn delete_where_removes_matches() {
    let mut db = db();
    // Remove everyone's city edge.
    let n = changed(&mut db, "DELETE WHERE { ?s <http://ex/city> ?c }");
    assert_eq!(n, 3);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/city> ?c }"), 0);
    // age edges untouched
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/age> ?a }"), 3);
}

#[test]
fn delete_insert_where_rewrites() {
    let mut db = db();
    // Move everyone in paris to lyon: delete city paris, insert city lyon.
    let n = changed(
        &mut db,
        "DELETE { ?s <http://ex/city> <http://ex/paris> }
         INSERT { ?s <http://ex/city> <http://ex/lyon> }
         WHERE  { ?s <http://ex/city> <http://ex/paris> }",
    );
    assert_eq!(n, 4); // 2 deletes + 2 inserts
    assert_eq!(
        count(
            &mut db,
            "SELECT ?s WHERE { ?s <http://ex/city> <http://ex/paris> }"
        ),
        0
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT ?s WHERE { ?s <http://ex/city> <http://ex/lyon> }"
        ),
        2
    );
}

#[test]
fn insert_where_derives_new_triples() {
    let mut db = db();
    // Everyone with an age is a :Person.
    let n = changed(
        &mut db,
        "INSERT { ?s a <http://ex/Person> } WHERE { ?s <http://ex/age> ?a }",
    );
    assert_eq!(n, 3);
    assert_eq!(
        count(
            &mut db,
            "SELECT ?s WHERE { ?s a <http://ex/Person> }"
        ),
        3
    );
}

#[test]
fn clear_all_empties_the_store() {
    let mut db = db();
    let n = changed(&mut db, "CLEAR ALL");
    assert_eq!(n, 6); // 6 triples cleared
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s ?p ?o }"), 0);
    // a named-graph DROP is a no-op (we keep only the default graph)
    assert_eq!(changed(&mut db, "DROP SILENT GRAPH <http://ex/g>"), 0);
}

#[test]
fn update_sequence_applies_in_order() {
    let mut db = db();
    let n = changed(
        &mut db,
        "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> } ;
         DELETE WHERE { ?s <http://ex/city> ?c }",
    );
    assert_eq!(n, 1 + 3);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }"), 1);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/city> ?c }"), 0);
}

#[test]
fn load_reads_local_turtle_file() {
    let mut db = db();
    let dir = std::env::temp_dir();
    let path = dir.join("gstore_load_test.ttl");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "@prefix : <http://ex/> .\n:eve :age 22 .").unwrap();
    }
    let q = format!("LOAD <file://{}>", path.display());
    let n = changed(&mut db, &q);
    assert_eq!(n, 1);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/age> 22 }"), 1);
    std::fs::remove_file(&path).ok();
}

#[test]
fn load_remote_errors_unless_silent() {
    let mut db = db();
    assert!(db.query("LOAD <http://example.org/x.ttl>").is_err());
    // SILENT swallows the failure.
    assert_eq!(changed(&mut db, "LOAD SILENT <http://example.org/x.ttl>"), 0);
}

#[test]
fn transaction_rollback_reverts_updates() {
    let mut db = db();
    let before = count(&mut db, "SELECT ?s WHERE { ?s ?p ?o }");
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/dave> <http://ex/age> 50 }").unwrap();
    db.query("DELETE WHERE { ?s <http://ex/city> ?c }").unwrap();
    // changes are visible inside the transaction
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/age> 50 }"), 1);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/city> ?c }"), 0);
    db.rollback().unwrap();
    // everything is restored
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/age> 50 }"), 0);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/city> ?c }"), 3);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s ?p ?o }"), before);
}

#[test]
fn transaction_commit_persists_updates() {
    let mut db = db();
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/dave> <http://ex/age> 50 }").unwrap();
    db.commit().unwrap();
    db.rollback().unwrap_err(); // nothing to roll back after commit
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s <http://ex/age> 50 }"), 1);
}

#[test]
fn rollback_restores_cleared_store() {
    let mut db = db();
    db.begin().unwrap();
    assert_eq!(changed(&mut db, "CLEAR ALL"), 6);
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s ?p ?o }"), 0);
    db.rollback().unwrap();
    assert_eq!(count(&mut db, "SELECT ?s WHERE { ?s ?p ?o }"), 6);
}

#[test]
fn transaction_misuse_errors() {
    let mut db = db();
    assert!(db.commit().is_err()); // no active transaction
    assert!(db.rollback().is_err());
    db.begin().unwrap();
    assert!(db.begin().is_err()); // no nesting
    db.commit().unwrap();
}
