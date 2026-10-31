//! Centrality, ranking, community, similarity and structure graph functions,
//! plus the Jaro string similarity behind `SIMILARITY(a, b)`.
//!
//! Every function is a literal port of its upstream gStore 1.4 counterpart:
//! the specification in each doc comment was re-checked line by line against
//! the C++ (`src/Query/Algorithm/*.cpp`, `PathQueryHandler.cpp`, the output
//! builders in `GeneralEvaluation.cpp`); where the earlier stub specification
//! was wrong or incomplete the doc says so ("Spec fix"). The ports were also
//! compared with the upstream sources compiled with g++ 13 (-O2, asserts on,
//! like gStore's default build) on tens of thousands of random graphs and
//! arguments: outputs are byte-identical except where upstream crashes, and
//! `PPR` where upstream reads uninitialised heap memory.
//!
//! Spec conventions are those of [`paths`](super::paths) ("GE:n", entries,
//! `‹u›`, literal ports). Whole-graph functions run once per call regardless
//! of bindings (upstream's `doneOnceOp`, GE:1575) and iterate vertices in
//! node order (= upstream's `for (vid = 0; vid < getVertNum(); ++vid)` when
//! entity ids are dense, see [`csr`](crate::analytics::csr) "Node ids").
//! Per-source functions loop over [`GArgs::distinct_srcs`].
//!
//! Numbers: `ss << double` → [`cxx_g`](super::fmt::cxx_g),
//! `to_string(double)` → [`cxx_to_string`](super::fmt::cxx_to_string),
//! `float` → [`cxx_g_f32`](super::fmt::cxx_g_f32). Parameters upstream reads
//! with `stof` (`alpha`, `tol`, `increase`, `maxIter`) go through
//! [`f32_round`](super::fmt::f32_round). C `int` accumulators wrap like `i32`.
//!
//! # Cost bounds
//!
//! Upstream's algorithms are kept, including their asymptotic cost, with
//! three exceptions that avoid blow-ups without changing any output:
//! `betweennessCentrality` counts shortest paths by dynamic programming
//! instead of enumerating them (upstream's DFS is exponential on graphs with
//! many equal-length paths; the `int` sums are reproduced exactly because
//! wrapping arithmetic is a ring), `labelProp` counts neighbour labels
//! sparsely instead of clearing an `n`-sized array per vertex, and `louvain`
//! precomputes each super-node's incoming weight once per round. `PR` keeps
//! upstream's `O(n · dangling)` sweep up to [`PR_EXACT_DANGLING_WORK`]
//! additions per sweep and aggregates the dangling mass beyond that (see
//! [`pr`]). `PPR`, `maximumKplex` and `coreTruss` keep upstream's costs
//! (random-walk counts, the exponential branch and bound per ego network,
//! bounded to ego networks of 8192 vertices; see [`maximum_kplex`]).
//! Every long loop polls the cancellation flag ([`GFunCtx::check_cancel`]).

mod grisu;
mod kplex;
mod ppr;

pub use grisu::nlohmann_dump_f64;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use super::cxx::{cxx_sort_by, CxxUnorderedOrder, GlibcRand};
use super::fmt::{
    cxx_g, cxx_g_f32, cxx_int_id, cxx_to_string, f32_round, paths_json, push_json_quoted,
    push_vert_list_json, JsonList,
};
use super::traverse::{for_each_nbr, NbrOrder, NodeMarks};
use super::{CsrGraph, Direction, GArgs, GFnKind, GFunCtx, GFunValue, Node, PredFilter};
use crate::error::{GStoreError, Result};
use crate::model::id::EntityLiteralId;

/// The fixed seed ports use for FORA's random walks ([`ppr`]): upstream calls
/// `srand(time(NULL))` (SSPPR.cpp:100), which is not reproducible; ours uses
/// [`GlibcRand::new(PPR_SEED)`](super::cxx::GlibcRand) (glibc's default
/// state, as if `srand` had never been called), reseeded on every call.
pub const PPR_SEED: u32 = 1;

/// The seed upstream's `labelProp` passes to `std::srand` (labelProp.cpp:173-176).
pub const LABEL_PROP_SEED: u32 = 114_514;

/// `PR` follows upstream's literal sweep — every dangling vertex adds its
/// share to every vertex, in vertex order — while `dangling · n` stays at or
/// below this many additions per sweep; beyond it the dangling mass is
/// summed once per sweep and added to every vertex (mathematically equal,
/// may differ from upstream in the last bits of the floating-point sums).
pub const PR_EXACT_DANGLING_WORK: usize = 1 << 26;

// ---------------------------------------------------------------------------
// small shared helpers
// ---------------------------------------------------------------------------

/// Poll cancellation every 1024 steps.
#[inline]
fn tick(ctx: &GFunCtx<'_>, i: usize) -> Result<()> {
    if i & 0x3ff == 0 {
        ctx.check_cancel()
    } else {
        Ok(())
    }
}

/// An `i64` argument as the C `int` upstream parses it into (`stoi`),
/// saturated instead of throwing.
fn to_int(x: i64) -> i32 {
    x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// C++ `(int)double` (truncation; out-of-range values are UB upstream and
/// saturate here).
fn d2i(x: f64) -> i32 {
    x as i32
}

/// A required `name=value` argument (the grammar always has it; a missing
/// one means the caller built the [`GArgs`] by hand).
fn named(kind: GFnKind, v: Option<f64>, key: &str) -> Result<f64> {
    v.ok_or_else(|| GStoreError::Query(format!("{}: missing argument {key}=", kind.name())))
}

/// The printed form of a vertex (node) as upstream's `getStringByID(vid)`.
fn node_name<'a>(ctx: &GFunCtx<'a>, n: Node) -> &'a str {
    ctx.node_str(n)
}

/// The degree of `v` over `f` in the `directed` sense upstream's loops use:
/// out-degree, plus in-degree when undirected (with multiplicity).
fn degree(g: &CsrGraph, v: Node, f: &PredFilter, directed: bool) -> usize {
    g.out_degree(v, f) + if directed { 0 } else { g.in_degree(v, f) }
}

/// Distinct in- ∪ out-neighbours (ascending) — upstream's
/// `unordered_set` / `std::set` neighbour sets built from both directions.
fn nbr_set(g: &CsrGraph, v: Node, f: &PredFilter) -> Vec<Node> {
    g.distinct_neighbors(v, f, Direction::Both)
}

// ---------------------------------------------------------------------------
// PPR
// ---------------------------------------------------------------------------

/// `PPR(u, hopCnt, preds, retNum)` — top-`retNum` personalized PageRank from
/// `u` (FORA, approximate).
///
/// **Upstream:** GE:1770-1783 / GE:2824-2841; `PathQueryHandler::SSPPR`
/// (SSPPR.cpp:96-191) with its helpers (SSPPR.cpp:5-94,
/// PathQueryHandler.cpp:3025-3337). Ported in the private `ppr` module.
///
/// **Per source** (sources separated by `,`):
/// `{"src":"‹u›","results":[{"dst":"‹d›","PPR":x},…]}` with `x` printed by
/// `ss << double`.
///
/// **Algorithm** (literal port): out-edges only (no `directed` argument),
/// teleport `alpha = 0.2`, `ppr_decay_alpha = 0.77`, `epsilon = 0.5`,
/// `delta` from 1/4 halving down to `1/n`, `pfail = 1/n²/ln n`; forward push
/// (`forward_local_update_linear_topk`) bounded by `hopCnt` levels
/// (`k == -1` = unbounded, other values compare `level < k`), then random
/// walks (`random_walk`: `rand()/RAND_MAX <= alpha` stops; next hop
/// `rand() % out_degree` picked across predicates in filter order,
/// PathQueryHandler.cpp:3118-3152) with
/// [`GlibcRand::new(PPR_SEED)`](super::cxx::GlibcRand) — upstream's result
/// differs run to run (time seed; the docs say so), ours is deterministic.
/// Note that `v2ppr` keeps the walk estimates of earlier `delta` rounds
/// (only nodes with a reserve are overwritten) — reproduced.
///
/// **Spec fixes / quirks reproduced:**
/// * The `delta` loop runs only while `delta >= 1/n`, i.e. never on graphs
///   with **fewer than 4 vertices**: `v2ppr` stays empty and the result is
///   the single default slot `{"dst":"‹vertex 0›","PPR":0}` (for every
///   source).
/// * Otherwise a source without out-edges keeps all mass: `{u: 1}`
///   (SSPPR.cpp:159-165); this also holds for an isolated source.
/// * `kth_ppr` (SSPPR.cpp:5-20) indexes `v2ppr` with the PPR *values*
///   (`v2ppr[it->second]`, a `double` converted to an `int` key), so it reads
///   — and, if absent, inserts with value 0 — vertex `0` (vertex `1` for a
///   value of exactly 1.0), while iterating the map. The insertion changes
///   `v2ppr`'s iteration order and can surface vertex 0 in the output.
/// * `kth_ppr`'s buffer is a function-`static` vector: with `retNum` larger
///   than `v2ppr` it returns a stale value an earlier call (an earlier
///   source of the same query, upstream: of the process) left behind, and
///   `0.0` where upstream reads foreign heap memory. This decides how many
///   `delta` rounds run, i.e. the values printed.
/// * `if_stop`'s scan (SSPPR.cpp:74-91) evaluates `v2ppr[nodeid]` for
///   every vertex outside the current top-k, inserting zeros.
///
/// **Top-k extraction** (SSPPR.cpp:178-190): `topk.resize(retNum)`, then
/// `partial_sort_copy` of the `unordered_map<int,double>` `v2ppr` (iterated
/// in [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder) order of its
/// insertion sequence, keyed by entity id) by value descending into `topk`
/// ([`cxx_partial_sort_copy_by`](super::cxx::cxx_partial_sort_copy_by));
/// unfilled slots stay `(0, 0.0)`; then trailing entries with value `0` are
/// dropped but **at least one entry is kept** — an all-zero result prints
/// vertex 0 with `"PPR":0` (quirk).
///
/// **Edge cases:** `retNum = 0` → `"results":[]` (upstream: the same after
/// out-of-bounds reads); negative `retNum` → error (upstream:
/// `std::length_error` from `resize`). A missing `u` → no entry (upstream
/// writes through index `-1`, undefined behaviour). Every random walk and
/// `delta` round polls the cancellation flag; the walk count is upstream's
/// (`ω · rsum` per round, `ω ≈ 2.5 · ln(2/pfail) / (δ · ε²)`).
pub fn ppr(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let ret_num = to_int(a.ret_num.unwrap_or(-1));
    let k = to_int(a.k.unwrap_or(-1));
    if ret_num < 0 {
        // Upstream: `topkV2ppr.resize(retNum)` throws `std::length_error`.
        return Err(GStoreError::Query(format!(
            "PPR: retNum must not be negative (got {ret_num})"
        )));
    }
    let mut out = JsonList::new();
    let mut session = None;
    for src in a.distinct_srcs() {
        ctx.check_cancel()?;
        if src.is_missing() {
            continue;
        }
        let top = if ret_num == 0 {
            Vec::new()
        } else {
            let session = session.get_or_insert_with(|| ppr::Session::new(ctx.csr, &a.preds));
            ppr::ssppr(ctx, session, &src, &a.preds, ret_num as usize, k)?
        };
        let e = out.item();
        e.push_str("{\"src\":");
        push_json_quoted(e, ctx.gnode_str(&src));
        e.push_str(",\"results\":[");
        for (i, (entity, value)) in top.iter().enumerate() {
            if i > 0 {
                e.push(',');
            }
            e.push_str("{\"dst\":");
            push_json_quoted(e, ctx.entity_str(*entity));
            e.push_str(",\"PPR\":");
            e.push_str(&cxx_g(*value));
            e.push('}');
        }
        e.push_str("]}");
    }
    Ok(GFunValue::Json(out.into_paths()))
}

