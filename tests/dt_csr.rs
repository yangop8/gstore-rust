//! Data test: the per-predicate CSR behind gStore's advanced graph functions
//! (`gstore::analytics::csr`), its per-`Database` cache, and the `gfun`
//! library skeleton (argument types, dispatch, upstream output builders).
//!
//! Uses the sample social graph of upstream `docs/en-us/SPARQL_QUERY.md`
//! ("Advanced functions"), whose entity ids — in first-appearance order —
//! are Alice 0, Bob 1, Eve 2, Carol 3, Francis 4, Dave 5, exactly as in the
//! upstream examples' `nodeIndex` values.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use gstore::analytics::csr::{CsrCache, CsrGraph, Direction, EdgeDir, Node, PredFilter};
use gstore::analytics::gfun::{cxx, fmt, metrics, ArgSlot, GFnKind, GFunCtx, GFunValue, GNode};
use gstore::{Database, QueryResult};

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

fn social() -> Database {
    Database::build_from_str("social", SOCIAL).unwrap()
}

fn eid(db: &Database, name: &str) -> u32 {
    db.dict().entity_id(&format!("<{name}>")).unwrap()
}

fn pid(db: &Database, name: &str) -> u32 {
    db.dict().predicate_id(&format!("<{name}>")).unwrap()
}

fn node(db: &Database, g: &CsrGraph, name: &str) -> Node {
    g.node_of(eid(db, name)).unwrap()
}

fn names(db: &Database, g: &CsrGraph, nodes: &[Node]) -> Vec<String> {
    nodes
        .iter()
        .map(|&n| db.dict().id_to_string(g.entity_of(n)).unwrap().to_string())
        .collect()
}

