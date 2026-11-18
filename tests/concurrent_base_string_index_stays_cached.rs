//! A `ConcurrentDb`'s base snapshot keeps its dictionary substring index
//! for base-state reads, whatever a view over another state asks first
//! (round-4 review of design#9's fix).
//!
//! The base snapshot's index cell keeps the first index it is asked for. A
//! version view over a state that interned new terms (another dictionary
//! generation) asked the base's cell with its own dictionary; the cell then
//! held that index, served no base-state read, and never replaced it — so
//! every substring query of a base-state view rebuilt the index from the
//! whole dictionary. A view over another state now builds an index of its
//! own and leaves the base's cell alone. The work counter
//! `gstore::query::engine::string_index_builds()` (per thread) counts the
//! builds.

use gstore::query::engine::string_index_builds;
use gstore::{ConcurrentDb, QueryResult, Term, Triple};

/// Enough literals for a substring index to be built (and kept).
fn data() -> String {
    (0..200)
        .map(|i| format!("<http://ex/s{i}> <http://ex/name> \"name {i}\" .\n"))
        .collect()
}

const CONTAINS: &str =
    "SELECT ?s WHERE { ?s <http://ex/name> ?n FILTER(CONTAINS(?n, \"name 19\")) }";

fn rows(r: gstore::Result<QueryResult>) -> usize {
    match r.unwrap() {
        QueryResult::Select(rs) => rs.row_count(),
        other => panic!("not a SELECT result: {other:?}"),
    }
}

#[test]
fn a_view_of_another_state_leaves_the_base_index_to_base_state_reads() {
    let c = ConcurrentDb::build_from_str("idx", &data()).unwrap();
    // A view of the base state, then a commit that interns a new term.
    let base_view = c.version_view();
    c.run_txn(4, |t| {
        t.insert(Triple::new(
            Term::iri("http://ex/new"),
            Term::iri("http://ex/name"),
            Term::iri("http://ex/newname"),
        ));
        Ok(())
    })
    .unwrap();
    let later = c.version_view();

    // The view over the new state asks first: an index of its own.
    let before = string_index_builds();
    assert_eq!(rows(later.query(CONTAINS)), 11);
    assert_eq!(string_index_builds() - before, 1);

    // The base-state view builds the base's index once and keeps it.
    let before = string_index_builds();
    for _ in 0..3 {
        assert_eq!(rows(base_view.query(CONTAINS)), 11);
    }
    assert_eq!(
        string_index_builds() - before,
        1,
        "the base-state view rebuilt the substring index for every query"
    );
}
