//! End-to-end golden tests: gStore 1.4's advanced graph functions run as
//! SPARQL through [`Database::query`], on the sample social graph of upstream
//! `docs/en-us/SPARQL_QUERY.md` ("Advanced functions"), against the outputs
//! that document shows.
//!
//! Where the document and upstream's code disagree, the code wins and the
//! test says why (search for "DOC vs CODE"):
//!
//! * `cycleBoolean`'s result is printed as `"true"^^<…XMLSchema#>` in the
//!   document; GeneralEvaluation.cpp:2021-2027 writes `…XMLSchema#boolean>`.
//! * The `kHopEnumerate` example's `MINUS { <Alice> <dislike> }` has no
//!   object and does not parse (upstream's grammar requires one either); the
//!   test uses `MINUS { <Alice> <dislike> ?x . }`. The document's `nodes`
//!   order 3,2,0,1 is stale: upstream iterates an `unordered_map<int,
//!   string>` filled in path order, which for these ids is 3,2,1,0.
//! * The `diameterEstimation` and `betweennessCentrality` examples name the
//!   predicates `<likes>`, `<follows>`, `<dislikes>`, which the sample data
//!   does not use: upstream resolves them to an empty predicate list
//!   (`getPredSetByStringList` skips unknown IRIs) and returns 0. The
//!   document's 4 and 6.5 are the results over the data's own predicates,
//!   `{}`, which the tests check as well.
//! * The `kHopCount` and `kHopNeighbor` sections show only the output shape
//!   for Alice at depth 3 (the `kHopNeighbor` one names a vertex, `<Car>`,
//!   absent from the data); the tests use the values that shape takes on the
//!   sample data (Francis), traced in `dt_gfun_path.rs`.
//! * The `PR` / `PPR` / `labelProp` / `louvain` / `WCC` / `clusterCoeff`
//!   sections, and the functions the document does not describe
//!   (`simpleCyclePath`, `SSSP`, `shortestPaths`, `maximumKplex`, …), give
//!   no concrete output; those are checked against the library
//!   (`gstore::analytics::gfun`, itself traced against upstream in
//!   `dt_gfun_path.rs` / `dt_gfun_metric.rs`) on the same arguments, which
//!   pins the engine's part: argument resolution, distinct bindings, row
//!   shape and literal types.
//! * `PR`'s documented signature `PR(directed, pred_set, alpha, maxIter,
//!   tol)` omits the integer the grammar requires after `pred_set`
//!   (SPARQL.g4: `K_PR '(' booleanLiteral ',' predSet ',' integerLiteral ','
//!   K_ALPHA '=' … )`).
//!
//! The second half covers the engine integration itself: row shape next to
//! other projections, GROUP BY, ORDER BY/DISTINCT/LIMIT, queries without
//! solutions, constants vs variables, unknown IRIs, literal bindings, FILTER
//! forms, PFN, the `rs:` legacy namespace, XSD casts, user IRI functions,
//! cancellation and the shared CSR cache.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gstore::analytics::csr::{CsrGraph, PredFilter};
use gstore::analytics::gfun::{self, GArgs, GFnKind, GFunCtx, GFunValue, GNode};
use gstore::parser::ntriples::parse_term;
use gstore::{Database, GStoreError, ResultSet, Term};

/// SPARQL_QUERY.md's sample data. Entity ids follow first appearance:
/// Alice 0, Bob 1, Eve 2, Carol 3, Francis 4, Dave 5.
const SOCIAL: &str = "
<Alice> <focus> <Bob> .
<Alice> <like> <Bob> .
<Alice> <dislike> <Eve> .
<Bob> <focus> <Alice> .
<Bob> <like> <Eve> .
<Carol> <focus> <Bob> .
<Carol> <like> <Bob> .
<Carol> <dislike> <Francis> .
<Dave> <focus> <Alice> .
<Dave> <focus> <Eve> .
<Dave> <dislike> <Francis> .
<Eve> <like> <Carol> .
<Francis> <like> <Carol> .
<Francis> <dislike> <Dave> .
<Francis> <dislike> <Eve> .
";

const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const EMPTY: &str = r#"{"paths":[]}"#;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn social() -> Database {
    social_with("")
}

fn social_with(extra: &str) -> Database {
    let db = Database::build_from_str("gfun-e2e", &format!("{SOCIAL}{extra}")).unwrap();
    for (i, n) in ["Alice", "Bob", "Eve", "Carol", "Francis", "Dave"]
        .iter()
        .enumerate()
    {
        assert_eq!(db.dict().entity_id(&format!("<{n}>")), Some(i as u32));
    }
    db
}

fn select(db: &mut Database, q: &str) -> ResultSet {
    db.select(q).unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
}

fn query_err(db: &mut Database, q: &str) -> String {
    match db.query(q) {
        Err(e) => e.to_string(),
        Ok(r) => panic!("`{q}` should fail, got {r:?}"),
    }
}

/// A result cell as an RDF term.
fn term(cell: &Option<String>) -> Option<Term> {
    cell.as_ref()
        .map(|s| parse_term(s).unwrap_or_else(|e| panic!("bad term {s}: {e}")))
}

/// The lexical form of a plain-literal cell (panics on anything else).
fn plain(cell: &Option<String>) -> String {
    match term(cell) {
        Some(Term::Literal {
            value,
            datatype: None,
            lang: None,
        }) => value,
        other => panic!("expected a plain literal, got {other:?}"),
    }
}

/// The only cell of a one-row, one-column result.
fn only_cell(db: &mut Database, q: &str) -> Option<String> {
    let rs = select(db, q);
    assert_eq!(rs.vars.len(), 1, "{q}");
    assert_eq!(
        rs.rows.len(),
        1,
        "`{q}` should yield one row: {:?}",
        rs.rows
    );
    rs.rows[0][0].clone()
}

/// The JSON text of a one-row, one-column graph-function result.
fn json(db: &mut Database, q: &str) -> String {
    plain(&only_cell(db, q))
}

/// The distinct values of `?x` over `where_clause`'s solutions, in the
/// engine's solution order, as graph vertices.
fn bindings(db: &mut Database, where_clause: &str) -> Vec<GNode> {
    let rs = select(db, &format!("SELECT ?x WHERE {{ {where_clause} }}"));
    let csr = db.csr().unwrap();
    let mut out: Vec<GNode> = Vec::new();
    for row in &rs.rows {
        let key = row[0].clone().unwrap();
        let n = GNode::resolve(
            &csr,
            db.dict()
                .entity_id(&key)
                .or(db.dict().term_id(&parse_term(&key).unwrap())),
        );
        if !out.iter().any(|m| m.entity == n.entity) {
            out.push(n);
        }
    }
    out
}

/// Direct library evaluation, the reference for functions whose upstream
/// documentation shows no concrete output.
struct Lib<'a> {
    db: &'a Database,
    csr: Arc<CsrGraph>,
}

