//! Data test: the resource limits of gStore's advanced graph functions
//! (review analytics#8, analytics#11, gap-gfun-fuzz#2, #3, #4, #5, #6).
//!
//! * **Mid-run cancellation.** Every long-running metric polls the
//!   cancellation flag inside its long loops — a giant component, a hub, a
//!   sweep — not only between outer iterations. The tests raise the flag
//!   *from inside* a loop (a [`gfun::debug_poll_hook`] that fires at one of
//!   the function's polls), never before the call, and check that the call
//!   stops at that very poll.
//! * **Iteration bound.** `PR` with `tol < 0` and `louvain` with
//!   `increase < 0` never converge; they stop at the iteration-work bound
//!   with an error instead of running up to 2^31 sweeps.
//! * **Output bounds.** `maximumKplex` / `coreTruss` stop at
//!   `gfun_max_output_bytes` while they build their text (measured with a
//!   counting allocator); the bound is a per-query budget that every
//!   graph-function call of the query is charged against, GROUP BY groups
//!   and `GRAPH` blocks included.
//!
//! The poll hook and the allocation counter are process-wide, so every test
//! here holds [`serial`] for its whole run.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use gstore::analytics::csr::{CsrGraph, PredFilter};
use gstore::analytics::gfun::{self, GArgs, GFnKind, GFunCtx, GFunValue, GNode};
use gstore::dict::Dictionary;
use gstore::error::GStoreError;
use gstore::model::IdTriple;
use gstore::Database;

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// The system allocator, counting the bytes live now and the peak since
/// [`reset_peak`].
struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = CURRENT.fetch_add(by, Ordering::SeqCst) + by;
    PEAK.fetch_max(now, Ordering::SeqCst);
}

// SAFETY: every method forwards to `System` with the caller's arguments; the
// counters do not affect allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
        if !p.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Start a new peak measurement; returns the bytes live now.
fn reset_peak() -> usize {
    let now = CURRENT.load(Ordering::SeqCst);
    PEAK.store(now, Ordering::SeqCst);
    now
}

/// One test at a time (the poll hook and the allocation counter are
/// process-wide).
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes the poll hook when dropped (also when the test fails).
struct HookGuard;

