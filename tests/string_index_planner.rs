//! End-to-end: a substring `FILTER` over a dictionary-backed variable is
//! accelerated by the [`StringIndex`](gstore::kvstore::StringIndex) candidate
//! pruning wired into the planner — and, crucially, still returns exactly the
//! same rows as the plain scan-and-verify path. The dataset is large enough
//! (>64 entity/literal strings) to enable the index.

use gstore::Database;

/// 100 nodes `<http://ex/nI> <http://ex/label> "…"`; three carry "MARKER".
fn dataset() -> String {
    let mut s = String::new();
    for i in 0..100 {
        let label = if [7, 42, 88].contains(&i) {
            format!("\"MARKER item {i}\"")
        } else {
            format!("\"plain label {i}\"")
        };
        s.push_str(&format!("<http://ex/n{i}> <http://ex/label> {label} .\n"));
    }
    s
}

fn count(db: &mut Database, q: &str) -> usize {
    db.select(q).unwrap().row_count()
}

#[test]
fn contains_filter_is_accelerated_and_correct() {
    let mut db = Database::build_from_str("t", &dataset()).unwrap();

    // CONTAINS on the object literal: exactly the three MARKER rows.
    assert_eq!(
        count(
            &mut db,
            "SELECT ?s WHERE { ?s <http://ex/label> ?o . FILTER(CONTAINS(STR(?o), \"MARKER\")) }"
        ),
        3
    );

    // A needle present nowhere: zero rows (the index's empty superset agrees
    // with the exact filter).
    assert_eq!(
        count(
            &mut db,
            "SELECT ?s WHERE { ?s <http://ex/label> ?o . FILTER(CONTAINS(STR(?o), \"ZZZZZ\")) }"
        ),
        0
    );

    // Soundness under superset over-inclusion: "http" occurs in the *IRI* strings
    // (so the index superset for ?o may include entity ids), but ?o only ever
    // binds label literals, which contain no "http" — the exact filter must still
    // yield zero rows, proving the candidate superset never adds false results.
    assert_eq!(
        count(
            &mut db,
            "SELECT ?s WHERE { ?s <http://ex/label> ?o . FILTER(CONTAINS(STR(?o), \"http\")) }"
        ),
        0
    );
}

#[test]
fn contains_is_sound_for_escaped_characters() {
    // The dictionary stores a value with a newline in N-Triples-escaped form
    // ("foo\nbar"), but CONTAINS matches the *unescaped* value. The planner's
    // StringIndex is keyed by the lexical value, so a needle containing the real
    // newline must still match — indexing the escaped dict form would drop it.
    // >64 entries keeps the index active.
    let mut s = String::new();
    for i in 0..80 {
        s.push_str(&format!(
            "<http://ex/n{i}> <http://ex/label> \"plain label {i}\" .\n"
        ));
    }
    // n7's tag value is "foo\nbar" with a real line feed.
    s.push_str("<http://ex/n7> <http://ex/tag> \"foo\\nbar\" .\n");
    let mut db = Database::build_from_str("esc", &s).unwrap();
    // Needle "o\nb" (o, LF, b) — a 3-byte substring of the unescaped value.
    let n = count(
        &mut db,
        "SELECT ?s WHERE { ?s <http://ex/tag> ?o . FILTER(CONTAINS(STR(?o), \"o\\nb\")) }",
    );
    assert_eq!(n, 1, "an escaped-character needle must match the unescaped value");
}

#[test]
fn strstarts_on_subject_iri_is_correct() {
    let mut db = Database::build_from_str("t", &dataset()).unwrap();
    // n9, n90..n99 → 11 subjects start with "http://ex/n9".
    assert_eq!(
        count(
            &mut db,
            "SELECT ?s WHERE { ?s <http://ex/label> ?o . FILTER(STRSTARTS(STR(?s), \"http://ex/n9\")) }"
        ),
        11
    );
}
