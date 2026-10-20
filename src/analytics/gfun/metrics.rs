//! Centrality, ranking, community, similarity and structure graph functions
//! (stubs + upstream specifications), plus the Jaro string similarity behind
//! `SIMILARITY(a, b)`.
//!
//! Spec conventions are those of [`paths`](super::paths) ("GE:n", entries,
//! `‹u›`, literal ports). Whole-graph functions run once per call regardless
//! of bindings (upstream's `doneOnceOp`, GE:1575) and iterate vertices in
//! node order (= upstream's `for (vid = 0; vid < getVertNum(); ++vid)` when
//! entity ids are dense, see [`csr`](crate::analytics::csr) "Node ids").
//!
//! Numbers: `ss << double` → [`cxx_g`](super::fmt::cxx_g),
//! `to_string(double)` → [`cxx_to_string`](super::fmt::cxx_to_string),
//! `float` → [`cxx_g_f32`](super::fmt::cxx_g_f32). Parameters upstream reads
//! with `stof` (`alpha`, `tol`, `increase`, `maxIter`) must go through
//! [`f32_round`](super::fmt::f32_round).

use super::{not_implemented, GArgs, GFnKind, GFunCtx, GFunValue};
use crate::error::Result;

/// The fixed seed ports use for FORA's random walks ([`ppr`]): upstream calls
/// `srand(time(NULL))` (SSPPR.cpp:100), which is not reproducible; ours uses
/// [`GlibcRand::new(PPR_SEED)`](super::cxx::GlibcRand) (glibc's default
/// state, as if `srand` had never been called).
pub const PPR_SEED: u32 = 1;

/// The seed upstream's `labelProp` passes to `std::srand` (labelProp.cpp:173-176).
pub const LABEL_PROP_SEED: u32 = 114_514;

/// `PPR(u, hopCnt, preds, retNum)` — top-`retNum` personalized PageRank from
/// `u` (FORA, approximate).
///
/// **Upstream:** GE:1770-1783 / GE:2824-2841; `PathQueryHandler::SSPPR`
/// (SSPPR.cpp:96-191) with its helpers (SSPPR.cpp:5-94,
/// PathQueryHandler.cpp:3025-3337).
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
/// `rand() % out_degree` picked across predicates in filter order, PathQueryHandler.cpp:3118-3152) with
/// [`GlibcRand::new(PPR_SEED)`](super::cxx::GlibcRand) — upstream's result
/// differs run to run (time seed; the docs say so), ours is deterministic.
/// A source without out-edges keeps all mass: `{u: 1}` (SSPPR.cpp:152-158).
///
/// **Top-k extraction** (SSPPR.cpp:178-190): `topk.resize(retNum)`, then
/// `partial_sort_copy` of the `unordered_map<int,double>` `v2ppr` (iterate
/// it in [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder) order of its
/// insertion sequence) by value descending into `topk`
/// ([`cxx_partial_sort_copy_by`](super::cxx::cxx_partial_sort_copy_by));
/// unfilled slots stay `(0, 0.0)`; then trailing entries with value `0` are
/// dropped but **at least one entry is kept** — an all-zero result prints
/// vertex id 0 with `"PPR":0` (quirk). `retNum <= 0` → `"results":[]`
/// (negative is UB upstream). Missing `u` → no entry.
pub fn ppr(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::Ppr))
}

/// `PR(directed, preds, retNum, alpha=a, maxIter=m, tol=t)` — PageRank.
///
/// **Upstream:** GE:1812-1831 / GE:2878-2897; PR.cpp:15-91. Arguments:
/// `alpha = f32_round(a)`, `tol = f32_round(t)`, `maxIter = f32_round(m) as i32`,
/// `retNum = a.ret_num`.
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
///
/// **Selection:** `retNum > n` is clamped to `n`. If `retNum == -1` or
/// `retNum >= n`: all vertices in node order (unsorted). Otherwise
/// (`0 <= retNum < n`): libstdc++ `std::sort` by value descending —
/// [`cxx_sort_by`](super::cxx::cxx_sort_by), which reproduces its order for
/// ties — and keep the first `retNum`. `retNum < -1` is UB upstream; treat it
/// as `-1`. Empty graph → `{"paths":[]}`.
pub fn pr(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::Pr))
}