impl HookGuard {
    fn install(hook: impl Fn(&'static str) + Send + Sync + 'static) -> HookGuard {
        gfun::debug_poll_hook(Some(Arc::new(hook)));
        HookGuard
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        gfun::debug_poll_hook(None);
    }
}

/// A graph over predicate 0 from `(s, o)` entity-id pairs.
fn graph(edges: impl IntoIterator<Item = (u32, u32)>) -> CsrGraph {
    CsrGraph::from_id_triples(
        edges
            .into_iter()
            .map(|(s, o)| IdTriple::new(s, 0, o))
            .collect(),
    )
    .unwrap()
}

/// `0 → 1 → … → n-1`.
fn chain(n: u32) -> CsrGraph {
    graph((0..n - 1).map(|i| (i, i + 1)))
}

fn src(g: &CsrGraph, e: u32) -> Vec<GNode> {
    vec![GNode::from_node(g, g.node_of(e).unwrap())]
}

fn args(g: &CsrGraph, f: impl FnOnce(&mut GArgs)) -> GArgs {
    let _ = g;
    let mut a = GArgs {
        preds: PredFilter::from_ids([0]),
        ..GArgs::default()
    };
    f(&mut a);
    a
}

/// A watchdog that raises `flag` after `secs` seconds unless dropped first:
/// a hang guard only (a test that reaches it fails on the `Cancelled` it
/// causes, never passes because of it).
struct Watchdog(Option<std::sync::mpsc::Sender<()>>);

impl Watchdog {
    fn arm(flag: &Arc<AtomicBool>, secs: u64) -> Watchdog {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let flag = Arc::clone(flag);
        std::thread::spawn(move || {
            if rx.recv_timeout(Duration::from_secs(secs)).is_err() {
                flag.store(true, Ordering::SeqCst);
            }
        });
        Watchdog(Some(tx))
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// mid-run cancellation (analytics#8, analytics#11)
// ---------------------------------------------------------------------------

/// `kind` polls the loop `site` repeatedly while it runs, and a flag raised
/// at one of those polls (from inside the loop, as a `/cancel` arriving
/// meanwhile would) stops the call right there, at that poll.
fn stops_at_the_next_poll(kind: GFnKind, g: &CsrGraph, a: &GArgs, site: &'static str) {
    let d = Dictionary::new();
    // 1. Uncancelled: the loop polls more than once.
    let calls = Arc::new(AtomicUsize::new(0));
    {
        let calls = Arc::clone(&calls);
        let _hook = HookGuard::install(move |s| {
            if s == site {
                calls.fetch_add(1, Ordering::SeqCst);
            }
        });
        let r = gfun::eval(kind, &GFunCtx::new(g, &d), a);
        assert!(r.is_ok(), "{kind}: {r:?}");
    }
    let polls = calls.load(Ordering::SeqCst);
    assert!(
        polls >= 2,
        "{kind}: the loop `{site}` polled the cancellation flag {polls} time(s)"
    );
    // 2. The flag goes up at the first poll of `site`: the call stops there
    //    (no further poll of any loop runs after it).
    let flag = Arc::new(AtomicBool::new(false));
    let after = Arc::new(AtomicUsize::new(0));
    let r = {
        let (flag2, after2) = (Arc::clone(&flag), Arc::clone(&after));
        let _hook = HookGuard::install(move |s| {
            if flag2.load(Ordering::SeqCst) {
                after2.fetch_add(1, Ordering::SeqCst);
            } else if s == site {
                flag2.store(true, Ordering::SeqCst);
            }
        });
        gfun::eval(kind, &GFunCtx::new(g, &d).with_cancel(&flag), a)
    };
    assert!(flag.load(Ordering::SeqCst), "{kind}: `{site}` never polled");
    assert!(
        matches!(r, Err(GStoreError::Cancelled)),
        "{kind}: expected Cancelled, got {r:?}"
    );
    assert_eq!(
        after.load(Ordering::SeqCst),
        0,
        "{kind}: kept running past the poll that saw the flag"
    );
}

#[test]
fn wcc_stops_inside_a_giant_component() {
    let _s = serial();
    // One component: every poll after the first outer step is inside its BFS.
    let g = chain(20_000);
    stops_at_the_next_poll(GFnKind::Wcc, &g, &args(&g, |_| {}), "wcc");
}

#[test]
fn pr_stops_inside_a_sweep() {
    let _s = serial();
    let g = chain(20_000);
    let a = args(&g, |a| {
        a.directed = true;
        a.alpha = Some(0.85);
        a.max_iter = Some(3.0);
        a.tol = Some(1e-9);
        a.ret_num = Some(1);
    });
    stops_at_the_next_poll(GFnKind::Pr, &g, &a, "pr");
}

#[test]
fn label_prop_stops_inside_an_iteration() {
    let _s = serial();
    let g = chain(20_000);
    let a = args(&g, |a| a.max_iter = Some(2.0));
    stops_at_the_next_poll(GFnKind::LabelProp, &g, &a, "labelProp");
}

#[test]
fn louvain_stops_inside_a_sweep() {
    let _s = serial();
    let g = chain(300);
    let a = args(&g, |a| {
        a.directed = true;
        a.max_iter = Some(2.0);
        a.increase = Some(0.0);
    });
    stops_at_the_next_poll(GFnKind::Louvain, &g, &a, "louvain");
}

#[test]
fn triangle_counting_stops_inside_its_vertex_loop() {
    let _s = serial();
    let g = chain(20_000);
    stops_at_the_next_poll(
        GFnKind::TriangleCounting,
        &g,
        &args(&g, |_| {}),
        "triangleCounting",
    );
}

#[test]
fn closeness_stops_inside_its_bfs() {
    let _s = serial();
    let g = chain(20_000);
    let a = args(&g, |a| a.srcs = src(&g, 0));
    stops_at_the_next_poll(GFnKind::ClosenessCentrality, &g, &a, "closenessCentrality");
}

#[test]
fn betweenness_stops_inside_its_bfs() {
    let _s = serial();
    let g = chain(300);
    let a = args(&g, |a| {
        a.srcs = src(&g, 150);
        a.directed = true;
    });
    stops_at_the_next_poll(
        GFnKind::BetweennessCentrality,
        &g,
        &a,
        "betweennessCentrality",
    );
}

#[test]
fn cluster_coeff_stops_inside_its_vertex_loop() {
    let _s = serial();
    // a hub with 20 000 leaves: its neighbour set alone is one long step
    let g = graph((1..=20_000).map(|i| (0, i)));
    stops_at_the_next_poll(GFnKind::ClusterCoeff, &g, &args(&g, |_| {}), "clusterCoeff");
    let a = args(&g, |a| a.srcs = src(&g, 0));
    stops_at_the_next_poll(GFnKind::ClusterCoeff, &g, &a, "clusterCoeff");
}

#[test]
fn diameter_and_confidence_stop_inside_their_bfs() {
    let _s = serial();
    let g = chain(500);
    stops_at_the_next_poll(
        GFnKind::DiameterEstimation,
        &g,
        &args(&g, |_| {}),
        "diameterEstimation",
    );
    stops_at_the_next_poll(
        GFnKind::ConfidenceDegree,
        &g,
        &args(&g, |_| {}),
        "confidenceDegree",
    );
}

#[test]
fn degree_correlation_stops_inside_its_bfs() {
    let _s = serial();
    // `k < 0` means 999 hops; a hub with 20 000 leaves is one hop deep
    let g = graph((1..=20_000).map(|i| (0, i)));
    let a = args(&g, |a| {
        a.srcs = src(&g, 0);
        a.k = Some(-1);
    });
    stops_at_the_next_poll(GFnKind::DegreeCorrelation, &g, &a, "degreeCorrelation");
}

#[test]
fn jaccard_stops_inside_a_hub_neighbourhood() {
    let _s = serial();
    let g = graph((1..=5_000).map(|i| (0, i)));
    let a = args(&g, |a| {
        a.srcs = src(&g, 0);
        a.k = Some(2);
        a.ret_num = Some(10);
    });
    stops_at_the_next_poll(GFnKind::JaccardSimilarity, &g, &a, "jaccard");
}

/// A pseudo-random simple graph (`n` vertices, edge probability
/// `p_percent`) over predicate 0.
fn random_graph(n: u32, p_percent: u64, seed: u64) -> CsrGraph {
    let mut x = seed;
    let mut edges = Vec::new();
    for a in 0..n {
        for b in a + 1..n {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if (x >> 33) % 100 < p_percent {
                edges.push((a, b));
            }
        }
    }
    graph(edges)
}

/// `maximumKplex`'s branch-and-bound `Searcher` polls inside its `O(n²)`
/// initialisation (review gap-gfun-fuzz#3), not only in its search.
#[test]
fn kplex_stops_inside_the_searcher_initialisation() {
    let _s = serial();
    let g = random_graph(40, 50, 12345);
    let a = args(&g, |a| a.k = Some(2));
    stops_at_the_next_poll(GFnKind::MaximumKplex, &g, &a, "kplex-init");
}

/// The path family's inner loops report through the same hook.
#[test]
fn path_functions_stop_inside_their_search() {
    let _s = serial();
    let g = chain(20_000);
    let a = args(&g, |a| {
        a.srcs = src(&g, 0);
        a.directed = true;
    });
    stops_at_the_next_poll(GFnKind::BfsCount, &g, &a, "paths");
}

// ---------------------------------------------------------------------------
// iteration bound (gap-gfun-fuzz#2)
// ---------------------------------------------------------------------------

fn expect_iteration_bound(r: gstore::error::Result<GFunValue>, kind: GFnKind) {
    match r {
        Err(GStoreError::Query(m)) => {
            assert!(m.contains("iteration bound"), "{kind}: {m}");
            assert!(m.starts_with(kind.name()), "{kind}: {m}");
        }
        other => panic!("{kind}: expected the iteration bound, got {other:?}"),
    }
}

#[test]
fn pr_with_a_negative_tolerance_stops_at_the_iteration_bound() {
    let _s = serial();
    let d = Dictionary::new();
    let g = graph([(0, 1), (1, 2), (2, 0)]);
    let flag = Arc::new(AtomicBool::new(false));
    let _dog = Watchdog::arm(&flag, 120);
    // `maxIter = 2^32` saturates to 2^31 - 1 sweeps; `tol < 0` never converges.
    let a = args(&g, |a| {
        a.directed = true;
        a.alpha = Some(0.85);
        a.max_iter = Some(4_294_967_296.0);
        a.tol = Some(-1.0);
        a.ret_num = Some(3);
    });
    let ctx = GFunCtx::new(&g, &d)
        .with_cancel(&flag)
        .with_max_iteration_work(1_000_000);
    expect_iteration_bound(gfun::eval(GFnKind::Pr, &ctx, &a), GFnKind::Pr);
    // A converging run under the same bound is not affected.
    let a = args(&g, |a| {
        a.directed = true;
        a.alpha = Some(0.85);
        a.max_iter = Some(1000.0);
        a.tol = Some(1e-6);
        a.ret_num = Some(3);
    });
    assert!(gfun::eval(GFnKind::Pr, &ctx, &a).is_ok());
    assert!(!flag.load(Ordering::SeqCst), "the hang guard fired");
}

#[test]
fn louvain_with_a_negative_increase_stops_at_the_iteration_bound() {
    let _s = serial();
    let d = Dictionary::new();
    // review gap-gfun-fuzz#2's repro: this graph never stops moving vertices
    // with increase = -1 (p = 0, q = 1)
    let g = CsrGraph::from_id_triples(
        [
            (2, 0, 0),
            (2, 1, 0),
            (1, 0, 1),
            (2, 1, 2),
            (0, 1, 1),
            (1, 1, 0),
            (2, 0, 1),
        ]
        .into_iter()
        .map(|(s, p, o)| IdTriple::new(s, p, o))
        .collect(),
    )
    .unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let _dog = Watchdog::arm(&flag, 120);
    let a = GArgs {
        directed: true,
        preds: PredFilter::from_ids([0, 1]),
        max_iter: Some(4_294_967_296.0),
        increase: Some(-1.0),
        ..GArgs::default()
    };
    let ctx = GFunCtx::new(&g, &d)
        .with_cancel(&flag)
        .with_max_iteration_work(1_000_000);
    expect_iteration_bound(gfun::eval(GFnKind::Louvain, &ctx, &a), GFnKind::Louvain);
    // The same call with a sane threshold converges under the bound.
    let a = GArgs {
        increase: Some(0.0),
        ..a
    };
    assert!(gfun::eval(GFnKind::Louvain, &ctx, &a).is_ok());
    assert!(!flag.load(Ordering::SeqCst), "the hang guard fired");
}

#[test]
fn the_default_iteration_bound_is_documented() {
    let _s = serial();
    let g = CsrGraph::empty();
    let d = Dictionary::new();
    assert_eq!(
        GFunCtx::new(&g, &d).max_iteration_work(),
        gfun::MAX_ITERATION_WORK
    );
    assert_eq!(gfun::MAX_ITERATION_WORK, 1 << 34);
}

// ---------------------------------------------------------------------------
// output bounds while building (gap-gfun-fuzz#4)
// ---------------------------------------------------------------------------

/// `maximumKplex`'s fake solution names CSR vertex 0 `2k - 2` times; with a
/// long IRI that is tens of MB of text. The call must fail at the bound
/// having allocated about the bound, not the whole text (twice).
#[test]
fn kplex_fake_solution_stops_at_the_output_bound_while_built() {
    let _s = serial();
    const BOUND: usize = 1 << 20;
    let long = format!("<http://example.org/{}>", "x".repeat(180));
    let db = Database::build_from_str("kplex-bound", &format!("{long} <http://p> <http://b> .\n"))
        .unwrap();
    let csr = db.csr().unwrap();
    let ctx = GFunCtx::new(&csr, db.dict()).with_max_output_bytes(BOUND);
    let a = GArgs {
        k: Some(65_536), // 131 070 copies of a 200-byte IRI: 26 MB
        preds: PredFilter::resolve::<&str>(&[], &[], db.dict()),
        ..GArgs::default()
    };
    let base = reset_peak();
    let r = gfun::eval(GFnKind::MaximumKplex, &ctx, &a);
    let used = PEAK.load(Ordering::SeqCst) - base;
    match r {
        Err(GStoreError::Query(m)) => assert!(m.contains("bytes of output"), "{m}"),
        other => panic!("expected the output bound, got {other:?}"),
    }
    // the kplex vector itself is 512 KB; the text stops near the bound
    assert!(
        used < BOUND + (3 << 20),
        "allocated {used} bytes before failing at a {BOUND}-byte bound"
    );
    // Small k still answers (the fake solution of 2k - 2 = 4 copies).
    let ok = GArgs { k: Some(3), ..a };
    match gfun::eval(GFnKind::MaximumKplex, &ctx, &ok).unwrap() {
        GFunValue::Json(t) => assert_eq!(
            t,
            format!("{{\"paths\":[{{\"kplex\":[{l},{l},{l},{l}]}}]}}", l = long)
        ),
        v => panic!("{v:?}"),
    }
}

/// `coreTruss` names (shrunk) vertex 0 once per peeled vertex; the text is
/// bounded while built too.
#[test]
fn core_truss_stops_at_the_output_bound_while_built() {
    let _s = serial();
    const BOUND: usize = 16 << 10;
    // a triangle on a long-named vertex 0, plus 2 000 squares (core 2, no
    // triangle): coreTruss(2, 1) peels every square vertex
    let long = format!("<http://example.org/{}>", "y".repeat(2000));
    let mut nt = format!(
        "{long} <http://p> <http://t1> .\n<http://t1> <http://p> <http://t2> .\n<http://t2> <http://p> {long} .\n"
    );
    for i in 0..2000 {
        for (a, b) in [(0, 1), (1, 2), (2, 3), (3, 0)] {
            nt.push_str(&format!(
                "<http://s{i}_{a}> <http://p> <http://s{i}_{b}> .\n"
            ));
        }
    }
    let db = Database::build_from_str("core-truss-bound", &nt).unwrap();
    let csr = db.csr().unwrap();
    let a = GArgs {
        nums: vec![2.0, 1.0],
        preds: PredFilter::resolve::<&str>(&[], &[], db.dict()),
        ..GArgs::default()
    };
    // Unbounded, the text repeats the long name many times.
    let full = match gfun::eval(GFnKind::CoreTruss, &GFunCtx::new(&csr, db.dict()), &a).unwrap() {
        GFunValue::Json(t) => t,
        v => panic!("{v:?}"),
    };
    assert!(full.len() > 64 * BOUND, "{} bytes", full.len());
    let ctx = GFunCtx::new(&csr, db.dict()).with_max_output_bytes(BOUND);
    let base = reset_peak();
    let r = gfun::eval(GFnKind::CoreTruss, &ctx, &a);
    let used = PEAK.load(Ordering::SeqCst) - base;
    match r {
        Err(GStoreError::Query(m)) => assert!(m.contains("bytes of output"), "{m}"),
        other => panic!("expected the output bound, got {other:?}"),
    }
    assert!(
        used < full.len() / 4,
        "allocated {used} bytes before failing at a {BOUND}-byte bound (full text {} bytes)",
        full.len()
    );
}

// ---------------------------------------------------------------------------
// the per-query output budget (gap-gfun-fuzz#5)
// ---------------------------------------------------------------------------

/// `<s0> <p> <s1> . … <s{n-1}> <p> <sn> .` in the default graph.
fn chain_nt(n: usize) -> String {
    (0..n)
        .map(|i| format!("<http://x/s{i}> <http://x/p> <http://x/s{}> .\n", i + 1))
        .collect()
}

/// The length of `WCC({})`'s text on `db`.
fn wcc_len(db: &mut Database) -> usize {
    let rs = db.select("SELECT (WCC({}) AS ?w) WHERE {}").unwrap();
    rs.rows[0][0].as_ref().unwrap().len()
}

fn expect_budget_error<T: std::fmt::Debug>(r: gstore::error::Result<T>, what: &str) {
    match r {
        Err(GStoreError::Query(m)) => {
            assert!(m.contains("bytes of output in this query"), "{what}: {m}")
        }
        other => {
            let shown: String = format!("{other:?}").chars().take(300).collect();
            panic!("{what}: expected the per-query output budget, got {shown}")
        }
    }
}

/// Under GROUP BY every group shows the function's value: each group is
/// charged against the query's budget, so the bound holds for the query as
/// a whole, not per call.
#[test]
fn group_by_groups_share_the_output_budget() {
    let _s = serial();
    let mut db = Database::build_from_str("gfun-budget-groups", &chain_nt(40)).unwrap();
    let one = wcc_len(&mut db);
    // room for three values, not for ten
    db.set_gfun_max_output_bytes(3 * one + one / 2);
    let q = |n: usize| {
        format!("SELECT ?s (WCC({{}}) AS ?w) WHERE {{ ?s <http://x/p> ?o }} GROUP BY ?s LIMIT {n}")
    };
    let rs = db
        .select("SELECT ?s (WCC({}) AS ?w) WHERE { ?s <http://x/p> ?o FILTER(?s IN (<http://x/s1>, <http://x/s2>, <http://x/s3>)) } GROUP BY ?s")
        .unwrap();
    assert_eq!(rs.rows.len(), 3);
    expect_budget_error(db.select(&q(10)), "ten groups");
    db.set_gfun_max_output_bytes(400);
    // A per-group (variable) argument is charged the same way.
    expect_budget_error(
        db.select(
            "SELECT ?s (kHopCount(?s, true, 1, {}) AS ?x) WHERE { ?s <http://x/p> ?o } GROUP BY ?s",
        ),
        "groups with a variable argument",
    );
}

/// A graph function inside a `GRAPH` block charges the same budget as the
/// rest of the query.
#[test]
fn graph_blocks_share_the_output_budget() {
    let _s = serial();
    let mut db = Database::build_from_str("gfun-budget-graph", &chain_nt(40)).unwrap();
    let one = wcc_len(&mut db);
    let named: String = chain_nt(40).lines().map(|l| format!("{l}\n")).collect();
    db.query(&format!(
        "INSERT DATA {{ GRAPH <http://x/g> {{ {named} }} }}"
    ))
    .unwrap();
    let both = "SELECT ?a ?b WHERE { { SELECT (WCC({}) AS ?a) WHERE {} } \
                GRAPH <http://x/g> { { SELECT (WCC({}) AS ?b) WHERE {} } } }";
    // Either call alone fits, and so do both under a large enough budget.
    db.set_gfun_max_output_bytes(2 * one + 64);
    let rs = db.select(both).unwrap();
    assert_eq!(rs.rows.len(), 1, "{rs:?}");
    assert!(rs.rows[0].iter().all(Option::is_some), "{rs:?}");
    db.set_gfun_max_output_bytes(one + one / 2);
    assert_eq!(wcc_len(&mut db), one);
    expect_budget_error(db.select(both), "a GRAPH block");
}

// ---------------------------------------------------------------------------
// the legacy rs: built-ins (analytics#3, analytics#4)
// ---------------------------------------------------------------------------

/// A [`TripleSource`] that counts whole-store reads (`iter_all`, one
/// `so_by_p` per predicate) — what building a legacy view from the store
/// costs — and delegates everything.
struct ScanCounting<'a> {
    inner: &'a gstore::store::TripleStore,
    scans: AtomicUsize,
}

mod scan_counting {
    use super::ScanCounting;
    use gstore::model::id::{EntityLiteralId, PredId};
    use gstore::model::IdTriple;
    use gstore::store::TripleSource;
    use std::sync::atomic::Ordering;

