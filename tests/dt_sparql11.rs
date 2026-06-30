//! Data test: SPARQL 1.1 features — OPTIONAL, MINUS, BIND, VALUES, sub-SELECT,
//! aggregates (GROUP BY / HAVING), and CONSTRUCT — evaluated end to end.

use gstore::{Database, QueryResult};

const DATA: &str = r#"
@prefix : <http://ex/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
:alice :dept :d1 ; :salary 2500 ; :name "Alice" ; :knows :bob .
:bob   :dept :d1 ; :salary 3000 ; :name "Bob" .
:carol :dept :d2 ; :salary 4000 ; :name "Carol" .
"#;

fn db() -> Database {
    Database::build_from_str("s11", DATA).expect("build")
}

/// Sorted single-column values.
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
fn optional_left_join_keeps_unmatched() {
    let mut db = db();
    // Everyone with a salary; their knows-target is OPTIONAL.
    let rs = db
        .select(
            "SELECT ?p ?k WHERE { ?p <http://ex/salary> ?s OPTIONAL { ?p <http://ex/knows> ?k } }",
        )
        .unwrap();
    assert_eq!(rs.row_count(), 3); // alice, bob, carol all retained
                                   // Exactly one row (alice) has a bound ?k.
    let bound = rs.rows.iter().filter(|r| r[1].is_some()).count();
    assert_eq!(bound, 1);
    let alice = rs
        .rows
        .iter()
        .find(|r| r[0].as_deref() == Some("<http://ex/alice>"))
        .unwrap();
    assert_eq!(alice[1].as_deref(), Some("<http://ex/bob>"));
}

#[test]
fn minus_removes_compatible() {
    let mut db = db();
    // People who do NOT know anyone (alice knows bob → removed).
    let got = col0(
        &mut db,
        "SELECT ?p WHERE { ?p <http://ex/salary> ?s MINUS { ?p <http://ex/knows> ?k } }",
    );
    assert_eq!(got, vec!["<http://ex/bob>", "<http://ex/carol>"]);
}

#[test]
fn bind_computes_value() {
    let mut db = db();
    let rs = db
        .select("SELECT ?p ?bonus WHERE { ?p <http://ex/salary> ?s . BIND(?s * 2 AS ?bonus) }")
        .unwrap();
    let carol = rs
        .rows
        .iter()
        .find(|r| r[0].as_deref() == Some("<http://ex/carol>"))
        .unwrap();
    assert!(carol[1].as_ref().unwrap().contains("8000"));
}

#[test]
fn values_restricts_subjects() {
    let mut db = db();
    let got = col0(
        &mut db,
        "SELECT ?p WHERE { VALUES ?p { <http://ex/alice> <http://ex/carol> } ?p <http://ex/salary> ?s }",
    );
    assert_eq!(got, vec!["<http://ex/alice>", "<http://ex/carol>"]);
}

#[test]
fn subselect_inner_query() {
    let mut db = db();
    let got = col0(
        &mut db,
        "SELECT ?p WHERE { { SELECT ?p WHERE { ?p <http://ex/dept> <http://ex/d1> } } }",
    );
    assert_eq!(got, vec!["<http://ex/alice>", "<http://ex/bob>"]);
}

#[test]
fn count_per_group() {
    let mut db = db();
    let rs = db
        .select("SELECT ?dept (COUNT(?p) AS ?c) WHERE { ?p <http://ex/dept> ?dept } GROUP BY ?dept")
        .unwrap();
    assert_eq!(rs.row_count(), 2);
    // d1 → 2, d2 → 1
    for r in &rs.rows {
        let dept = r[0].as_deref().unwrap();
        let c = r[1].as_deref().unwrap();
        if dept == "<http://ex/d1>" {
            assert!(c.contains('2'));
        } else {
            assert!(c.contains('1'));
        }
    }
}

#[test]
fn sum_and_having() {
    let mut db = db();
    // Departments with more than one member, and their total salary.
    let rs = db
        .select(
            "SELECT ?dept (SUM(?s) AS ?total) WHERE {
                ?p <http://ex/dept> ?dept . ?p <http://ex/salary> ?s
             } GROUP BY ?dept HAVING(COUNT(?p) > 1)",
        )
        .unwrap();
    assert_eq!(rs.row_count(), 1); // only d1 has >1 member
    assert_eq!(rs.rows[0][0].as_deref(), Some("<http://ex/d1>"));
    assert!(rs.rows[0][1].as_ref().unwrap().contains("5500")); // 2500 + 3000
}