// ---------------------------------------------------------------------------
// PR
// ---------------------------------------------------------------------------

/// `PR(directed, preds, retNum, alpha=a, maxIter=m, tol=t)` — PageRank.
///
/// **Upstream:** GE:1812-1831 / GE:2878-2897; PR.cpp:15-91. Arguments:
/// `alpha = f32_round(a)`, `tol = f32_round(t)`, `maxIter = f32_round(m) as i32`,
/// `retNum = a.ret_num` (absent → `-1`).
///
/// **Output (once):** entries `{"src":"‹v›","result":x}` (`ss << double`),
/// comma-separated: `{"paths":[{"src":"<Alice>","result":0.1},…]}`. (The docs
/// show a `"results"` key; the code writes `"result"`.)
///
/// **Algorithm:** `n` vertices, `neiNum[v]` = out-degree (+ in-degree when
/// undirected) with multiplicity; `old = 1/n`, `new = (1-alpha)/n`;
/// `while !stop && cnt++ <= maxIter` (so up to `maxIter + 1` sweeps): for
/// each vertex in node order, dangling (`neiNum == 0`) → add
/// `alpha·old[v]/n` to every vertex, else add `alpha·old[v]/neiNum[v]` to
/// each out-neighbour (and in-neighbour when undirected) per edge; then
/// `stop` unless some `|new-old| > tol`, `old = new`, `new = (1-alpha)/n`.
/// The dangling additions are exact up to [`PR_EXACT_DANGLING_WORK`] per
/// sweep (see there).
///
/// **Selection:** `retNum > n` is clamped to `n`. If `retNum == -1` or
/// `retNum >= n`: all vertices in node order (unsorted). Otherwise
/// (`0 <= retNum < n`): libstdc++ `std::sort` by value descending —
/// [`cxx_sort_by`](super::cxx::cxx_sort_by), which reproduces its order for
/// ties — and keep the first `retNum`. `retNum < -1` is UB upstream; treated
/// as `-1`. Empty graph → `{"paths":[]}`.
pub fn pr(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let kind = GFnKind::Pr;
    let alpha = f32_round(named(kind, a.alpha, "alpha")?);
    let max_iter = d2i(f32_round(named(kind, a.max_iter, "maxIter")?));
    let tol = f32_round(named(kind, a.tol, "tol")?);
    let g = ctx.csr;
    let f = &a.preds;
    let n = g.num_nodes();
    let node_num = n as i32;
    let mut ret_num = to_int(a.ret_num.unwrap_or(-1));
    if ret_num < -1 {
        ret_num = -1;
    }
    if ret_num > node_num {
        ret_num = node_num;
    }
    let nei: Vec<usize> = (0..n as Node)
        .map(|v| degree(g, v, f, a.directed))
        .collect();
    let dangling: Vec<usize> = (0..n).filter(|&v| nei[v] == 0).collect();
    let exact = dangling.len().saturating_mul(n) <= PR_EXACT_DANGLING_WORK;
    let nf = n as f64;
    let offset = (1.0 - alpha) / nf;
    let mut old = vec![1.0 / nf; n];
    let mut new = vec![offset; n];
    let mut stop = false;
    let mut cnt: i64 = 0;
    while !stop && {
        let c = cnt;
        cnt += 1;
        c <= i64::from(max_iter)
    } {
        ctx.check_cancel()?;
        stop = true;
        let mut dangling_mass = 0.0;
        for vid in 0..n {
            tick(ctx, vid)?;
            if nei[vid] == 0 {
                let add = alpha * old[vid] / nf;
                if exact {
                    new.iter_mut().for_each(|x| *x += add);
                } else {
                    dangling_mass += add;
                }
            } else {
                let add = alpha * old[vid] / nei[vid] as f64;
                let v = vid as Node;
                if a.directed {
                    g.for_each_out(v, f, |to, _| new[to as usize] += add);
                } else {
                    g.for_each_both(v, f, |to, _, _| new[to as usize] += add);
                }
            }
        }
        if !exact {
            new.iter_mut().for_each(|x| *x += dangling_mass);
        }
        for vid in 0..n {
            if (new[vid] - old[vid]).abs() > tol {
                stop = false;
            }
            old[vid] = new[vid];
            new[vid] = offset;
        }
    }
    let mut idx2val: Vec<(Node, f64)> = (0..n as Node).map(|v| (v, old[v as usize])).collect();
    if ret_num != -1 && ret_num < node_num {
        cxx_sort_by(&mut idx2val, |l, r| l.1 > r.1);
        idx2val.truncate(ret_num as usize);
    }
    let mut out = JsonList::new();
    for (v, x) in idx2val {
        let e = out.item();
        e.push_str("{\"src\":");
        push_json_quoted(e, node_name(ctx, v));
        e.push_str(",\"result\":");
        e.push_str(&cxx_g(x));
        e.push('}');
    }
    Ok(GFunValue::Json(out.into_paths()))
}

// ---------------------------------------------------------------------------
// triangleCounting
// ---------------------------------------------------------------------------

/// `triangleCounting(directed, preds)` — number of triangles.
///
/// **Upstream:** GE:1784-1792 / GE:2842-2850; triangleCounting.cpp:14-105.
///
/// **Output (once):** `{"paths":[N]}` (C `long long`, `to_string`).
///
/// **N:** empty `preds` → -1. For every vertex `v`:
/// * directed (only cycles `a→b→c→a`): `S` = distinct in-neighbours of `v`;
///   for every out-edge `v→w` and every out-edge `w→x` (per predicate, with
///   multiplicity) count `x ∈ S`; total / 3.
/// * undirected: `S` = distinct in- ∪ out-neighbours of `v`; for every `w ∈ S`
///   count the members of `S` among `w`'s distinct in- ∪ out-neighbours;
///   total / 6.
///
/// If the total is not divisible (3 resp. 6, e.g. because of multi-edges or
/// self-loops) upstream returns **-1** — reproduced.
///
/// **Example** (SPARQL_QUERY.md): `triangleCounting(true, {<like>})` →
/// `{"paths":[1]}`.
pub fn triangle_counting(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let n = triangle_count(ctx, &a.preds, a.directed)?;
    Ok(GFunValue::Json(paths_json(&n.to_string())))
}

fn triangle_count(ctx: &GFunCtx<'_>, f: &PredFilter, directed: bool) -> Result<i64> {
    if f.is_empty() {
        return Ok(-1);
    }
    let g = ctx.csr;
    let n = g.num_nodes();
    let mut s = NodeMarks::new(n);
    let mut nb = NodeMarks::new(n);
    let mut total: i64 = 0;
    for vid in 0..n as Node {
        tick(ctx, vid as usize)?;
        s.clear();
        if directed {
            g.for_each_in(vid, f, |x, _| {
                s.mark(x);
            });
            g.for_each_out(vid, f, |w, _| {
                g.for_each_out(w, f, |x, _| {
                    if s.is_marked(x) {
                        total += 1;
                    }
                });
            });
        } else {
            let set = nbr_set(g, vid, f);
            for &x in &set {
                s.mark(x);
            }
            for &w in &set {
                nb.clear();
                g.for_each_both(w, f, |x, _, _| {
                    if nb.mark(x) && s.is_marked(x) {
                        total += 1;
                    }
                });
            }
        }
    }
    let dup = if directed { 3 } else { 6 };
    Ok(if total % dup != 0 { -1 } else { total / dup })
}

// ---------------------------------------------------------------------------
// closenessCentrality
// ---------------------------------------------------------------------------