    impl TripleSource for ScanCounting<'_> {
        fn exists(&self, s: EntityLiteralId, p: PredId, o: EntityLiteralId) -> bool {
            TripleSource::exists(self.inner, s, p, o)
        }
        fn po_by_s(&self, s: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
            TripleSource::po_by_s(self.inner, s)
        }
        fn o_by_sp(&self, s: EntityLiteralId, p: PredId) -> Vec<EntityLiteralId> {
            TripleSource::o_by_sp(self.inner, s, p)
        }
        fn p_by_so(&self, s: EntityLiteralId, o: EntityLiteralId) -> Vec<PredId> {
            TripleSource::p_by_so(self.inner, s, o)
        }
        fn ps_by_o(&self, o: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
            TripleSource::ps_by_o(self.inner, o)
        }
        fn s_by_po(&self, p: PredId, o: EntityLiteralId) -> Vec<EntityLiteralId> {
            TripleSource::s_by_po(self.inner, p, o)
        }
        fn so_by_p(&self, p: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
            self.scans.fetch_add(1, Ordering::SeqCst);
            TripleSource::so_by_p(self.inner, p)
        }
        fn subs_by_p(&self, p: PredId) -> Vec<EntityLiteralId> {
            TripleSource::subs_by_p(self.inner, p)
        }
        fn objs_by_p(&self, p: PredId) -> Vec<EntityLiteralId> {
            TripleSource::objs_by_p(self.inner, p)
        }
        fn subject_keys(&self) -> Vec<EntityLiteralId> {
            TripleSource::subject_keys(self.inner)
        }
        fn object_keys(&self) -> Vec<EntityLiteralId> {
            TripleSource::object_keys(self.inner)
        }
        fn triple_count(&self) -> u64 {
            TripleSource::triple_count(self.inner)
        }
        fn distinct_subjects(&self) -> usize {
            TripleSource::distinct_subjects(self.inner)
        }
        fn distinct_objects(&self) -> usize {
            TripleSource::distinct_objects(self.inner)
        }
        fn num_predicates(&self) -> usize {
            TripleSource::num_predicates(self.inner)
        }
        fn pred_card(&self, p: PredId) -> usize {
            TripleSource::pred_card(self.inner, p)
        }
        fn pred_distinct_subj(&self, p: PredId) -> usize {
            TripleSource::pred_distinct_subj(self.inner, p)
        }
        fn pred_distinct_obj(&self, p: PredId) -> usize {
            TripleSource::pred_distinct_obj(self.inner, p)
        }
        fn iter_all(&self) -> Vec<IdTriple> {
            self.scans.fetch_add(1, Ordering::SeqCst);
            TripleSource::iter_all(self.inner)
        }
    }
}

/// Entities with entity edges (a cycle, two predicates on one pair), an
/// entity with only a literal object, literal objects shared by two
/// subjects, and an entity used only as a predicate.
const MIXED: &str = "<http://x/a> <http://x/p> <http://x/b> .\n\
    <http://x/a> <http://x/q> <http://x/b> .\n\
    <http://x/b> <http://x/p> <http://x/c> .\n\
    <http://x/c> <http://x/p> <http://x/a> .\n\
    <http://x/c> <http://x/p> <http://x/d> .\n\
    <http://x/a> <http://x/name> \"Alice\" .\n\
    <http://x/d> <http://x/name> \"Alice\" .\n\
    <http://x/e> <http://x/name> \"Eve\" .\n";

const MIXED_TERMS: [&str; 7] = [
    "<http://x/a>",
    "<http://x/b>",
    "<http://x/c>",
    "<http://x/d>",
    "<http://x/e>",
    "\"Alice\"",
    "\"Eve\"",
];

/// With a CSR provider, the literal-independent legacy built-ins (cycles,
/// paths between entities) build their view from the provided CSR: the
/// store is never scanned (it was, once per query).
#[test]
fn literal_independent_legacy_built_ins_use_the_provided_csr() {
    use gstore::query::engine::CsrProvider;
    use gstore::query::{Evaluator, QueryResult};
    let _s = serial();
    let db = Database::build_from_str("legacy-csr", MIXED).unwrap();
    let csr = db.csr().unwrap();
    let counting = ScanCounting {
        inner: db.store(),
        scans: AtomicUsize::new(0),
    };
    let provider = move || Ok(Arc::clone(&csr));
    let provider: &CsrProvider<'_> = &provider;
    for q in [
        "SELECT ?c WHERE { BIND(rs:CYCLEBOOLEAN() AS ?c) }",
        "SELECT ?l WHERE { VALUES ?x { 1 2 3 } BIND(rs:SHORTESTPATHLEN(<http://x/a>, <http://x/d>) AS ?l) }",
        "SELECT ?l WHERE { BIND(rs:SSSPLEN(<http://x/e>, <http://x/e>) AS ?l) }",
        "SELECT ?l WHERE { BIND(rs:SSSP(<http://x/b>, <http://x/a>) AS ?l) }",
        "SELECT ?l WHERE { BIND(rs:KHOPREACHABLE(<http://x/a>, <http://x/d>, 2) AS ?l) }",
        "SELECT ?l WHERE { BIND(rs:SHORTESTPATHS(<http://x/a>, <http://x/e>) AS ?l) }",
    ] {
        let parsed = gstore::parser::sparql::parse(q).unwrap();
        let ev = Evaluator::new(db.dict(), &counting).with_csr_provider(provider);
        match ev.evaluate(&parsed).unwrap() {
            QueryResult::Select(rs) => assert!(rs.rows[0][0].is_some(), "{q}: {rs:?}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(counting.scans.load(Ordering::SeqCst), 0, "{q} scanned the store");
    }
    // The ones that count literal objects still build the full view.
    let parsed =
        gstore::parser::sparql::parse("SELECT ?n WHERE { BIND(rs:BFSCOUNT(<http://x/a>) AS ?n) }")
            .unwrap();
    let ev = Evaluator::new(db.dict(), &counting).with_csr_provider(provider);
    assert!(ev.evaluate(&parsed).is_ok());
    assert!(counting.scans.load(Ordering::SeqCst) > 0);
}

/// The entity view answers exactly what the full view (every triple,
/// literals as nodes) answers, for every pair of terms — entities with and
/// without entity edges, literals — and for cycles.
#[test]
fn legacy_path_built_ins_answer_as_on_the_full_view() {
    use gstore::analytics::GraphView;
    let _s = serial();
    let mut db = Database::build_from_str("legacy-paths", MIXED).unwrap();
    let full = GraphView::from_store(db.store());
    let id = |db: &Database, t: &str| {
        db.dict()
            .term_id(&gstore::parser::ntriples::parse_term(t).unwrap())
            .unwrap()
    };
    let cell = |db: &mut Database, expr: &str| -> Option<String> {
        let rs = db
            .select(&format!("SELECT ?v WHERE {{ BIND({expr} AS ?v) }}"))
            .unwrap();
        rs.rows[0][0].clone()
    };
    for a in MIXED_TERMS {
        for c in MIXED_TERMS {
            let (ia, ic) = (id(&db, a), id(&db, c));
            let len = full.shortest_path_len(ia, ic).map_or(-1, |x| x as i64);
            assert_eq!(
                cell(&mut db, &format!("rs:SHORTESTPATHLEN({a}, {c})")),
                Some(format!(
                    "\"{len}\"^^<http://www.w3.org/2001/XMLSchema#integer>"
                )),
                "SHORTESTPATHLEN({a}, {c})"
            );
            let reach = full.khop_reachable(ia, ic, 2).unwrap_or(false);
            assert_eq!(
                cell(&mut db, &format!("rs:KHOPREACHABLE({a}, {c}, 2)")),
                Some(format!(
                    "\"{reach}\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
                )),
                "KHOPREACHABLE({a}, {c})"
            );
            let count = full.count_shortest_paths(ia, ic);
            assert_eq!(
                cell(&mut db, &format!("rs:SHORTESTPATHS({a}, {c})")),
                count.map(|n| format!("\"{n}\"^^<http://www.w3.org/2001/XMLSchema#integer>")),
                "SHORTESTPATHS({a}, {c})"
            );
            let hops = full.shortest_path(ia, ic).map_or(0, |p| p.len());
            let sssp = cell(&mut db, &format!("rs:SSSP({a}, {c})")).unwrap();
            assert_eq!(
                sssp.matches(' ').count() + usize::from(sssp != "\"\""),
                hops,
                "SSSP({a}, {c}) = {sssp}"
            );
        }
    }
    assert_eq!(
        cell(&mut db, "rs:CYCLEBOOLEAN()"),
        Some(format!(
            "\"{}\"^^<http://www.w3.org/2001/XMLSchema#boolean>",
            full.has_cycle()
        ))
    );
}

/// `rs:LOUVAIN` gives the same partition on every run (it changed from run
/// to run on equal gains; review analytics#3).
#[test]
fn legacy_louvain_is_deterministic() {
    let _s = serial();
    let mut nt = String::new();
    for i in 1..=4 {
        nt.push_str(&format!("<http://x/c> <http://x/p> <http://x/l{i}> .\n"));
        nt.push_str(&format!("<http://x/l{i}> <http://x/p> <http://x/x{i}> .\n"));
    }
    let q = "SELECT ?o ?same WHERE { <http://x/c> <http://x/p> ?o \
             BIND(rs:LOUVAIN(<http://x/c>, ?o) AS ?same) } ORDER BY ?o";
    let mut answers = std::collections::BTreeSet::new();
    for _ in 0..12 {
        let mut db = Database::build_from_str("legacy-louvain", &nt).unwrap();
        let rs = db.select(q).unwrap();
        answers.insert(format!("{:?}", rs.rows));
    }
    assert_eq!(answers.len(), 1, "{answers:#?}");
}

/// A cancel raised while a legacy built-in builds its view or runs its
/// algorithm (from inside its loop) fails the query at that poll; it used
/// to run to the end (no poll at all).
#[test]
fn legacy_built_ins_stop_at_a_mid_run_cancel() {
    let _s = serial();
    let mut db = Database::build_from_str("legacy-cancel", &chain_nt(20_000)).unwrap();
    for q in [
        // full view (store build, then WCC)
        "SELECT ?n WHERE { BIND(rs:WCC(<http://x/s0>) AS ?n) }",
        // entity view (CSR → view, then Tarjan)
        "SELECT ?c WHERE { BIND(rs:CYCLEBOOLEAN() AS ?c) }",
        "SELECT ?c WHERE { BIND(rs:SHORTESTPATHLEN(<http://x/s0>, <http://x/s19999>) AS ?c) }",
    ] {
        let flag = Arc::new(AtomicBool::new(false));
        let after = Arc::new(AtomicUsize::new(0));
        let r = {
            let (flag2, after2) = (Arc::clone(&flag), Arc::clone(&after));
            let _hook = HookGuard::install(move |s| {
                if s != "graph-view" {
                    return;
                }
                if flag2.load(Ordering::SeqCst) {
                    after2.fetch_add(1, Ordering::SeqCst);
                } else {
                    flag2.store(true, Ordering::SeqCst);
                }
            });
            db.query_cancellable(q, Arc::clone(&flag))
        };
        assert!(flag.load(Ordering::SeqCst), "{q}: never polled");
        assert!(matches!(r, Err(GStoreError::Cancelled)), "{q}: {r:?}");
        assert_eq!(after.load(Ordering::SeqCst), 0, "{q}: ran past the poll");
    }
}

/// `rs:CYCLEBOOLEAN()` is computed once per query, not once per row.
#[test]
fn legacy_cycle_boolean_is_memoised_per_query() {
    let _s = serial();
    let mut db = Database::build_from_str("legacy-cycle-memo", &chain_nt(20_000)).unwrap();
    let polls = |db: &mut Database, rows: usize| {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        let _hook = HookGuard::install(move |s| {
            if s == "graph-view" {
                n2.fetch_add(1, Ordering::SeqCst);
            }
        });
        let q = format!(
            "SELECT ?c WHERE {{ ?s <http://x/p> ?o FILTER(rs:CYCLEBOOLEAN() = false) }} LIMIT {rows}"
        );
        // the hook only counts while a cancel flag is attached
        let rs = db
            .query_cancellable(&q, Arc::new(AtomicBool::new(false)))
            .unwrap();
        drop(rs);
        n.load(Ordering::SeqCst)
    };
    let one = polls(&mut db, 1);
    let many = polls(&mut db, 500);
    assert!(one > 0);
    assert_eq!(one, many, "the cycle test ran again for later rows");
}
