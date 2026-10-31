//! Data test: the path / reachability / BFS family of gStore's advanced graph
//! functions (`gstore::analytics::gfun::paths`): `simpleCyclePath`,
//! `simpleCycleBoolean`, `cyclePath`, `cycleBoolean`, `shortestPath`,
//! `shortestPathLen`, `shortestPathCount`, `shortestPaths`, `kHopReachable`,
//! `kHopReachablePath`, `kHopEnumerate`, `kHopCount`, `kHopNeighbor`,
//! `kHopAllNeighbors`, `bfsCount`, `SSSP`, `SSSPLen`.
//!
//! The expected outputs are derived by tracing upstream gStore 1.4
//! (`src/Query/Algorithm/*.cpp`, `GeneralEvaluation.cpp` output builders) by
//! hand; each test says briefly how. Most tests use the sample social graph of
//! upstream `docs/en-us/SPARQL_QUERY.md`, whose ids — in first-appearance
//! order — are Alice 0, Bob 1, Eve 2, Carol 3, Francis 4, Dave 5 (entities)
//! and focus 0, like 1, dislike 2 (predicates). `{}` means every predicate,
//! ascending by id.
//!
//! Path entries print their `nodes` in the iteration order of upstream's
//! `unordered_map<int, string>` filled in path order: for these small ids
//! (one key per bucket) that is the *reverse* of the first-visit order.
//!
//! The last section checks every function on random graphs against simple
//! reference implementations written here.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gstore::analytics::csr::{CsrGraph, PredFilter};
use gstore::analytics::gfun::{self, cxx, GArgs, GFnKind, GFunCtx, GFunValue, GNode};
use gstore::{Database, GStoreError};

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

/// Entity names of the social graph by id.
const NAMES: [&str; 6] = ["Alice", "Bob", "Eve", "Carol", "Francis", "Dave"];

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

struct Fx {
    db: Database,
    csr: Arc<CsrGraph>,
}

impl Fx {
    fn new(nt: &str) -> Fx {
        let db = Database::build_from_str("paths", nt).unwrap();
        let csr = db.csr().unwrap();
        Fx { db, csr }
    }

    fn social() -> Fx {
        let fx = Fx::new(SOCIAL);
        for (i, n) in NAMES.iter().enumerate() {
            assert_eq!(fx.eid(n), Some(i as u32), "{n}");
        }
        fx
    }

    /// The social graph plus `<Zed>`, an entity with only a literal-valued
    /// triple: it has an id but no CSR node ("isolated").
    fn social_with_zed() -> Fx {
        let fx = Fx::new(&format!("{SOCIAL}<Zed> <age> \"30\" .\n"));
        assert_eq!(fx.eid("Zed"), Some(6));
        assert!(fx.csr.node_of(6).is_none());
        fx
    }

    fn eid(&self, name: &str) -> Option<u32> {
        self.db.dict().entity_id(&format!("<{name}>"))
    }

    /// A vertex argument; an unknown name is a missing vertex.
    fn node(&self, name: &str) -> GNode {
        GNode::resolve(&self.csr, self.eid(name))
    }

    fn call(&self) -> Call<'_> {
        Call {
            fx: self,
            a: GArgs {
                preds: PredFilter::all(self.db.dict()),
                ..GArgs::default()
            },
        }
    }

    fn ctx(&self) -> GFunCtx<'_> {
        GFunCtx::new(&self.csr, self.db.dict())
    }
}

/// A graph-function call under construction.
struct Call<'f> {
    fx: &'f Fx,
    a: GArgs,
}

impl Call<'_> {
    fn src(mut self, names: &[&str]) -> Self {
        self.a.srcs = names.iter().map(|n| self.fx.node(n)).collect();
        self
    }

    fn dst(mut self, names: &[&str]) -> Self {
        self.a.dsts = names.iter().map(|n| self.fx.node(n)).collect();
        self
    }

    fn directed(mut self, d: bool) -> Self {
        self.a.directed = d;
        self
    }

    fn k(mut self, k: i64) -> Self {
        self.a.k = Some(k);
        self
    }

    fn ret(mut self, r: i64) -> Self {
        self.a.ret_num = Some(r);
        self
    }

    /// `{<inc>…, !<exc>…}` (names without brackets).
    fn preds(mut self, inc: &[&str], exc: &[&str]) -> Self {
        let wrap = |v: &[&str]| v.iter().map(|p| format!("<{p}>")).collect::<Vec<_>>();
        self.a.preds = PredFilter::resolve(&wrap(inc), &wrap(exc), self.fx.db.dict());
        self
    }

    fn where_empty(mut self) -> Self {
        self.a.where_empty = true;
        self
    }

    fn eval(&self, kind: GFnKind) -> GFunValue {
        gfun::eval(kind, &self.fx.ctx(), &self.a).unwrap()
    }

    fn json(&self, kind: GFnKind) -> String {
        match self.eval(kind) {
            GFunValue::Json(s) => s,
            other => panic!("{kind}: expected JSON, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// expected-output builders (upstream `pathVec2JSON` and the envelope)
// ---------------------------------------------------------------------------

fn br(name: &str) -> String {
    if name.is_empty() {
        String::new()
    } else {
        format!("<{name}>")
    }
}

/// `{"src":…,"dst":…,"edges":[…],"nodes":[…]}`; names without brackets, `""`
/// for a missing vertex.
fn path(src: &str, dst: &str, edges: &[(i32, i32, &str)], nodes: &[(i32, &str)]) -> String {
    let mut s = format!(
        "{{\"src\":\"{}\",\"dst\":\"{}\",\"edges\":[",
        br(src),
        br(dst)
    );
    for (i, (f, t, p)) in edges.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"fromNode\":{f},\"toNode\":{t},\"predIRI\":\"<{p}>\"}}"
        ));
    }
    s.push_str("],\"nodes\":[");
    for (i, (n, iri)) in nodes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"nodeIndex\":{n},\"nodeIRI\":\"{}\"}}",
            br(iri)
        ));
    }
    s.push_str("]}");
    s
}

/// Social-graph `nodes` list for the given ids, in the given order.
fn sn(ids: &[i32]) -> Vec<(i32, &'static str)> {
    ids.iter().map(|&i| (i, NAMES[i as usize])).collect()
}

fn paths(items: &[String]) -> String {
    format!("{{\"paths\":[{}]}}", items.join(","))
}

const EMPTY: &str = "{\"paths\":[]}";

// ---------------------------------------------------------------------------
// simpleCyclePath / simpleCycleBoolean
// ---------------------------------------------------------------------------

/// Parsed but never evaluated upstream (no branch in GE; simpleCycle is
/// commented out): always the empty envelope, never a boolean.
#[test]
fn simple_cycle_functions_are_always_empty() {
    let fx = Fx::social();
    for c in [
        fx.call().src(&["Eve"]).dst(&["Carol"]).directed(true),
        fx.call()
            .src(&["Alice", "Bob"])
            .dst(&["Bob"])
            .directed(false),
        fx.call().src(&["Nobody"]).dst(&["Carol"]),
    ] {
        assert_eq!(c.json(GFnKind::SimpleCyclePath), EMPTY);
        assert_eq!(c.json(GFnKind::SimpleCycleBoolean), EMPTY);
    }
}

// ---------------------------------------------------------------------------
// cyclePath / cycleBoolean
// ---------------------------------------------------------------------------

/// SPARQL_QUERY.md: `cyclePath(?x, <Carol>, true, {<like>})` with
/// `?x ∈ {Dave, Eve}`. Dave has no `like` out-edge → shortestPath(Dave,
/// Carol) is empty → no entry. Eve: shortestPath(Eve, Carol) = [2,like,3];
/// shortestPath(Carol, Eve): u-phase 3→1 (du[1]=1), v-phase in(Eve,like) =
/// [Bob] ∈ du → meet 1 → [3,like,1,like,2]. Cycle = [2,3,1,2]; nodes
/// inserted 2,3,1 → printed 1,3,2.
#[test]
fn cycle_path_docs_example() {
    let fx = Fx::social();
    let c = fx
        .call()
        .src(&["Dave", "Eve"])
        .dst(&["Carol"])
        .directed(true)
        .preds(&["like"], &[]);
    let want = "{\"paths\":[{\"src\":\"<Eve>\",\"dst\":\"<Carol>\",\"edges\":[{\"fromNode\":2,\"toNode\":3,\"predIRI\":\"<like>\"},{\"fromNode\":3,\"toNode\":1,\"predIRI\":\"<like>\"},{\"fromNode\":1,\"toNode\":2,\"predIRI\":\"<like>\"}],\"nodes\":[{\"nodeIndex\":1,\"nodeIRI\":\"<Bob>\"},{\"nodeIndex\":3,\"nodeIRI\":\"<Carol>\"},{\"nodeIndex\":2,\"nodeIRI\":\"<Eve>\"}]}]}";
    assert_eq!(c.json(GFnKind::CyclePath), want);
    assert_eq!(
        want,
        paths(&[path(
            "Eve",
            "Carol",
            &[(2, 3, "like"), (3, 1, "like"), (1, 2, "like")],
            &sn(&[1, 3, 2])
        )])
    );
    // The docs' cycleBoolean example (same arguments) is true.
    assert_eq!(c.eval(GFnKind::CycleBoolean), GFunValue::Bool(true));
    // Dave alone has no cycle.
    let dave = fx
        .call()
        .src(&["Dave"])
        .dst(&["Carol"])
        .directed(true)
        .preds(&["like"], &[]);
    assert_eq!(dave.json(GFnKind::CyclePath), EMPTY);
    assert_eq!(dave.eval(GFnKind::CycleBoolean), GFunValue::Bool(false));
}