/// `closenessCentrality(u, directed, preds)`.
///
/// **Upstream:** GE:1793-1798 / GE:2851-2860; closenessCentrality.cpp:17-56.
///
/// **Per source** (separated by `,`): `{"src":"‹u›","result":x}` with `x`
/// printed by **`to_string`** (6 decimals: `0.555556`, `0.500000`).
///
/// **Value:** BFS from `u` (out-edges, plus in-edges when undirected), `cnt`
/// = reached vertices including `u`, `sum` = Σ distances (C `int`):
/// `(cnt - 1) / sum` as double. An isolated / missing `u` (missing prints
/// `"src":""`; upstream's `std::map` BFS has no out-of-bounds access) gives
/// `0.0/0.0`, which on x86-64 is a *negative* NaN → `"result":-nan`.
///
/// **Examples** (SPARQL_QUERY.md): `closenessCentrality(<Alice>, false, {<like>, <focus>})`
/// → `{"paths":[{"src":"<Alice>","result":0.555556}]}`; with `true` →
/// `"result":0.500000`.
pub fn closeness_centrality(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let g = ctx.csr;
    let mut out = JsonList::new();
    for src in a.distinct_srcs() {
        ctx.check_cancel()?;
        let (mut cnt, mut sum) = (1i32, 0i32);
        if let Some(u) = src.node {
            cnt = 0;
            let _ = super::traverse::bfs(
                g,
                u,
                &a.preds,
                Direction::from_directed(a.directed),
                NbrOrder::PerPredOutThenIn,
                None,
                |_, d| {
                    cnt = cnt.wrapping_add(1);
                    sum = sum.wrapping_add(d as i32);
                    std::ops::ControlFlow::<()>::Continue(())
                },
            );
        }
        let mut x = f64::from(cnt.wrapping_sub(1)) / f64::from(sum);
        if x.is_nan() {
            x = -f64::NAN; // x86-64 default NaN of 0.0/0.0
        }
        let e = out.item();
        e.push_str("{\"src\":");
        push_json_quoted(e, ctx.gnode_str(&src));
        e.push_str(",\"result\":");
        e.push_str(&cxx_to_string(x));
        e.push('}');
    }
    Ok(GFunValue::Json(out.into_paths()))
}

// ---------------------------------------------------------------------------
// betweennessCentrality
// ---------------------------------------------------------------------------

/// `betweennessCentrality(u, directed, preds)`.
///
/// **Upstream:** GE:1996-1999 / GE:3074-3077 → wrapper GE:4099-4105;
/// betweennessCentrality.cpp:5-258.
///
/// **Per source:** missing `u` → nothing; else `{"src":"‹u›", "result":x}`
/// (space after the comma; `ss << double`). Upstream writes consecutive
/// sources without `,`; we separate them.
///
/// **Value** (`betweennessCentrality`, :233-258): empty `preds` → 0; `V` =
/// all vertices reachable from `u` in the **undirected** view
/// (`getAllNeighborsById(u, false, …)`, :5-48), minus `u`; `|V| <= 1` → 0;
/// otherwise `Σ_{s ∈ V ascending} shortestPathScalePassId(s, u, directed)`,
/// halved when undirected. `shortestPathScalePassId(s, u)` (:74-231): BFS
/// from `s` (out-edges, plus in-edges when undirected) recording all
/// shortest-path predecessors; for each target `t` (ascending,
/// `dist(t) > 1`, `t ∉ {s, u}`) such that `u` lies on some shortest
/// `s → t` path, add `pass/total`, where each vertex path counts with the
/// product over its hops `(a, b)` of the number of distinct predicates `p`
/// with `a -p-> b` (or, when undirected, `b -p-> a`), `total` sums all
/// shortest paths, `pass` those through `u` (C `int` arithmetic; zero
/// `pass` or `total` → skipped).
///
/// **Bound:** upstream enumerates every shortest path by DFS (exponential on
/// e.g. grids); the port computes the same `int` sums by dynamic
/// programming over the shortest-path DAG (`total(t) = W_s(t)`,
/// `pass(t) = W_s(u) · W_u(t)`), which is exact in wrapping `i32`
/// arithmetic.
///
/// **Example** (SPARQL_QUERY.md): `betweennessCentrality(<Eve>, true, {…})`
/// → `{"paths":[{"src":"<Eve>", "result":6.5}]}` (the doc example writes
/// `<likes>`, `<follows>`, `<dislikes>`, which the sample data does not use
/// and which resolve to nothing, giving 0; over the data's own predicates,
/// `{}`, the value is the doc's 6.5).
pub fn betweenness_centrality(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let mut out = JsonList::new();
    for src in a.distinct_srcs() {
        ctx.check_cancel()?;
        if src.is_missing() {
            continue;
        }
        let x = match src.node {
            Some(u) => betweenness_value(ctx, &a.preds, u, a.directed)?,
            None => 0.0,
        };
        let e = out.item();
        e.push_str("{\"src\":");
        push_json_quoted(e, ctx.gnode_str(&src));
        e.push_str(", \"result\":");
        e.push_str(&cxx_g(x));
        e.push('}');
    }
    Ok(GFunValue::Json(out.into_paths()))
}

fn betweenness_value(ctx: &GFunCtx<'_>, f: &PredFilter, id: Node, directed: bool) -> Result<f64> {
    if f.is_empty() {
        return Ok(0.0);
    }
    let g = ctx.csr;
    let mut vertex: Vec<Node> =
        super::traverse::bfs_order(g, id, f, Direction::Both, NbrOrder::PerPredOutThenIn)
            .into_iter()
            .map(|(v, _)| v)
            .filter(|&v| v != id)
            .collect();
    if vertex.len() <= 1 {
        return Ok(0.0);
    }
    vertex.sort_unstable();
    let mut ret = 0.0;
    let mut scratch = ScaleScratch::new(g.num_nodes());
    for s in vertex {
        ctx.check_cancel()?;
        ret += shortest_path_scale_pass_id(g, f, s, id, directed, &mut scratch);
    }
    if !directed {
        ret /= 2.0;
    }
    Ok(ret)
}

struct ScaleScratch {
    dist: Vec<u32>,
    w: Vec<i32>,
    wp: Vec<i32>,
    reach: Vec<bool>,
    touched: Vec<Node>,
}

impl ScaleScratch {
    fn new(n: usize) -> ScaleScratch {
        ScaleScratch {
            dist: vec![u32::MAX; n],
            w: vec![0; n],
            wp: vec![0; n],
            reach: vec![false; n],
            touched: Vec::new(),
        }
    }
}

/// `shortestPathScalePassId(uid = s, passid = pass)`; see
/// [`betweenness_centrality`] for the DP formulation.
fn shortest_path_scale_pass_id(
    g: &CsrGraph,
    f: &PredFilter,
    s: Node,
    pass: Node,
    directed: bool,
    sc: &mut ScaleScratch,
) -> f64 {
    if s == pass {
        return 0.0;
    }
    for &t in &sc.touched {
        sc.dist[t as usize] = u32::MAX;
        sc.w[t as usize] = 0;
        sc.wp[t as usize] = 0;
        sc.reach[t as usize] = false;
    }
    sc.touched.clear();
    // BFS (distances do not depend on the neighbour order).
    let dir = Direction::from_directed(directed);
    let mut order: Vec<Node> = vec![s];
    sc.dist[s as usize] = 0;
    sc.touched.push(s);
    let mut head = 0;
    while head < order.len() {
        let v = order[head];
        head += 1;
        let d = sc.dist[v as usize];
        for_each_nbr(g, v, f, dir, NbrOrder::PerPredOutThenIn, |t, _, _| {
            if sc.dist[t as usize] == u32::MAX {
                sc.dist[t as usize] = d + 1;
                sc.touched.push(t);
                order.push(t);
            }
        });
    }
    // Predecessors of t with the hop weight (number of distinct predicates
    // joining them), from t's side: p -x-> t is an in-edge of t; when
    // undirected also t -x-> p (an out-edge of t).
    let mut hops: Vec<(Node, u32)> = Vec::new();
    let mut preds_of = |t: Node, dist: &[u32]| -> Vec<(Node, i32)> {
        hops.clear();
        let dt = dist[t as usize];
        let back = match dir {
            Direction::Out => Direction::In,
            _ => Direction::Both,
        };
        for_each_nbr(g, t, f, back, NbrOrder::PerPredOutThenIn, |p, x, _| {
            if dist[p as usize] != u32::MAX && dist[p as usize] + 1 == dt {
                hops.push((p, x));
            }
        });
        hops.sort_unstable();
        hops.dedup();
        let mut out: Vec<(Node, i32)> = Vec::new();
        for &(p, _) in hops.iter() {
            match out.last_mut() {
                Some((q, c)) if *q == p => *c += 1,
                _ => out.push((p, 1)),
            }
        }
        out
    };
    sc.w[s as usize] = 1;
    let pass_reached = sc.dist[pass as usize] != u32::MAX;
    if pass_reached {
        sc.wp[pass as usize] = 1;
        sc.reach[pass as usize] = true;
    }
    for &t in order.iter().skip(1) {
        let ps = preds_of(t, &sc.dist);
        let (mut w, mut wp, mut reach) = (0i32, 0i32, false);
        for (p, c) in ps {
            w = w.wrapping_add(sc.w[p as usize].wrapping_mul(c));
            if t != pass {
                wp = wp.wrapping_add(sc.wp[p as usize].wrapping_mul(c));
                reach |= sc.reach[p as usize];
            }
        }
        sc.w[t as usize] = w;
        if t != pass {
            sc.wp[t as usize] = wp;
            sc.reach[t as usize] = reach;
        }
    }
    let mut targets: Vec<Node> = order
        .iter()
        .copied()
        .filter(|&t| sc.dist[t as usize] > 1 && t != s && t != pass && sc.reach[t as usize])
        .collect();
    targets.sort_unstable();
    let w_pass = if pass_reached { sc.w[pass as usize] } else { 0 };
    let mut ret = 0.0;
    for t in targets {
        let pass_num = w_pass.wrapping_mul(sc.wp[t as usize]);
        let total_num = sc.w[t as usize];
        if pass_num == 0 || total_num == 0 {
            continue;
        }
        ret += f64::from(pass_num) / f64::from(total_num);
    }
    ret
}

// ---------------------------------------------------------------------------
// clusterCoeff
// ---------------------------------------------------------------------------

