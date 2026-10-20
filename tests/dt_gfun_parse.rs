//! Data test: parsing of upstream gStore 1.4's advanced graph functions
//! (`SPARQL.g4` `builtInCall` path/analytics alternatives), `PFN.name({…})`
//! calls, the `rs:` namespace of the legacy gstore-rust graph built-ins and
//! IRI function calls.
//!
//! The golden queries are every advanced-function query in upstream's
//! `docs/en-us/SPARQL_QUERY.md` ("Advanced functions" chapter), copied
//! verbatim with their line numbers, plus the matching `docs/zh-cn` variants
//! that differ, and the three path queries of `scripts/test/parser_test`.

use gstore::parser::sparql::ast::*;
use gstore::parser::sparql::{parse, rs_function_name, RS_FUNCTIONS, RS_NAMESPACE};
use gstore::{Database, GStoreError};

// ---- helpers -------------------------------------------------------------

fn sel(q: &str) -> SelectQuery {
    match parse(q) {
        Ok(Query::Select(s)) => s,
        Ok(other) => panic!("expected SELECT, got {other:?}"),
        Err(e) => panic!("`{q}` failed to parse: {e}"),
    }
}

/// The `(expr AS ?v)` projection items of a SELECT.
fn items(q: &str) -> Vec<(Expr, String)> {
    match sel(q).projection {
        Projection::Items(items) => items
            .into_iter()
            .filter_map(|it| match it {
                SelectItem::Expr(e, v) => Some((e, v)),
                SelectItem::Var(_) => None,
            })
            .collect(),
        Projection::All => panic!("expected projection items in `{q}`"),
    }
}

/// The single projected advanced-function call of a SELECT, and its alias.
fn gfn(q: &str) -> (GFnName, Vec<GArg>, String) {
    let mut it = items(q);
    assert_eq!(it.len(), 1, "expected one projected expression in `{q}`");
    match it.pop().unwrap() {
        (Expr::GraphFn { func, args }, v) => (func, args, v),
        (other, _) => panic!("expected Expr::GraphFn in `{q}`, got {other:?}"),
    }
}

/// Parse `call` as the whole projection of `SELECT (call AS ?y) WHERE {}`.
fn call(call: &str) -> (GFnName, Vec<GArg>) {
    let (f, a, _) = gfn(&format!("SELECT ({call} AS ?y) WHERE {{}}"));
    (f, a)
}

/// The parse-error message of `q` (which must fail to parse).
fn err(q: &str) -> String {
    match parse(q) {
        Err(GStoreError::SparqlParse(m)) => m,
        Err(other) => panic!("expected a SPARQL parse error for `{q}`, got {other:?}"),
        Ok(parsed) => panic!("`{q}` should not parse, got {parsed:?}"),
    }
}

/// Parse-error message of `SELECT (c AS ?y) WHERE {}`.
fn call_err(c: &str) -> String {
    err(&format!("SELECT ({c} AS ?y) WHERE {{}}"))
}

fn var(v: &str) -> GArg {
    GArg::Node(VarOrIri::Var(v.into()))
}
fn iri(i: &str) -> GArg {
    GArg::Node(VarOrIri::Iri(i.into()))
}
fn ps(include: &[&str], exclude: &[&str]) -> GArg {
    GArg::PredSet {
        include: include.iter().map(|s| s.to_string()).collect(),
        exclude: exclude.iter().map(|s| s.to_string()).collect(),
    }
}
fn named_num(k: &str, v: f64) -> GArg {
    GArg::Named {
        key: k.into(),
        value: GNum::Num(v),
    }
}
fn named_int(k: &str, v: i64) -> GArg {
    GArg::Named {
        key: k.into(),
        value: GNum::Int(v),
    }
}

fn assert_contains(msg: &str, parts: &[&str]) {
    for p in parts {
        assert!(msg.contains(p), "expected {p:?} in error message:\n  {msg}");
    }
}

use GArg::{Bool, Int, Num};

// ---- upstream docs: every advanced-function query ------------------------

/// `docs/en-us/SPARQL_QUERY.md`, every ```sparql block of the "Advanced
/// functions" chapter that contains a SELECT (17 queries), verbatim.
/// (Line numbers are the first query line in upstream `bebe1ea`.)
const DOC_QUERIES: &[(u32, &str)] = &[
    (1001, "select (cycleBoolean(?x, <Carol>, true, {<like>}) as ?y)where{\t<Francis> <dislike> ?x .}"),
    (1013, "SELECT (cyclePath(?x, <Carol>, true, {<like>}) as ?y)\nWHERE\n{\n\t<Francis> <dislike> ?x .\n}"),
    (1095, "SELECT (shortestPath(<Francis>, ?x, true, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x . }\n     }"),
    (1156, "SELECT (shortestPathLen(<Francis>, ?x, true, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x .}\n     }"),
    (1268, "SELECT (kHopReachable(<Francis>, ?x, true, 2, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x . }\n     }"),
    (1287, "SELECT (kHopReachable(<Francis>, ?x, true, -1, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x . }\n     }"),
    (1302, "SELECT (kHopReachablePath(<Francis>, ?x, true, -1, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x . }\n     }"),
    // L1388 (kHopEnumerate) is malformed upstream; see `doc_khop_enumerate_example`.
    (1586, "SELECT(bfsCount(<Alice>,true,{<like>,<focus>,<dislike>}) AS ?y)\nWHERE{}"),
    (1619, "SELECT (diameterEstimation({<likes>, <follows>, <dislikes>}) AS ?y)WHERE{}"),
    (1706, "select (PPR(?x, -1, {}, 3) as ?y)\nwhere\n{\n\t<Bob> ?pred ?x .\n}"),
    (1747, "SELECT (closenessCentrality(<Alice>, false, {<like>, <focus>}) AS ?x) WHERE{}"),
    (1778, "SELECT (closenessCentrality(<Alice>, true, {<like>, <focus>}) AS ?x) WHERE{}"),
    (1833, "select (triangleCounting(true, {<like>}) as ?y) where {}"),
    (1869, "SELECT (betweennessCentrality(<Eve>, true, {<likes>, <follows>, <dislikes>}) AS ?y) WHERE{}"),
    (2033, "SELECT (JaccardSimilarity(<Alice>, {}, 2, 10) AS ?y) WHERE{}"),
    (2073, "SELECT (degreeCorrelation(<Alice>, -1, {}) AS ?y) WHERE{}"),
];