/// Undirected cycles use `shortestPath0` (cycle.cpp:20-176), which records
/// every edge with its plain predicate id, and then walk the path back, so
/// all edges print forward (`fromNode` = earlier node) even where the real
/// edge points the other way.
///
/// * (Alice, Eve, {like}): u-phase: Alice -like-> Bob (route_u[1]); v-phase:
///   in(Eve, like) = [Bob] ∈ route_u → meet on the v side: [0,1,2] → cycle
///   0,1,2,1,0; nodes inserted 0,1,2 → printed 2,1,0.
/// * (Eve, Alice, {like}): u-phase from Eve: out Carol, in Bob; v-phase from
///   Alice: in none, out Bob ∈ route_u → [2,1,0] → cycle 2,1,0,1,2 (the
///   real edges are 1→2 and 0→1).
/// * (Francis, Alice, {like}): round 1: u: Francis → Carol; v: Alice → Bob
///   (out-edge). Round 2: u: Carol -like-> Bob ∈ route_v → meet on the u side
///   → [4,3,1,0] → cycle 4,3,1,0,1,3,4; nodes printed 0,1,3,4.
/// * Pairs with `u == v` are skipped; cross product in `srcs × dsts` order.
#[test]
fn cycle_path_undirected_walks_the_path_back() {
    let fx = Fx::social();
    fn like(c: Call<'_>) -> Call<'_> {
        c.directed(false).preds(&["like"], &[])
    }
    let ae = path(
        "Alice",
        "Eve",
        &[
            (0, 1, "like"),
            (1, 2, "like"),
            (2, 1, "like"),
            (1, 0, "like"),
        ],
        &sn(&[2, 1, 0]),
    );
    let ea = path(
        "Eve",
        "Alice",
        &[
            (2, 1, "like"),
            (1, 0, "like"),
            (0, 1, "like"),
            (1, 2, "like"),
        ],
        &sn(&[0, 1, 2]),
    );
    let c = like(fx.call().src(&["Alice", "Eve"]).dst(&["Eve", "Alice"]));
    assert_eq!(c.json(GFnKind::CyclePath), paths(&[ae, ea]));
    let fa = path(
        "Francis",
        "Alice",
        &[
            (4, 3, "like"),
            (3, 1, "like"),
            (1, 0, "like"),
            (0, 1, "like"),
            (1, 3, "like"),
            (3, 4, "like"),
        ],
        &sn(&[0, 1, 3, 4]),
    );
    let c = like(fx.call().src(&["Francis"]).dst(&["Alice"]));
    assert_eq!(c.json(GFnKind::CyclePath), paths(&[fa]));
    // A single edge is a "cycle" u → v → u undirected.
    let c = like(fx.call().src(&["Alice"]).dst(&["Bob"]));
    assert_eq!(
        c.json(GFnKind::CyclePath),
        paths(&[path(
            "Alice",
            "Bob",
            &[(0, 1, "like"), (1, 0, "like")],
            &sn(&[1, 0])
        )])
    );
    assert_eq!(c.eval(GFnKind::CycleBoolean), GFunValue::Bool(true));
    // Directed, the same pair has no cycle (Bob does not like Alice).
    let c = fx
        .call()
        .src(&["Alice"])
        .dst(&["Bob"])
        .directed(true)
        .preds(&["like"], &[]);
    assert_eq!(c.json(GFnKind::CyclePath), EMPTY);
    assert_eq!(c.eval(GFnKind::CycleBoolean), GFunValue::Bool(false));
}

#[test]
fn cycle_functions_skip_equal_missing_and_isolated_vertices() {
    let fx = Fx::social_with_zed();
    for (s, d) in [
        ("Alice", "Alice"),
        ("Nobody", "Carol"),
        ("Carol", "Nobody"),
        ("Nobody", "Ghost"),
        ("Zed", "Alice"),
    ] {
        for directed in [true, false] {
            let c = fx.call().src(&[s]).dst(&[d]).directed(directed);
            assert_eq!(c.json(GFnKind::CyclePath), EMPTY, "{s} {d} {directed}");
            assert_eq!(c.eval(GFnKind::CycleBoolean), GFunValue::Bool(false));
        }
    }
    // No bindings at all → false / empty.
    let c = fx.call().dst(&["Carol"]).directed(true);
    assert_eq!(c.eval(GFnKind::CycleBoolean), GFunValue::Bool(false));
    assert_eq!(c.json(GFnKind::CyclePath), EMPTY);
}

// ---------------------------------------------------------------------------
// shortestPath / shortestPathLen / kHopReachablePath
// ---------------------------------------------------------------------------

/// SPARQL_QUERY.md: `shortestPath(<Francis>, ?x, true, {<like>, <focus>})`,
/// `?x = Alice`. Bidirectional BFS (shortestPath.cpp): u: 4 -like-> 3; v:
/// in(0, focus) = [1, 5]; u: 3 -like-> 1 ∈ dis_v → meet 1. Towards v: out(1,
/// like) = [2] has no dis_v, out(1, focus) = [0] does; towards u: in(1, like)
/// = [0, 3], du[3] = 1; in(3, like) = [2, 4], du[4] = 0.
#[test]
fn shortest_path_docs_example() {
    let fx = Fx::social();
    let c = fx
        .call()
        .src(&["Francis"])
        .dst(&["Alice"])
        .directed(true)
        .preds(&["like", "focus"], &[]);
    let want = "{\"paths\":[{\"src\":\"<Francis>\",\"dst\":\"<Alice>\",\"edges\":[{\"fromNode\":4,\"toNode\":3,\"predIRI\":\"<like>\"},{\"fromNode\":3,\"toNode\":1,\"predIRI\":\"<like>\"},{\"fromNode\":1,\"toNode\":0,\"predIRI\":\"<focus>\"}],\"nodes\":[{\"nodeIndex\":0,\"nodeIRI\":\"<Alice>\"},{\"nodeIndex\":1,\"nodeIRI\":\"<Bob>\"},{\"nodeIndex\":3,\"nodeIRI\":\"<Carol>\"},{\"nodeIndex\":4,\"nodeIRI\":\"<Francis>\"}]}]}";
    assert_eq!(c.json(GFnKind::ShortestPath), want);
    assert_eq!(
        c.json(GFnKind::ShortestPathLen),
        "{\"paths\":[{\"src\":\"<Francis>\",\"dst\":\"<Alice>\",\"length\":3}]}"
    );
    // kHopReachablePath = the same shortest path (no k → 999 hops).
    assert_eq!(c.json(GFnKind::KHopReachablePath), want);
}

#[test]
fn k_hop_reachable_path_filters_the_shortest_path_by_length() {
    let fx = Fx::social();
    let c = |k| {
        fx.call()
            .src(&["Francis"])
            .dst(&["Alice"])
            .directed(true)
            .k(k)
            .preds(&["like", "focus"], &[])
    };
    let sp = c(-1).json(GFnKind::ShortestPath);
    // SPARQL_QUERY.md: k = -1 (→ 999) gives the shortest path.
    assert_eq!(c(-1).json(GFnKind::KHopReachablePath), sp);
    assert_eq!(c(3).json(GFnKind::KHopReachablePath), sp);
    assert_eq!(c(2).json(GFnKind::KHopReachablePath), EMPTY);
    assert_eq!(c(0).json(GFnKind::KHopReachablePath), EMPTY);
    // u == v → the empty-path entry, whatever k.
    let c = fx.call().src(&["Bob"]).dst(&["Bob"]).k(0);
    assert_eq!(
        c.json(GFnKind::KHopReachablePath),
        paths(&[path("Bob", "Bob", &[], &[])])
    );
}

/// Undirected: Eve → Alice over `like`. u: out(Eve) = [Carol], in(Eve) =
/// [Bob]; v: out(Alice) = [Bob] ∈ dis_u → meet Bob. Towards v: out(Bob,
/// like) = [Eve] (no dis_v), then in(Bob, like) = [Alice] → reversed edge;
/// towards u: in(Bob, like) = [Alice, Carol] (Carol: du 1, not 0), then
/// out(Bob, like) = [Eve] → reversed. Reversed edges print the real
/// direction: 1→2 and 0→1.
#[test]
fn shortest_path_undirected_prints_reversed_edges_in_their_real_direction() {
    let fx = Fx::social();
    let c = fx
        .call()
        .src(&["Eve"])
        .dst(&["Alice"])
        .directed(false)
        .preds(&["like"], &[]);
    let want = path(
        "Eve",
        "Alice",
        &[(1, 2, "like"), (0, 1, "like")],
        &sn(&[0, 1, 2]),
    );
    assert_eq!(
        c.json(GFnKind::ShortestPath),
        paths(std::slice::from_ref(&want))
    );
    assert_eq!(
        c.json(GFnKind::ShortestPathLen),
        "{\"paths\":[{\"src\":\"<Eve>\",\"dst\":\"<Alice>\",\"length\":2}]}"
    );
    // kHopEnumerate with k = 2 finds exactly this path (edges reversed too).
    assert_eq!(c.k(2).json(GFnKind::KHopEnumerate), paths(&[want]));
}

/// Cross product in `srcs × dsts` order (GE:2623-2672): (F,A) is the docs
/// path; (F,B): u 4→3, v in(1, like) = [0, 3] → meet 3 → [4,3,1]; (C,A): u
/// 3→1, v in(0, focus) = [1, 5] → meet 1 → [3,1,0] (1→0 via focus); (C,B):
/// 3 -like-> 1 directly.
#[test]
fn shortest_path_cross_product_order() {
    let fx = Fx::social();
    let c = fx
        .call()
        .src(&["Francis", "Carol"])
        .dst(&["Alice", "Bob"])
        .directed(true)
        .preds(&["like", "focus"], &[]);
    let want = paths(&[
        path(
            "Francis",
            "Alice",
            &[(4, 3, "like"), (3, 1, "like"), (1, 0, "focus")],
            &sn(&[0, 1, 3, 4]),
        ),
        path(
            "Francis",
            "Bob",
            &[(4, 3, "like"), (3, 1, "like")],
            &sn(&[1, 3, 4]),
        ),
        path(
            "Carol",
            "Alice",
            &[(3, 1, "like"), (1, 0, "focus")],
            &sn(&[0, 1, 3]),
        ),
        path("Carol", "Bob", &[(3, 1, "like")], &sn(&[1, 3])),
    ]);
    assert_eq!(c.json(GFnKind::ShortestPath), want);
    let len = |s: &str, d: &str, l: u32| {
        format!("{{\"src\":\"<{s}>\",\"dst\":\"<{d}>\",\"length\":{l}}}")
    };
    assert_eq!(
        c.json(GFnKind::ShortestPathLen),
        paths(&[
            len("Francis", "Alice", 3),
            len("Francis", "Bob", 2),
            len("Carol", "Alice", 2),
            len("Carol", "Bob", 1),
        ])
    );
}

#[test]
fn shortest_path_special_cases() {
    let fx = Fx::social_with_zed();
    let c = |s: &[&str], d: &[&str]| fx.call().src(s).dst(d).directed(true);
    // u == v → empty path / length 0.
    let aa = c(&["Alice"], &["Alice"]);
    assert_eq!(
        aa.json(GFnKind::ShortestPath),
        paths(&[path("Alice", "Alice", &[], &[])])
    );
    assert_eq!(
        aa.json(GFnKind::ShortestPathLen),
        "{\"paths\":[{\"src\":\"<Alice>\",\"dst\":\"<Alice>\",\"length\":0}]}"
    );
    // Two missing vertices are the same upstream id (-1): u == v.
    let mm = c(&["Nobody"], &["Ghost"]);
    assert_eq!(
        mm.json(GFnKind::ShortestPath),
        paths(&[path("", "", &[], &[])])
    );
    assert_eq!(
        mm.json(GFnKind::ShortestPathLen),
        "{\"paths\":[{\"src\":\"\",\"dst\":\"\",\"length\":0}]}"
    );
    // One missing / isolated end, or unreachable (nobody likes or focuses
    // on Francis) → no entry.
    for (s, d) in [
        ("Nobody", "Alice"),
        ("Alice", "Nobody"),
        ("Zed", "Alice"),
        ("Alice", "Zed"),
    ] {
        assert_eq!(c(&[s], &[d]).json(GFnKind::ShortestPath), EMPTY);
        assert_eq!(c(&[s], &[d]).json(GFnKind::ShortestPathLen), EMPTY);
    }
    let un = c(&["Alice"], &["Francis"]).preds(&["like", "focus"], &[]);
    assert_eq!(un.json(GFnKind::ShortestPath), EMPTY);
    assert_eq!(un.json(GFnKind::ShortestPathLen), EMPTY);
    // {!like, !focus} = {dislike}: Alice -dislike-> Eve, a dead end.
    let ex = c(&["Alice"], &["Francis"]).preds(&[], &["like", "focus"]);
    assert_eq!(ex.json(GFnKind::ShortestPathLen), EMPTY);
    // {}: Alice -dislike-> Eve -like-> Carol -dislike-> Francis.
    let all = c(&["Alice"], &["Francis"]);
    assert_eq!(
        all.json(GFnKind::ShortestPathLen),
        "{\"paths\":[{\"src\":\"<Alice>\",\"dst\":\"<Francis>\",\"length\":3}]}"
    );
    // Repeated bindings are de-duplicated pairwise.
    let rep = c(&["Carol", "Carol"], &["Bob", "Bob"]).preds(&["like"], &[]);
    assert_eq!(
        rep.json(GFnKind::ShortestPathLen),
        "{\"paths\":[{\"src\":\"<Carol>\",\"dst\":\"<Bob>\",\"length\":1}]}"
    );
}

// ---------------------------------------------------------------------------
// shortestPathCount
// ---------------------------------------------------------------------------

fn count(s: &str, d: &str, n: i64) -> String {
    format!(
        "{{\"src\":\"{}\", \"dst\":\"{}\", \"count\":{n}}}",
        br(s),
        br(d)
    )
}

/// shortestPathCount.cpp: level-synchronous bidirectional BFS, the smaller
/// frontier expands (ties: target side).
///
/// * (Alice, Carol, directed, {}): round 1 (tie → target): in(Carol, like)
///   = [Eve, Francis] → tcnt 1 each. Round 2 (1 < 2 → source): out(Alice,
///   focus) = Bob (fresh), out(Alice, like) = Bob (+1 → 2), out(Alice,
///   dislike) = Eve: tcnt[Eve] = 1 → joint. Result scnt·tcnt = 1·1 = 1.
/// * (Alice, Eve, directed, {focus, like}): round 1: in(Eve, focus) = Dave,
///   in(Eve, like) = Bob. Round 2: out(Alice, focus) = Bob → joint, scnt 1;
///   out(Alice, like) = Bob → scnt 2. Result 2·1 = 2.
/// * (Alice, Carol, undirected, {}): round 1 (target, out first): out(Carol)
///   = Bob (focus), Bob (like) → tcnt[Bob] = 2, Francis (dislike); in(Carol,
///   like) = Eve, Francis → tcnt[Eve] = 1, tcnt[Francis] = 2. Round 2
///   (source, in first): in(Alice, focus) = Bob → joint, Dave; out(Alice) =
///   Bob, Bob → scnt[Bob] = 3, Eve → joint. Result 3·2 + 1·1 = 7.
/// * (Carol, Dave, directed, {like}): in(Dave, like) = [] → 0.
/// * (Alice, Alice, directed, {}): the sides never meet at Alice itself, but
///   round 1 (target) marks in(Alice, focus) = Bob, Dave; round 2 (source)
///   reaches Bob over focus (joint) and like → 2·1 = 2 (the two closed walks
///   Alice → Bob → Alice).
#[test]
fn shortest_path_count_traces() {
    let fx = Fx::social_with_zed();
    let c = |s: &str, d: &str| fx.call().src(&[s]).dst(&[d]);
    assert_eq!(
        c("Alice", "Carol")
            .directed(true)
            .json(GFnKind::ShortestPathCount),
        paths(&[count("Alice", "Carol", 1)])
    );
    assert_eq!(
        c("Alice", "Eve")
            .directed(true)
            .preds(&["focus", "like"], &[])
            .json(GFnKind::ShortestPathCount),
        paths(&[count("Alice", "Eve", 2)])
    );
    assert_eq!(
        c("Alice", "Carol")
            .directed(false)
            .preds(&["focus", "like", "dislike"], &[])
            .json(GFnKind::ShortestPathCount),
        paths(&[count("Alice", "Carol", 7)])
    );
    assert_eq!(
        c("Carol", "Dave")
            .directed(true)
            .preds(&["like"], &[])
            .json(GFnKind::ShortestPathCount),
        paths(&[count("Carol", "Dave", 0)])
    );
    assert_eq!(
        c("Alice", "Alice")
            .directed(true)
            .preds(&["focus", "like", "dislike"], &[])
            .json(GFnKind::ShortestPathCount),
        paths(&[count("Alice", "Alice", 2)])
    );
    // Missing vertices: upstream indexes its arrays with -1 → no entry;
    // an isolated vertex has no edges → 0.
    assert_eq!(c("Nobody", "Alice").json(GFnKind::ShortestPathCount), EMPTY);
    assert_eq!(
        c("Zed", "Alice").json(GFnKind::ShortestPathCount),
        paths(&[count("Zed", "Alice", 0)])
    );
    // Every pair gets an entry, in loop order. (Carol, Carol) over {like}:
    // round 1 (target) marks in(Carol) = Eve, Francis; rounds 2-3 (source,
    // the smaller frontier) go Carol → Bob → Eve, a joint: 1·1 = 1 (the
    // cycle Carol → Bob → Eve → Carol).
    assert_eq!(
        fx.call()
            .src(&["Alice", "Carol"])
            .dst(&["Carol", "Bob"])
            .directed(true)
            .preds(&["like"], &[])
            .json(GFnKind::ShortestPathCount),
        paths(&[
            count("Alice", "Carol", 1),
            count("Alice", "Bob", 1),
            count("Carol", "Carol", 1),
            count("Carol", "Bob", 1),
        ])
    );
}

/// A diamond with a parallel edge: a -p-> b, a -p-> c, b -p-> d, c -p-> d,
/// a -q-> b (ids a 0, b 1, c 2, d 3; p 0, q 1).
const DIAMOND: &str = "
<a> <p> <b> .
<a> <p> <c> .
<b> <p> <d> .
<c> <p> <d> .
<a> <q> <b> .
";

/// Directed a → d: three shortest edge sequences (a-p-b-p-d, a-q-b-p-d,
/// a-p-c-p-d). Undirected d → a: d–b (p), b–a (p, q), d–c–a → 3 as well.
#[test]
fn shortest_path_count_counts_parallel_edges() {
    let fx = Fx::new(DIAMOND);
    let c = |s: &str, d: &str, directed: bool| {
        fx.call()
            .src(&[s])
            .dst(&[d])
            .directed(directed)
            .json(GFnKind::ShortestPathCount)
    };
    assert_eq!(c("a", "d", true), paths(&[count("a", "d", 3)]));
    assert_eq!(c("d", "a", false), paths(&[count("d", "a", 3)]));
    assert_eq!(c("d", "a", true), paths(&[count("d", "a", 0)]));
    assert_eq!(c("a", "b", true), paths(&[count("a", "b", 2)]));
}

// ---------------------------------------------------------------------------
// shortestPaths
// ---------------------------------------------------------------------------

/// kHopShortestPaths.cpp on the diamond, directed a → d:
/// BFS from a: out(a, p) = [b, c] → dis 1, pre[b] = pre[c] = [a]; out(a, q)
/// = [b] (same distance, a already listed). Then b: pre[d] = [b]; c: pre[d]
/// = [b, c]. DFS from d over pre: [a,b,d], [a,c,d]. Fan-out of a → b:
/// {p, q} → a-p-b-p-d, then a-q-b-p-d; a → c → d: {p}, {p}.
/// Undirected d → a: BFS from d: in(d, p) = [b, c]; b: in(b, p) = [a],
/// in(b, q) = [a] → pre[a] = [b]; c: in(c, p) = [a] → pre[a] = [b, c].
/// Hops d → b: {p}; b → a: {p, q} (both in-edges, recorded forward).
#[test]
fn shortest_paths_fan_out_over_predicates() {
    let fx = Fx::new(DIAMOND);
    let dn = |ids: &[i32]| -> Vec<(i32, &'static str)> {
        ids.iter()
            .map(|&i| (i, ["a", "b", "c", "d"][i as usize]))
            .collect()
    };
    let c = fx.call().src(&["a"]).dst(&["d"]).directed(true);
    assert_eq!(
        c.json(GFnKind::ShortestPaths),
        paths(&[
            path("a", "d", &[(0, 1, "p"), (1, 3, "p")], &dn(&[3, 1, 0])),
            path("a", "d", &[(0, 1, "q"), (1, 3, "p")], &dn(&[3, 1, 0])),
            path("a", "d", &[(0, 2, "p"), (2, 3, "p")], &dn(&[3, 2, 0])),
        ])
    );
    let c = fx.call().src(&["d"]).dst(&["a"]).directed(false);
    assert_eq!(
        c.json(GFnKind::ShortestPaths),
        paths(&[
            path("d", "a", &[(3, 1, "p"), (1, 0, "p")], &dn(&[0, 1, 3])),
            path("d", "a", &[(3, 1, "p"), (1, 0, "q")], &dn(&[0, 1, 3])),
            path("d", "a", &[(3, 2, "p"), (2, 0, "p")], &dn(&[0, 2, 3])),
        ])
    );
    // Predicate filters: {q} cannot leave b; {!q} keeps the two p-paths.
    let c = fx.call().src(&["a"]).dst(&["d"]).directed(true);
    assert_eq!(c.preds(&["q"], &[]).json(GFnKind::ShortestPaths), EMPTY);
    let c = fx.call().src(&["a"]).dst(&["d"]).directed(true);
    assert_eq!(
        c.preds(&[], &["q"]).json(GFnKind::ShortestPaths),
        paths(&[
            path("a", "d", &[(0, 1, "p"), (1, 3, "p")], &dn(&[3, 1, 0])),
            path("a", "d", &[(0, 2, "p"), (2, 3, "p")], &dn(&[3, 2, 0])),
        ])
    );
    // Directed d → a is unreachable.
    let c = fx.call().src(&["d"]).dst(&["a"]).directed(true);
    assert_eq!(c.json(GFnKind::ShortestPaths), EMPTY);
}

/// Two hops with two predicates each: the first hop's choice varies fastest
/// (upstream extends its list in place, then appends copies per further
/// predicate): x-p-y-p-z, x-q-y-p-z, x-p-y-q-z, x-q-y-q-z.
#[test]
fn shortest_paths_first_hop_varies_fastest() {
    let fx = Fx::new("<x> <p> <y> .\n<x> <q> <y> .\n<y> <p> <z> .\n<y> <q> <z> .\n");
    let n = [(2, "z"), (1, "y"), (0, "x")];
    let c = fx.call().src(&["x"]).dst(&["z"]).directed(true);
    assert_eq!(
        c.json(GFnKind::ShortestPaths),
        paths(&[
            path("x", "z", &[(0, 1, "p"), (1, 2, "p")], &n),
            path("x", "z", &[(0, 1, "q"), (1, 2, "p")], &n),
            path("x", "z", &[(0, 1, "p"), (1, 2, "q")], &n),
            path("x", "z", &[(0, 1, "q"), (1, 2, "q")], &n),
        ])
    );
}

#[test]
fn shortest_paths_special_cases() {
    let fx = Fx::social_with_zed();
    let c = |s: &str, d: &str| fx.call().src(&[s]).dst(&[d]).directed(true);
    assert_eq!(
        c("Bob", "Bob").json(GFnKind::ShortestPaths),
        paths(&[path("Bob", "Bob", &[], &[])])
    );
    // The wrapper's `uid < 0 || vid < 0` guard comes before `uid == vid`:
    // two missing vertices give nothing (unlike shortestPath).
    assert_eq!(c("Nobody", "Ghost").json(GFnKind::ShortestPaths), EMPTY);
    assert_eq!(c("Nobody", "Bob").json(GFnKind::ShortestPaths), EMPTY);
    assert_eq!(c("Zed", "Bob").json(GFnKind::ShortestPaths), EMPTY);
    assert_eq!(c("Bob", "Zed").json(GFnKind::ShortestPaths), EMPTY);
    // Social: Francis → Alice over {like, focus}: one vertex sequence
    // 4, 3, 1, 0; the hop Carol → Bob has both predicates, and the fan-out
    // takes them ascending by id (`std::set`: focus 0, like 1) whatever the
    // filter order — so the shortestPath path (via like) comes second.
    let fa = c("Francis", "Alice").preds(&["like", "focus"], &[]);
    let n = sn(&[0, 1, 3, 4]);
    assert_eq!(
        fa.json(GFnKind::ShortestPaths),
        paths(&[
            path(
                "Francis",
                "Alice",
                &[(4, 3, "like"), (3, 1, "focus"), (1, 0, "focus")],
                &n
            ),
            path(
                "Francis",
                "Alice",
                &[(4, 3, "like"), (3, 1, "like"), (1, 0, "focus")],
                &n
            ),
        ])
    );
}

// ---------------------------------------------------------------------------
// kHopReachable
// ---------------------------------------------------------------------------

fn reach(s: &str, d: &str, v: bool) -> String {
    format!(
        "{{\"src\":\"{}\",\"dst\":\"{}\",\"value\":\"{v}\"}}",
        br(s),
        br(d)
    )
}

/// SPARQL_QUERY.md: Francis → Alice over {like, focus} is `false` for k = 2
/// and `true` for k = -1. kHopReachable.cpp (no push phase on small graphs):
/// forward BFS pops Francis (f1[Carol] = 1) and Carol (f1[Bob] = 2), then
/// stops at fullL1 = 3 > 2; the backward step from Alice reads in-positions
/// 0, 1 of out(Alice, focus) = [Bob] → Bob (f1 2 + 1 > 2), -1 → false. With
/// k = 999 the forward BFS also pops Bob and discovers Alice (f2 = 0) → true.
#[test]
fn k_hop_reachable_docs_examples() {
    let fx = Fx::social();
    let c = |k| {
        fx.call()
            .src(&["Francis"])
            .dst(&["Alice"])
            .directed(true)
            .k(k)
            .preds(&["like", "focus"], &[])
    };
    assert_eq!(
        c(2).json(GFnKind::KHopReachable),
        "{\"paths\":[{\"src\":\"<Francis>\",\"dst\":\"<Alice>\",\"value\":\"false\"}]}"
    );
    assert_eq!(
        c(-1).json(GFnKind::KHopReachable),
        "{\"paths\":[{\"src\":\"<Francis>\",\"dst\":\"<Alice>\",\"value\":\"true\"}]}"
    );
    assert_eq!(
        c(3).json(GFnKind::KHopReachable),
        paths(&[reach("Francis", "Alice", true)])
    );
    // k = 0 → false for distinct vertices; u == v → true (even k = 0); a
    // missing source prints "" and false; two missing vertices are equal.
    assert_eq!(
        c(0).json(GFnKind::KHopReachable),
        paths(&[reach("Francis", "Alice", false)])
    );
    let multi = fx
        .call()
        .src(&["Alice", "Nobody"])
        .dst(&["Alice", "Ghost"])
        .directed(true)
        .k(0);
    assert_eq!(
        multi.json(GFnKind::KHopReachable),
        paths(&[
            reach("Alice", "Alice", true),
            reach("Alice", "", false),
            reach("", "Alice", false),
            reach("", "", true),
        ])
    );
}

/// The forward BFS gets one batch of 10 pops; the backward queue never
/// refills (its discoveries go to the forward next-queue), so the search
/// ends after one round. Chain c0 → … → c12: c10 is discovered by the 10th
/// pop (c9) → true; c11 and c12 are reachable but come out false.
#[test]
fn k_hop_reachable_one_round_quirk_on_a_chain() {
    let nt: String = (0..12)
        .map(|i| format!("<c{i}> <next> <c{}> .\n", i + 1))
        .collect();
    let fx = Fx::new(&nt);
    for (dst, want) in [("c1", true), ("c10", true), ("c11", false), ("c12", false)] {
        let c = fx.call().src(&["c0"]).dst(&[dst]).directed(true).k(-1);
        assert_eq!(
            c.json(GFnKind::KHopReachable),
            paths(&[reach("c0", dst, want)]),
            "{dst}"
        );
        // shortestPathLen still finds every one of them.
        assert_ne!(c.json(GFnKind::ShortestPathLen), EMPTY);
    }
    // k bounds the forward levels: c3 is at depth 3.
    let c = |k| fx.call().src(&["c0"]).dst(&["c3"]).directed(true).k(k);
    assert_eq!(
        c(2).json(GFnKind::KHopReachable),
        paths(&[reach("c0", "c3", false)])
    );
    assert_eq!(
        c(3).json(GFnKind::KHopReachable),
        paths(&[reach("c0", "c3", true)])
    );
}

/// Upstream's backward step reads in-neighbour *positions* through
/// `getOutVertID` (kHopReachable.cpp:340). x -p-> z, y -p-> x, w -p-> y:
/// y is not reachable from x, but the backward step from y reads position 0
/// of out(y) = [x] = u → l1[x] + 1 <= 1 → true.
#[test]
fn k_hop_reachable_reverse_edge_quirk() {
    let fx = Fx::new("<x> <p> <z> .\n<y> <p> <x> .\n<w> <p> <y> .\n");
    let c = fx.call().src(&["x"]).dst(&["y"]).directed(true).k(1);
    assert_eq!(
        c.json(GFnKind::KHopReachable),
        paths(&[reach("x", "y", true)])
    );
    assert_eq!(
        c.json(GFnKind::ShortestPathLen),
        EMPTY,
        "really unreachable"
    );
    // Degree check: z has no out-edges → false at once.
    let c = fx.call().src(&["z"]).dst(&["y"]).directed(true).k(5);
    assert_eq!(
        c.json(GFnKind::KHopReachable),
        paths(&[reach("z", "y", false)])
    );
}

/// The `-1` that `getOutVertID` returns past the out-degree acts as a
/// pseudo-vertex: a -p-> u, b -p-> v (disconnected), undirected. The forward
/// step from u reads position 0 of out(u) = [] → f1[-1] = 1; the backward step
/// from v records f2[-1] = 1 → 1 + 1 <= k for k >= 2.
#[test]
fn k_hop_reachable_sentinel_quirk() {
    let fx = Fx::new("<a> <p> <u> .\n<b> <p> <v> .\n");
    let c = |k| fx.call().src(&["u"]).dst(&["v"]).directed(false).k(k);
    assert_eq!(
        c(2).json(GFnKind::KHopReachable),
        paths(&[reach("u", "v", true)])
    );
    assert_eq!(
        c(-1).json(GFnKind::KHopReachable),
        paths(&[reach("u", "v", true)])
    );
    assert_eq!(
        c(1).json(GFnKind::KHopReachable),
        paths(&[reach("u", "v", false)])
    );
    // Directed, u has no out-edge → false.
    let c = fx.call().src(&["u"]).dst(&["v"]).directed(true).k(5);
    assert_eq!(
        c.json(GFnKind::KHopReachable),
        paths(&[reach("u", "v", false)])
    );
}

// ---------------------------------------------------------------------------
// kHopEnumerate
// ---------------------------------------------------------------------------

/// SPARQL_QUERY.md: `kHopEnumerate(<Alice>, ?x, true, 3, {<like>, <focus>})`
/// with `?x = Carol`. bar (reverse BFS from Carol): Eve 1, Francis 1, Bob 2,
/// Dave 2, Alice 3. DFS from Alice, predicates in written order: like → Bob
/// (1 + 2 <= 3) → like → Eve → like → Carol: record; Bob's focus → Alice is
/// on the stack. Then Alice's focus → Bob → … → Carol: record. (The docs'
/// `nodes` order 3,2,0,1 is stale; libstdc++ gives 3,2,1,0.)
#[test]
fn k_hop_enumerate_docs_example() {
    let fx = Fx::social();
    let c = |k, r: Option<i64>| {
        let c = fx
            .call()
            .src(&["Alice"])
            .dst(&["Carol"])
            .directed(true)
            .k(k)
            .preds(&["like", "focus"], &[]);
        match r {
            Some(r) => c.ret(r),
            None => c,
        }
    };
    let p1 = path(
        "Alice",
        "Carol",
        &[(0, 1, "like"), (1, 2, "like"), (2, 3, "like")],
        &sn(&[3, 2, 1, 0]),
    );
    let p2 = path(
        "Alice",
        "Carol",
        &[(0, 1, "focus"), (1, 2, "like"), (2, 3, "like")],
        &sn(&[3, 2, 1, 0]),
    );
    assert_eq!(
        c(3, None).json(GFnKind::KHopEnumerate),
        paths(&[p1.clone(), p2.clone()])
    );
    // retNum caps the paths per pair; 0 enumerates nothing; negative = all.
    assert_eq!(
        c(3, Some(1)).json(GFnKind::KHopEnumerate),
        paths(std::slice::from_ref(&p1))
    );
    assert_eq!(c(3, Some(0)).json(GFnKind::KHopEnumerate), EMPTY);
    assert_eq!(
        c(3, Some(-7)).json(GFnKind::KHopEnumerate),
        paths(&[p1.clone(), p2.clone()])
    );
    // k = 2: Bob is 2 hops from Carol, 1 + 2 > 2 → pruned → nothing.
    assert_eq!(c(2, None).json(GFnKind::KHopEnumerate), EMPTY);
    // k = -1 (→ 999): still only these two simple paths.
    assert_eq!(c(-1, None).json(GFnKind::KHopEnumerate), paths(&[p1, p2]));
}

#[test]
fn k_hop_enumerate_special_cases() {
    let fx = Fx::social_with_zed();
    let c = |s: &str, d: &str| fx.call().src(&[s]).dst(&[d]).directed(true).k(3);
    assert_eq!(
        c("Bob", "Bob").json(GFnKind::KHopEnumerate),
        paths(&[path("Bob", "Bob", &[], &[])])
    );
    // `uid == vid` is checked before kHopEnumeratePath's `uid < 0` guard.
    assert_eq!(
        c("Nobody", "Ghost").json(GFnKind::KHopEnumerate),
        paths(&[path("", "", &[], &[])])
    );
    assert_eq!(c("Nobody", "Bob").json(GFnKind::KHopEnumerate), EMPTY);
    assert_eq!(c("Zed", "Bob").json(GFnKind::KHopEnumerate), EMPTY);
    assert_eq!(c("Bob", "Zed").json(GFnKind::KHopEnumerate), EMPTY);
}

/// Undirected, each predicate and each direction is its own path, and the
/// budget is per pair: a -p-> b and b -p-> a and a -q-> b give three 1-hop
/// paths a → b (p forward, p reversed, q forward).
#[test]
fn k_hop_enumerate_parallel_and_reversed_edges() {
    let fx = Fx::new("<a> <p> <b> .\n<b> <p> <a> .\n<a> <q> <b> .\n");
    let n = [(1, "b"), (0, "a")];
    let c = fx.call().src(&["a"]).dst(&["b"]).directed(false).k(1);
    // DFS from a, per predicate: out(a, p) = [b]; in(a, p) = [b] (the edge
    // b -p-> a, traversed against its direction and printed in its real
    // direction 1 → 0); out(a, q) = [b].
    let want = [
        path("a", "b", &[(0, 1, "p")], &n),
        path("a", "b", &[(1, 0, "p")], &n),
        path("a", "b", &[(0, 1, "q")], &n),
    ];
    assert_eq!(c.json(GFnKind::KHopEnumerate), paths(&want));
    let c = fx
        .call()
        .src(&["a"])
        .dst(&["b"])
        .directed(false)
        .k(1)
        .ret(2);
    assert_eq!(c.json(GFnKind::KHopEnumerate), paths(&want[..2]));
    // Directed: only the two out-edges a -p-> b and a -q-> b.
    let c = fx.call().src(&["a"]).dst(&["b"]).directed(true).k(1);
    assert_eq!(
        c.json(GFnKind::KHopEnumerate),
        paths(&[want[0].clone(), want[2].clone()])
    );
    // The budget is per pair: two pairs, retNum 1 each.
    let c = fx
        .call()
        .src(&["a", "b"])
        .dst(&["b", "a"])
        .directed(true)
        .k(1)
        .ret(1);
    assert_eq!(
        c.json(GFnKind::KHopEnumerate),
        paths(&[
            want[0].clone(),
            path("a", "a", &[], &[]),
            path("b", "b", &[], &[]),
            path("b", "a", &[(1, 0, "p")], &[(0, "a"), (1, "b")]),
        ])
    );
}

// ---------------------------------------------------------------------------
// kHopCount / kHopNeighbor
// ---------------------------------------------------------------------------

fn kcount(s: &str, k: i64, n: usize) -> String {
    format!("{{\"src\":\"{}\",\"depth\":{k}, \"count\":{n}}}", br(s))
}

fn knbr(key: &str, s: &str, k: i64, nbrs: &[&str]) -> String {
    let list: Vec<String> = nbrs.iter().map(|n| format!("\"<{n}>\"")).collect();
    format!(
        "{{\"src\":\"{}\",\"depth\":{k}, \"{key}\":[{}]}}",
        br(s),
        list.join(",")
    )
}

/// kHopNeighbor.cpp: level-synchronous BFS; a vertex counts at the level it
/// is first seen. Alice, directed, {}: level 1 {Bob, Eve}, level 2 {Carol},
/// level 3 {Francis} → SPARQL_QUERY.md's `"depth":3, "count":1`.
#[test]
fn k_hop_count_and_neighbor_docs_example() {
    let fx = Fx::social();
    let c = |k| fx.call().src(&["Alice"]).directed(true).k(k);
    assert_eq!(
        c(3).json(GFnKind::KHopCount),
        "{\"paths\":[{\"src\":\"<Alice>\",\"depth\":3, \"count\":1}]}"
    );
    assert_eq!(
        c(3).json(GFnKind::KHopNeighbor),
        paths(&[knbr("neighbor", "Alice", 3, &["Francis"])])
    );
    // The constant-argument branch (WHERE without solutions) names it "dst".
    assert_eq!(
        c(3).where_empty().json(GFnKind::KHopNeighbor),
        paths(&[knbr("dst", "Alice", 3, &["Francis"])])
    );
    assert_eq!(
        c(1).json(GFnKind::KHopCount),
        paths(&[kcount("Alice", 1, 2)])
    );
    assert_eq!(
        c(2).json(GFnKind::KHopCount),
        paths(&[kcount("Alice", 2, 1)])
    );
    assert_eq!(
        c(4).json(GFnKind::KHopCount),
        paths(&[kcount("Alice", 4, 1)])
    );
    assert_eq!(
        c(5).json(GFnKind::KHopCount),
        paths(&[kcount("Alice", 5, 0)])
    );
    // k <= 0 → nothing; the depth prints as written (no 999).
    assert_eq!(
        c(0).json(GFnKind::KHopCount),
        paths(&[kcount("Alice", 0, 0)])
    );
    assert_eq!(
        c(-1).json(GFnKind::KHopCount),
        paths(&[kcount("Alice", -1, 0)])
    );
    assert_eq!(
        c(-1).json(GFnKind::KHopNeighbor),
        paths(&[knbr("neighbor", "Alice", -1, &[])])
    );
}

/// Undirected order (kHopNeighbor.cpp:32-45): per expanded vertex, every
/// predicate's in-neighbours, then every predicate's out-neighbours. Bob:
/// in(focus) = [Alice, Carol], in(like) = [Alice, Carol], out(focus) =
/// [Alice], out(like) = [Eve] → Alice, Carol, Eve. Level 2 expands Alice
/// (in(focus) → Dave), Carol (in(like) → Francis), Eve (nothing new).
/// retNum: stop once `len >= retNum` after an append — so 0 still gives one.
#[test]
fn k_hop_neighbor_order_and_ret_num() {
    let fx = Fx::social();
    let c = |k| fx.call().src(&["Bob"]).directed(false).k(k);
    assert_eq!(
        c(1).json(GFnKind::KHopNeighbor),
        paths(&[knbr("neighbor", "Bob", 1, &["Alice", "Carol", "Eve"])])
    );
    assert_eq!(
        c(1).ret(2).json(GFnKind::KHopNeighbor),
        paths(&[knbr("neighbor", "Bob", 1, &["Alice", "Carol"])])
    );
    assert_eq!(
        c(1).ret(0).json(GFnKind::KHopNeighbor),
        paths(&[knbr("neighbor", "Bob", 1, &["Alice"])])
    );
    // Negative retNum → 100.
    assert_eq!(
        c(1).ret(-3).json(GFnKind::KHopNeighbor),
        c(1).json(GFnKind::KHopNeighbor)
    );
    assert_eq!(
        c(2).json(GFnKind::KHopNeighbor),
        paths(&[knbr("neighbor", "Bob", 2, &["Dave", "Francis"])])
    );
    assert_eq!(c(2).json(GFnKind::KHopCount), paths(&[kcount("Bob", 2, 2)]));
    // kHopCount ignores retNum (it calls kHopNeighbor with -1).
    assert_eq!(
        c(1).ret(1).json(GFnKind::KHopCount),
        paths(&[kcount("Bob", 1, 3)])
    );
    // Predicate filter: {like} only: in(like) = [Alice, Carol], out = [Eve].
    assert_eq!(
        c(1).preds(&["like"], &[]).json(GFnKind::KHopNeighbor),
        paths(&[knbr("neighbor", "Bob", 1, &["Alice", "Carol", "Eve"])])
    );
    // {!focus, !like} = {dislike}: Bob has no dislike edge.
    assert_eq!(
        c(1).preds(&[], &["focus", "like"]).json(GFnKind::KHopCount),
        paths(&[kcount("Bob", 1, 0)])
    );
}

#[test]
fn k_hop_count_and_neighbor_per_source() {
    let fx = Fx::social_with_zed();
    let c = fx
        .call()
        .src(&["Alice", "Nobody", "Zed", "Bob", "Alice"])
        .directed(true)
        .k(1);
    assert_eq!(
        c.json(GFnKind::KHopCount),
        paths(&[
            kcount("Alice", 1, 2),
            kcount("", 1, 0),
            kcount("Zed", 1, 0),
            kcount("Bob", 1, 2),
        ])
    );
    assert_eq!(
        c.json(GFnKind::KHopNeighbor),
        paths(&[
            knbr("neighbor", "Alice", 1, &["Bob", "Eve"]),
            knbr("neighbor", "", 1, &[]),
            knbr("neighbor", "Zed", 1, &[]),
            knbr("neighbor", "Bob", 1, &["Alice", "Eve"]),
        ])
    );
    // No bindings → empty envelope.
    assert_eq!(fx.call().k(1).json(GFnKind::KHopCount), EMPTY);
}

// ---------------------------------------------------------------------------
// kHopAllNeighbors
// ---------------------------------------------------------------------------

fn kall(nbrs: &[&str]) -> String {
    let list: Vec<String> = nbrs.iter().map(|n| format!("\"<{n}>\"")).collect();
    format!("{{\"results\":[{}]}}", list.join(","))
}

/// kHopAllNeighbors.cpp: BFS from Bob (undirected, {}); every dequeued
/// vertex within k joins the (ascending) result *after* the retNum check.
/// Bob → nl {Alice, Eve, Carol} enqueued ascending; k = 1: Alice, Eve, Carol
/// join; their neighbours are at distance 2.
/// retNum 2: Bob, Alice join; at Eve |result| = 2 >= 2 → stop.
#[test]
fn k_hop_all_neighbors_traces() {
    let fx = Fx::social_with_zed();
    let c = |k| fx.call().src(&["Bob"]).directed(false).k(k);
    assert_eq!(
        c(1).json(GFnKind::KHopAllNeighbors),
        paths(&[kall(&["Alice", "Bob", "Eve", "Carol"])])
    );
    assert_eq!(
        c(1).ret(2).json(GFnKind::KHopAllNeighbors),
        paths(&[kall(&["Alice", "Bob"])])
    );
    assert_eq!(
        c(1).ret(0).json(GFnKind::KHopAllNeighbors),
        paths(&[kall(&[])])
    );
    // k = -1 → 999: the whole component (Zed is not in it).
    assert_eq!(
        c(-1).json(GFnKind::KHopAllNeighbors),
        paths(&[kall(&["Alice", "Bob", "Eve", "Carol", "Francis", "Dave"])])
    );
    // Directed from Alice, k = 2: Alice, Bob, Eve (1), Carol (2).
    assert_eq!(
        fx.call()
            .src(&["Alice"])
            .directed(true)
            .k(2)
            .json(GFnKind::KHopAllNeighbors),
        paths(&[kall(&["Alice", "Bob", "Eve", "Carol"])])
    );
    // Missing sources print nothing; an isolated one is its own result.
    assert_eq!(
        fx.call()
            .src(&["Bob", "Nobody", "Zed", "Alice"])
            .directed(true)
            .k(0)
            .json(GFnKind::KHopAllNeighbors),
        paths(&[kall(&["Bob"]), kall(&["Zed"]), kall(&["Alice"])])
    );
    assert_eq!(
        fx.call()
            .src(&["Zed"])
            .k(3)
            .ret(0)
            .json(GFnKind::KHopAllNeighbors),
        paths(&[kall(&[])])
    );
}

// ---------------------------------------------------------------------------
// bfsCount
// ---------------------------------------------------------------------------

fn levels(src: &str, counts: &[u32]) -> String {
    let items: Vec<String> = counts
        .iter()
        .enumerate()
        .map(|(d, c)| format!("{{\"depth\":{d}, \"count\":{c}}}"))
        .collect();
    format!(
        "{{\"src\":\"{}\",\"results\":[{}]}}",
        br(src),
        items.join(", ")
    )
}

/// SPARQL_QUERY.md: `bfsCount(<Alice>, true, {<like>,<focus>,<dislike>})`
/// → 1, 2 (Bob, Eve), 1 (Carol), 1 (Francis), 1 (Dave).
#[test]
fn bfs_count_docs_example_and_filters() {
    let fx = Fx::social();
    let c = || fx.call().src(&["Alice"]).directed(true);
    assert_eq!(
        c().preds(&["like", "focus", "dislike"], &[]).json(GFnKind::BfsCount),
        "{\"paths\":[{\"src\":\"<Alice>\",\"results\":[{\"depth\":0, \"count\":1}, {\"depth\":1, \"count\":2}, {\"depth\":2, \"count\":1}, {\"depth\":3, \"count\":1}, {\"depth\":4, \"count\":1}]}]}"
    );
    // {!dislike}: Alice → Bob → Eve → Carol (Francis only via dislike).
    assert_eq!(
        c().preds(&[], &["dislike"]).json(GFnKind::BfsCount),
        paths(&[levels("Alice", &[1, 1, 1, 1])])
    );
    // An exclusion makes the include list irrelevant: {dislike, !like} =
    // {focus, dislike}: Alice → Bob, Eve; nothing further.
    assert_eq!(
        c().preds(&["dislike"], &["like"]).json(GFnKind::BfsCount),
        paths(&[levels("Alice", &[1, 2])])
    );
    // An unknown predicate resolves to the empty list → only the source.
    assert_eq!(
        c().preds(&["nope"], &[]).json(GFnKind::BfsCount),
        paths(&[levels("Alice", &[1])])
    );
    // Undirected from Bob: {Alice, Carol, Eve}, then {Dave, Francis}.
    assert_eq!(
        fx.call()
            .src(&["Bob"])
            .directed(false)
            .json(GFnKind::BfsCount),
        paths(&[levels("Bob", &[1, 3, 2])])
    );
    // Missing source: depth 0 only, printed "".
    assert_eq!(
        fx.call()
            .src(&["Nobody", "Dave"])
            .directed(true)
            .preds(&["focus"], &[])
            .json(GFnKind::BfsCount),
        paths(&[levels("", &[1]), levels("Dave", &[1, 2, 1])])
    );
}

// ---------------------------------------------------------------------------
// SSSP / SSSPLen
// ---------------------------------------------------------------------------

/// SSSP.cpp: BFS from Alice over {like, focus}: like → Bob, Bob -like-> Eve,
/// Eve -like-> Carol. The result map's iteration order for the insertion
/// sequence 0, 1, 2, 3 is 3, 2, 1, 0.
#[test]
fn sssp_directed() {
    let fx = Fx::social();
    let c = fx
        .call()
        .src(&["Alice"])
        .directed(true)
        .preds(&["like", "focus"], &[]);
    assert_eq!(
        c.json(GFnKind::Sssp),
        paths(&[
            path(
                "Alice",
                "Carol",
                &[(0, 1, "like"), (1, 2, "like"), (2, 3, "like")],
                &sn(&[3, 2, 1, 0])
            ),
            path(
                "Alice",
                "Eve",
                &[(0, 1, "like"), (1, 2, "like")],
                &sn(&[2, 1, 0])
            ),
            path("Alice", "Bob", &[(0, 1, "like")], &sn(&[1, 0])),
            path("Alice", "Alice", &[], &sn(&[0])),
        ])
    );
    assert_eq!(
        c.json(GFnKind::SsspLen),
        "{\"paths\":[{\"src\":\"<Alice>\",\"dst\":\"<Carol>\",\"length\":3},{\"src\":\"<Alice>\",\"dst\":\"<Eve>\",\"length\":2},{\"src\":\"<Alice>\",\"dst\":\"<Bob>\",\"length\":1},{\"src\":\"<Alice>\",\"dst\":\"<Alice>\",\"length\":0}]}"
    );
}

/// Undirected from Eve over {like}: Eve: out Carol, in Bob; Carol: in
/// Francis; Bob: in Alice → discovery 2, 3, 1, 4, 0 → map order 0, 4, 1, 3,
/// 2. In-edges are recorded with the plain predicate id, so they print
/// forward (Eve → Bob although Bob likes Eve).
#[test]
fn sssp_undirected_records_in_edges_forward() {
    let fx = Fx::social();
    let c = fx
        .call()
        .src(&["Eve"])
        .directed(false)
        .preds(&["like"], &[]);
    assert_eq!(
        c.json(GFnKind::Sssp),
        paths(&[
            path(
                "Eve",
                "Alice",
                &[(2, 1, "like"), (1, 0, "like")],
                &sn(&[0, 1, 2])
            ),
            path(
                "Eve",
                "Francis",
                &[(2, 3, "like"), (3, 4, "like")],
                &sn(&[4, 3, 2])
            ),
            path("Eve", "Bob", &[(2, 1, "like")], &sn(&[1, 2])),
            path("Eve", "Carol", &[(2, 3, "like")], &sn(&[3, 2])),
            path("Eve", "Eve", &[], &sn(&[2])),
        ])
    );
    let len = |d: &str, l| format!("{{\"src\":\"<Eve>\",\"dst\":\"<{d}>\",\"length\":{l}}}");
    assert_eq!(
        c.json(GFnKind::SsspLen),
        paths(&[
            len("Alice", 2),
            len("Francis", 2),
            len("Bob", 1),
            len("Carol", 1),
            len("Eve", 0),
        ])
    );
}

#[test]
fn sssp_per_source_and_special_cases() {
    let fx = Fx::social_with_zed();
    // Francis over {like}: 4 → 3 → 1 → 2 → map order 2, 1, 3, 4; Dave has no
    // like edge: only itself. Sources are separated by ",".
    let c = fx
        .call()
        .src(&["Francis", "Dave"])
        .directed(true)
        .preds(&["like"], &[]);
    let len =
        |s: &str, d: &str, l| format!("{{\"src\":\"<{s}>\",\"dst\":\"<{d}>\",\"length\":{l}}}");
    assert_eq!(
        c.json(GFnKind::SsspLen),
        paths(&[
            len("Francis", "Eve", 3),
            len("Francis", "Bob", 2),
            len("Francis", "Carol", 1),
            len("Francis", "Francis", 0),
            len("Dave", "Dave", 0),
        ])
    );
    // A missing source: res[-1] = [-1] → nodeIndex -1, empty IRIs.
    let m = fx.call().src(&["Nobody"]).directed(true);
    assert_eq!(
        m.json(GFnKind::Sssp),
        "{\"paths\":[{\"src\":\"\",\"dst\":\"\",\"edges\":[],\"nodes\":[{\"nodeIndex\":-1,\"nodeIRI\":\"\"}]}]}"
    );
    assert_eq!(
        m.json(GFnKind::SsspLen),
        "{\"paths\":[{\"src\":\"\",\"dst\":\"\",\"length\":0}]}"
    );
    // An isolated source: only itself.
    let z = fx.call().src(&["Zed"]).directed(false);
    assert_eq!(
        z.json(GFnKind::Sssp),
        paths(&[path("Zed", "Zed", &[], &[(6, "Zed")])])
    );
    assert_eq!(z.json(GFnKind::SsspLen), paths(&[len("Zed", "Zed", 0)]));
}

// ---------------------------------------------------------------------------
// cancellation
// ---------------------------------------------------------------------------

#[test]
fn every_path_function_honours_cancellation() {
    let fx = Fx::social();
    let flag = AtomicBool::new(true);
    let ctx = fx.ctx().with_cancel(&flag);
    let c = fx.call().src(&["Alice"]).dst(&["Carol"]).k(3);
    for kind in GFnKind::ALL {
        if kind.family() != gfun::GFnFamily::Paths {
            continue;
        }
        assert!(
            matches!(gfun::eval(kind, &ctx, &c.a), Err(GStoreError::Cancelled)),
            "{kind}"
        );
        // ... and every one of them is implemented.
        let v = gfun::eval(kind, &fx.ctx(), &c.a);
        assert!(v.is_ok(), "{kind}: {v:?}");
    }
}

/// `SSSP` prints a whole path per reachable vertex — `O(V²)` text on a
/// chain — so it stops with a query error once its output passes the
/// context's limit (default `paths::MAX_OUTPUT_BYTES`) instead of exhausting
/// memory; so does every other function that writes entries.
#[test]
fn every_path_function_bounds_its_output() {
    let chain: String = (0..100)
        .map(|i| format!("<n{i}> <next> <n{}> .\n", i + 1))
        .collect();
    let fx = Fx::new(&chain);
    let c = fx.call().src(&["n0"]).directed(true);
    let full = c.json(GFnKind::Sssp);
    assert!(full.len() > 200_000, "{}", full.len());
    let limited = |bytes| gfun::eval(GFnKind::Sssp, &fx.ctx().with_max_output_bytes(bytes), &c.a);
    // The limit applies to the entries (the envelope adds 12 bytes).
    assert_eq!(
        limited(full.len() - 12).unwrap(),
        GFunValue::Json(full.clone())
    );
    let err = limited(full.len() / 2).unwrap_err();
    assert!(
        matches!(&err, GStoreError::Query(m) if m.starts_with("SSSP: more than") && m.contains("bytes")),
        "{err}"
    );
    assert_eq!(fx.ctx().max_output_bytes(), gfun::paths::MAX_OUTPUT_BYTES);
    // Every function with entries, over the social graph's all-pairs product.
    let s = Fx::social();
    let c = s.call().src(&NAMES).dst(&NAMES).directed(false).k(3);
    let mut unbounded = Vec::new();
    for kind in GFnKind::ALL {
        if kind.family() != gfun::GFnFamily::Paths {
            continue;
        }
        match gfun::eval(kind, &s.ctx(), &c.a).unwrap() {
            GFunValue::Json(text) if text != r#"{"paths":[]}"# => {}
            _ => {
                unbounded.push(kind);
                continue;
            }
        }
        let r = gfun::eval(kind, &s.ctx().with_max_output_bytes(1), &c.a);
        assert!(
            matches!(&r, Err(GStoreError::Query(m)) if m.contains("bytes of output")),
            "{kind}: {r:?}"
        );
    }
    // Only the functions that never write an entry.
    assert_eq!(
        unbounded,
        [
            GFnKind::SimpleCyclePath,
            GFnKind::SimpleCycleBoolean,
            GFnKind::CycleBoolean
        ]
    );
}

// ---------------------------------------------------------------------------
// randomized consistency checks against reference implementations
// ---------------------------------------------------------------------------

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// One hop of a path: (predicate index, traversed against its direction).
type Hop = (usize, bool);

/// A path as (vertex sequence, hops).
type RefPath = (Vec<usize>, Vec<Hop>);

/// A path as (vertex sequence, predicate per hop) — `shortestPaths`, whose
/// edges all print forward.
type PredPath = (Vec<usize>, Vec<usize>);

/// A random multigraph `<n0>…` with predicates `<p0>…` (entity id = index,
/// predicate id order = index order) and adjacency lists for the references.
struct Rand {
    fx: Fx,
    n: usize,
    np: usize,
    /// `out[x][p]`, `inn[x][p]`: neighbours ascending.
    out: Vec<Vec<Vec<usize>>>,
    inn: Vec<Vec<Vec<usize>>>,
}

/// A resolved predicate filter for the references (`order`) and its written
/// form (`inc`, `exc`).
#[derive(Clone, Debug)]
struct Filt {
    inc: Vec<String>,
    exc: Vec<String>,
    order: Vec<usize>,
}

impl Rand {
    fn new(seed: u64, n: usize, np: usize, m: usize) -> Rand {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut nt = String::new();
        // Fix entity ids (first appearance) and predicate ids: literal
        // objects create no CSR edges.
        for i in 0..n {
            nt.push_str(&format!("<n{i}> <name> \"{i}\" .\n"));
        }
        for p in 0..np {
            nt.push_str(&format!("<n0> <p{p}> \"x\" .\n"));
        }
        let mut edges = BTreeSet::new();
        for _ in 0..m {
            let s = rng.below(n);
            // a few self-loops and reciprocal / parallel edges
            let o = match rng.below(12) {
                0 => s,
                _ => rng.below(n),
            };
            let p = rng.below(np);
            edges.insert((s, p, o));
            if rng.below(8) == 0 {
                edges.insert((o, p, s));
            }
            if rng.below(8) == 0 {
                edges.insert((s, (p + 1) % np, o));
            }
        }
        let mut out = vec![vec![Vec::new(); np]; n];
        let mut inn = vec![vec![Vec::new(); np]; n];
        for &(s, p, o) in &edges {
            nt.push_str(&format!("<n{s}> <p{p}> <n{o}> .\n"));
            out[s][p].push(o);
            inn[o][p].push(s);
        }
        for lists in out.iter_mut().chain(inn.iter_mut()) {
            lists.iter_mut().for_each(|l| l.sort_unstable());
        }
        let fx = Fx::new(&nt);
        for i in 0..n {
            assert_eq!(fx.eid(&format!("n{i}")), Some(i as u32));
        }
        for p in 0..np {
            assert_eq!(
                fx.db.dict().predicate_id(&format!("<p{p}>")),
                Some(p as u32 + 1)
            );
        }
        Rand {
            fx,
            n,
            np,
            out,
            inn,
        }
    }

    /// `{}`, a random include list (random order), or one exclusion.
    fn filters(&self, rng: &mut Rng) -> Vec<Filt> {
        let all = Filt {
            inc: vec![],
            exc: vec![],
            order: (0..self.np).collect(),
        };
        let mut order: Vec<usize> = (0..self.np).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, rng.below(i + 1));
        }
        order.truncate(1 + rng.below(self.np));
        let inc = Filt {
            inc: order.iter().map(|p| format!("p{p}")).collect(),
            exc: vec![],
            order: order.clone(),
        };
        let x = rng.below(self.np);
        let exc = Filt {
            inc: vec![],
            exc: vec![format!("p{x}")],
            order: (0..self.np).filter(|&p| p != x).collect(),
        };
        vec![all, inc, exc]
    }

    fn name(i: usize) -> String {
        format!("n{i}")
    }

    fn call(&self, f: &Filt, srcs: &[usize], dsts: &[usize], directed: bool) -> Call<'_> {
        let s: Vec<String> = srcs.iter().map(|&i| Rand::name(i)).collect();
        let d: Vec<String> = dsts.iter().map(|&i| Rand::name(i)).collect();
        let s: Vec<&str> = s.iter().map(String::as_str).collect();
        let d: Vec<&str> = d.iter().map(String::as_str).collect();
        let inc: Vec<&str> = f.inc.iter().map(String::as_str).collect();
        let exc: Vec<&str> = f.exc.iter().map(String::as_str).collect();
        self.fx
            .call()
            .src(&s)
            .dst(&d)
            .directed(directed)
            .preds(&inc, &exc)
    }

    /// Neighbours per predicate in filter order: out-neighbours, then (when
    /// undirected) in-neighbours; `(y, p, reversed)`, once per edge.
    fn nbrs(&self, x: usize, f: &Filt, directed: bool) -> Vec<(usize, usize, bool)> {
        let mut v = Vec::new();
        for &p in &f.order {
            v.extend(self.out[x][p].iter().map(|&y| (y, p, false)));
            if !directed {
                v.extend(self.inn[x][p].iter().map(|&y| (y, p, true)));
            }
        }
        v
    }

    /// BFS distances and discovery order.
    fn bfs(&self, s: usize, f: &Filt, directed: bool) -> (Vec<Option<usize>>, Vec<usize>) {
        let mut dist = vec![None; self.n];
        let mut order = vec![s];
        dist[s] = Some(0);
        let mut head = 0;
        while head < order.len() {
            let x = order[head];
            head += 1;
            for (y, _, _) in self.nbrs(x, f, directed) {
                if dist[y].is_none() {
                    dist[y] = Some(dist[x].unwrap() + 1);
                    order.push(y);
                }
            }
        }
        (dist, order)
    }

    /// Number of shortest edge sequences s → t (each edge, and undirected
    /// each direction of it, counted separately).
    fn count_shortest(&self, s: usize, t: usize, f: &Filt, directed: bool) -> u64 {
        let (dist, order) = self.bfs(s, f, directed);
        let mut cnt = vec![0u64; self.n];
        cnt[s] = 1;
        for &x in &order {
            for (y, _, _) in self.nbrs(x, f, directed) {
                if dist[y] == Some(dist[x].unwrap() + 1) {
                    cnt[y] += cnt[x];
                }
            }
        }
        if dist[t].is_some() {
            cnt[t]
        } else {
            0
        }
    }

    /// The predicates usable for the hop x → y (`x -p-> y`, or undirected
    /// `y -p-> x`), ascending, once each.
    fn hop_preds(&self, x: usize, y: usize, f: &Filt, directed: bool) -> Vec<usize> {
        let mut ps: Vec<usize> = f
            .order
            .iter()
            .copied()
            .filter(|&p| self.out[x][p].contains(&y) || (!directed && self.inn[x][p].contains(&y)))
            .collect();
        ps.sort_unstable();
        ps
    }

    /// All shortest paths s → t as (vertex sequence, predicate per hop).
    fn all_shortest(&self, s: usize, t: usize, f: &Filt, directed: bool) -> Vec<PredPath> {
        let (dist, _) = self.bfs(s, f, directed);
        let mut out = Vec::new();
        if dist[t].is_none() {
            return out;
        }
        // vertex sequences: walk back from t over predecessors one closer
        struct W<'r> {
            r: &'r Rand,
            s: usize,
            dist: Vec<Option<usize>>,
            f: &'r Filt,
            directed: bool,
        }
        fn walk(w: &W<'_>, x: usize, acc: &mut Vec<usize>, seqs: &mut Vec<Vec<usize>>) {
            acc.push(x);
            if x == w.s {
                seqs.push(acc.iter().rev().copied().collect());
            } else {
                for (y, dy) in w.dist.iter().enumerate() {
                    if dy.is_some_and(|d| d + 1 == w.dist[x].unwrap())
                        && !w.r.hop_preds(y, x, w.f, w.directed).is_empty()
                    {
                        walk(w, y, acc, seqs);
                    }
                }
            }
            acc.pop();
        }
        let mut seqs = Vec::new();
        let w = W {
            r: self,
            s,
            dist,
            f,
            directed,
        };
        walk(&w, t, &mut Vec::new(), &mut seqs);
        for seq in seqs {
            let mut combos: Vec<Vec<usize>> = vec![vec![]];
            for w in seq.windows(2) {
                let ps = self.hop_preds(w[0], w[1], f, directed);
                combos = combos
                    .into_iter()
                    .flat_map(|c| {
                        ps.iter().map(move |&p| {
                            let mut c = c.clone();
                            c.push(p);
                            c
                        })
                    })
                    .collect();
            }
            for c in combos {
                out.push((seq.clone(), c));
            }
        }
        out
    }

    /// All simple paths s → t with at most k edges, DFS in neighbour order,
    /// stopping at t: (vertex sequence, (pred, reversed) per hop).
    fn enumerate(&self, s: usize, t: usize, k: usize, f: &Filt, directed: bool) -> Vec<RefPath> {
        let mut out = Vec::new();
        let mut path: RefPath = (vec![s], vec![]);
        self.enumerate_from(t, k, f, directed, &mut path, &mut out);
        out
    }

    fn enumerate_from(
        &self,
        t: usize,
        k: usize,
        f: &Filt,
        directed: bool,
        path: &mut RefPath,
        out: &mut Vec<RefPath>,
    ) {
        let x = *path.0.last().unwrap();
        if x == t {
            out.push(path.clone());
            return;
        }
        if path.1.len() >= k {
            return;
        }
        for (y, p, rev) in self.nbrs(x, f, directed) {
            if path.0.contains(&y) {
                continue;
            }
            path.0.push(y);
            path.1.push((p, rev));
            self.enumerate_from(t, k, f, directed, path, out);
            path.0.pop();
            path.1.pop();
        }
    }

    /// kHopNeighbor.cpp transliterated (vertex ids are indices).
    fn k_hop_neighbor(
        &self,
        s: usize,
        directed: bool,
        mut k: i64,
        f: &Filt,
        ret: i64,
    ) -> Vec<usize> {
        let mut visited = vec![false; self.n];
        visited[s] = true;
        let mut cnt = Vec::new();
        let mut q = vec![s];
        while k != 0 && !q.is_empty() {
            let mut next = Vec::new();
            for &v in &q {
                let passes: &[bool] = if directed { &[true] } else { &[false, true] };
                for &out in passes {
                    for &p in &f.order {
                        let list = if out {
                            &self.out[v][p]
                        } else {
                            &self.inn[v][p]
                        };
                        for &n in list {
                            if !visited[n] {
                                if k == 1 {
                                    cnt.push(n);
                                    if ret >= 0 && cnt.len() as i64 >= ret {
                                        return cnt;
                                    }
                                } else {
                                    next.push(n);
                                }
                                visited[n] = true;
                            }
                        }
                    }
                }
            }
            k -= 1;
            q = next;
        }
        cnt
    }

    /// kHopReachable.cpp's BFS phase transliterated (the push phase never
    /// runs below ~10^8 vertices); `-1` is the "past the out-degree" id.
    fn k_hop_reachable(&self, u: usize, v: usize, directed: bool, k: i64, f: &Filt) -> bool {
        if u == v {
            return true;
        }
        let (mut ut, mut vt) = (0, 0);
        for &p in &f.order {
            ut += self.out[u][p].len();
            vt += self.inn[v][p].len();
            if !directed {
                ut += self.inn[u][p].len();
                vt += self.out[v][p].len();
            }
        }
        if ut == 0 || vt == 0 {
            return false;
        }
        let out_at = |x: i64, p: usize, i: usize| -> i64 {
            if x < 0 {
                -1
            } else {
                self.out[x as usize][p].get(i).map_or(-1, |&y| y as i64)
            }
        };
        let size = |x: i64, p: usize, out: bool| -> usize {
            if x < 0 {
                0
            } else if out {
                self.out[x as usize][p].len()
            } else {
                self.inn[x as usize][p].len()
            }
        };
        let (u, v) = (u as i64, v as i64);
        let l1: HashMap<i64, i64> = HashMap::from([(u, 0)]);
        let l2: HashMap<i64, i64> = HashMap::from([(v, 0)]);
        let mut f1: HashMap<i64, i64> = HashMap::from([(u, 0)]);
        let mut f2: HashMap<i64, i64> = HashMap::from([(v, 0)]);
        let mut fwd: VecDeque<i64> = VecDeque::from([u]);
        let mut fwd_next: VecDeque<i64> = VecDeque::new();
        let mut bwd: VecDeque<i64> = VecDeque::from([v]);
        let mut bwd_next: VecDeque<i64> = VecDeque::new();
        let (mut full1, mut full2) = (1i64, 1i64);
        while full1 <= k
            && full2 <= k
            && (!fwd.is_empty() || !fwd_next.is_empty())
            && (!bwd.is_empty() || !bwd_next.is_empty())
        {
            let mut op = 0;
            while (!fwd.is_empty() || !fwd_next.is_empty()) && op < 10 && full1 <= k {
                let Some(cur) = fwd.pop_front() else {
                    std::mem::swap(&mut fwd, &mut fwd_next);
                    full1 += 1;
                    continue;
                };
                op += 1;
                for &p in &f.order {
                    let mut adjs: Vec<i64> =
                        (0..size(cur, p, true)).map(|i| out_at(cur, p, i)).collect();
                    if !directed {
                        adjs.extend((0..size(cur, p, false)).map(|i| out_at(cur, p, i)));
                    }
                    for adj in adjs {
                        if f1.contains_key(&adj) {
                            continue;
                        }
                        f1.insert(adj, full1);
                        if l2.get(&adj).is_some_and(|&x| full1 + x <= k)
                            || f2.get(&adj).is_some_and(|&x| full1 + x <= k)
                        {
                            return true;
                        }
                        fwd_next.push_back(adj);
                    }
                }
            }
            let mut op = 0;
            while (!bwd.is_empty() || !bwd_next.is_empty()) && op < 10 && full2 <= k {
                let Some(cur) = bwd.pop_front() else {
                    std::mem::swap(&mut bwd, &mut bwd_next);
                    full2 += 1;
                    continue;
                };
                op += 1;
                for &p in &f.order {
                    let mut adjs: Vec<i64> = (0..size(cur, p, false))
                        .map(|i| out_at(cur, p, i))
                        .collect();
                    if !directed {
                        adjs.extend((0..size(cur, p, true)).map(|i| out_at(cur, p, i)));
                    }
                    for adj in adjs {
                        if f2.contains_key(&adj) {
                            continue;
                        }
                        f2.insert(adj, full2);
                        if l1.get(&adj).is_some_and(|&x| x + full2 <= k)
                            || f1.get(&adj).is_some_and(|&x| x + full2 <= k)
                        {
                            return true;
                        }
                        fwd_next.push_back(adj);
                    }
                }
            }
        }
        false
    }
}