/// `clusterCoeff([u,] directed, preds)` — local or global clustering
/// coefficient.
///
/// **Upstream:** GE:1906-1923 / GE:2972-2989; clusteringCoeff.cpp:12-193.
///
/// * **No source** (`srcs` empty) — and also a **missing** constant source,
///   because upstream tests `uid == -1` for "not given" (quirk): once,
///   `{"paths":[x]}` with `x = to_string(global)`, e.g. `{"paths":[0.333333]}`.
/// * **Per source:** `{"src":"‹u›","result":x}`, `x = to_string(local)`.
///   Upstream writes consecutive sources (and the global value, when a
///   missing source is mixed with real ones) without `,`; we separate them.
///   (A variable with no bindings at all also lands here as "no source";
///   upstream would print `{"paths":[]}` for a WHERE that has rows but never
///   binds it — the [`GArgs`] model cannot express that case.)
///
/// **Global** (:12-89, ignores `directed` — always undirected): empty `preds`
/// → -1; for each vertex `c`, `S` = distinct in ∪ out neighbours; for each
/// `w ∈ S`, for each distinct neighbour `x` of `w`: `x ∈ S` → closed += 1,
/// else if `x != c` → open += 1; both halved (integer division); value
/// `closed / (closed + open)`, 0 when both are 0.
///
/// **Local** (:91-193): empty `preds` → -1. Directed: `I` = distinct
/// in-neighbours, `N` = distinct in ∪ out neighbours, `d` = Σ over out-edges
/// `u→w` and out-edges `w→x` (with multiplicity) of `[x ∈ I]`, value
/// `d / (|N|·(|N|-1))`. Undirected: `S` = distinct in ∪ out neighbours,
/// `d = (Σ_{w ∈ S} |nbrs(w) ∩ S|) / 2` (integer), value `d / (|S|(|S|-1)/2)`.
/// A zero denominator gives 0; an isolated `u` gives 0.
pub fn cluster_coeff(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let srcs = a.distinct_srcs();
    if srcs.is_empty() {
        let x = global_cluster_coeff(ctx, &a.preds)?;
        return Ok(GFunValue::Json(paths_json(&cxx_to_string(x))));
    }
    let mut out = JsonList::new();
    let mut done_once = false;
    for src in srcs {
        ctx.check_cancel()?;
        if src.is_missing() {
            if !done_once {
                let x = global_cluster_coeff(ctx, &a.preds)?;
                out.item().push_str(&cxx_to_string(x));
                done_once = true;
            }
            continue;
        }
        let x = local_cluster_coeff(ctx.csr, &a.preds, src.node, a.directed);
        let e = out.item();
        e.push_str("{\"src\":");
        push_json_quoted(e, ctx.gnode_str(&src));
        e.push_str(",\"result\":");
        e.push_str(&cxx_to_string(x));
        e.push('}');
    }
    Ok(GFunValue::Json(out.into_paths()))
}

fn global_cluster_coeff(ctx: &GFunCtx<'_>, f: &PredFilter) -> Result<f64> {
    if f.is_empty() {
        return Ok(-1.0);
    }
    let g = ctx.csr;
    let n = g.num_nodes();
    let mut in_s = NodeMarks::new(n);
    let (mut closed, mut open) = (0i32, 0i32);
    for c in 0..n as Node {
        tick(ctx, c as usize)?;
        in_s.clear();
        let set = nbr_set(g, c, f);
        for &x in &set {
            in_s.mark(x);
        }
        for &w in &set {
            for x in nbr_set(g, w, f) {
                if in_s.is_marked(x) {
                    closed = closed.wrapping_add(1);
                } else if x != c {
                    open = open.wrapping_add(1);
                }
            }
        }
    }
    closed /= 2;
    open /= 2;
    let sum = closed.wrapping_add(open);
    Ok(if sum == 0 {
        0.0
    } else {
        f64::from(closed) / f64::from(sum)
    })
}

fn local_cluster_coeff(g: &CsrGraph, f: &PredFilter, u: Option<Node>, directed: bool) -> f64 {
    if f.is_empty() {
        return -1.0;
    }
    let Some(u) = u else {
        return 0.0;
    };
    let n = g.num_nodes();
    let mut marks = NodeMarks::new(n);
    let (dins, maxns): (i32, i32) = if directed {
        let ins = g.distinct_neighbors(u, f, Direction::In);
        for &x in &ins {
            marks.mark(x);
        }
        let mut d = 0i32;
        g.for_each_out(u, f, |w, _| {
            g.for_each_out(w, f, |x, _| {
                if marks.is_marked(x) {
                    d = d.wrapping_add(1);
                }
            });
        });
        let ns = nbr_set(g, u, f).len() as i32;
        (d, ns.wrapping_mul(ns.wrapping_sub(1)))
    } else {
        let set = nbr_set(g, u, f);
        for &x in &set {
            marks.mark(x);
        }
        let mut d = 0i32;
        for &w in &set {
            for x in nbr_set(g, w, f) {
                if marks.is_marked(x) {
                    d = d.wrapping_add(1);
                }
            }
        }
        let ns = set.len() as i32;
        (d / 2, ns.wrapping_mul(ns.wrapping_sub(1)) / 2)
    };
    if maxns == 0 {
        0.0
    } else {
        f64::from(dins) / f64::from(maxns)
    }
}

// ---------------------------------------------------------------------------
// labelProp
// ---------------------------------------------------------------------------

/// `labelProp(directed, preds, maxIter=m)` — label-propagation communities.
///
/// **Upstream:** GE:1872-1888 / GE:2938-2954; labelProp.cpp:13-198.
///
/// **Output (once):** the communities as `vertVec2JSON` arrays
/// ([`push_vert_list_json`](super::fmt::push_vert_list_json)),
/// comma-separated: `{"paths":[["<Alice>","<Bob>"],["<Carol>"]]}`.
///
/// **Algorithm** (literal port of class `LPA`): `maxIter = f32_round(m) as i32`;
/// [`GlibcRand::new(LABEL_PROP_SEED)`](super::cxx::GlibcRand) (upstream
/// seeds `std::srand(114514)` — reproducible). Labels start as the vertex
/// ids. Each iteration (`propagate`, asynchronous in node order): count the
/// labels of the in-neighbours (and out-neighbours when undirected) per
/// edge; collect the labels with the maximal count in ascending label order;
/// if the maximum is non-zero pick `maxLbl[rand() % maxNum]` (one `rand()`
/// per such vertex). Then the modularity
/// `Q = Σ_{com ascending} edge/dblM - (degree/dblM)²` with `dblM = 2 · n`
/// (upstream uses the **vertex** count as edge count — kept), and
/// `maxLabels = labels` whenever `Q > -1` (`maxQ` is never updated — kept).
/// Stop after `maxIter` iterations, or at the 11th consecutive iteration in
/// which the community count stayed the same and `|ΔQ| < 0.001`
/// (`tol = 10`, `thres = 0.001`, :173-196).
///
/// **Order:** communities by label ascending (the *current* labels'
/// `std::set`), members in node order. Vertices without edges in `preds`
/// keep their own label.
///
/// **Spec fix (quirks kept):** `output()` maps `maxLabels` through the
/// labels of the **last** iteration (`lbl2id[maxLabels[vid]]`): when the
/// last iteration had `Q <= -1`, `maxLabels` is stale, a stale label that is
/// no longer current maps to community index 0 (the `std::map` default), and
/// a current label nobody carries in `maxLabels` prints as `[]`. When
/// `maxLabels` was never assigned (`maxIter <= 0`, or `Q <= -1` throughout —
/// UB upstream) the initial labels (every vertex its own label) are used.
pub fn label_prop(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let max_iter = d2i(f32_round(named(GFnKind::LabelProp, a.max_iter, "maxIter")?));
    let g = ctx.csr;
    let f = &a.preds;
    let n = g.num_nodes();
    let dir = if a.directed {
        Direction::In
    } else {
        Direction::Both
    };
    let mut labels: Vec<u32> = (0..n as u32).collect();
    let mut coms: Vec<u32> = labels.clone();
    let mut max_labels: Option<Vec<u32>> = None;
    let mut rng = GlibcRand::new(LABEL_PROP_SEED);
    let mut cnt = vec![0i32; n];
    let mut touched: Vec<u32> = Vec::new();
    let mut max_lbl: Vec<u32> = Vec::new();
    let dbl_m = 2.0 * n as f64;
    let (mut old_q, mut old_num) = (-1.0f64, -1.0f64);
    let mut stable = 0i32;
    for _ in 1..=max_iter.max(0) {
        ctx.check_cancel()?;
        // propagate
        for vid in 0..n {
            tick(ctx, vid)?;
            let mut max_cnt = 0;
            for_each_nbr(
                g,
                vid as Node,
                f,
                dir,
                NbrOrder::PerPredInThenOut,
                |to, _, _| {
                    let l = labels[to as usize] as usize;
                    if cnt[l] == 0 {
                        touched.push(l as u32);
                    }
                    cnt[l] += 1;
                    if max_cnt < cnt[l] {
                        max_cnt = cnt[l];
                    }
                },
            );
            if max_cnt > 0 {
                max_lbl.clear();
                max_lbl.extend(
                    touched
                        .iter()
                        .copied()
                        .filter(|&l| cnt[l as usize] == max_cnt),
                );
                max_lbl.sort_unstable();
                let pick = rng.rand() % max_lbl.len() as i32;
                labels[vid] = max_lbl[pick as usize];
            }
            for &l in &touched {
                cnt[l as usize] = 0;
            }
            touched.clear();
        }
        coms = labels.clone();
        coms.sort_unstable();
        coms.dedup();
        // modularity
        let mut edge = vec![0i32; n];
        let mut deg = vec![0i32; n];
        for v in 0..n as Node {
            let l = labels[v as usize];
            let li = l as usize;
            g.for_each_out(v, f, |to, _| {
                deg[li] = deg[li].wrapping_add(1);
                if labels[to as usize] == l {
                    edge[li] = edge[li].wrapping_add(1);
                }
            });
            if !a.directed {
                g.for_each_in(v, f, |to, _| {
                    deg[li] = deg[li].wrapping_add(1);
                    if labels[to as usize] == l {
                        edge[li] = edge[li].wrapping_add(1);
                    }
                });
            }
        }
        let mut q = 0.0;
        for &c in &coms {
            let (e, d) = (f64::from(edge[c as usize]), f64::from(deg[c as usize]));
            q += e / dbl_m - (d / dbl_m) * (d / dbl_m);
        }
        if -1.0 < q {
            max_labels = Some(labels.clone());
        }
        let com_num = coms.len() as f64;
        if com_num == old_num && (q - old_q).abs() < 0.001 {
            if stable >= 10 {
                break;
            }
            stable += 1;
        } else {
            stable = 0;
        }
        old_q = q;
        old_num = com_num;
    }
    let max_labels = max_labels.unwrap_or_else(|| (0..n as u32).collect());
    let mut lbl2id: BTreeMap<u32, usize> = coms.iter().enumerate().map(|(i, &c)| (c, i)).collect();
    let mut res: Vec<Vec<EntityLiteralId>> = vec![Vec::new(); coms.len()];
    for (vid, &label) in max_labels.iter().enumerate() {
        let idx = *lbl2id.entry(label).or_insert(0);
        if let Some(r) = res.get_mut(idx) {
            r.push(g.entity_of(vid as Node));
        }
    }
    let mut out = JsonList::new();
    for r in &res {
        push_vert_list_json(out.item(), ctx, r);
    }
    Ok(GFunValue::Json(out.into_paths()))
}

