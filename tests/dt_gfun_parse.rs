//! Parsing of upstream gStore's advanced graph functions (`SPARQL.g4`
//! `builtInCall` alternatives), `PFN.name({json})` calls, IRI function calls
//! and the `rs:` namespace of the gstore-rust legacy graph built-ins.
//!
//! Golden inputs: every advanced-function query in upstream
//! `docs/en-us/SPARQL_QUERY.md` (17 queries, extracted with a regex over its
//! fenced code blocks; listed in `DOC_QUERIES` with their source lines), plus
//! the zh-cn variants of the same examples.

use gstore::parser::sparql::ast::{
    Expr, GArg, GFnName, GNum, GraphPattern, OrderCondition, Projection, Query, SelectItem,
    SelectQuery, VarOrIri,
};
use gstore::parser::sparql::parse;
use gstore::{Database, GStoreError, QueryResult};

// ---- helpers ---------------------------------------------------------------

fn sel(q: &str) -> SelectQuery {
    match parse(q) {
        Ok(Query::Select(s)) => s,
        Ok(other) => panic!("expected SELECT, got {other:?}"),
        Err(e) => panic!("failed to parse {q:?}: {e}"),
    }
}

/// The expression of projection item `i`.
fn item(q: &SelectQuery, i: usize) -> &Expr {
    match &q.projection {
        Projection::Items(items) => match &items[i] {
            SelectItem::Expr(e, _) => e,
            other => panic!("item {i} is not an expression: {other:?}"),
        },
        Projection::All => panic!("SELECT *"),
    }
}

/// The advanced-function call that is the first projection item of `q`.
fn gfn(q: &str) -> (GFnName, Vec<GArg>) {
    let s = sel(q);
    match item(&s, 0) {
        Expr::GraphFn { func, args } => (*func, args.clone()),
        other => panic!("expected GraphFn in {q:?}, got {other:?}"),
    }
}

/// `SELECT (<call> AS ?y) WHERE {}` → its GraphFn.
fn call(c: &str) -> (GFnName, Vec<GArg>) {
    gfn(&format!("SELECT ({c} AS ?y) WHERE {{}}"))
}

/// The parse-error message of `q` (panics if it parses).
fn err(q: &str) -> String {
    match parse(q) {
        Err(GStoreError::SparqlParse(m)) => m,
        Err(other) => panic!("expected a SPARQL parse error for {q:?}, got {other:?}"),
        Ok(ok) => panic!("expected {q:?} to be rejected, parsed as {ok:?}"),
    }
}

/// The parse-error message of `SELECT (<call> AS ?y) WHERE {}`.
fn call_err(c: &str) -> String {
    err(&format!("SELECT ({c} AS ?y) WHERE {{}}"))
}

fn var(v: &str) -> GArg {
    GArg::Node(VarOrIri::Var(v.into()))
}

fn iri(i: &str) -> GArg {
    GArg::Node(VarOrIri::Iri(i.into()))
}

fn preds(include: &[&str], exclude: &[&str]) -> GArg {
    GArg::PredSet {
        include: include.iter().map(|s| s.to_string()).collect(),
        exclude: exclude.iter().map(|s| s.to_string()).collect(),
    }
}

fn named_int(k: &str, v: i64) -> GArg {
    GArg::Named {
        key: k.into(),
        value: GNum::Int(v),
    }
}

fn named_num(k: &str, v: f64) -> GArg {
    GArg::Named {
        key: k.into(),
        value: GNum::Num(v),
    }
}

/// The FILTER expressions directly under the WHERE pattern.
fn filters(gp: &GraphPattern) -> &[Expr] {
    match gp {
        GraphPattern::Filter(fs, _) => fs,
        other => panic!("expected a FILTER, got {other:?}"),
    }
}

// ---- 1. upstream documentation examples -------------------------------------

/// Every advanced-function query in upstream `docs/en-us/SPARQL_QUERY.md`
/// (line of the query in that file, query text verbatim).
const DOC_QUERIES: [(u32, &str); 17] = [
    (
        1002,
        "select (cycleBoolean(?x, <Carol>, true, {<like>}) as ?y)where{\t<Francis> <dislike> ?x .}",
    ),
    (
        1014,
        "SELECT (cyclePath(?x, <Carol>, true, {<like>}) as ?y)\nWHERE\n{\n\t<Francis> <dislike> ?x .\n}",
    ),
    (
        1096,
        "SELECT (shortestPath(<Francis>, ?x, true, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x . }\n     }",
    ),
    (
        1157,
        "SELECT (shortestPathLen(<Francis>, ?x, true, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x .}\n     }",
    ),
    (
        1269,
        "SELECT (kHopReachable(<Francis>, ?x, true, 2, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x . }\n     }",
    ),
    (
        1288,
        "SELECT (kHopReachable(<Francis>, ?x, true, -1, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x . }\n     }",
    ),
    (
        1303,
        "SELECT (kHopReachablePath(<Francis>, ?x, true, -1, {<like>, <focus>}) AS ?y)\nWHERE{\t<Bob> ?pred ?x .\t\n      MINUS { <Francis> <dislike> ?x . }\n     }",
    ),
    (
        1389,
        "SELECT (kHopEnumerate(<Alice>, ?x, true, 3, {<like>, <focus>}) AS ?y) \nWHERE \n{\n    <Francis> ?pred ?x.\n    MINUS { <Alice> <dislike> }\n    }",
    ),
    (
        1587,
        "SELECT(bfsCount(<Alice>,true,{<like>,<focus>,<dislike>}) AS ?y)\nWHERE{}",
    ),
    (
        1620,
        "SELECT (diameterEstimation({<likes>, <follows>, <dislikes>}) AS ?y)WHERE{}",
    ),
    (1707, "select (PPR(?x, -1, {}, 3) as ?y)\nwhere\n{\n\t<Bob> ?pred ?x .\n}"),
    (
        1748,
        "SELECT (closenessCentrality(<Alice>, false, {<like>, <focus>}) AS ?x) WHERE{}",
    ),
    (
        1779,
        "SELECT (closenessCentrality(<Alice>, true, {<like>, <focus>}) AS ?x) WHERE{}",
    ),
    (1834, "select (triangleCounting(true, {<like>}) as ?y) where {}"),
    (
        1870,
        "SELECT (betweennessCentrality(<Eve>, true, {<likes>, <follows>, <dislikes>}) AS ?y) WHERE{}",
    ),
    (2034, "SELECT (JaccardSimilarity(<Alice>, {}, 2, 10) AS ?y) WHERE{}"),
    (2074, "SELECT (degreeCorrelation(<Alice>, -1, {}) AS ?y) WHERE{}"),
];

