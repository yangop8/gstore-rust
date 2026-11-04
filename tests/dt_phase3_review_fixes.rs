//! Regression tests for the Phase 3 review findings: decimal division
//! overflow, `xsd:float` type promotion, the RDF term identity of integer
//! literals of derived types, `COUNT(DISTINCT *)` over `[]`, fresh blank
//! nodes in the `CONSTRUCT WHERE` short form, and a SELECT expression that
//! refers to its own alias under `ORDER BY`.

use gstore::query::engine::TopKOptions;
use gstore::{Database, QueryResult};

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const P: &str = "PREFIX : <http://ex/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";

fn ex(local: &str) -> String {
    format!("<http://ex/{local}>")
}

fn typed(lex: &str, local: &str) -> String {
    format!("\"{lex}\"^^<{XSD}{local}>")
}

/// Rows of a SELECT, each cell rendered as its N-Triples form ("" = unbound),
/// sorted.
fn rows(db: &mut Database, q: &str) -> Vec<Vec<String>> {
    let rs = db.select(q).unwrap_or_else(|e| panic!("{q}\n=> {e}"));
    let mut v: Vec<Vec<String>> = rs
        .rows
        .iter()
        .map(|r| r.iter().map(|c| c.clone().unwrap_or_default()).collect())
        .collect();
    v.sort();
    v
}

/// The sorted first column.
fn col0(db: &mut Database, q: &str) -> Vec<String> {
    rows(db, q).into_iter().map(|mut r| r.remove(0)).collect()
}

