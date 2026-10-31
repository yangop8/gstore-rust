//! Data test: the per-predicate CSR behind the advanced graph functions
//! (upstream `GeneralEvaluation::loadCSR`), its predicate-set semantics, its
//! per-database cache, and the graph-function dispatch skeleton.

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use gstore::analytics::csr::{CsrGraph, Dir, NodeId, PredFilter};
use gstore::analytics::gfun::{self, GArgs, GFnKind, GFunCtx};
use gstore::{Database, GStoreError};

/// The documentation's social graph (`SPARQL_QUERY.md` "Sample data").
const SOCIAL: &str = "\
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

fn tmp_dir(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("gstore_csr_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn ent(db: &Database, name: &str) -> u32 {
    db.dict().entity_id(&format!("<{name}>")).unwrap_or_else(|| panic!("no entity {name}"))
}

fn pred(db: &Database, name: &str) -> u32 {
    db.dict().predicate_id(&format!("<{name}>")).unwrap_or_else(|| panic!("no predicate {name}"))
}

/// `(neighbour entity, predicate, base direction)` in traversal order.
fn walk(g: &CsrGraph, node: NodeId, f: &PredFilter, dir: Dir) -> Vec<(u32, u32, Dir)> {
    let mut v = Vec::new();
    let _ = g.try_for_each::<(), _>(node, f, dir, |n, p, d| {
        v.push((g.entity_of(n), p, d));
        ControlFlow::Continue(())
    });
    v
}

#[test]
fn sample_graph_counts_match_upstream() {
    let db = Database::build_from_str("social", SOCIAL).unwrap();
    let g = db.csr().unwrap();
    // Six people, fifteen entity–entity edges.
    assert_eq!(g.num_nodes(), 6);
    assert_eq!(g.num_edges(), 15);
    assert_eq!(g.predicates().len(), 3);
    assert_eq!(g.pred_edge_count(pred(&db, "like")), 5);
    assert_eq!(g.pred_edge_count(pred(&db, "focus")), 5);
    assert_eq!(g.pred_edge_count(pred(&db, "dislike")), 5);
    // Node ids follow ascending entity ids.
    let ents = g.entities().to_vec();
    let mut sorted = ents.clone();
    sorted.sort_unstable();
    assert_eq!(ents, sorted);
    for (i, &e) in ents.iter().enumerate() {
        assert_eq!(g.node_of(e), Some(i as NodeId));
        assert_eq!(g.entity_of(i as NodeId), e);
    }
}

#[test]
fn csr_keeps_entities_only_per_predicate_in_both_directions() {
    let data = "\
<a> <p> <b> .
<a> <q> <b> .
<a> <p> <c> .
<a> <name> \"Alice\" .
<d> <name> \"lonely\" .
<c> <p> <a> .
<e> <q> <e> .
";
    let db = Database::build_from_str("t", data).unwrap();
    let g = db.csr().unwrap();
    let (a, b, c, e) = (ent(&db, "a"), ent(&db, "b"), ent(&db, "c"), ent(&db, "e"));
    let (p, q, name) = (pred(&db, "p"), pred(&db, "q"), pred(&db, "name"));
    // Literal objects are skipped: <d> only has a literal, so it is no vertex.
    assert_eq!(g.node_of(ent(&db, "d")), None);
    assert_eq!(g.num_nodes(), 4, "a, b, c, e");
    assert_eq!(g.num_edges(), 5, "a-p-b, a-q-b, a-p-c, c-p-a, e-q-e");
    assert_eq!(g.pred_edge_count(name), 0);
    assert!(!g.predicates().contains(&name));
    assert!(g.pred_slots() > name, "literal-only predicates still count towards the id space");

    let na = g.node_of(a).unwrap();
    let nb = g.node_of(b).unwrap();
    // Duplicate (s, o) pairs under different predicates stay separate edges.
    let out_b: Vec<u32> = g.out_neighbors(na, p).iter().map(|&n| g.entity_of(n)).collect();
    let mut want = vec![b, c];
    want.sort_unstable();
    assert_eq!(out_b, want);
    assert_eq!(g.out_neighbors(na, q).iter().map(|&n| g.entity_of(n)).collect::<Vec<_>>(), vec![b]);
    assert_eq!(g.in_neighbors(nb, p).iter().map(|&n| g.entity_of(n)).collect::<Vec<_>>(), vec![a]);
    assert_eq!(g.in_neighbors(nb, q).iter().map(|&n| g.entity_of(n)).collect::<Vec<_>>(), vec![a]);
    let all = PredFilter::all(db.dict().predicate_num() as u32);
    assert_eq!(g.out_degree(na, &all), 3);
    assert_eq!(g.in_degree(na, &all), 1);
    assert_eq!(g.degree(na, &all, false), 4);
    assert_eq!(g.total_out_degree(na), 3);
    assert_eq!(g.total_in_degree(nb), 2);
    // Self-loops are kept and appear in both directions.
    let ne = g.node_of(e).unwrap();
    assert_eq!(g.out_neighbors(ne, q), &[ne]);
    assert_eq!(g.in_neighbors(ne, q), &[ne]);
    assert_eq!(g.degree(ne, &all, false), 2);
    // Per predicate (ascending id), out first then in.
    let mut want_walk = Vec::new();
    let mut preds = vec![p, q];
    preds.sort_unstable();
    for pr in preds {
        let mut outs: Vec<u32> = if pr == p { vec![b, c] } else { vec![b] };
        outs.sort_unstable();
        want_walk.extend(outs.into_iter().map(|x| (x, pr, Dir::Out)));
        if pr == p {
            want_walk.push((c, p, Dir::In));
        }
    }
    assert_eq!(walk(&g, na, &all, Dir::OutIn), want_walk);
    assert_eq!(walk(&g, na, &all, Dir::Out).len(), 3);
    assert_eq!(walk(&g, na, &all, Dir::In), vec![(c, p, Dir::In)]);
}

#[test]
fn pred_filter_resolves_with_upstream_semantics() {
    let db = Database::build_from_str("social", SOCIAL).unwrap();
    let g = db.csr().unwrap();
    let dict = db.dict();
    let (like, focus, dislike) = (pred(&db, "like"), pred(&db, "focus"), pred(&db, "dislike"));
    let none: [&str; 0] = [];

    // {} → every predicate, ascending id.
    let all = PredFilter::resolve(&none, &none, dict);
    assert!(all.is_all());
    let mut every = vec![like, focus, dislike];
    every.sort_unstable();
    assert_eq!(all.pred_ids(&g), every);
    assert_eq!(g.num_edges_in(&all), 15);

    // A positive list keeps its order, drops unknown IRIs and duplicates;
    // bare and bracketed IRIs are both accepted.
    let inc = PredFilter::resolve(&["focus", "<nope>", "<like>", "focus"], &none, dict);
    assert_eq!(inc.pred_ids(&g), vec![focus, like]);
    assert!(inc.allows(like) && !inc.allows(dislike));
    assert_eq!(g.num_edges_in(&inc), 10);

    // Only unknown IRIs → the empty set (no edge qualifies).
    let empty = PredFilter::resolve(&["<nope>"], &none, dict);
    assert!(empty.is_empty(&g));
    assert_eq!(g.num_edges_in(&empty), 0);
    let alice = g.node_of(ent(&db, "Alice")).unwrap();
    assert_eq!(g.out_degree(alice, &empty), 0);

    // Any exclusion → all predicates except the excluded ones; positive
    // entries are then ignored, unknown exclusions too.
    let ex = PredFilter::resolve(&["like"], &["dislike", "nope"], dict);
    let mut rest = vec![like, focus];
    rest.sort_unstable();
    assert_eq!(ex.pred_ids(&g), rest);
    assert!(ex.allows(like) && ex.allows(focus) && !ex.allows(dislike));
    assert_eq!(g.num_edges_in(&ex), 10);
    // Alice: focus->Bob, like->Bob, dislike->Eve (excluded).
    assert_eq!(g.out_degree(alice, &ex), 2);

    // Excluding everything leaves the empty set.
    let ex_all = PredFilter::resolve(&none, &["like", "focus", "dislike"], dict);
    assert!(ex_all.is_empty(&g));
    assert!(!all.is_empty(&g));
}

#[test]
fn csr_is_cached_until_the_data_changes() {
    let mut db = Database::build_from_str("c", "<a> <p> <b> .\n").unwrap();
    assert!(!db.csr_is_loaded());
    db.preload_csr().unwrap();
    assert!(db.csr_is_loaded());
    let g1 = db.csr().unwrap();
    let g2 = db.csr().unwrap();
    assert!(Arc::ptr_eq(&g1, &g2), "unchanged data reuses the cached graph");

    // Read queries do not invalidate it.
    db.query("SELECT ?s WHERE { ?s ?p ?o }").unwrap();
    assert!(Arc::ptr_eq(&g1, &db.csr().unwrap()));

    // INSERT DATA.
    db.query("INSERT DATA { <b> <p> <c> . }").unwrap();
    assert!(!db.csr_is_loaded());
    let g3 = db.csr().unwrap();
    assert!(!Arc::ptr_eq(&g1, &g3));
    assert_eq!((g1.num_edges(), g3.num_edges()), (1, 2));
    // The old snapshot stays valid for whoever holds it.
    assert_eq!(g1.num_nodes(), 2);

    // A no-op insert (already present) keeps the cache.
    db.query("INSERT DATA { <b> <p> <c> . }").unwrap();
    assert!(Arc::ptr_eq(&g3, &db.csr().unwrap()));

    // A literal-object insert changes the store, so the CSR is rebuilt (with
    // the same shape).
    db.query("INSERT DATA { <a> <name> \"x\" . }").unwrap();
    let g4 = db.csr().unwrap();
    assert!(!Arc::ptr_eq(&g3, &g4));
    assert_eq!(g4.num_edges(), 2);

    // DELETE DATA.
    db.query("DELETE DATA { <a> <p> <b> . }").unwrap();
    assert_eq!(db.csr().unwrap().num_edges(), 1);

    // Direct-API insert.
    db.insert_triple(&gstore::Triple::new(
        gstore::Term::iri("x"),
        gstore::Term::iri("p"),
        gstore::Term::iri("y"),
    ));
    assert_eq!(db.csr().unwrap().num_edges(), 2);
}

#[test]
fn csr_follows_transactions_load_and_clear() {
    let mut db = Database::build_from_str("t", "<a> <p> <b> .\n").unwrap();
    assert_eq!(db.csr().unwrap().num_edges(), 1);

    // Inside a transaction the CSR sees the uncommitted writes (same store).
    db.begin().unwrap();
    db.query("INSERT DATA { <b> <p> <c> . <c> <p> <d> . }").unwrap();
    assert_eq!(db.csr().unwrap().num_edges(), 3);
    db.rollback().unwrap();
    let g = db.csr().unwrap();
    assert_eq!(g.num_edges(), 1, "rollback restores the graph");
    assert_eq!(g.num_nodes(), 2);

    // Committed writes stay.
    db.begin().unwrap();
    db.query("INSERT DATA { <b> <p> <c> . }").unwrap();
    db.commit().unwrap();
    assert_eq!(db.csr().unwrap().num_edges(), 2);

    // LOAD.
    let dir = tmp_dir("load");
    let file = dir.join("extra.nt");
    std::fs::write(&file, "<c> <q> <d> .\n<d> <q> <e> .\n").unwrap();
    db.query(&format!("LOAD <file://{}>", file.display())).unwrap();
    let g = db.csr().unwrap();
    assert_eq!(g.num_edges(), 4);
    assert_eq!(g.num_nodes(), 5);

    // Named graphs are not part of the CSR (upstream has a single graph)...
    db.query("INSERT DATA { GRAPH <g1> { <x> <p> <y> . } }").unwrap();
    assert_eq!(db.csr().unwrap().num_edges(), 4);

    // ... and CLEAR DEFAULT empties it.
    db.query("CLEAR DEFAULT").unwrap();
    let g = db.csr().unwrap();
    assert_eq!((g.num_nodes(), g.num_edges()), (0, 0));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn csr_graph_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CsrGraph>();
    // The database (with its cache) still moves between threads, and the
    // cached graph is shared by reference count.
    let db = Database::build_from_str("social", SOCIAL).unwrap();
    let (db, g1) = std::thread::spawn(move || {
        let g = db.csr().unwrap();
        (db, g)
    })
    .join()
    .unwrap();
    let edges: Vec<u64> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..4).map(|_| s.spawn(|| g1.num_edges())).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(edges, vec![15; 4]);
    assert!(Arc::ptr_eq(&g1, &db.csr().unwrap()));
}