// ---- a minimal JSON reader for the outputs ----------------------------------

#[derive(Debug, Clone, PartialEq)]
enum J {
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    fn parse(s: &str) -> J {
        let b = s.as_bytes();
        let mut i = 0;
        let v = J::val(b, &mut i);
        J::ws(b, &mut i);
        assert_eq!(i, b.len(), "trailing text in {s}");
        v
    }

    fn ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    }

    fn val(b: &[u8], i: &mut usize) -> J {
        J::ws(b, i);
        match b[*i] {
            b'{' => {
                *i += 1;
                let mut kv = Vec::new();
                J::ws(b, i);
                if b[*i] == b'}' {
                    *i += 1;
                    return J::Obj(kv);
                }
                loop {
                    J::ws(b, i);
                    let J::Str(k) = J::val(b, i) else {
                        panic!("key")
                    };
                    J::ws(b, i);
                    assert_eq!(b[*i], b':');
                    *i += 1;
                    kv.push((k, J::val(b, i)));
                    J::ws(b, i);
                    *i += 1;
                    if b[*i - 1] == b'}' {
                        return J::Obj(kv);
                    }
                    assert_eq!(b[*i - 1], b',');
                }
            }
            b'[' => {
                *i += 1;
                let mut v = Vec::new();
                J::ws(b, i);
                if b[*i] == b']' {
                    *i += 1;
                    return J::Arr(v);
                }
                loop {
                    v.push(J::val(b, i));
                    J::ws(b, i);
                    *i += 1;
                    if b[*i - 1] == b']' {
                        return J::Arr(v);
                    }
                    assert_eq!(b[*i - 1], b',');
                }
            }
            b'"' => {
                *i += 1;
                let start = *i;
                while b[*i] != b'"' {
                    assert_ne!(b[*i], b'\\', "no escapes expected");
                    *i += 1;
                }
                *i += 1;
                J::Str(String::from_utf8(b[start..*i - 1].to_vec()).unwrap())
            }
            _ => {
                let start = *i;
                while *i < b.len() && (b[*i] == b'-' || b[*i] == b'.' || b[*i].is_ascii_digit()) {
                    *i += 1;
                }
                J::Num(std::str::from_utf8(&b[start..*i]).unwrap().parse().unwrap())
            }
        }
    }

    fn get(&self, key: &str) -> &J {
        match self {
            J::Obj(kv) => &kv.iter().find(|(k, _)| k == key).unwrap().1,
            _ => panic!("not an object: {self:?}"),
        }
    }

    fn arr(&self) -> &[J] {
        match self {
            J::Arr(v) => v,
            _ => panic!("not an array: {self:?}"),
        }
    }

    fn str(&self) -> &str {
        match self {
            J::Str(s) => s,
            _ => panic!("not a string: {self:?}"),
        }
    }

    fn int(&self) -> i64 {
        match self {
            J::Num(x) => *x as i64,
            _ => panic!("not a number: {self:?}"),
        }
    }
}