fn construct(db: &mut Database, q: &str) -> Vec<String> {
    match db.query(q).unwrap_or_else(|e| panic!("{q}\n=> {e}")) {
        QueryResult::Construct(ts) => {
            let mut v: Vec<String> = ts.iter().map(|t| t.to_string()).collect();
            v.sort();
            v
        }
        other => panic!("expected CONSTRUCT triples, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Decimal division: i128::MIN / -1 must not panic
// ---------------------------------------------------------------------------

const I128_MIN: &str = "-170141183460469231731687303715884105728";

#[test]
fn integer_division_of_i128_min_by_minus_one_does_not_panic() {
    let data = format!("@prefix : <http://ex/> . :a :v \"{I128_MIN}\"^^<{XSD}integer> .");
    let mut db = Database::build_from_str("divmin", &data).unwrap();
    for q in [
        format!("{P} SELECT (\"{I128_MIN}\"^^xsd:integer / -1 AS ?x) WHERE {{}}"),
        format!("{P} SELECT (?v / -1 AS ?x) WHERE {{ :a :v ?v }}"),
        format!(
            "{P} SELECT ((\"-170141183460469231731687303715884105727\"^^xsd:integer - 1) / -1 \
             AS ?x) WHERE {{}}"
        ),
        // The same mantissas at a common decimal scale: (i128::MIN × 10⁻¹)
        // / (−1 × 10⁻¹).
        format!(
            "{P} SELECT ((-17014118346046923173168730371588410572.7 - 0.1) / -0.1 AS ?x) \
             WHERE {{}}"
        ),
    ] {
        // 2^127 does not fit i128: approximated as a double, like any other
        // decimal result beyond i128.
        let got = rows(&mut db, &q);
        assert_eq!(got.len(), 1, "{q}");
        let (lex, dt) = got[0][0]
            .strip_prefix('"')
            .and_then(|c| c.split_once("\"^^"))
            .unwrap_or_else(|| panic!("{q}: {got:?}"));
        assert_eq!(dt, format!("<{XSD}double>"), "{q}: {got:?}");
        let x: f64 = lex.parse().unwrap();
        let two_127 = 2f64.powi(127);
        assert!((x - two_127).abs() / two_127 < 1e-12, "{q}: {got:?}");
    }
}

// ---------------------------------------------------------------------------
// xsd:float against xsd:decimal / xsd:integer: promote to float
// ---------------------------------------------------------------------------

fn float_db() -> Database {
    Database::build_from_str(
        "floatpromo",
        r#"@prefix : <http://ex/> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
           :a :f "0.1"^^xsd:float .
           :b :g "16777217"^^xsd:float .
           :c :h "0.1"^^xsd:decimal ."#,
    )
    .unwrap()
}

#[test]
fn float_compares_with_decimal_and_integer_at_single_precision() {
    let mut db = float_db();
    let sel = |f: &str| format!("{P} SELECT ?s WHERE {{ ?s :f ?f FILTER({f}) }}");
    assert_eq!(col0(&mut db, &sel("?f = 0.1")), vec![ex("a")]);
    assert_eq!(col0(&mut db, &sel("?f IN (0.1)")), vec![ex("a")]);
    assert!(col0(&mut db, &sel("?f != 0.1")).is_empty());
    assert!(col0(&mut db, &sel("?f > 0.1")).is_empty());
    assert_eq!(col0(&mut db, &sel("?f >= 0.1")), vec![ex("a")]);
    // A double is not promoted down to float: 0.1f32 is not 0.1e0.
    assert!(col0(&mut db, &sel("?f = 0.1e0")).is_empty());
    assert_eq!(col0(&mut db, &sel("?f > 0.1e0")), vec![ex("a")]);
    // 16777217 promoted to xsd:float is 16777216, the stored float.
    assert_eq!(
        col0(
            &mut db,
            &format!("{P} SELECT ?s WHERE {{ ?s :g ?f FILTER(?f = 16777217) }}")
        ),
        vec![ex("b")]
    );
    // A join against the stored decimal.
    assert_eq!(
        col0(
            &mut db,
            &format!("{P} SELECT ?s WHERE {{ ?s :f ?f . ?c :h ?d FILTER(?f = ?d) }}")
        ),
        vec![ex("a")]
    );
    // Arithmetic promotes the decimal too: 0.1f32 - 0.1 is exactly 0.
    assert_eq!(
        col0(
            &mut db,
            &format!("{P} SELECT (?f - 0.1 AS ?d) WHERE {{ :a :f ?f }}")
        ),
        vec![typed("0", "float")]
    );
}

// ---------------------------------------------------------------------------
// Integer literals of derived types keep their RDF term
// ---------------------------------------------------------------------------

fn derived_db() -> Database {
    Database::build_from_str(
        "derivedint",
        r#"@prefix : <http://ex/> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
           :a :v "05"^^xsd:unsignedByte .   :b :r "05"^^xsd:unsignedByte .
           :c :v "-3"^^xsd:negativeInteger . :d :r "-3"^^xsd:negativeInteger .
           :e :v "7"^^xsd:unsignedShort .   :f :r "7"^^xsd:unsignedShort .
           :g :v "0"^^xsd:nonPositiveInteger . :h :r "0"^^xsd:nonPositiveInteger .
           :i :v "09"^^xsd:int .            :j :r "09"^^xsd:int ."#,
    )
    .unwrap()
}

#[test]
fn derived_integer_literals_keep_datatype_and_lexical_form() {
    let mut db = derived_db();
    assert_eq!(
        rows(
            &mut db,
            &format!("{P} SELECT ?x (DATATYPE(?v) AS ?d) (STR(?v) AS ?s) WHERE {{ ?x :v ?v }}")
        ),
        vec![
            vec![ex("a"), format!("<{XSD}unsignedByte>"), "\"05\"".into()],
            vec![ex("c"), format!("<{XSD}negativeInteger>"), "\"-3\"".into()],
            vec![ex("e"), format!("<{XSD}unsignedShort>"), "\"7\"".into()],
            vec![
                ex("g"),
                format!("<{XSD}nonPositiveInteger>"),
                "\"0\"".into()
            ],
            vec![ex("i"), format!("<{XSD}int>"), "\"09\"".into()],
        ]
    );
    // A constant keeps its datatype too.
    assert_eq!(
        col0(
            &mut db,
            &format!("{P} SELECT (DATATYPE(\"5\"^^xsd:unsignedByte) AS ?d) WHERE {{}}")
        ),
        vec![format!("<{XSD}unsignedByte>")]
    );
    // Computed values are canonical xsd:integer as before.
    assert_eq!(
        rows(
            &mut db,
            &format!(
                "{P} SELECT (DATATYPE(?v + 0) AS ?d) (STR(?v + 0) AS ?s) WHERE {{ :a :v ?v }}"
            )
        ),
        vec![vec![format!("<{XSD}integer>"), "\"5\"".into()]]
    );
}

#[test]
fn bind_and_select_expression_copy_the_bound_term() {
    let mut db = derived_db();
    let pairs = vec![
        vec![ex("a"), ex("b")],
        vec![ex("c"), ex("d")],
        vec![ex("e"), ex("f")],
        vec![ex("g"), ex("h")],
        vec![ex("i"), ex("j")],
    ];
    // BIND(?v AS ?w) binds the very same term, so it joins.
    assert_eq!(
        rows(
            &mut db,
            &format!("{P} SELECT ?x ?s WHERE {{ ?x :v ?v BIND(?v AS ?w) ?s :r ?w }}")
        ),
        pairs
    );
    // So does a sub-SELECT's (?v AS ?w).
    assert_eq!(
        rows(
            &mut db,
            &format!(
                "{P} SELECT ?x ?s WHERE {{ {{ SELECT ?x (?v AS ?w) WHERE {{ ?x :v ?v }} }} \
                 ?s :r ?w }}"
            )
        ),
        pairs
    );
    // A projected copy is the stored term, with or without ORDER BY, with
    // or without the Top-K optimizations.
    let want = vec![vec![typed("05", "unsignedByte")]];
    for q in [
        format!("{P} SELECT (?v AS ?w) WHERE {{ :a :v ?v }}"),
        format!("{P} SELECT (?v AS ?w) WHERE {{ :a :v ?v }} ORDER BY ?w"),
        format!("{P} SELECT (?v AS ?w) WHERE {{ :a :v ?v }} ORDER BY ?w LIMIT 1"),
    ] {
        for opts in [TopKOptions::ALL, TopKOptions::NONE] {
            db.set_topk_options(opts);
            assert_eq!(rows(&mut db, &q), want, "{q} {opts:?}");
        }
    }
    db.set_topk_options(TopKOptions::ALL);
    // A grouped copy of the group key too.
    assert_eq!(
        rows(
            &mut db,
            &format!("{P} SELECT (?v AS ?w) WHERE {{ :a :v ?v }} GROUP BY ?v")
        ),
        want
    );
}

#[test]
fn same_term_distinguishes_lexical_forms_and_datatypes() {
    let mut db = derived_db();
    let sel = |f: &str| format!("{P} SELECT ?x WHERE {{ ?x :v ?v FILTER({f}) }}");
    // "05"^^xsd:unsignedByte equals 5 by value but is not the same term.
    assert_eq!(col0(&mut db, &sel("?v = 5")), vec![ex("a")]);
    assert!(col0(&mut db, &sel("sameTerm(?v, 5)")).is_empty());
    assert_eq!(
        col0(&mut db, &sel("sameTerm(?v, \"05\"^^xsd:unsignedByte)")),
        vec![ex("a")]
    );
    // A simple literal and an xsd:string literal are the same RDF term.
    assert_eq!(
        col0(
            &mut db,
            &format!(
                "{P} SELECT ?x WHERE {{ :a :v ?x FILTER(sameTerm(\"s\", \"s\"^^xsd:string)) }}"
            )
        ),
        vec![typed("05", "unsignedByte")]
    );
}

// ---------------------------------------------------------------------------
// COUNT(DISTINCT *) ignores the hidden variables of [] / ( … )
// ---------------------------------------------------------------------------

#[test]
fn count_distinct_star_ignores_anonymous_blank_node_variables() {
    let mut db =
        Database::build_from_str("cdanon", "@prefix : <http://ex/> . :a :p :b . :a :p :c .")
            .unwrap();
    let one = vec![vec![typed("1", "integer")]];
    // `SELECT DISTINCT *` agrees: one (empty) solution mapping.
    assert_eq!(
        rows(
            &mut db,
            &format!("{P} SELECT DISTINCT * WHERE {{ :a :p [] }}")
        )
        .len(),
        1
    );
    assert_eq!(
        rows(
            &mut db,
            &format!("{P} SELECT (COUNT(DISTINCT *) AS ?c) WHERE {{ :a :p [] }}")
        ),
        one
    );
    assert_eq!(
        rows(
            &mut db,
            &format!("{P} SELECT (COUNT(DISTINCT *) AS ?c) WHERE {{ ?s :p [] }}")
        ),
        one
    );
    assert_eq!(
        rows(
            &mut db,
            &format!("{P} SELECT ?s (COUNT(DISTINCT *) AS ?c) WHERE {{ ?s :p [] }} GROUP BY ?s")
        ),
        vec![vec![ex("a"), typed("1", "integer")]]
    );
    // Visible variables still count: four solutions, two distinct ?o.
    assert_eq!(
        rows(
            &mut db,
            &format!("{P} SELECT (COUNT(DISTINCT *) AS ?c) WHERE {{ :a :p ?o . :a :p [] }}")
        ),
        vec![vec![typed("2", "integer")]]
    );
    // COUNT(*) still counts every solution.
    assert_eq!(
        rows(
            &mut db,
            &format!("{P} SELECT (COUNT(*) AS ?c) WHERE {{ :a :p [] }}")
        ),
        vec![vec![typed("2", "integer")]]
    );
}

// ---------------------------------------------------------------------------
// CONSTRUCT WHERE: template blank nodes are fresh per solution
// ---------------------------------------------------------------------------

/// Replace each `_:label` in an N-Triples line with `_:b`.
fn mask_blanks(line: &str) -> String {
    line.split(' ')
        .map(|w| if w.starts_with("_:") { "_:b" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn construct_where_short_form_mints_fresh_blank_nodes() {
    let mut db = Database::build_from_str(
        "cwfresh",
        "@prefix : <http://ex/> . :a :p :b . :a :p :c . :x :q :y . :y :r :z .",
    )
    .unwrap();
    for q in [
        format!("{P} CONSTRUCT WHERE {{ ?s :p [] }}"),
        format!("{P} CONSTRUCT {{ ?s :p [] }} WHERE {{ ?s :p [] }}"),
    ] {
        let got = construct(&mut db, &q);
        assert_eq!(got.len(), 2, "{q}: {got:?}");
        assert!(
            got.iter()
                .all(|t| mask_blanks(t) == format!("{} {} _:b .", ex("a"), ex("p"))),
            "{q}: {got:?}"
        );
        // One fresh node per solution.
        assert_ne!(got[0], got[1], "{q}");
    }
    // A blank node property list: one fresh node shared by both template
    // triples of a solution.
    let got = construct(
        &mut db,
        &format!("{P} CONSTRUCT WHERE {{ :x :q [ :r ?o ] }}"),
    );
    let masked: Vec<String> = got.iter().map(|t| mask_blanks(t)).collect();
    assert_eq!(
        masked,
        vec![
            format!("{} {} _:b .", ex("x"), ex("q")),
            format!("_:b {} {} .", ex("r"), ex("z")),
        ],
        "{got:?}"
    );
    let label = |t: &str| {
        t.split(' ')
            .find(|w| w.starts_with("_:"))
            .unwrap()
            .to_owned()
    };
    assert_eq!(label(&got[0]), label(&got[1]), "{got:?}");
}

// ---------------------------------------------------------------------------
// ORDER BY with a SELECT expression that refers to its own alias
// ---------------------------------------------------------------------------

#[test]
fn self_referencing_alias_under_order_by_is_unbound_on_every_path() {
    let mut db = Database::build_from_str(
        "selfalias",
        "@prefix : <http://ex/> . :e0 :val 1 . :e1 :val 5 . :e2 :val 3 .",
    )
    .unwrap();
    let all = vec![
        vec![ex("e0"), String::new()],
        vec![ex("e1"), String::new()],
        vec![ex("e2"), String::new()],
    ];
    for opts in [TopKOptions::ALL, TopKOptions::NONE] {
        db.set_topk_options(opts);
        let base = format!("{P} SELECT ?s (?a + 1 AS ?a) WHERE {{ ?s :val ?x }} ORDER BY ?a");
        assert_eq!(rows(&mut db, &base), all, "{opts:?}");
        let limited = rows(&mut db, &format!("{base} LIMIT 2"));
        assert_eq!(limited.len(), 2, "{opts:?}");
        assert!(limited.iter().all(|r| r[1].is_empty()), "{opts:?}");
    }
}
