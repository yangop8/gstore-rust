//! `Database`-level regressions for the dictionary id-space bounds: every
//! insert path must surface exhaustion as an error (or a `false` from the
//! infallible `bool` API) instead of handing out an id in the wrong range.
//!
//! A child module of `db`, so it can advance the private dictionary's counters
//! with [`Dictionary::debug_advance_id_counters`] instead of interning two
//! billion terms.

use super::*;
use crate::dict::{ENTITY_ID_LIMIT, LITERAL_INDEX_LIMIT};
use crate::model::id::{is_entity_id, LITERAL_FIRST_ID};

/// A database whose next entity id is the last valid one.
fn nearly_full_entities() -> Database {
    let mut db = Database::new("full");
    db.dict
        .debug_advance_id_counters(ENTITY_ID_LIMIT - 1, 0, 0)
        .unwrap();
    db
}

fn exhausted(e: &GStoreError) -> bool {
    e.to_string().contains("id space exhausted")
}

#[test]
fn insert_data_reports_entity_exhaustion() {
    let mut db = nearly_full_entities();
    // <s> takes the last entity id; <o> would be the 2,000,000,000th entity.
    let err = db
        .query("INSERT DATA { <http://ex/s> <http://ex/p> <http://ex/o> . }")
        .unwrap_err();
    assert!(exhausted(&err), "{err}");
    assert_eq!(db.triple_num(), 0);
    let s = db
        .dict
        .entity_id(&Term::iri("http://ex/s").dict_key())
        .unwrap();
    assert_eq!(s, LITERAL_FIRST_ID - 1);
    assert!(is_entity_id(s));
    // Nothing was given an id in the literal range.
    assert!(db
        .dict
        .entity_id(&Term::iri("http://ex/o").dict_key())
        .is_none());

    // A literal object still fits (separate space), so this insert succeeds.
    let r = db
        .query("INSERT DATA { <http://ex/s> <http://ex/p> \"v\" . }")
        .unwrap();
    assert_eq!(r, QueryResult::Update { changed: 1 });
    let rs = db
        .select("SELECT ?o WHERE { <http://ex/s> <http://ex/p> ?o }")
        .unwrap();
    assert_eq!(rs.rows, vec![vec![Some("\"v\"".to_string())]]);
}

#[test]
fn modify_and_named_graph_inserts_report_exhaustion() {
    // <http://ex/a> takes the last entity id; every insert below needs a new one.
    let mut fresh = nearly_full_entities();
    fresh
        .query("INSERT DATA { <http://ex/a> <http://ex/p> \"1\" . }")
        .unwrap();

    let err = fresh
        .query("INSERT { ?s <http://ex/q> <http://ex/new> } WHERE { ?s <http://ex/p> ?o }")
        .unwrap_err();
    assert!(exhausted(&err), "{err}");
    let err = fresh
        .query("INSERT DATA { GRAPH <http://ex/g> { <http://ex/a> <http://ex/p> \"2\" } }")
        .unwrap_err();
    assert!(exhausted(&err), "{err}");
}

#[test]
fn bool_insert_api_returns_false_and_try_variant_errs() {
    let mut db = nearly_full_entities();
    let t = Triple::new(
        Term::iri("http://ex/a"),
        Term::iri("http://ex/p"),
        Term::iri("http://ex/b"),
    );
    assert!(db.try_insert_triple(&t).is_err());
    assert!(!db.insert_triple(&t));
    assert_eq!(db.triple_num(), 0);
    // insert_data (usize API) skips the unencodable triple; try_insert_data errs.
    let g = GroundTriple {
        subject: t.subject.clone(),
        predicate: t.predicate.clone(),
        object: t.object.clone(),
        graph: None,
    };
    assert_eq!(db.insert_data(std::slice::from_ref(&g)), 0);
    assert!(db.try_insert_data(&[g]).is_err());
}

#[test]
fn literal_exhaustion_is_reported_too() {
    let mut db = Database::new("lit");
    db.dict
        .debug_advance_id_counters(0, LITERAL_INDEX_LIMIT, 0)
        .unwrap();
    let err = db
        .query("INSERT DATA { <http://ex/s> <http://ex/p> \"x\" . }")
        .unwrap_err();
    assert!(
        err.to_string().contains("literal id space exhausted"),
        "{err}"
    );
}

#[test]
fn load_reports_exhaustion() {
    let dir = std::env::temp_dir().join(format!("gstore_hardening_load_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let f = dir.join("x.nt");
    fs::write(&f, "<http://ex/a> <http://ex/p> <http://ex/b> .\n").unwrap();
    let mut db = nearly_full_entities();
    let err = db
        .query(&format!("LOAD <file://{}>", f.display()))
        .unwrap_err();
    assert!(exhausted(&err), "{err}");
    let _ = fs::remove_dir_all(&dir);
}