/// `"<n12>"` → 12.
fn idx(iri: &str) -> usize {
    iri.trim_start_matches("<n")
        .trim_end_matches('>')
        .parse()
        .unwrap()
}

/// `"<p2>"` → 2.
fn pidx(iri: &str) -> usize {
    iri.trim_start_matches("<p")
        .trim_end_matches('>')
        .parse()
        .unwrap()
}

/// A path entry walked from `src`: vertex sequence and (pred, reversed) per
/// hop. `forward_only`: every edge must be printed from the earlier vertex
/// (functions that record in-edges with the plain predicate id).
/// The empty path (`u == v` entries) walks to no vertices at all.
fn walk_path(e: &J, forward_only: bool) -> (usize, usize, Vec<usize>, Vec<Hop>) {
    let src = idx(e.get("src").str());
    let dst = idx(e.get("dst").str());
    if e.get("nodes").arr().is_empty() {
        assert!(e.get("edges").arr().is_empty());
        return (src, dst, vec![], vec![]);
    }
    let mut cur = src;
    let mut nodes = vec![src];
    let mut hops = Vec::new();
    for ed in e.get("edges").arr() {
        let (from, to) = (
            ed.get("fromNode").int() as usize,
            ed.get("toNode").int() as usize,
        );
        let p = pidx(ed.get("predIRI").str());
        if from == cur {
            cur = to;
            hops.push((p, false));
        } else {
            assert!(!forward_only, "reversed edge in {e:?}");
            assert_eq!(to, cur, "broken path {e:?}");
            cur = from;
            hops.push((p, true));
        }
        nodes.push(cur);
    }
    // `nodes` lists each distinct path vertex once.
    let listed: BTreeSet<usize> = e
        .get("nodes")
        .arr()
        .iter()
        .map(|n| {
            let i = n.get("nodeIndex").int() as usize;
            assert_eq!(idx(n.get("nodeIRI").str()), i);
            i
        })
        .collect();
    assert_eq!(listed, nodes.iter().copied().collect::<BTreeSet<_>>());
    (src, dst, nodes, hops)
}