// ---------------------------------------------------------------------------
// WCC
// ---------------------------------------------------------------------------

/// `WCC(preds)` — weakly connected components.
///
/// **Upstream:** GE:1889-1905 / GE:2955-2971; WCC.cpp:13-101.
///
/// **Output (once):** components as `vertVec2JSON` arrays, comma-separated
/// (same shape as [`label_prop`]).
///
/// **Algorithm:** for each vertex in node order not yet assigned, BFS over
/// in- and out-edges regardless of direction (`NbrOrder::PerPredInThenOut`),
/// assigning the next component id; every CSR vertex appears (vertices
/// without edges in `preds` are singletons). Components are ordered by id
/// (discovery). Members: upstream inserts every vertex into one global
/// `unordered_map<int,int> vid2wccid` in discovery order and then appends
/// each map element to its component **in map iteration order** — the
/// global [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder) order of all
/// discovered entity ids, bucketed by component.
pub fn wcc(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let g = ctx.csr;
    let f = &a.preds;
    let n = g.num_nodes();
    let mut comp = vec![u32::MAX; n];
    let mut order = CxxUnorderedOrder::new();
    let mut ncomp = 0u32;
    let mut queue: VecDeque<Node> = VecDeque::new();
    for start in 0..n as Node {
        tick(ctx, start as usize)?;
        if comp[start as usize] != u32::MAX {
            continue;
        }
        comp[start as usize] = ncomp;
        order.insert(i64::from(cxx_int_id(g.entity_of(start))));
        queue.push_back(start);
        while let Some(u) = queue.pop_front() {
            for_each_nbr(
                g,
                u,
                f,
                Direction::Both,
                NbrOrder::PerPredInThenOut,
                |v, _, _| {
                    if comp[v as usize] == u32::MAX {
                        comp[v as usize] = ncomp;
                        order.insert(i64::from(cxx_int_id(g.entity_of(v))));
                        queue.push_back(v);
                    }
                },
            );
        }
        ncomp += 1;
    }
    let mut res: Vec<Vec<EntityLiteralId>> = vec![Vec::new(); ncomp as usize];
    for key in order.keys() {
        let e = key as i32 as EntityLiteralId;
        if let Some(v) = g.node_of(e) {
            res[comp[v as usize] as usize].push(e);
        }
    }
    let mut out = JsonList::new();
    for r in &res {
        push_vert_list_json(out.item(), ctx, r);
    }
    Ok(GFunValue::Json(out.into_paths()))
}

// ---------------------------------------------------------------------------
// louvain
// ---------------------------------------------------------------------------

/// `louvain(directed, preds, maxIter=m, increase=x)` — Louvain communities.
///
/// **Upstream:** GE:1961-1991 / GE:3039-3069; louvain.cpp:14-268.
/// Arguments: `phase1_loop_num = f32_round(m) as i32`,
/// `min_modularity_increase = x as f32` (a `float` parameter, widened to
/// double where compared).
///
/// **Output (once):** `{"paths":[{"count":C, "community":[[0,1,5],[2,3,4]]}]}`
/// — `, "community":` with a space; members are **entity ids printed as
/// integers** (not IRIs); communities in lexicographic order of their sorted
/// member lists (`std::set<std::set<int>>`), separated by `,`. (The docs show
/// a `"details"` / `"communityId"` / `"menberNum"` shape — the code does not.)
///
/// **Algorithm:** literal port with `BTreeMap`s for upstream's `std::map`s:
/// vertices = those with ≥ 1 in/out edge in `preds`; edge weights from the
/// in-adjacency (`weights[src][dst] += 1` per predicate and edge, plus the
/// reverse when undirected; `m` = number of weight increments). Each round:
/// `s_in[c]` = Σ weights of edges inside community `c` (map order) and —
/// quirk — `s_tot[c]` = the weight of the **last** edge (in map order)
/// entering `c` (upstream assigns instead of adding). Phase 1 (up to
/// `phase1_loop_num` sweeps over the super-nodes ascending, stopping early
/// when a sweep moves nothing): for each super-node `i` with incoming weight
/// `k_i`, the community `c ≠ own` (ascending) maximising
/// `ΔQ = (k_i_in - s_tot[c]·k_i·2/m)/m` strictly above the threshold, where
/// `k_i_in` sums `i`'s weights into `c`; a move updates `s_in` / `s_tot`
/// with upstream's `int` truncations (`i_to_i`, `i_to_old`, …) and erases
/// an emptied community. The run ends when no community has more than one
/// member; otherwise phase 2 contracts every community into a super-node
/// (self-loop weight `s_in[c]`, inter-community weights summed) and the
/// next round starts. The result are the original vertices of each final
/// super-node.
///
/// **Bound:** phase 1 is `O(#super-nodes · #communities)` per sweep, as
/// upstream; `k_i` is precomputed per round (upstream recomputes it by
/// copying every weight map per super-node — same sum, same order).
pub fn louvain(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let kind = GFnKind::Louvain;
    let phase1_loop_num = d2i(f32_round(named(kind, a.max_iter, "maxIter")?));
    let min_inc = f64::from(named(kind, a.increase, "increase")? as f32);
    let result = louvain_communities(ctx, &a.preds, a.directed, phase1_loop_num, min_inc)?;
    let mut s = format!("{{\"count\":{}, \"community\":[", result.len());
    for (i, com) in result.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('[');
        for (j, &m) in com.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str(&cxx_int_id(m).to_string());
        }
        s.push(']');
    }
    s.push_str("]}");
    Ok(GFunValue::Json(paths_json(&s)))
}

type Weights = BTreeMap<u32, BTreeMap<u32, f64>>;

