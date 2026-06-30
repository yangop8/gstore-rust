//! Data test: RDFS entailment materialization (Database::materialize_rdfs).

use gstore::Database;

const DATA: &str = r#"
@prefix : <http://ex/> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

:GradStudent rdfs:subClassOf :Student .
:Student     rdfs:subClassOf :Person .
:alice       rdf:type :GradStudent .

:advises rdfs:domain :Professor .
:advises rdfs:range  :Student .
:bob :advises :carol .

:mother rdfs:subPropertyOf :parent .
:dave :mother :eve .
"#;

fn db() -> Database {
    Database::build_from_str("reason", DATA).expect("build")
}

fn col0(db: &mut Database, q: &str) -> Vec<String> {
    let rs = db.select(q).unwrap();
    let mut v: Vec<String> = rs
        .rows
        .iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect();
    v.sort();
    v
}

#[test]
fn type_propagates_through_subclass_chain() {
    let mut db = db();
    // Before reasoning, alice is only a GradStudent.
    assert_eq!(
        col0(&mut db, "SELECT ?x WHERE { ?x a <http://ex/Person> }"),
        Vec::<String>::new()
    );
    let added = db.materialize_rdfs();
    assert!(added > 0);
    // After reasoning, alice is a Student and a Person.
    assert_eq!(
        col0(&mut db, "SELECT ?x WHERE { ?x a <http://ex/Student> }"),
        vec!["<http://ex/alice>", "<http://ex/carol>"] // carol via advises range
    );
    assert_eq!(
        col0(&mut db, "SELECT ?x WHERE { ?x a <http://ex/Person> }"),
        vec!["<http://ex/alice>", "<http://ex/carol>"]
    );
}

#[test]
fn domain_and_range_infer_types() {
    let mut db = db();
    db.materialize_rdfs();
    assert_eq!(
        col0(&mut db, "SELECT ?x WHERE { ?x a <http://ex/Professor> }"),
        vec!["<http://ex/bob>"] // advises domain Professor
    );
    assert!(
        col0(&mut db, "SELECT ?x WHERE { ?x a <http://ex/Student> }")
            .contains(&"<http://ex/carol>".to_string()),
        "advises range Student"
    );
}

#[test]
fn subproperty_propagates() {
    let mut db = db();
    db.materialize_rdfs();
    assert_eq!(
        col0(&mut db, "SELECT ?x WHERE { ?x <http://ex/parent> <http://ex/eve> }"),
        vec!["<http://ex/dave>"] // mother ⊑ₚ parent
    );
}

#[test]
fn materialize_is_idempotent() {
    let mut db = db();
    let first = db.materialize_rdfs();
    let second = db.materialize_rdfs();
    assert!(first > 0);
    assert_eq!(second, 0, "re-running adds nothing");
}