fn doc_expected(line: u32) -> (GFnName, Vec<GArg>, &'static str) {
    use GFnName as F;
    let lf = || ps(&["like", "focus"], &[]);
    match line {
        1001 => (
            F::CycleBoolean,
            vec![var("x"), iri("Carol"), Bool(true), ps(&["like"], &[])],
            "y",
        ),
        1013 => (
            F::CyclePath,
            vec![var("x"), iri("Carol"), Bool(true), ps(&["like"], &[])],
            "y",
        ),
        1095 => (
            F::ShortestPath,
            vec![iri("Francis"), var("x"), Bool(true), lf()],
            "y",
        ),
        1156 => (
            F::ShortestPathLen,
            vec![iri("Francis"), var("x"), Bool(true), lf()],
            "y",
        ),
        1268 => (
            F::KHopReachable,
            vec![iri("Francis"), var("x"), Bool(true), Int(2), lf()],
            "y",
        ),
        1287 => (
            F::KHopReachable,
            vec![iri("Francis"), var("x"), Bool(true), Int(-1), lf()],
            "y",
        ),
        1302 => (
            F::KHopReachablePath,
            vec![iri("Francis"), var("x"), Bool(true), Int(-1), lf()],
            "y",
        ),
        1586 => (
            F::BfsCount,
            vec![
                iri("Alice"),
                Bool(true),
                ps(&["like", "focus", "dislike"], &[]),
            ],
            "y",
        ),
        1619 => (
            F::DiameterEstimation,
            vec![ps(&["likes", "follows", "dislikes"], &[])],
            "y",
        ),
        1706 => (F::Ppr, vec![var("x"), Int(-1), ps(&[], &[]), Int(3)], "y"),
        1747 => (
            F::ClosenessCentrality,
            vec![iri("Alice"), Bool(false), lf()],
            "x",
        ),
        1778 => (
            F::ClosenessCentrality,
            vec![iri("Alice"), Bool(true), lf()],
            "x",
        ),
        1833 => (
            F::TriangleCounting,
            vec![Bool(true), ps(&["like"], &[])],
            "y",
        ),
        1869 => (
            F::BetweennessCentrality,
            vec![
                iri("Eve"),
                Bool(true),
                ps(&["likes", "follows", "dislikes"], &[]),
            ],
            "y",
        ),
        2033 => (
            F::JaccardSimilarity,
            vec![iri("Alice"), ps(&[], &[]), Int(2), Int(10)],
            "y",
        ),
        2073 => (
            F::DegreeCorrelation,
            vec![iri("Alice"), Int(-1), ps(&[], &[])],
            "y",
        ),
        _ => unreachable!("no expectation for doc line {line}"),
    }
}

#[test]
fn every_upstream_doc_query_parses_into_the_expected_ast() {
    assert_eq!(
        DOC_QUERIES.len(),
        16,
        "17 doc queries minus the malformed L1388 one"
    );
    for &(line, q) in DOC_QUERIES {
        let (func, args, alias) = gfn(q);
        let (efunc, eargs, ealias) = doc_expected(line);
        assert_eq!(func, efunc, "doc line {line}");
        assert_eq!(args, eargs, "doc line {line}");
        assert_eq!(alias, ealias, "doc line {line}");
    }
}

#[test]
fn doc_where_clauses_parse_alongside_the_function() {
    // The WHERE clause is parsed normally (MINUS, variable predicates, empty).
    let q = sel(DOC_QUERIES[2].1); // shortestPath: { <Bob> ?pred ?x . MINUS {…} }
    assert!(
        matches!(q.pattern, GraphPattern::Minus(_, _)),
        "{:?}",
        q.pattern
    );
    let q = sel(DOC_QUERIES[7].1); // bfsCount … WHERE{}
    assert_eq!(q.pattern, GraphPattern::Empty);
}

#[test]
fn doc_khop_enumerate_example() {
    // docs/en-us/SPARQL_QUERY.md:1388 is malformed upstream: its MINUS triple
    // `<Alice> <dislike>` lacks an object, so upstream's ANTLR parser rejects
    // it too. The verbatim text must fail cleanly; docs/zh-cn has the
    // intended query (`MINUS { <Alice> <不喜欢> ?x. }`).
    let verbatim = "SELECT (kHopEnumerate(<Alice>, ?x, true, 3, {<like>, <focus>}) AS ?y) \nWHERE \n{\n    <Francis> ?pred ?x.\n    MINUS { <Alice> <dislike> }\n    }";
    err(verbatim);
    let fixed = "SELECT (kHopEnumerate(<Alice>, ?x, true, 3, {<喜欢>, <关注>}) AS ?y) \nWHERE \n{\n    <Francis> ?pred ?x.\n    MINUS { <Alice> <不喜欢> ?x. }\n    }";
    let (func, args, _) = gfn(fixed);
    assert_eq!(func, GFnName::KHopEnumerate);
    assert_eq!(
        args,
        vec![
            iri("Alice"),
            var("x"),
            Bool(true),
            Int(3),
            ps(&["喜欢", "关注"], &[])
        ]
    );
}

#[test]
fn zh_cn_doc_variants_with_non_ascii_iris_parse() {
    // docs/zh-cn uses Chinese predicate IRIs and multi-line WHERE blocks.
    let (f, a, _) = gfn("select (cycleBoolean(?x, <Carol>, true, {<喜欢>}) as ?y)\nwhere\n{\n\t<Francis> <不喜欢> ?x .\n}");
    assert_eq!(f, GFnName::CycleBoolean);
    assert_eq!(a[3], ps(&["喜欢"], &[]));
    let (f, a, _) = gfn("SELECT (diameterEstimation({<喜欢>, <关注>, <不喜欢>}) AS ?y)WHERE{}");
    assert_eq!(f, GFnName::DiameterEstimation);
    assert_eq!(a, vec![ps(&["喜欢", "关注", "不喜欢"], &[])]);
}

#[test]
fn upstream_parser_test_path_queries_parse() {
    // scripts/test/parser_test/parser_q33.sql … q35.sql
    let (f, a, _) = gfn("select (cyclePath(<Eve>, <Carol>, true, {}) as ?y)\nwhere\n{\n\t\n}");
    assert_eq!(
        (f, a),
        (
            GFnName::CyclePath,
            vec![iri("Eve"), iri("Carol"), Bool(true), ps(&[], &[])]
        )
    );
    let (f, a, _) = gfn("select (shortestPathLen(<Francis>, ?x, true, {<喜欢>, <关注>}) as ?y)\nwhere\n{\n\t<Bob> ?pred ?x .\n\tMINUS { <Francis> <不喜欢> ?x . }\n}");
    assert_eq!(f, GFnName::ShortestPathLen);
    assert_eq!(a[0], iri("Francis"));
    let (f, a, _) = gfn("SELECT (kHopReachable(<Francis>, ?x, true, -1, {<喜欢>, <关注>}) AS ?y)\nWHERE\n{\n\t<Bob> ?pred ?x .\n\tMINUS { <Francis> <不喜欢> ?x . }\n}");
    assert_eq!(f, GFnName::KHopReachable);
    assert_eq!(a[3], Int(-1));
}

