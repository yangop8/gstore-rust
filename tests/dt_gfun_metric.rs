//! Data test: gStore's metric / community / similarity graph functions
//! (`gstore::analytics::gfun::metrics`) with upstream-exact output.
//!
//! Expected values come from tracing the upstream C++ by hand (the trace is
//! summarised next to each assertion; "GE" = GeneralEvaluation.cpp) and
//! were cross-checked against the upstream algorithm sources compiled with
//! g++ 13 -O2 on the same graphs. Values that involve libstdc++ /
//! glibc behaviour (hash-map iteration, `rand()`, Grisu2 digits) or long
//! floating-point iterations are quoted from that build rather than traced.
//! The last section compares several functions with simple reference
//! implementations on random graphs.
//!
//! Graphs are written so that entity ids follow first appearance (`<a>` = 0,
//! `<b>` = 1, …), which makes CSR node order = entity id order = upstream's
//! vertex-id order.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gstore::analytics::csr::{CsrGraph, Node, PredFilter};
use gstore::analytics::gfun::{self, metrics, GArgs, GFnKind, GFunCtx, GFunValue, GNode};
use gstore::error::GStoreError;
use gstore::Database;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

struct G {
    db: Database,
    csr: Arc<CsrGraph>,
}

impl G {
    fn new(nt: &str) -> G {
        let db = Database::build_from_str("gfun-metric", nt).unwrap();
        let csr = db.csr().unwrap();
        G { db, csr }
    }

    fn ctx(&self) -> GFunCtx<'_> {
        GFunCtx::new(&self.csr, self.db.dict())
    }

    fn node(&self, iri: &str) -> GNode {
        gfun::iri_node(&self.csr, self.db.dict(), iri)
    }

    fn nodes(&self, iris: &[&str]) -> Vec<GNode> {
        iris.iter().map(|i| self.node(i)).collect()
    }

    /// `{<p>, <q>}` (`{}` for an empty slice).
    fn preds(&self, include: &[&str]) -> PredFilter {
        PredFilter::resolve(include, &[], self.db.dict())
    }

    /// `{!<p>, …}`.
    fn not_preds(&self, exclude: &[&str]) -> PredFilter {
        PredFilter::resolve(&[], exclude, self.db.dict())
    }

    fn eval(&self, kind: GFnKind, a: &GArgs) -> Result<GFunValue, GStoreError> {
        gfun::eval(kind, &self.ctx(), a)
    }

    fn json(&self, kind: GFnKind, a: &GArgs) -> String {
        match self.eval(kind, a).unwrap() {
            GFunValue::Json(s) | GFunValue::ResultsJson(s) => s,
            other => panic!("{kind}: unexpected value {other:?}"),
        }
    }

    fn name(&self, n: Node) -> String {
        self.db
            .dict()
            .id_to_string(self.csr.entity_of(n))
            .unwrap()
            .to_string()
    }
}

fn args(preds: PredFilter) -> GArgs {
    GArgs {
        preds,
        ..GArgs::default()
    }
}

/// `<a> -p-> <b> -p-> <c>`, plus `<d>` with only a literal (an entity
/// without CSR vertex) and the predicate `<q>` without entity edges.
const PATH: &str = "
<a> <p> <b> .
<b> <p> <c> .
<d> <q> \"lit\" .
";

/// `a→b, a→c, b→d, c→d` over `<p>`, plus the parallel `b -q-> d`.
const DIAMOND: &str = "
<a> <p> <b> .
<a> <p> <c> .
<b> <p> <d> .
<c> <p> <d> .
<b> <q> <d> .
";

/// The directed triangle `a→b→c→a` and the pendant `c→d` over `<p>`, plus
/// the parallel `a -q-> b`.
const TRIANGLE: &str = "
<a> <p> <b> .
<b> <p> <c> .
<c> <p> <a> .
<c> <p> <d> .
<a> <q> <b> .
";

/// Two directed triangles `a→b→c→a`, `d→e→f→d` over `<p>`, bridged by
/// `c -q-> d`.
const TWO_TRIANGLES: &str = "
<a> <p> <b> .
<b> <p> <c> .
<c> <p> <a> .
<d> <p> <e> .
<e> <p> <f> .
<f> <p> <d> .
<c> <q> <d> .
";

/// The 4-clique `a, b, c, d` plus `e→a`.
const CLIQUE: &str = "
<a> <p> <b> .
<a> <p> <c> .
<a> <p> <d> .
<b> <p> <c> .
<b> <p> <d> .
<c> <p> <d> .
<e> <p> <a> .
";

/// The sample social graph of upstream's SPARQL_QUERY.md ("Advanced
/// functions"): Alice 0, Bob 1, Eve 2, Carol 3, Francis 4, Dave 5;
/// predicates focus, like, dislike.
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

// ---------------------------------------------------------------------------
// PR
// ---------------------------------------------------------------------------

fn pr_args(g: &G, directed: bool, preds: &[&str], ret: i64) -> GArgs {
    GArgs {
        directed,
        ret_num: Some(ret),
        alpha: Some(0.85),
        max_iter: Some(100.0),
        tol: Some(0.0001),
        ..args(g.preds(preds))
    }
}

#[test]
fn pagerank() {
    // Directed 3-cycle: every vertex has out-degree 1, so the first sweep
    // gives (1-0.85)/3 + 0.85·(1/3) = 1/3 again → stop; printed with `%g`.
    let g = G::new("<a> <p> <b> .\n<b> <p> <c> .\n<c> <p> <a> .\n");
    assert_eq!(
        g.json(GFnKind::Pr, &pr_args(&g, true, &[], -1)),
        r#"{"paths":[{"src":"<a>","result":0.333333},{"src":"<b>","result":0.333333},{"src":"<c>","result":0.333333}]}"#
    );
    // retNum 2 < n: libstdc++ `std::sort` by value (three equal values stay
    // in order: n < 16 is one insertion sort), then the first two.
    assert_eq!(
        g.json(GFnKind::Pr, &pr_args(&g, true, &[], 2)),
        r#"{"paths":[{"src":"<a>","result":0.333333},{"src":"<b>","result":0.333333}]}"#
    );
    // retNum 0 keeps nothing; retNum > n is clamped to n (all, unsorted).
    assert_eq!(
        g.json(GFnKind::Pr, &pr_args(&g, true, &[], 0)),
        r#"{"paths":[]}"#
    );
    assert_eq!(
        g.json(GFnKind::Pr, &pr_args(&g, true, &[], 99)),
        g.json(GFnKind::Pr, &pr_args(&g, true, &[], -1))
    );

    // PATH: `c` is dangling (its share goes to every vertex), so mass
    // accumulates downstream: a < b < c.
    let g = G::new(PATH);
    assert_eq!(
        g.json(GFnKind::Pr, &pr_args(&g, true, &["<p>", "<q>"], -1)),
        r#"{"paths":[{"src":"<a>","result":0.184403},{"src":"<b>","result":0.341169},{"src":"<c>","result":0.474429}]}"#
    );

    // TRIANGLE, directed over <p>: d only receives from c; sorted top-2.
    let g = G::new(TRIANGLE);
    assert_eq!(
        g.json(GFnKind::Pr, &pr_args(&g, true, &["<p>"], -1)),
        r#"{"paths":[{"src":"<a>","result":0.213758},{"src":"<b>","result":0.264652},{"src":"<c>","result":0.307832},{"src":"<d>","result":0.213758}]}"#
    );
    assert_eq!(
        g.json(GFnKind::Pr, &pr_args(&g, true, &["<p>"], 2)),
        r#"{"paths":[{"src":"<c>","result":0.307832},{"src":"<b>","result":0.264652}]}"#
    );
    // Undirected over {p, q}: the parallel a–b edge counts twice in the
    // degrees (multiplicity per predicate), lifting a and b.
    assert_eq!(
        g.json(GFnKind::Pr, &pr_args(&g, false, &["<p>", "<q>"], -1)),
        r#"{"paths":[{"src":"<a>","result":0.285744},{"src":"<b>","result":0.285744},{"src":"<c>","result":0.3047},{"src":"<d>","result":0.123813}]}"#
    );

    // Upstream parses alpha / tol / maxIter with `stof`; the named
    // arguments are required.
    let mut a = pr_args(&g, true, &[], -1);
    a.alpha = None;
    assert!(g.eval(GFnKind::Pr, &a).is_err());
}