fn entries(json: &str) -> Vec<J> {
    J::parse(json).get("paths").arr().to_vec()
}

/// Random graphs: (seed, filters, directions, a handful of sources and
/// targets each).
fn random_cases(mut each: impl FnMut(&Rand, &Filt, bool, &[usize], &[usize], &mut Rng)) {
    for seed in 0..10u64 {
        let (n, np) = (14 + (seed as usize % 5), 3);
        let r = Rand::new(seed, n, np, 2 * n + 4);
        let mut rng = Rng(seed + 77);
        for f in r.filters(&mut rng) {
            for directed in [true, false] {
                let srcs: Vec<usize> = (0..4).map(|_| rng.below(n)).collect();
                let dsts: Vec<usize> = (0..4).map(|_| rng.below(n)).collect();
                each(&r, &f, directed, &srcs, &dsts, &mut rng);
            }
        }
    }
}

/// Distinct values in first-appearance order.
fn distinct(v: &[usize]) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    v.iter().copied().filter(|x| seen.insert(*x)).collect()
}

fn pairs(srcs: &[usize], dsts: &[usize]) -> Vec<(usize, usize)> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for &u in srcs {
        for &v in dsts {
            if seen.insert((u, v)) {
                out.push((u, v));
            }
        }
    }
    out
}