/// `triangleCounting(directed, preds)` — number of triangles.
///
/// **Upstream:** GE:1784-1792 / GE:2842-2850; triangleCounting.cpp:14-105.
///
/// **Output (once):** `{"paths":[N]}` (C `long long`).
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
/// self-loops) upstream returns **-1** — reproduce.
///
/// **Example** (SPARQL_QUERY.md): `triangleCounting(true, {<like>})` →
/// `{"paths":[1]}`.
pub fn triangle_counting(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::TriangleCounting))
}

/// `closenessCentrality(u, directed, preds)`.
///
/// **Upstream:** GE:1793-1798 / GE:2851-2860; closenessCentrality.cpp:17-56.
///
/// **Per source** (separated by `,`): `{"src":"‹u›","result":x}` with `x`
/// printed by **`to_string`** (6 decimals: `0.555556`, `0.500000`).
///
/// **Value:** BFS from `u` (`NbrOrder::PerPredOutThenIn`), `cnt` = reached
/// vertices including `u`, `sum` = Σ distances (C `int`):
/// `(cnt - 1) / sum` as double. An isolated / missing `u` gives `0.0/0.0`,
/// which on x86-64 is a *negative* NaN → `"result":-nan` (pass `-f64::NAN`).
///
/// **Examples** (SPARQL_QUERY.md): `closenessCentrality(<Alice>, false, {<like>, <focus>})`
/// → `{"paths":[{"src":"<Alice>","result":0.555556}]}`; with `true` →
/// `"result":0.500000`.
pub fn closeness_centrality(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::ClosenessCentrality))
}

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
/// from `s` recording all shortest-path predecessors (`dis_pre`, as in
/// [`shortest_paths`](super::paths::shortest_paths)); for each target `t`
/// (ascending, `dist(t) > 1`, `t ∉ {s, u}`) whose predecessor DFS passes `u`,
/// add `pass/total`, where each vertex path counts with the product over its
/// hops of the number of distinct predicates joining the hop (out, plus in
/// when undirected) and `pass` sums the paths containing `u` (C `int`
/// arithmetic).
///
/// **Example** (SPARQL_QUERY.md): `betweennessCentrality(<Eve>, true, {…})`
/// → `{"paths":[{"src":"<Eve>", "result":6.5}]}` (the doc example uses
/// predicates absent from the sample data; check against a real run).
pub fn betweenness_centrality(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::BetweennessCentrality))
}