#[test]
fn doc_signatures_that_differ_from_the_grammar_follow_the_grammar() {
    // The docs write PR(directed, pred_set, alpha, maxIter, tol),
    // labelProp(directed, pred_set) and louvain(directed, pred_set, maxIter,
    // increase); upstream's grammar (authoritative) requires PR's retNum and
    // the named `key=` forms.
    let m = call_err("PR(true, {}, 0.85, 100, 0.0001)");
    assert_contains(
        &m,
        &[
            "PR expects PR(",
            "argument 3 must be an integerLiteral: found 0.85",
        ],
    );
    let m = call_err("PR(true, {}, 10, 0.85, 100, 0.0001)");
    assert_contains(
        &m,
        &["argument 4 must be the named argument alpha=numericLiteral, found 0.85"],
    );
    let m = call_err("labelProp(true, {})");
    assert_contains(
        &m,
        &[
            "labelProp expects",
            "missing argument 3 (maxIter=num_integer)",
        ],
    );
    let m = call_err("louvain(true, {}, 10, 0.1)");
    assert_contains(&m, &["argument 3 must be the named argument maxIter="]);
    // The grammar forms parse.
    assert_eq!(
        call("PR(true, {}, 10, alpha=0.85, maxIter=100, tol=0.0001)"),
        (
            GFnName::Pr,
            vec![
                Bool(true),
                ps(&[], &[]),
                Int(10),
                named_num("alpha", 0.85),
                named_int("maxIter", 100),
                named_num("tol", 0.0001)
            ]
        )
    );
    assert_eq!(
        call("labelProp(false, {<p>}, maxIter=5)"),
        (
            GFnName::LabelProp,
            vec![Bool(false), ps(&["p"], &[]), named_int("maxIter", 5)]
        )
    );
    assert_eq!(
        call("louvain(true, {}, maxIter=10, increase=0.001)"),
        (
            GFnName::Louvain,
            vec![
                Bool(true),
                ps(&[], &[]),
                named_int("maxIter", 10),
                named_num("increase", 0.001)
            ]
        )
    );
}

// ---- every grammar alternative ------------------------------------------

#[test]
fn every_advanced_function_alternative_parses_with_its_positional_shape() {
    use GFnName as F;
    let uv = || vec![var("u"), iri("v"), Bool(true), ps(&["p"], &[])];
    let cases: Vec<(&str, F, Vec<GArg>)> = vec![
        (
            "simpleCyclePath(?u, <v>, true, {<p>})",
            F::SimpleCyclePath,
            uv(),
        ),
        (
            "simpleCycleBoolean(?u, <v>, true, {<p>})",
            F::SimpleCycleBoolean,
            uv(),
        ),
        ("cyclePath(?u, <v>, true, {<p>})", F::CyclePath, uv()),
        ("cycleBoolean(?u, <v>, true, {<p>})", F::CycleBoolean, uv()),
        ("shortestPath(?u, <v>, true, {<p>})", F::ShortestPath, uv()),
        (
            "shortestPathLen(?u, <v>, true, {<p>})",
            F::ShortestPathLen,
            uv(),
        ),
        (
            "shortestPathCount(?u, <v>, true, {<p>})",
            F::ShortestPathCount,
            uv(),
        ),
        (
            "shortestPaths(?u, <v>, true, {<p>})",
            F::ShortestPaths,
            uv(),
        ),
        (
            "kHopReachable(?u, <v>, false, 3, {})",
            F::KHopReachable,
            vec![var("u"), iri("v"), Bool(false), Int(3), ps(&[], &[])],
        ),
        (
            "kHopReachablePath(?u, <v>, false, -1, {})",
            F::KHopReachablePath,
            vec![var("u"), iri("v"), Bool(false), Int(-1), ps(&[], &[])],
        ),
        (
            "kHopEnumerate(?u, <v>, true, 2, {})",
            F::KHopEnumerate,
            vec![var("u"), iri("v"), Bool(true), Int(2), ps(&[], &[])],
        ),
        (
            "kHopEnumerate(?u, <v>, true, 2, {}, 50)",
            F::KHopEnumerate,
            vec![
                var("u"),
                iri("v"),
                Bool(true),
                Int(2),
                ps(&[], &[]),
                Int(50),
            ],
        ),
        (
            "PPR(<u>, 2, {<p>}, 10)",
            F::Ppr,
            vec![iri("u"), Int(2), ps(&["p"], &[]), Int(10)],
        ),
        (
            "triangleCounting(false, {})",
            F::TriangleCounting,
            vec![Bool(false), ps(&[], &[])],
        ),
        (
            "closenessCentrality(?u, true, {})",
            F::ClosenessCentrality,
            vec![var("u"), Bool(true), ps(&[], &[])],
        ),
        (
            "bfsCount(?u, false, {})",
            F::BfsCount,
            vec![var("u"), Bool(false), ps(&[], &[])],
        ),
        (
            "PR(false, {}, -1, alpha=1, maxIter=0, tol=1e-6)",
            F::Pr,
            vec![
                Bool(false),
                ps(&[], &[]),
                Int(-1),
                named_num("alpha", 1.0),
                named_int("maxIter", 0),
                named_num("tol", 1e-6),
            ],
        ),
        (
            "SSSP(<u>, true, {})",
            F::Sssp,
            vec![iri("u"), Bool(true), ps(&[], &[])],
        ),
        (
            "SSSPLen(<u>, false, {<p>})",
            F::SsspLen,
            vec![iri("u"), Bool(false), ps(&["p"], &[])],
        ),
        (
            "labelProp(true, {}, maxIter=7)",
            F::LabelProp,
            vec![Bool(true), ps(&[], &[]), named_int("maxIter", 7)],
        ),
        ("WCC({})", F::Wcc, vec![ps(&[], &[])]),
        ("WCC({<p>, !<q>})", F::Wcc, vec![ps(&["p"], &["q"])]),
        (
            "clusterCoeff(?u, true, {})",
            F::ClusterCoeff,
            vec![var("u"), Bool(true), ps(&[], &[])],
        ),
        (
            "clusterCoeff(false, {<p>})",
            F::ClusterCoeff,
            vec![Bool(false), ps(&["p"], &[])],
        ),
        ("maximumKplex({})", F::MaximumKplex, vec![ps(&[], &[])]),
        (
            "maximumKplex({}, 3)",
            F::MaximumKplex,
            vec![ps(&[], &[]), Int(3)],
        ),
        (
            "coreTruss({<p>}, 1, 2)",
            F::CoreTruss,
            vec![ps(&["p"], &[]), Int(1), Int(2)],
        ),
        (
            "kHopCount(?u, true, 2, {})",
            F::KHopCount,
            vec![var("u"), Bool(true), Int(2), ps(&[], &[])],
        ),
        (
            "kHopNeighbor(?u, true, 2, {})",
            F::KHopNeighbor,
            vec![var("u"), Bool(true), Int(2), ps(&[], &[])],
        ),
        (
            "kHopNeighbor(?u, true, 2, {}, 100)",
            F::KHopNeighbor,
            vec![var("u"), Bool(true), Int(2), ps(&[], &[]), Int(100)],
        ),
        (
            "kHopAllNeighbors(<u>, false, 1, {}, -1)",
            F::KHopAllNeighbors,
            vec![iri("u"), Bool(false), Int(1), ps(&[], &[]), Int(-1)],
        ),
        (
            "louvain(false, {}, maxIter=3, increase=1)",
            F::Louvain,
            vec![
                Bool(false),
                ps(&[], &[]),
                named_int("maxIter", 3),
                named_num("increase", 1.0),
            ],
        ),
        (
            "confidenceDegree(1, 0.5, -2, +3, 1e-2, .5)",
            F::ConfidenceDegree,
            vec![Num(1.0), Num(0.5), Num(-2.0), Num(3.0), Num(0.01), Num(0.5)],
        ),
        (
            "diameterEstimation({})",
            F::DiameterEstimation,
            vec![ps(&[], &[])],
        ),
        (
            "betweennessCentrality(?u, false, {})",
            F::BetweennessCentrality,
            vec![var("u"), Bool(false), ps(&[], &[])],
        ),
        (
            "JaccardSimilarity(?u, {}, 1, 5)",
            F::JaccardSimilarity,
            vec![var("u"), ps(&[], &[]), Int(1), Int(5)],
        ),
        (
            "degreeCorrelation(?u, 2, {})",
            F::DegreeCorrelation,
            vec![var("u"), Int(2), ps(&[], &[])],
        ),
    ];
    let mut seen = std::collections::HashSet::new();
    for (text, func, args) in cases {
        assert_eq!(call(text), (func, args), "{text}");
        seen.insert(func);
    }
    // Every upstream alternative is covered.
    for f in GFnName::ALL {
        assert!(seen.contains(&f), "no case for {f}");
    }
}