impl<'a> Lib<'a> {
    fn new(db: &'a Database) -> Lib<'a> {
        Lib {
            db,
            csr: db.csr().unwrap(),
        }
    }

    fn node(&self, name: &str) -> GNode {
        gfun::iri_node(&self.csr, self.db.dict(), name)
    }

    fn nodes(&self, names: &[&str]) -> Vec<GNode> {
        names.iter().map(|n| self.node(n)).collect()
    }

    fn preds(&self, include: &[&str]) -> PredFilter {
        PredFilter::resolve(include, &[], self.db.dict())
    }

    fn all(&self) -> PredFilter {
        self.preds(&[])
    }

    fn eval(&self, kind: GFnKind, args: &GArgs) -> GFunValue {
        gfun::eval(kind, &GFunCtx::new(&self.csr, self.db.dict()), args).unwrap()
    }

    fn json(&self, kind: GFnKind, args: &GArgs) -> String {
        match self.eval(kind, args) {
            GFunValue::Json(s) | GFunValue::ResultsJson(s) => s,
            other => panic!("{kind}: {other:?}"),
        }
    }
}

fn args(preds: PredFilter) -> GArgs {
    GArgs {
        preds,
        ..GArgs::default()
    }
}

/// One `{"src","dst","edges","nodes"}` path entry of the social graph.
fn path(src: &str, dst: &str, edges: &[(u32, u32, &str)], nodes: &[(u32, &str)]) -> String {
    let edges: Vec<String> = edges
        .iter()
        .map(|(f, t, p)| format!("{{\"fromNode\":{f},\"toNode\":{t},\"predIRI\":\"<{p}>\"}}"))
        .collect();
    let nodes: Vec<String> = nodes
        .iter()
        .map(|(i, n)| format!("{{\"nodeIndex\":{i},\"nodeIRI\":\"<{n}>\"}}"))
        .collect();
    format!(
        "{{\"src\":\"<{src}>\",\"dst\":\"<{dst}>\",\"edges\":[{}],\"nodes\":[{}]}}",
        edges.join(","),
        nodes.join(",")
    )
}

fn paths(items: &[String]) -> String {
    format!("{{\"paths\":[{}]}}", items.join(","))
}

/// The doc's shortest path Francis → Alice over {like, focus}.
fn francis_to_alice() -> String {
    paths(&[path(
        "Francis",
        "Alice",
        &[(4, 3, "like"), (3, 1, "like"), (1, 0, "focus")],
        &[(0, "Alice"), (1, "Bob"), (3, "Carol"), (4, "Francis")],
    )])
}

/// `<Bob> ?pred ?x` minus the people Francis dislikes: `?x = Alice`.
const BOB_NOT_DISLIKED: &str = "{ <Bob> ?pred ?x . MINUS { <Francis> <dislike> ?x . } }";

// ---------------------------------------------------------------------------
// SPARQL_QUERY.md: path-related queries
// ---------------------------------------------------------------------------

#[test]
fn doc_cycle_boolean_and_cycle_path() {
    let mut db = social();
    // Verbatim, including the missing blank before `where`.
    let cell = only_cell(
        &mut db,
        "select (cycleBoolean(?x, <Carol>, true, {<like>}) as ?y)where{\t<Francis> <dislike> ?x .}",
    );
    // DOC vs CODE: the document prints the datatype as `…XMLSchema#>`.
    assert_eq!(
        term(&cell),
        Some(Term::typed_literal("true", XSD_BOOLEAN)),
        "{cell:?}"
    );
    let got = json(
        &mut db,
        "SELECT (cyclePath(?x, <Carol>, true, {<like>}) as ?y)\nWHERE\n{\n\t<Francis> <dislike> ?x .\n}",
    );
    // Eve likes Carol, Carol likes Bob, Bob likes Eve; Dave has no cycle.
    let want = paths(&[path(
        "Eve",
        "Carol",
        &[(2, 3, "like"), (3, 1, "like"), (1, 2, "like")],
        &[(1, "Bob"), (3, "Carol"), (2, "Eve")],
    )]);
    assert_eq!(got, want);
    // Dave alone: no cycle → false.
    let cell = only_cell(
        &mut db,
        "SELECT (cycleBoolean(<Dave>, <Carol>, true, {<like>}) AS ?y) WHERE {}",
    );
    assert_eq!(term(&cell), Some(Term::typed_literal("false", XSD_BOOLEAN)));
}

#[test]
fn doc_shortest_path_and_length() {
    let mut db = social();
    let got = json(
        &mut db,
        "SELECT (shortestPath(<Francis>, ?x, true, {<like>, <focus>}) AS ?y)\n\
         WHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x . }\n     }",
    );
    assert_eq!(got, francis_to_alice());
    let got = json(
        &mut db,
        "SELECT (shortestPathLen(<Francis>, ?x, true, {<like>, <focus>}) AS ?y)\n\
         WHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x .}\n     }",
    );
    assert_eq!(
        got,
        r#"{"paths":[{"src":"<Francis>","dst":"<Alice>","length":3}]}"#
    );
    // The same with both vertices constant and an empty WHERE.
    let got = json(
        &mut db,
        "SELECT (shortestPath(<Francis>, <Alice>, true, {<like>, <focus>}) AS ?y) WHERE {}",
    );
    assert_eq!(got, francis_to_alice());
}

#[test]
fn doc_k_hop_reachable_and_path() {
    let mut db = social();
    let q = |f: &str, k: i64| {
        format!("SELECT ({f}(<Francis>, ?x, true, {k}, {{<like>, <focus>}}) AS ?y) WHERE{BOB_NOT_DISLIKED}")
    };
    assert_eq!(
        json(&mut db, &q("kHopReachable", 2)),
        r#"{"paths":[{"src":"<Francis>","dst":"<Alice>","value":"false"}]}"#
    );
    assert_eq!(
        json(&mut db, &q("kHopReachable", -1)),
        r#"{"paths":[{"src":"<Francis>","dst":"<Alice>","value":"true"}]}"#
    );
    // "The result may be the shortest path described above": it is.
    assert_eq!(
        json(&mut db, &q("kHopReachablePath", -1)),
        francis_to_alice()
    );
    assert_eq!(json(&mut db, &q("kHopReachablePath", 2)), EMPTY);
}

/// Upstream's regression suite: `scripts/test/parser_test/parser_q34.sql`
/// and `parser_q35.sql` on `parser_d34.ttl` (the sample graph with Chinese
/// predicate names; the file ends without a newline), expected
/// `parser_r34.txt` / `parser_r35.txt` (pretty-printed there).
#[test]
fn upstream_parser_test_q34_q35() {
    let data = SOCIAL
        .replace("<focus>", "<关注>")
        .replace("<dislike>", "<不喜欢>")
        .replace("<like>", "<喜欢>");
    let mut db = Database::build_from_str("parser_d34", data.trim_end()).unwrap();
    assert_eq!(
        json(
            &mut db,
            "select (shortestPathLen(<Francis>, ?x, true, {<喜欢>, <关注>}) as ?y)\nwhere\n{\n\t\
             <Bob> ?pred ?x .\n\tMINUS { <Francis> <不喜欢> ?x . }\n}"
        ),
        r#"{"paths":[{"src":"<Francis>","dst":"<Alice>","length":3}]}"#
    );
    assert_eq!(
        json(
            &mut db,
            "SELECT (kHopReachable(<Francis>, ?x, true, -1, {<喜欢>, <关注>}) AS ?y)\nWHERE\n{\n\t\
             <Bob> ?pred ?x .\n\tMINUS { <Francis> <不喜欢> ?x . }\n}"
        ),
        r#"{"paths":[{"src":"<Francis>","dst":"<Alice>","value":"true"}]}"#
    );
}

#[test]
fn doc_k_hop_enumerate() {
    let mut db = social();
    // DOC vs CODE: the document's `MINUS { <Alice> <dislike> }` lacks an
    // object and is a syntax error.
    let verbatim = "SELECT (kHopEnumerate(<Alice>, ?x, true, 3, {<like>, <focus>}) AS ?y) \n\
                    WHERE \n{\n    <Francis> ?pred ?x.\n    MINUS { <Alice> <dislike> }\n    }";
    assert!(db.query(verbatim).is_err());
    // ?x ∈ {Carol, Dave}; only Carol is reachable within 3 hops.
    let got = json(
        &mut db,
        "SELECT (kHopEnumerate(<Alice>, ?x, true, 3, {<like>, <focus>}) AS ?y) \n\
         WHERE \n{\n    <Francis> ?pred ?x.\n    MINUS { <Alice> <dislike> ?x . }\n    }",
    );
    // DOC vs CODE: `nodes` print 3,2,1,0 (the document's 3,2,0,1 is stale).
    let nodes = [(3, "Carol"), (2, "Eve"), (1, "Bob"), (0, "Alice")];
    let want = paths(&[
        path(
            "Alice",
            "Carol",
            &[(0, 1, "like"), (1, 2, "like"), (2, 3, "like")],
            &nodes,
        ),
        path(
            "Alice",
            "Carol",
            &[(0, 1, "focus"), (1, 2, "like"), (2, 3, "like")],
            &nodes,
        ),
    ]);
    assert_eq!(got, want);
}

#[test]
fn doc_k_hop_count_and_neighbor() {
    let mut db = social();
    // The documented output shape for Alice at depth 3: Francis.
    assert_eq!(
        json(
            &mut db,
            "SELECT (kHopCount(<Alice>, true, 3, {}) AS ?y) WHERE {}"
        ),
        r#"{"paths":[{"src":"<Alice>","depth":3, "count":1}]}"#
    );
    // Upstream's constant path (no WHERE solutions) names the list "dst", as
    // the document shows …
    assert_eq!(
        json(
            &mut db,
            "SELECT (kHopNeighbor(<Alice>, true, 3, {}) AS ?y) WHERE {}"
        ),
        r#"{"paths":[{"src":"<Alice>","depth":3, "dst":["<Francis>"]}]}"#
    );
    // … while with solutions it is "neighbor" (GE:2759-2769).
    assert_eq!(
        json(
            &mut db,
            "SELECT (kHopNeighbor(?x, true, 3, {}) AS ?y) WHERE { <Bob> <focus> ?x }"
        ),
        r#"{"paths":[{"src":"<Alice>","depth":3, "neighbor":["<Francis>"]}]}"#
    );
    // retNum caps the list.
    assert_eq!(
        json(
            &mut db,
            "SELECT (kHopNeighbor(<Bob>, false, 1, {}, 2) AS ?y) WHERE {}"
        ),
        r#"{"paths":[{"src":"<Bob>","depth":1, "dst":["<Alice>","<Carol>"]}]}"#
    );
}

#[test]
fn doc_bfs_count() {
    let mut db = social();
    assert_eq!(
        json(
            &mut db,
            "SELECT(bfsCount(<Alice>,true,{<like>,<focus>,<dislike>}) AS ?y)\nWHERE{}"
        ),
        r#"{"paths":[{"src":"<Alice>","results":[{"depth":0, "count":1}, {"depth":1, "count":2}, {"depth":2, "count":1}, {"depth":3, "count":1}, {"depth":4, "count":1}]}]}"#
    );
}

#[test]
fn doc_diameter_estimation() {
    let mut db = social();
    // DOC vs CODE: <likes>/<follows>/<dislikes> are not in the data → an
    // empty predicate list → 0; the document's 4 is over `{}`.
    assert_eq!(
        json(
            &mut db,
            "SELECT (diameterEstimation({<likes>, <follows>, <dislikes>}) AS ?y)WHERE{}"
        ),
        r#"{"paths":[0]}"#
    );
    assert_eq!(
        json(&mut db, "SELECT (diameterEstimation({}) AS ?y) WHERE {}"),
        r#"{"paths":[4]}"#
    );
}

#[test]
fn sssp_and_sssp_len_and_shortest_paths() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    let a = GArgs {
        srcs: lib.nodes(&["<Alice>"]),
        directed: true,
        ..args(lib.preds(&["<like>"]))
    };
    assert_eq!(
        json(
            &mut db,
            "SELECT (SSSP(<Alice>, true, {<like>}) AS ?y) WHERE {}"
        ),
        lib.json(GFnKind::Sssp, &a)
    );
    let len = json(
        &mut db,
        "SELECT (SSSPLen(<Alice>, true, {<like>}) AS ?y) WHERE {}",
    );
    assert_eq!(len, lib.json(GFnKind::SsspLen, &a));
    // Alice -like-> Bob -like-> Eve -like-> Carol.
    for (dst, d) in [("Bob", 1), ("Eve", 2), ("Carol", 3)] {
        assert!(
            len.contains(&format!(r#""dst":"<{dst}>","length":{d}"#)),
            "{len}"
        );
    }
    let pair = GArgs {
        srcs: lib.nodes(&["<Francis>"]),
        dsts: lib.nodes(&["<Alice>"]),
        directed: true,
        ..args(lib.preds(&["<like>", "<focus>"]))
    };
    for (f, kind) in [
        ("shortestPaths", GFnKind::ShortestPaths),
        ("shortestPathCount", GFnKind::ShortestPathCount),
    ] {
        let got = json(
            &mut db,
            &format!("SELECT ({f}(<Francis>, ?x, true, {{<like>, <focus>}}) AS ?y) WHERE {BOB_NOT_DISLIKED}"),
        );
        assert_eq!(got, lib.json(kind, &pair), "{f}");
        assert!(got.contains("<Francis>"), "{f}: {got}");
    }
}

#[test]
fn simple_cycles_and_k_hop_all_neighbors() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    // ?x ∈ the people Francis dislikes, in the engine's solution order.
    let xs = bindings(&mut db, "<Francis> <dislike> ?x");
    assert_eq!(xs.len(), 2);
    let a = GArgs {
        srcs: xs.clone(),
        dsts: lib.nodes(&["<Carol>"]),
        directed: true,
        ..args(lib.preds(&["<like>"]))
    };
    assert_eq!(
        json(
            &mut db,
            "SELECT (simpleCyclePath(?x, <Carol>, true, {<like>}) AS ?y) WHERE { <Francis> <dislike> ?x }"
        ),
        lib.json(GFnKind::SimpleCyclePath, &a)
    );
    let got = json(
        &mut db,
        "SELECT (simpleCycleBoolean(?x, <Carol>, true, {<like>}) AS ?y) WHERE { <Francis> <dislike> ?x }",
    );
    assert_eq!(got, lib.json(GFnKind::SimpleCycleBoolean, &a));
    let a = GArgs {
        srcs: lib.nodes(&["<Alice>"]),
        directed: true,
        k: Some(2),
        ..args(lib.all())
    };
    let got = json(
        &mut db,
        "SELECT (kHopAllNeighbors(<Alice>, true, 2, {}) AS ?y) WHERE {}",
    );
    assert_eq!(got, lib.json(GFnKind::KHopAllNeighbors, &a));
    assert!(got.contains("<Carol>"), "{got}");
}

// ---------------------------------------------------------------------------
// SPARQL_QUERY.md: importance analysis
// ---------------------------------------------------------------------------

#[test]
fn doc_pagerank() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    let got = json(
        &mut db,
        "SELECT (PR(true, {}, 10, alpha=0.85, maxIter=100, tol=0.0001) AS ?y) WHERE {}",
    );
    let a = GArgs {
        directed: true,
        ret_num: Some(10),
        alpha: Some(0.85),
        max_iter: Some(100.0),
        tol: Some(0.0001),
        ..args(lib.all())
    };
    assert_eq!(got, lib.json(GFnKind::Pr, &a));
    // The documented shape: one {"src","results"} entry per vertex.
    for n in ["Alice", "Bob", "Eve", "Carol", "Francis", "Dave"] {
        assert!(got.contains(&format!(r#""src":"<{n}>""#)), "{got}");
    }
}

#[test]
fn doc_personalized_pagerank() {
    let mut db = social();
    let got = json(
        &mut db,
        "select (PPR(?x, -1, {}, 3) as ?y)\nwhere\n{\n\t<Bob> ?pred ?x .\n}",
    );
    // ?x = Alice (Bob focus) and Eve (Bob like), one entry each.
    let xs = bindings(&mut db, "<Bob> ?pred ?x");
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    let a = GArgs {
        srcs: xs,
        k: Some(-1),
        ret_num: Some(3),
        ..args(lib.all())
    };
    assert_eq!(got, lib.json(GFnKind::Ppr, &a));
    assert!(
        got.contains(r#""src":"<Alice>""#) && got.contains(r#""src":"<Eve>""#),
        "{got}"
    );
}

#[test]
fn doc_closeness_centrality() {
    let mut db = social();
    assert_eq!(
        json(
            &mut db,
            "SELECT (closenessCentrality(<Alice>, false, {<like>, <focus>}) AS ?x) WHERE{}"
        ),
        r#"{"paths":[{"src":"<Alice>","result":0.555556}]}"#
    );
    assert_eq!(
        json(
            &mut db,
            "SELECT (closenessCentrality(<Alice>, true, {<like>, <focus>}) AS ?x) WHERE{}"
        ),
        r#"{"paths":[{"src":"<Alice>","result":0.500000}]}"#
    );
}

#[test]
fn doc_triangle_counting() {
    let mut db = social();
    assert_eq!(
        json(
            &mut db,
            "select (triangleCounting(true, {<like>}) as ?y) where {}"
        ),
        r#"{"paths":[1]}"#
    );
}

#[test]
fn doc_betweenness_centrality() {
    let mut db = social();
    // DOC vs CODE: the document's predicates are not in the data → 0.
    assert_eq!(
        json(
            &mut db,
            "SELECT (betweennessCentrality(<Eve>, true, {<likes>, <follows>, <dislikes>}) AS ?y) WHERE{}"
        ),
        r#"{"paths":[{"src":"<Eve>", "result":0}]}"#
    );
    assert_eq!(
        json(
            &mut db,
            "SELECT (betweennessCentrality(<Eve>, true, {}) AS ?y) WHERE{}"
        ),
        r#"{"paths":[{"src":"<Eve>", "result":6.5}]}"#
    );
}

// ---------------------------------------------------------------------------
// SPARQL_QUERY.md: community detection and correlation analysis
// ---------------------------------------------------------------------------

#[test]
fn communities_wcc_label_prop_louvain() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    let wcc = json(&mut db, "SELECT (WCC({}) AS ?y) WHERE {}");
    assert_eq!(wcc, lib.json(GFnKind::Wcc, &args(lib.all())));
    // The sample graph is one weakly connected component.
    for n in ["Alice", "Bob", "Eve", "Carol", "Francis", "Dave"] {
        assert!(wcc.contains(&format!("\"<{n}>\"")), "{wcc}");
    }
    let got = json(
        &mut db,
        "SELECT (labelProp(true, {<like>}, maxIter=10) AS ?y) WHERE {}",
    );
    let a = GArgs {
        directed: true,
        max_iter: Some(10.0),
        ..args(lib.preds(&["<like>"]))
    };
    assert_eq!(got, lib.json(GFnKind::LabelProp, &a));
    let got = json(
        &mut db,
        "SELECT (louvain(false, {}, maxIter=5, increase=0.1) AS ?y) WHERE {}",
    );
    let a = GArgs {
        max_iter: Some(5.0),
        increase: Some(0.1),
        ..args(lib.all())
    };
    assert_eq!(got, lib.json(GFnKind::Louvain, &a));
}

#[test]
fn cluster_coefficient_local_and_global() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    let local = GArgs {
        srcs: lib.nodes(&["<Bob>"]),
        ..args(lib.all())
    };
    assert_eq!(
        json(
            &mut db,
            "SELECT (clusterCoeff(<Bob>, false, {}) AS ?y) WHERE {}"
        ),
        lib.json(GFnKind::ClusterCoeff, &local)
    );
    // Without the vertex: the whole graph's coefficient.
    assert_eq!(
        json(&mut db, "SELECT (clusterCoeff(false, {}) AS ?y) WHERE {}"),
        lib.json(GFnKind::ClusterCoeff, &args(lib.all()))
    );
}

#[test]
fn doc_jaccard_similarity_and_degree_correlation() {
    let mut db = social();
    // Upstream prints these IRIs unquoted (GE:4099-4177), as documented.
    assert_eq!(
        json(
            &mut db,
            "SELECT (JaccardSimilarity(<Alice>, {}, 2, 10) AS ?y) WHERE{}"
        ),
        r#"{"paths":[{"dst":<Francis>, "value":0.5},{"dst":<Carol>, "value":0.5},{"dst":<Eve>, "value":0.5},{"dst":<Dave>, "value":0.333333},{"dst":<Bob>, "value":0.333333}]}"#
    );
    assert_eq!(
        json(
            &mut db,
            "SELECT (degreeCorrelation(<Alice>, -1, {}) AS ?y) WHERE{}"
        ),
        r#"{"paths":[{"src":"<Alice>", "result":0.0429273}]}"#
    );
}

#[test]
fn kplex_core_truss_and_confidence_degree() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    let a = GArgs {
        k: Some(2),
        ..args(lib.all())
    };
    assert_eq!(
        json(&mut db, "SELECT (maximumKplex({}, 2) AS ?y) WHERE {}"),
        lib.json(GFnKind::MaximumKplex, &a)
    );
    let a = GArgs {
        nums: vec![1.0, 2.0],
        ..args(lib.all())
    };
    assert_eq!(
        json(&mut db, "SELECT (coreTruss({}, 1, 2) AS ?y) WHERE {}"),
        lib.json(GFnKind::CoreTruss, &a)
    );
    // `confidenceDegree` is upstream's `pfn_type` result; it comes back as a
    // plain literal holding the JSON array.
    assert_eq!(
        json(
            &mut db,
            "SELECT (confidenceDegree(1, 1, 1, 1, 1, 1) AS ?y) WHERE {}"
        ),
        r#"[["<Alice>",0.7142857142857143],["<Bob>",0.7142857142857143],["<Eve>",1.0],["<Carol>",0.7142857142857143],["<Francis>",0.7142857142857143],["<Dave>",0.7142857142857143]]"#
    );
}

/// Every advanced function evaluates end to end (one call each, built from
/// its grammar), returning the literal type upstream gives it.
#[test]
fn every_function_evaluates_through_sparql() {
    let mut db = social();
    for kind in GFnKind::ALL {
        let mut args = Vec::new();
        let mut nodes = 0;
        for slot in kind.signature() {
            use gfun::ArgSlot as S;
            match slot {
                S::Node => {
                    nodes += 1;
                    args.push(if nodes == 1 { "?x" } else { "<Carol>" }.to_string());
                }
                S::OptNode => args.push("?x".into()),
                S::Bool => args.push("true".into()),
                S::Int | S::UInt => args.push("2".into()),
                S::OptInt | S::OptUInt => args.push("3".into()),
                S::Num => args.push("1".into()),
                S::PredSet => args.push("{<like>, !<dislike>}".into()),
                S::Named(key, _) => args.push(format!("{key}=0.5")),
            }
        }
        let args = args.join(", ").replace("maxIter=0.5", "maxIter=4");
        let q = format!(
            "SELECT ({}({args}) AS ?r) WHERE {{ <Francis> <dislike> ?x }}",
            kind.name()
        );
        let cell = only_cell(&mut db, &q);
        match kind {
            GFnKind::CycleBoolean => {
                assert!(
                    matches!(term(&cell), Some(Term::Literal { datatype: Some(ref d), .. }) if d == XSD_BOOLEAN),
                    "{q}: {cell:?}"
                );
            }
            GFnKind::ConfidenceDegree => assert!(plain(&cell).starts_with("[["), "{q}"),
            _ => assert!(plain(&cell).starts_with("{\"paths\":["), "{q}: {cell:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// engine integration: row shape
// ---------------------------------------------------------------------------

/// `bfsCount` over the given sources (all predicates, directed).
fn bfs(lib: &Lib<'_>, srcs: Vec<GNode>) -> String {
    lib.json(
        GFnKind::BfsCount,
        &GArgs {
            srcs,
            directed: true,
            ..args(lib.all())
        },
    )
}

#[test]
fn variables_use_the_distinct_bindings_cross_product() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    // ?x is bound three times to two vertices (Alice twice): one entry each.
    let where_clause = "?s <focus> ?x . FILTER(?x != <Bob>)";
    let xs = bindings(&mut db, where_clause);
    let names: Vec<&str> = xs
        .iter()
        .map(|n| db.dict().id_to_string(n.entity).unwrap())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    let got = json(
        &mut db,
        &format!("SELECT (bfsCount(?x, true, {{}}) AS ?c) WHERE {{ {where_clause} }}"),
    );
    assert_eq!(got, bfs(&lib, xs.clone()));
    // Two variables: the srcs × dsts cross product, in loop order.
    let got = json(
        &mut db,
        "SELECT (shortestPathLen(?x, ?y, true, {}) AS ?l) \
         WHERE { ?s <focus> ?x . FILTER(?x != <Bob>) <Francis> <like> ?y }",
    );
    let a = GArgs {
        srcs: xs,
        dsts: lib.nodes(&["<Carol>"]),
        directed: true,
        ..args(lib.all())
    };
    assert_eq!(got, lib.json(GFnKind::ShortestPathLen, &a));
    assert_eq!(got.matches("\"length\"").count(), 2, "{got}");
}

#[test]
fn next_to_other_projections_the_value_is_on_the_last_row() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    let xs = bindings(&mut db, "<Francis> <dislike> ?x");
    let rs = select(
        &mut db,
        "SELECT ?x (bfsCount(?x, true, {}) AS ?c) WHERE { <Francis> <dislike> ?x }",
    );
    assert_eq!(rs.vars, ["x", "c"]);
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(rs.rows[0][1], None);
    // Computed over all solutions, written to the last one (GE:3108).
    assert_eq!(plain(&rs.rows[1][1]), bfs(&lib, xs.clone()));
    let last = db.dict().id_to_string(xs[1].entity).unwrap().to_string();
    assert_eq!(rs.rows[1][0].as_deref(), Some(last.as_str()));
    // ORDER BY moves that row with its solution.
    let rs = select(
        &mut db,
        "SELECT ?x (bfsCount(?x, true, {}) AS ?c) WHERE { <Francis> <dislike> ?x } ORDER BY DESC(?x)",
    );
    let with_value: Vec<_> = rs.rows.iter().filter(|r| r[1].is_some()).collect();
    assert_eq!(with_value.len(), 1);
    assert_eq!(with_value[0][0].as_deref(), Some(last.as_str()));
    // ORDER BY the alias: unbound sorts first.
    let rs = select(
        &mut db,
        "SELECT ?x (bfsCount(?x, true, {}) AS ?c) WHERE { <Francis> <dislike> ?x } ORDER BY ?c",
    );
    assert_eq!(rs.rows[0][1], None);
    assert!(rs.rows[1][1].is_some());
    // An ORDER BY variable that is not projected counts as a projection
    // upstream (GE:1418-1437): one row per solution even for a lone function.
    let rs = select(
        &mut db,
        "SELECT (bfsCount(?x, true, {}) AS ?c) WHERE { <Francis> <dislike> ?x } ORDER BY ?x",
    );
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(rs.rows.iter().filter(|r| r[0].is_some()).count(), 1);
    // LIMIT on a single BGP is pushed below the function (upstream's
    // limitK): it sees the first solution only, which is the last row.
    let rs = select(
        &mut db,
        "SELECT ?x (bfsCount(?x, true, {}) AS ?c) WHERE { <Francis> <dislike> ?x } LIMIT 1",
    );
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(plain(&rs.rows[0][1]), bfs(&lib, xs[..1].to_vec()));
    // OFFSET / DISTINCT apply to the rows as usual.
    let rs = select(
        &mut db,
        "SELECT ?x (bfsCount(?x, true, {}) AS ?c) WHERE { <Francis> <dislike> ?x } OFFSET 1",
    );
    assert_eq!(rs.rows.len(), 1);
    assert!(rs.rows[0][1].is_some());
    let rs = select(
        &mut db,
        "SELECT DISTINCT (bfsCount(?x, true, {}) AS ?c) WHERE { <Francis> <dislike> ?x }",
    );
    assert_eq!(rs.rows.len(), 1);
    // Two functions side by side: both on the last row.
    let rs = select(
        &mut db,
        "SELECT (bfsCount(?x, true, {}) AS ?c) (kHopCount(?x, true, 1, {}) AS ?k) \
         WHERE { <Francis> <dislike> ?x }",
    );
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(rs.rows[0], vec![None, None]);
    assert!(rs.rows[1].iter().all(Option::is_some));
    // The alias must be a fresh variable.
    let e = query_err(
        &mut db,
        "SELECT ?y (bfsCount(?x, true, {}) AS ?x) WHERE { <Francis> <dislike> ?x . ?x ?p ?y }",
    );
    assert!(e.contains("already bound"), "{e}");
}

#[test]
fn group_by_evaluates_once_per_group() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    let rs = select(
        &mut db,
        "SELECT ?p (bfsCount(?x, true, {}) AS ?c) WHERE { <Francis> ?p ?x } GROUP BY ?p",
    );
    assert_eq!(rs.rows.len(), 2);
    for row in &rs.rows {
        let p = row[0].clone().unwrap();
        let xs = bindings(&mut db, &format!("<Francis> {p} ?x"));
        assert_eq!(plain(&row[1]), bfs(&lib, xs), "{p}");
    }
    // With a standard aggregate and no GROUP BY: one group, one row (SPARQL
    // aggregate semantics; upstream would repeat empty rows here).
    let rs = select(
        &mut db,
        "SELECT (COUNT(?x) AS ?n) (bfsCount(?x, true, {}) AS ?c) WHERE { <Francis> ?p ?x }",
    );
    assert_eq!(rs.rows.len(), 1);
    assert!(plain(&rs.rows[0][1]).contains("<Carol>"));
}

/// Upstream pushes LIMIT into a WHERE clause that is a single BGP when there
/// is no ORDER BY (`singleBGP`, GE:921-926, the limitK strategy): the BGP
/// stops after LIMIT rows (OFFSET is not added), so the functions see only
/// those.
#[test]
fn single_bgp_limit_is_pushed_below_graph_functions() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    // `?x <like> ?y` has five solutions with five distinct ?x.
    let xs = bindings(&mut db, "?x <like> ?y");
    assert_eq!(xs.len(), 5);
    let alice = lib.nodes(&["<Alice>"]);
    let len_to_alice = |srcs: &[GNode]| {
        lib.json(
            GFnKind::ShortestPathLen,
            &GArgs {
                srcs: srcs.to_vec(),
                dsts: alice.clone(),
                directed: true,
                ..args(lib.all())
            },
        )
    };
    // A lone function: one row over the first LIMIT solutions.
    let got = json(
        &mut db,
        "SELECT (shortestPathLen(?x, <Alice>, true, {}) AS ?l) WHERE { ?x <like> ?y } LIMIT 1",
    );
    assert_eq!(got, len_to_alice(&xs[..1]));
    // Next to a variable: LIMIT rows, the value on the last of them.
    let rs = select(
        &mut db,
        "SELECT ?x (shortestPathLen(?x, <Alice>, true, {}) AS ?l) WHERE { ?x <like> ?y } LIMIT 2",
    );
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(rs.rows[0][1], None);
    assert_eq!(plain(&rs.rows[1][1]), len_to_alice(&xs[..2]));
    // OFFSET is applied to the rows afterwards (the BGP still stops after
    // LIMIT rows).
    let rs = select(
        &mut db,
        "SELECT ?x (shortestPathLen(?x, <Alice>, true, {}) AS ?l) WHERE { ?x <like> ?y } \
         LIMIT 2 OFFSET 1",
    );
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(plain(&rs.rows[0][1]), len_to_alice(&xs[..2]));
    // GROUP BY groups only those solutions.
    let rs = select(
        &mut db,
        "SELECT ?y (bfsCount(?x, true, {}) AS ?c) WHERE { ?x <like> ?y } GROUP BY ?y LIMIT 1",
    );
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(plain(&rs.rows[0][1]), bfs(&lib, xs[..1].to_vec()));
    // No pushdown with ORDER BY, or when the WHERE clause is more than one
    // BGP: the function sees every solution.
    let all = len_to_alice(&xs);
    let rs = select(
        &mut db,
        "SELECT ?x (shortestPathLen(?x, <Alice>, true, {}) AS ?l) WHERE { ?x <like> ?y } \
         ORDER BY DESC(?x) LIMIT 5",
    );
    let values: Vec<_> = rs.rows.iter().filter_map(|r| r[1].as_ref()).collect();
    assert_eq!(values.len(), 1);
    assert_eq!(plain(&Some(values[0].clone())), all);
    assert_eq!(
        json(
            &mut db,
            "SELECT (shortestPathLen(?x, <Alice>, true, {}) AS ?l) \
             WHERE { ?x <like> ?y FILTER(?y != <Nobody>) } LIMIT 1"
        ),
        all
    );
    // Queries without a graph function keep SPARQL's LIMIT semantics.
    let rs = select(
        &mut db,
        "SELECT (COUNT(?x) AS ?n) WHERE { ?x <like> ?y } LIMIT 1",
    );
    assert_eq!(
        term(&rs.rows[0][0]).unwrap(),
        Term::typed_literal("5", format!("{XSD}integer"))
    );
}

/// A projected function is an aggregate upstream, so without GROUP BY all
/// solutions form one group and a failing HAVING leaves the function value
/// unset (GE:2071-2075) — the same HAVING that drops a COUNT row.
#[test]
fn having_applies_to_graph_functions_without_group_by() {
    let mut db = social();
    let q = |having: &str| {
        format!(
            "SELECT (bfsCount(<Alice>, true, {{}}) AS ?l) WHERE {{ ?x <like> ?y }} HAVING ({having})"
        )
    };
    let full = json(&mut db, &q("COUNT(?x) > 1"));
    assert!(full.contains("<Alice>"), "{full}");
    assert_eq!(only_cell(&mut db, &q("COUNT(?x) > 100")), None);
    let rs = select(
        &mut db,
        "SELECT (COUNT(?x) AS ?c) WHERE { ?x <like> ?y } HAVING (COUNT(?x) > 100)",
    );
    assert!(rs.rows.is_empty());
    // Several functions: the rows stay, no value is written.
    let mut two = |having: &str| {
        select(
            &mut db,
            &format!(
                "SELECT (bfsCount(?x, true, {{}}) AS ?c) (kHopCount(?x, true, 1, {{}}) AS ?k) \
                 WHERE {{ <Francis> <dislike> ?x }} HAVING ({having})"
            ),
        )
    };
    let rs = two("COUNT(?x) = 2");
    assert_eq!(rs.rows.len(), 2);
    assert!(rs.rows[1].iter().all(Option::is_some));
    let rs = two("COUNT(?x) > 2");
    assert_eq!(rs.rows.len(), 2);
    assert!(
        rs.rows.iter().flatten().all(Option::is_none),
        "{:?}",
        rs.rows
    );
}

/// Upstream keeps every BIND result in a string column
/// (TempResult::doBind) and rejects it as a vertex (GE:2583-2606), whether
/// or not the value is a stored term: `{"paths":[]}`.
#[test]
fn bind_results_are_never_vertices() {
    let mut db = social();
    for q in [
        "SELECT (bfsCount(?z, true, {}) AS ?l) WHERE { ?x <like> ?y . BIND(<Alice> AS ?z) }",
        "SELECT (bfsCount(?z, true, {}) AS ?l) WHERE { ?x <like> ?y . BIND(IRI(\"Alice\") AS ?z) }",
        "SELECT (bfsCount(?z, true, {}) AS ?l) WHERE { ?x <like> ?y . BIND(?y AS ?z) }",
        "SELECT (shortestPath(?x, ?z, true, {}) AS ?l) WHERE { ?x <like> ?y . BIND(?y AS ?z) }",
    ] {
        assert_eq!(json(&mut db, q), EMPTY, "{q}");
    }
    let cell = only_cell(
        &mut db,
        "SELECT (cycleBoolean(?x, ?z, true, {}) AS ?b) WHERE { ?x <like> ?y . BIND(?y AS ?z) }",
    );
    assert_eq!(term(&cell), Some(Term::typed_literal("false", XSD_BOOLEAN)));
    // The BIND variable's source is still a vertex.
    assert_ne!(
        json(
            &mut db,
            "SELECT (bfsCount(?y, true, {}) AS ?l) WHERE { ?x <like> ?y . BIND(?y AS ?z) }"
        ),
        EMPTY
    );
    // A variable a triple pattern binds too is read from the triple's id
    // column: a vertex.
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    assert_eq!(
        json(
            &mut db,
            "SELECT (bfsCount(?z, true, {}) AS ?l) WHERE { BIND(<Alice> AS ?z) ?z <like> ?y }"
        ),
        bfs(&lib, lib.nodes(&["<Alice>"]))
    );
}

/// Static call checks run over every operation of an UPDATE request before
/// the first one is applied: a typo in a later operation rejects the whole
/// request.
#[test]
fn statically_invalid_updates_apply_nothing() {
    let mut db = social();
    let before = db.triple_num();
    let mutations = db.mutation_count();
    for (bad, msg) in [
        (
            "FILTER(<http://ex/fn/typo>(?o))",
            "unknown function <http://ex/fn/typo>",
        ),
        (
            "BIND(shortestPathLen(?s, ?o, false, {}) AS ?l)",
            "only evaluated as the whole expression",
        ),
        (
            "FILTER(kHopReachable(?s, <Carol>, true, 3, {}))",
            "only evaluated as the whole expression",
        ),
    ] {
        let e = query_err(
            &mut db,
            &format!(
                "INSERT DATA {{ <New> <like> <New2> }} ; \
                 DELETE {{ ?s <like> ?o }} WHERE {{ ?s <like> ?o {bad} }}"
            ),
        );
        assert!(e.contains(msg), "{bad}: {e}");
        assert_eq!(db.triple_num(), before, "{bad}");
        assert_eq!(db.mutation_count(), mutations, "{bad}");
        assert!(
            select(&mut db, "SELECT ?o WHERE { <New> <like> ?o }")
                .rows
                .is_empty(),
            "{bad}"
        );
    }
    // A valid request still runs every operation.
    db.query("INSERT DATA { <New> <like> <New2> } ; DELETE WHERE { <New> <like> ?o }")
        .unwrap();
    assert_eq!(db.triple_num(), before);
    assert!(db.mutation_count() > mutations);
}

#[test]
fn queries_without_solutions() {
    let mut db = social();
    let lib_db = social();
    let lib = Lib::new(&lib_db);
    // A lone function: upstream's constant path, one row; the variable is
    // looked up as an IRI and is missing.
    let got = json(
        &mut db,
        "SELECT (shortestPathLen(<Francis>, ?x, true, {}) AS ?y) WHERE { <Nobody> <like> ?x }",
    );
    let a = GArgs {
        srcs: lib.nodes(&["<Francis>"]),
        dsts: vec![GNode::MISSING],
        directed: true,
        where_empty: true,
        ..args(lib.all())
    };
    assert_eq!(got, lib.json(GFnKind::ShortestPathLen, &a));
    let got = json(
        &mut db,
        "SELECT (bfsCount(?x, true, {}) AS ?y) WHERE { <Nobody> <like> ?x } GROUP BY ?x",
    );
    let a = GArgs {
        srcs: vec![GNode::MISSING],
        directed: true,
        where_empty: true,
        ..args(lib.all())
    };
    assert_eq!(got, lib.json(GFnKind::BfsCount, &a));
    // Next to another projection: no row at all.
    let rs = select(
        &mut db,
        "SELECT ?x (bfsCount(?x, true, {}) AS ?y) WHERE { <Nobody> <like> ?x }",
    );
    assert!(rs.rows.is_empty());
    // `WHERE {}` has no solutions upstream either.
    let rs = select(
        &mut db,
        "SELECT (1 AS ?one) (bfsCount(<Alice>, true, {}) AS ?y) WHERE {}",
    );
    assert!(rs.rows.is_empty());
}

#[test]
fn constants_unknown_iris_and_literal_bindings() {
    let mut db = social_with("<Zed> <age> \"30\" .\n");
    let lib_db = social_with("<Zed> <age> \"30\" .\n");
    let lib = Lib::new(&lib_db);
    // A constant equals the same vertex bound through a variable.
    let constant = json(
        &mut db,
        "SELECT (bfsCount(<Alice>, true, {}) AS ?y) WHERE {}",
    );
    let variable = json(
        &mut db,
        "SELECT (bfsCount(?x, true, {}) AS ?y) WHERE { <Bob> <focus> ?x }",
    );
    assert_eq!(constant, variable);
    // An unknown vertex IRI is upstream's INVALID id.
    let got = json(
        &mut db,
        "SELECT (bfsCount(<Nobody>, true, {}) AS ?y) WHERE {}",
    );
    assert_eq!(got, bfs(&lib, vec![GNode::MISSING]));
    // An unknown predicate resolves to nothing: only the source is visited.
    assert_eq!(
        json(
            &mut db,
            "SELECT (bfsCount(<Alice>, true, {<nope>}) AS ?y) WHERE {}"
        ),
        r#"{"paths":[{"src":"<Alice>","results":[{"depth":0, "count":1}]}]}"#
    );
    // An entity without entity edges (only a literal) is an isolated vertex.
    let got = json(&mut db, "SELECT (bfsCount(<Zed>, true, {}) AS ?y) WHERE {}");
    assert_eq!(
        got,
        r#"{"paths":[{"src":"<Zed>","results":[{"depth":0, "count":1}]}]}"#
    );
    // A literal binding is passed with its id (no edges), as upstream does.
    let lit = db.dict().term_id(&Term::plain_literal("30")).unwrap();
    let got = json(
        &mut db,
        "SELECT (bfsCount(?n, true, {}) AS ?y) WHERE { <Zed> <age> ?n }",
    );
    assert_eq!(got, bfs(&lib, vec![GNode::resolve(&lib.csr, Some(lit))]));
    // A BIND result is never a vertex (upstream's string column, "src must
    // be an entity"), even when it is a stored IRI.
    for bind in ["<http://nowhere/>", "<Alice>"] {
        assert_eq!(
            json(
                &mut db,
                &format!("SELECT (bfsCount(?v, true, {{}}) AS ?y) WHERE {{ BIND({bind} AS ?v) }}")
            ),
            EMPTY,
            "{bind}"
        );
    }
    // A variable the WHERE clause never binds: no vertex.
    assert_eq!(
        json(
            &mut db,
            "SELECT (bfsCount(?zz, true, {}) AS ?y) WHERE { <Bob> <focus> ?x }"
        ),
        EMPTY
    );
    // Prefixed names expand like everywhere else.
    assert_eq!(
        json(
            &mut db,
            "PREFIX : <> SELECT (bfsCount(<Alice>, true, {:like, !:dislike}) AS ?y) WHERE {}"
        ),
        json(
            &mut db,
            "SELECT (bfsCount(<Alice>, true, {<like>, <focus>}) AS ?y) WHERE {}"
        )
    );
}

/// A vertex variable that is unbound in every solution gives upstream's
/// never-entered loop: no entry (`false` for cycleBoolean) — also for
/// clusterCoeff, whose omitted vertex would mean the global coefficient.
#[test]
fn unbound_vertex_variables_give_no_entries() {
    let mut db = social();
    let where_clause = "WHERE { <Bob> <focus> ?x OPTIONAL { ?x <nope> ?z } }";
    for f in [
        "clusterCoeff(?z, false, {})",
        "bfsCount(?z, true, {})",
        "shortestPath(<Alice>, ?z, true, {})",
        "shortestPath(?x, ?z, true, {})",
    ] {
        assert_eq!(
            json(&mut db, &format!("SELECT ({f} AS ?c) {where_clause}")),
            EMPTY,
            "{f}"
        );
    }
    let cell = only_cell(
        &mut db,
        &format!("SELECT (cycleBoolean(?z, <Carol>, true, {{}}) AS ?c) {where_clause}"),
    );
    assert_eq!(term(&cell), Some(Term::typed_literal("false", XSD_BOOLEAN)));
    // With the vertex omitted, clusterCoeff is the global coefficient.
    let global = json(
        &mut db,
        &format!("SELECT (clusterCoeff(false, {{}}) AS ?c) {where_clause}"),
    );
    assert!(global.starts_with(r#"{"paths":[0."#), "{global}");
}

#[test]
fn graph_functions_in_sub_selects() {
    let mut db = social();
    let rs = select(
        &mut db,
        "SELECT ?c WHERE { { SELECT (kHopCount(<Alice>, true, 1, {}) AS ?c) WHERE {} } }",
    );
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(
        plain(&rs.rows[0][0]),
        r#"{"paths":[{"src":"<Alice>","depth":1, "count":2}]}"#
    );
}

// ---------------------------------------------------------------------------
// engine integration: errors, FILTER forms, PFN
// ---------------------------------------------------------------------------

#[test]
fn filter_forms_are_a_clean_error() {
    let mut db = social();
    for (q, name, legacy) in [
        (
            "SELECT ?x WHERE { ?x <like> ?y FILTER(cycleBoolean(?x, ?y, true, {})) }",
            "cycleBoolean",
            "rs:CYCLEBOOLEAN",
        ),
        (
            "SELECT ?x WHERE { ?x <like> ?y FILTER(simpleCycleBoolean(?x, ?y, true, {})) }",
            "simpleCycleBoolean",
            "rs:SIMPLECYCLEBOOLEAN",
        ),
        (
            "SELECT ?x WHERE { ?x <like> ?y FILTER(shortestPathLen(?x, ?y, true, {}) = 1) }",
            "shortestPathLen",
            "rs:SHORTESTPATHLEN",
        ),
        (
            "ASK { ?x <like> ?y FILTER(kHopReachable(?x, ?y, true, 1, {})) }",
            "kHopReachable",
            "rs:KHOPREACHABLE",
        ),
        (
            "SELECT ?x ?l WHERE { ?x <like> ?y BIND(shortestPathLen(?x, ?y, true, {}) AS ?l) }",
            "shortestPathLen",
            "rs:SHORTESTPATHLEN",
        ),
        (
            "SELECT ((shortestPathLen(<Alice>, <Bob>, true, {}) + 1) AS ?l) WHERE {}",
            "shortestPathLen",
            "rs:SHORTESTPATHLEN",
        ),
    ] {
        let e = query_err(&mut db, q);
        assert!(
            e.contains(&format!(
                "{name}(…) is only evaluated as the whole expression"
            )) && e.contains(legacy),
            "{q}: {e}"
        );
    }
    // Also in an UPDATE's WHERE (nothing is changed).
    let before = db.triple_num();
    let e = query_err(
        &mut db,
        "DELETE { ?x <like> ?y } WHERE { ?x <like> ?y FILTER(shortestPathLen(?x, ?y, true, {}) = 1) }",
    );
    assert!(e.contains("only evaluated as the whole expression"), "{e}");
    assert_eq!(db.triple_num(), before);
}

#[test]
fn pfn_is_a_clean_not_supported_yet_error() {
    let mut db = social();
    let e = query_err(&mut db, "SELECT (PFN.f({\"k\":\"v\"}) AS ?z) WHERE {}");
    assert!(e.contains("PFN not supported yet (Phase 4): PFN.f"), "{e}");
}

#[test]
fn legacy_shaped_bare_calls_point_to_rs() {
    let mut db = social();
    let e = query_err(&mut db, "SELECT (SSSP(<Alice>, <Carol>) AS ?p) WHERE {}");
    assert!(e.contains("rs:SSSP"), "{e}");
}

// ---------------------------------------------------------------------------
// rs: legacy namespace, XSD casts, user IRI functions
// ---------------------------------------------------------------------------

#[test]
fn rs_legacy_functions_work_anywhere() {
    let mut db = social();
    // SELECT, FILTER and BIND, with the predeclared prefix and the full IRI.
    let cell = only_cell(
        &mut db,
        "SELECT (rs:SSSPLEN(<Alice>, <Carol>) AS ?l) WHERE {}",
    );
    // The legacy function keeps its semantics: every predicate, so Alice
    // -dislike-> Eve -like-> Carol is 2 hops.
    assert_eq!(
        term(&cell).unwrap(),
        Term::typed_literal("2", format!("{XSD}integer"))
    );
    let rs = select(
        &mut db,
        "SELECT ?x WHERE { <Alice> <like> ?x FILTER(<urn:gstore-rust:fn:shortestPathLen>(?x, <Carol>) = 2) }",
    );
    assert_eq!(rs.rows, vec![vec![Some("<Bob>".to_string())]]);
    let rs = select(
        &mut db,
        "SELECT ?l WHERE { <Alice> <like> ?x BIND(rs:sssplen(?x, <Carol>) AS ?l) }",
    );
    assert_eq!(
        term(&rs.rows[0][0]).unwrap(),
        Term::typed_literal("2", format!("{XSD}integer"))
    );
    // The bare SIMILARITY is upstream's Jaro similarity of the term strings
    // (quotes included); the legacy one compares the lexical forms.
    // ("martha"/"marhta": Jaro 17/18 on the bare strings.)
    let upstream = gfun::metrics::jaro_similarity("\"martha\"", "\"marhta\"");
    let legacy = 17.0 / 18.0;
    assert!((upstream - legacy).abs() > 1e-3);
    for (f, want) in [("SIMILARITY", upstream), ("rs:SIMILARITY", legacy)] {
        let cell = only_cell(
            &mut db,
            &format!("SELECT ({f}(\"martha\", \"marhta\") AS ?s) WHERE {{}}"),
        );
        let Some(Term::Literal { value, .. }) = term(&cell) else {
            panic!("{cell:?}")
        };
        let got: f64 = value.parse().unwrap();
        assert!((got - want).abs() < 1e-6, "{f}: {got} vs {want}");
    }
    // A query's own PREFIX rs: replaces the predeclared one.
    let e = query_err(
        &mut db,
        "PREFIX rs: <http://other/> SELECT (rs:SSSPLEN(<Alice>, <Carol>) AS ?l) WHERE {}",
    );
    assert!(e.contains("unknown function <http://other/SSSPLEN>"), "{e}");
}

/// Evaluate one expression; `None` when it is unbound (an expression error).
fn eval1(db: &mut Database, expr: &str) -> Option<Term> {
    let q =
        format!("PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> SELECT ({expr} AS ?v) WHERE {{}}");
    term(&only_cell(db, &q))
}

fn typed(lexical: &str, dt: &str) -> Option<Term> {
    Some(Term::typed_literal(lexical, format!("{XSD}{dt}")))
}

#[test]
fn xsd_constructor_casts() {
    let mut db = social();
    let mut e = |x: &str| eval1(&mut db, x);
    // xsd:integer
    assert_eq!(e("xsd:integer(\" 42 \")"), typed("42", "integer"));
    assert_eq!(e("xsd:integer(\"-007\")"), typed("-7", "integer"));
    assert_eq!(e("xsd:integer(3.99)"), typed("3", "integer"));
    assert_eq!(e("xsd:integer(-3.99)"), typed("-3", "integer"));
    assert_eq!(
        e("xsd:integer(\"2.5e1\"^^xsd:double)"),
        typed("25", "integer")
    );
    assert_eq!(e("xsd:integer(true)"), typed("1", "integer"));
    assert_eq!(e("xsd:integer(\"4.5\")"), None);
    assert_eq!(e("xsd:integer(\"abc\")"), None);
    assert_eq!(e("xsd:integer(\"NaN\"^^xsd:double)"), None);
    assert_eq!(e("xsd:integer(<Alice>)"), None);
    assert_eq!(e("xsd:integer(\"1\"@en)"), None);
    // xsd:decimal
    assert_eq!(e("xsd:decimal(\"1.50\")"), typed("1.50", "decimal"));
    assert_eq!(e("xsd:decimal(2)"), typed("2", "decimal"));
    assert_eq!(
        e("xsd:decimal(\"0.1\"^^xsd:double)"),
        typed("0.1", "decimal")
    );
    assert_eq!(e("xsd:decimal(\"1e3\")"), None);
    assert_eq!(e("xsd:decimal(\"INF\"^^xsd:double)"), None);
    // xsd:double / xsd:float
    assert_eq!(e("xsd:double(\"1e3\")"), typed("1000.0", "double"));
    assert_eq!(e("xsd:double(2)"), typed("2.0", "double"));
    assert_eq!(e("xsd:double(\"inf\")"), None);
    // The special values print as their XSD lexicals, not Rust's inf / NaN.
    assert_eq!(e("xsd:double(\"-INF\")"), typed("-INF", "double"));
    assert_eq!(e("xsd:double(\"INF\")"), typed("INF", "double"));
    assert_eq!(e("xsd:double(\"NaN\")"), typed("NaN", "double"));
    assert_eq!(
        e("(xsd:double(\"-INF\") < -1e308)"),
        typed("true", "boolean")
    );
    assert_eq!(e("xsd:float(\"0.1\")"), typed("0.1", "float"));
    assert_eq!(e("xsd:float(\"NaN\")"), typed("NaN", "float"));
    assert_eq!(e("xsd:float(1)"), typed("1", "float"));
    // A float takes part in arithmetic and comparisons as a number.
    assert_eq!(e("(xsd:float(\"1.5\") + 1)"), typed("2.5", "double"));
    assert_eq!(e("(xsd:float(\"1.5\") > 1)"), typed("true", "boolean"));
    assert_eq!(e("(!xsd:float(\"0\"))"), typed("true", "boolean"));
    // xsd:boolean
    assert_eq!(e("xsd:boolean(\"1\")"), typed("true", "boolean"));
    assert_eq!(e("xsd:boolean(\"false\")"), typed("false", "boolean"));
    assert_eq!(e("xsd:boolean(0.0)"), typed("false", "boolean"));
    assert_eq!(e("xsd:boolean(5)"), typed("true", "boolean"));
    assert_eq!(e("xsd:boolean(\"yes\")"), None);
    // xsd:string
    assert_eq!(e("xsd:string(<Alice>)"), Some(Term::plain_literal("Alice")));
    assert_eq!(e("xsd:string(12)"), Some(Term::plain_literal("12")));
    assert_eq!(e("xsd:string(true)"), Some(Term::plain_literal("true")));
    assert_eq!(e("xsd:string(\"x\"@en)"), None);
    // xsd:dateTime / xsd:date
    assert_eq!(
        e("xsd:dateTime(\"2024-02-29T12:30:00Z\")"),
        typed("2024-02-29T12:30:00Z", "dateTime")
    );
    assert_eq!(e("xsd:dateTime(\"2023-02-29T12:30:00Z\")"), None);
    assert_eq!(e("xsd:dateTime(\"2024-02-29\")"), None);
    assert_eq!(
        e("xsd:dateTime(\"2024-02-29+08:00\"^^xsd:date)"),
        typed("2024-02-29T00:00:00+08:00", "dateTime")
    );
    assert_eq!(e("xsd:date(\"2024-01-31\")"), typed("2024-01-31", "date"));
    assert_eq!(
        e("xsd:date(\"2024-01-31T23:00:00-05:00\"^^xsd:dateTime)"),
        typed("2024-01-31-05:00", "date")
    );
    assert_eq!(e("xsd:date(5)"), None);
    // Ill-typed input literals are not castable.
    assert_eq!(e("xsd:string(\"abc\"^^xsd:integer)"), None);
    // In FILTER: a failed cast is an error, the solution is dropped.
    let rs = select(
        &mut db,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
         SELECT ?x WHERE { VALUES ?x { \"7\" \"x\" \"12\" } FILTER(xsd:integer(?x) > 8) }",
    );
    assert_eq!(rs.rows, vec![vec![Some("\"12\"".to_string())]]);
    // Wrong arity is a query error.
    let e = query_err(
        &mut db,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> SELECT (xsd:integer(1, 2) AS ?v) WHERE {}",
    );
    assert!(e.contains("takes exactly one argument"), "{e}");
}

#[test]
fn iri_functions_reach_user_functions_or_fail_cleanly() {
    let mut db = social();
    db.register_function("http://ex/fn/double", |args| {
        Some(gstore::Value::Int(args.first()?.as_f64()? as i64 * 2))
    });
    db.register_function("triple", |args| {
        Some(gstore::Value::Int(args.first()?.as_f64()? as i64 * 3))
    });
    let int = |v: &str| Some(Term::typed_literal(v, format!("{XSD}integer")));
    assert_eq!(
        term(&only_cell(
            &mut db,
            "SELECT (<http://ex/fn/double>(21) AS ?v) WHERE {}"
        )),
        int("42")
    );
    // By local name when the full IRI is not registered.
    assert_eq!(
        term(&only_cell(
            &mut db,
            "PREFIX f: <http://ex/fn#> SELECT (f:triple(5) AS ?v) WHERE {}"
        )),
        int("15")
    );
    // In an UPDATE's WHERE as well.
    db.query("INSERT { <Alice> <score> ?s } WHERE { BIND(<http://ex/fn/double>(4) AS ?s) }")
        .unwrap();
    let rs = select(&mut db, "SELECT ?s WHERE { <Alice> <score> ?s }");
    assert_eq!(term(&rs.rows[0][0]), int("8"));
    // Unknown IRI functions are a query error, wherever they appear.
    for q in [
        "SELECT (<http://ex/fn/none>(1) AS ?v) WHERE {}",
        "SELECT ?x WHERE { ?x <like> ?y FILTER(<http://ex/fn/none>(?y)) }",
        "ASK { BIND(<http://ex/fn/none>() AS ?v) }",
    ] {
        let e = query_err(&mut db, q);
        assert!(
            e.contains("unknown function <http://ex/fn/none>"),
            "{q}: {e}"
        );
    }
    // Including unknown xsd: local names (not a cast).
    let e = query_err(
        &mut db,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> SELECT (xsd:gYear(\"2020\") AS ?v) WHERE {}",
    );
    assert!(e.contains("unknown function"), "{e}");
}

// ---------------------------------------------------------------------------
// engine integration: cancellation and the shared CSR
// ---------------------------------------------------------------------------

#[test]
fn cancellation_reaches_graph_functions() {
    let mut db = social();
    let flag = Arc::new(AtomicBool::new(true));
    let r = db.query_cancellable(
        "SELECT (PR(true, {}, 10, alpha=0.85, maxIter=100, tol=0.0001) AS ?y) WHERE {}",
        flag,
    );
    assert!(matches!(r, Err(GStoreError::Cancelled)), "{r:?}");
}

#[test]
fn the_csr_is_cached_per_database_and_rebuilt_after_updates() {
    let mut db = social();
    assert!(!db.csr_is_cached());
    // A query without graph functions does not build it.
    select(&mut db, "SELECT ?x WHERE { <Alice> <like> ?x }");
    assert!(!db.csr_is_cached());
    let before = json(
        &mut db,
        "SELECT (kHopCount(<Alice>, true, 1, {}) AS ?y) WHERE {}",
    );
    assert!(db.csr_is_cached());
    let csr = db.csr().unwrap();
    // Another graph-function query reuses the same CSR.
    json(&mut db, "SELECT (WCC({}) AS ?y) WHERE {}");
    assert!(Arc::ptr_eq(&csr, &db.csr().unwrap()));
    assert_eq!(
        before,
        r#"{"paths":[{"src":"<Alice>","depth":1, "count":2}]}"#
    );
    // An update invalidates it; the next query sees the new edge.
    db.query("INSERT DATA { <Alice> <like> <Dave> . }").unwrap();
    assert!(!db.csr_is_cached());
    assert_eq!(
        json(
            &mut db,
            "SELECT (kHopCount(<Alice>, true, 1, {}) AS ?y) WHERE {}"
        ),
        r#"{"paths":[{"src":"<Alice>","depth":1, "count":3}]}"#
    );
    assert!(!Arc::ptr_eq(&csr, &db.csr().unwrap()));
}