/// `clusterCoeff([u,] directed, preds)` — local or global clustering
/// coefficient.
///
/// **Upstream:** GE:1906-1923 / GE:2972-2989; clusteringCoeff.cpp:12-193.
///
/// * **No source** (`srcs` empty) — and also a **missing** constant source,
///   because upstream tests `uid == -1` for "not given" (quirk): once,
///   `{"paths":[x]}` with `x = to_string(global)`, e.g. `{"paths":[0.333333]}`.
/// * **Per source:** `{"src":"‹u›","result":x}`, `x = to_string(local)`.
///   Upstream writes consecutive sources without `,`; we separate them.
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
/// A zero denominator gives 0.
pub fn cluster_coeff(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::ClusterCoeff))
}

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
/// predicate (`NbrOrder::PerPredInThenOut`, with multiplicity); collect the
/// labels with the maximal count in ascending label order; if the maximum is
/// non-zero pick `maxLbl[rand() % maxNum]` (one `rand()` per such vertex).
/// Then the modularity `Q` with `dblM = 2 · n` (upstream uses the **vertex**
/// count as edge count — keep), and `maxLabels = labels` whenever `Q > -1`
/// (`maxQ` is never updated — keep). Stop after `maxIter` iterations or when
/// the community count and `|ΔQ| < 0.001` stayed stable for more than 10
/// consecutive iterations (`tol`/`thres`, :173-196).
///
/// **Order:** communities by label ascending (`std::set` of labels), members
/// in node order. Vertices without edges in `preds` keep their own label.
/// `maxIter <= 0`: upstream reads an empty `maxLabels` (UB) — return every
/// vertex as its own community, in node order.
pub fn label_prop(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::LabelProp))
}

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
/// each map element to its component **in map iteration order** — compute
/// the global [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder) order of
/// all discovered entity ids once and bucket it by component.
pub fn wcc(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::Wcc))
}

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
/// member lists (`std::set<std::set<int>>` — a
/// `BTreeSet<BTreeSet<EntityLiteralId>>`), separated by `,`. (The docs show a
/// `"details"` / `"communityId"` / `"menberNum"` shape — the code does not.)
///
/// **Algorithm:** literal port with `BTreeMap`s for upstream's `std::map`s:
/// vertices = those with ≥ 1 in/out edge in `preds`; edge weights from the
/// in-adjacency (`weights[src][dst] += 1`, plus the reverse when undirected;
/// `m` = number of weight increments); phase 1 moves each super-node to the
/// neighbouring community with the best `ΔQ = (k_i_in - s_tot·k_i·2/m)/m`
/// above the threshold (several `int` truncations of double weights —
/// `i_to_i`, `i_to_old`, … — keep them), up to `phase1_loop_num` sweeps;
/// phase 2 contracts communities; stop when no community has more than one
/// member or phase 1 changes nothing.
pub fn louvain(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::Louvain))
}

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
/// (`std::set`). `N(u)` empty → nothing. Candidates: every `w ∈ N(u)`
/// (ascending), then — if `k' > 1` — every 2-hop vertex collected in
/// `no_used` (ascending). Score (f32): `num` = |N(w) ∩ N(u)| (ignoring `u`),
/// `total = |N(u)| + |N(w)|`, minus 1 if `w ∈ N(u)`, minus 1 if `u ∈ N(w)`;
/// `js = if total - num > 0 { num / (total - num) } else { 0 }`. Insert each
/// candidate into a list kept sorted by descending score, **before** existing
/// equal scores (binary search, :30-45). If fewer than `r` candidates, append
/// the vertices of `kHopNeighbor(u, false, k', preds, r)` (exactly `k'` hops)
/// not seen yet with score 0. Output the first `r`.
///
/// **Example** (SPARQL_QUERY.md): `JaccardSimilarity(<Alice>, {}, 2, 10)` →
/// `{"paths":[{"dst":<Francis>, "value":0.5},{"dst":<Carol>, "value":0.5},{"dst":<Eve>, "value":0.5},{"dst":<Dave>, "value":0.333333},{"dst":<Bob>, "value":0.333333}]}`.
pub fn jaccard_similarity(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::JaccardSimilarity))
}

/// `degreeCorrelation(u, k, preds)` — degree assortativity around `u`.
///
/// **Upstream:** GE:2004-2007 / GE:3082-3085 → wrapper GE:4125-4132;
/// degreeCorrelation.cpp:5-121.
///
/// **Per source:** missing `u` → nothing; `k' = if k < 0 { 999 } else { k }`;
/// `{"src":"‹u›", "result":x}` (`ss << double`; separator rule applies).
///
/// **Value:** empty `preds` → 0. BFS from `u` over in- and out-edges within
/// `k'` hops (distances via a `std::map`, neighbour candidates via a
/// `std::set`), recording for each dequeued vertex its degree (out + in with
/// multiplicity); `|visited| <= 1` → 0. Then for each visited vertex `x`
/// (ascending) and each out-edge `x → y` with recorded degree `deg(y) != 0`,
/// collect the pair `(deg(x), deg(y))`; the result is the Pearson correlation
/// of the pairs (`s1·s2 == 0` → 0), summed in that order.
///
/// **Example** (SPARQL_QUERY.md): `degreeCorrelation(<Alice>, -1, {})` →
/// `{"paths":[{"src":"<Alice>", "result":0.0429273}]}`.
pub fn degree_correlation(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::DegreeCorrelation))
}

