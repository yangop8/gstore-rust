//! Data test: the LUBM benchmark (`testdata/lubm/lubm.nt`, ~100k triples).
//!
//! This is the scale + correctness regression. The bundled `lubm_q*.rq` queries
//! are the standard LUBM workload, rewritten (as in the gStore repo) to use
//! explicit type UNIONs instead of relying on RDFS inference — so the expected
//! answer counts are exact and reasoning-independent. Half the queries exercise
//! UNION; all exercise multi-pattern joins over the six-way index.

use std::path::PathBuf;

use gstore::Database;

fn lubm_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm")
}

fn build() -> Database {
    Database::build_from_files("lubm", &[lubm_dir().join("lubm.nt")]).expect("build lubm.nt")
}

#[test]
fn build_stats_match() {
    let db = build();
    assert_eq!(db.triple_num(), 100_543);
    assert_eq!(db.predicate_num(), 17);
}

#[test]
fn all_standard_queries_return_expected_counts() {
    let mut db = build();

    // (query file, expected row count) — validated against published LUBM(1)
    // answers using the non-inference query rewrites bundled with gStore.
    let expected: &[(&str, usize)] = &[
        ("lubm_q1.rq", 4),
        ("lubm_q2.rq", 0),
        ("lubm_q3.rq", 6),
        ("lubm_q4.rq", 10),
        ("lubm_q5.rq", 678),
        ("lubm_q6.rq", 7790),
        ("lubm_q7.rq", 67),
        ("lubm_q8.rq", 7790),
        ("lubm_q9.rq", 102),
        ("lubm_q10.rq", 4),
        ("lubm_q11.rq", 224),
        ("lubm_q12.rq", 15),
        ("lubm_q13.rq", 8330),
        ("lubm_q14.rq", 5916),
    ];

    for (file, want) in expected {
        let q = std::fs::read_to_string(lubm_dir().join(file)).expect("read query");
        let rs = db.select(&q).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(rs.row_count(), *want, "{file} row count");
    }
}

#[test]
fn query1_returns_specific_graduate_students() {
    let mut db = build();
    let q = std::fs::read_to_string(lubm_dir().join("lubm_q1.rq")).unwrap();
    let rs = db.select(&q).unwrap();
    let got: std::collections::HashSet<String> =
        rs.rows.iter().map(|r| r[0].clone().unwrap()).collect();
    // The four graduate students taking GraduateCourse0.
    for id in [
        "<http://www.Department0.University0.edu/GraduateStudent44>",
        "<http://www.Department0.University0.edu/GraduateStudent101>",
        "<http://www.Department0.University0.edu/GraduateStudent124>",
        "<http://www.Department0.University0.edu/GraduateStudent142>",
    ] {
        assert!(got.contains(id), "missing {id}");
    }
}

#[test]
fn save_load_preserves_lubm_query() {
    let dir = std::env::temp_dir().join("gstore_dt_lubm_roundtrip");
    let _ = std::fs::remove_dir_all(&dir);

    let db = build();
    db.save(&dir).expect("save");
    let mut reloaded = Database::load(&dir).expect("load");
    assert_eq!(reloaded.triple_num(), 100_543);

    let q = std::fs::read_to_string(lubm_dir().join("lubm_q6.rq")).unwrap();
    assert_eq!(reloaded.select(&q).unwrap().row_count(), 7790);

    let _ = std::fs::remove_dir_all(&dir);
}
