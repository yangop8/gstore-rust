//! Data test: SPARQL 1.1 grammar gaps closed in Phase 3 (docs/GAP_PLAN.md
//! §4.1 "语法"), each parsed per the W3C grammar and evaluated end to end on
//! small data, with negative tests for the error messages.
//!
//! Probe on the Phase 2 base (before this work), for the record: IN / NOT IN,
//! signed numbers and `true`/`false` as terms (patterns, VALUES, DATA), bare
//! built-in ORDER BY keys, CONSTRUCT WHERE, `HAVING c1 c2`, trailing VALUES,
//! `[ … ]` / `( … )`, ADD/MOVE/COPY, PN_LOCAL escapes and `:` in local names,
//! and `GROUP BY STR(?o)` were parse errors; `CONTAINALL(…)` parsed as an
//! unknown function and evaluated to an error; `GROUP BY (expr AS ?v)` parsed
//! but left `?v` unbound; `xsd:integer(?z)` as an ORDER BY key, IRI function
//! calls and `%XX` in local names already worked.

use gstore::parser::sparql::ast::{Expr, GraphPattern, Query, UpdateOp};
use gstore::parser::sparql::parse;
use gstore::{Database, GStoreError, QueryResult};

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

const DATA: &str = r#"
@prefix : <http://ex/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
:alice :age 30 ; :temp -5 ; :ok true ; :name "Alice" ; :knows :bob ; :dept :d1 ; :salary 100 .
:bob   :age 25 ; :temp 7 ; :ok false ; :name "Bob" ; :knows :carol ; :dept :d1 ; :salary 200 .
:carol :age 35 ; :temp -1.5e3 ; :name "Carol" ; :dept :d2 ; :salary 50 .
"#;