/// The expected AST of each documented call (same order as `DOC_QUERIES`).
fn doc_expected() -> Vec<(GFnName, Vec<GArg>)> {
    use GFnName::*;
    let like_focus = || preds(&["like", "focus"], &[]);
    vec![
        (
            CycleBoolean,
            vec![
                var("x"),
                iri("Carol"),
                GArg::Bool(true),
                preds(&["like"], &[]),
            ],
        ),
        (
            CyclePath,
            vec![
                var("x"),
                iri("Carol"),
                GArg::Bool(true),
                preds(&["like"], &[]),
            ],
        ),
        (
            ShortestPath,
            vec![iri("Francis"), var("x"), GArg::Bool(true), like_focus()],
        ),
        (
            ShortestPathLen,
            vec![iri("Francis"), var("x"), GArg::Bool(true), like_focus()],
        ),
        (
            KHopReachable,
            vec![
                iri("Francis"),
                var("x"),
                GArg::Bool(true),
                GArg::Int(2),
                like_focus(),
            ],
        ),
        (
            KHopReachable,
            vec![
                iri("Francis"),
                var("x"),
                GArg::Bool(true),
                GArg::Int(-1),
                like_focus(),
            ],
        ),
        (
            KHopReachablePath,
            vec![
                iri("Francis"),
                var("x"),
                GArg::Bool(true),
                GArg::Int(-1),
                like_focus(),
            ],
        ),
        (
            KHopEnumerate,
            vec![
                iri("Alice"),
                var("x"),
                GArg::Bool(true),
                GArg::Int(3),
                like_focus(),
            ],
        ),
        (
            BfsCount,
            vec![
                iri("Alice"),
                GArg::Bool(true),
                preds(&["like", "focus", "dislike"], &[]),
            ],
        ),
        (
            DiameterEstimation,
            vec![preds(&["likes", "follows", "dislikes"], &[])],
        ),
        (
            Ppr,
            vec![var("x"), GArg::Int(-1), preds(&[], &[]), GArg::Int(3)],
        ),
        (
            ClosenessCentrality,
            vec![iri("Alice"), GArg::Bool(false), like_focus()],
        ),
        (
            ClosenessCentrality,
            vec![iri("Alice"), GArg::Bool(true), like_focus()],
        ),
        (
            TriangleCounting,
            vec![GArg::Bool(true), preds(&["like"], &[])],
        ),
        (
            BetweennessCentrality,
            vec![
                iri("Eve"),
                GArg::Bool(true),
                preds(&["likes", "follows", "dislikes"], &[]),
            ],
        ),
        (
            JaccardSimilarity,
            vec![iri("Alice"), preds(&[], &[]), GArg::Int(2), GArg::Int(10)],
        ),
        (
            DegreeCorrelation,
            vec![iri("Alice"), GArg::Int(-1), preds(&[], &[])],
        ),
    ]
}

#[test]
fn every_documented_advanced_query_parses_to_the_expected_ast() {
    for ((line, q), expected) in DOC_QUERIES.iter().zip(doc_expected()) {
        if *line == 1389 {
            // Upstream doc bug: `MINUS { <Alice> <dislike> }` is a triple with
            // no object, which upstream's grammar rejects as well. The call
            // itself is fine — the error is about the triple, not the function
            // — and the corrected query (the zh-cn doc's version) parses.
            let e = err(q);
            assert!(
                !e.to_lowercase().contains("khopenumerate"),
                "line {line}: {e}"
            );
            let fixed = q.replace("<Alice> <dislike> }", "<Alice> <dislike> ?x . }");
            assert_eq!(gfn(&fixed), expected, "line {line} (corrected)");
            continue;
        }
        assert_eq!(gfn(q), expected, "doc query at SPARQL_QUERY.md:{line}");
    }
}

#[test]
fn documented_queries_keep_their_where_clause_and_alias() {
    let s = sel(DOC_QUERIES[2].1);
    assert_eq!(s.result_vars(), vec!["y".to_string()]);
    assert!(
        matches!(s.pattern, GraphPattern::Minus(_, _)),
        "{:?}",
        s.pattern
    );
    let s = sel(DOC_QUERIES[11].1);
    assert_eq!(s.result_vars(), vec!["x".to_string()]);
    assert_eq!(s.pattern, GraphPattern::Empty);
}

/// The zh-cn documentation's versions of the same examples (non-ASCII IRIs).
#[test]
fn zh_cn_documented_queries_parse() {
    let cases: [(&str, GFnName, usize); 6] = [
        (
            "select (cycleBoolean(?x, <Carol>, true, {<喜欢>}) as ?y)\nwhere\n{\n\t<Francis> <不喜欢> ?x .\n}",
            GFnName::CycleBoolean,
            4,
        ),
        (
            "SELECT (kHopEnumerate(<Alice>, ?x, true, 3, {<喜欢>, <关注>}) AS ?y) \nWHERE \n{\n    <Francis> ?pred ?x.\n    MINUS { <Alice> <不喜欢> ?x . }\n}",
            GFnName::KHopEnumerate,
            5,
        ),
        (
            "SELECT(bfsCount(<Alice>,true,{<喜欢>,<关注>,<不喜欢>}) AS ?y)\nWHERE{}",
            GFnName::BfsCount,
            3,
        ),
        (
            "SELECT (diameterEstimation({<喜欢>, <关注>, <不喜欢>}) AS ?y)WHERE{}",
            GFnName::DiameterEstimation,
            1,
        ),
        (
            "SELECT (closenessCentrality(<Alice>, false, {<喜欢>, <关注>}) AS ?x) WHERE{}",
            GFnName::ClosenessCentrality,
            3,
        ),
        (
            "SELECT (betweennessCentrality(<Eve>, true, {<喜欢>, <关注>, <不喜欢>}) AS ?y) WHERE{}",
            GFnName::BetweennessCentrality,
            3,
        ),
    ];
    for (q, f, n) in cases {
        let (func, args) = gfn(q);
        assert_eq!((func, args.len()), (f, n), "{q}");
    }
    let (_, args) = gfn(cases[2].0);
    assert_eq!(args[2], preds(&["喜欢", "关注", "不喜欢"], &[]));
}

/// Where the documentation and upstream's grammar disagree, the grammar (the
/// code) wins: PR, labelProp and louvain take named arguments plus (PR) a
/// retNum, and JaccardSimilarity's retNum is required.
#[test]
fn documented_signatures_that_differ_from_the_grammar_follow_the_grammar() {
    // Doc: PR(directed, pred_set, alpha, maxIter, tol).
    let e = call_err("PR(true, {}, 0.85, 100, 0.000001)");
    assert!(
        e.contains("PR(directed, {preds}, retNum, alpha=X, maxIter=N, tol=X)"),
        "{e}"
    );
    assert_eq!(
        call("PR(true, {}, 10, alpha=0.85, maxIter=100, tol=1e-6)"),
        (
            GFnName::Pr,
            vec![
                GArg::Bool(true),
                preds(&[], &[]),
                GArg::Int(10),
                named_num("alpha", 0.85),
                named_int("maxIter", 100),
                named_num("tol", 1e-6),
            ]
        )
    );
    // Doc: labelProp(directed, pred_set).
    assert!(call_err("labelProp(true, {})").contains("missing argument 3 (maxIter)"));
    assert_eq!(
        call("labelProp(false, {<p>}, maxIter=5)"),
        (
            GFnName::LabelProp,
            vec![
                GArg::Bool(false),
                preds(&["p"], &[]),
                named_int("maxIter", 5)
            ]
        )
    );
    // Doc: louvain(directed, pred_set, maxIter, increase).
    assert!(call_err("louvain(true, {}, 10, 0.1)").contains("maxIter=…"));
    assert_eq!(
        call("louvain(true, {}, maxIter=10, increase=0.1)"),
        (
            GFnName::Louvain,
            vec![
                GArg::Bool(true),
                preds(&[], &[]),
                named_int("maxIter", 10),
                named_num("increase", 0.1),
            ]
        )
    );
    // Doc: "ret_num: Integer, optional, default is 10" — the grammar requires it.
    assert!(call_err("JaccardSimilarity(<Alice>, {}, 2)").contains("missing argument 4 (retNum)"));
}

