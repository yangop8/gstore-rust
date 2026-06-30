//! Data test: administrative APIs — query cache, schema extraction, stats.

use gstore::Database;

const DATA: &str = r#"
@prefix : <http://ex/> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

:GradStudent rdfs:subClassOf :Student .
:advises rdfs:domain :Professor ; rdfs:range :Student .
:alice rdf:type :GradStudent ; :name "Alice" .
:bob :advises :alice .
"#;

fn db() -> Database {
    Database::build_from_str("admin", DATA).expect("build")
}

#[test]
fn query_cache_serves_repeated_queries_and_invalidates_on_update() {
    let mut db = db();
    let q = "SELECT ?s WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/GradStudent> }";
    // First and second calls agree (second is served from cache).
    assert_eq!(db.select(q).unwrap().row_count(), 1);
    assert_eq!(db.select(q).unwrap().row_count(), 1);

    // An update must invalidate the cache: the next read reflects it.
    db.query("INSERT DATA { <http://ex/carol> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/GradStudent> }")
        .unwrap();
    assert_eq!(db.select(q).unwrap().row_count(), 2);

    // Rollback also invalidates.
    db.begin().unwrap();
    db.query("DELETE WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/GradStudent> }")
        .unwrap();
    assert_eq!(db.select(q).unwrap().row_count(), 0);
    db.rollback().unwrap();
    assert_eq!(db.select(q).unwrap().row_count(), 2);
}

#[test]
fn schema_lists_classes_and_properties() {
    let db = db();
    let s = db.schema();
    // Classes: GradStudent, Student (subClassOf), Professor (domain), Student (range).
    assert!(s.classes.contains(&"<http://ex/GradStudent>".to_string()));
    assert!(s.classes.contains(&"<http://ex/Student>".to_string()));
    assert!(s.classes.contains(&"<http://ex/Professor>".to_string()));
    // Properties: name, advises used in data; advises also has domain/range.
    assert!(s.properties.contains(&"<http://ex/advises>".to_string()));
    assert!(s.properties.contains(&"<http://ex/name>".to_string()));
}

#[test]
fn stats_report_counts_and_status() {
    let mut db = db();
    let s = db.stats();
    assert_eq!(s.triple_num, db.triple_num());
    assert_eq!(s.entity_num, db.entity_num());
    assert!(!s.in_transaction);

    db.begin().unwrap();
    assert!(db.stats().in_transaction);
    db.commit().unwrap();
    assert!(!db.stats().in_transaction);
}