fn db() -> Database {
    Database::build_from_str("syntax", DATA).expect("build")
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

/// Rows in result order (unsorted).
fn ordered(db: &mut Database, q: &str) -> Vec<Vec<String>> {
    let rs = db.select(q).unwrap_or_else(|e| panic!("{q}\n=> {e}"));
    rs.rows
        .iter()
        .map(|r| r.iter().map(|c| c.clone().unwrap_or_default()).collect())
        .collect()
}

/// The sorted first column.
fn col0(db: &mut Database, q: &str) -> Vec<String> {
    rows(db, q).into_iter().map(|mut r| r.remove(0)).collect()
}

fn ex(local: &str) -> String {
    format!("<http://ex/{local}>")
}

fn int(n: &str) -> String {
    format!("\"{n}\"^^<{XSD}integer>")
}

fn parse_err(q: &str) -> String {
    match parse(q) {
        Ok(q) => panic!("expected a parse error, got {q:?}"),
        Err(GStoreError::SparqlParse(m)) => m,
        Err(e) => panic!("expected SparqlParse, got {e:?}"),
    }
}

fn assert_contains(msg: &str, parts: &[&str]) {
    for p in parts {
        assert!(msg.contains(p), "{msg:?} should contain {p:?}");
    }
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

fn ask(db: &mut Database, q: &str) -> bool {
    match db.query(q).unwrap_or_else(|e| panic!("{q}\n=> {e}")) {
        QueryResult::Ask(b) => b,
        other => panic!("expected ASK, got {other:?}"),
    }
}

fn update(db: &mut Database, q: &str) -> usize {
    match db.query(q).unwrap_or_else(|e| panic!("{q}\n=> {e}")) {
        QueryResult::Update { changed } => changed,
        other => panic!("expected an update, got {other:?}"),
    }
}

const P: &str = "PREFIX : <http://ex/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";

// ---------------------------------------------------------------------------
// 1. IN / NOT IN
// ---------------------------------------------------------------------------

#[test]
fn in_and_not_in_filter_rows() {
    let mut db = db();
    let q = format!("{P} SELECT ?p WHERE {{ ?p :age ?a FILTER(?a IN (25, 35, 99)) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("bob"), ex("carol")]);
    let q = format!("{P} SELECT ?p WHERE {{ ?p :age ?a FILTER(?a NOT IN (25, 35)) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("alice")]);
    // Members are arbitrary expressions, compared by value (30 = 30.0).
    let q = format!("{P} SELECT ?p WHERE {{ ?p :age ?a FILTER(?a IN (20 + 10, \"x\")) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("alice")]);
    // IRIs and case-insensitive keywords.
    let q = format!("{P} SELECT ?p WHERE {{ ?p :knows ?k FILTER(?k in (:carol)) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("bob")]);
    let q = format!("{P} SELECT ?p WHERE {{ ?p :dept ?d FILTER(?d Not In (:d1)) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("carol")]);
}

/// The W3C §17.4.1.9/10 examples, as projected values (unbound = error).
#[test]
fn in_follows_the_spec_examples_including_errors() {
    let mut db = db();
    let cases = [
        ("2 IN (1, 2, 3)", "true"),
        ("2 IN ()", "false"),
        ("2 IN (<http://example/iri>, \"str\", 2.0)", "true"),
        ("2 IN (1/0, 2)", "true"),
        ("2 IN (2, 1/0)", "true"),
        ("2 IN (3, 1/0)", ""),
        ("2 NOT IN (1, 2, 3)", "false"),
        ("2 NOT IN ()", "true"),
        ("2 NOT IN (<http://example/iri>, \"str\", 2.0)", "false"),
        ("2 NOT IN (1/0, 2)", "false"),
        ("2 NOT IN (2, 1/0)", "false"),
        ("2 NOT IN (3, 1/0)", ""),
        // RDFterm-equal: an IRI and a literal are simply different terms…
        ("<http://ex/a> IN (\"a\", <http://ex/b>)", "false"),
        ("<http://ex/a> NOT IN (\"a\")", "true"),
        // …two incomparable literals are a type error.
        ("2 IN (\"2\")", ""),
        ("\"a\" NOT IN (1)", ""),
        // An error in the tested term is an error (but `IN ()` is false).
        ("?unbound IN (1)", ""),
        ("?unbound IN ()", "false"),
    ];
    for (expr, want) in cases {
        let q = format!("SELECT ?v WHERE {{ BIND(({expr}) AS ?v) }}");
        let got = ordered(&mut db, &q);
        let want = if want.is_empty() {
            String::new()
        } else {
            format!("\"{want}\"^^<{XSD}boolean>")
        };
        assert_eq!(got, vec![vec![want]], "{expr}");
    }
}

#[test]
fn in_works_in_having_and_projection_and_binds_tighter_than_and() {
    let mut db = db();
    let q = format!(
        "{P} SELECT ?d (COUNT(?p) AS ?n) WHERE {{ ?p :dept ?d }} GROUP BY ?d \
         HAVING (COUNT(?p) IN (2, 3))"
    );
    assert_eq!(rows(&mut db, &q), vec![vec![ex("d1"), int("2")]]);
    // `a IN (…) && b` parses as `(a IN (…)) && b`.
    let q = format!(
        "{P} SELECT ?p WHERE {{ ?p :age ?a FILTER(?a IN (25, 30) && ?a > 26) }}"
    );
    assert_eq!(col0(&mut db, &q), vec![ex("alice")]);
    match parse("SELECT * WHERE { ?s ?p ?o FILTER(?o NOT IN (1)) }").unwrap() {
        Query::Select(s) => match &s.pattern {
            GraphPattern::Filter(fs, _) => {
                assert!(matches!(&fs[0], Expr::In { negated: true, list, .. } if list.len() == 1))
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn in_syntax_errors_are_explained() {
    let m = parse_err("SELECT * WHERE { ?s ?p ?o FILTER(?o IN 1) }");
    assert_contains(&m, &["IN expects a parenthesized list", "found 1"]);
    let m = parse_err("SELECT * WHERE { ?s ?p ?o FILTER(?o NOT 1) }");
    assert_contains(&m, &["expected IN after NOT"]);
    let m = parse_err("SELECT * WHERE { ?s ?p ?o FILTER(?o IN (1 2)) }");
    assert_contains(&m, &["expected ',' or ')' in the IN list", "found 2"]);
}

// ---------------------------------------------------------------------------
// 2. Signed numeric literals and booleans as terms
// ---------------------------------------------------------------------------

#[test]
fn signed_numbers_and_booleans_match_in_triple_patterns() {
    let mut db = db();
    assert_eq!(col0(&mut db, &format!("{P} SELECT ?p WHERE {{ ?p :temp -5 }}")), vec![ex("alice")]);
    assert_eq!(
        col0(&mut db, &format!("{P} SELECT ?p WHERE {{ ?p :temp -1.5e3 }}")),
        vec![ex("carol")]
    );
    assert_eq!(col0(&mut db, &format!("{P} SELECT ?p WHERE {{ ?p :ok true }}")), vec![ex("alice")]);
    assert_eq!(col0(&mut db, &format!("{P} SELECT ?p WHERE {{ ?p :ok FALSE }}")), vec![ex("bob")]);
    // A term in subject position too (no match, but it parses and evaluates).
    assert!(col0(&mut db, &format!("{P} SELECT ?p WHERE {{ -5 ?p ?o }}")).is_empty());
    // Graph matching is by term: `+7` is not the stored `7`.
    assert!(col0(&mut db, &format!("{P} SELECT ?p WHERE {{ ?p :temp +7 }}")).is_empty());
    assert_eq!(col0(&mut db, &format!("{P} SELECT ?p WHERE {{ ?p :temp 7 }}")), vec![ex("bob")]);
    match parse("SELECT * WHERE { ?s ?p -1.5e3, +2, -.5, true }").unwrap() {
        Query::Select(s) => match &s.pattern {
            GraphPattern::Bgp(tps) => {
                let objs: Vec<String> = tps.iter().map(|t| format!("{:?}", t.object)).collect();
                assert!(objs[0].contains("\"-1.5e3\"") && objs[0].contains("double"), "{objs:?}");
                assert!(objs[1].contains("\"+2\"") && objs[1].contains("integer"), "{objs:?}");
                assert!(objs[2].contains("\"-.5\"") && objs[2].contains("decimal"), "{objs:?}");
                assert!(objs[3].contains("\"true\"") && objs[3].contains("boolean"), "{objs:?}");
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn signed_numbers_and_booleans_in_values_blocks() {
    let mut db = db();
    let q = format!("{P} SELECT ?p WHERE {{ VALUES ?t {{ -5 7 }} ?p :temp ?t }}");
    assert_eq!(col0(&mut db, &q), vec![ex("alice"), ex("bob")]);
    let q = format!("{P} SELECT ?p ?o WHERE {{ VALUES (?o ?t) {{ (true -5) (false +9) }} ?p :ok ?o ; :temp ?t }}");
    assert_eq!(
        rows(&mut db, &q),
        vec![vec![ex("alice"), format!("\"true\"^^<{XSD}boolean>")]]
    );
    // Trailing VALUES too.
    let q = format!("{P} SELECT ?p WHERE {{ ?p :temp ?t }} VALUES ?t {{ -1.5e3 }}");
    assert_eq!(col0(&mut db, &q), vec![ex("carol")]);
}

#[test]
fn signed_numbers_and_booleans_in_data_blocks() {
    let mut db = db();
    let n = update(&mut db, &format!("{P} INSERT DATA {{ :dave :temp -40 ; :ok true ; :d -0.5 }}"));
    assert_eq!(n, 3);
    assert_eq!(col0(&mut db, &format!("{P} SELECT ?p WHERE {{ ?p :temp -40 }}")), vec![ex("dave")]);
    let got = rows(&mut db, &format!("{P} SELECT ?o WHERE {{ :dave :d ?o }}"));
    assert_eq!(got, vec![vec![format!("\"-0.5\"^^<{XSD}decimal>")]]);
    // Arithmetic sees the sign.
    let q = format!("{P} SELECT ?p WHERE {{ ?p :temp ?t FILTER(?t < -10) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("carol"), ex("dave")]);
    let n = update(&mut db, &format!("{P} DELETE DATA {{ :dave :temp -40 . :dave :ok true }}"));
    assert_eq!(n, 2);
    assert!(col0(&mut db, &format!("{P} SELECT ?p WHERE {{ ?p :temp -40 }}")).is_empty());
}

#[test]
fn signed_number_errors() {
    let m = parse_err("SELECT * WHERE { ?s ?p - 5 }");
    assert_contains(&m, &["signed number", "without a space", "-5"]);
    let m = parse_err("SELECT * WHERE { ?s ?p -?x }");
    assert_contains(&m, &["expected a number after '-'", "?x"]);
    let m = parse_err("SELECT * WHERE { VALUES ?x { ?y } }");
    assert_contains(&m, &["VALUES cell", "?y"]);
}

// ---------------------------------------------------------------------------
// 3. CONTAINALL / CONTAINANY (gStore extension)
// ---------------------------------------------------------------------------

fn list_db() -> Database {
    Database::build_from_str(
        "lists",
        r#"
@prefix : <http://ex/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
:a :tags "red,green,blue"^^xsd:list ; :need "red, blue"^^xsd:list ; :one "green" .
:b :tags "red;yellow"^^xsd:list ; :need "red,green"^^xsd:list ; :one "black" .
:c :tags "x"^^xsd:string ; :need ""^^xsd:list ; :one "x" .
"#,
    )
    .expect("build")
}

#[test]
fn containall_and_containany_follow_upstream() {
    let mut db = list_db();
    // Every needed item is among the tags (items are trimmed of spaces).
    let q = format!("{P} SELECT ?s WHERE {{ ?s :tags ?t ; :need ?n FILTER(CONTAINALL(?n, ?t)) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("a")]);
    // `:c`'s tags are not an xsd:list: an error, so the row is dropped even
    // though its needed list is empty.
    // A single (non-list) first argument is one item.
    let q = format!("{P} SELECT ?s WHERE {{ ?s :tags ?t ; :one ?o FILTER(CONTAINALL(?o, ?t)) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("a")]);
    // CONTAINANY: some item of the list (2nd argument) is among the items.
    let q = format!("{P} SELECT ?s WHERE {{ ?s :tags ?t ; :need ?n FILTER(CONTAINANY(?n, ?t)) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("a")]);
    // A custom separator (third argument).
    let q = format!(
        "{P} SELECT ?s WHERE {{ ?s :tags ?t FILTER(CONTAINANY(\"yellow\", ?t, \";\")) }}"
    );
    assert_eq!(col0(&mut db, &q), vec![ex("b")]);
    let q = format!("{P} SELECT ?s WHERE {{ ?s :tags ?t FILTER(containall(\"red\", ?t)) }}");
    assert_eq!(col0(&mut db, &q), vec![ex("a")], "',' does not split :b's ';' list");
    // An IRI item is its IRI string; an empty list contains nothing.
    let cases = [
        ("CONTAINALL(<http://x>, \"http://x,y\"^^xsd:list)", "true"),
        ("CONTAINANY(\"a,b\"^^xsd:list, \"c, b\"^^xsd:list)", "true"),
        ("CONTAINANY(\"a,b\"^^xsd:list, \"\"^^xsd:list)", "false"),
        ("CONTAINALL(\"\"^^xsd:list, \"\"^^xsd:list)", "true"),
        ("CONTAINALL(5, \"5,6\"^^xsd:list)", "true"),
        ("CONTAINALL(\"a\", \"a\")", ""),
        ("CONTAINALL(\"a\", \"a|b\"^^xsd:list, 1)", ""),
        ("CONTAINALL(\"a\", \"a||b\"^^xsd:list, \"||\")", "true"),
    ];
    for (expr, want) in cases {
        let q = format!("{P} SELECT ?v WHERE {{ BIND({expr} AS ?v) }}");
        let want = if want.is_empty() {
            String::new()
        } else {
            format!("\"{want}\"^^<{XSD}boolean>")
        };
        assert_eq!(ordered(&mut db, &q), vec![vec![want]], "{expr}");
    }
}

#[test]
fn containall_arity_is_checked() {
    let m = parse_err("SELECT * WHERE { ?s ?p ?o FILTER(CONTAINALL(?o)) }");
    assert_contains(&m, &["CONTAINALL expects 2 to 3 arguments, got 1"]);
    let m = parse_err("SELECT * WHERE { ?s ?p ?o FILTER(CONTAINANY(?o, ?o, \",\", ?o)) }");
    assert_contains(&m, &["CONTAINANY expects 2 to 3 arguments, got 4"]);
}

// ---------------------------------------------------------------------------
// 4. ORDER BY with bare built-in / function-call keys
// ---------------------------------------------------------------------------

#[test]
fn order_by_bare_builtin_and_function_calls() {
    let mut db = db();
    let q = format!("{P} SELECT ?n WHERE {{ ?p :name ?n }} ORDER BY STRLEN(?n) DESC(?n)");
    assert_eq!(
        ordered(&mut db, &q),
        vec![vec!["\"Bob\"".to_string()], vec!["\"Carol\"".into()], vec!["\"Alice\"".into()]]
    );
    let q = format!(
        "{P} SELECT ?p WHERE {{ ?p :age ?a ; :name ?n }} ORDER BY xsd:string(?a) STRLEN(?n)"
    );
    assert_eq!(
        ordered(&mut db, &q),
        vec![vec![ex("bob")], vec![ex("alice")], vec![ex("carol")]]
    );
    // Keys of every kind in one clause, then LIMIT.
    let q = format!(
        "{P} SELECT ?p WHERE {{ ?p :age ?a ; :temp ?t }} ORDER BY ABS(?t) ?p DESC(?a) LIMIT 2"
    );
    assert_eq!(ordered(&mut db, &q), vec![vec![ex("alice")], vec![ex("bob")]]);
    // A bare aggregate as the key of a grouped query.
    let q = format!(
        "{P} SELECT ?d WHERE {{ ?p :dept ?d }} GROUP BY ?d ORDER BY DESC(COUNT(?p))"
    );
    assert_eq!(ordered(&mut db, &q), vec![vec![ex("d1")], vec![ex("d2")]]);
}

#[test]
fn order_by_errors() {
    let m = parse_err("SELECT * WHERE { ?s ?p ?o } ORDER BY LIMIT 1");
    assert_contains(&m, &["ORDER BY needs at least one condition", "'LIMIT'"]);
    let m = parse_err("SELECT * WHERE { ?s ?p ?o } ORDER BY DESC STRLEN(?o)");
    assert_contains(&m, &["DESC expects a bracketted expression", "'STRLEN'"]);
}

// ---------------------------------------------------------------------------
// 5. CONSTRUCT WHERE short form
// ---------------------------------------------------------------------------

#[test]
fn construct_where_short_form_returns_the_matched_triples() {
    let mut db = db();
    let got = construct(&mut db, &format!("{P} CONSTRUCT WHERE {{ ?p :knows ?k . ?k :age ?a }}"));
    let want = vec![
        format!("{} {} {} .", ex("alice"), ex("knows"), ex("bob")),
        format!("{} {} {} .", ex("bob"), ex("age"), int("25")),
        format!("{} {} {} .", ex("bob"), ex("knows"), ex("carol")),
        format!("{} {} {} .", ex("carol"), ex("age"), int("35")),
    ];
    assert_eq!(got, want);
    // Solution modifiers and a blank node property list, whose hidden
    // variable is bound to the matched node.
    let got = construct(
        &mut db,
        &format!("{P} CONSTRUCT WHERE {{ :alice :knows [ :name ?n ] }} LIMIT 5"),
    );
    assert_eq!(
        got,
        vec![
            format!("{} {} {} .", ex("alice"), ex("knows"), ex("bob")),
            format!("{} {} \"Bob\" .", ex("bob"), ex("name")),
        ]
    );
    // Empty short form: no triples.
    assert!(construct(&mut db, "CONSTRUCT WHERE { }").is_empty());
}

#[test]
fn construct_where_short_form_rejects_non_bgp_patterns() {
    let m = parse_err("CONSTRUCT WHERE { ?s ?p ?o FILTER(?o > 1) }");
    assert_contains(&m, &["CONSTRUCT WHERE", "only triple patterns", "FILTER", "CONSTRUCT { template }"]);
    let m = parse_err("CONSTRUCT WHERE { ?s ?p ?o OPTIONAL { ?o ?q ?r } }");
    assert_contains(&m, &["only triple patterns", "OPTIONAL"]);
    let m = parse_err("CONSTRUCT WHERE { { ?s ?p ?o } }");
    assert_contains(&m, &["nested group"]);
    let m = parse_err("CONSTRUCT WHERE { ?s <p>/<q> ?o }");
    assert_contains(&m, &["property paths are not allowed", "CONSTRUCT WHERE"]);
    let m = parse_err("CONSTRUCT ?s WHERE { ?s ?p ?o }");
    assert_contains(&m, &["expected a CONSTRUCT template", "short form"]);
}

// ---------------------------------------------------------------------------
// 6. HAVING with several constraints
// ---------------------------------------------------------------------------

#[test]
fn having_with_several_constraints_requires_all() {
    let mut db = db();
    let base = format!("{P} SELECT ?d (SUM(?s) AS ?t) WHERE {{ ?p :dept ?d ; :salary ?s }} GROUP BY ?d");
    // d1: 2 people, 300; d2: 1 person, 50.
    let q = format!("{base} HAVING (COUNT(?p) > 1) (SUM(?s) < 1000)");
    assert_eq!(rows(&mut db, &q), vec![vec![ex("d1"), int("300")]]);
    let q = format!("{base} HAVING (COUNT(?p) > 1) (SUM(?s) < 100)");
    assert!(rows(&mut db, &q).is_empty());
    let q = format!("{base} HAVING (COUNT(?p) >= 1) (SUM(?s) < 100)");
    assert_eq!(rows(&mut db, &q), vec![vec![ex("d2"), int("50")]]);
    // Bare calls mix with bracketted constraints; HAVING may repeat.
    let q = format!("{base} HAVING COUNT(?p) (COUNT(?p) > 1) HAVING (SUM(?s) > 0) ORDER BY ?d");
    assert_eq!(rows(&mut db, &q), vec![vec![ex("d1"), int("300")]]);
    match parse(&format!("{base} HAVING BOUND(?d) rs:WCC(?d) (SUM(?s) > 0)")).unwrap() {
        Query::Select(s) => assert_eq!(s.having.len(), 3, "{:?}", s.having),
        other => panic!("{other:?}"),
    }
    // Followed by a trailing VALUES with a variable list.
    let q = format!("{base} HAVING (COUNT(?p) > 0) (SUM(?s) > 0) VALUES (?d) {{ (:d2) }}");
    assert_eq!(rows(&mut db, &q), vec![vec![ex("d2"), int("50")]]);
    let m = parse_err("SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o } HAVING LIMIT 1");
    assert_contains(&m, &["HAVING needs a constraint"]);
}

// ---------------------------------------------------------------------------
// 7. Trailing VALUES
// ---------------------------------------------------------------------------

#[test]
fn trailing_values_joins_select_results() {
    let mut db = db();
    let q = format!("{P} SELECT ?p ?a WHERE {{ ?p :age ?a }} VALUES ?p {{ :alice :carol :nobody }}");
    assert_eq!(rows(&mut db, &q), vec![vec![ex("alice"), int("30")], vec![ex("carol"), int("35")]]);
    // UNDEF matches anything; ORDER BY / LIMIT apply after the join.
    let q = format!(
        "{P} SELECT ?p WHERE {{ ?p :age ?a }} ORDER BY DESC(?a) LIMIT 2 \
         VALUES (?p ?a) {{ (:alice UNDEF) (UNDEF 25) (:bob 30) }}"
    );
    assert_eq!(ordered(&mut db, &q), vec![vec![ex("alice")], vec![ex("bob")]]);
    // SELECT * includes the VALUES-only variables.
    let q = format!("{P} SELECT * WHERE {{ ?p :age 25 }} VALUES ?x {{ 1 2 }}");
    let rs = db.select(&q).unwrap();
    assert_eq!(rs.vars, vec!["p".to_string(), "x".to_string()]);
    assert_eq!(rs.row_count(), 2);
    // An empty WHERE with VALUES is the table itself; `()` rows / variable lists.
    let q = "SELECT ?x WHERE { } VALUES ?x { 1 2 3 }";
    assert_eq!(rows(&mut db, q).len(), 3);
    let q = "SELECT * WHERE { } VALUES () { () () }";
    assert_eq!(db.select(q).unwrap().row_count(), 2);
}

#[test]
fn trailing_values_after_grouping_joins_the_groups() {
    let mut db = db();
    let q = format!(
        "{P} SELECT ?d (COUNT(?p) AS ?n) WHERE {{ ?p :dept ?d }} GROUP BY ?d VALUES ?d {{ :d2 }}"
    );
    assert_eq!(rows(&mut db, &q), vec![vec![ex("d2"), int("1")]]);
    // Joins on aggregate aliases too, and before ORDER BY.
    let q = format!(
        "{P} SELECT ?d (COUNT(?p) AS ?n) WHERE {{ ?p :dept ?d }} GROUP BY ?d ORDER BY ?d \
         VALUES ?n {{ 1 2 }}"
    );
    assert_eq!(
        ordered(&mut db, &q),
        vec![vec![ex("d1"), int("2")], vec![ex("d2"), int("1")]]
    );
    // A VALUES variable outside the result multiplies the rows, as the join
    // before the projection does.
    let q = format!("{P} SELECT (COUNT(*) AS ?c) WHERE {{ ?p :dept ?d }} VALUES ?z {{ 1 2 }}");
    assert_eq!(rows(&mut db, &q), vec![vec![int("3")], vec![int("3")]]);
}

#[test]
fn trailing_values_on_ask_construct_describe_and_subselect() {
    let mut db = db();
    assert!(ask(&mut db, &format!("{P} ASK {{ ?p :age ?a }} VALUES ?a {{ 25 }}")));
    assert!(!ask(&mut db, &format!("{P} ASK {{ ?p :age ?a }} VALUES ?a {{ 26 }}")));
    let got = construct(
        &mut db,
        &format!("{P} CONSTRUCT {{ ?p :young true }} WHERE {{ ?p :age ?a }} VALUES ?a {{ 25 }}"),
    );
    assert_eq!(got, vec![format!("{} {} \"true\"^^<{XSD}boolean> .", ex("bob"), ex("young"))]);
    let got = construct(&mut db, &format!("{P} DESCRIBE ?p VALUES ?p {{ :carol }}"));
    assert_eq!(got.len(), 5, "{got:?}");
    // A sub-SELECT's own VALUES clause.
    let q = format!(
        "{P} SELECT ?p ?n WHERE {{ {{ SELECT ?p WHERE {{ ?p :age ?a }} VALUES ?p {{ :bob }} }} \
         ?p :name ?n }}"
    );
    assert_eq!(rows(&mut db, &q), vec![vec![ex("bob"), "\"Bob\"".into()]]);
}

#[test]
fn values_errors() {
    let m = parse_err("SELECT * WHERE { VALUES (?a ?b) { (1) } }");
    assert_contains(&m, &["VALUES row 1 has 1 value(s) but 2 variable(s)"]);
    let m = parse_err("SELECT * WHERE { ?s ?p ?o } VALUES ?s { 1 ");
    assert_contains(&m, &["unterminated VALUES block"]);
    let m = parse_err("SELECT * WHERE { ?s ?p ?o } VALUES (?s ?s) { }");
    assert_contains(&m, &["declares ?s twice"]);
    let m = parse_err("SELECT * WHERE { ?s ?p ?o } VALUES ?s { _:b }");
    assert_contains(&m, &["not the blank node _:b"]);
    // VALUES ends the query.
    let m = parse_err("SELECT * WHERE { ?s ?p ?o } VALUES ?s { 1 } LIMIT 1");
    assert_contains(&m, &["end of query"]);
}

// ---------------------------------------------------------------------------
// 8. Blank node property lists and collections
// ---------------------------------------------------------------------------

#[test]
fn blank_node_property_lists_in_patterns() {
    let mut db = db();
    // Object position: someone who knows a person aged 25.
    let q = format!("{P} SELECT ?p WHERE {{ ?p :knows [ :age 25 ] }}");
    assert_eq!(col0(&mut db, &q), vec![ex("alice")]);
    // Subject position with `;` and `,`, plus a trailing property list.
    let q = format!("{P} SELECT ?n WHERE {{ [ :dept :d1 ; :name ?n ] :knows ?k . }}");
    assert_eq!(col0(&mut db, &q), vec!["\"Alice\"".to_string(), "\"Bob\"".into()]);
    // Nested, and a property list as the whole statement.
    let q = format!("{P} SELECT ?n WHERE {{ [ :knows [ :knows [ :name ?n ] ] ] . }}");
    assert_eq!(col0(&mut db, &q), vec!["\"Carol\"".to_string()]);
    // `[]` alone is an anonymous node; SELECT * hides blank-node variables.
    let q = format!("{P} SELECT * WHERE {{ ?p :knows [] ; :age ?a }}");
    let rs = db.select(&q).unwrap();
    assert_eq!(rs.vars, vec!["p".to_string(), "a".to_string()]);
    assert_eq!(rs.row_count(), 2);
    // Property paths inside a blank node property list.
    let q = format!("{P} SELECT ?p WHERE {{ ?p :knows [ :knows/:name \"Carol\" ] }}");
    assert_eq!(col0(&mut db, &q), vec![ex("alice")]);
}

fn list_data_db() -> Database {
    let mut db = Database::build_from_str(
        "coll",
        "@prefix : <http://ex/> . :x :p ( 1 2 :z ) . :y :p () .",
    )
    .expect("build");
    let _ = &mut db;
    db
}

#[test]
fn collections_in_patterns() {
    let mut db = list_data_db();
    // A collection pattern matches the rdf:first/rdf:rest chain.
    let q = format!("{P} SELECT ?s ?last WHERE {{ ?s :p ( 1 2 ?last ) }}");
    assert_eq!(rows(&mut db, &q), vec![vec![ex("x"), ex("z")]]);
    assert!(col0(&mut db, &format!("{P} SELECT ?s WHERE {{ ?s :p ( 1 ?b ) }}")).is_empty());
    // `()` is rdf:nil.
    assert_eq!(col0(&mut db, &format!("{P} SELECT ?s WHERE {{ ?s :p () }}")), vec![ex("y")]);
    // A collection as the subject, standing alone.
    assert!(ask(&mut db, &format!("{P} ASK {{ ( 1 2 :z ) . }}")));
    assert!(!ask(&mut db, &format!("{P} ASK {{ ( 2 1 :z ) . }}")));
    // Members may themselves be triples nodes.
    assert!(ask(&mut db, &format!("{P} ASK {{ ?s :p ( 1 [] :z ) }}")));
    match parse("SELECT * WHERE { ?s <http://p> (1 ?x) }").unwrap() {
        Query::Select(s) => {
            let GraphPattern::Bgp(tps) = &s.pattern else {
                panic!("{:?}", s.pattern)
            };
            // first/rest twice + the linking triple.
            assert_eq!(tps.len(), 5, "{tps:?}");
            assert_eq!(s.result_vars(), vec!["s".to_string(), "x".to_string()]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn blank_nodes_and_collections_in_insert_data_are_fresh() {
    let mut db = db();
    let n = update(
        &mut db,
        &format!("{P} INSERT DATA {{ :e :addr [ :city \"Paris\" ; :zip 75001 ] ; :list ( 1 [ :v 2 ] ) }}"),
    );
    // addr + city + zip, list + 2×(first, rest) + v.
    assert_eq!(n, 9);
    let q = format!("{P} SELECT ?c WHERE {{ :e :addr [ :city ?c ] }}");
    assert_eq!(col0(&mut db, &q), vec!["\"Paris\"".to_string()]);
    assert!(ask(&mut db, &format!("{P} ASK {{ :e :list ( 1 [ :v 2 ] ) }}")));
    // Each INSERT DATA mints new blank nodes: the same text inserts again.
    let n = update(&mut db, &format!("{P} INSERT DATA {{ :e :addr [ :city \"Paris\" ] }}"));
    assert_eq!(n, 2);
    assert_eq!(rows(&mut db, &format!("{P} SELECT ?b WHERE {{ :e :addr ?b }}")).len(), 2);
}

#[test]
fn blank_nodes_in_templates_are_fresh_per_solution() {
    let mut db = db();
    let got = construct(
        &mut db,
        &format!("{P} CONSTRUCT {{ ?p :card [ :label ?n ] }} WHERE {{ ?p :name ?n }}"),
    );
    assert_eq!(got.len(), 6, "{got:?}");
    let cards: std::collections::HashSet<&str> = got
        .iter()
        .filter(|t| t.contains("/card>"))
        .map(|t| t.rsplit(' ').nth(1).unwrap())
        .collect();
    assert_eq!(cards.len(), 3, "one fresh node per solution: {got:?}");
    // INSERT … WHERE with a template node and collection.
    let n = update(
        &mut db,
        &format!("{P} INSERT {{ ?p :tags ( ?n ) }} WHERE {{ ?p :age ?a ; :name ?n FILTER(?a > 26) }}"),
    );
    assert_eq!(n, 6);
    let q = format!("{P} SELECT ?p ?n WHERE {{ ?p :tags ( ?n ) }}");
    assert_eq!(
        rows(&mut db, &q),
        vec![vec![ex("alice"), "\"Alice\"".into()], vec![ex("carol"), "\"Carol\"".into()]]
    );
}

#[test]
fn blank_node_syntax_is_rejected_where_update_forbids_it() {
    let m = parse_err("DELETE DATA { <a> <p> [ <q> 1 ] }");
    assert_contains(&m, &["blank nodes are not allowed in DELETE DATA", "[ … ]"]);
    let m = parse_err("DELETE WHERE { ?s <p> ( 1 ) }");
    assert_contains(&m, &["not allowed in DELETE WHERE", "collection"]);
    let m = parse_err("DELETE { ?s <p> [] } WHERE { ?s ?p ?o }");
    assert_contains(&m, &["not allowed in a DELETE template", "[]"]);
    // `()` is rdf:nil, a plain IRI.
    assert!(parse("DELETE DATA { <a> <p> () }").is_ok());
    // Malformed triples nodes.
    let m = parse_err("SELECT * WHERE { ?s <p> [ <q> ?o ; }");
    assert_contains(&m, &["']' closing the blank node property list"]);
    let m = parse_err("SELECT * WHERE { ?s <p> [ ?o ] }");
    assert_contains(&m, &["expected an RDF term", "found ']'"]);
    let m = parse_err("SELECT * WHERE { ?s <p> [ \"x\" ] }");
    assert_contains(&m, &["expected a predicate in the blank node property list"]);
    let m = parse_err("SELECT * WHERE { ?s <p> ( 1 2 }");
    assert_contains(&m, &["expected an RDF term", "'}'"]);
    let deep = format!("SELECT * WHERE {{ ?s <p> {}1{} }}", "( ".repeat(300), " )".repeat(300));
    assert_contains(&parse_err(&deep), &["nested more than 256 deep"]);
}

// ---------------------------------------------------------------------------
// 9. ADD / MOVE / COPY
// ---------------------------------------------------------------------------

fn graph_db() -> Database {
    let mut db = Database::build_from_str("graphs", "<http://ex/s> <http://ex/p> <http://ex/o0> .")
        .expect("build");
    update(
        &mut db,
        "INSERT DATA { GRAPH <http://ex/g1> { <http://ex/s> <http://ex/p> <http://ex/o1> . \
         <http://ex/s> <http://ex/p> <http://ex/o2> } \
         GRAPH <http://ex/g2> { <http://ex/s> <http://ex/p> <http://ex/old> } }",
    );
    db
}

/// The objects of `<s> <p> ?o` in `graph` (`None` = default), sorted.
fn objects(db: &mut Database, graph: Option<&str>) -> Vec<String> {
    let q = match graph {
        None => "SELECT ?o WHERE { <http://ex/s> <http://ex/p> ?o }".to_string(),
        Some(g) => format!("SELECT ?o WHERE {{ GRAPH <{g}> {{ <http://ex/s> <http://ex/p> ?o }} }}"),
    };
    col0(db, &q)
}

#[test]
fn add_inserts_and_keeps_both_graphs() {
    let mut db = graph_db();
    assert_eq!(update(&mut db, "ADD <http://ex/g1> TO <http://ex/g2>"), 2);
    assert_eq!(objects(&mut db, Some("http://ex/g2")), vec![ex("o1"), ex("o2"), ex("old")]);
    assert_eq!(objects(&mut db, Some("http://ex/g1")), vec![ex("o1"), ex("o2")]);
    // GRAPH keyword and DEFAULT on either side; already-present triples don't count.
    assert_eq!(update(&mut db, "ADD GRAPH <http://ex/g1> TO DEFAULT"), 2);
    assert_eq!(update(&mut db, "ADD GRAPH <http://ex/g1> TO DEFAULT"), 0);
    assert_eq!(objects(&mut db, None), vec![ex("o0"), ex("o1"), ex("o2")]);
    // Into a new graph.
    assert_eq!(update(&mut db, "ADD DEFAULT TO <http://ex/g3>"), 3);
    assert_eq!(objects(&mut db, Some("http://ex/g3")), vec![ex("o0"), ex("o1"), ex("o2")]);
}

#[test]
fn copy_replaces_the_destination() {
    let mut db = graph_db();
    // Clears g2's one triple, adds g1's two.
    assert_eq!(update(&mut db, "COPY <http://ex/g1> TO <http://ex/g2>"), 3);
    assert_eq!(objects(&mut db, Some("http://ex/g2")), vec![ex("o1"), ex("o2")]);
    assert_eq!(objects(&mut db, Some("http://ex/g1")), vec![ex("o1"), ex("o2")]);
    assert_eq!(update(&mut db, "COPY DEFAULT TO GRAPH <http://ex/g1>"), 3);
    assert_eq!(objects(&mut db, Some("http://ex/g1")), vec![ex("o0")]);
    assert_eq!(update(&mut db, "COPY <http://ex/g2> TO DEFAULT"), 3);
    assert_eq!(objects(&mut db, None), vec![ex("o1"), ex("o2")]);
}

#[test]
fn move_replaces_the_destination_and_drops_the_source() {
    let mut db = graph_db();
    assert_eq!(update(&mut db, "MOVE <http://ex/g1> TO <http://ex/g2>"), 5);
    assert_eq!(objects(&mut db, Some("http://ex/g2")), vec![ex("o1"), ex("o2")]);
    assert!(objects(&mut db, Some("http://ex/g1")).is_empty());
    assert_eq!(update(&mut db, "MOVE DEFAULT TO <http://ex/g1>"), 2);
    assert!(objects(&mut db, None).is_empty());
    assert_eq!(objects(&mut db, Some("http://ex/g1")), vec![ex("o0")]);
    // Inside an update sequence.
    let n = update(&mut db, "MOVE <http://ex/g1> TO DEFAULT ; COPY DEFAULT TO <http://ex/g4>");
    assert_eq!(n, 3);
    assert_eq!(objects(&mut db, Some("http://ex/g4")), vec![ex("o0")]);
}

#[test]
fn same_graph_transfers_are_no_ops() {
    let mut db = graph_db();
    for op in ["ADD", "MOVE", "COPY"] {
        assert_eq!(update(&mut db, &format!("{op} <http://ex/g1> TO GRAPH <http://ex/g1>")), 0);
        assert_eq!(update(&mut db, &format!("{op} DEFAULT TO DEFAULT")), 0);
        // Even for a graph that does not exist.
        assert_eq!(update(&mut db, &format!("{op} <http://ex/none> TO <http://ex/none>")), 0);
    }
    assert_eq!(objects(&mut db, Some("http://ex/g1")), vec![ex("o1"), ex("o2")]);
    assert_eq!(objects(&mut db, None), vec![ex("o0")]);
}

#[test]
fn missing_source_graph_fails_unless_silent() {
    let mut db = graph_db();
    for op in ["ADD", "MOVE", "COPY"] {
        let e = db.query(&format!("{op} <http://ex/none> TO <http://ex/g2>")).unwrap_err();
        let m = e.to_string();
        assert_contains(&m, &[op, "source graph <http://ex/none> does not exist", "SILENT"]);
        assert_eq!(update(&mut db, &format!("{op} SILENT <http://ex/none> TO <http://ex/g2>")), 0);
        // The destination was not touched.
        assert_eq!(objects(&mut db, Some("http://ex/g2")), vec![ex("old")]);
    }
    // An empty default graph exists: COPY from it clears the destination.
    update(&mut db, "CLEAR DEFAULT");
    assert_eq!(update(&mut db, "COPY DEFAULT TO <http://ex/g2>"), 1);
    assert!(objects(&mut db, Some("http://ex/g2")).is_empty());
}

#[test]
fn add_move_copy_parse_and_errors() {
    match parse("PREFIX ex: <http://ex/> COPY SILENT GRAPH ex:a TO DEFAULT").unwrap() {
        Query::Update(ops) => assert_eq!(
            ops,
            vec![UpdateOp::Copy {
                from: Some("http://ex/a".into()),
                to: None,
                silent: true
            }]
        ),
        other => panic!("{other:?}"),
    }
    let m = parse_err("ADD <a> <b>");
    assert_contains(&m, &["expected TO in ADD"]);
    let m = parse_err("MOVE ALL TO DEFAULT");
    assert_contains(&m, &["MOVE expects DEFAULT or a graph IRI", "'ALL'"]);
    let m = parse_err("COPY DEFAULT TO ?g");
    assert_contains(&m, &["COPY expects DEFAULT or a graph IRI", "?g"]);
}

// ---------------------------------------------------------------------------
// 10. PN_LOCAL escapes, percent-encoding, ':' and '.'
// ---------------------------------------------------------------------------

#[test]
fn prefixed_name_edge_cases() {
    let mut db = Database::build_from_str(
        "pn",
        "<http://ex/a-b> <http://ex/p> <http://ex/a:b> .\n\
         <http://ex/a~b.c> <http://ex/p> <http://ex/x%20y> .\n\
         <http://ex/s> <http://ex/p> <http://ex/o.> .\n\
         <http://ex/n> <http://ex/v2.p> <http://ex/o> .",
    )
    .expect("build");
    let pfx = "PREFIX ex: <http://ex/> PREFIX my-ns.v2: <http://ex/> ";
    // `\-` / `\~` / `\.` are unescaped; ':' belongs to the local name.
    let q = format!("{pfx} SELECT ?o WHERE {{ ex:a\\-b ex:p ?o }}");
    assert_eq!(col0(&mut db, &q), vec![ex("a:b")]);
    let q = format!("{pfx} SELECT ?s WHERE {{ ?s ex:p ex:a:b }}");
    assert_eq!(col0(&mut db, &q), vec![ex("a-b")]);
    let q = format!("{pfx} SELECT ?o WHERE {{ ex:a\\~b.c ex:p ?o }}");
    assert_eq!(col0(&mut db, &q), vec![ex("x%20y")]);
    // `%XX` stays encoded.
    let q = format!("{pfx} SELECT ?s WHERE {{ ?s ex:p ex:x%20y }}");
    assert_eq!(col0(&mut db, &q), vec![ex("a~b.c")]);
    // An unescaped trailing '.' ends the statement; an escaped one does not.
    let q = format!("{pfx} SELECT ?s WHERE {{ ?s ex:p ex:o\\. }}");
    assert_eq!(col0(&mut db, &q), vec![ex("s")]);
    let q = format!("{pfx} SELECT ?s WHERE {{ ?s ex:p ex:a:b.}}");
    assert_eq!(col0(&mut db, &q), vec![ex("a-b")]);
    // A prefix with '-' and '.' inside; a local name with '.' inside.
    let q = format!("{pfx} SELECT ?s WHERE {{ ?s my-ns.v2:v2.p my-ns.v2:o }}");
    assert_eq!(col0(&mut db, &q), vec![ex("n")]);
    // A blank node label's trailing '.' ends the statement too.
    assert!(parse("SELECT * WHERE { ?s ?p _:b.}").is_ok());
}

#[test]
fn prefixed_name_errors() {
    let m = parse_err("PREFIX ex: <http://ex/> SELECT * WHERE { ?s ex:a%2 ?o }");
    assert_contains(&m, &["invalid percent-encoding", "ex:a"]);
    let m = parse_err("PREFIX ex: <http://ex/> SELECT * WHERE { ?s ex:a\\b ?o }");
    assert_contains(&m, &["invalid escape '\\b'", "ex:a"]);
}

// ---------------------------------------------------------------------------
// GROUP BY conditions: (expr AS ?v) and bare calls
// ---------------------------------------------------------------------------

#[test]
fn group_by_expression_as_variable_binds_the_key() {
    let mut db = db();
    let q = format!(
        "{P} SELECT ?young (COUNT(?p) AS ?n) WHERE {{ ?p :age ?a }} GROUP BY ((?a < 32) AS ?young)"
    );
    assert_eq!(
        rows(&mut db, &q),
        vec![
            vec![format!("\"false\"^^<{XSD}boolean>"), int("1")],
            vec![format!("\"true\"^^<{XSD}boolean>"), int("2")],
        ]
    );
    // The bound key is usable in HAVING and ORDER BY.
    let q = format!(
        "{P} SELECT ?k WHERE {{ ?p :dept ?d }} GROUP BY (STR(?d) AS ?k) HAVING (COUNT(*) > 1) ORDER BY ?k"
    );
    assert_eq!(ordered(&mut db, &q), vec![vec!["\"http://ex/d1\"".to_string()]]);
    let m = parse_err("SELECT ?a WHERE { ?s ?p ?a } GROUP BY (STR(?s) AS ?a)");
    assert_contains(&m, &["?a is already bound by the WHERE clause"]);
}

#[test]
fn group_by_bare_builtin_call() {
    let mut db = db();
    let q = format!("{P} SELECT (COUNT(*) AS ?c) WHERE {{ ?p :dept ?d }} GROUP BY STR(?d)");
    assert_eq!(rows(&mut db, &q), vec![vec![int("1")], vec![int("2")]]);
    let m = parse_err("SELECT ?s WHERE { ?s ?p ?o } GROUP BY LIMIT 1");
    assert_contains(&m, &["GROUP BY needs at least one condition"]);
}
