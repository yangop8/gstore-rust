//! SPARQL 1.1 §11.4 (Aggregate Projection Restrictions): in an aggregate
//! query — one with GROUP BY, or with an aggregate in the SELECT clause,
//! HAVING or ORDER BY — a variable of the WHERE pattern may be projected,
//! or used in a SELECT expression outside an aggregate, only when it is a
//! GROUP BY key (review sparqlconf#15). The engine used to answer such a
//! query with the value of the group's first solution; it is a query error
//! now, on every layout and with every executor setting. The legal forms
//! (keys, aliases, aggregates, SAMPLE) are in `tests/w3c/aggregates-grouping/`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use gstore::query::TopKOptions;
use gstore::{Database, GStoreError, QueryResult};

const DATA: &str = "@prefix : <http://ex/> .\n\
    :a :p :b , :c , \"Alice\" .\n\
    :b :p :c .\n\
    :c :q :a .\n";

/// A temp directory removed when dropped (also when the test fails).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore_aggproj_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The database on every layout this test builds: in memory, and the
/// on-disk store (whatever `GSTORE_DISK_QUERY_MODE` says).
fn databases(tmp: &TempDir) -> Vec<(&'static str, Database)> {
    let data = tmp.0.join("data.ttl");
    std::fs::write(&data, DATA).unwrap();
    let disk = tmp.0.join("disk");
    Database::build_disk(&disk, &[&data]).unwrap();
    vec![
        ("memory", Database::build_from_str("aggproj", DATA).unwrap()),
        ("disk", Database::open(&disk).unwrap()),
    ]
}

/// Run `q` on every layout with the materializing evaluator and with every
/// streaming optimization on; the results, one per run.
fn run_everywhere(q: &str) -> Vec<(String, gstore::Result<QueryResult>)> {
    let tmp = TempDir::new();
    let mut out = Vec::new();
    for (layout, mut db) in databases(&tmp) {
        db.set_query_cache_size(0);
        for (exec, opts) in [("full", TopKOptions::NONE), ("stream", TopKOptions::ALL)] {
            db.set_topk_options(opts);
            out.push((format!("{layout}/{exec}"), db.query(q)));
        }
    }
    out
}

/// `q` fails everywhere with a query error naming `var` as not grouped.
fn rejected(q: &str, var: &str) {
    for (run, r) in run_everywhere(q) {
        match r {
            Err(GStoreError::Query(msg)) => {
                assert!(
                    msg.contains(&format!("?{var} is not grouped")) && msg.contains("§11.4"),
                    "[{run}] {q}: unexpected message {msg:?}"
                );
                // One sentence: the source's line breaks must not leak runs
                // of indentation into the message a client sees.
                assert!(
                    !msg.contains("  ") && msg.contains("of the WHERE pattern, or use it"),
                    "[{run}] {q}: garbled message {msg:?}"
                );
            }
            other => panic!("[{run}] {q}: expected a query error, got {other:?}"),
        }
    }
}

/// `q` is answered everywhere with `rows` rows.
fn accepted(q: &str, rows: usize) {
    for (run, r) in run_everywhere(q) {
        match r {
            Ok(QueryResult::Select(rs)) => {
                assert_eq!(rs.row_count(), rows, "[{run}] {q}: {:?}", rs.rows)
            }
            other => panic!("[{run}] {q}: expected rows, got {other:?}"),
        }
    }
}

#[test]
fn a_non_grouped_projected_variable_is_a_query_error() {
    // The review's repro: :a has three objects, and one of them was shown.
    rejected(
        "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o } GROUP BY ?s",
        "o",
    );
    rejected(
        "SELECT ?s ?o (COUNT(*) AS ?c) WHERE { ?s ?p ?o } GROUP BY ?s",
        "o",
    );
}

#[test]
fn an_implicit_group_restricts_the_projection_too() {
    // An aggregate without GROUP BY makes one group of all solutions.
    rejected("SELECT ?o (COUNT(*) AS ?c) WHERE { ?s ?p ?o }", "o");
    rejected("SELECT ?s WHERE { ?s ?p ?o } HAVING (COUNT(*) > 1)", "s");
}

#[test]
fn a_select_expression_may_not_use_a_non_grouped_variable() {
    rejected(
        "SELECT ?s (STR(?o) AS ?x) WHERE { ?s <http://ex/p> ?o } GROUP BY ?s",
        "o",
    );
    // Outside the aggregate, next to one.
    rejected(
        "SELECT ?s (COUNT(?o) + STRLEN(STR(?o)) AS ?x) WHERE { ?s <http://ex/p> ?o } \
         GROUP BY ?s",
        "o",
    );
}

#[test]
fn a_sub_select_follows_the_same_rule() {
    rejected(
        "SELECT * WHERE { { SELECT ?s ?o WHERE { ?s <http://ex/p> ?o } GROUP BY ?s } }",
        "o",
    );
}

#[test]
fn keys_aliases_aggregates_and_unmentioned_variables_stay_legal() {
    // Group keys and aggregates (two subjects with :p).
    accepted(
        "SELECT ?s (COUNT(?o) AS ?n) (SAMPLE(?o) AS ?any) WHERE { ?s <http://ex/p> ?o } \
         GROUP BY ?s",
        2,
    );
    // An earlier alias in a later expression.
    accepted(
        "SELECT ?s (COUNT(?o) AS ?n) (?n * 2 AS ?twice) WHERE { ?s <http://ex/p> ?o } \
         GROUP BY ?s",
        2,
    );
    // A variable the pattern never binds is unbound in every group.
    accepted(
        "SELECT ?s ?nowhere (COUNT(?o) AS ?n) WHERE { ?s <http://ex/p> ?o } GROUP BY ?s",
        2,
    );
    // Lenient (documented): an expression equal to a GROUP BY expression.
    accepted(
        "SELECT (STR(?o) AS ?k) (COUNT(*) AS ?n) WHERE { ?s <http://ex/p> ?o } \
         GROUP BY STR(?o)",
        3,
    );
    // A trailing VALUES variable is joined after grouping.
    accepted(
        "SELECT ?s ?w (COUNT(?o) AS ?n) WHERE { ?s <http://ex/p> ?o } GROUP BY ?s \
         VALUES ?w { 1 }",
        2,
    );
}