/// Temporary: every graph function is reachable through the dispatcher and,
/// while it is still a stub, fails with the clean not-implemented error that
/// names it. (The path/metric packages replace the stubs and add real
/// expectations; an implemented function must return its declared kind for
/// these complete arguments.)
#[test]
fn gfun_dispatch_reaches_every_function() {
    let db = Database::build_from_str("social", SOCIAL).unwrap();
    let g = db.csr().unwrap();
    let ctx = GFunCtx::new(&g, db.dict());
    let alice = ent(&db, "Alice");
    let args = GArgs {
        srcs: vec![alice],
        dsts: vec![ent(&db, "Carol")],
        src_given: true,
        directed: true,
        k: Some(2),
        ret_num: Some(3),
        preds: PredFilter::all(db.dict().predicate_num() as u32),
        alpha: Some(0.85),
        max_iter: Some(10.0),
        tol: Some(1e-6),
        increase: Some(0.1),
        extra: vec![1.0, 1.0, 0.0, 0.0, 0.0, 0.0],
        empty_where: true,
    };
    for kind in GFnKind::ALL {
        match gfun::eval(kind, &ctx, &args) {
            Err(e) => {
                assert!(gfun::is_not_implemented(&e), "{kind}: unexpected error {e}");
                assert!(e.to_string().contains(kind.name()), "{kind}: {e}");
            }
            Ok(v) => {
                let got = match v {
                    gfun::GFunValue::Json(_) => gfun::GFunValueKind::Json,
                    gfun::GFunValue::Bool(_) => gfun::GFunValueKind::Bool,
                    gfun::GFunValue::Int(_) => gfun::GFunValueKind::Int,
                    gfun::GFunValue::Num(_) => gfun::GFunValueKind::Num,
                };
                assert_eq!(got, kind.output_kind(), "{kind}");
            }
        }
    }
    // Cancellation is checked before dispatch.
    let flag = AtomicBool::new(true);
    let cancelled = GFunCtx::new(&g, db.dict()).with_cancel(&flag);
    assert!(matches!(
        gfun::eval(GFnKind::Wcc, &cancelled, &args),
        Err(GStoreError::Cancelled)
    ));
    // Similarity is a pure function and already available.
    assert_eq!(gfun::metrics::jaro_similarity("abc", "abc"), 1.0);
    assert_eq!(ctx.term(alice), "<Alice>");
    assert_eq!(ctx.pred_iri(pred(&db, "like")), "<like>");
    assert_eq!(ctx.term(gfun::INVALID_ENTITY_LITERAL_ID), "");

    // Upstream-style positional neighbour access on entity ids.
    let (like, focus) = (pred(&db, "like"), pred(&db, "focus"));
    let (bob, carol) = (i64::from(ent(&db, "Bob")), i64::from(ent(&db, "Carol")));
    let a = i64::from(alice);
    assert_eq!(ctx.get_out_size(a, like), 1);
    assert_eq!(ctx.get_out_vert_id(a, like, 0), bob);
    assert_eq!(ctx.get_out_vert_id(a, like, 1), -1, "past the end is -1");
    // Carol is liked by Eve and Francis (ascending entity id).
    let mut likers = [i64::from(ent(&db, "Eve")), i64::from(ent(&db, "Francis"))];
    likers.sort_unstable();
    assert_eq!(ctx.get_in_size(carol, like), 2);
    assert_eq!(
        (ctx.get_in_vert_id(carol, like, 0), ctx.get_in_vert_id(carol, like, 1)),
        (likers[0], likers[1])
    );
    assert_eq!(ctx.get_in_size(carol, focus), 0);
    assert_eq!((ctx.get_out_size(-1, like), ctx.get_out_vert_id(-1, like, 0)), (0, -1));
}