// ---- 2. every grammar alternative -------------------------------------------

/// One well-formed call per upstream function, with its expected AST.
fn canonical_calls() -> Vec<(&'static str, GFnName, Vec<GArg>)> {
    use GFnName::*;
    let uv = |f| {
        (
            f,
            vec![
                var("u"),
                iri("http://ex/v"),
                GArg::Bool(true),
                preds(&["http://ex/p"], &[]),
            ],
        )
    };
    let u_dir = |f| (f, vec![var("u"), GArg::Bool(false), preds(&[], &[])]);
    let mk = |s: &'static str, (f, a): (GFnName, Vec<GArg>)| (s, f, a);
    vec![
        mk(
            "simpleCyclePath(?u, <http://ex/v>, true, {<http://ex/p>})",
            uv(SimpleCyclePath),
        ),
        mk(
            "simpleCycleBoolean(?u, <http://ex/v>, true, {<http://ex/p>})",
            uv(SimpleCycleBoolean),
        ),
        mk(
            "cyclePath(?u, <http://ex/v>, true, {<http://ex/p>})",
            uv(CyclePath),
        ),
        mk(
            "cycleBoolean(?u, <http://ex/v>, true, {<http://ex/p>})",
            uv(CycleBoolean),
        ),
        mk(
            "shortestPath(?u, <http://ex/v>, true, {<http://ex/p>})",
            uv(ShortestPath),
        ),
        mk(
            "shortestPathLen(?u, <http://ex/v>, true, {<http://ex/p>})",
            uv(ShortestPathLen),
        ),
        mk(
            "shortestPathCount(?u, <http://ex/v>, true, {<http://ex/p>})",
            uv(ShortestPathCount),
        ),
        mk(
            "shortestPaths(?u, <http://ex/v>, true, {<http://ex/p>})",
            uv(ShortestPaths),
        ),
        (
            "kHopReachable(?u, ?v, false, 3, {})",
            KHopReachable,
            vec![
                var("u"),
                var("v"),
                GArg::Bool(false),
                GArg::Int(3),
                preds(&[], &[]),
            ],
        ),
        (
            "kHopReachablePath(?u, ?v, false, 3, {})",
            KHopReachablePath,
            vec![
                var("u"),
                var("v"),
                GArg::Bool(false),
                GArg::Int(3),
                preds(&[], &[]),
            ],
        ),
        (
            "kHopEnumerate(?u, ?v, false, 3, {}, 7)",
            KHopEnumerate,
            vec![
                var("u"),
                var("v"),
                GArg::Bool(false),
                GArg::Int(3),
                preds(&[], &[]),
                GArg::Int(7),
            ],
        ),
        (
            "PPR(?u, 2, {}, 5)",
            Ppr,
            vec![var("u"), GArg::Int(2), preds(&[], &[]), GArg::Int(5)],
        ),
        (
            "triangleCounting(false, {})",
            TriangleCounting,
            vec![GArg::Bool(false), preds(&[], &[])],
        ),
        mk(
            "closenessCentrality(?u, false, {})",
            u_dir(ClosenessCentrality),
        ),
        mk("bfsCount(?u, false, {})", u_dir(BfsCount)),
        (
            "PR(false, {}, 3, alpha=0.5, maxIter=7, tol=0.01)",
            Pr,
            vec![
                GArg::Bool(false),
                preds(&[], &[]),
                GArg::Int(3),
                named_num("alpha", 0.5),
                named_int("maxIter", 7),
                named_num("tol", 0.01),
            ],
        ),
        mk("SSSP(?u, false, {})", u_dir(Sssp)),
        mk("SSSPLen(?u, false, {})", u_dir(SsspLen)),
        (
            "labelProp(false, {}, maxIter=4)",
            LabelProp,
            vec![GArg::Bool(false), preds(&[], &[]), named_int("maxIter", 4)],
        ),
        ("WCC({})", Wcc, vec![preds(&[], &[])]),
        (
            "clusterCoeff(?u, false, {})",
            ClusterCoeff,
            vec![var("u"), GArg::Bool(false), preds(&[], &[])],
        ),
        (
            "maximumKplex({}, 2)",
            MaximumKplex,
            vec![preds(&[], &[]), GArg::Int(2)],
        ),
        (
            "coreTruss({}, 2, 3)",
            CoreTruss,
            vec![preds(&[], &[]), GArg::Int(2), GArg::Int(3)],
        ),
        (
            "kHopCount(?u, false, 2, {})",
            KHopCount,
            vec![var("u"), GArg::Bool(false), GArg::Int(2), preds(&[], &[])],
        ),
        (
            "kHopNeighbor(?u, false, 2, {}, 9)",
            KHopNeighbor,
            vec![
                var("u"),
                GArg::Bool(false),
                GArg::Int(2),
                preds(&[], &[]),
                GArg::Int(9),
            ],
        ),
        (
            "louvain(false, {}, maxIter=3, increase=0.25)",
            Louvain,
            vec![
                GArg::Bool(false),
                preds(&[], &[]),
                named_int("maxIter", 3),
                named_num("increase", 0.25),
            ],
        ),
        (
            "confidenceDegree(1, 2.5, -3, +4, 5e0, .5)",
            ConfidenceDegree,
            [1.0, 2.5, -3.0, 4.0, 5.0, 0.5]
                .iter()
                .map(|x| GArg::Num(*x))
                .collect(),
        ),
        (
            "diameterEstimation({})",
            DiameterEstimation,
            vec![preds(&[], &[])],
        ),
        mk(
            "betweennessCentrality(?u, false, {})",
            u_dir(BetweennessCentrality),
        ),
        (
            "JaccardSimilarity(?u, {}, 2, 10)",
            JaccardSimilarity,
            vec![var("u"), preds(&[], &[]), GArg::Int(2), GArg::Int(10)],
        ),
        (
            "degreeCorrelation(?u, 2, {})",
            DegreeCorrelation,
            vec![var("u"), GArg::Int(2), preds(&[], &[])],
        ),
        (
            "kHopAllNeighbors(?u, false, 2, {})",
            KHopAllNeighbors,
            vec![var("u"), GArg::Bool(false), GArg::Int(2), preds(&[], &[])],
        ),
    ]
}