fn louvain_communities(
    ctx: &GFunCtx<'_>,
    f: &PredFilter,
    directed: bool,
    phase1_loop_num: i32,
    min_inc: f64,
) -> Result<BTreeSet<BTreeSet<EntityLiteralId>>> {
    let g = ctx.csr;
    let n = g.num_nodes();
    let vids: Vec<u32> = (0..n as Node)
        .filter(|&v| g.in_degree(v, f) + g.out_degree(v, f) > 0)
        .collect();
    let mut super_nodes: BTreeMap<u32, u32> = BTreeMap::new();
    let mut super2vid: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    let mut community: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    let mut weights: Weights = BTreeMap::new();
    for &v in &vids {
        super_nodes.insert(v, v);
        super2vid.insert(v, BTreeSet::from([v]));
        community.insert(v, BTreeSet::from([v]));
        weights.insert(v, BTreeMap::new());
    }
    let mut m = 0.0f64;
    for &p in f.preds() {
        for &vid in &vids {
            for &out_vid in g.in_nbrs(vid, p) {
                *weights
                    .entry(out_vid)
                    .or_default()
                    .entry(vid)
                    .or_insert(0.0) += 1.0;
                m += 1.0;
                if !directed {
                    *weights
                        .entry(vid)
                        .or_default()
                        .entry(out_vid)
                        .or_insert(0.0) += 1.0;
                    m += 1.0;
                }
            }
        }
    }
    let wget = |w: &Weights, a: u32, b: u32| w.get(&a).and_then(|x| x.get(&b)).copied();
    loop {
        ctx.check_cancel()?;
        let mut s_in: BTreeMap<u32, f64> = BTreeMap::new();
        let mut s_tot: BTreeMap<u32, f64> = BTreeMap::new();
        let mut k_in: HashMap<u32, f64> = HashMap::new();
        for (out_id, inner) in &weights {
            let oc = super_nodes.get(out_id).copied().unwrap_or(0);
            for (in_id, &w) in inner {
                let ic = super_nodes.get(in_id).copied().unwrap_or(0);
                if ic == oc {
                    *s_in.entry(ic).or_insert(0.0) += w;
                }
                s_tot.insert(ic, w);
                *k_in.entry(*in_id).or_insert(0.0) += w;
            }
        }
        for _ in 0..phase1_loop_num.max(0) {
            let mut phase1_finished = true;
            let keys: Vec<u32> = super_nodes.keys().copied().collect();
            for (step, i) in keys.into_iter().enumerate() {
                tick(ctx, step)?;
                let own = super_nodes[&i];
                let k_i = k_in.get(&i).copied().unwrap_or(0.0);
                let mut acc: HashMap<u32, f64> = HashMap::new();
                if let Some(wi) = weights.get(&i) {
                    for (node, &w) in wi {
                        let c = super_nodes.get(node).copied().unwrap_or(0);
                        *acc.entry(c).or_insert(0.0) += w;
                    }
                }
                let mut max_dq = min_inc;
                let mut max_c: Option<u32> = None;
                for &c in community.keys() {
                    if c == own {
                        continue;
                    }
                    let kin = acc.get(&c).copied().unwrap_or(0.0);
                    let st = s_tot.get(&c).copied().unwrap_or(0.0);
                    let dq = (kin - st * k_i * 2.0 / m) / m;
                    if dq > max_dq {
                        max_dq = dq;
                        max_c = Some(c);
                    }
                }
                let Some(mc) = max_c else {
                    continue;
                };
                phase1_finished = false;
                let old = own;
                let i_to_i = wget(&weights, i, i).map_or(0, d2i);
                if let Some(c) = community.get_mut(&old) {
                    c.remove(&i);
                }
                if community.get(&old).is_some_and(|c| !c.is_empty()) {
                    let (mut i_to_old, mut old_to_i) = (0i32, 0i32);
                    for &node in &community[&old] {
                        if let Some(w) = wget(&weights, i, node) {
                            i_to_old = d2i(f64::from(i_to_old) + w);
                        }
                        if let Some(w) = wget(&weights, node, i) {
                            old_to_i = d2i(f64::from(old_to_i) + w);
                        }
                    }
                    let si = s_in.entry(old).or_insert(0.0);
                    *si = *si - f64::from(i_to_old) - f64::from(old_to_i) - f64::from(i_to_i);
                    let st = s_tot.entry(old).or_insert(0.0);
                    *st -= k_i;
                } else {
                    s_in.remove(&old);
                    s_tot.remove(&old);
                    community.remove(&old);
                }
                let (mut i_to_new, mut new_to_i) = (0i32, 0i32);
                for &node in community.entry(mc).or_default().iter() {
                    if let Some(w) = wget(&weights, i, node) {
                        i_to_new = d2i(f64::from(i_to_new) + w);
                    }
                    if let Some(w) = wget(&weights, node, i) {
                        new_to_i = d2i(f64::from(new_to_i) + w);
                    }
                }
                let si = s_in.entry(mc).or_insert(0.0);
                *si = *si + f64::from(i_to_new) + f64::from(new_to_i) + f64::from(i_to_i);
                *s_tot.entry(mc).or_insert(0.0) += k_i;
                community.entry(mc).or_default().insert(i);
                super_nodes.insert(i, mc);
            }
            if phase1_finished {
                break;
            }
        }
        if !community.values().any(|c| c.len() > 1) {
            break;
        }
        // phase 2: contract the communities
        let mut new_super_nodes: BTreeMap<u32, u32> = BTreeMap::new();
        let mut new_community: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        let mut new_weights: Weights = BTreeMap::new();
        let mut new_super2vid: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for (&c, members) in &community {
            new_super_nodes.insert(c, c);
            new_community.insert(c, BTreeSet::from([c]));
            let self_w = s_in.get(&c).copied().unwrap_or(0.0);
            new_weights.insert(c, BTreeMap::from([(c, self_w)]));
            let mut vs = BTreeSet::new();
            for node in members {
                if let Some(s) = super2vid.get(node) {
                    vs.extend(s.iter().copied());
                }
            }
            new_super2vid.insert(c, vs);
        }
        for (out_id, inner) in &weights {
            let oc = super_nodes.get(out_id).copied().unwrap_or(0);
            for (in_id, &w) in inner {
                let ic = super_nodes.get(in_id).copied().unwrap_or(0);
                if ic != oc {
                    *new_weights.entry(oc).or_default().entry(ic).or_insert(0.0) += w;
                }
            }
        }
        super_nodes = new_super_nodes;
        community = new_community;
        weights = new_weights;
        super2vid = new_super2vid;
    }
    Ok(super2vid
        .values()
        .map(|s| s.iter().map(|&v| g.entity_of(v)).collect())
        .collect())
}

// ---------------------------------------------------------------------------
// JaccardSimilarity
// ---------------------------------------------------------------------------

/// `JaccardSimilarity(u, preds, k, retNum)` — the most Jaccard-similar
/// vertices.
///
/// **Upstream:** GE:2000-2003 / GE:3078-3081 → wrapper GE:4107-4123;
/// JaccardSimilarity.cpp:6-127.
///
/// **Per source:** missing `u` → nothing; `k' = if k < 0 { 999 } else { k }`,
/// `r = if retNum < 0 { 10 } else { retNum }`; entries
/// `{"dst":‹d›, "value":v}` — the IRI is **not quoted** (as in upstream code
/// and docs), a space after the comma, `v` a C `float` printed with
/// [`cxx_g_f32`](super::fmt::cxx_g_f32); comma-separated (upstream adds no
/// `,` between consecutive sources; we do).
///
/// **Algorithm:** `N(x)` = distinct in ∪ out neighbours over `preds`
/// (`std::set`). `N(u)` empty (also: isolated `u`) → nothing. Candidates:
/// every `w ∈ N(u)` (ascending), then — if `k' > 1` — every 2-hop vertex
/// collected in `no_used` (ascending). Score (f32): `num` = |N(w) ∩ N(u)|
/// (ignoring `u`), `total = |N(u)| + |N(w)|`, minus 1 if `w ∈ N(u)`, minus 1
/// if `u ∈ N(w)`; `js = if total - num > 0 { num / (total - num) } else { 0 }`.
/// Insert each candidate into a list kept sorted by descending score,
/// **before** existing equal scores (binary search, :30-45). If fewer than
/// `r` candidates, append the vertices of `kHopNeighbor(u, false, k', preds, r)`
/// (exactly `k'` hops; in-edges of all predicates first, then out-edges;
/// stops once `r` were collected) not seen yet with score 0. Output the
/// first `r`. A self-loop puts `u` itself among the candidates (kept).
///
/// **Example** (SPARQL_QUERY.md): `JaccardSimilarity(<Alice>, {}, 2, 10)` →
/// `{"paths":[{"dst":<Francis>, "value":0.5},{"dst":<Carol>, "value":0.5},{"dst":<Eve>, "value":0.5},{"dst":<Dave>, "value":0.333333},{"dst":<Bob>, "value":0.333333}]}`.
pub fn jaccard_similarity(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let k = a.hop_limit();
    let r = a.ret_num_or(10);
    let mut out = JsonList::new();
    for src in a.distinct_srcs() {
        ctx.check_cancel()?;
        let Some(u) = src.node else {
            continue; // missing: wrapper returns; isolated: N(u) is empty
        };
        for (v, js) in jaccard(ctx.csr, &a.preds, u, k, r) {
            let e = out.item();
            e.push_str("{\"dst\":");
            e.push_str(node_name(ctx, v));
            e.push_str(", \"value\":");
            e.push_str(&cxx_g_f32(js));
            e.push('}');
        }
    }
    Ok(GFunValue::Json(out.into_paths()))
}

fn jaccard(g: &CsrGraph, f: &PredFilter, u: Node, k: i64, ret_num: i64) -> Vec<(Node, f32)> {
    let uids = nbr_set(g, u, f);
    if uids.is_empty() {
        return Vec::new();
    }
    let in_uids = |x: Node| uids.binary_search(&x).is_ok();
    let binary_insert = |a: &mut Vec<(Node, f32)>, p: (Node, f32)| {
        let (mut l, mut r) = (0usize, a.len());
        while l < r {
            let loc = l + (r - l) / 2;
            if a[loc].1 > p.1 {
                l = loc + 1;
            } else {
                r = loc;
            }
        }
        a.insert(l, p);
    };
    let mut js_list: Vec<(Node, f32)> = Vec::new();
    let mut no_used: BTreeSet<Node> = BTreeSet::new();
    for &w in &uids {
        let comp = nbr_set(g, w, f);
        let mut num: f32 = 0.0;
        let mut total = (uids.len() + comp.len()) as f32;
        if in_uids(w) {
            total -= 1.0;
        }
        for &c in &comp {
            if c == u {
                total -= 1.0;
                continue;
            }
            if !in_uids(c) {
                no_used.insert(c);
                continue;
            }
            num += 1.0;
        }
        let js = if total - num > 0.0 {
            num / (total - num)
        } else {
            0.0
        };
        binary_insert(&mut js_list, (w, js));
    }
    if k > 1 {
        for &w in &no_used {
            let comp = nbr_set(g, w, f);
            let mut num: f32 = 0.0;
            let mut total = (uids.len() + comp.len()) as f32;
            if in_uids(w) {
                total -= 1.0;
            }
            for &c in &comp {
                if c == u {
                    total -= 1.0;
                    continue;
                }
                if !in_uids(c) {
                    continue;
                }
                num += 1.0;
            }
            let js = if total - num > 0.0 {
                num / (total - num)
            } else {
                0.0
            };
            binary_insert(&mut js_list, (w, js));
        }
    }
    if (js_list.len() as u64) < ret_num as u64 {
        let khop = k_hop_neighbor(g, u, false, k, f, ret_num);
        let mut others: BTreeSet<Node> = BTreeSet::new();
        for v in khop {
            if !in_uids(v) && !no_used.contains(&v) && others.insert(v) {
                js_list.push((v, 0.0));
            }
        }
    }
    js_list.truncate(ret_num.max(0) as usize);
    js_list
}