#[test]
fn random_shortest_path_family() {
    let (mut deep, mut reversed) = (0, 0);
    random_cases(|r, f, directed, srcs, dsts, rng| {
        let c = r.call(f, srcs, dsts, directed);
        let sp = entries(&c.json(GFnKind::ShortestPath));
        let mut want_len = Vec::new();
        let mut want_pairs = Vec::new();
        for (u, v) in pairs(srcs, dsts) {
            let d = if u == v {
                Some(0)
            } else {
                r.bfs(u, f, directed).0[v]
            };
            if let Some(d) = d {
                want_len.push(format!(
                    "{{\"src\":\"<n{u}>\",\"dst\":\"<n{v}>\",\"length\":{d}}}"
                ));
                want_pairs.push((u, v, d));
            }
        }
        assert_eq!(c.json(GFnKind::ShortestPathLen), paths(&want_len));
        assert_eq!(sp.len(), want_pairs.len());
        for (e, &(u, v, d)) in sp.iter().zip(&want_pairs) {
            let (s, t, nodes, hops) = walk_path(e, directed);
            assert_eq!((s, t), (u, v));
            assert_eq!(hops.len(), d, "{e:?}");
            if d > 0 {
                assert_eq!((nodes[0], nodes[d]), (u, v));
            }
            deep += usize::from(d >= 2);
            for (w, &(p, rev)) in nodes.windows(2).zip(&hops) {
                assert!(f.order.contains(&p));
                reversed += usize::from(rev);
                if rev {
                    assert!(r.out[w[1]][p].contains(&w[0]));
                } else {
                    assert!(r.out[w[0]][p].contains(&w[1]));
                }
            }
        }
        // kHopReachablePath = shortestPath entries of at most k edges.
        let k = rng.below(4) as i64;
        let kp = entries(&c.k(k).json(GFnKind::KHopReachablePath));
        let kept: Vec<&J> = sp
            .iter()
            .zip(&want_pairs)
            .filter(|(_, &(u, v, d))| u == v || d as i64 <= k)
            .map(|(e, _)| e)
            .collect();
        assert_eq!(kp.iter().collect::<Vec<_>>(), kept);
    });
    // the checks above are not vacuous
    eprintln!("shortestPath: {deep} paths of 2+ hops, {reversed} reversed edges");
    assert!(deep >= 50 && reversed >= 20, "{deep} {reversed}");
}