#[test]
fn avg_min_max_over_all() {
    let mut db = db();
    let rs = db
        .select(
            "SELECT (AVG(?s) AS ?a) (MIN(?s) AS ?mn) (MAX(?s) AS ?mx) (COUNT(*) AS ?n)
             WHERE { ?p <http://ex/salary> ?s }",
        )
        .unwrap();
    assert_eq!(rs.row_count(), 1);
    let row = &rs.rows[0];
    assert!(row[0].as_ref().unwrap().contains("3166")); // (2500+3000+4000)/3
    assert!(row[1].as_ref().unwrap().contains("2500"));
    assert!(row[2].as_ref().unwrap().contains("4000"));
    assert!(row[3].as_ref().unwrap().contains('3'));
}

#[test]
fn group_concat_names() {
    let mut db = db();
    let rs = db
        .select(
            "SELECT (GROUP_CONCAT(?n ; SEPARATOR=\",\") AS ?names)
             WHERE { ?p <http://ex/name> ?n }",
        )
        .unwrap();
    let names = rs.rows[0][0].as_ref().unwrap();
    // All three names present, comma-separated.
    for n in ["Alice", "Bob", "Carol"] {
        assert!(names.contains(n), "missing {n} in {names}");
    }
}

#[test]
fn count_distinct() {
    let mut db = db();
    let rs = db
        .select("SELECT (COUNT(DISTINCT ?dept) AS ?c) WHERE { ?p <http://ex/dept> ?dept }")
        .unwrap();
    assert!(rs.rows[0][0].as_ref().unwrap().contains('2')); // d1, d2
}

const CHAIN: &str = r#"
@prefix : <http://ex/> .
:a :p :b . :b :p :c . :c :p :d .
:a :type :T .
"#;

fn chain_col(q: &str, col: usize) -> Vec<String> {
    let mut db = Database::build_from_str("chain", CHAIN).unwrap();
    let rs = db.select(q).unwrap();
    let mut v: Vec<String> = rs
        .rows
        .iter()
        .map(|r| r[col].clone().unwrap_or_default())
        .collect();
    v.sort();
    v
}

#[test]
fn path_one_or_more() {
    // :a :p+ ?x → b, c, d
    let got = chain_col("SELECT ?x WHERE { <http://ex/a> <http://ex/p>+ ?x }", 0);
    assert_eq!(got, vec!["<http://ex/b>", "<http://ex/c>", "<http://ex/d>"]);
}

#[test]
fn path_zero_or_more_is_reflexive() {
    // :a :p* ?x → a, b, c, d (includes the start)
    let got = chain_col("SELECT ?x WHERE { <http://ex/a> <http://ex/p>* ?x }", 0);
    assert_eq!(
        got,
        vec![
            "<http://ex/a>",
            "<http://ex/b>",
            "<http://ex/c>",
            "<http://ex/d>"
        ]
    );
}

#[test]
fn path_inverse() {
    // `:b ^:p ?x` ≡ `?x :p :b` → a (a is the subject pointing to b).
    let got = chain_col("SELECT ?x WHERE { <http://ex/b> ^<http://ex/p> ?x }", 0);
    assert_eq!(got, vec!["<http://ex/a>"]);
}

#[test]
fn path_sequence() {
    // :a :p/:p ?x → c (two steps)
    let got = chain_col(
        "SELECT ?x WHERE { <http://ex/a> <http://ex/p>/<http://ex/p> ?x }",
        0,
    );
    assert_eq!(got, vec!["<http://ex/c>"]);
}

#[test]
fn path_alternative() {
    // :a (:p|:type) ?x → b (via p) and T (via type)
    let got = chain_col(
        "SELECT ?x WHERE { <http://ex/a> (<http://ex/p>|<http://ex/type>) ?x }",
        0,
    );
    assert_eq!(got, vec!["<http://ex/T>", "<http://ex/b>"]);
}

#[test]
fn construct_builds_graph() {
    let mut db = db();
    match db
        .query("CONSTRUCT { ?p <http://ex/worksIn> ?d } WHERE { ?p <http://ex/dept> ?d }")
        .unwrap()
    {
        QueryResult::Construct(triples) => {
            assert_eq!(triples.len(), 3);
            assert!(triples.iter().all(|t| {
                matches!(&t.predicate, gstore::Term::Iri(i) if i == "http://ex/worksIn")
            }));
        }
        other => panic!("expected Construct, got {other:?}"),
    }
}

#[test]
fn exists_keeps_only_knowers() {
    let mut db = db();
    // Only people who know someone (alice knows bob).
    let got = col0(
        &mut db,
        "SELECT ?p WHERE { ?p <http://ex/salary> ?s FILTER EXISTS { ?p <http://ex/knows> ?k } }",
    );
    assert_eq!(got, vec!["<http://ex/alice>"]);
}

#[test]
fn not_exists_removes_knowers() {
    let mut db = db();
    let got = col0(
        &mut db,
        "SELECT ?p WHERE { ?p <http://ex/salary> ?s FILTER NOT EXISTS { ?p <http://ex/knows> ?k } }",
    );
    assert_eq!(got, vec!["<http://ex/bob>", "<http://ex/carol>"]);
}

#[test]
fn path_zero_or_one_is_reflexive_plus_one_hop() {
    let mut db = db();
    // alice knows? ?o → alice herself (zero hops) and bob (one hop).
    let got = col0(
        &mut db,
        "SELECT ?o WHERE { <http://ex/alice> <http://ex/knows>? ?o }",
    );
    assert_eq!(got, vec!["<http://ex/alice>", "<http://ex/bob>"]);
}

#[test]
fn describe_returns_outgoing_triples() {
    let mut db = db();
    match db.query("DESCRIBE <http://ex/alice>").unwrap() {
        QueryResult::Construct(triples) => {
            // alice: dept d1, salary 2500, name "Alice", knows bob.
            assert_eq!(triples.len(), 4);
            assert!(triples.iter().all(|t| {
                matches!(&t.subject, gstore::Term::Iri(i) if i == "http://ex/alice")
            }));
            assert!(triples
                .iter()
                .any(|t| matches!(&t.predicate, gstore::Term::Iri(i) if i == "http://ex/knows")));
        }
        other => panic!("expected Construct, got {other:?}"),
    }
}

#[test]
fn describe_star_over_where() {
    let mut db = db();
    // Describe every ?p in dept d1 (alice, bob): their outgoing triples.
    match db
        .query("DESCRIBE * WHERE { ?p <http://ex/dept> <http://ex/d1> }")
        .unwrap()
    {
        QueryResult::Construct(triples) => {
            // alice (4) + bob (3: dept, salary, name) = 7.
            assert_eq!(triples.len(), 7);
        }
        other => panic!("expected Construct, got {other:?}"),
    }
}