fn update(db: &mut Database, q: &str) -> usize {
    match db.query(q).unwrap() {
        QueryResult::Update { changed } => changed,
        other => panic!("expected an update result, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// CSR structure
// ---------------------------------------------------------------------------

#[test]
fn sample_graph_ids_match_the_upstream_examples() {
    let db = social();
    for (i, n) in ["Alice", "Bob", "Eve", "Carol", "Francis", "Dave"]
        .iter()
        .enumerate()
    {
        assert_eq!(eid(&db, n), i as u32, "{n}");
    }
    let g = db.csr().unwrap();
    assert_eq!(g.num_nodes(), 6);
    assert_eq!(g.num_edges(), 15);
    assert_eq!(g.predicates().len(), 3);
    // Dense ids → node index == entity id (upstream's raw vertex ids).
    for e in 0..6u32 {
        assert_eq!(g.node_of(e), Some(e));
        assert_eq!(g.entity_of(e), e);
    }
}

#[test]
fn csr_is_per_predicate_and_bidirectional_with_cross_predicate_duplicates() {
    let db = social();
    let g = db.csr().unwrap();
    let (like, focus, dislike) = (pid(&db, "like"), pid(&db, "focus"), pid(&db, "dislike"));
    let alice = node(&db, &g, "Alice");
    let bob = node(&db, &g, "Bob");
    // <Alice> -focus-> <Bob> and <Alice> -like-> <Bob>: one edge per predicate.
    assert_eq!(names(&db, &g, g.out_nbrs(alice, focus)), ["<Bob>"]);
    assert_eq!(names(&db, &g, g.out_nbrs(alice, like)), ["<Bob>"]);
    assert_eq!(names(&db, &g, g.out_nbrs(alice, dislike)), ["<Eve>"]);
    assert_eq!(g.out_degree(alice, &PredFilter::all(db.dict())), 3);
    assert_eq!(
        g.distinct_neighbors(alice, &PredFilter::all(db.dict()), Direction::Out),
        vec![bob, node(&db, &g, "Eve")]
    );
    // In-direction, neighbours ascending by entity id.
    assert_eq!(names(&db, &g, g.in_nbrs(bob, like)), ["<Alice>", "<Carol>"]);
    assert_eq!(
        names(&db, &g, g.in_nbrs(bob, focus)),
        ["<Alice>", "<Carol>"]
    );
    assert!(g.in_nbrs(bob, dislike).is_empty());
    assert_eq!(g.in_degree(bob, &PredFilter::all(db.dict())), 4);
    // Per-predicate edge counts (upstream getSetEdgeNum).
    assert_eq!(g.pred_edge_count(like), 5);
    assert_eq!(g.pred_edge_count(focus), 5);
    assert_eq!(g.pred_edge_count(dislike), 5);
    assert_eq!(g.num_edges_with(&PredFilter::from_ids([like, focus])), 10);
    // Out and in views describe the same edge set.
    let mut out_edges = 0;
    let mut in_edges = 0;
    for v in 0..g.num_nodes() as Node {
        out_edges += g.out_edges(v).0.len();
        in_edges += g.in_edges(v).0.len();
        for (&p, &w) in g.out_edges(v).0.iter().zip(g.out_edges(v).1) {
            assert!(g.in_nbrs(w, p).contains(&v));
        }
    }
    assert_eq!((out_edges, in_edges), (15, 15));
}

#[test]
fn both_direction_iteration_follows_upstream_predicate_interleaving() {
    let db = social();
    let g = db.csr().unwrap();
    let bob = node(&db, &g, "Bob");
    let filter = PredFilter::resolve(&["<like>", "<focus>"], &[], db.dict());
    let mut seen = Vec::new();
    g.for_each_both(bob, &filter, |n, p, d| {
        seen.push((
            db.dict().predicate_to_string(p).unwrap().to_string(),
            names(&db, &g, &[n])[0].clone(),
            d,
        ))
    });
    let s = |p: &str, n: &str, d| (p.to_string(), n.to_string(), d);
    assert_eq!(
        seen,
        vec![
            s("<like>", "<Eve>", EdgeDir::Out),
            s("<like>", "<Alice>", EdgeDir::In),
            s("<like>", "<Carol>", EdgeDir::In),
            s("<focus>", "<Alice>", EdgeDir::Out),
            s("<focus>", "<Alice>", EdgeDir::In),
            s("<focus>", "<Carol>", EdgeDir::In),
        ]
    );
}

#[test]
fn literal_objects_are_not_edges_and_blank_nodes_are_vertices() {
    let db = Database::build_from_str(
        "lit",
        "<http://ex/a> <http://ex/name> \"A\" .
         <http://ex/a> <http://ex/knows> _:b .
         _:b <http://ex/knows> <http://ex/c> .
         <http://ex/z> <http://ex/name> \"Z\" .
         <http://ex/c> <http://ex/age> 7 .",
    )
    .unwrap();
    let g = db.csr().unwrap();
    assert_eq!(g.num_nodes(), 3, "a, _:b, c");
    assert_eq!(g.num_edges(), 2);
    let name = db.dict().predicate_id("<http://ex/name>").unwrap();
    assert_eq!(g.pred_edge_count(name), 0);
    assert_eq!(g.predicates().len(), 1, "only <knows> has entity edges");
    let z = db.dict().entity_id("<http://ex/z>").unwrap();
    assert_eq!(
        g.node_of(z),
        None,
        "an entity with only literal objects is isolated"
    );
    let iso = GNode::resolve(&g, Some(z));
    assert_eq!((iso.entity, iso.node), (z, None));
    assert!(!iso.is_missing());
    assert!(GNode::resolve(&g, None).is_missing());
    let c = db.dict().entity_id("<http://ex/c>").unwrap();
    assert!(GNode::resolve(&g, Some(c)).node.is_some());
}

#[test]
fn empty_database_has_an_empty_csr() {
    let db = Database::new("empty");
    let g = db.csr().unwrap();
    assert_eq!((g.num_nodes(), g.num_edges()), (0, 0));
    assert!(g.out_nbrs(0, 0).is_empty());
    assert_eq!(g.degree(0, &PredFilter::all(db.dict()), Direction::Both), 0);
}

// ---------------------------------------------------------------------------
// PredFilter (GeneralEvaluation::getPredSetByStringList semantics)
// ---------------------------------------------------------------------------

#[test]
fn pred_filter_resolution_follows_upstream() {
    let db = Database::build_from_str(
        "preds",
        "<a> <p0> <b> . <a> <p1> <c> . <a> <p2> \"lit\" . <b> <p3> <c> .",
    )
    .unwrap();
    let d = db.dict();
    let id = |p: &str| d.predicate_id(&format!("<{p}>")).unwrap();
    let none: [&str; 0] = [];
    // {} → every dictionary predicate ascending (also <p2>, which has no
    // entity edge — upstream iterates 0..pre_num).
    let all = PredFilter::resolve(&none, &none, d);
    assert_eq!(all.preds(), &[id("p0"), id("p1"), id("p2"), id("p3")]);
    assert!(all.is_ascending());
    assert_eq!(all, PredFilter::all(d));
    // Include list: written order, de-duplicated, unknown IRIs dropped,
    // bracketed or bare.
    let inc = PredFilter::resolve(&["<p3>", "p1", "<nope>", "<p3>"], &none, d);
    assert_eq!(inc.preds(), &[id("p3"), id("p1")]);
    assert!(!inc.is_ascending());
    // Only unknown IRIs → empty (no edge may be traversed).
    assert!(PredFilter::resolve(&["<nope>"], &none, d).is_empty());
    // Exclusions: all but the excluded, ascending; unknown exclusions ignored.
    let exc = PredFilter::resolve(&none, &["<p1>", "<nope>"], d);
    assert_eq!(exc.preds(), &[id("p0"), id("p2"), id("p3")]);
    // With an exclusion present the include list is ignored (GE:4199-4209).
    let mixed = PredFilter::resolve(&["<p0>"], &["<p3>"], d);
    assert_eq!(mixed.preds(), &[id("p0"), id("p1"), id("p2")]);
    // Traversal honours the filter order.
    let g = db.csr().unwrap();
    let a = g.node_of(d.entity_id("<a>").unwrap()).unwrap();
    let mut order = Vec::new();
    g.for_each_out(a, &PredFilter::from_ids([id("p1"), id("p0")]), |n, _| {
        order.push(d.id_to_string(g.entity_of(n)).unwrap().to_string())
    });
    assert_eq!(order, ["<c>", "<b>"]);
}

// ---------------------------------------------------------------------------
// cache invalidation
// ---------------------------------------------------------------------------

fn has_edge(db: &Database, s: &str, p: &str, o: &str) -> bool {
    let g = db.csr().unwrap();
    let (Some(s), Some(p), Some(o)) = (
        db.dict().entity_id(s).and_then(|e| g.node_of(e)),
        db.dict().predicate_id(p),
        db.dict().entity_id(o).and_then(|e| g.node_of(e)),
    ) else {
        return false;
    };
    g.out_nbrs(s, p).contains(&o)
}

#[test]
fn csr_is_cached_until_the_data_changes() {
    let mut db = social();
    assert!(!db.csr_is_cached());
    let g1 = db.csr().unwrap();
    assert!(db.csr_is_cached());
    let g2 = db.csr().unwrap();
    assert!(Arc::ptr_eq(&g1, &g2), "no rebuild without a mutation");
    // A read query does not invalidate.
    db.select("SELECT ?x WHERE { <Alice> <like> ?x }").unwrap();
    assert!(db.csr_is_cached());
    // A no-op update (triple already present) does not either.
    assert_eq!(update(&mut db, "INSERT DATA { <Alice> <like> <Bob> }"), 0);
    assert!(Arc::ptr_eq(&g1, &db.csr().unwrap()));

    // INSERT DATA.
    assert_eq!(update(&mut db, "INSERT DATA { <Eve> <like> <Alice> }"), 1);
    assert!(!db.csr_is_cached());
    let g3 = db.csr().unwrap();
    assert!(!Arc::ptr_eq(&g1, &g3));
    assert_eq!(g3.num_edges(), 16);
    assert!(has_edge(&db, "<Eve>", "<like>", "<Alice>"));
    // The old Arc stays valid and unchanged for holders.
    assert_eq!(g1.num_edges(), 15);

    // DELETE DATA.
    assert_eq!(update(&mut db, "DELETE DATA { <Eve> <like> <Alice> }"), 1);
    assert!(!has_edge(&db, "<Eve>", "<like>", "<Alice>"));
    assert_eq!(db.csr().unwrap().num_edges(), 15);

    // DELETE/INSERT WHERE.
    update(
        &mut db,
        "DELETE { <Bob> <like> ?x } INSERT { <Bob> <hate> ?x } WHERE { <Bob> <like> ?x }",
    );
    assert!(!has_edge(&db, "<Bob>", "<like>", "<Eve>"));
    assert!(has_edge(&db, "<Bob>", "<hate>", "<Eve>"));

    // Direct API.
    let t = gstore::Triple::new(
        gstore::Term::iri("Dave"),
        gstore::Term::iri("like"),
        gstore::Term::iri("Bob"),
    );
    assert!(db.insert_triple(&t));
    assert!(has_edge(&db, "<Dave>", "<like>", "<Bob>"));
    assert!(db.remove_triple(&t));
    assert!(!has_edge(&db, "<Dave>", "<like>", "<Bob>"));
}

#[test]
fn rollback_and_commit_invalidate() {
    let mut db = social();
    let before = db.csr().unwrap();
    db.begin().unwrap();
    update(&mut db, "INSERT DATA { <Eve> <like> <Dave> }");
    assert!(
        has_edge(&db, "<Eve>", "<like>", "<Dave>"),
        "visible inside the txn"
    );
    db.rollback().unwrap();
    assert!(!db.csr_is_cached(), "rollback changes the data back");
    let after = db.csr().unwrap();
    assert!(!has_edge(&db, "<Eve>", "<like>", "<Dave>"));
    assert_eq!(after.num_edges(), before.num_edges());
    assert_eq!(after.entities(), before.entities());

    db.begin().unwrap();
    update(&mut db, "INSERT DATA { <Eve> <like> <Dave> }");
    db.commit().unwrap();
    assert!(has_edge(&db, "<Eve>", "<like>", "<Dave>"));
}

#[test]
fn load_and_clear_invalidate_and_named_graphs_stay_out() {
    let mut db = social();
    let dir = std::env::temp_dir().join(format!("gstore_dt_csr_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("extra.nt");
    std::fs::write(&file, "<Zoe> <like> <Alice> .\n<Zoe> <age> \"30\" .\n").unwrap();
    db.csr().unwrap();
    let n = update(&mut db, &format!("LOAD <file://{}>", file.display()));
    assert_eq!(n, 2);
    assert!(!db.csr_is_cached());
    let g = db.csr().unwrap();
    assert_eq!(g.num_nodes(), 7);
    assert_eq!(
        g.num_edges(),
        16,
        "the literal-object triple is not an edge"
    );
    assert!(has_edge(&db, "<Zoe>", "<like>", "<Alice>"));

    // A named-graph insert advances the generation (rebuild) but the CSR is
    // built from the default graph only.
    update(
        &mut db,
        "INSERT DATA { GRAPH <g> { <Alice> <like> <Zoe> } }",
    );
    assert!(!db.csr_is_cached());
    assert!(!has_edge(&db, "<Alice>", "<like>", "<Zoe>"));
    assert_eq!(db.csr().unwrap().num_edges(), 16);

    // CLEAR DEFAULT empties it; the named graph is not part of it anyway.
    update(&mut db, "CLEAR DEFAULT");
    let g = db.csr().unwrap();
    assert_eq!((g.num_nodes(), g.num_edges()), (0, 0));
    update(&mut db, "INSERT DATA { <Alice> <like> <Bob> }");
    assert_eq!(db.csr().unwrap().num_edges(), 1);
    update(&mut db, "DROP ALL");
    assert_eq!(db.csr().unwrap().num_edges(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rdfs_inference_invalidates() {
    let mut db = Database::build_from_str(
        "rdfs",
        "<http://ex/Cat> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <http://ex/Animal> .
         <http://ex/tom> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/Cat> .",
    )
    .unwrap();
    let before = db.csr().unwrap().num_edges();
    assert!(db.materialize_rdfs() > 0);
    assert!(!db.csr_is_cached());
    assert!(db.csr().unwrap().num_edges() > before);
    assert!(has_edge(
        &db,
        "<http://ex/tom>",
        "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>",
        "<http://ex/Animal>"
    ));
}

#[test]
fn preload_builds_eagerly() {
    let db = social();
    assert!(!db.csr_is_cached());
    db.preload_csr().unwrap();
    assert!(db.csr_is_cached());
}

#[test]
fn csr_cache_is_shared_across_threads() {
    let cache = Arc::new(CsrCache::new());
    let builds = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (cache, builds) = (Arc::clone(&cache), Arc::clone(&builds));
            std::thread::spawn(move || {
                cache
                    .get_or_build(7, || {
                        builds.fetch_add(1, Ordering::SeqCst);
                        CsrGraph::from_id_triples(vec![gstore::IdTriple::new(0, 0, 1)])
                    })
                    .unwrap()
            })
        })
        .collect();
    let graphs: Vec<Arc<CsrGraph>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        builds.load(Ordering::SeqCst),
        1,
        "one build for one generation"
    );
    assert!(graphs.windows(2).all(|w| Arc::ptr_eq(&w[0], &w[1])));
}

// ---------------------------------------------------------------------------
// gfun skeleton
// ---------------------------------------------------------------------------

#[test]
fn function_table_covers_the_upstream_grammar() {
    let upstream = [
        "SIMPLECYCLEPATH",
        "SIMPLECYCLEBOOLEAN",
        "CYCLEPATH",
        "CYCLEBOOLEAN",
        "SHORTESTPATH",
        "SHORTESTPATHLEN",
        "KHOPREACHABLE",
        "KHOPENUMERATE",
        "KHOPREACHABLEPATH",
        "PPR",
        "TRIANGLECOUNTING",
        "CLOSENESSCENTRALITY",
        "BFSCOUNT",
        "PR",
        "SSSP",
        "SSSPLEN",
        "LABELPROP",
        "WCC",
        "CLUSTERCOEFF",
        "MAXIMUMKPLEX",
        "CORETRUSS",
        "KHOPCOUNT",
        "KHOPNEIGHBOR",
        "SHORTESTPATHCOUNT",
        "LOUVAIN",
        "CONFIDENCEDEGREE",
        "DIAMETERESTIMATION",
        "BETWEENNESSCENTRALITY",
        "JACCARDSIMILARITY",
        "DEGREECORRELATION",
        "SHORTESTPATHS",
        "KHOPALLNEIGHBORS",
    ];
    for name in upstream {
        assert!(GFnKind::from_name(name).is_some(), "{name}");
    }
    // A few grammar shapes (SPARQL.g4:197-229).
    assert_eq!(
        GFnKind::Ppr.signature(),
        &[ArgSlot::Node, ArgSlot::Int, ArgSlot::PredSet, ArgSlot::Int]
    );
    assert_eq!(GFnKind::Wcc.signature(), &[ArgSlot::PredSet]);
    assert_eq!(
        GFnKind::JaccardSimilarity.signature(),
        &[ArgSlot::Node, ArgSlot::PredSet, ArgSlot::Int, ArgSlot::Int]
    );
}

#[test]
#[allow(clippy::excessive_precision)] // golden as C printed it (%.9g)
fn jaro_similarity_is_available_for_similarity() {
    assert_eq!(metrics::jaro_similarity("abc", "abc"), 1.0);
    assert_eq!(
        metrics::jaro_similarity("MARTHA", "MARHTA"),
        f64::from(0.944444418f32)
    );
    assert_eq!(metrics::jaro_similarity("", "x"), 0.0);
}

/// The shared `pathVec2JSON` builder reproduces the documented outputs
/// byte for byte (SPARQL_QUERY.md, outputs minified as upstream prints them).
#[test]
fn path_builder_reproduces_the_documented_outputs() {
    let db = social();
    let g = db.csr().unwrap();
    let ctx = GFunCtx::new(&g, db.dict());
    let e = |n: &str| i64::from(eid(&db, n));
    let p = |n: &str| i64::from(pid(&db, n));

    // shortestPath(<Francis>, ?x, true, {<like>, <focus>}) → Alice.
    let path = fmt::GPath::from_upstream(&[
        e("Francis"),
        p("like"),
        e("Carol"),
        p("like"),
        e("Bob"),
        p("focus"),
        e("Alice"),
    ]);
    let mut list = fmt::JsonList::new();
    fmt::push_path_json(
        list.item(),
        &ctx,
        eid(&db, "Francis"),
        eid(&db, "Alice"),
        &path,
    );
    assert_eq!(
        list.into_paths(),
        r#"{"paths":[{"src":"<Francis>","dst":"<Alice>","edges":[{"fromNode":4,"toNode":3,"predIRI":"<like>"},{"fromNode":3,"toNode":1,"predIRI":"<like>"},{"fromNode":1,"toNode":0,"predIRI":"<focus>"}],"nodes":[{"nodeIndex":0,"nodeIRI":"<Alice>"},{"nodeIndex":1,"nodeIRI":"<Bob>"},{"nodeIndex":3,"nodeIRI":"<Carol>"},{"nodeIndex":4,"nodeIRI":"<Francis>"}]}]}"#
    );

    // cyclePath(?x, <Carol>, true, {<like>}) → Eve → Carol → Bob → Eve.
    let cycle = fmt::GPath::from_upstream(&[
        e("Eve"),
        p("like"),
        e("Carol"),
        p("like"),
        e("Bob"),
        p("like"),
        e("Eve"),
    ]);
    let mut s = String::new();
    fmt::push_path_json(&mut s, &ctx, eid(&db, "Eve"), eid(&db, "Carol"), &cycle);
    assert_eq!(
        s,
        r#"{"src":"<Eve>","dst":"<Carol>","edges":[{"fromNode":2,"toNode":3,"predIRI":"<like>"},{"fromNode":3,"toNode":1,"predIRI":"<like>"},{"fromNode":1,"toNode":2,"predIRI":"<like>"}],"nodes":[{"nodeIndex":1,"nodeIRI":"<Bob>"},{"nodeIndex":3,"nodeIRI":"<Carol>"},{"nodeIndex":2,"nodeIRI":"<Eve>"}]}"#
    );

    // kHopEnumerate(<Alice>, ?x, true, 3, {<like>, <focus>}): the docs print
    // the nodes as 3,2,0,1, but libstdc++'s map order for the insertion
    // sequence 0,1,2,3 is 3,2,1,0 (the docs' other two examples agree with
    // the emulation; see `CxxUnorderedOrder`).
    let k = fmt::GPath::from_upstream(&[
        e("Alice"),
        p("focus"),
        e("Bob"),
        p("like"),
        e("Eve"),
        p("like"),
        e("Carol"),
    ]);
    let mut s = String::new();
    fmt::push_path_json(&mut s, &ctx, eid(&db, "Alice"), eid(&db, "Carol"), &k);
    assert!(s.ends_with(
        r#""edges":[{"fromNode":0,"toNode":1,"predIRI":"<focus>"},{"fromNode":1,"toNode":2,"predIRI":"<like>"},{"fromNode":2,"toNode":3,"predIRI":"<like>"}],"nodes":[{"nodeIndex":3,"nodeIRI":"<Carol>"},{"nodeIndex":2,"nodeIRI":"<Eve>"},{"nodeIndex":1,"nodeIRI":"<Bob>"},{"nodeIndex":0,"nodeIRI":"<Alice>"}]}"#
    ));

    // A reversed edge swaps fromNode/toNode; the empty path; a missing node.
    let rev = fmt::GPath::from_upstream(&[e("Bob"), -p("like") - 1, e("Alice")]);
    let mut s = String::new();
    fmt::push_path_json(&mut s, &ctx, eid(&db, "Bob"), eid(&db, "Alice"), &rev);
    assert!(
        s.contains(r#"{"fromNode":0,"toNode":1,"predIRI":"<like>"}"#),
        "{s}"
    );
    let mut s = String::new();
    fmt::push_path_json(
        &mut s,
        &ctx,
        eid(&db, "Bob"),
        eid(&db, "Bob"),
        &fmt::GPath::empty(),
    );
    assert_eq!(s, r#"{"src":"<Bob>","dst":"<Bob>","edges":[],"nodes":[]}"#);
    let missing = GNode::MISSING;
    let mut s = String::new();
    fmt::push_path_json(
        &mut s,
        &ctx,
        missing.entity,
        missing.entity,
        &fmt::GPath::single(missing.entity),
    );
    assert_eq!(
        s,
        r#"{"src":"","dst":"","edges":[],"nodes":[{"nodeIndex":-1,"nodeIRI":""}]}"#
    );

    // vertVec2JSON.
    let mut s = String::new();
    fmt::push_vert_list_json(&mut s, &ctx, &[eid(&db, "Alice"), eid(&db, "Bob")]);
    assert_eq!(s, r#"["<Alice>","<Bob>"]"#);
}

#[test]
fn value_and_number_helpers() {
    assert_eq!(fmt::cxx_to_string(1.0 / 1.8), "0.555556");
    assert_eq!(fmt::cxx_to_string(0.5), "0.500000");
    assert_eq!(fmt::cxx_g(0.0429273), "0.0429273");
    assert_eq!(fmt::cxx_g_f32(1.0 / 3.0), "0.333333");
    assert_eq!(fmt::nlohmann_f64(1.0), "1.0");
    assert_eq!(
        cxx::unordered_iteration_order([4, 3, 1, 0]),
        vec![0, 1, 3, 4]
    );
    assert_eq!(
        GFunValue::Json(fmt::paths_json("")),
        GFunValue::Json("{\"paths\":[]}".into())
    );
}