#[test]
fn random_shortest_path_count() {
    let mut multi = 0;
    random_cases(|r, f, directed, srcs, dsts, _| {
        // distinct vertices only (u == v is a documented quirk)
        let dsts: Vec<usize> = dsts.iter().copied().filter(|d| !srcs.contains(d)).collect();
        let c = r.call(f, srcs, &dsts, directed);
        let want: Vec<String> = pairs(srcs, &dsts)
            .into_iter()
            .map(|(u, v)| {
                let n = r.count_shortest(u, v, f, directed) as u32 as i32;
                multi += usize::from(n >= 2);
                format!("{{\"src\":\"<n{u}>\", \"dst\":\"<n{v}>\", \"count\":{n}}}")
            })
            .collect();
        assert_eq!(c.json(GFnKind::ShortestPathCount), paths(&want));
    });
    eprintln!("shortestPathCount: {multi} pairs with 2+ shortest paths");
    assert!(multi >= 20, "{multi}");
}

#[test]
fn random_shortest_paths() {
    let (mut multi, mut total) = (0, 0);
    random_cases(|r, f, directed, srcs, dsts, _| {
        let c = r.call(f, srcs, dsts, directed);
        let got = entries(&c.json(GFnKind::ShortestPaths));
        let mut by_pair: BTreeMap<(usize, usize), Vec<PredPath>> = BTreeMap::new();
        let mut order = Vec::new();
        for e in &got {
            let (s, t, nodes, hops) = walk_path(e, true);
            if order.last() != Some(&(s, t)) {
                order.push((s, t));
            }
            by_pair
                .entry((s, t))
                .or_default()
                .push((nodes, hops.into_iter().map(|(p, _)| p).collect()));
        }
        let mut want_order = Vec::new();
        for (u, v) in pairs(srcs, dsts) {
            let want = if u == v {
                vec![(vec![u], vec![])]
            } else {
                r.all_shortest(u, v, f, directed)
            };
            if want.is_empty() {
                assert!(!by_pair.contains_key(&(u, v)));
                continue;
            }
            want_order.push((u, v));
            let mut got = by_pair.remove(&(u, v)).unwrap();
            if u == v {
                // the empty path: no nodes at all
                assert_eq!(got.len(), 1);
                continue;
            }
            let mut want = want;
            multi += usize::from(want.len() >= 2);
            total += want.len();
            got.sort();
            want.sort();
            assert_eq!(got, want, "{u} → {v} {f:?} directed={directed}");
        }
        assert_eq!(order, want_order);
        assert!(by_pair.is_empty());
    });
    eprintln!("shortestPaths: {total} paths, {multi} pairs with 2+");
    assert!(multi >= 20 && total >= 200, "{multi} {total}");
}

