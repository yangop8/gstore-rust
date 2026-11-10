//! Data test: Phase 3 SPARQL syntax and semantics together. The syntax
//! package (signed literals, `IN` / `NOT IN`, trailing `VALUES`, `[ … ]` /
//! `( … )`, `ADD` / `MOVE` / `COPY`, `CONTAINALL` / `CONTAINANY`, bare-call
//! `HAVING` / `ORDER BY`) and the semantics package (named-graph Modify with
//! `WITH` / `USING`, value-space equality of typed literals, aggregates inside
//! expressions, the query-cache bypass for non-deterministic queries, `INF` /
//! `NaN` lexicals, the integer-division mode) are tested in their own files;
//! this one checks that they compose.

use gstore::query::IntegerDivision;
use gstore::{ConcurrentDb, Database, QueryResult};

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const P: &str = "PREFIX : <http://ex/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";

fn lit(lexical: &str, local: &str) -> String {
    format!("\"{lexical}\"^^<{XSD}{local}>")
}

fn int(n: i64) -> String {
    lit(&n.to_string(), "integer")
}

fn ex(local: &str) -> String {
    format!("<http://ex/{local}>")
}

fn rows(db: &mut Database, q: &str) -> Vec<Vec<Option<String>>> {
    db.select(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

/// Every row as `Some` cells, sorted.
fn sorted_rows(db: &mut Database, q: &str) -> Vec<Vec<String>> {
    let mut v: Vec<Vec<String>> = rows(db, q)
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|c| c.unwrap_or_else(|| "-".into()))
                .collect()
        })
        .collect();
    v.sort();
    v
}