/// `PathQueryHandler::kHopNeighbor` (kHopNeighbor.cpp:12-76): the vertices
/// first reached at exactly `k` hops, level-synchronous BFS expanding (when
/// undirected) all predicates' in-edges, then all out-edges; returns as
/// soon as `retNum >= 0` vertices were collected (note `retNum = 0` still
/// returns the first one). `k == 0` → empty.
fn k_hop_neighbor(
    g: &CsrGraph,
    s: Node,
    directed: bool,
    k: i64,
    f: &PredFilter,
    ret_num: i64,
) -> Vec<Node> {
    let mut cnt: Vec<Node> = Vec::new();
    let mut visited = vec![false; g.num_nodes()];
    if s as usize >= visited.len() {
        return cnt;
    }
    let mut cur: Vec<Node> = vec![s];
    let mut next: Vec<Node> = Vec::new();
    visited[s as usize] = true;
    let mut k = k;
    while k != 0 && !cur.is_empty() {
        for &v in &cur {
            for d in usize::from(directed)..=1 {
                let mut hit = |n: Node| -> bool {
                    if !visited[n as usize] {
                        if k == 1 {
                            cnt.push(n);
                            if ret_num >= 0 && cnt.len() as u64 >= ret_num as u64 {
                                return true;
                            }
                        } else {
                            next.push(n);
                        }
                        visited[n as usize] = true;
                    }
                    false
                };
                let stop = if d == 1 {
                    g.try_for_each_out(v, f, |n, _| {
                        if hit(n) {
                            std::ops::ControlFlow::Break(())
                        } else {
                            std::ops::ControlFlow::Continue(())
                        }
                    })
                } else {
                    g.try_for_each_in(v, f, |n, _| {
                        if hit(n) {
                            std::ops::ControlFlow::Break(())
                        } else {
                            std::ops::ControlFlow::Continue(())
                        }
                    })
                };
                if stop.is_break() {
                    return cnt;
                }
            }
        }
        k -= 1;
        std::mem::swap(&mut cur, &mut next);
        next.clear();
    }
    cnt
}

// ---------------------------------------------------------------------------
// degreeCorrelation
// ---------------------------------------------------------------------------

/// `degreeCorrelation(u, k, preds)` — degree assortativity around `u`.
///
/// **Upstream:** GE:2004-2007 / GE:3082-3085 → wrapper GE:4125-4132;
/// degreeCorrelation.cpp:5-121.
///
/// **Per source:** missing `u` → nothing; `k' = if k < 0 { 999 } else { k }`;
/// `{"src":"‹u›", "result":x}` (`ss << double`; separator rule applies).
///
/// **Value:** empty `preds` → 0. BFS from `u` over out- and in-edges within
/// `k'` hops (distances via a `std::map`, neighbour candidates via a
/// `std::set`), recording for each dequeued vertex its degree (out + in with
/// multiplicity); `|visited| <= 1` → 0 (also for an isolated `u`). Then for
/// each visited vertex `x` (ascending) and each out-edge `x → y` (filter
/// order, then neighbour order) with recorded degree `deg(y) != 0`, collect
/// the pair `(deg(x), deg(y))`; the result is the Pearson correlation of the
/// pairs (`s1·s2 == 0` → 0), summed in that order.
///
/// **Example** (SPARQL_QUERY.md): `degreeCorrelation(<Alice>, -1, {})` →
/// `{"paths":[{"src":"<Alice>", "result":0.0429273}]}`.
pub fn degree_correlation(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let k = a.hop_limit();
    let mut out = JsonList::new();
    for src in a.distinct_srcs() {
        ctx.check_cancel()?;
        if src.is_missing() {
            continue;
        }
        let x = match src.node {
            Some(u) if !a.preds.is_empty() => degree_correlation_value(ctx.csr, &a.preds, u, k),
            _ => 0.0,
        };
        let e = out.item();
        e.push_str("{\"src\":");
        push_json_quoted(e, ctx.gnode_str(&src));
        e.push_str(", \"result\":");
        e.push_str(&cxx_g(x));
        e.push('}');
    }
    Ok(GFunValue::Json(out.into_paths()))
}

fn degree_correlation_value(g: &CsrGraph, f: &PredFilter, u: Node, k: i64) -> f64 {
    let n = g.num_nodes();
    let mut dis: Vec<i64> = vec![-1; n];
    let mut deg: Vec<i32> = vec![0; n];
    let mut visited = NodeMarks::new(n);
    let mut visited_list: Vec<Node> = Vec::new();
    let mut q: VecDeque<Node> = VecDeque::new();
    q.push_back(u);
    dis[u as usize] = 0;
    visited.mark(u);
    visited_list.push(u);
    let mut nl: Vec<Node> = Vec::new();
    while let Some(t) = q.pop_front() {
        nl.clear();
        let distance = dis[t as usize] + 1;
        let mut degree_num = 0i32;
        g.for_each_both(t, f, |x, _, _| {
            degree_num = degree_num.wrapping_add(1);
            nl.push(x);
            let dx = &mut dis[x as usize];
            if *dx == -1 || *dx > distance {
                *dx = distance;
            }
        });
        deg[t as usize] = deg[t as usize].wrapping_add(degree_num);
        if dis[t as usize] <= k {
            nl.sort_unstable();
            nl.dedup();
            for &m in &nl {
                if !visited.is_marked(m) && dis[m as usize] <= k {
                    visited.mark(m);
                    visited_list.push(m);
                    q.push_back(m);
                }
            }
        }
    }
    if visited_list.len() <= 1 {
        return 0.0;
    }
    visited_list.sort_unstable();
    let mut pairs: Vec<(f64, f64)> = Vec::new();
    let (mut avg1, mut avg2) = (0.0f64, 0.0f64);
    for &x in &visited_list {
        let du = f64::from(deg[x as usize]);
        g.for_each_out(x, f, |y, _| {
            let dt = f64::from(deg[y as usize]);
            if dt == 0.0 {
                return;
            }
            pairs.push((du, dt));
            avg1 += du;
            avg2 += dt;
        });
    }
    avg1 /= pairs.len() as f64;
    avg2 /= pairs.len() as f64;
    let (mut s1, mut s2, mut ret) = (0.0f64, 0.0f64, 0.0f64);
    for &(a, b) in &pairs {
        // GCC folds `pow(x, 2)` into `x * x`.
        s1 += (a - avg1) * (a - avg1);
        s2 += (b - avg2) * (b - avg2);
        ret += (a - avg1) * (b - avg2);
    }
    // Upstream calls glibc `pow(s, 0.5)` (GCC does not turn it into `sqrt`
    // without -ffast-math); hide the constant from LLVM, which would.
    let half = std::hint::black_box(0.5f64);
    s1 = s1.powf(half);
    s2 = s2.powf(half);
    if s1 * s2 == 0.0 {
        0.0
    } else {
        ret / (s1 * s2)
    }
}

// ---------------------------------------------------------------------------
// diameterEstimation
// ---------------------------------------------------------------------------

/// `diameterEstimation(preds)` — the longest shortest-path length found.
///
/// **Upstream:** GE:1992-1995 / GE:3070-3073 → wrapper GE:4093-4097;
/// diameterEstimation.cpp:5-90.
///
/// **Output (once):** `{"paths":[D]}` (C `unsigned`).
///
/// **D:** for every vertex with at least one out-edge in `preds`
/// (ascending), a BFS along **out-edges only** (always directed); `D` = the
/// largest distance seen; 0 for an edgeless graph. (`O(n · (n + m))`, as
/// upstream.)
///
/// **Example** (SPARQL_QUERY.md): `{"paths":[4]}` for the sample graph over
/// its own predicates (the doc's `<likes>`, `<follows>`, `<dislikes>` do
/// not occur in the sample data and would give 0).
pub fn diameter_estimation(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let g = ctx.csr;
    let f = &a.preds;
    let n = g.num_nodes();
    let mut best: u32 = 0;
    let mut marks = NodeMarks::new(n);
    let mut queue: VecDeque<(Node, u32)> = VecDeque::new();
    for s in 0..n as Node {
        if g.out_degree(s, f) == 0 {
            continue;
        }
        ctx.check_cancel()?;
        marks.clear();
        marks.mark(s);
        queue.push_back((s, 0));
        while let Some((v, d)) = queue.pop_front() {
            g.for_each_out(v, f, |t, _| {
                if marks.mark(t) {
                    best = best.max(d + 1);
                    queue.push_back((t, d + 1));
                }
            });
        }
    }
    Ok(GFunValue::Json(paths_json(&best.to_string())))
}

// ---------------------------------------------------------------------------
// confidenceDegree
// ---------------------------------------------------------------------------

/// `confidenceDegree(w1, …, w6)` — per-vertex closeness-like score.
///
/// **Upstream:** GE:1538-1552 / GE:2531-2546; ConfidenceDegree.cpp:5-45. The
/// six weights are parsed but ignored (QueryParser.cpp:569 tests
/// `tmp == "DEGREE"`, never true); there is no `predSet`, so `preds` is
/// [`PredFilter::all`](crate::analytics::csr::PredFilter::all).
///
/// **Output:** [`GFunValue::ResultsJson`] with the `nlohmann::json` dump of
/// `[[‹v›, x], …]` for every vertex in node order, e.g.
/// `[["<Alice>",0.5],["<Bob>",1.0]]` — strings escaped like nlohmann
/// ([`push_json_quoted`](super::fmt::push_json_quoted)), numbers via
/// [`nlohmann_dump_f64`] (an exact Grisu2 port, which
/// [`fmt::nlohmann_f64`](super::fmt::nlohmann_f64) delegates to: nlohmann's
/// digits are not always the shortest round-trip form, e.g.
/// `0.27559055118110237`). **Spec fix:** an empty graph dumps the
/// never-assigned `nlohmann::json` value: `null`.
///
/// **x:** undirected BFS from `v` (per predicate: out, then in); `cnt` =
/// reached vertices including `v`, `sum` = Σ distances (C `unsigned`);
/// `(cnt - 1) / sum`; NaN (isolated vertex) → `0.0`. Upstream iterates
/// predicate ids `0..preds.len()` instead of the list itself — identical for
/// the all-predicates list. (`O(n · (n + m))`, as upstream.)
pub fn confidence_degree(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let g = ctx.csr;
    let n = g.num_nodes();
    if n == 0 {
        return Ok(GFunValue::ResultsJson("null".to_string()));
    }
    let mut marks = NodeMarks::new(n);
    let mut queue: VecDeque<(Node, u32)> = VecDeque::new();
    let mut s = String::from("[");
    for v in 0..n as Node {
        ctx.check_cancel()?;
        marks.clear();
        marks.mark(v);
        queue.push_back((v, 0));
        let (mut cnt, mut sum) = (0u32, 0u32);
        while let Some((x, d)) = queue.pop_front() {
            cnt = cnt.wrapping_add(1);
            g.for_each_both(x, &a.preds, |t, _, _| {
                if marks.mark(t) {
                    sum = sum.wrapping_add(d + 1);
                    queue.push_back((t, d + 1));
                }
            });
        }
        let mut res = f64::from(cnt.wrapping_sub(1)) / f64::from(sum);
        if res.is_nan() {
            res = 0.0;
        }
        if v > 0 {
            s.push(',');
        }
        s.push('[');
        push_json_quoted(&mut s, node_name(ctx, v));
        s.push(',');
        s.push_str(&nlohmann_dump_f64(res));
        s.push(']');
    }
    s.push(']');
    Ok(GFunValue::ResultsJson(s))
}