// ---------------------------------------------------------------------------
// triangleCounting
// ---------------------------------------------------------------------------

#[test]
fn triangle_counting() {
    let g = G::new(TRIANGLE);
    let tri = |directed: bool, preds: PredFilter| {
        g.json(
            GFnKind::TriangleCounting,
            &GArgs {
                directed,
                ..args(preds)
            },
        )
    };
    // Directed over <p>: each vertex v counts out-edge pairs v→w→x with x an
    // in-neighbour of v: a (a→b→c, c ∈ in(a)) 1, b 1, c (c→a→b) 1, d 0 →
    // 3 / 3 = 1. Undirected: the 3 triangle vertices each see 2 closing
    // neighbours → 6 / 6 = 1.
    assert_eq!(tri(true, g.preds(&["<p>"])), r#"{"paths":[1]}"#);
    assert_eq!(tri(false, g.preds(&["<p>"])), r#"{"paths":[1]}"#);
    // With the parallel a -q-> b the directed walk counts a→b twice (once
    // per predicate): a 2, b 1, c 2 → 5, not a multiple of 3 → upstream's
    // error value -1. Undirected uses distinct neighbour sets → still 1.
    assert_eq!(tri(true, g.preds(&["<p>", "<q>"])), r#"{"paths":[-1]}"#);
    assert_eq!(tri(false, g.preds(&[])), r#"{"paths":[1]}"#);
    // `{!<q>}` = all predicates but q.
    assert_eq!(tri(true, g.not_preds(&["<q>"])), r#"{"paths":[1]}"#);
    // An empty predicate list (an unknown IRI resolves to nothing) → -1.
    assert_eq!(tri(true, g.preds(&["<nope>"])), r#"{"paths":[-1]}"#);
    // Doc example (SPARQL_QUERY.md): Bob → Eve → Carol → Bob over <like>.
    let s = G::new(SOCIAL);
    assert_eq!(
        s.json(
            GFnKind::TriangleCounting,
            &GArgs {
                directed: true,
                ..args(s.preds(&["<like>"]))
            }
        ),
        r#"{"paths":[1]}"#
    );
    assert_eq!(
        s.json(GFnKind::TriangleCounting, &args(s.preds(&[]))),
        r#"{"paths":[5]}"#
    );
}

// ---------------------------------------------------------------------------
// closenessCentrality
// ---------------------------------------------------------------------------

#[test]
fn closeness_centrality() {
    let g = G::new(PATH);
    let a = GArgs {
        srcs: g.nodes(&["<a>", "<c>", "<nope>", "<d>"]),
        directed: true,
        ..args(g.preds(&["<p>"]))
    };
    // a: BFS reaches b (1), c (2): (3-1)/(1+2) = 0.666667 (`to_string`).
    // c has no out-edges: (1-1)/0 = 0/0, x86-64's negative NaN → "-nan".
    // A missing IRI prints as "" (also -nan); `<d>` exists but has no CSR
    // vertex: an isolated vertex.
    assert_eq!(
        g.json(GFnKind::ClosenessCentrality, &a),
        r#"{"paths":[{"src":"<a>","result":0.666667},{"src":"<c>","result":-nan},{"src":"","result":-nan},{"src":"<d>","result":-nan}]}"#
    );
    // Undirected from b: a and c at distance 1 → 2/2.
    let a = GArgs {
        srcs: g.nodes(&["<b>", "<b>"]),
        ..args(g.preds(&["<p>"]))
    };
    assert_eq!(
        g.json(GFnKind::ClosenessCentrality, &a),
        r#"{"paths":[{"src":"<b>","result":1.000000}]}"#
    );
    // Doc examples: Alice over {like, focus}: undirected 1/1.8, directed 1/2.
    let s = G::new(SOCIAL);
    let mut a = GArgs {
        srcs: s.nodes(&["<Alice>"]),
        ..args(s.preds(&["<like>", "<focus>"]))
    };
    assert_eq!(
        s.json(GFnKind::ClosenessCentrality, &a),
        r#"{"paths":[{"src":"<Alice>","result":0.555556}]}"#
    );
    a.directed = true;
    assert_eq!(
        s.json(GFnKind::ClosenessCentrality, &a),
        r#"{"paths":[{"src":"<Alice>","result":0.500000}]}"#
    );
}

// ---------------------------------------------------------------------------
// betweennessCentrality
// ---------------------------------------------------------------------------

#[test]
fn betweenness_centrality() {
    let g = G::new(DIAMOND);
    let btw = |srcs: &[&str], directed: bool, preds: PredFilter| {
        g.json(
            GFnKind::BetweennessCentrality,
            &GArgs {
                srcs: g.nodes(srcs),
                directed,
                ..args(preds)
            },
        )
    };
    // Directed over <p>, b: V = {a, c, d} (undirected reach). From a, the
    // only target at distance > 1 is d with shortest paths a→b→d, a→c→d:
    // 1 of 2 passes b → 0.5; c and d reach nothing at distance > 1.
    // Output: `ss << double`, ", \"result\":" with a space.
    assert_eq!(
        btw(&["<b>"], true, g.preds(&["<p>"])),
        r#"{"paths":[{"src":"<b>", "result":0.5}]}"#
    );
    // Undirected: from a → d 0.5; from c only b itself is at distance 2
    // (skipped); from d → a via b or c: 0.5; total 1.0, halved → 0.5.
    assert_eq!(
        btw(&["<b>"], false, g.preds(&["<p>"])),
        r#"{"paths":[{"src":"<b>", "result":0.5}]}"#
    );
    // With the parallel b -q-> d a path counts once per predicate on each
    // hop: a→b→d weighs 1·2, a→c→d 1·1 → 2/3; c gets the other 1/3. A
    // missing source prints nothing (GE:4101 `if (uid < 0) return`);
    // consecutive sources are separated by ','.
    assert_eq!(
        btw(&["<b>", "<nope>", "<c>"], true, g.preds(&[])),
        r#"{"paths":[{"src":"<b>", "result":0.666667},{"src":"<c>", "result":0.333333}]}"#
    );
    // An empty predicate list → 0.
    assert_eq!(
        btw(&["<b>"], true, g.preds(&["<nope>"])),
        r#"{"paths":[{"src":"<b>", "result":0}]}"#
    );
    // Doc example: Eve in the social graph. The doc writes predicates its
    // sample data does not use (<likes>, <follows>, <dislikes> resolve to
    // nothing and give 0); over the data's own predicates the value is the
    // doc's 6.5.
    let s = G::new(SOCIAL);
    assert_eq!(
        s.json(
            GFnKind::BetweennessCentrality,
            &GArgs {
                srcs: s.nodes(&["<Eve>"]),
                directed: true,
                ..args(s.preds(&[]))
            }
        ),
        r#"{"paths":[{"src":"<Eve>", "result":6.5}]}"#
    );
}

// ---------------------------------------------------------------------------
// clusterCoeff
// ---------------------------------------------------------------------------

#[test]
fn cluster_coefficient() {
    let g = G::new(TRIANGLE);
    let cc = |srcs: &[&str], directed: bool, preds: PredFilter| {
        g.json(
            GFnKind::ClusterCoeff,
            &GArgs {
                srcs: g.nodes(srcs),
                directed,
                ..args(preds)
            },
        )
    };
    // Local, undirected, c: S = {a, b, d}; links inside S: a–b → 2 / 2 = 1
    // of 3·2/2 possible → 0.333333 (`to_string`).
    assert_eq!(
        cc(&["<c>"], false, g.preds(&["<p>"])),
        r#"{"paths":[{"src":"<c>","result":0.333333}]}"#
    );
    // Local, directed, c: in(c) = {b}; out-edges c→a, c→d and their
    // out-edges a→b (b ∈ in(c)) → 1 of |{a, b, d}|·2 = 6 → 0.166667.
    assert_eq!(
        cc(&["<c>"], true, g.preds(&["<p>"])),
        r#"{"paths":[{"src":"<c>","result":0.166667}]}"#
    );
    // Global (ignores `directed`): closed/open triplets per centre:
    // a 2/1, b 2/1, c 2/0, d 0/2 → (6/2) / (6/2 + 4/2) = 3/5.
    assert_eq!(cc(&[], true, g.preds(&["<p>"])), r#"{"paths":[0.600000]}"#);
    // A missing constant source means "no source" upstream (`uid == -1`):
    // the global value, once, between the local entries.
    assert_eq!(
        cc(&["<c>", "<nope>", "<a>"], false, g.preds(&["<p>"])),
        r#"{"paths":[{"src":"<c>","result":0.333333},0.600000,{"src":"<a>","result":1.000000}]}"#
    );
    // Empty predicate list → -1.
    assert_eq!(
        cc(&["<c>"], false, g.preds(&["<nope>"])),
        r#"{"paths":[{"src":"<c>","result":-1.000000}]}"#
    );
    assert_eq!(
        cc(&[], false, PredFilter::default()),
        r#"{"paths":[-1.000000]}"#
    );
}

// ---------------------------------------------------------------------------
// labelProp / WCC / louvain
// ---------------------------------------------------------------------------

#[test]
fn communities() {
    let g = G::new(TWO_TRIANGLES);
    // labelProp: labels start as vertex ids; vertices adopt the most
    // frequent neighbour label (ties: glibc rand() seeded 114514), so each
    // triangle converges to one label; the bridge (q) only exists in {p, q}.
    let lp = |directed: bool, preds: PredFilter| {
        g.json(
            GFnKind::LabelProp,
            &GArgs {
                directed,
                max_iter: Some(10.0),
                ..args(preds)
            },
        )
    };
    let two = r#"{"paths":[["<a>","<b>","<c>"],["<d>","<e>","<f>"]]}"#;
    assert_eq!(lp(false, g.preds(&[])), two);
    assert_eq!(lp(false, g.preds(&["<p>"])), two);
    assert_eq!(lp(true, g.preds(&["<p>"])), two);
    // maxIter is required.
    assert!(g.eval(GFnKind::LabelProp, &args(g.preds(&[]))).is_err());

    // WCC: BFS from each unassigned vertex (per predicate: in, then out);
    // members follow the iteration order of one global
    // `unordered_map<int,int>`: keys 0..5 inserted in discovery order land
    // in their own buckets and come out in reverse insertion order.
    // Over <p>: insertion 0, 2 (in-edge c→a first), 1, then 3, 5, 4 →
    // iteration 4, 5, 3, 1, 2, 0, split by component.
    assert_eq!(
        g.json(GFnKind::Wcc, &args(g.preds(&["<p>"]))),
        r#"{"paths":[["<b>","<c>","<a>"],["<e>","<f>","<d>"]]}"#
    );
    // With the bridge: one component, insertion 0, 2, 1, 3, 5, 4 (d found
    // from c over q after c's p-edges) → iteration order e, f, d, b, c, a.
    assert_eq!(
        g.json(GFnKind::Wcc, &args(g.preds(&[]))),
        r#"{"paths":[["<e>","<f>","<d>","<b>","<c>","<a>"]]}"#
    );
    assert_eq!(
        g.json(GFnKind::Wcc, &args(g.preds(&["<nope>"]))),
        r#"{"paths":[["<a>"],["<b>"],["<c>"],["<d>"],["<e>"],["<f>"]]}"#
    );

    // louvain: the two triangles are the communities whether or not the
    // bridge is included; members print as entity ids.
    let lv = |directed: bool, preds: PredFilter| {
        g.json(
            GFnKind::Louvain,
            &GArgs {
                directed,
                max_iter: Some(10.0),
                increase: Some(0.0),
                ..args(preds)
            },
        )
    };
    let want = r#"{"paths":[{"count":2, "community":[[0,1,2],[3,4,5]]}]}"#;
    assert_eq!(lv(false, g.preds(&[])), want);
    assert_eq!(lv(true, g.preds(&[])), want);
    assert_eq!(lv(false, g.preds(&["<p>"])), want);
    // Nothing to cluster without edges.
    assert_eq!(
        lv(false, g.preds(&["<nope>"])),
        r#"{"paths":[{"count":0, "community":[]}]}"#
    );
}

// ---------------------------------------------------------------------------
// JaccardSimilarity / degreeCorrelation
// ---------------------------------------------------------------------------

#[test]
fn jaccard_similarity() {
    // a→b, a→c, d→b, d→c.
    let g = G::new("<a> <p> <b> .\n<a> <p> <c> .\n<d> <p> <b> .\n<d> <p> <c> .\n");
    let jac = |srcs: &[&str], k: i64, r: i64| {
        g.json(
            GFnKind::JaccardSimilarity,
            &GArgs {
                srcs: g.nodes(srcs),
                k: Some(k),
                ret_num: Some(r),
                ..args(g.preds(&[]))
            },
        )
    };
    // N(a) = {b, c}. b: N(b) = {a, d}; total 2+2-1 (b ∈ N(a)) - 1 (a ∈ N(b))
    // = 2, common 0 → 0; same for c, inserted before the equal b; d (2-hop,
    // collected in no_used, k > 1): N(d) = {b, c}, total 4, common 2 →
    // 2/(4-2) = 1, inserted first. IRIs are not quoted (upstream builder).
    assert_eq!(
        jac(&["<a>"], 2, 10),
        r#"{"paths":[{"dst":<d>, "value":1},{"dst":<c>, "value":0},{"dst":<b>, "value":0}]}"#
    );
    // k = 1: no 2-hop candidates; kHopNeighbor(a, 1 hop) adds nothing new.
    assert_eq!(
        jac(&["<a>"], 1, 10),
        r#"{"paths":[{"dst":<c>, "value":0},{"dst":<b>, "value":0}]}"#
    );
    assert_eq!(jac(&["<a>"], 2, 1), r#"{"paths":[{"dst":<d>, "value":1}]}"#);
    // Defaults k < 0 → 999, retNum < 0 → 10; two sources joined by ','; a
    // missing source prints nothing.
    assert_eq!(
        jac(&["<b>", "<nope>", "<a>"], -1, -1),
        r#"{"paths":[{"dst":<c>, "value":1},{"dst":<d>, "value":0},{"dst":<a>, "value":0},{"dst":<d>, "value":1},{"dst":<c>, "value":0},{"dst":<b>, "value":0}]}"#
    );
    // Doc example (SPARQL_QUERY.md).
    let s = G::new(SOCIAL);
    assert_eq!(
        s.json(
            GFnKind::JaccardSimilarity,
            &GArgs {
                srcs: s.nodes(&["<Alice>"]),
                k: Some(2),
                ret_num: Some(10),
                ..args(s.preds(&[]))
            }
        ),
        r#"{"paths":[{"dst":<Francis>, "value":0.5},{"dst":<Carol>, "value":0.5},{"dst":<Eve>, "value":0.5},{"dst":<Dave>, "value":0.333333},{"dst":<Bob>, "value":0.333333}]}"#
    );
}

#[test]
fn degree_correlation() {
    let g = G::new(PATH);
    let dc = |src: &str, k: i64, preds: PredFilter| {
        g.json(
            GFnKind::DegreeCorrelation,
            &GArgs {
                srcs: g.nodes(&[src]),
                k: Some(k),
                ..args(preds)
            },
        )
    };
    // Path a→b→c: degrees (out + in) a 1, b 2, c 1; edge pairs (a,b) =
    // (1,2), (b,c) = (2,1) → Pearson = -0.5 / (√0.5·√0.5) = -1 (printed
    // `%g`; √0.5² rounds to 0.5000000000000001, the ratio prints as -1).
    assert_eq!(
        dc("<a>", -1, g.preds(&["<p>"])),
        r#"{"paths":[{"src":"<a>", "result":-1}]}"#
    );
    // k = 1 from b still reaches a and c.
    assert_eq!(
        dc("<b>", 1, g.preds(&[])),
        r#"{"paths":[{"src":"<b>", "result":-1}]}"#
    );
    // k = 0: only the source is visited → 0; an empty predicate list → 0;
    // an isolated source → 0; a missing one prints nothing.
    assert_eq!(
        dc("<a>", 0, g.preds(&["<p>"])),
        r#"{"paths":[{"src":"<a>", "result":0}]}"#
    );
    assert_eq!(
        dc("<a>", -1, g.preds(&["<nope>"])),
        r#"{"paths":[{"src":"<a>", "result":0}]}"#
    );
    assert_eq!(
        dc("<d>", -1, g.preds(&[])),
        r#"{"paths":[{"src":"<d>", "result":0}]}"#
    );
    assert_eq!(dc("<nope>", -1, g.preds(&[])), r#"{"paths":[]}"#);
    // Doc example (SPARQL_QUERY.md).
    let s = G::new(SOCIAL);
    assert_eq!(
        s.json(
            GFnKind::DegreeCorrelation,
            &GArgs {
                srcs: s.nodes(&["<Alice>"]),
                k: Some(-1),
                ..args(s.preds(&[]))
            }
        ),
        r#"{"paths":[{"src":"<Alice>", "result":0.0429273}]}"#
    );
}

// ---------------------------------------------------------------------------
// diameterEstimation / confidenceDegree
// ---------------------------------------------------------------------------

#[test]
fn diameter_estimation() {
    // Directed BFS from every vertex with an out-edge; the longest distance.
    let g = G::new(PATH);
    assert_eq!(
        g.json(GFnKind::DiameterEstimation, &args(g.preds(&[]))),
        r#"{"paths":[2]}"#
    );
    assert_eq!(
        g.json(GFnKind::DiameterEstimation, &args(g.preds(&["<q>"]))),
        r#"{"paths":[0]}"#
    );
    let g = G::new(DIAMOND);
    assert_eq!(
        g.json(GFnKind::DiameterEstimation, &args(g.preds(&[]))),
        r#"{"paths":[2]}"#
    );
    let s = G::new(SOCIAL);
    assert_eq!(
        s.json(GFnKind::DiameterEstimation, &args(s.preds(&[]))),
        r#"{"paths":[4]}"#
    );
    assert_eq!(
        s.json(GFnKind::DiameterEstimation, &args(s.preds(&["<like>"]))),
        r#"{"paths":[3]}"#
    );
}

#[test]
fn confidence_degree() {
    // Undirected BFS over all predicates from every vertex:
    // a: (3-1)/(1+2), b: 2/2, c: 2/3, printed by nlohmann::json::dump.
    let g = G::new(PATH);
    let a = args(PredFilter::all(g.db.dict()));
    assert_eq!(
        g.eval(GFnKind::ConfidenceDegree, &a).unwrap(),
        GFunValue::ResultsJson(
            r#"[["<a>",0.6666666666666666],["<b>",1.0],["<c>",0.6666666666666666]]"#.into()
        )
    );
    let s = G::new(SOCIAL);
    assert_eq!(
        s.json(
            GFnKind::ConfidenceDegree,
            &args(PredFilter::all(s.db.dict()))
        ),
        r#"[["<Alice>",0.7142857142857143],["<Bob>",0.7142857142857143],["<Eve>",1.0],["<Carol>",0.7142857142857143],["<Francis>",0.7142857142857143],["<Dave>",0.7142857142857143]]"#
    );
    // Without vertices nlohmann dumps the never-assigned value.
    let e = G::new("<a> <q> \"x\" .\n");
    assert_eq!(
        e.json(
            GFnKind::ConfidenceDegree,
            &args(PredFilter::all(e.db.dict()))
        ),
        "null"
    );
    // nlohmann's Grisu2 is not always the shortest representation.
    assert_eq!(
        metrics::nlohmann_dump_f64(35.0 / 127.0),
        "0.27559055118110237"
    );
}

// ---------------------------------------------------------------------------
// maximumKplex / coreTruss
// ---------------------------------------------------------------------------

#[test]
fn maximum_kplex() {
    let kplex = |g: &G, k: i64, preds: PredFilter| {
        g.json(
            GFnKind::MaximumKplex,
            &GArgs {
                k: Some(k),
                ..args(preds)
            },
        )
    };
    // The 4-clique is the maximum 1-plex (clique) and 2-plex; IRIs unquoted.
    let g = G::new(CLIQUE);
    assert_eq!(
        kplex(&g, 1, g.preds(&[])),
        r#"{"paths":[{"kplex":[<a>,<b>,<c>,<d>]}]}"#
    );
    assert_eq!(
        kplex(&g, 2, g.preds(&[])),
        r#"{"paths":[{"kplex":[<a>,<b>,<c>,<d>]}]}"#
    );
    // k = 3: the search starts from a fake solution of 2k-2 = 4 copies of
    // vertex 0 and only replaces it by something larger; the best 3-plex
    // has 4 vertices (e has one neighbour) → the fake is printed.
    assert_eq!(
        kplex(&g, 3, g.preds(&[])),
        r#"{"paths":[{"kplex":[<a>,<a>,<a>,<a>]}]}"#
    );
    // A single edge: the 1-plex {a, b} (degeneracy tail order b, a), and
    // the fake [0, 0] for k = 2.
    let p = G::new("<a> <p> <b> .\n");
    assert_eq!(
        kplex(&p, 1, p.preds(&[])),
        r#"{"paths":[{"kplex":[<b>,<a>]}]}"#
    );
    assert_eq!(
        kplex(&p, 2, p.preds(&[])),
        r#"{"paths":[{"kplex":[<a>,<a>]}]}"#
    );
    // Two triangles: the max-degree heuristic finds {d, e, f} (ties go to
    // the vertex pushed last into the degree list).
    let t = G::new(TWO_TRIANGLES);
    assert_eq!(
        kplex(&t, 1, t.preds(&[])),
        r#"{"paths":[{"kplex":[<d>,<e>,<f>]}]}"#
    );
    // k defaults to 1; k = 0 throws upstream (`resize(-2)`).
    assert_eq!(
        g.json(GFnKind::MaximumKplex, &args(g.preds(&[]))),
        kplex(&g, 1, g.preds(&[]))
    );
    assert!(g
        .eval(
            GFnKind::MaximumKplex,
            &GArgs {
                k: Some(0),
                ..args(g.preds(&[]))
            }
        )
        .is_err());
    // Social graph, all predicates (simple undirected view).
    let s = G::new(SOCIAL);
    assert_eq!(
        kplex(&s, 1, s.preds(&[])),
        r#"{"paths":[{"kplex":[<Eve>,<Alice>,<Bob>]}]}"#
    );
    assert_eq!(
        kplex(&s, 2, s.preds(&[])),
        r#"{"paths":[{"kplex":[<Carol>,<Eve>,<Bob>,<Alice>]}]}"#
    );
    assert_eq!(
        kplex(&s, 3, s.preds(&[])),
        r#"{"paths":[{"kplex":[<Dave>,<Francis>,<Carol>,<Eve>,<Bob>,<Alice>]}]}"#
    );
}

#[test]
fn core_truss() {
    let ct = |g: &G, alpha: f64, beta: f64, preds: PredFilter| {
        g.json(
            GFnKind::CoreTruss,
            &GArgs {
                nums: vec![alpha, beta],
                ..args(preds)
            },
        )
    };
    // 4-clique + pendant e: the 3-core drops e; beta = 2 keeps every clique
    // edge (each is in 2 triangles). Remaining vertices come out of the
    // linear heap (ascending degree, last touched first).
    let g = G::new(CLIQUE);
    assert_eq!(
        ct(&g, 3.0, 0.0, g.preds(&[])),
        r#"{"paths":[{"coreTruss":[<a>,<b>,<c>,<d>]}]}"#
    );
    assert_eq!(
        ct(&g, 3.0, 2.0, g.preds(&[])),
        r#"{"paths":[{"coreTruss":[<a>,<b>,<c>,<d>]}]}"#
    );
    assert_eq!(
        ct(&g, 1.0, 0.0, g.preds(&[])),
        r#"{"paths":[{"coreTruss":[<e>,<b>,<c>,<d>,<a>]}]}"#
    );
    assert_eq!(
        ct(&g, 4.0, 0.0, g.preds(&[])),
        r#"{"paths":[{"coreTruss":[]}]}"#
    );
    // Two triangles + bridge, alpha 2 / beta 1: the bridge c–d is in no
    // triangle and is peeled; c and d keep degree 2. Upstream then pops n
    // times from a heap that lost nothing here.
    let t = G::new(TWO_TRIANGLES);
    assert_eq!(
        ct(&t, 2.0, 1.0, t.preds(&[])),
        r#"{"paths":[{"coreTruss":[<c>,<d>,<a>,<b>,<e>,<f>]}]}"#
    );
    assert_eq!(
        ct(&t, 2.0, 0.0, t.preds(&[])),
        r#"{"paths":[{"coreTruss":[<a>,<b>,<e>,<f>,<c>,<d>]}]}"#
    );
    // A graph without edges in the predicate set: upstream asserts (and
    // aborts); the port answers with the empty set.
    assert_eq!(
        ct(&t, 0.0, 0.0, t.preds(&["<nope>"])),
        r#"{"paths":[{"coreTruss":[]}]}"#
    );
    // Path a–b–c with alpha 1: heap order a, c, b.
    let p = G::new(PATH);
    assert_eq!(
        ct(&p, 1.0, 0.0, p.preds(&[])),
        r#"{"paths":[{"coreTruss":[<a>,<c>,<b>]}]}"#
    );
    // Triangle a, b, c and the square d–e–f–g: with beta = 1 the square's
    // edges (in no triangle) go, its vertices drop below alpha = 2 and are
    // peeled. `peel` still pops n = 7 times: the 4 failed pops read the
    // uninitialised `u`, which g++ -O2 turns into 0, and vertex 0 (a, still
    // with edges) is appended each time.
    let q = G::new(
        "<a> <p> <b> .\n<b> <p> <c> .\n<c> <p> <a> .\n\
         <d> <p> <e> .\n<e> <p> <f> .\n<f> <p> <g> .\n<g> <p> <d> .\n",
    );
    assert_eq!(
        ct(&q, 2.0, 1.0, q.preds(&[])),
        r#"{"paths":[{"coreTruss":[<a>,<b>,<c>,<a>,<a>,<a>,<a>]}]}"#
    );
    assert_eq!(
        ct(&q, 2.0, 0.0, q.preds(&[])),
        r#"{"paths":[{"coreTruss":[<a>,<b>,<c>,<d>,<e>,<f>,<g>]}]}"#
    );
    let s = G::new(SOCIAL);
    assert_eq!(
        ct(&s, 2.0, 0.0, s.preds(&[])),
        r#"{"paths":[{"coreTruss":[<Alice>,<Bob>,<Carol>,<Francis>,<Dave>,<Eve>]}]}"#
    );
    // Dave has no <like> edge: degree 0, not printed.
    assert_eq!(
        ct(&s, 0.0, 0.0, s.preds(&["<like>"])),
        r#"{"paths":[{"coreTruss":[<Alice>,<Francis>,<Eve>,<Carol>,<Bob>]}]}"#
    );
    assert!(t
        .eval(
            GFnKind::CoreTruss,
            &GArgs {
                nums: vec![-1.0, 0.0],
                ..args(t.preds(&[]))
            }
        )
        .is_err());
}

// ---------------------------------------------------------------------------
// PPR
// ---------------------------------------------------------------------------

#[test]
fn personalized_pagerank() {
    let ppr = |g: &G, srcs: &[&str], hops: i64, ret: i64| {
        g.json(
            GFnKind::Ppr,
            &GArgs {
                srcs: g.nodes(srcs),
                k: Some(hops),
                ret_num: Some(ret),
                ..args(g.preds(&[]))
            },
        )
    };
    // Fewer than 4 vertices: `while (delta >= 1/n)` with delta = 1/4 never
    // runs, v2ppr stays empty and the result is the default slot
    // (vertex 0, 0) — for every source.
    let p = G::new("<a> <p> <b> .\n");
    assert_eq!(
        ppr(&p, &["<b>"], -1, 5),
        r#"{"paths":[{"src":"<b>","results":[{"dst":"<a>","PPR":0}]}]}"#
    );
    let g = G::new(DIAMOND);
    // d has no out-edges: all mass stays on d.
    assert_eq!(
        ppr(&g, &["<d>"], -1, 3),
        r#"{"paths":[{"src":"<d>","results":[{"dst":"<d>","PPR":1}]}]}"#
    );
    // Forward push from a plus random walks (glibc rand() seeded with
    // PPR_SEED on every call; upstream seeds with time(NULL)).
    assert_eq!(
        ppr(&g, &["<a>"], -1, 3),
        r#"{"paths":[{"src":"<a>","results":[{"dst":"<a>","PPR":0.398852},{"dst":"<d>","PPR":0.255813},{"dst":"<b>","PPR":0.186939}]}]}"#
    );
    // hopCnt bounds the push levels.
    assert_eq!(
        ppr(&g, &["<a>"], 0, 4),
        r#"{"paths":[{"src":"<a>","results":[{"dst":"<d>","PPR":0.340984},{"dst":"<c>","PPR":0.242623},{"dst":"<b>","PPR":0.216393},{"dst":"<a>","PPR":0.2}]}]}"#
    );
    assert_eq!(
        ppr(&g, &["<a>"], 1, 4),
        r#"{"paths":[{"src":"<a>","results":[{"dst":"<d>","PPR":0.64},{"dst":"<a>","PPR":0.2},{"dst":"<c>","PPR":0.08},{"dst":"<b>","PPR":0.08}]}]}"#
    );
    // retNum 0 → no results; a missing source prints nothing; negative
    // retNum is an error (upstream: length_error).
    assert_eq!(
        ppr(&g, &["<a>"], -1, 0),
        r#"{"paths":[{"src":"<a>","results":[]}]}"#
    );
    assert_eq!(ppr(&g, &["<nope>"], -1, 3), r#"{"paths":[]}"#);
    assert!(g
        .eval(
            GFnKind::Ppr,
            &GArgs {
                srcs: g.nodes(&["<a>"]),
                k: Some(-1),
                ret_num: Some(-1),
                ..args(g.preds(&[]))
            }
        )
        .is_err());
    // Deterministic: the same call twice gives the same text.
    assert_eq!(ppr(&g, &["<a>"], -1, 3), ppr(&g, &["<a>"], -1, 3));
    // Several sources (the second one sees `kth_ppr`'s static buffer as the
    // first left it).
    let s = G::new(SOCIAL);
    assert_eq!(
        ppr(&s, &["<Alice>", "<Francis>"], -1, 10),
        r#"{"paths":[{"src":"<Alice>","results":[{"dst":"<Alice>","PPR":0.301205},{"dst":"<Bob>","PPR":0.263309},{"dst":"<Eve>","PPR":0.197658},{"dst":"<Carol>","PPR":0.181169},{"dst":"<Francis>","PPR":0.0442897},{"dst":"<Dave>","PPR":0.0123699}]},{"src":"<Francis>","results":[{"dst":"<Francis>","PPR":0.271995},{"dst":"<Carol>","PPR":0.232286},{"dst":"<Eve>","PPR":0.182508},{"dst":"<Bob>","PPR":0.161556},{"dst":"<Alice>","PPR":0.077394},{"dst":"<Dave>","PPR":0.0742609}]}]}"#
    );
}

// ---------------------------------------------------------------------------
// misc: cancellation, Jaro
// ---------------------------------------------------------------------------

#[test]
fn cancellation_is_honoured() {
    let s = G::new(SOCIAL);
    let flag = AtomicBool::new(true);
    let ctx = s.ctx().with_cancel(&flag);
    for kind in [
        GFnKind::Pr,
        GFnKind::MaximumKplex,
        GFnKind::CoreTruss,
        GFnKind::Wcc,
        GFnKind::Ppr,
    ] {
        let a = GArgs {
            srcs: s.nodes(&["<Alice>"]),
            ret_num: Some(3),
            alpha: Some(0.85),
            max_iter: Some(10.0),
            tol: Some(0.001),
            nums: vec![1.0, 0.0],
            ..args(s.preds(&[]))
        };
        assert!(
            matches!(gfun::eval(kind, &ctx, &a), Err(GStoreError::Cancelled)),
            "{kind}"
        );
    }
}

#[test]
fn jaro_similarity_upstream_variant() {
    // TempResult::doSimilarity: byte-wise, f32, containment shortcut.
    assert_eq!(metrics::jaro_similarity("abc", "abc"), 1.0);
    assert_eq!(
        metrics::jaro_similarity("abcdef", "bcd"),
        f64::from(2.5f32 / 3.0)
    );
    assert_eq!(metrics::jaro_similarity("ab", "ba"), 0.0);
    assert_eq!(metrics::jaro_similarity("", ""), 1.0);
    assert_eq!(metrics::jaro_similarity("x", ""), 0.0);
    // (6/6 + 6/6 + (6 - 1)/6) / 3 in f32, the textbook value 0.9444….
    assert_eq!(
        metrics::jaro_similarity("MARTHA", "MARHTA"),
        f64::from((1.0f32 + 1.0 + 5.0 / 6.0) / 3.0)
    );
}

// ---------------------------------------------------------------------------
// randomized consistency checks against reference implementations
// ---------------------------------------------------------------------------

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// A random simple-ish graph over `<v0>…`: no self-loops, `npred`
/// predicates (parallel edges across predicates allowed).
struct RandGraph {
    g: G,
    /// (subject, predicate index, object) by CSR node.
    edges: Vec<(Node, usize, Node)>,
    n: usize,
}

fn rand_graph(seed: u64, max_n: u64, npred: u64) -> RandGraph {
    let mut rng = Lcg(seed);
    let n = 2 + rng.below(max_n - 1);
    let m = 1 + rng.below(n * 2 + 1);
    let mut nt = String::new();
    for _ in 0..m {
        let a = rng.below(n);
        let mut b = rng.below(n);
        if a == b {
            b = (b + 1) % n;
        }
        let p = rng.below(npred);
        nt.push_str(&format!("<v{a}> <p{p}> <v{b}> .\n"));
    }
    let g = G::new(&nt);
    let mut edges = Vec::new();
    for v in 0..g.csr.num_nodes() as Node {
        let (ps, ns) = g.csr.out_edges(v);
        for (&p, &o) in ps.iter().zip(ns) {
            let name = g.db.dict().predicate_to_string(p).unwrap();
            let idx: usize = name
                .trim_start_matches("<p")
                .trim_end_matches('>')
                .parse()
                .unwrap();
            edges.push((v, idx, o));
        }
    }
    let n = g.csr.num_nodes();
    RandGraph { g, edges, n }
}

impl RandGraph {
    fn adj(&self, directed: bool) -> Vec<BTreeSet<Node>> {
        let mut adj = vec![BTreeSet::new(); self.n];
        for &(a, _, b) in &self.edges {
            adj[a as usize].insert(b);
            if !directed {
                adj[b as usize].insert(a);
            }
        }
        adj
    }

    fn dist(&self, src: Node, directed: bool) -> Vec<Option<u32>> {
        let adj = self.adj(directed);
        let mut d = vec![None; self.n];
        d[src as usize] = Some(0);
        let mut q = VecDeque::from([src]);
        while let Some(v) = q.pop_front() {
            let dv = d[v as usize].unwrap();
            for &w in &adj[v as usize] {
                if d[w as usize].is_none() {
                    d[w as usize] = Some(dv + 1);
                    q.push_back(w);
                }
            }
        }
        d
    }
}

/// The numbers following `key` in a JSON text.
fn numbers_after(s: &str, key: &str) -> Vec<f64> {
    s.split(key)
        .skip(1)
        .map(|t| {
            let end = t.find(['}', ',', ']']).unwrap_or(t.len());
            t[..end].trim().parse::<f64>().unwrap()
        })
        .collect()
}

#[test]
fn random_triangles_wcc_closeness_diameter() {
    for seed in 0..60u64 {
        let r = rand_graph(seed, 12, 2);
        let g = &r.g;
        let all = g.preds(&[]);
        // triangleCounting, undirected: unordered triples pairwise adjacent.
        let und = r.adj(false);
        let mut tri = 0;
        for a in 0..r.n {
            for b in a + 1..r.n {
                for c in b + 1..r.n {
                    let (a, b, c) = (a as Node, b as Node, c as Node);
                    if und[a as usize].contains(&b)
                        && und[b as usize].contains(&c)
                        && und[a as usize].contains(&c)
                    {
                        tri += 1;
                    }
                }
            }
        }
        assert_eq!(
            g.json(GFnKind::TriangleCounting, &args(all.clone())),
            format!("{{\"paths\":[{tri}]}}"),
            "seed {seed}"
        );
        // triangleCounting, directed, one predicate (no multiplicity):
        // directed 3-cycles.
        let p0: Vec<_> = r.edges.iter().filter(|e| e.1 == 0).collect();
        if !p0.is_empty() {
            let has = |a: Node, b: Node| p0.iter().any(|e| e.0 == a && e.2 == b);
            let mut cyc = 0;
            for a in 0..r.n as Node {
                for b in 0..r.n as Node {
                    for c in 0..r.n as Node {
                        if a != b && b != c && a != c && has(a, b) && has(b, c) && has(c, a) {
                            cyc += 1;
                        }
                    }
                }
            }
            assert_eq!(
                g.json(
                    GFnKind::TriangleCounting,
                    &GArgs {
                        directed: true,
                        ..args(g.preds(&["<p0>"]))
                    }
                ),
                format!("{{\"paths\":[{}]}}", cyc / 3),
                "seed {seed}"
            );
        }
        // WCC: same partition as union-find (every CSR vertex appears).
        let mut parent: Vec<usize> = (0..r.n).collect();
        fn find(p: &mut Vec<usize>, x: usize) -> usize {
            if p[x] != x {
                let r = find(p, p[x]);
                p[x] = r;
            }
            p[x]
        }
        for &(a, _, b) in &r.edges {
            let (x, y) = (find(&mut parent, a as usize), find(&mut parent, b as usize));
            parent[x] = y;
        }
        let mut groups: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        for v in 0..r.n {
            let root = find(&mut parent, v);
            groups
                .entry(root)
                .or_default()
                .insert(format!("\"{}\"", g.name(v as Node)));
        }
        let want: BTreeSet<BTreeSet<String>> = groups.into_values().collect();
        let out = g.json(GFnKind::Wcc, &args(all.clone()));
        let inner = &out["{\"paths\":[".len()..out.len() - 2];
        let got: BTreeSet<BTreeSet<String>> = inner
            .split("],[")
            .map(|c| {
                c.trim_matches(|ch| ch == '[' || ch == ']')
                    .split(',')
                    .map(str::to_string)
                    .collect()
            })
            .collect();
        assert_eq!(got, want, "seed {seed}");
        // closenessCentrality (both directions) and diameterEstimation.
        let mut diameter = 0;
        for v in 0..r.n as Node {
            for directed in [false, true] {
                let d = r.dist(v, directed);
                let reached: Vec<u32> = d.iter().flatten().copied().collect();
                let (cnt, sum) = (reached.len() as f64, reached.iter().sum::<u32>() as f64);
                let x = (cnt - 1.0) / sum;
                let shown = if x.is_nan() {
                    "-nan".to_string()
                } else {
                    format!("{x:.6}")
                };
                assert_eq!(
                    g.json(
                        GFnKind::ClosenessCentrality,
                        &GArgs {
                            srcs: vec![GNode::from_node(&g.csr, v)],
                            directed,
                            ..args(all.clone())
                        }
                    ),
                    format!(
                        "{{\"paths\":[{{\"src\":\"{}\",\"result\":{shown}}}]}}",
                        g.name(v)
                    ),
                    "seed {seed} v {v}"
                );
                if directed {
                    diameter = diameter.max(reached.into_iter().max().unwrap_or(0));
                }
            }
        }
        assert_eq!(
            g.json(GFnKind::DiameterEstimation, &args(all.clone())),
            format!("{{\"paths\":[{diameter}]}}"),
            "seed {seed}"
        );
    }
}

#[test]
fn random_cluster_coeff_confidence_pagerank() {
    for seed in 100..150u64 {
        let r = rand_graph(seed, 14, 2);
        let g = &r.g;
        let all = g.preds(&[]);
        let und = r.adj(false);
        // Global clustering coefficient: closed / open triplets around each
        // centre, both halved (integer division).
        let (mut closed, mut open) = (0i64, 0i64);
        for c in 0..r.n {
            let s = &und[c];
            for &w in s {
                for &x in &und[w as usize] {
                    if s.contains(&x) {
                        closed += 1;
                    } else if x as usize != c {
                        open += 1;
                    }
                }
            }
        }
        let (closed, open) = (closed / 2, open / 2);
        let gcc = if closed + open == 0 {
            0.0
        } else {
            closed as f64 / (closed + open) as f64
        };
        assert_eq!(
            g.json(GFnKind::ClusterCoeff, &args(all.clone())),
            format!("{{\"paths\":[{gcc:.6}]}}"),
            "seed {seed}"
        );
        // confidenceDegree: undirected closeness per vertex, NaN → 0.
        let out = g.json(
            GFnKind::ConfidenceDegree,
            &args(PredFilter::all(g.db.dict())),
        );
        let got: Vec<f64> = out
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split("],[")
            .map(|e| e.rsplit(',').next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(got.len(), r.n, "seed {seed}");
        for (v, &value) in got.iter().enumerate() {
            let reached: Vec<u32> = r.dist(v as Node, false).into_iter().flatten().collect();
            let x = (reached.len() as f64 - 1.0) / reached.iter().sum::<u32>() as f64;
            let x = if x.is_nan() { 0.0 } else { x };
            assert_eq!(value, x, "seed {seed} v {v}");
        }
        // PR: a direct power iteration (same update rule, own summation).
        for directed in [true, false] {
            let n = r.n;
            let alpha = f64::from(0.85f32);
            let tol = f64::from(0.0001f32);
            let mut deg = vec![0usize; n];
            for &(a, _, b) in &r.edges {
                deg[a as usize] += 1;
                if !directed {
                    deg[b as usize] += 1;
                }
            }
            let mut old = vec![1.0 / n as f64; n];
            for _ in 0..=100 {
                let mut new = vec![(1.0 - alpha) / n as f64; n];
                for v in 0..n {
                    if deg[v] == 0 {
                        new.iter_mut().for_each(|x| *x += alpha * old[v] / n as f64);
                    }
                }
                for &(a, _, b) in &r.edges {
                    new[b as usize] += alpha * old[a as usize] / deg[a as usize] as f64;
                    if !directed {
                        new[a as usize] += alpha * old[b as usize] / deg[b as usize] as f64;
                    }
                }
                let done = new.iter().zip(&old).all(|(x, y)| (x - y).abs() <= tol);
                old = new;
                if done {
                    break;
                }
            }
            let got = numbers_after(
                &g.json(
                    GFnKind::Pr,
                    &GArgs {
                        directed,
                        ret_num: Some(-1),
                        alpha: Some(0.85),
                        max_iter: Some(100.0),
                        tol: Some(0.0001),
                        ..args(all.clone())
                    },
                ),
                "\"result\":",
            );
            assert_eq!(got.len(), n);
            for v in 0..n {
                assert!(
                    (got[v] - old[v]).abs() <= 1e-5 * old[v].max(1e-3),
                    "seed {seed} v {v}: {} vs {}",
                    got[v],
                    old[v]
                );
            }
        }
    }
}

#[test]
fn random_betweenness_against_path_enumeration() {
    for seed in 200..240u64 {
        let r = rand_graph(seed, 9, 2);
        let g = &r.g;
        let all = g.preds(&[]);
        for directed in [true, false] {
            // Hop weight: number of predicates joining the two vertices in
            // the traversal direction (upstream's pred_l set size).
            let w = |a: Node, b: Node| -> usize {
                let mut ps = BTreeSet::new();
                for &(s, p, o) in &r.edges {
                    if (s == a && o == b) || (!directed && s == b && o == a) {
                        ps.insert(p);
                    }
                }
                ps.len()
            };
            for x in 0..r.n as Node {
                // Reference: Σ over s ≠ x reachable from x (undirected) of
                // Σ over targets t (dist > 1, t ∉ {s, x}) of
                // weighted shortest s→t paths through x / all of them.
                let reach: BTreeSet<Node> = r
                    .dist(x, false)
                    .iter()
                    .enumerate()
                    .filter_map(|(v, d)| d.map(|_| v as Node))
                    .filter(|&v| v != x)
                    .collect();
                let mut want = 0.0;
                if reach.len() > 1 {
                    for &s in &reach {
                        let d = r.dist(s, directed);
                        for t in 0..r.n as Node {
                            let Some(dt) = d[t as usize] else { continue };
                            if dt <= 1 || t == s || t == x {
                                continue;
                            }
                            // enumerate shortest paths s → t
                            let (mut total, mut pass) = (0i64, 0i64);
                            let mut stack = vec![(s, vec![s])];
                            while let Some((v, path)) = stack.pop() {
                                if v == t {
                                    let mut weight = 1i64;
                                    for h in path.windows(2) {
                                        weight *= w(h[0], h[1]) as i64;
                                    }
                                    total += weight;
                                    if path.contains(&x) {
                                        pass += weight;
                                    }
                                    continue;
                                }
                                let dv = d[v as usize].unwrap();
                                for u in 0..r.n as Node {
                                    if d[u as usize] == Some(dv + 1) && w(v, u) > 0 {
                                        let mut p2 = path.clone();
                                        p2.push(u);
                                        stack.push((u, p2));
                                    }
                                }
                            }
                            if pass != 0 && total != 0 {
                                want += pass as f64 / total as f64;
                            }
                        }
                    }
                    if !directed {
                        want /= 2.0;
                    }
                }
                let got = numbers_after(
                    &g.json(
                        GFnKind::BetweennessCentrality,
                        &GArgs {
                            srcs: vec![GNode::from_node(&g.csr, x)],
                            directed,
                            ..args(all.clone())
                        },
                    ),
                    "\"result\":",
                );
                assert!(
                    (got[0] - want).abs() <= 1e-5 * want.max(1.0),
                    "seed {seed} x {x} directed {directed}: {} vs {want}",
                    got[0]
                );
            }
        }
    }
}

/// Whether `set` is a k-plex of the simple undirected graph `adj`.
fn is_kplex(adj: &[BTreeSet<Node>], set: &[Node], k: usize) -> bool {
    set.iter().all(|&v| {
        let inside = set
            .iter()
            .filter(|&&w| adj[v as usize].contains(&w))
            .count();
        inside + k >= set.len()
    })
}

#[test]
fn random_kplex_and_core_truss_against_brute_force() {
    for seed in 300..380u64 {
        let r = rand_graph(seed, 10, 2);
        let g = &r.g;
        let und = r.adj(false);
        for k in 1..=3usize {
            let out = g.json(
                GFnKind::MaximumKplex,
                &GArgs {
                    k: Some(k as i64),
                    ..args(g.preds(&[]))
                },
            );
            let names: Vec<&str> = out["{\"paths\":[{\"kplex\":[".len()..out.len() - 4]
                .split(',')
                .filter(|s| !s.is_empty())
                .collect();
            // brute-force maximum k-plex size
            let mut best = 0;
            for mask in 0u32..(1 << r.n) {
                let set: Vec<Node> = (0..r.n as Node).filter(|&v| mask & (1 << v) != 0).collect();
                if set.len() > best && is_kplex(&und, &set, k) {
                    best = set.len();
                }
            }
            let fake = 2 * k - 2;
            if best <= fake {
                // nothing beats the initial fake solution of vertex 0
                assert_eq!(names.len(), fake, "seed {seed} k {k}: {out}");
                assert!(names.iter().all(|n| *n == g.name(0)), "seed {seed}: {out}");
            } else {
                let set: Vec<Node> = names
                    .iter()
                    .map(|n| g.csr.node_of(g.db.dict().entity_id(n).unwrap()).unwrap())
                    .collect();
                // The maximum size is always found; the members are a valid
                // k-plex except where upstream maps the pruned ego network
                // back through the array it just overwrote (`ids == Qv` in
                // `subgraph_prune`) — these two graphs, where the compiled
                // upstream code prints the same invalid sets.
                assert_eq!(set.len(), best, "seed {seed} k {k}: {out}");
                let upstream_scrambled = k == 2 && (seed == 361 || seed == 375);
                assert_eq!(
                    is_kplex(&und, &set, k),
                    !upstream_scrambled,
                    "seed {seed} k {k}: {out}"
                );
            }
        }
        // coreTruss: the fixpoint of "drop vertices of degree < alpha, drop
        // edges in < beta triangles" (vertices with remaining edges).
        for (alpha, beta) in [(1usize, 0usize), (2, 0), (2, 1), (3, 1), (0, 1)] {
            let mut adj: Vec<BTreeSet<Node>> = und.clone();
            loop {
                let mut changed = false;
                for v in 0..r.n {
                    if !adj[v].is_empty() && adj[v].len() < alpha {
                        for w in std::mem::take(&mut adj[v]) {
                            adj[w as usize].remove(&(v as Node));
                        }
                        changed = true;
                    }
                }
                for v in 0..r.n as Node {
                    for w in adj[v as usize].clone() {
                        if w > v {
                            let support = adj[v as usize].intersection(&adj[w as usize]).count();
                            if support < beta {
                                adj[v as usize].remove(&w);
                                adj[w as usize].remove(&v);
                                changed = true;
                            }
                        }
                    }
                }
                if !changed {
                    break;
                }
            }
            let want: BTreeSet<String> = (0..r.n)
                .filter(|&v| !adj[v].is_empty())
                .map(|v| g.name(v as Node))
                .collect();
            let out = g.json(
                GFnKind::CoreTruss,
                &GArgs {
                    nums: vec![alpha as f64, beta as f64],
                    ..args(g.preds(&[]))
                },
            );
            let got: BTreeSet<String> = out["{\"paths\":[{\"coreTruss\":[".len()..out.len() - 4]
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            assert_eq!(got, want, "seed {seed} alpha {alpha} beta {beta}: {out}");
        }
    }
}
