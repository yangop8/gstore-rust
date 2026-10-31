//! Data test: SPARQL semantics — aggregates inside expressions (§18.2.4.1),
//! SPARQL Update over named graphs (quad templates, `WITH`, `USING`,
//! `USING NAMED`, `LOAD … INTO GRAPH`), typed-literal validation, the query
//! cache policy, the integer-division mode and XSD lexicals of special
//! floating-point values.

use gstore::query::IntegerDivision;
use gstore::sparql_results::{write_select, ResultFormat};
use gstore::{Database, LoadPolicy, QueryResult};

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// The N-Triples form of a typed literal, as result cells hold it.
fn lit(lexical: &str, local: &str) -> String {
    format!("\"{lexical}\"^^<{XSD}{local}>")
}

fn int(n: i64) -> String {
    lit(&n.to_string(), "integer")
}

fn plain(s: &str) -> String {
    format!("\"{s}\"")
}

fn ex(local: &str) -> String {
    format!("<http://ex/{local}>")
}

fn rows(db: &mut Database, q: &str) -> Vec<Vec<Option<String>>> {
    db.select(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

/// The first column of every row, unbound as `-`.
fn col(db: &mut Database, q: &str) -> Vec<String> {
    rows(db, q)
        .into_iter()
        .map(|r| r[0].clone().unwrap_or_else(|| "-".into()))
        .collect()
}

/// The first column, sorted.
fn sorted(db: &mut Database, q: &str) -> Vec<String> {
    let mut v = col(db, q);
    v.sort();
    v
}

/// The only row of a query, unbound cells as `None`.
fn row(db: &mut Database, q: &str) -> Vec<Option<String>> {
    let mut r = rows(db, q);
    assert_eq!(r.len(), 1, "{q}: {r:?}");
    r.remove(0)
}

fn update(db: &mut Database, u: &str) -> usize {
    match db.query(u).unwrap_or_else(|e| panic!("{u}: {e}")) {
        QueryResult::Update { changed } => changed,
        other => panic!("{u}: not an update result: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. aggregates wrapped in expressions
// ---------------------------------------------------------------------------

const STAFF: &str = r#"
@prefix : <http://ex/> .
:alice :dept :d1 ; :salary 2500 ; :name "Alice" .
:bob   :dept :d1 ; :salary 3000 ; :name "Bob" .
:carol :dept :d2 ; :salary 4000 ; :name "Carol" .
:dave  :dept :d3 .
:salary a :Property .
"#;

fn staff() -> Database {
    Database::build_from_str("staff", STAFF).expect("build")
}

const P: &str = "PREFIX : <http://ex/> ";

#[test]
fn builtins_wrapping_aggregates_in_select() {
    let mut db = staff();
    let r = row(
        &mut db,
        &format!(
            "{P}SELECT (ROUND(AVG(?s)) AS ?r) (STR(COUNT(*)) AS ?c) (SUM(?s)/COUNT(?s) AS ?avg) \
             (AVG(?s) AS ?a) WHERE {{ ?p :salary ?s }}"
        ),
    );
    // AVG over integers is a decimal (9500 / 3).
    let avg = lit("3166.666666666666666667", "decimal");
    assert_eq!(
        r,
        vec![
            Some(lit("3167.0", "decimal")),
            Some(plain("3")),
            Some(avg.clone()),
            Some(avg)
        ]
    );

    // Per group, with IF over an aggregate.
    let r = rows(
        &mut db,
        &format!(
            "{P}SELECT ?d (SUM(?s)/COUNT(?s) AS ?avg) (IF(COUNT(*) > 1, \"many\", \"one\") AS ?k) \
             WHERE {{ ?p :dept ?d ; :salary ?s }} GROUP BY ?d ORDER BY ?d"
        ),
    );
    assert_eq!(
        r,
        vec![
            vec![
                Some(ex("d1")),
                Some(lit("2750.0", "decimal")),
                Some(plain("many"))
            ],
            vec![
                Some(ex("d2")),
                Some(lit("4000.0", "decimal")),
                Some(plain("one"))
            ],
        ]
    );

    // COALESCE over an aggregate of no values (MAX of an empty set is an
    // error, so COALESCE falls through to 0).
    let r = rows(
        &mut db,
        &format!(
            "{P}SELECT ?d (COALESCE(MAX(?s), 0) AS ?m) \
             WHERE {{ ?p :dept ?d OPTIONAL {{ ?p :salary ?s }} }} GROUP BY ?d ORDER BY ?d"
        ),
    );
    let m: Vec<_> = r.iter().map(|r| r[1].clone().unwrap()).collect();
    assert_eq!(m, vec![int(3000), int(4000), int(0)]);

    // A later SELECT expression sees an earlier alias.
    let r = row(
        &mut db,
        &format!("{P}SELECT (COUNT(*) AS ?n) (?n * 2 AS ?m) WHERE {{ ?p :salary ?s }}"),
    );
    assert_eq!(r, vec![Some(int(3)), Some(int(6))]);

    // Nested built-ins around an aggregate over strings.
    let r = row(
        &mut db,
        &format!(
            "{P}SELECT (UCASE(GROUP_CONCAT(?n ; SEPARATOR=\",\")) AS ?g) \
             (STRLEN(MIN(?n)) AS ?l) WHERE {{ ?p :name ?n FILTER(?n != \"Bob\") }}"
        ),
    );
    let g = r[0].clone().unwrap();
    assert!(
        g == plain("ALICE,CAROL") || g == plain("CAROL,ALICE"),
        "{g}"
    );
    assert_eq!(r[1], Some(int(5)));
}

#[test]
fn aggregates_in_having_and_order_by() {
    let mut db = staff();
    let base = format!("{P}SELECT ?d WHERE {{ ?p :dept ?d ; :salary ?s }} GROUP BY ?d");
    // HAVING with arithmetic and builtins over aggregates.
    assert_eq!(
        col(
            &mut db,
            &format!("{base} HAVING (SUM(?s) / COUNT(?s) > 2800)")
        ),
        vec![ex("d2")]
    );
    assert_eq!(
        col(&mut db, &format!("{base} HAVING (STR(COUNT(*)) = \"2\")")),
        vec![ex("d1")]
    );
    // ORDER BY an aggregate that is not projected, and expressions of one.
    assert_eq!(
        col(&mut db, &format!("{base} ORDER BY DESC(SUM(?s))")),
        vec![ex("d1"), ex("d2")]
    );
    assert_eq!(
        col(&mut db, &format!("{base} ORDER BY (0 - COUNT(*))")),
        vec![ex("d1"), ex("d2")]
    );
    assert_eq!(
        col(&mut db, &format!("{base} ORDER BY (ABS(MIN(?s) - 3900))")),
        vec![ex("d2"), ex("d1")]
    );
    // ORDER BY an aggregate alias.
    assert_eq!(
        col(
            &mut db,
            &format!(
                "{P}SELECT ?d (SUM(?s) AS ?t) WHERE {{ ?p :dept ?d ; :salary ?s }} \
                 GROUP BY ?d ORDER BY ?t"
            )
        ),
        vec![ex("d2"), ex("d1")]
    );
    // HAVING without GROUP BY: one implicit group.
    let q = |h: &str| {
        format!("{P}SELECT (COUNT(*) AS ?n) WHERE {{ ?p :salary ?s }} HAVING (COUNT(*) > {h})")
    };
    assert_eq!(rows(&mut db, &q("100")).len(), 0);
    assert_eq!(col(&mut db, &q("1")), vec![int(3)]);
    // An aggregate only in ORDER BY also makes it an aggregate query.
    assert_eq!(
        col(
            &mut db,
            &format!("{P}SELECT (1 AS ?one) WHERE {{ ?p :salary ?s }} ORDER BY (COUNT(*))")
        ),
        vec![int(1)]
    );
}

#[test]
fn group_by_keys_by_term() {
    let mut db = staff();
    // ?p is a predicate variable whose IRI (:salary) is also a subject: the
    // group key must stay in the predicate id space.
    let r = rows(
        &mut db,
        &format!("{P}SELECT ?p (COUNT(*) AS ?c) WHERE {{ ?s ?p ?o }} GROUP BY ?p ORDER BY ?p"),
    );
    let salary = r
        .iter()
        .find(|r| r[0].as_deref() == Some(ex("salary").as_str()))
        .unwrap_or_else(|| panic!("no :salary group in {r:?}"));
    assert_eq!(salary[1], Some(int(3)));
    // Distinct terms with equal values are distinct group keys (GROUP BY
    // groups RDF terms), each printed as written.
    let mut db = Database::new("g");
    update(
        &mut db,
        "INSERT DATA { <http://ex/x> <http://ex/n> 1 . <http://ex/y> <http://ex/n> \"01\"^^<http://www.w3.org/2001/XMLSchema#integer> }",
    );
    let mut keys = col(
        &mut db,
        "SELECT ?n (COUNT(*) AS ?c) WHERE { ?s <http://ex/n> ?n } GROUP BY ?n",
    );
    keys.sort();
    assert_eq!(keys, vec![lit("01", "integer"), int(1)]);
}

// ---------------------------------------------------------------------------
// 2. SPARQL Update over named graphs
// ---------------------------------------------------------------------------

fn quads() -> Database {
    let mut db = Database::new("quads");
    update(
        &mut db,
        "PREFIX : <http://ex/> INSERT DATA { :c :p 3 . \
         GRAPH :g1 { :a :p 1 } GRAPH :g2 { :b :p 2 } }",
    );
    db
}

/// Sorted `s p o` lines of a graph (`None` = the default graph).
fn graph(db: &mut Database, g: Option<&str>) -> Vec<String> {
    let q = match g {
        None => "SELECT ?s ?p ?o WHERE { ?s ?p ?o }".to_string(),
        Some(g) => format!("SELECT ?s ?p ?o WHERE {{ GRAPH <http://ex/{g}> {{ ?s ?p ?o }} }}"),
    };
    let mut v: Vec<String> = rows(db, &q)
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|c| c.unwrap_or_default())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    v.sort();
    v
}

fn t(s: &str, p: &str, o: &str) -> String {
    format!("{} {} {o}", ex(s), ex(p))
}

#[test]
fn modify_sees_named_graphs_and_writes_quad_templates() {
    let mut db = quads();
    // GRAPH ?g in WHERE and in both templates: each graph is rewritten in place.
    let n = update(
        &mut db,
        &format!(
            "{P}DELETE {{ GRAPH ?g {{ ?s :p ?o }} }} INSERT {{ GRAPH ?g {{ ?s :q ?o }} }} \
             WHERE {{ GRAPH ?g {{ ?s :p ?o }} }}"
        ),
    );
    assert_eq!(n, 4);
    assert_eq!(graph(&mut db, Some("g1")), vec![t("a", "q", &int(1))]);
    assert_eq!(graph(&mut db, Some("g2")), vec![t("b", "q", &int(2))]);
    assert_eq!(graph(&mut db, None), vec![t("c", "p", &int(3))]);

    // A constant template graph, created on first insert.
    update(
        &mut db,
        &format!(
            "{P}INSERT {{ GRAPH :g3 {{ ?s :copied ?o }} }} WHERE {{ GRAPH :g1 {{ ?s ?p ?o }} }}"
        ),
    );
    assert_eq!(graph(&mut db, Some("g3")), vec![t("a", "copied", &int(1))]);

    // Default-graph and named-graph templates in one request.
    update(
        &mut db,
        &format!(
            "{P}INSERT {{ ?s :seen \"y\" . GRAPH :g2 {{ ?s :seen \"y\" }} }} WHERE {{ ?s :p ?o }}"
        ),
    );
    let yes = plain("y");
    assert_eq!(
        graph(&mut db, None),
        vec![t("c", "p", &int(3)), t("c", "seen", &yes)]
    );
    assert!(graph(&mut db, Some("g2")).contains(&t("c", "seen", &yes)));

    // DELETE WHERE with a GRAPH block.
    assert_eq!(
        update(
            &mut db,
            &format!("{P}DELETE WHERE {{ GRAPH :g2 {{ ?s ?p ?o }} }}")
        ),
        2
    );
    assert!(graph(&mut db, Some("g2")).is_empty());
    assert_eq!(graph(&mut db, Some("g1")), vec![t("a", "q", &int(1))]);
}

#[test]
fn modify_skips_illegal_template_quads() {
    let mut db = quads();
    // Unbound graph variable, a literal as graph name, a literal subject: skipped.
    for u in [
        "INSERT { GRAPH ?nope { ?s :x 1 } } WHERE { ?s :p ?o }",
        "INSERT { GRAPH ?o { ?s :x 1 } } WHERE { ?s :p ?o }",
        "INSERT { ?o :x 1 } WHERE { ?s :p ?o }",
        "INSERT { ?s ?o 1 } WHERE { ?s :p ?o }",
    ] {
        assert_eq!(update(&mut db, &format!("{P}{u}")), 0, "{u}");
    }
    // A predicate variable reused as a subject keeps its term.
    update(
        &mut db,
        &format!("{P}INSERT {{ ?p a :Property }} WHERE {{ ?s ?p ?o }}"),
    );
    assert!(graph(&mut db, None).contains(&format!(
        "{} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> {}",
        ex("p"),
        ex("Property")
    )));
}

#[test]
fn with_names_the_default_graph_of_where_and_templates() {
    let mut db = quads();
    let n = update(
        &mut db,
        &format!("{P}WITH :g1 DELETE {{ ?s :p ?o }} INSERT {{ ?s :p2 ?o }} WHERE {{ ?s :p ?o }}"),
    );
    assert_eq!(n, 2);
    assert_eq!(graph(&mut db, Some("g1")), vec![t("a", "p2", &int(1))]);
    assert_eq!(graph(&mut db, None), vec![t("c", "p", &int(3))]);
    assert_eq!(graph(&mut db, Some("g2")), vec![t("b", "p", &int(2))]);

    // An explicit GRAPH in a template overrides WITH; GRAPH in WHERE still
    // reaches the named graphs.
    update(
        &mut db,
        &format!(
            "{P}WITH :g1 INSERT {{ ?s :tag \"x\" . GRAPH :g2 {{ ?s :tag \"y\" }} }} \
             WHERE {{ ?s ?p ?o }}"
        ),
    );
    assert!(graph(&mut db, Some("g1")).contains(&t("a", "tag", &plain("x"))));
    assert!(graph(&mut db, Some("g2")).contains(&t("a", "tag", &plain("y"))));
    update(
        &mut db,
        &format!("{P}WITH :g1 INSERT {{ ?s :from2 ?o }} WHERE {{ GRAPH :g2 {{ ?s :p ?o }} }}"),
    );
    assert!(graph(&mut db, Some("g1")).contains(&t("b", "from2", &int(2))));

    // WITH a graph that does not exist: the WHERE sees an empty graph, and
    // ground templates create it.
    assert_eq!(
        update(
            &mut db,
            &format!("{P}WITH :none INSERT {{ ?s :t 1 }} WHERE {{ ?s ?p ?o }}")
        ),
        0
    );
    assert_eq!(
        update(
            &mut db,
            &format!("{P}WITH :g9 INSERT {{ :n :t 1 }} WHERE {{}}")
        ),
        1
    );
    assert_eq!(graph(&mut db, Some("g9")), vec![t("n", "t", &int(1))]);
    assert_eq!(graph(&mut db, None), vec![t("c", "p", &int(3))]);
}

#[test]
fn using_and_using_named_define_the_where_dataset() {
    let mut db = quads();
    // USING: the WHERE's default graph is g2; the template goes to the
    // store's default graph.
    assert_eq!(
        update(
            &mut db,
            &format!("{P}INSERT {{ ?s :seen \"y\" }} USING :g2 WHERE {{ ?s :p ?o }}")
        ),
        1
    );
    assert!(graph(&mut db, None).contains(&t("b", "seen", &plain("y"))));
    // USING overrides WITH for the WHERE, WITH still targets the template.
    update(
        &mut db,
        &format!("{P}WITH :g1 INSERT {{ ?s :w 1 }} USING :g2 WHERE {{ ?s :p ?o }}"),
    );
    assert!(graph(&mut db, Some("g1")).contains(&t("b", "w", &int(1))));
    // Several USING graphs are merged.
    assert_eq!(
        update(
            &mut db,
            &format!("{P}INSERT {{ ?s :m 1 }} USING :g1 USING :g2 WHERE {{ ?s :p ?o }}")
        ),
        2
    );
    // USING NAMED: only those graphs are visible to GRAPH ?g …
    update(
        &mut db,
        &format!("{P}INSERT {{ ?s :n ?g }} USING NAMED :g2 WHERE {{ GRAPH ?g {{ ?s :p ?o }} }}"),
    );
    let named: Vec<_> = graph(&mut db, None)
        .into_iter()
        .filter(|l| l.contains("<http://ex/n>"))
        .collect();
    assert_eq!(named, vec![t("b", "n", &ex("g2"))]);
    // … and without USING the default graph is empty.
    assert_eq!(
        update(
            &mut db,
            &format!("{P}INSERT {{ ?s :z 1 }} USING NAMED :g2 WHERE {{ ?s :p ?o }}")
        ),
        0
    );
    // A DELETE with USING deletes from the default graph, matched elsewhere.
    update(&mut db, &format!("{P}INSERT DATA {{ :b :p 2 }}"));
    assert_eq!(
        update(
            &mut db,
            &format!("{P}DELETE {{ ?s :p ?o }} USING :g2 WHERE {{ ?s :p ?o }}")
        ),
        1
    );
    assert_eq!(graph(&mut db, Some("g2")), vec![t("b", "p", &int(2))]);
}

#[test]
fn load_into_graph_targets_that_graph() {
    let dir = std::env::temp_dir().join(format!("gstore_p3b_load_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("data.ttl");
    std::fs::write(&file, "<http://ex/l> <http://ex/p> \"loaded\" .\n").unwrap();
    let iri = format!("file://{}", file.display());

    let mut db = Database::new("load");
    assert_eq!(
        update(&mut db, &format!("LOAD <{iri}> INTO GRAPH <http://ex/lg>")),
        1
    );
    assert_eq!(
        graph(&mut db, Some("lg")),
        vec![t("l", "p", &plain("loaded"))]
    );
    assert!(graph(&mut db, None).is_empty());
    // Without INTO it goes to the default graph.
    assert_eq!(update(&mut db, &format!("LOAD <{iri}>")), 1);
    assert_eq!(graph(&mut db, None), vec![t("l", "p", &plain("loaded"))]);

    // The LoadPolicy still applies to LOAD … INTO.
    let mut db = Database::new("load2");
    db.set_load_policy(LoadPolicy::restricted());
    assert!(db
        .query(&format!("LOAD <{iri}> INTO GRAPH <http://ex/lg>"))
        .is_err());
    assert_eq!(
        update(
            &mut db,
            &format!("LOAD SILENT <{iri}> INTO GRAPH <http://ex/lg>")
        ),
        0
    );
    db.set_load_policy(LoadPolicy::restricted().with_allowed_dir(&dir));
    assert_eq!(
        update(&mut db, &format!("LOAD <{iri}> INTO GRAPH <http://ex/lg>")),
        1
    );
    assert_eq!(
        graph(&mut db, Some("lg")),
        vec![t("l", "p", &plain("loaded"))]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 4. typed literals
// ---------------------------------------------------------------------------

fn typed_db() -> Database {
    let mut db = Database::new("typed");
    update(
        &mut db,
        "PREFIX : <http://ex/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> INSERT DATA { \
         :a :v \"abc\"^^xsd:integer . :b :v \"01\"^^xsd:integer . :c :v \"300\"^^xsd:byte . \
         :d :v \"1\"^^xsd:byte . :e :v \"2020-13-01T00:00:00Z\"^^xsd:dateTime . \
         :f :v \"2020-01-01T00:00:00Z\"^^xsd:dateTime . :g :v \"1e3\"^^xsd:decimal . \
         :h :v \"-0\"^^xsd:nonPositiveInteger }",
    );
    db
}

fn subjects(db: &mut Database, filter: &str) -> Vec<String> {
    sorted(
        db,
        &format!(
            "PREFIX : <http://ex/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
             SELECT ?s WHERE {{ ?s :v ?o FILTER({filter}) }}"
        ),
    )
    .into_iter()
    .map(|s| {
        s.trim_start_matches("<http://ex/")
            .trim_end_matches('>')
            .to_string()
    })
    .collect()
}

#[test]
fn ill_typed_literals_are_stored_but_never_numbers() {
    let mut db = typed_db();
    // INSERT accepts ill-typed literals as they are.
    assert_eq!(
        col(
            &mut db,
            "SELECT ?o WHERE { <http://ex/a> <http://ex/v> ?o }"
        ),
        vec![lit("abc", "integer")]
    );
    assert_eq!(
        col(
            &mut db,
            "SELECT ?o WHERE { <http://ex/c> <http://ex/v> ?o }"
        ),
        vec![lit("300", "byte")]
    );
    // Value-space equality of valid literals, type errors for ill-typed ones.
    assert_eq!(subjects(&mut db, "?o = 1"), vec!["b", "d"]);
    assert_eq!(subjects(&mut db, "?o = \"1\"^^xsd:integer"), vec!["b", "d"]);
    assert_eq!(subjects(&mut db, "?o > -1"), vec!["b", "d", "h"]);
    assert_eq!(subjects(&mut db, "isNumeric(?o)"), vec!["b", "d", "h"]);
    // The same term is equal to itself; anything else against it is an error.
    assert_eq!(subjects(&mut db, "?o = \"abc\"^^xsd:integer"), vec!["a"]);
    assert!(subjects(&mut db, "?o != \"abc\"^^xsd:integer").is_empty());
    assert_eq!(
        subjects(&mut db, "sameTerm(?o, \"300\"^^xsd:byte)"),
        vec!["c"]
    );
    // An IRI is simply a different term from every literal.
    assert_eq!(subjects(&mut db, "?o != <http://ex/x>").len(), 8);
    // EBV of an ill-typed numeric is false (so its negation holds); a
    // dateTime has no EBV at all.
    assert_eq!(subjects(&mut db, "!?o"), vec!["a", "c", "g", "h"]);
    // Ill-typed dateTimes do not compare.
    assert_eq!(
        subjects(&mut db, "?o < \"2020-06-01T00:00:00Z\"^^xsd:dateTime"),
        vec!["f"]
    );
    // Arithmetic and casts on an ill-typed literal are errors (unbound),
    // while STR / DATATYPE see the literal as written.
    let r = rows(
        &mut db,
        "PREFIX : <http://ex/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
         SELECT ?s (?o + 1 AS ?x) (xsd:integer(?o) AS ?i) (STR(?o) AS ?l) (DATATYPE(?o) AS ?t) \
         WHERE { ?s :v ?o }",
    );
    let by = |s: &str| {
        r.iter()
            .find(|r| r[0].as_deref() == Some(ex(s).as_str()))
            .unwrap()
            .clone()
    };
    assert_eq!(
        by("c"),
        vec![
            Some(ex("c")),
            None,
            None,
            Some(plain("300")),
            Some(format!("<{XSD}byte>"))
        ]
    );
    assert_eq!(by("b")[1], Some(int(2)));
    assert_eq!(by("b")[2], Some(int(1)));
    assert_eq!(by("a")[1], None);
}

// ---------------------------------------------------------------------------
// 5. query cache
// ---------------------------------------------------------------------------

#[test]
fn query_cache_skips_non_deterministic_queries() {
    let mut db = staff();
    let det = format!("{P}SELECT ?p WHERE {{ ?p :salary ?s }}");
    db.select(&det).unwrap();
    assert_eq!(db.query_cache_len(), 1);
    assert!(db.query_cache_bytes() > 0);
    db.register_function("http://ex/fn#twice", |args| {
        Some(gstore::Value::Int(args.first()?.as_i128()? as i64 * 2))
    });
    assert_eq!(db.query_cache_len(), 0, "registering a function clears it");
    for q in [
        "SELECT (NOW() AS ?t) WHERE {}",
        "SELECT (RAND() AS ?r) WHERE {}",
        "SELECT (UUID() AS ?u) WHERE {}",
        "SELECT (STRUUID() AS ?u) WHERE {}",
        "SELECT ?b WHERE { BIND(BNODE() AS ?b) }",
        "SELECT ?p WHERE { ?p <http://ex/salary> ?s } ORDER BY (RAND())",
        "SELECT * WHERE { SERVICE SILENT <urn:x-no-network:sparql> { ?s ?p ?o } }",
        "SELECT (<http://ex/fn#twice>(2) AS ?x) WHERE {}",
    ] {
        db.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(db.query_cache_len(), 0, "{q} must not be cached");
    }
    assert_eq!(
        col(&mut db, "SELECT (<http://ex/fn#twice>(2) AS ?x) WHERE {}"),
        vec![int(4)]
    );
    // Deterministic ones are, until the next mutation.
    db.select(&det).unwrap();
    db.select(&format!(
        "{P}SELECT (ROUND(AVG(?s)) AS ?a) WHERE {{ ?p :salary ?s }}"
    ))
    .unwrap();
    assert_eq!(db.query_cache_len(), 2);
    update(&mut db, &format!("{P}INSERT DATA {{ :erin :salary 1 }}"));
    assert_eq!(db.query_cache_len(), 0);
    assert_eq!(db.query_cache_bytes(), 0);
}

#[test]
fn query_cache_is_bounded_by_bytes() {
    let mut db = Database::new("big");
    let mut data = String::from("INSERT DATA {");
    for i in 0..2000 {
        data.push_str(&format!(
            " <http://ex/s{i}> <http://ex/p> \"value number {i}\" ."
        ));
    }
    data.push('}');
    update(&mut db, &data);
    let small = "SELECT ?o WHERE { <http://ex/s1> <http://ex/p> ?o }";
    let big = "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }";
    assert_eq!(db.query_cache_size(), gstore::db::DEFAULT_QUERY_CACHE_BYTES);
    db.select(big).unwrap();
    assert_eq!(db.query_cache_len(), 1);
    let big_bytes = db.query_cache_bytes();
    assert!(big_bytes > 2000 * 20, "{big_bytes}");
    // A budget too small for the big result (above 1/8 of it) still caches
    // small ones.
    db.set_query_cache_size(4 * big_bytes);
    db.select(big).unwrap();
    db.select(small).unwrap();
    assert_eq!(db.query_cache_len(), 1);
    assert!(db.query_cache_bytes() < big_bytes);
    // Zero disables the cache; results stay correct.
    db.set_query_cache_size(0);
    assert_eq!(db.select(big).unwrap().row_count(), 2000);
    assert_eq!(db.query_cache_len(), 0);
}

// ---------------------------------------------------------------------------
// 6. integer division
// ---------------------------------------------------------------------------

#[test]
fn integer_division_is_decimal_by_default_and_truncates_on_request() {
    let mut db = staff();
    let q = "SELECT (7/2 AS ?a) (6/2 AS ?b) (-7/2 AS ?c) (7.0/2 AS ?d) (1/0 AS ?e) (7e0/2 AS ?f) \
             WHERE {}";
    assert_eq!(db.integer_division(), IntegerDivision::Decimal);
    assert_eq!(
        row(&mut db, q),
        vec![
            Some(lit("3.5", "decimal")),
            Some(lit("3.0", "decimal")),
            Some(lit("-3.5", "decimal")),
            Some(lit("3.5", "decimal")),
            None,
            Some(lit("3.5", "double")),
        ]
    );
    let filter = format!("{P}SELECT ?p WHERE {{ ?p :salary ?s FILTER(?s / 1000 = 2) }}");
    assert!(col(&mut db, &filter).is_empty());
    let avg = format!("{P}SELECT (AVG(?s) AS ?a) WHERE {{ ?p :salary ?s }}");
    assert_eq!(
        col(&mut db, &avg),
        vec![lit("3166.666666666666666667", "decimal")]
    );

    // Upstream gStore compatibility: truncating integer division. Changing
    // the mode empties the cache, so the cached answers above are not reused.
    db.set_integer_division(IntegerDivision::Truncate);
    assert_eq!(db.query_cache_len(), 0);
    assert_eq!(
        row(&mut db, q),
        vec![
            Some(int(3)),
            Some(int(3)),
            Some(int(-3)),
            Some(lit("3.5", "decimal")),
            None,
            Some(lit("3.5", "double")),
        ]
    );
    assert_eq!(col(&mut db, &filter), vec![ex("alice")]);
    assert_eq!(col(&mut db, &avg), vec![int(3166)]);
    // The mode applies to UPDATE … WHERE as well.
    update(
        &mut db,
        &format!("{P}INSERT {{ ?p :half ?h }} WHERE {{ ?p :salary ?s BIND(?s / 3 AS ?h) }}"),
    );
    assert_eq!(
        col(
            &mut db,
            &format!("{P}SELECT ?h WHERE {{ :alice :half ?h }}")
        ),
        vec![int(833)]
    );
    db.set_integer_division(IntegerDivision::Decimal);
    assert_eq!(
        col(&mut db, &avg),
        vec![lit("3166.666666666666666667", "decimal")]
    );
}

// ---------------------------------------------------------------------------
// 7. XSD lexicals of doubles and floats
// ---------------------------------------------------------------------------

#[test]
fn special_doubles_print_as_xsd_lexicals_everywhere() {
    let mut db = Database::new("inf");
    let q = "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
             SELECT (1e0/0 AS ?a) (-1e0/0 AS ?b) (0e0/0 AS ?n) (STR(1e0/0) AS ?s) \
             (xsd:float(\"-INF\") AS ?f) (xsd:float(\"1.5\") * 0 / 0 AS ?fn) WHERE {}";
    let rs = db.select(q).unwrap();
    assert_eq!(
        rs.rows,
        vec![vec![
            Some(lit("INF", "double")),
            Some(lit("-INF", "double")),
            Some(lit("NaN", "double")),
            Some(plain("INF")),
            Some(lit("-INF", "float")),
            Some(lit("NaN", "float")),
        ]]
    );
    for fmt in [
        ResultFormat::Json,
        ResultFormat::Xml,
        ResultFormat::Csv,
        ResultFormat::Tsv,
    ] {
        let mut out = Vec::new();
        write_select(fmt, &rs, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        for lexical in ["INF", "-INF", "NaN"] {
            assert!(out.contains(lexical), "{fmt:?}: {out}");
        }
        for rust in ["inf", "Infinity", "nan"] {
            assert!(!out.contains(rust), "{fmt:?} contains {rust}: {out}");
        }
    }
}

#[test]
fn floats_keep_their_datatype_through_expressions() {
    let mut db = Database::new("float");
    update(
        &mut db,
        "INSERT DATA { <http://ex/x> <http://ex/f> \"1.5\"^^<http://www.w3.org/2001/XMLSchema#float> }",
    );
    let r = row(
        &mut db,
        "SELECT (?f * 2 AS ?y) (ABS(-?f) AS ?z) (DATATYPE(?f + 1) AS ?t) (?f + 1e0 AS ?d) \
         (ROUND(?f) AS ?r) WHERE { <http://ex/x> <http://ex/f> ?f }",
    );
    assert_eq!(
        r,
        vec![
            Some(lit("3", "float")),
            Some(lit("1.5", "float")),
            Some(format!("<{XSD}float>")),
            Some(lit("2.5", "double")),
            Some(lit("2", "float")),
        ]
    );
}