#[test]
fn random_k_hop_enumerate() {
    let (mut total, mut reversed) = (0, 0);
    random_cases(|r, f, directed, srcs, dsts, rng| {
        let k = 1 + rng.below(3);
        let ret = [-1i64, 1, 3][rng.below(3)];
        let c = r.call(f, srcs, dsts, directed).k(k as i64).ret(ret);
        let got = entries(&c.json(GFnKind::KHopEnumerate));
        let mut want = Vec::new();
        for (u, v) in pairs(srcs, dsts) {
            if u == v {
                want.push((u, v, vec![u], vec![]));
                continue;
            }
            let mut ps = r.enumerate(u, v, k, f, directed);
            if ret >= 0 {
                ps.truncate(ret as usize);
            }
            want.extend(ps.into_iter().map(|(n, h)| (u, v, n, h)));
        }
        assert_eq!(got.len(), want.len(), "{f:?} directed={directed} k={k}");
        for (e, (u, v, nodes, hops)) in got.iter().zip(want) {
            if u == v {
                assert_eq!(e.get("edges").arr().len() + e.get("nodes").arr().len(), 0);
                continue;
            }
            total += 1;
            reversed += hops.iter().filter(|h| h.1).count();
            assert_eq!(walk_path(e, false), (u, v, nodes, hops));
        }
    });
    eprintln!("kHopEnumerate: {total} paths, {reversed} reversed edges");
    assert!(total >= 200 && reversed >= 50, "{total} {reversed}");
}

#[test]
fn random_neighbourhoods_and_levels() {
    let (mut nonempty, mut capped) = (0, 0);
    random_cases(|r, f, directed, srcs, _, rng| {
        let k = rng.below(4) as i64;
        let ret = [-1i64, 0, 2, 100][rng.below(4)];
        let c = r.call(f, srcs, &[], directed).k(k).ret(ret);
        let srcs = distinct(srcs);
        // kHopNeighbor: literal reference (order), and the level set.
        let got = entries(&c.json(GFnKind::KHopNeighbor));
        let counts = entries(&c.json(GFnKind::KHopCount));
        let all = entries(&c.json(GFnKind::KHopAllNeighbors));
        let bfs = entries(&c.json(GFnKind::BfsCount));
        assert_eq!(got.len(), srcs.len());
        assert_eq!(all.len(), srcs.len());
        for (i, &s) in srcs.iter().enumerate() {
            let (dist, _) = r.bfs(s, f, directed);
            let ret_eff = if ret < 0 { 100 } else { ret };
            let want = r.k_hop_neighbor(s, directed, k, f, ret_eff);
            let nb: Vec<usize> = got[i]
                .get("neighbor")
                .arr()
                .iter()
                .map(|x| idx(x.str()))
                .collect();
            assert_eq!(nb, want);
            nonempty += usize::from(!nb.is_empty());
            let level: BTreeSet<usize> = (0..r.n)
                .filter(|&y| k > 0 && dist[y] == Some(k as usize))
                .collect();
            if ret_eff >= r.n as i64 {
                assert_eq!(nb.iter().copied().collect::<BTreeSet<_>>(), level);
            }
            assert_eq!(counts[i].get("count").int() as usize, level.len());
            // kHopAllNeighbors: the ball of radius k (all of it when retNum
            // is large), ascending.
            let ball: Vec<usize> = (0..r.n)
                .filter(|&y| dist[y].is_some_and(|d| d as i64 <= k))
                .collect();
            let res: Vec<usize> = all[i]
                .get("results")
                .arr()
                .iter()
                .map(|x| idx(x.str()))
                .collect();
            assert!(res.windows(2).all(|w| w[0] < w[1]));
            assert_eq!(res.len(), ball.len().min(ret_eff as usize));
            capped += usize::from(res.len() < ball.len());
            assert!(res.iter().all(|y| ball.contains(y)));
            // bfsCount: vertices per level.
            let maxd = dist.iter().flatten().max().copied().unwrap_or(0);
            let want: Vec<i64> = (0..=maxd)
                .map(|d| dist.iter().filter(|x| **x == Some(d)).count() as i64)
                .collect();
            let lv: Vec<i64> = bfs[i]
                .get("results")
                .arr()
                .iter()
                .map(|x| x.get("count").int())
                .collect();
            assert_eq!(lv, want);
        }
    });
    eprintln!("kHopNeighbor: {nonempty} non-empty lists; kHopAllNeighbors: {capped} capped");
    assert!(nonempty >= 30 && capped >= 10, "{nonempty} {capped}");
}

#[test]
fn random_sssp() {
    let mut total = 0;
    random_cases(|r, f, directed, srcs, _, _| {
        let c = r.call(f, srcs, &[], directed);
        let got = entries(&c.json(GFnKind::Sssp));
        let lens = entries(&c.json(GFnKind::SsspLen));
        let mut gi = 0;
        for s in distinct(srcs) {
            let (dist, order) = r.bfs(s, f, directed);
            // unordered_map<int, …> iteration order of the discovery order
            let want: Vec<usize> = cxx::unordered_iteration_order(order.iter().map(|&x| x as i64))
                .into_iter()
                .map(|x| x as usize)
                .collect();
            for &t in &want {
                let (src, dst, nodes, hops) = walk_path(&got[gi], true);
                assert_eq!((src, dst), (s, t));
                assert_eq!(hops.len(), dist[t].unwrap());
                assert_eq!(*nodes.last().unwrap(), t);
                for (w, &(p, _)) in nodes.windows(2).zip(&hops) {
                    assert!(
                        r.out[w[0]][p].contains(&w[1])
                            || (!directed && r.out[w[1]][p].contains(&w[0]))
                    );
                }
                assert_eq!(
                    (idx(lens[gi].get("dst").str()), lens[gi].get("length").int()),
                    (t, dist[t].unwrap() as i64)
                );
                gi += 1;
            }
        }
        assert_eq!((gi, gi), (got.len(), lens.len()));
        total += gi;
    });
    eprintln!("SSSP: {total} entries");
    assert!(total >= 500, "{total}");
}

#[test]
fn random_cycles_and_reachability() {
    let (mut cycles, mut trues, mut falses, mut quirks) = (0, 0, 0, 0);
    random_cases(|r, f, directed, srcs, dsts, rng| {
        let c = r.call(f, srcs, dsts, directed);
        let got = entries(&c.json(GFnKind::CyclePath));
        let mut gi = 0;
        let mut any = false;
        for (u, v) in pairs(srcs, dsts) {
            if u == v {
                continue;
            }
            let duv = r.bfs(u, f, directed).0[v];
            let dvu = r.bfs(v, f, directed).0[u];
            let want = match (duv, dvu) {
                (Some(a), Some(b)) if directed => Some(a + b),
                (Some(a), _) if !directed => Some(2 * a),
                _ => None,
            };
            let Some(len) = want else { continue };
            any = true;
            // Directed cycles join two forward paths; undirected ones record
            // every edge with its plain predicate id: all print forward.
            let (s, t, nodes, _) = walk_path(&got[gi], true);
            assert_eq!((s, t), (u, v));
            assert_eq!(nodes.len(), len + 1);
            assert_eq!((nodes[0], *nodes.last().unwrap()), (u, u));
            assert!(nodes.contains(&v));
            gi += 1;
            cycles += 1;
        }
        assert_eq!(gi, got.len());
        assert_eq!(c.eval(GFnKind::CycleBoolean), GFunValue::Bool(any));
        // kHopReachable: the literal reference, pair by pair.
        let k = [-1i64, 0, 1, 2, 3, 12][rng.below(6)];
        let want: Vec<String> = pairs(srcs, dsts)
            .into_iter()
            .map(|(u, v)| {
                let kk = if k < 0 { 999 } else { k };
                let val = r.k_hop_reachable(u, v, directed, kk, f);
                let real = u == v || r.bfs(u, f, directed).0[v].is_some_and(|d| d as i64 <= kk);
                trues += usize::from(val);
                falses += usize::from(!val);
                quirks += usize::from(val != real);
                format!("{{\"src\":\"<n{u}>\",\"dst\":\"<n{v}>\",\"value\":\"{val}\"}}")
            })
            .collect();
        assert_eq!(c.k(k).json(GFnKind::KHopReachable), paths(&want));
    });
    eprintln!(
        "cyclePath: {cycles} cycles; kHopReachable: {trues} true, {falses} false, \
         {quirks} differing from plain k-hop reachability"
    );
    assert!(
        cycles >= 30 && trues >= 30 && falses >= 30,
        "{cycles} {trues} {falses}"
    );
}