/// `diameterEstimation(preds)` — the longest shortest-path length found.
///
/// **Upstream:** GE:1992-1995 / GE:3070-3073 → wrapper GE:4093-4097;
/// diameterEstimation.cpp:5-90.
///
/// **Output (once):** `{"paths":[D]}` (C `unsigned`).
///
/// **D:** for every vertex with at least one out-edge in `preds`
/// (ascending), a BFS along **out-edges only** (always directed); `D` = the
/// largest distance seen; 0 for an edgeless graph.
pub fn diameter_estimation(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::DiameterEstimation))
}

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
/// [`nlohmann_f64`](super::fmt::nlohmann_f64).
///
/// **x:** undirected BFS from `v` (per predicate: out, then in); `cnt` =
/// reached vertices including `v`, `sum` = Σ distances (C `unsigned`);
/// `(cnt - 1) / sum`; NaN (isolated vertex) → `0.0`. Upstream iterates
/// predicate ids `0..preds.len()` instead of the list itself — identical for
/// the all-predicates list.
pub fn confidence_degree(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::ConfidenceDegree))
}

/// `maximumKplex(preds [, k])` — a maximum k-plex.
///
/// **Upstream:** GE:1580-1597 / GE:2634-2651 (written before the pair loop,
/// once); `PathQueryHandler::maximumKplex` (PathQueryHandler.cpp:3012-3017)
/// over namespace `MaxKPX` (PathQueryHandler.cpp:298-3011). `k = a.k`,
/// default 1 (QueryParser.cpp:532-536).
///
/// **Output:** `{"paths":[{"kplex":[‹a›,‹b›,…]}]}` — IRIs **unquoted**
/// (`ss << getStringByID(vid)`), in the order of `graph.kplex`.
///
/// **Algorithm:** literal port of `MaxKPX::Graph::read` (an undirected simple
/// graph from every edge in `preds`, both directions, sorted, self-loops and
/// parallel edges removed, PathQueryHandler.cpp:1717-1745) and
/// `Graph::search` (heuristic + degeneracy ordering + the branch-and-bound
/// `Searcher`).
pub fn maximum_kplex(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::MaximumKplex))
}

/// `coreTruss(preds, alpha, beta)` — the (alpha-core, beta-truss) remainder.
///
/// **Upstream:** GE:1598-1610 / GE:2652-2664 (once, before the pair loop);
/// `PathQueryHandler::coreTruss` (PathQueryHandler.cpp:3019-3023) →
/// `MaxKPX::Graph::peel` (PathQueryHandler.cpp:1979-2066).
/// `alpha = a.nums[0]`, `beta = a.nums[1]` as integers.
///
/// **Output:** `{"paths":[{"coreTruss":[‹a›,‹b›,…]}]}` — IRIs **unquoted**.
///
/// **Algorithm:** literal port of `peel`: build the simple undirected graph
/// as for [`maximum_kplex`]; degeneracy ordering; `shrink_graph`; oriented
/// triangle counting; `peeling` removes vertices of degree `< alpha` and
/// edges with triangle support `< beta`; output the remaining vertices with
/// non-zero degree in linear-heap pop order, mapped back to entity ids.
/// Upstream asserts a non-empty graph (`assert(n>0 && m>0)`, UB in release
/// builds) — an empty one yields `{"coreTruss":[]}`.
pub fn core_truss(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::CoreTruss))
}

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