#[test]
fn every_upstream_function_parses_with_its_grammar_shape() {
    let calls = canonical_calls();
    let mut seen: Vec<GFnName> = calls.iter().map(|(_, f, _)| *f).collect();
    seen.sort_by_key(|f| f.keyword());
    seen.dedup();
    assert_eq!(
        seen.len(),
        GFnName::ALL.len(),
        "one canonical call per function"
    );
    for (c, f, args) in calls {
        let q = format!("SELECT ?u ({c} AS ?y) WHERE {{ ?u ?p ?v }}");
        let s = sel(&q);
        assert_eq!(
            item(&s, 1),
            &Expr::GraphFn {
                func: f,
                args: args.clone()
            },
            "{c}"
        );
    }
}

#[test]
fn function_names_and_named_argument_keywords_are_case_insensitive() {
    for f in GFnName::ALL {
        assert_eq!(GFnName::from_keyword(f.name()), Some(f));
        assert_eq!(GFnName::from_keyword(&f.name().to_lowercase()), Some(f));
        assert_eq!(GFnName::from_keyword(f.keyword()), Some(f));
        assert!(f.signature().starts_with(f.name()), "{}", f.signature());
    }
    for c in [
        "SHORTESTPATH(?a, ?b, true, {})",
        "shortestpath(?a, ?b, true, {})",
        "ShOrTeStPaTh(?a, ?b, true, {})",
    ] {
        assert_eq!(call(c).0, GFnName::ShortestPath, "{c}");
    }
    let expected = call("PR(true, {}, 1, alpha=0.85, maxIter=100, tol=0.001)");
    for c in [
        "pr(TRUE, {}, 1, ALPHA=0.85, MAXITER=100, TOL=0.001)",
        "Pr(True, {}, 1, Alpha = 0.85, maxiter = 100, Tol = 0.001)",
    ] {
        assert_eq!(call(c), expected, "{c}");
    }
    assert_eq!(
        call("LOUVAIN(FALSE, {}, MaxIter=2, INCREASE=1)"),
        (
            GFnName::Louvain,
            vec![
                GArg::Bool(false),
                preds(&[], &[]),
                named_int("maxIter", 2),
                named_num("increase", 1.0),
            ]
        )
    );
}

// ---- 3. predicate sets ------------------------------------------------------