#[test]
fn function_table_is_consistent() {
    let mut kws = std::collections::HashSet::new();
    for f in GFnName::ALL {
        assert!(kws.insert(f.keyword()), "duplicate keyword {}", f.keyword());
        assert_eq!(f.keyword(), f.name().to_ascii_uppercase());
        assert_eq!(GFnName::from_keyword(f.name()), Some(f));
        assert_eq!(
            GFnName::from_keyword(&f.keyword().to_ascii_lowercase()),
            Some(f)
        );
        assert!(!f.params().is_empty());
    }
    assert_eq!(GFnName::from_keyword("PFN"), None);
    assert_eq!(GFnName::from_keyword("CLUSTERINGCOEFF"), None);
    assert_eq!(
        GFnName::KHopEnumerate.signature(),
        "kHopEnumerate(varOrIri, varOrIri, booleanLiteral, integerLiteral, predSet [, integerLiteral])"
    );
    assert_eq!(
        GFnName::ClusterCoeff.signature(),
        "clusterCoeff([varOrIri, ] booleanLiteral, predSet)"
    );
    assert_eq!(
        GFnName::Pr.signature(),
        "PR(booleanLiteral, predSet, integerLiteral, alpha=numericLiteral, maxIter=num_integer, tol=numericLiteral)"
    );
    assert_eq!(GFnName::Wcc.signature(), "WCC(predSet)");
    let callable: Vec<GFnName> = GFnName::ALL
        .into_iter()
        .filter(|f| f.filter_callable())
        .collect();
    assert_eq!(
        callable,
        vec![
            GFnName::SimpleCycleBoolean,
            GFnName::CycleBoolean,
            GFnName::ShortestPathLen,
            GFnName::KHopReachable
        ]
    );
}

// ---- grammar edge cases --------------------------------------------------

#[test]
fn keywords_are_case_insensitive() {
    for spelling in [
        "shortestPath",
        "SHORTESTPATH",
        "shortestpath",
        "ShOrTeStPaTh",
    ] {
        let (f, _) = call(&format!("{spelling}(<a>, <b>, true, {{}})"));
        assert_eq!(f, GFnName::ShortestPath, "{spelling}");
    }
    // Named-argument keywords too; `key` is always the canonical spelling.
    let (_, a) = call("pr(TRUE, {}, 3, ALPHA=0.5, MaxIter=10, Tol=0.25)");
    assert_eq!(
        a[3..],
        [
            named_num("alpha", 0.5),
            named_int("maxIter", 10),
            named_num("tol", 0.25)
        ]
    );
    let (_, a) = call("LOUVAIN(false, {}, maxiter = 2, INCREASE = 0.5)");
    assert_eq!(
        a[2..],
        [named_int("maxIter", 2), named_num("increase", 0.5)]
    );
    // booleanLiteral: accepted in any case (upstream's 'true'/'false' tokens
    // are case-sensitive; SPARQL keywords are not).
    assert_eq!(call("WCC({})").0, GFnName::Wcc);
    assert_eq!(call("cycleBoolean(<a>, <b>, False, {})").1[2], Bool(false));
}