/// The first column, sorted.
fn sorted(db: &mut Database, q: &str) -> Vec<String> {
    sorted_rows(db, q)
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

fn update(db: &mut Database, u: &str) -> usize {
    match db.query(u).unwrap_or_else(|e| panic!("{u}: {e}")) {
        QueryResult::Update { changed } => changed,
        other => panic!("{u}: not an update result: {other:?}"),
    }
}

fn update_err(db: &mut Database, u: &str) -> String {
    match db.query(u) {
        Ok(r) => panic!("{u}: expected an error, got {r:?}"),
        Err(e) => e.to_string(),
    }
}

/// Sorted `s p o` lines of a graph (`None` = the default graph).
fn graph(db: &mut Database, g: Option<&str>) -> Vec<String> {
    let q = match g {
        None => "SELECT ?s ?p ?o WHERE { ?s ?p ?o }".to_string(),
        Some(g) => format!("SELECT ?s ?p ?o WHERE {{ GRAPH <http://ex/{g}> {{ ?s ?p ?o }} }}"),
    };
    sorted_rows(db, &q)
        .into_iter()
        .map(|r| r.join(" "))
        .collect()
}

fn t(s: &str, p: &str, o: &str) -> String {
    format!("{} {} {o}", ex(s), ex(p))
}

// ---------------------------------------------------------------------------
// ADD / MOVE / COPY with named-graph Modify (WITH / USING / GRAPH templates)
// ---------------------------------------------------------------------------

#[test]
fn copy_into_a_graph_then_modify_with_that_graph() {
    let mut db = Database::new("combo");
    // Signed numbers and booleans as INSERT DATA objects.
    update(
        &mut db,
        &format!("{P}INSERT DATA {{ :a :temp -5 ; :ok true . :b :temp +7 ; :ok false }}"),
    );
    assert_eq!(update(&mut db, &format!("{P}COPY DEFAULT TO :g")), 4);
    // WITH names the graph COPY just filled: the WHERE clause reads it and
    // templates outside GRAPH write to it; IN over signed literals selects.
    let n = update(
        &mut db,
        &format!(
            "{P}WITH :g DELETE {{ ?s :temp ?t }} INSERT {{ ?s :abs ?u }} \
             WHERE {{ ?s :temp ?t FILTER(?t IN (-5, +7)) BIND(ABS(?t) AS ?u) }}"
        ),
    );
    assert_eq!(n, 4);
    assert_eq!(
        graph(&mut db, Some("g")),
        vec![
            t("a", "abs", &int(5)),
            t("a", "ok", &lit("true", "boolean")),
            t("b", "abs", &int(7)),
            t("b", "ok", &lit("false", "boolean")),
        ]
    );
    // The default graph COPY read from is untouched; a signed number keeps
    // its lexical form (`+7` is the term "+7"^^xsd:integer).
    assert_eq!(
        graph(&mut db, None),
        vec![
            t("a", "ok", &lit("true", "boolean")),
            t("a", "temp", &int(-5)),
            t("b", "ok", &lit("false", "boolean")),
            t("b", "temp", &lit("+7", "integer")),
        ]
    );
    // USING the copied graph as the WHERE dataset, a `[ … ]` template node
    // is a fresh blank node per solution, written to a GRAPH block target.
    update(
        &mut db,
        &format!(
            "{P}INSERT {{ GRAPH :h {{ ?s :note [ :abs ?u ; :ok ?k ] }} }} USING :g \
             WHERE {{ ?s :abs ?u ; :ok ?k FILTER(?k IN (true)) }}"
        ),
    );
    let q =
        format!("{P}SELECT ?s ?u WHERE {{ GRAPH :h {{ ?s :note ?n . ?n :abs ?u ; :ok true }} }}");
    assert_eq!(sorted_rows(&mut db, &q), vec![vec![ex("a"), int(5)]]);
    // MOVE the graph away: the source is gone, so a plain ADD from it fails,
    // ADD SILENT is a no-op, and the destination holds the data.
    assert_eq!(update(&mut db, &format!("{P}MOVE :g TO :k")), 4 + 4);
    assert!(graph(&mut db, Some("g")).is_empty());
    assert_eq!(graph(&mut db, Some("k")).len(), 4);
    let e = update_err(&mut db, &format!("{P}ADD :g TO DEFAULT"));
    assert!(e.contains("does not exist"), "{e}");
    assert_eq!(update(&mut db, &format!("{P}ADD SILENT :g TO DEFAULT")), 0);
    // GRAPH ?g sees the moved graph like any other (and :h's blank nodes).
    let q = format!("{P}SELECT DISTINCT ?g WHERE {{ GRAPH ?g {{ ?s :abs ?u }} }}");
    assert_eq!(sorted(&mut db, &q), vec![ex("h"), ex("k")]);
    let n = update(
        &mut db,
        &format!(
            "{P}DELETE {{ GRAPH ?g {{ ?s :abs ?u }} }} USING NAMED :k \
             WHERE {{ GRAPH ?g {{ ?s :abs ?u }} }}"
        ),
    );
    assert_eq!(n, 2, "USING NAMED :k hides :h from the WHERE clause");
    assert_eq!(sorted(&mut db, &q), vec![ex("h")]);
}

#[test]
fn delete_forms_reject_anonymous_nodes_inside_graph_blocks() {
    let mut db = Database::new("combo");
    update(
        &mut db,
        &format!("{P}INSERT DATA {{ GRAPH :g {{ :a :p 1 }} }}"),
    );
    let e = update_err(
        &mut db,
        &format!("{P}DELETE WHERE {{ GRAPH :g {{ [] :p ?o }} }}"),
    );
    assert!(e.contains("DELETE WHERE"), "{e}");
    let e = update_err(
        &mut db,
        &format!(
            "{P}DELETE {{ GRAPH ?g {{ ?s :p [ :q 1 ] }} }} WHERE {{ GRAPH ?g {{ ?s :p ?o }} }}"
        ),
    );
    assert!(e.contains("DELETE template"), "{e}");
    // The quad form itself works. Patterns match terms: `+1` is not the
    // stored `1`.
    assert_eq!(
        update(
            &mut db,
            &format!("{P}DELETE WHERE {{ GRAPH :g {{ ?s :p +1 }} }}")
        ),
        0
    );
    assert_eq!(
        update(
            &mut db,
            &format!("{P}DELETE WHERE {{ GRAPH :g {{ ?s :p 1 }} }}")
        ),
        1
    );
    assert!(graph(&mut db, Some("g")).is_empty());
}

// ---------------------------------------------------------------------------
// IN over typed numerics: value-space equality (VALUES is term equality)
// ---------------------------------------------------------------------------

fn numbers() -> Database {
    let mut db = Database::new("numbers");
    update(
        &mut db,
        &format!(
            "{P}INSERT DATA {{ \
               :i01 :v \"01\"^^xsd:integer . :byte :v \"1\"^^xsd:byte . :dec :v 1.0 . \
               :dbl :v \"1\"^^xsd:double . :flt :v \"1e0\"^^xsd:float . :two :v +2 . \
               :bad :v \"abc\"^^xsd:integer . :str :v \"1\" . :iri :v :one }}"
        ),
    );
    db
}

#[test]
fn in_compares_typed_numerics_by_value() {
    let mut db = numbers();
    let q = format!("{P}SELECT ?s WHERE {{ ?s :v ?o FILTER(?o IN (+1)) }}");
    assert_eq!(
        sorted(&mut db, &q),
        vec![ex("byte"), ex("dbl"), ex("dec"), ex("flt"), ex("i01")]
    );
    // NOT IN: the IRI is simply unequal; the ill-typed literal and the
    // string are type errors against a number, so they drop out too.
    let q = format!("{P}SELECT ?s WHERE {{ ?s :v ?o FILTER(?o NOT IN (1, \"01\"^^xsd:byte)) }}");
    assert_eq!(sorted(&mut db, &q), vec![ex("iri"), ex("two")]);
    // The identical ill-typed term is equal to itself.
    let q = format!("{P}SELECT ?s WHERE {{ ?s :v ?o FILTER(?o IN (\"abc\"^^xsd:integer)) }}");
    assert_eq!(sorted(&mut db, &q), vec![ex("bad")]);
    // An empty list never errors.
    let q = format!("{P}SELECT ?s WHERE {{ ?s :v ?o FILTER(?o NOT IN ()) }}");
    assert_eq!(sorted(&mut db, &q).len(), 9);
    // A trailing VALUES joins on RDF terms: only the literal written `1`.
    let q = format!("{P}SELECT ?s WHERE {{ ?s :v ?o }} VALUES ?o {{ 1 }}");
    assert!(sorted(&mut db, &q).is_empty(), "no plain 1 is stored");
    let q = format!("{P}SELECT ?s WHERE {{ ?s :v ?o }} VALUES ?o {{ \"01\"^^xsd:integer +2 2 }}");
    assert_eq!(sorted(&mut db, &q), vec![ex("i01"), ex("two")]);
}

#[test]
fn in_and_builtins_over_aggregates_in_having_and_order_by() {
    let mut db = Database::new("staff");
    update(
        &mut db,
        &format!(
            "{P}INSERT DATA {{ :alice :dept :d1 ; :name \"alice\" . :bob :dept :d1 ; :name \"bob\" . \
             :carol :dept :d2 ; :name \"carol\" }}"
        ),
    );
    let base =
        format!("{P}SELECT ?d (COUNT(*) AS ?n) WHERE {{ ?p :dept ?d ; :name ?m }} GROUP BY ?d");
    // IN over an aggregate, with a typed member of another integer type.
    let q = format!("{base} HAVING (COUNT(*) IN (\"02\"^^xsd:byte, 5))");
    assert_eq!(sorted_rows(&mut db, &q), vec![vec![ex("d1"), int(2)]]);
    // Bare built-in constraints over the group: BOUND, and CONTAINANY over a
    // GROUP_CONCAT turned into an xsd:list.
    let q = format!(
        "{base} HAVING BOUND(?d) \
         CONTAINANY(\"bob\", STRDT(GROUP_CONCAT(?m; SEPARATOR=\",\"), xsd:list))"
    );
    assert_eq!(sorted_rows(&mut db, &q), vec![vec![ex("d1"), int(2)]]);
    // A bare aggregate call as an ORDER BY key, ascending then descending.
    let q = format!("{base} ORDER BY COUNT(*)");
    assert_eq!(
        rows(&mut db, &q),
        vec![
            vec![Some(ex("d2")), Some(int(1))],
            vec![Some(ex("d1")), Some(int(2))]
        ]
    );
    let q = format!("{base} ORDER BY DESC(STRLEN(GROUP_CONCAT(?m)))");
    assert_eq!(rows(&mut db, &q)[0][0], Some(ex("d1")));
}

// ---------------------------------------------------------------------------
// Trailing VALUES with aggregates
// ---------------------------------------------------------------------------

#[test]
fn trailing_values_after_grouping() {
    let mut db = Database::new("staff");
    update(
        &mut db,
        &format!("{P}INSERT DATA {{ :alice :dept :d1 . :bob :dept :d1 . :carol :dept :d2 }}"),
    );
    // A GROUP BY key restricts the groups even when it is not projected.
    let q = format!(
        "{P}SELECT (COUNT(*) AS ?n) WHERE {{ ?p :dept ?d }} GROUP BY ?d VALUES ?d {{ :d2 }}"
    );
    assert_eq!(rows(&mut db, &q), vec![vec![Some(int(1))]]);
    // A VALUES variable out of scope after grouping is visible to the SELECT
    // expressions (aggregates wrapped in arithmetic), one row per VALUES row.
    let q = format!(
        "{P}SELECT ?d ?k (COUNT(*) * ?k AS ?m) WHERE {{ ?p :dept ?d }} GROUP BY ?d \
         ORDER BY ?d ?k VALUES ?k {{ -1 +10 }}"
    );
    assert_eq!(
        rows(&mut db, &q),
        vec![
            vec![Some(ex("d1")), Some(int(-1)), Some(int(-2))],
            vec![Some(ex("d1")), Some(lit("+10", "integer")), Some(int(20))],
            vec![Some(ex("d2")), Some(int(-1)), Some(int(-1))],
            vec![Some(ex("d2")), Some(lit("+10", "integer")), Some(int(10))],
        ]
    );
    // HAVING alone makes one implicit group; VALUES then joins its alias.
    let q = format!(
        "{P}SELECT (COUNT(*) AS ?n) WHERE {{ ?p :dept ?d }} HAVING (COUNT(*) > 1) VALUES ?n {{ 3 4 }}"
    );
    assert_eq!(rows(&mut db, &q), vec![vec![Some(int(3))]]);
}

// ---------------------------------------------------------------------------
// The query cache and the new syntax
// ---------------------------------------------------------------------------

#[test]
fn query_cache_with_values_in_and_template_nodes() {
    let mut db = Database::new("cache");
    update(
        &mut db,
        &format!("{P}INSERT DATA {{ :alice :age 30 . :bob :age 25 . :carol :age -1 }}"),
    );
    // RAND() next to a trailing VALUES: never cached, in any position.
    for q in [
        format!("{P}SELECT ?p (RAND() AS ?r) WHERE {{ ?p :age ?a }} VALUES ?p {{ :alice }}"),
        format!("{P}SELECT ?p WHERE {{ ?p :age ?a }} ORDER BY RAND() VALUES ?a {{ 30 25 }}"),
        format!("{P}SELECT ?p WHERE {{ ?p :age ?a FILTER(RAND() IN (2, 3)) }} VALUES ?a {{ 30 }}"),
    ] {
        db.query(&q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(db.query_cache_len(), 0, "{q} must not be cached");
    }
    let q = format!("{P}SELECT ?p (RAND() AS ?r) WHERE {{ ?p :age ?a }} VALUES ?p {{ :alice }}");
    assert_eq!(sorted(&mut db, &q), vec![ex("alice")]);
    // A CONSTRUCT minting `[ … ]` nodes is not cached either: each run gets
    // fresh blank nodes.
    let c = format!("{P}CONSTRUCT {{ ?p :card [ :age ?a ] }} WHERE {{ ?p :age ?a }}");
    let bnodes = |db: &mut Database| -> Vec<String> {
        match db.query(&c).unwrap() {
            QueryResult::Construct(ts) => {
                let mut v: Vec<String> = ts
                    .iter()
                    .filter(|t| t.predicate.to_string() == ex("card"))
                    .map(|t| t.object.to_string())
                    .collect();
                v.sort();
                v
            }
            other => panic!("{other:?}"),
        }
    };
    let first = bnodes(&mut db);
    assert_eq!(first.len(), 3);
    assert_eq!(db.query_cache_len(), 0);
    let second = bnodes(&mut db);
    assert!(
        first.iter().all(|b| !second.contains(b)),
        "{first:?} / {second:?}"
    );
    // Deterministic queries with the new syntax are cached and replay
    // the same answer.
    let det = [
        format!("{P}SELECT ?p WHERE {{ ?p :age ?a FILTER(?a IN (-1, 25.0)) }}"),
        format!("{P}SELECT ?p ?a WHERE {{ ?p :age ?a }} VALUES ?p {{ :bob }}"),
        format!("{P}SELECT ?d (COUNT(*) AS ?n) WHERE {{ ?p :age ?d }} GROUP BY ?d HAVING (COUNT(*) IN (1))"),
    ];
    let before: Vec<_> = det.iter().map(|q| sorted_rows(&mut db, q)).collect();
    assert_eq!(db.query_cache_len(), det.len());
    let after: Vec<_> = det.iter().map(|q| sorted_rows(&mut db, q)).collect();
    assert_eq!(before, after);
    assert_eq!(before[0], vec![vec![ex("bob")], vec![ex("carol")]]);
    // ADD / COPY / MOVE invalidate it like any mutation.
    update(&mut db, &format!("{P}COPY DEFAULT TO :g"));
    assert_eq!(db.query_cache_len(), 0);
}

// ---------------------------------------------------------------------------
// INF / NaN lexicals with signed literals, VALUES and quad templates
// ---------------------------------------------------------------------------

#[test]
fn special_doubles_flow_through_values_in_and_graph_templates() {
    let mut db = Database::new("inf");
    // Signed double literals in VALUES; IEEE division by a signed zero.
    let q = format!(
        "{P}SELECT ?x (STR(1e0/?x) AS ?r) WHERE {{ VALUES ?x {{ -0e0 +0e0 +2e0 }} \
           FILTER((1e0/?x) IN (\"INF\"^^xsd:double, \"-INF\"^^xsd:double)) }}"
    );
    let specials = vec![
        vec![lit("+0e0", "double"), "\"INF\"".to_string()],
        vec![lit("-0e0", "double"), "\"-INF\"".to_string()],
    ];
    assert_eq!(sorted_rows(&mut db, &q), specials);
    // A trailing VALUES joins after the WHERE clause (whose FILTER sees no
    // ?x) and before the SELECT expressions and HAVING-free projection.
    let q = format!("{P}SELECT ?x (STR(1e0/?x) AS ?r) WHERE {{ }} VALUES ?x {{ -0e0 +0e0 }}");
    assert_eq!(sorted_rows(&mut db, &q), specials);
    // NaN is not equal to itself, so it is never IN a list.
    let q = format!("{P}SELECT ?x WHERE {{ BIND(0e0/0 AS ?x) FILTER(?x IN (?x)) }}");
    assert!(rows(&mut db, &q).is_empty());
    // The computed specials stored through a GRAPH template, one fresh `[ ]`
    // node per solution, and read back as xsd:double lexicals.
    update(
        &mut db,
        &format!(
            "{P}INSERT {{ GRAPH :g {{ [ :x ?x ; :inv ?r ] }} }} \
             WHERE {{ VALUES ?x {{ -0e0 +0e0 }} BIND(1e0/?x AS ?r) }}"
        ),
    );
    let q = format!("{P}SELECT ?r WHERE {{ GRAPH :g {{ ?b :inv ?r }} }}");
    assert_eq!(
        sorted(&mut db, &q),
        vec![lit("-INF", "double"), lit("INF", "double")]
    );
    let q = format!("{P}SELECT (COUNT(DISTINCT ?b) AS ?n) WHERE {{ GRAPH :g {{ ?b :inv ?r }} }}");
    assert_eq!(rows(&mut db, &q), vec![vec![Some(int(2))]]);
}

// ---------------------------------------------------------------------------
// The integer-division mode reaches the snapshot readers
// ---------------------------------------------------------------------------

#[test]
fn integer_division_mode_reaches_concurrent_snapshots() {
    let q = "SELECT (7/2 AS ?x) WHERE {}";
    let cell = |r: gstore::Result<QueryResult>| match r.unwrap() {
        QueryResult::Select(rs) => rs.rows[0][0].clone().unwrap(),
        other => panic!("{other:?}"),
    };
    let cdb = ConcurrentDb::new(Database::new("div"));
    assert_eq!(cell(cdb.query(q)), lit("3.5", "decimal"));
    cdb.write(|db| db.set_integer_division(IntegerDivision::Truncate));
    assert_eq!(cell(cdb.query(q)), int(3));
    assert_eq!(cell(cdb.begin().query(q)), int(3));
    assert_eq!(cell(cdb.version_view().query(q)), int(3));
    // Signed operands and IN in the same snapshot query.
    let q2 = "SELECT ?x WHERE { VALUES ?x { -7 +7 } FILTER((?x / 2) IN (-3, 3)) }";
    match cdb.query(q2).unwrap() {
        QueryResult::Select(rs) => assert_eq!(rs.row_count(), 2),
        other => panic!("{other:?}"),
    }
}