#[test]
fn pred_sets_empty_negated_mixed_and_prefixed() {
    let p = |set: &str| call(&format!("WCC({set})")).1.remove(0);
    assert_eq!(p("{}"), preds(&[], &[]));
    assert_eq!(p("{ }"), preds(&[], &[]));
    assert_eq!(p("{!<http://ex/p>}"), preds(&[], &["http://ex/p"]));
    assert_eq!(
        p("{<http://ex/a>, !<http://ex/b>, <http://ex/c>, ! <http://ex/d>}"),
        preds(
            &["http://ex/a", "http://ex/c"],
            &["http://ex/b", "http://ex/d"]
        )
    );
    // Duplicates are kept as written.
    assert_eq!(p("{<x>, <x>}"), preds(&["x", "x"], &[]));

    // Prefixed names (in the predicate set and as nodes) resolve through PREFIX.
    let (f, args) = gfn("PREFIX ex: <http://ex/> PREFIX : <http://default/>
         SELECT (shortestPath(ex:a, :b, true, {ex:likes, !ex:hates, :knows}) AS ?y) WHERE {}");
    assert_eq!(f, GFnName::ShortestPath);
    assert_eq!(
        args,
        vec![
            iri("http://ex/a"),
            iri("http://default/b"),
            GArg::Bool(true),
            preds(
                &["http://ex/likes", "http://default/knows"],
                &["http://ex/hates"]
            ),
        ]
    );
    // Relative IRIs resolve against BASE.
    let (_, args) = gfn("BASE <http://base/> SELECT (WCC({<p>, !<q>}) AS ?y) WHERE {}");
    assert_eq!(args, vec![preds(&["http://base/p"], &["http://base/q"])]);

    // Malformed sets.
    let e = call_err("maximumKplex({nope:p})");
    assert!(e.ends_with("argument 1 (pred_set): undefined prefix 'nope:'"), "{e}");
    let e = call_err("WCC({<p> <q>})");
    assert!(
        e.contains("argument 1 (pred_set)") && e.contains("found <q>"),
        "{e}"
    );
    let e = call_err("WCC({<p>,})");
    assert!(e.contains("predicate set of IRIs"), "{e}");
    let e = call_err("WCC({?p})");
    assert!(
        e.contains("predicate set of IRIs") && e.contains("?p"),
        "{e}"
    );
    let e = call_err("WCC({\"p\"})");
    assert!(e.contains("predicate set"), "{e}");
    let e = call_err("diameterEstimation(<p>)");
    assert!(e.contains("must be a predicate set like {}"), "{e}");
    // (`WCC(<p>)` has the legacy one-argument shape, so it points to rs:wcc.)
    let e = call_err("WCC(<p>)");
    assert!(e.contains("rs:wcc(…)"), "{e}");
    let e = call_err("WCC({!!<p>})");
    assert!(e.contains("predicate set"), "{e}");
}

// ---- 4. literals: booleans, integers, numbers --------------------------------

#[test]
fn signed_and_large_integers() {
    let k = |lit: &str| call(&format!("kHopCount(?u, true, {lit}, {{}})")).1[2].clone();
    assert_eq!(k("0"), GArg::Int(0));
    assert_eq!(k("+2"), GArg::Int(2));
    assert_eq!(k("-1"), GArg::Int(-1));
    assert_eq!(k("- 1"), GArg::Int(-1));
    assert_eq!(k("2147483647"), GArg::Int(i32::MAX as i64));
    assert_eq!(k("-2147483648"), GArg::Int(i32::MIN as i64));
    assert_eq!(k("007"), GArg::Int(7));
    // Upstream reads these with stoi: beyond int32 is an error, not a wrap.
    for big in ["2147483648", "-2147483649", "99999999999999999999999"] {
        let e = call_err(&format!("kHopCount(?u, true, {big}, {{}})"));
        assert!(
            e.contains("argument 3 (k)") && e.contains("32-bit") && e.contains(big),
            "{big}: {e}"
        );
    }
    // integerLiteral rejects decimals, doubles and non-numbers.
    for bad in ["1.5", "1e3", "\"2\"", "?k", "true"] {
        let e = call_err(&format!("kHopCount(?u, true, {bad}, {{}})"));
        assert!(
            e.contains("argument 3 (k) must be an integer"),
            "{bad}: {e}"
        );
    }
    // num_integer (maximumKplex k, coreTruss) is unsigned.
    assert_eq!(call("maximumKplex({})").1, vec![preds(&[], &[])]);
    for bad in ["-1", "+1"] {
        let e = call_err(&format!("maximumKplex({{}}, {bad})"));
        assert!(e.contains("unsigned integer (no sign)"), "{bad}: {e}");
    }
    let e = call_err("coreTruss({}, 2, 3000000000)");
    assert!(
        e.contains("argument 3 (beta)") && e.contains("32-bit"),
        "{e}"
    );
    // maxIter is read with stof upstream: no int32 bound, but still unsigned.
    assert_eq!(
        call("labelProp(true, {}, maxIter=5000000000)").1[2],
        named_int("maxIter", 5_000_000_000)
    );
    let e = call_err("labelProp(true, {}, maxIter=-1)");
    assert!(e.contains("maxIter=<unsigned integer>"), "{e}");
    let e = call_err("labelProp(true, {}, maxIter=1.5)");
    assert!(e.contains("maxIter=<unsigned integer>"), "{e}");
}

#[test]
fn numeric_literals_and_booleans() {
    let alpha =
        |lit: &str| call(&format!("PR(true, {{}}, 1, alpha={lit}, maxIter=1, tol=1)")).1[3].clone();
    assert_eq!(alpha("1"), named_num("alpha", 1.0));
    assert_eq!(alpha("0.85"), named_num("alpha", 0.85));
    assert_eq!(alpha("-0.5"), named_num("alpha", -0.5));
    assert_eq!(alpha("+.5"), named_num("alpha", 0.5));
    assert_eq!(alpha("1e-6"), named_num("alpha", 1e-6));
    assert_eq!(alpha("2.5E+2"), named_num("alpha", 250.0));
    let e = call_err("PR(true, {}, 1, alpha=\"x\", maxIter=1, tol=1)");
    assert!(e.contains("alpha=<number>"), "{e}");
    let e = call_err("PR(true, {}, 1, alpha 0.5, maxIter=1, tol=1)");
    assert!(e.contains("'=' is missing"), "{e}");
    // Named arguments are positional, in grammar order.
    let e = call_err("PR(true, {}, 1, maxIter=1, alpha=0.5, tol=1)");
    assert!(
        e.contains("argument 4 (alpha) must be the named argument alpha=…"),
        "{e}"
    );
    let e = call_err("PR(true, {}, 1, 0.5, 1, 1)");
    assert!(e.contains("named argument alpha=…, found 0.5"), "{e}");

    for (lit, b) in [
        ("true", true),
        ("false", false),
        ("TRUE", true),
        ("False", false),
    ] {
        assert_eq!(
            call(&format!("bfsCount(?u, {lit}, {{}})")).1[1],
            GArg::Bool(b)
        );
    }
    for bad in ["1", "\"true\"", "?d", "yes"] {
        let e = call_err(&format!("bfsCount(?u, {bad}, {{}})"));
        assert!(
            e.contains("argument 2 (directed) must be true or false"),
            "{bad}: {e}"
        );
    }
}

// ---- 5. arity, optional arguments, node arguments ---------------------------

#[test]
fn optional_arguments() {
    let n = |c: &str| call(c).1.len();
    assert_eq!(n("kHopEnumerate(?u, ?v, true, 2, {})"), 5);
    assert_eq!(n("kHopEnumerate(?u, ?v, true, 2, {}, 4)"), 6);
    assert_eq!(n("kHopNeighbor(?u, true, 2, {})"), 4);
    assert_eq!(n("kHopNeighbor(?u, true, 2, {}, -1)"), 5);
    assert_eq!(n("kHopAllNeighbors(?u, true, 2, {}, 10)"), 5);
    assert_eq!(n("maximumKplex({}, 3)"), 2);
    // clusterCoeff: the leading node is optional (local vs global coefficient).
    assert_eq!(
        call("clusterCoeff(true, {})").1,
        vec![GArg::Bool(true), preds(&[], &[])]
    );
    assert_eq!(
        call("clusterCoeff(<http://ex/a>, true, {})").1,
        vec![iri("http://ex/a"), GArg::Bool(true), preds(&[], &[])]
    );
    let e = call_err("clusterCoeff(\"a\", true, {})");
    assert!(
        e.contains("argument 1 (directed) must be true or false"),
        "{e}"
    );
    // A trailing comma is a missing optional argument, not an omitted one.
    let e = call_err("kHopEnumerate(?u, ?v, true, 2, {}, )");
    assert!(e.contains("missing argument 6 (retNum)"), "{e}");
}

#[test]
fn missing_extra_and_misplaced_arguments_have_helpful_messages() {
    let sig = "shortestPath(u, v, directed, {preds})";
    let e = call_err("shortestPath(<a>, <b>, true)");
    assert_eq!(
        e,
        format!("shortestPath expects the upstream gStore signature {sig}: missing argument 4 (pred_set)")
    );
    let e = call_err("shortestPath()");
    assert!(e.ends_with("missing argument 1 (u)"), "{e}");
    let e = call_err("shortestPath(<a>, <b>, true, {}, 3)");
    assert!(e.ends_with("too many arguments (more than 4)"), "{e}");
    let e = call_err("kHopEnumerate(?u, ?v, true, 2, {}, 4, 5)");
    assert!(e.ends_with("too many arguments (more than 6)"), "{e}");
    let e = call_err("shortestPath(<a> <b>, true, {})");
    assert!(
        e.contains("expected ',' before argument 2 (v), found <b>"),
        "{e}"
    );
    let e = call_err("WCC({} {})");
    assert!(
        e.contains("expected ')' after argument 1, found '{'"),
        "{e}"
    );
    // Node arguments are variables or IRIs only (upstream varOrIri).
    for bad in ["\"Alice\"", "1", "_:b", "true"] {
        let e = call_err(&format!("SSSP({bad}, true, {{}})"));
        assert!(
            e.contains("argument 1 (u) must be a ?variable or an IRI"),
            "{bad}: {e}"
        );
    }
    let e = call_err("SSSP(nope:x, true, {})");
    assert!(e.contains("argument 1 (u): undefined prefix 'nope:'"), "{e}");
    // `$var` works like `?var`.
    assert_eq!(call("SSSP($s, true, {})").1[0], var("s"));
}

// ---- 6. placement ------------------------------------------------------------

#[test]
fn projection_only_functions_are_rejected_elsewhere() {
    let shortest = "shortestPath(?a, ?b, true, {})";
    let placements = [
        format!("SELECT ?a WHERE {{ ?a ?p ?b FILTER({shortest}) }}"),
        format!("SELECT ?a WHERE {{ ?a ?p ?b FILTER {shortest} }}"),
        format!("SELECT ?a ?r WHERE {{ ?a ?p ?b BIND({shortest} AS ?r) }}"),
        format!("SELECT ?a WHERE {{ ?a ?p ?b }} ORDER BY ({shortest})"),
        format!("SELECT ?a WHERE {{ ?a ?p ?b }} GROUP BY ?a HAVING ({shortest})"),
        format!("SELECT ?a WHERE {{ ?a ?p ?b }} GROUP BY ({shortest} AS ?g)"),
        format!("SELECT (-{shortest} AS ?y) WHERE {{ ?a ?p ?b }}"),
        format!("SELECT (({shortest}) AS ?y) WHERE {{ ?a ?p ?b }}"),
        format!("SELECT (STR({shortest}) AS ?y) WHERE {{ ?a ?p ?b }}"),
        format!("SELECT (1 + {shortest} AS ?y) WHERE {{ ?a ?p ?b }}"),
        format!("ASK {{ ?a ?p ?b FILTER({shortest}) }}"),
    ];
    for q in &placements {
        let e = err(q);
        assert!(
            e.contains("shortestPath(…) is only supported as a whole SELECT projection item"),
            "{q}: {e}"
        );
    }
    // The function must also be the *entire* projection expression.
    let e = err(&format!(
        "SELECT ({shortest} + 1 AS ?y) WHERE {{ ?a ?p ?b }}"
    ));
    assert!(
        e.contains("must be the entire projection expression"),
        "{e}"
    );
    let e = err("SELECT (PFN.f({}) = 1 AS ?y) WHERE {}");
    assert!(
        e.contains("must be the entire projection expression"),
        "{e}"
    );
}

#[test]
fn filter_capable_functions_parse_inside_expressions() {
    // The four functions upstream's buildCompTree accepts in FILTER etc.
    let q = sel("SELECT ?a WHERE { ?a ?p ?b
           FILTER(shortestPathLen(?a, ?b, true, {}) > 2 && !cycleBoolean(?a, ?b, false, {}))
           FILTER simpleCycleBoolean(?a, <http://ex/c>, true, {<http://ex/p>})
           BIND(kHopReachable(?a, ?b, true, -1, {}) AS ?r) }
         ORDER BY (shortestPathLen(?a, ?b, false, {}))");
    let fs = filters(&q.pattern);
    assert_eq!(fs.len(), 2);
    match &fs[0] {
        Expr::And(l, r) => {
            assert!(matches!(&**l, Expr::Compare(_, x, _)
                if matches!(&**x, Expr::GraphFn { func: GFnName::ShortestPathLen, .. })));
            assert!(matches!(&**r, Expr::Not(x)
                if matches!(&**x, Expr::GraphFn { func: GFnName::CycleBoolean, .. })));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        fs[1],
        Expr::GraphFn {
            func: GFnName::SimpleCycleBoolean,
            ..
        }
    ));
    match &q.order_by[..] {
        [OrderCondition { expr, .. }] => {
            assert!(matches!(
                expr,
                Expr::GraphFn {
                    func: GFnName::ShortestPathLen,
                    ..
                }
            ))
        }
        other => panic!("{other:?}"),
    }
    // Nested in a projection expression and in HAVING.
    let q = sel("SELECT (shortestPathLen(?a, ?b, true, {}) + 1 AS ?n) WHERE { ?a ?p ?b }");
    assert!(matches!(item(&q, 0), Expr::Arith(..)));
    sel("SELECT ?a WHERE { ?a ?p ?b } GROUP BY ?a ?b HAVING (kHopReachable(?a, ?b, true, 2, {}))");
}

#[test]
fn advanced_functions_in_subselects_and_multiple_projections() {
    let q = sel("SELECT ?y WHERE { { SELECT (WCC({}) AS ?y) WHERE {} } }");
    match &q.pattern {
        GraphPattern::SubSelect(sq) => {
            assert!(matches!(
                item(sq, 0),
                Expr::GraphFn {
                    func: GFnName::Wcc,
                    ..
                }
            ))
        }
        other => panic!("{other:?}"),
    }
    let q = sel(
        "SELECT ?x (shortestPath(?x, <b>, true, {}) AS ?p) (shortestPathLen(?x, <b>, true, {}) AS ?l)
                (PFN.f({}) AS ?z)
         WHERE { ?x ?p2 ?o }",
    );
    assert!(matches!(
        item(&q, 1),
        Expr::GraphFn {
            func: GFnName::ShortestPath,
            ..
        }
    ));
    assert!(matches!(
        item(&q, 2),
        Expr::GraphFn {
            func: GFnName::ShortestPathLen,
            ..
        }
    ));
    assert!(matches!(item(&q, 3), Expr::Pfn { .. }));
    assert_eq!(q.result_vars(), vec!["x", "p", "l", "z"]);
}

// ---- 7. PFN -------------------------------------------------------------------

fn pfn(q: &str) -> (String, String) {
    match item(&sel(q), 0) {
        Expr::Pfn { name, json } => (name.clone(), json.clone()),
        other => panic!("expected Pfn in {q:?}, got {other:?}"),
    }
}

#[test]
fn pfn_calls_capture_name_and_raw_json() {
    // The example from upstream `src/Query/PfnAnalysisQuery.cpp`.
    let (name, json) = pfn(
        r#"SELECT (PFN.xxx({"src":"<Alice>", "dst":"<Bob>", "direct":"true", "predSet":["<喜欢>","<关注>"]}) AS ?z) WHERE {}"#,
    );
    assert_eq!(name, "xxx");
    // Upstream's pfn_params: whitespace between tokens dropped, strings verbatim.
    assert_eq!(
        json,
        r#"{"src":"<Alice>","dst":"<Bob>","direct":"true","predSet":["<喜欢>","<关注>"]}"#
    );
    // `PFN` is case-insensitive; the function name keeps its case.
    assert_eq!(
        pfn("select (pfn.MyFunc({}) as ?z) where {}"),
        ("MyFunc".into(), "{}".into())
    );
    // The raw grammar form `PFN name(…)` (what upstream's HTTP layer rewrites
    // `PFN.` into) is accepted too.
    assert_eq!(
        pfn(r#"SELECT (PFN f_2 ({ "k" : "v" }) AS ?z) WHERE {}"#),
        ("f_2".into(), r#"{"k":"v"}"#.into())
    );
    // Escapes and quote styles are kept exactly as written.
    assert_eq!(
        pfn(r#"SELECT (PFN.f({"a\"b":"c\\d", 'k':'v'}) AS ?z) WHERE {}"#).1,
        r#"{"a\"b":"c\\d",'k':'v'}"#
    );
    assert_eq!(
        pfn("SELECT (PFN.f({\"k\":[\"a\"], \"m\" : [ \"x\" , \"y\" ]}) AS ?z) WHERE {}").1,
        r#"{"k":["a"],"m":["x","y"]}"#
    );
    // Multi-line, with comments between tokens.
    assert_eq!(
        pfn("SELECT (PFN.f({\n  \"k\": \"v\", # comment\n  \"j\": \"w\"\n}) AS ?z) WHERE {}").1,
        r#"{"k":"v","j":"w"}"#
    );
}

#[test]
fn malformed_pfn_calls_are_rejected() {
    let cases = [
        (r#"PFN.f({"k":1})"#, "plain string literals"),
        (r#"PFN.f({"k":true})"#, "plain string literals"),
        (r#"PFN.f({"k":"v"@en})"#, "plain string literals"),
        (r#"PFN.f({"k":["a", 1]})"#, "plain string literals"),
        (r#"PFN.f({"k" "v"})"#, "expected ':' after a JSON key"),
        (r#"PFN.f({"k":"v",})"#, "expected a string key"),
        (
            r#"PFN.f({"k":"v" "j":"w"})"#,
            "expected ',' or '}' in the JSON object",
        ),
        (
            r#"PFN.f({"k":["a" "b"]})"#,
            "expected ',' or ']' in a JSON array",
        ),
        (r#"PFN.f({"k":[]})"#, "expected a string array element"),
        (r#"PFN.f("k")"#, "must be a JSON object"),
        ("PFN.f()", "must be a JSON object"),
        ("PFN.f({}, {})", "expected ')' after the JSON argument"),
        ("PFN.f", "expected '(' and a JSON object argument"),
        ("PFN({})", "must be followed by the function name"),
        ("PFN.(\"x\")", "must be followed by the function name"),
    ];
    for (c, needle) in cases {
        let e = call_err(c);
        assert!(e.contains(needle), "{c}: {e}");
    }
    // Only as a whole projection item, like upstream.
    let e = err("SELECT ?x WHERE { ?x ?p ?o FILTER(PFN.f({})) }");
    assert!(
        e.contains("PFN.f(…) is only supported as a whole SELECT projection item"),
        "{e}"
    );
    let e = err("SELECT ?x ?r WHERE { ?x ?p ?o BIND(PFN.f({}) AS ?r) }");
    assert!(
        e.contains("only supported as a whole SELECT projection item"),
        "{e}"
    );
}

// ---- 8. legacy gstore-rust names ---------------------------------------------

#[test]
fn legacy_shaped_calls_of_colliding_names_point_to_rs() {
    let cases = [
        ("SSSP(?a, ?b)", "sssp"),
        ("SSSPLEN(?a, ?b)", "sssplen"),
        ("SHORTESTPATHS(?a, ?b)", "shortestpaths"),
        ("PPR(?a, ?b)", "ppr"),
        ("WCC(?x)", "wcc"),
        ("WCC(?x, ?y)", "wcc"),
        ("TRIANGLECOUNTING()", "trianglecounting"),
        ("CYCLEBOOLEAN()", "cycleboolean"),
        ("simpleCycleBoolean()", "simplecycleboolean"),
        ("BFSCOUNT(<a>, 2)", "bfscount"),
        ("KHOPCOUNT(<a>, 2)", "khopcount"),
        ("khopNeighbor(<a>, 2)", "khopneighbor"),
        ("KHOPALLNEIGHBORS(<a>, 2)", "khopallneighbors"),
        ("KHOPREACHABLE(<a>, <b>, 3)", "khopreachable"),
        ("LOUVAIN(?a)", "louvain"),
        ("LABELPROP(?a, ?b)", "labelprop"),
        ("BETWEENNESSCENTRALITY(?a)", "betweennesscentrality"),
        ("CLOSENESSCENTRALITY(?a)", "closenesscentrality"),
        ("JACCARDSIMILARITY(?a, ?b)", "jaccardsimilarity"),
        ("DEGREECORRELATION()", "degreecorrelation"),
        ("DIAMETERESTIMATION()", "diameterestimation"),
    ];
    for (c, rs) in cases {
        let name = &c[..c.find('(').unwrap()];
        let e = call_err(c);
        assert!(
            e.contains("is the legacy gstore-rust form")
                && e.contains(&format!("call the legacy function as rs:{rs}(…)"))
                && e.starts_with(&format!("{name} expects the upstream gStore signature")),
            "{c}: {e}"
        );
    }
    // In FILTER / BIND too.
    let e = err("SELECT ?a WHERE { ?a ?p ?b FILTER(SHORTESTPATHLEN(?a, ?b) = 3) }");
    assert!(e.contains("rs:shortestpathlen(…)"), "{e}");
    let e = err("SELECT ?a ?w WHERE { ?a ?p ?b BIND(WCC(?a) AS ?w) }");
    assert!(e.contains("rs:wcc(…)"), "{e}");
    // Not legacy-shaped either: the upstream error, plus a pointer to rs:.
    let e = call_err("SSSP(?a, ?b, ?c, ?d)");
    assert!(
        e.contains("argument 2 (directed) must be true or false")
            && e.ends_with("(the legacy gstore-rust function is rs:sssp(…))"),
        "{e}"
    );
    // Upstream-only names have no rs: hint.
    let e = call_err("shortestPath(?a, ?b)");
    assert!(!e.contains("rs:"), "{e}");
}

#[test]
fn non_colliding_legacy_names_still_parse_as_builtins() {
    let q = sel(
        "SELECT (CLUSTERINGCOEFF() AS ?a) (clusteringCoeff(<x>) AS ?b)
                (KHOPENUMERATEPATH(<x>, 2) AS ?c) (SIMILARITY(\"ab\", \"ac\") AS ?d)
         WHERE {}",
    );
    let got: Vec<(String, usize)> = (0..4)
        .map(|i| match item(&q, i) {
            Expr::Builtin(n, a) => (n.clone(), a.len()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("CLUSTERINGCOEFF".to_string(), 0),
            ("CLUSTERINGCOEFF".to_string(), 1),
            ("KHOPENUMERATEPATH".to_string(), 2),
            ("SIMILARITY".to_string(), 2),
        ]
    );
    // Upstream's clusterCoeff is a different (upstream) function.
    assert_eq!(call("clusterCoeff(true, {})").0, GFnName::ClusterCoeff);
}

// ---- 9. rs: namespace and IRI function calls ---------------------------------

fn iri_call(e: &Expr) -> (&str, usize) {
    match e {
        Expr::IriCall { iri, args } => (iri.as_str(), args.len()),
        other => panic!("expected IriCall, got {other:?}"),
    }
}

#[test]
fn rs_calls_parse_anywhere_an_expression_is_allowed() {
    let q = sel(
        "SELECT ?a (rs:ppr(<http://ex/a>, ?a) AS ?r) (rs:BfsCount(?a) + 1 AS ?n)
         WHERE {
           ?a ?p ?b
           FILTER(rs:sssplen(<http://ex/a>, ?b) = 3 && rs:wcc(?a, ?b))
           FILTER rs:cycleboolean()
           BIND(rs:trianglecounting(?a) AS ?t)
           FILTER EXISTS { ?a ?q ?c FILTER(rs:khopreachable(?a, ?c, 2)) }
         }
         GROUP BY ?a HAVING (rs:wcc(?a) > 1)",
    );
    assert_eq!(iri_call(item(&q, 1)), ("urn:gstore-rust:fn:PPR", 2));
    match item(&q, 2) {
        Expr::Arith(_, l, _) => assert_eq!(iri_call(l), ("urn:gstore-rust:fn:BFSCOUNT", 1)),
        other => panic!("{other:?}"),
    }
    let GraphPattern::Filter(fs, inner) = &q.pattern else {
        panic!("{:?}", q.pattern)
    };
    match &fs[0] {
        Expr::And(l, r) => {
            match &**l {
                Expr::Compare(_, x, _) => {
                    assert_eq!(iri_call(x), ("urn:gstore-rust:fn:SSSPLEN", 2))
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(iri_call(r), ("urn:gstore-rust:fn:WCC", 2));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(iri_call(&fs[1]), ("urn:gstore-rust:fn:CYCLEBOOLEAN", 0));
    assert!(matches!(fs[2], Expr::Exists(false, _)));
    match &**inner {
        GraphPattern::Extend(_, v, e) => {
            assert_eq!(v, "t");
            assert_eq!(iri_call(e), ("urn:gstore-rust:fn:TRIANGLECOUNTING", 1));
        }
        other => panic!("{other:?}"),
    }
    match &q.having[0] {
        Expr::Compare(_, x, _) => assert_eq!(iri_call(x), ("urn:gstore-rust:fn:WCC", 1)),
        other => panic!("{other:?}"),
    }
    // The full IRI and any case of the local name canonicalize the same way.
    for c in [
        "<urn:gstore-rust:fn:sssp>(?a, ?b)",
        "rs:SSSP(?a, ?b)",
        "rs:Sssp(?a, ?b)",
    ] {
        let q = sel(&format!("SELECT ({c} AS ?y) WHERE {{}}"));
        assert_eq!(iri_call(item(&q, 0)), ("urn:gstore-rust:fn:SSSP", 2), "{c}");
    }
    // Every legacy function is reachable through rs:.
    for name in gstore::parser::sparql::ast::RS_LEGACY_FNS {
        let spec = gstore::parser::sparql::builtins::lookup(name).expect("legacy arity");
        let args = vec!["?a"; spec.arity.min].join(", ");
        let q = sel(&format!(
            "SELECT (rs:{}({args}) AS ?y) WHERE {{ ?a ?p ?o }}",
            name.to_lowercase()
        ));
        assert_eq!(
            iri_call(item(&q, 0)),
            (
                format!("urn:gstore-rust:fn:{name}").as_str(),
                spec.arity.min
            )
        );
    }
}

#[test]
fn rs_call_errors() {
    let e = call_err("rs:nosuch(?a)");
    assert!(
        e.contains("rs:nosuch: unknown gstore-rust function 'nosuch'")
            && e.contains("rs:sssp")
            && e.contains("rs:clusteringcoeff"),
        "{e}"
    );
    // Upstream-only functions are not in the legacy namespace.
    let e = call_err("rs:shortestPath(?a, ?b)");
    assert!(
        e.contains("unknown gstore-rust function 'shortestPath'"),
        "{e}"
    );
    let e = call_err("rs:sssp(?a)");
    assert_eq!(e, "rs:sssp: SSSP expects exactly 2 arguments, got 1");
    let e = call_err("<urn:gstore-rust:fn:wcc>()");
    assert!(e.contains("WCC expects 1 to 2 arguments, got 0"), "{e}");
}

#[test]
fn user_prefix_rs_overrides_the_predeclared_one() {
    let q = sel("PREFIX rs: <http://example.org/fn#>
         SELECT (rs:sssp(?a) AS ?y) (rs:anything(1, 2, 3) AS ?z) WHERE {}");
    // No legacy validation: these are ordinary IRI function calls.
    assert_eq!(iri_call(item(&q, 0)), ("http://example.org/fn#sssp", 1));
    assert_eq!(iri_call(item(&q, 1)), ("http://example.org/fn#anything", 3));
    // The predeclared prefix also expands in triple patterns.
    let q = sel("SELECT ?s WHERE { ?s rs:p ?o }");
    let GraphPattern::Bgp(tps) = &q.pattern else {
        panic!("{:?}", q.pattern)
    };
    assert_eq!(
        tps[0].predicate,
        gstore::parser::sparql::ast::PatternTerm::Term(gstore::Term::iri("urn:gstore-rust:fn:p"))
    );
}

#[test]
fn other_iri_function_calls_parse() {
    let q = sel("PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (xsd:integer(?x) AS ?i) (<http://ex/f>() AS ?f) WHERE { ?s ?p ?x
           FILTER(<http://ex/g>(?x, 1) && xsd:boolean(\"true\")) }");
    assert_eq!(
        iri_call(item(&q, 0)),
        ("http://www.w3.org/2001/XMLSchema#integer", 1)
    );
    assert_eq!(iri_call(item(&q, 1)), ("http://ex/f", 0));
    match &filters(&q.pattern)[0] {
        Expr::And(l, r) => {
            assert_eq!(iri_call(l), ("http://ex/g", 2));
            assert_eq!(iri_call(r), ("http://www.w3.org/2001/XMLSchema#boolean", 1));
        }
        other => panic!("{other:?}"),
    }
    // A bare IRI constant (no argument list) is still a constant.
    let q = sel("SELECT ?s WHERE { ?s ?p ?o FILTER(?o = <http://ex/a>) }");
    assert!(
        matches!(&filters(&q.pattern)[0], Expr::Compare(_, _, r) if matches!(**r, Expr::Const(_)))
    );
}

// ---- 10. engine: clean errors until evaluation is wired ----------------------

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> <http://ex/c> .\n\
                    <http://ex/c> <http://ex/p> <http://ex/d> .\n";

#[test]
fn engine_rejects_unwired_calls_cleanly_and_runs_rs_calls() {
    let mut db = Database::build_from_str("gfun", DATA).unwrap();
    let unwired = [
        (
            "SELECT (shortestPath(<http://ex/a>, <http://ex/d>, true, {}) AS ?p) WHERE {}",
            "advanced function evaluation not wired yet: shortestPath(…)",
        ),
        (
            "SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER(shortestPathLen(?s, ?o, true, {}) = 1) }",
            "advanced function evaluation not wired yet: shortestPathLen(…)",
        ),
        (
            "ASK { ?s <http://ex/p> ?o FILTER EXISTS { ?o ?q ?z FILTER(cycleBoolean(?s, ?o, true, {})) } }",
            "advanced function evaluation not wired yet: cycleBoolean(…)",
        ),
        (
            "SELECT ?y WHERE { { SELECT (WCC({}) AS ?y) WHERE {} } }",
            "advanced function evaluation not wired yet: WCC(…)",
        ),
        (
            "SELECT (PFN.f({\"k\":\"v\"}) AS ?z) WHERE {}",
            "PFN not supported yet: PFN.f(…)",
        ),
        (
            "SELECT (<http://ex/nosuch>(1) AS ?z) WHERE {}",
            "unsupported function <http://ex/nosuch>",
        ),
    ];
    for (q, msg) in unwired {
        match db.query(q) {
            Err(GStoreError::Query(m)) => assert_eq!(m, msg, "{q}"),
            other => panic!("{q}: expected a query error, got {other:?}"),
        }
    }
    // rs: calls run the legacy implementations (here: shortest-path length).
    let rs = db
        .select("SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o FILTER(rs:sssplen(<http://ex/a>, <http://ex/d>) = 3) }")
        .unwrap();
    assert_eq!(rs.row_count(), 1);
    match db
        .query("SELECT (rs:shortestpathlen(<http://ex/a>, <http://ex/c>) AS ?n) WHERE {}")
        .unwrap()
    {
        QueryResult::Select(rs) => {
            let v = rs.rows[0][0].clone().expect("bound");
            assert!(v.starts_with("\"2\""), "{v}");
        }
        other => panic!("{other:?}"),
    }
}