// ---------------------------------------------------------------------------
// maximumKplex / coreTruss
// ---------------------------------------------------------------------------

/// `maximumKplex(preds [, k])` — a maximum k-plex.
///
/// **Upstream:** GE:1580-1597 / GE:2634-2651 (written once, before the pair
/// loop); `PathQueryHandler::maximumKplex` (PathQueryHandler.cpp:3012-3017)
/// over namespace `MaxKPX` (PathQueryHandler.cpp:298-3011). `k = a.k`,
/// default 1 (QueryParser.cpp:532-536). Ported in the private `kplex` module.
///
/// **Output:** `{"paths":[{"kplex":[‹a›,‹b›,…]}]}` — IRIs **unquoted**
/// (`ss << getStringByID(vid)`), in the order of `graph.kplex`.
///
/// **Algorithm:** literal port of `MaxKPX::Graph::read` (an undirected simple
/// graph from every edge in `preds`, both directions, sorted, self-loops and
/// parallel edges removed, PathQueryHandler.cpp:1696-1743) and
/// `Graph::search` (:1745-1977: max-degree heuristic, degeneracy ordering,
/// core shrinking, oriented triangle counting, core-truss co-pruning and the
/// branch-and-bound `Searcher` per ego network, :489-1565).
///
/// **Quirks kept** (see the `kplex` module docs for the full list):
/// `search` starts from `kplex.resize(2k - 2)` — a fake solution of `2k - 2`
/// copies of vertex 0, printed as such when no larger k-plex is found (e.g.
/// `k = 2` on a graph without edges in `preds` → `[‹v0›,‹v0›]`); the ego
/// network ids are mapped through an array the pruning step overwrote, so
/// the reported set can contain repeats or non-members.
///
/// **Errors:** `k = 0` (upstream throws in `resize(-2)`), a `k` whose fake
/// solution exceeds 2^20 slots, an ego network beyond 8192 vertices (the
/// `Searcher` allocates `n²` matrices), and the upstream `assert`s that fail
/// on some graphs (gStore aborts there).
pub fn maximum_kplex(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let k = a.k.unwrap_or(1);
    let k = i32::try_from(k)
        .map_err(|_| GStoreError::Query(format!("maximumKplex: k out of range ({k})")))?;
    let res = kplex::maximum_kplex(ctx, &a.preds, k)?;
    Ok(GFunValue::Json(paths_json(&vertex_list_unquoted(
        ctx, "kplex", &res,
    ))))
}

/// `coreTruss(preds, alpha, beta)` — the (alpha-core, beta-truss) remainder.
///
/// **Upstream:** GE:1598-1610 / GE:2652-2664 (once, before the pair loop);
/// `PathQueryHandler::coreTruss` (PathQueryHandler.cpp:3019-3023) →
/// `MaxKPX::Graph::peel` (PathQueryHandler.cpp:1979-2066) with `K = 1`.
/// `alpha = a.nums[0]`, `beta = a.nums[1]` as integers (missing → 0).
///
/// **Output:** `{"paths":[{"coreTruss":[‹a›,‹b›,…]}]}` — IRIs **unquoted**.
///
/// **Algorithm:** literal port of `peel`: build the simple undirected graph
/// as for [`maximum_kplex`]; degeneracy ordering; `shrink_graph` (keep
/// vertices with core number `>= alpha`); oriented triangle counting;
/// `peeling` removes vertices of degree `< alpha` and edges in fewer than
/// `beta` triangles until both hold; output the remaining vertices with
/// non-zero degree in linear-heap pop order (ascending degree, last touched
/// first), mapped back to the original vertices.
///
/// **Quirk kept:** `peel` pops `n` times from a heap that lost the peeled
/// vertices; every failed pop reads the uninitialised `int u`, which the
/// g++ -O2 build materialises as 0 — so the shrunk graph's vertex 0 is
/// appended once per peeled vertex while it has edges.
///
/// **Spec fix:** upstream asserts a graph with edges in `preds`
/// (`assert(n>0 && m>0)`; gStore's default build keeps asserts, so it
/// aborts); the port returns `{"coreTruss":[]}`. Negative `alpha` / `beta`
/// are errors (upstream throws in `kplex.resize(alpha)`).
pub fn core_truss(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let num = |i: usize| a.nums.get(i).map_or(0, |&x| d2i(x));
    let res = kplex::core_truss(ctx, &a.preds, num(0), num(1))?;
    Ok(GFunValue::Json(paths_json(&vertex_list_unquoted(
        ctx,
        "coreTruss",
        &res,
    ))))
}

/// `{"<key>":[a,b,…]}` with the vertices' strings spliced in unquoted.
/// Vertex ids are upstream vertex ids (= node indices); an id without a node
/// (the fake `0` of a graph without CSR vertices) prints entity id `v`'s
/// string, as upstream's `getStringByID(v)` does.
fn vertex_list_unquoted(ctx: &GFunCtx<'_>, key: &str, vs: &[i32]) -> String {
    let mut s = format!("{{\"{key}\":[");
    for (i, &v) in vs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let name = if v >= 0 && (v as usize) < ctx.csr.num_nodes() {
            node_name(ctx, v as Node)
        } else {
            ctx.entity_str(v as u32)
        };
        s.push_str(name);
    }
    s.push_str("]}");
    s
}

// ---------------------------------------------------------------------------
// SIMILARITY (Jaro)
// ---------------------------------------------------------------------------

/// `SIMILARITY(a, b)` — upstream's Jaro string similarity
/// (`TempResult::doSimilarity`, TempResult.cpp:784-845; used by FILTER
/// `SIMILARITY`, TempResult.cpp:1278-1288, whose value is an `xsd:decimal`).
///
/// Ported exactly, including its deviations from textbook Jaro: strings are
/// compared as **bytes** (C++ `std::string`); equal strings → 1; either empty
/// → 0; the longer string is `s`; if `s` contains `t` →
/// `(|t|/|s| + 1 + 1) / 3`; the match window is `⌊|t|/2⌋ - 1` (no `max(…)`);
/// a match takes the first unmatched `t[j]` in the window; transpositions are
/// counted between the match *ranks* and halved with integer division. All
/// arithmetic is in `f32` (as upstream), widened to `f64` at the end.
pub fn jaro_similarity(a: &str, b: &str) -> f64 {
    f64::from(jaro_f32(a.as_bytes(), b.as_bytes()))
}

fn jaro_f32(s: &[u8], t: &[u8]) -> f32 {
    if s == t {
        return 1.0;
    }
    let (s_len, t_len) = (s.len() as i64, t.len() as i64);
    if s_len == 0 || t_len == 0 {
        return 0.0;
    }
    if s_len < t_len {
        return jaro_f32(t, s);
    }
    if s.windows(t.len()).any(|w| w == t) {
        // Upstream: `((float)t_len/(float)s_len + (float)t_len/(float)t_len + 1)/3`;
        // `x / x` is exactly 1.0 in IEEE arithmetic for the non-zero `t_len`.
        return (t_len as f32 / s_len as f32 + 1.0 + 1.0) / 3.0;
    }
    let match_dt = t_len / 2 - 1;
    let mut sv = vec![-1i64; s.len()];
    let mut tv = vec![-1i64; t.len()];
    let mut m: i64 = 0;
    for i in 0..s_len {
        if i - match_dt > t_len {
            break;
        }
        for j in 0..t_len {
            if j < i - match_dt || j > i + match_dt {
                continue;
            }
            if s[i as usize] == t[j as usize] && tv[j as usize] == -1 {
                sv[i as usize] = m;
                tv[j as usize] = m;
                m += 1;
                break;
            }
        }
    }
    if m == 0 {
        return 0.0;
    }
    let sm: Vec<i64> = sv.into_iter().filter(|&x| x != -1).collect();
    let tm: Vec<i64> = tv.into_iter().filter(|&x| x != -1).collect();
    let num = sm.iter().zip(&tm).filter(|(x, y)| x != y).count() as i64 / 2;
    let mf = m as f32;
    (mf / s_len as f32 + mf / t_len as f32 + (mf - num as f32) / mf) / 3.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::excessive_precision)] // goldens as C printed them (%.9g)
    fn jaro_matches_upstream_float_results() {
        // Values printed (%.9g) by the upstream function compiled with g++ 13.
        let cases: &[(&str, &str, f32)] = &[
            ("MARTHA", "MARHTA", 0.944444418),
            ("DIXON", "DICKSONX", 0.550000012),
            ("abc", "abc", 1.0),
            ("abcdef", "bcd", 0.833333313),
            ("", "", 1.0),
            ("a", "", 0.0),
            ("\"hello\"", "\"world\"", 0.619047642),
            ("<http://a/b>", "<http://a/c>", 0.944444478),
            ("jellyfish", "smellyfish", 0.896296322),
            ("CRATE", "TRACE", 0.733333349),
            ("x", "y", 0.0),
            ("ab", "ba", 0.0),
        ];
        for &(a, b, want) in cases {
            assert_eq!(jaro_similarity(a, b), f64::from(want), "{a:?} vs {b:?}");
            assert_eq!(jaro_similarity(b, a), f64::from(want), "symmetric {a:?}");
        }
    }
}