#[test]
fn pred_sets_empty_negated_mixed_and_prefixed() {
    let q = "PREFIX ex: <http://ex/>
             BASE <http://base/>
             SELECT (shortestPath(ex:Alice, <rel>, false,
                                  {<like>, !<dislike>, ex:focus, !ex:block, <http://abs/p>}) AS ?y)
             WHERE {}";
    let (f, a, _) = gfn(q);
    assert_eq!(f, GFnName::ShortestPath);
    assert_eq!(
        a,
        vec![
            iri("http://ex/Alice"),
            iri("http://base/rel"),
            Bool(false),
            ps(
                &["http://base/like", "http://ex/focus", "http://abs/p"],
                &["http://base/dislike", "http://ex/block"]
            ),
        ]
    );
    // `{}` and an all-negated set.
    assert_eq!(call("WCC({})").1, vec![ps(&[], &[])]);
    assert_eq!(call("WCC({!<a>, !<b>})").1, vec![ps(&[], &["a", "b"])]);
    // A predicate set is IRIs only.
    let m = call_err("WCC({a})");
    assert_contains(
        &m,
        &[
            "WCC expects WCC(predSet)",
            "expected an IRI or !IRI, found 'a'",
        ],
    );
    let m = call_err("WCC({?p})");
    assert_contains(&m, &["expected an IRI or !IRI, found ?p"]);
    let m = call_err("WCC({\"p\"})");
    assert_contains(&m, &["argument 1 must be a predSet"]);
    let m = call_err("WCC({<a> <b>})");
    assert_contains(&m, &["expected ',' or '}' in the predicate set, found <b>"]);
    let m = call_err("WCC({<a>,})");
    assert_contains(&m, &["expected an IRI or !IRI, found '}'"]);
    let m = call_err("WCC(<p>)");
    assert_contains(&m, &["argument 1 must be a predSet: expected '{'"]);
    // Prefixed names resolve through PREFIX; an undeclared prefix is an error.
    let m = call_err("WCC({nope:p})");
    assert_contains(&m, &["undefined prefix 'nope:'"]);
    let m = call_err("bfsCount(nope:a, true, {})");
    assert_contains(&m, &["argument 1", "undefined prefix 'nope:'"]);
}

#[test]
fn signed_and_large_integers() {
    let k = |lit: &str| match call(&format!("kHopCount(<a>, true, {lit}, {{}})")).1[2] {
        Int(n) => n,
        ref other => panic!("{other:?}"),
    };
    assert_eq!(k("0"), 0);
    assert_eq!(k("+7"), 7);
    assert_eq!(k("-1"), -1);
    assert_eq!(k("9223372036854775807"), i64::MAX);
    assert_eq!(k("-9223372036854775808"), i64::MIN);
    let m = call_err("kHopCount(<a>, true, 9223372036854775808, {})");
    assert_contains(
        &m,
        &[
            "argument 3 must be an integerLiteral",
            "out of the 64-bit integer range",
        ],
    );
    let m = call_err("kHopCount(<a>, true, 2.5, {})");
    assert_contains(&m, &["argument 3 must be an integerLiteral: found 2.5"]);
    let m = call_err("kHopCount(<a>, true, ?k, {})");
    assert_contains(&m, &["argument 3 must be an integerLiteral: found ?k"]);
    // num_integer slots are unsigned.
    let m = call_err("maximumKplex({}, -2)");
    assert_contains(
        &m,
        &["argument 2 must be an unsigned integer: found signed -2"],
    );
    let m = call_err("coreTruss({}, 1, +2)");
    assert_contains(&m, &["argument 3 must be an unsigned integer"]);
    let m = call_err("labelProp(true, {}, maxIter=-1)");
    assert_contains(&m, &["argument 3 (maxIter=num_integer): found signed -1"]);
    let m = call_err("labelProp(true, {}, maxIter=1.5)");
    assert_contains(&m, &["argument 3 (maxIter=num_integer): found 1.5"]);
    // numericLiteral: integers, decimals and doubles, signed.
    assert_eq!(
        call("PR(true, {}, 1, alpha=-.5, maxIter=1, tol=+2E3)").1[3],
        named_num("alpha", -0.5)
    );
    assert_eq!(
        call("PR(true, {}, 1, alpha=-.5, maxIter=1, tol=+2E3)").1[5],
        named_num("tol", 2000.0)
    );
    let m = call_err("confidenceDegree(1, 2, 3, 4, 5, 1e999)");
    assert_contains(
        &m,
        &["argument 6 must be a numericLiteral: 1e999 is out of range"],
    );
    let m = call_err("confidenceDegree(1, 2, 3, 4, 5, true)");
    assert_contains(&m, &["argument 6 must be a numericLiteral: found 'true'"]);
}

#[test]
fn named_arguments_must_appear_in_grammar_order() {
    let m = call_err("PR(true, {}, 10, maxIter=100, alpha=0.85, tol=0.1)");
    assert_contains(
        &m,
        &["argument 4 must be the named argument alpha=numericLiteral, found 'maxIter'"],
    );
    let m = call_err("PR(true, {}, 10, alpha 0.85, maxIter=100, tol=0.1)");
    assert_contains(&m, &["argument 4: expected '=' after alpha, found 0.85"]);
    let m = call_err("PR(true, {}, 10, alpha=0.85, maxIter=100)");
    assert_contains(&m, &["missing argument 6 (tol=numericLiteral)"]);
    let m = call_err("PR(true, {}, 10, alpha=, maxIter=100, tol=0.1)");
    assert_contains(&m, &["argument 4 (alpha=numericLiteral): found ','"]);
}

#[test]
fn missing_extra_and_mistyped_arguments_have_good_messages() {
    let m = call_err("shortestPath(?x, ?y)");
    assert_contains(
        &m,
        &[
            "shortestPath expects shortestPath(varOrIri, varOrIri, booleanLiteral, predSet)",
            "missing argument 3 (booleanLiteral)",
        ],
    );
    let m = call_err("WCC()");
    assert_contains(
        &m,
        &["WCC expects WCC(predSet): missing argument 1 (predSet)"],
    );
    let m = call_err("WCC({}, 3)");
    assert_contains(&m, &["too many arguments: expected ')' after argument 1"]);
    let m = call_err("kHopEnumerate(?u, ?v, true, 2, {}, 5, 6)");
    assert_contains(&m, &["too many arguments: expected ')' after argument 6"]);
    let m = call_err("cycleBoolean(?x, <Carol>, \"true\", {<like>})");
    assert_contains(
        &m,
        &["argument 3 must be a booleanLiteral (true or false), found the string literal \"true\""],
    );
    let m = call_err("shortestPath(\"a\", ?y, true, {})");
    assert_contains(
        &m,
        &["argument 1 must be a varOrIri (a ?variable or an IRI), found the string literal \"a\""],
    );
    let m = call_err("shortestPath(_:b, ?y, true, {})");
    assert_contains(&m, &["argument 1 must be a varOrIri", "found _:b"]);
    let m = call_err("shortestPath(?x ?y, true, {})");
    assert_contains(&m, &["expected ',' before argument 2, found ?y"]);
    let m = call_err("shortestPath(?x, ?y, true, {},)");
    assert_contains(&m, &["too many arguments"]);
    let m = call_err("shortestPath(?x, ?y, true, {} AS ?z");
    assert_contains(&m, &["expected ')' after argument 4, found 'AS'"]);
    // clusterCoeff's leading varOrIri is optional; anything else is checked.
    let m = call_err("clusterCoeff(?u, {})");
    assert_contains(
        &m,
        &[
            "clusterCoeff expects clusterCoeff([varOrIri, ] booleanLiteral, predSet)",
            "argument 2 must be a booleanLiteral",
        ],
    );
    let m = call_err("clusterCoeff({})");
    assert_contains(&m, &["argument 1 must be a booleanLiteral"]);
}

// ---- where the calls are accepted ----------------------------------------

#[test]
fn filter_callable_functions_nest_inside_expressions() {
    // FILTER: upstream's comp-tree builder accepts these four.
    let q = sel("SELECT ?x WHERE { ?x <p> ?y FILTER(shortestPathLen(?x, ?y, true, {}) < 3) }");
    let GraphPattern::Filter(fs, _) = &q.pattern else {
        panic!("{:?}", q.pattern)
    };
    match &fs[0] {
        Expr::Compare(CompareOp::Lt, lhs, _) => assert!(
            matches!(
                **lhs,
                Expr::GraphFn {
                    func: GFnName::ShortestPathLen,
                    ..
                }
            ),
            "{lhs:?}"
        ),
        other => panic!("{other:?}"),
    }
    for f in [
        "simpleCycleBoolean(?x, ?y, false, {})",
        "cycleBoolean(?x, ?y, true, {<p>})",
        "kHopReachable(?x, ?y, true, 2, {})",
    ] {
        let q = sel(&format!("SELECT ?x WHERE {{ ?x <p> ?y FILTER({f}) }}"));
        let GraphPattern::Filter(fs, _) = &q.pattern else {
            panic!()
        };
        assert!(matches!(fs[0], Expr::GraphFn { .. }), "{f}: {:?}", fs[0]);
        // A bare function-call constraint (no extra parentheses) as well.
        let q = sel(&format!("SELECT ?x WHERE {{ ?x <p> ?y FILTER {f} }}"));
        assert!(matches!(q.pattern, GraphPattern::Filter(_, _)), "{f}");
    }
    // BIND / HAVING / ORDER BY use the same expression builder upstream.
    let q = sel("SELECT * WHERE { ?x <p> ?y BIND(cycleBoolean(?x, ?y, true, {}) AS ?c) }");
    assert!(matches!(
        q.pattern,
        GraphPattern::Extend(
            _,
            _,
            Expr::GraphFn {
                func: GFnName::CycleBoolean,
                ..
            }
        )
    ));
    let q = sel("SELECT ?x WHERE { ?x <p> ?y } ORDER BY DESC(shortestPathLen(?x, ?y, false, {}))");
    assert!(matches!(q.order_by[0].expr, Expr::GraphFn { .. }));
    let q = sel(
        "SELECT ?x WHERE { ?x <p> ?y } GROUP BY ?x HAVING (kHopReachable(?x, <b>, true, 1, {}))",
    );
    assert!(matches!(q.having[0], Expr::GraphFn { .. }));
    // Inside a compound projection (upstream's CompTree projection path).
    let it = items("SELECT (1 + shortestPathLen(?x, ?y, true, {}) AS ?n) WHERE { ?x <p> ?y }");
    assert!(matches!(it[0].0, Expr::Arith(ArithOp::Add, _, _)));
}

#[test]
fn other_functions_only_as_the_whole_projection() {
    let m = err("SELECT ?x WHERE { ?x <p> ?y FILTER(shortestPath(?x, ?y, true, {})) }");
    assert_contains(
        &m,
        &["shortestPath(…) is only allowed as the whole expression of a SELECT projection"],
    );
    let m = err("SELECT * WHERE { ?x <p> ?y BIND(PPR(?x, 2, {}, 10) AS ?r) }");
    assert_contains(
        &m,
        &[
            "PPR(…) is only allowed as the whole expression",
            "rs:PPR(…)",
        ],
    );
    let m = err("SELECT (STR(kHopCount(?x, true, 2, {})) AS ?s) WHERE { ?x <p> ?y }");
    assert_contains(&m, &["kHopCount(…) is only allowed"]);
    // As the whole projection, the call must not be followed by an operator.
    let m = err("SELECT (shortestPath(?x, ?y, true, {}) + 1 AS ?v) WHERE { ?x <p> ?y }");
    assert_contains(
        &m,
        &[
            "shortestPath(…) must be the whole projected expression",
            "found '+' after the call",
        ],
    );
    let m = err("SELECT (shortestPathLen(?x, ?y, true, {}) > 1 AS ?v) WHERE { ?x <p> ?y }");
    assert_contains(&m, &["must be the whole projected expression"]);
}

#[test]
fn several_projections_and_sub_selects() {
    let it = items(
        "SELECT ?x (shortestPath(?x, <b>, true, {}) AS ?p) (WCC({}) AS ?w) (COUNT(?x) AS ?c)
         WHERE { ?x <p> ?y }",
    );
    assert!(
        matches!(it[0], (Expr::GraphFn { func: GFnName::ShortestPath, .. }, ref v) if v == "p")
    );
    assert!(matches!(it[1], (Expr::GraphFn { func: GFnName::Wcc, .. }, ref v) if v == "w"));
    assert!(matches!(it[2].0, Expr::Aggregate { .. }));
    let q = sel("SELECT * WHERE { { SELECT (bfsCount(<a>, true, {}) AS ?b) WHERE {} } }");
    assert!(matches!(q.pattern, GraphPattern::SubSelect(_)));
}

// ---- PFN ---------------------------------------------------------------

fn pfn(q: &str) -> (String, String) {
    match items(q).pop().unwrap().0 {
        Expr::Pfn { name, json } => (name, json),
        other => panic!("expected Expr::Pfn, got {other:?}"),
    }
}

#[test]
fn pfn_calls_capture_the_raw_json_text() {
    // The example in upstream's src/Query/PfnAnalysisQuery.cpp.
    let (name, json) = pfn(
        r#"SELECT (PFN.xxx({"src":"<Alice>", "dst":"<Bob>", "direct":"true", "predSet":["<喜欢>","<关注>"]}) AS ?z) WHERE {}"#,
    );
    assert_eq!(name, "xxx");
    assert_eq!(
        json,
        r#"{"src":"<Alice>","dst":"<Bob>","direct":"true","predSet":["<喜欢>","<关注>"]}"#
    );
    // Empty object, lower-case keyword, the grammar's `PFN name` spelling,
    // whitespace/comments between tokens dropped, lexemes (escapes, quote
    // style) kept verbatim.
    assert_eq!(
        pfn("SELECT (pfn.myFun({ }) AS ?z) WHERE {}"),
        ("myFun".into(), "{}".into())
    );
    assert_eq!(
        pfn("SELECT (PFN f({ \"k\" : [ \"a\" ,\n \"b\" ] } # note\n) AS ?z) WHERE {}"),
        ("f".into(), r#"{"k":["a","b"]}"#.into())
    );
    assert_eq!(
        pfn(r#"SELECT (PFN.f({"k":"a\"b\\c", 'q':'v'}) AS ?z) WHERE {}"#).1,
        r#"{"k":"a\"b\\c",'q':'v'}"#
    );
}

#[test]
fn pfn_errors() {
    let m = err(r#"SELECT (PFN.f({"k": 3}) AS ?z) WHERE {}"#);
    assert_contains(
        &m,
        &[
            "PFN.f expects one jsonVars argument",
            "values must be strings or [string, …] lists, found 3",
        ],
    );
    let m = err(r#"SELECT (PFN.f({"k"}) AS ?z) WHERE {}"#);
    assert_contains(&m, &["expected ':' after the key, found '}'"]);
    let m = err(r#"SELECT (PFN.f({k: "v"}) AS ?z) WHERE {}"#);
    assert_contains(&m, &["expected a string key"]);
    let m = err(r#"SELECT (PFN.f({"k": ["a" "b"]}) AS ?z) WHERE {}"#);
    assert_contains(&m, &["expected ',' or ']' in the value list"]);
    let m = err(r#"SELECT (PFN.f({"k": "v"@en}) AS ?z) WHERE {}"#);
    assert_contains(&m, &["values must be strings"]);
    let m = err("SELECT (PFN.f() AS ?z) WHERE {}");
    assert_contains(&m, &["expected a JSON object '{…}', found ')'"]);
    let m = err(r#"SELECT (PFN.f({}, {}) AS ?z) WHERE {}"#);
    assert_contains(&m, &["expected ')' after it, found ','"]);
    let m = err(r#"SELECT ?x WHERE { ?x <p> ?y FILTER(PFN.f({})) }"#);
    assert_contains(
        &m,
        &["PFN.f(…) is only allowed as the whole expression of a SELECT projection"],
    );
    let m = err(r#"SELECT (PFN.f({}) + 1 AS ?z) WHERE {}"#);
    assert_contains(&m, &["PFN.f(…) must be the whole projected expression"]);
}

// ---- legacy gstore-rust names --------------------------------------------

#[test]
fn legacy_shaped_calls_to_colliding_names_point_to_rs() {
    let cases = [
        ("SSSP(?a, ?b)", "rs:SSSP(?a, ?b)"),
        ("SHORTESTPATHS(?a,?b)", "rs:SHORTESTPATHS(?a,?b)"),
        ("PPR(?a, ?b)", "rs:PPR(?a, ?b)"),
        ("WCC(?x)", "rs:WCC(?x)"),
        ("sssp(<a>, <b>)", "rs:sssp(<a>, <b>)"),
        ("TRIANGLECOUNTING()", "rs:TRIANGLECOUNTING()"),
        (
            "khopReachable(<a>, <b>, 3)",
            "rs:khopReachable(<a>, <b>, 3)",
        ),
    ];
    for (legacy, suggestion) in cases {
        let m = call_err(legacy);
        assert_contains(
            &m,
            &[
                "legacy gstore-rust built-in",
                suggestion,
                "the bare name is upstream gStore's",
            ],
        );
    }
    // In FILTER / BIND as well.
    let m = err("SELECT * WHERE { ?n <p> ?o FILTER(SSSPLEN(<a>, <d>) = 3) }");
    assert_contains(&m, &["rs:SSSPLEN(<a>, <d>)"]);
    let m = err("SELECT * WHERE { ?n <p> ?o BIND(CYCLEBOOLEAN() AS ?c) }");
    assert_contains(&m, &["rs:CYCLEBOOLEAN()"]);
    // Not legacy-shaped either: the upstream error plus a pointer to rs:.
    let m = call_err("SSSP(<a>, true, {<p>}, 5)");
    assert_contains(
        &m,
        &[
            "SSSP expects SSSP(varOrIri, booleanLiteral, predSet): too many arguments",
            "rs:SSSP(…)",
        ],
    );
}

#[test]
fn every_colliding_legacy_name_suggests_rs() {
    for &name in RS_FUNCTIONS {
        let Some(func) = GFnName::from_keyword(name) else {
            continue;
        };
        let arity = gstore::parser::sparql::builtins::lookup(name)
            .expect("legacy names are built-ins")
            .arity;
        let args = vec!["<x>"; arity.min].join(", ");
        let m = call_err(&format!("{name}({args})"));
        assert_contains(&m, &[&format!("rs:{name}({args})"), func.name()]);
    }
}

#[test]
fn non_colliding_legacy_bare_names_keep_working() {
    let it = items(
        "SELECT (CLUSTERINGCOEFF() AS ?c) (KHOPENUMERATEPATH(<a>, 2) AS ?k)
                (SIMILARITY(\"ab\", \"ac\") AS ?s) WHERE {}",
    );
    let names: Vec<(&str, usize)> = it
        .iter()
        .map(|(e, _)| match e {
            Expr::Builtin(n, a) => (n.as_str(), a.len()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        vec![
            ("CLUSTERINGCOEFF", 0),
            ("KHOPENUMERATEPATH", 2),
            ("SIMILARITY", 2)
        ]
    );
}

// ---- rs: and IRI function calls ------------------------------------------

fn rs(local: &str) -> String {
    format!("{RS_NAMESPACE}{local}")
}

#[test]
fn rs_calls_parse_anywhere_an_expression_is_allowed() {
    let it = items(
        "SELECT (rs:sssp(<a>, <b>) AS ?p) (rs:SSSPLEN(?a, ?b) + 1 AS ?n) WHERE { ?a <p> ?b }",
    );
    assert_eq!(
        it[0].0,
        Expr::IriCall {
            iri: rs("sssp"),
            args: vec![
                Expr::Const(gstore::Term::iri("a")),
                Expr::Const(gstore::Term::iri("b"))
            ]
        }
    );
    assert!(matches!(&it[1].0, Expr::Arith(ArithOp::Add, l, _)
        if matches!(&**l, Expr::IriCall { iri, args } if *iri == rs("SSSPLEN") && args.len() == 2)));
    // FILTER (parenthesised and bare), BIND, ORDER BY, HAVING, GROUP BY.
    let q = sel("SELECT ?x (COUNT(?y) AS ?c) WHERE {
            ?x <p> ?y
            FILTER(rs:WCC(?x, ?y))
            FILTER rs:KHOPREACHABLE(?x, ?y, 2)
            BIND(rs:PPR(?x, ?y) AS ?s)
         } GROUP BY (rs:BFSCOUNT(?x)) HAVING(rs:TRIANGLECOUNTING() >= 0)
           ORDER BY (rs:DIAMETERESTIMATION())");
    let GraphPattern::Filter(fs, inner) = &q.pattern else {
        panic!("{:?}", q.pattern)
    };
    assert_eq!(fs.len(), 2);
    assert!(fs.iter().all(|f| matches!(f, Expr::IriCall { .. })));
    assert!(matches!(&**inner, GraphPattern::Extend(_, v, Expr::IriCall { .. }) if v == "s"));
    assert!(matches!(q.group_by[0], Expr::IriCall { .. }));
    assert!(matches!(q.having[0], Expr::Compare(CompareOp::Ge, _, _)));
    assert!(matches!(q.order_by[0].expr, Expr::IriCall { .. }));
    // The full IRI works the same, and names resolve case-insensitively.
    let it = items("SELECT (<urn:gstore-rust:fn:SSSP>(?a, ?b) AS ?p) WHERE {}");
    assert!(matches!(&it[0].0, Expr::IriCall { iri, .. } if rs_function_name(iri) == Some("SSSP")));
    assert_eq!(rs_function_name(&rs("khopCount")), Some("KHOPCOUNT"));
    assert_eq!(rs_function_name(&rs("nope")), None);
    assert_eq!(rs_function_name("http://ex/SSSP"), None);
    // Every legacy name is reachable.
    for name in RS_FUNCTIONS {
        assert_eq!(
            rs_function_name(&rs(&name.to_ascii_lowercase())),
            Some(*name)
        );
    }
    // Function calls are also GROUP BY / ORDER BY keys without parentheses.
    let q = sel("PREFIX ex: <http://ex/>
         SELECT ?x WHERE { ?x <p> ?y }
         GROUP BY rs:WCC(?x) ?x <http://ex/f>(?x)
         ORDER BY rs:BFSCOUNT(?x) DESC(?x) ex:g(?x)
         LIMIT 3");
    assert_eq!(q.group_by.len(), 3);
    assert!(matches!(&q.group_by[0], Expr::IriCall { iri, .. } if *iri == rs("WCC")));
    assert!(matches!(q.group_by[1], Expr::Var(_)));
    assert!(matches!(&q.group_by[2], Expr::IriCall { iri, .. } if iri == "http://ex/f"));
    assert_eq!(q.order_by.len(), 3);
    assert!(matches!(&q.order_by[0].expr, Expr::IriCall { iri, .. } if *iri == rs("BFSCOUNT")));
    assert!(q.order_by[1].descending);
    assert!(matches!(&q.order_by[2].expr, Expr::IriCall { iri, .. } if iri == "http://ex/g"));
    assert_eq!(q.limit, Some(3));
}

#[test]
fn rs_calls_are_checked_against_the_legacy_signatures() {
    let m = call_err("rs:SSSP(?a)");
    assert_contains(&m, &["rs:SSSP expects exactly 2 arguments, got 1"]);
    let m = call_err("rs:BFSCOUNT()");
    assert_contains(&m, &["rs:BFSCOUNT expects 1 to 2 arguments, got 0"]);
    let m = call_err("rs:nope(?a)");
    assert_contains(&m, &["unknown gstore-rust function rs:nope", "SSSP"]);
    let m = call_err("<urn:gstore-rust:fn:STR>(?a)");
    assert_contains(
        &m,
        &["unknown gstore-rust function <urn:gstore-rust:fn:STR>"],
    );
}

#[test]
fn other_iri_function_calls_parse_too() {
    let it = items(
        "PREFIX ex: <http://ex/>
         SELECT (<http://ex/f>(?x, 1) AS ?a) (ex:g() AS ?b) WHERE { ?x <p> ?y }",
    );
    assert!(
        matches!(&it[0].0, Expr::IriCall { iri, args } if iri == "http://ex/f" && args.len() == 2)
    );
    assert!(
        matches!(&it[1].0, Expr::IriCall { iri, args } if iri == "http://ex/g" && args.is_empty())
    );
    // An IRI not followed by '(' is still a constant.
    let q = sel("SELECT * WHERE { ?x <p> ?y FILTER(?y = <http://ex/o>) }");
    let GraphPattern::Filter(fs, _) = &q.pattern else {
        panic!()
    };
    assert!(matches!(&fs[0], Expr::Compare(CompareOp::Eq, _, r) if matches!(**r, Expr::Const(_))));
}

#[test]
fn user_prefix_rs_overrides_the_predeclared_one() {
    let it = items(
        "PREFIX rs: <http://other/>
         SELECT (rs:SSSP(?a) AS ?p) (rs:nope() AS ?q) WHERE { ?a rs:knows ?b }",
    );
    assert!(
        matches!(&it[0].0, Expr::IriCall { iri, args } if iri == "http://other/SSSP" && args.len() == 1)
    );
    assert!(matches!(&it[1].0, Expr::IriCall { iri, .. } if iri == "http://other/nope"));
    let q = sel("PREFIX rs: <http://other/> SELECT * WHERE { ?a rs:knows ?b }");
    let GraphPattern::Bgp(tps) = &q.pattern else {
        panic!()
    };
    assert_eq!(
        tps[0].predicate,
        PatternTerm::Term(gstore::Term::iri("http://other/knows"))
    );
    // Without the override, rs: is the predeclared namespace everywhere.
    let q = sel("SELECT * WHERE { ?a rs:knows ?b }");
    let GraphPattern::Bgp(tps) = &q.pattern else {
        panic!()
    };
    assert_eq!(
        tps[0].predicate,
        PatternTerm::Term(gstore::Term::iri(rs("knows")))
    );
}

// ---- evaluation staging --------------------------------------------------

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .
<http://ex/b> <http://ex/p> <http://ex/c> .
";

fn query_err(db: &mut Database, q: &str) -> String {
    match db.select(q) {
        Err(e) => e.to_string(),
        Ok(rs) => panic!("`{q}` should fail, got {rs:?}"),
    }
}

#[test]
fn advanced_functions_and_pfn_are_a_clean_not_wired_yet_error() {
    let mut db = Database::build_from_str("gfun_parse", DATA).unwrap();
    let m = query_err(
        &mut db,
        "SELECT (shortestPath(<http://ex/a>, <http://ex/c>, true, {}) AS ?p) WHERE {}",
    );
    assert_contains(
        &m,
        &["advanced function evaluation not wired yet: shortestPath"],
    );
    let m = query_err(
        &mut db,
        "SELECT ?x WHERE { ?x <http://ex/p> ?y FILTER(cycleBoolean(?x, ?y, true, {})) }",
    );
    assert_contains(&m, &["not wired yet: cycleBoolean"]);
    let m = query_err(&mut db, "SELECT (PFN.f({\"k\":\"v\"}) AS ?z) WHERE {}");
    assert_contains(&m, &["PFN not supported yet: PFN.f"]);
    let m = query_err(
        &mut db,
        "ASK { ?x <http://ex/p> ?y FILTER(kHopReachable(?x, ?y, true, 1, {})) }",
    );
    assert_contains(&m, &["not wired yet: kHopReachable"]);
}

#[test]
fn advanced_functions_in_an_update_where_are_a_clean_error() {
    let mut db = Database::build_from_str("gfun_parse_update", DATA).unwrap();
    let before = db.triple_num();
    for (q, want) in [
        (
            "DELETE { ?x <http://ex/p> ?y } WHERE { ?x <http://ex/p> ?y \
             FILTER(shortestPathLen(?x, ?y, true, {}) = 1) }",
            "not wired yet: shortestPathLen",
        ),
        (
            "INSERT { ?x <http://ex/q> ?y } WHERE { ?x <http://ex/p> ?y \
             OPTIONAL { ?y <http://ex/p> ?z FILTER(kHopReachable(?x, ?z, true, 2, {})) } }",
            "not wired yet: kHopReachable",
        ),
    ] {
        let e = match db.query(q) {
            Err(e) => e.to_string(),
            Ok(r) => panic!("`{q}` should fail, got {r:?}"),
        };
        assert_contains(&e, &[want]);
    }
    assert_eq!(db.triple_num(), before);
}

#[test]
fn iri_calls_reach_functions_registered_under_their_iri() {
    let mut db = Database::build_from_str("gfun_parse_iri_fn", DATA).unwrap();
    db.register_function("http://ex/fn/double", |args| {
        Some(gstore::Value::Int(args.first()?.as_f64()? as i64 * 2))
    });
    for q in [
        "SELECT (<http://ex/fn/double>(21) AS ?v) WHERE {}",
        "PREFIX f: <http://ex/fn/> SELECT (f:double(21) AS ?v) WHERE {}",
        "PREFIX f: <http://ex/fn/> SELECT (f:DOUBLE(21) AS ?v) WHERE {}",
    ] {
        let rs = db.select(q).unwrap();
        let cell = rs.rows[0][0].clone().unwrap_or_default();
        assert!(cell.starts_with("\"42\""), "{q}: {cell}");
    }
    // Unregistered IRI: evaluates to unbound.
    let rs = db
        .select("SELECT (<http://ex/fn/none>(1) AS ?v) WHERE {}")
        .unwrap();
    assert_eq!(rs.rows, vec![vec![None]]);
}

#[test]
fn rs_calls_evaluate_with_the_legacy_implementations() {
    let mut db = Database::build_from_str("gfun_parse_rs", DATA).unwrap();
    let rs = db
        .select("SELECT (rs:SSSPLEN(<http://ex/a>, <http://ex/c>) AS ?l) (rs:wcc(<http://ex/a>, <http://ex/c>) AS ?w) WHERE {}")
        .unwrap();
    assert_eq!(rs.rows.len(), 1);
    let row: Vec<String> = rs.rows[0]
        .iter()
        .map(|c| c.clone().unwrap_or_default())
        .collect();
    assert!(row[0].starts_with("\"2\""), "{row:?}");
    assert!(row[1].starts_with("\"true\""), "{row:?}");
    // Inside FILTER.
    let rs = db
        .select("SELECT ?x WHERE { ?x <http://ex/p> ?y FILTER(rs:SHORTESTPATHLEN(?x, <http://ex/c>) = 1) }")
        .unwrap();
    assert_eq!(rs.rows, vec![vec![Some("<http://ex/b>".to_string())]]);
}
