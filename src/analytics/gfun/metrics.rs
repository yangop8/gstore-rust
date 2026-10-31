//! Metric-family graph functions: (personalised) PageRank, centralities,
//! clustering, communities, similarity, degree correlation, diameter,
//! confidence degree, maximum k-plex and core/truss peeling — plus the Jaro
//! string similarity behind upstream's `SIMILARITY(a, b)`.
//!
//! Conventions and notation as in [`paths`](super::paths) and the
//! [parent module](super): "GE" = `src/Query/GeneralEvaluation.cpp` (lines as
//! `constant-branch / WHERE-branch`), algorithms under `src/Query/Algorithm/`,
//! `S(x)` = `getStringByID(x)`, `SEP` = one shared comma flag per call,
//! `g(x)` = [`fmt_g`](super::cxx::fmt_g) (`ss << double`), `f(x)` =
//! [`fmt_f`](super::cxx::fmt_f) (`std::to_string(double)`).
//!
//! **Once-per-call functions** (`doneOnceOp` in GE: `triangleCounting`, `PR`,
//! `labelProp`, `WCC`, `louvain`, global `clusterCoeff`) and the
//! before-the-loop ones (`maximumKplex`, `coreTruss`) emit a single result;
//! with no node argument the pair loop runs once (`src_iterations`).
//!
//! **Whole-graph loops** iterate the CSR nodes in ascending entity id (see
//! "Vertex domain" in the parent module); array sizes use `n =
//! num_nodes()`; upstream prints vertices by entity id.
//!
//! **NaN.** Several formulas divide `0/0` for isolated vertices. On x86 the
//! resulting NaN has its sign bit set and glibc prints it as `-nan`
//! (`to_string` and `ss <<` alike); compute it the same way or emit `-nan`
//! explicitly — a Rust `f64::NAN` constant prints as `nan`.

use super::{not_implemented, GArgs, GFnKind, GFunCtx, GFunValue};
use crate::error::Result;

/// `PPR(u, hopCnt, predSet, retNum)` — top-`retNum` personalised PageRank
/// from `u` (FORA, approximate).
///
/// GE ~1770/~2824; algorithm `PathQueryHandler::SSPPR` (`SSPPR.cpp:97`) with
/// its helpers (`SSPPR.cpp:5-96`, `PathQueryHandler.cpp:3025-3337`):
/// constants `alpha = 0.2`, `epsilon = 0.5`, `delta` from 1/4 halving down to
/// `1/n`, `pfail = 1/n/n/ln n`, `ppr_decay_alpha = 0.77`; forward push limited
/// to `level < hopCnt` unless `hopCnt == -1` (`k` in [`GArgs`]); random walks
/// pick a uniformly random filtered out-edge (`rand() % outDegree`, walking
/// predicates in filter order) and stop with probability `alpha` (`rand() /
/// RAND_MAX <= alpha`). A source without filtered out-edges gets PPR 1 for
/// itself. Upstream seeds with `srand(time(NULL))` (non-deterministic); use
/// [`GlibcRand`](super::cxx::GlibcRand) with the fixed seed 1 (glibc's
/// default) so results are reproducible, and document it.
///
/// Result per source: the `retNum` largest values (`partial_sort_copy` by
/// value, descending — ties in libstdc++ order; prefer ascending entity id),
/// trailing zero-valued entries removed (`retNum < 0`: upstream
/// `vector::resize` throws — return a clean error). Item:
/// `{"src":"S(u)","results":[{"dst":"S(x)","PPR":g(ppr)},...]}`. Separator
/// between sources: none in the constant branch, `SEP` in the WHERE branch.
pub fn ppr(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::Ppr))
}

/// `PR(directed, predSet, retNum, alpha=a, maxIter=m, tol=t)` — PageRank.
///
/// GE ~1812/~2878 (`doneOnceOp`); algorithm `PR.cpp:15`. Arguments as
/// upstream parses them: `alpha = (a as f32) as f64`, `tol = (t as f32) as
/// f64`, `maxIter = (m as f32) as i32`. Power iteration over all `n` nodes:
/// `neiNum[v]` = filtered out-degree (+ in-degree when undirected); start
/// `old = 1/n`; each round `new[x] = (1-alpha)/n` plus, for every `v`,
/// `alpha*old[v]/neiNum[v]` to each out-neighbour (and in-neighbour when
/// undirected, per predicate out then in, duplicates counted) or, for a
/// dangling `v`, `alpha*old[v]/n` to every node; stop when no
/// `|new - old| > tol` or after the loop `while (!stop && cnt++ <= maxIter)`
/// (i.e. at most `maxIter + 1` rounds). Result: every node with its value in
/// ascending entity id, or — when `retNum != -1 && retNum < n` — sorted by
/// value descending (libstdc++ `std::sort`, unstable; use a stable sort, ties
/// by ascending id) and truncated to `retNum` (`retNum > n` is clamped to
/// `n`; other negative values: upstream UB — treat as -1).
///
/// Output: `{"paths":[{"src":"S(x)","result":g(pr)},...]}` (items joined by
/// `,`). Note the documented example's `"results"` key is wrong; the code
/// prints `"result"`.
pub fn pr(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::Pr))
}

/// `triangleCounting(directed, predSet)` — number of triangles.
///
/// GE ~1784/~2842 (`doneOnceOp`); algorithm `triangleCounting.cpp:14`: empty
/// `pred_id_set` ([`PredFilter::is_empty`](super::PredFilter::is_empty)) →
/// `-1`. For every node `v`: `inSet` = in-neighbours (+ out-neighbours when
/// undirected) as a set. Directed: count, for every out-neighbour `o` of `v`
/// (per predicate, duplicates counted) and every out-neighbour `w` of `o` (per
/// predicate), `w ∈ inSet`; divide by 3. Undirected: for every `o ∈ inSet`,
/// count the distinct neighbours (out ∪ in) of `o` that are in `inSet`;
/// divide by 6. A total not divisible by the divisor → `-1`.
///
/// Output: the integer (`to_string(long long)`): `{"paths":[1]}`.
pub fn triangle_counting(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::TriangleCounting))
}

/// `closenessCentrality(u, directed, predSet)`.
///
/// GE ~1793/~2851; algorithm `closenessCentrality.cpp:17`: BFS from `u` (per
/// predicate out-neighbours, then in-neighbours when undirected) summing the
/// distances `sum` of all reached nodes and counting dequeued nodes `cnt`
/// (`u` included); result `(cnt - 1) / sum` as double (`int` sum) — `0/0` →
/// `-nan` for a vertex that reaches nothing.
///
/// Per source item: `{"src":"S(u)","result":f(c)}` (`to_string`: the
/// documented `0.555556`, `0.500000`). Separator between sources: none in the
/// constant branch, `SEP` in the WHERE branch.
pub fn closeness_centrality(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::ClosenessCentrality))
}

/// `betweennessCentrality(u, directed, predSet)`.
///
/// GE ~1996/~3074 → `GeneralEvaluation::betweennessCentrality` (GE ~4100):
/// `u < 0` → nothing. Algorithm `betweennessCentrality.cpp:233` (+
/// `getAllNeighborsById` `:5`, `shortestPathScalePassId` `:74`, the DFS
/// helpers `:50`): empty `pred_id_set` → 0; `vertex =
/// getAllNeighborsById(u, false, preds)` (a `std::set`); `|vertex| <= 1` → 0;
/// otherwise the sum over `x ∈ vertex, x != u` of
/// `shortestPathScalePassId(x, u, directed, preds)`, halved when undirected.
/// Port the helpers loop for loop.
///
/// Per source item: `{"src":"S(u)", "result":g(b)}` (space after the comma).
/// **No separator between sources** in either branch.
pub fn betweenness_centrality(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::BetweennessCentrality))
}

/// `clusterCoeff([u,] directed, predSet)` — local or global clustering
/// coefficient.
///
/// GE ~1906/~2972. The pair loop runs over
/// [`src_iterations`](super::src_iterations); an iteration whose `u` is `-1`
/// — no source argument, **or a source IRI unknown to the dictionary** —
/// computes the global coefficient once (`doneOnceOp`) and prints `f(gcc)`
/// bare: `{"paths":[0.500000]}`. Otherwise it prints
/// `{"src":"S(u)","result":f(lcc)}` with **no separator** between sources.
///
/// * Global (`clusteringCoeff.cpp:12`): empty `pred_id_set` → `-1`
///   (`-1.000000`); for every node `c`: `neiSet` = in ∪ out neighbours (any
///   direction, `directed` ignored); for every `x ∈ neiSet` and every distinct
///   neighbour `y` (out ∪ in) of `x`: `y ∈ neiSet` → closed += 1, else if
///   `y != c` → open += 1; both halved (integer); `closed / (closed + open)`,
///   0 when the denominator is 0.
/// * Local (`clusteringCoeff.cpp:91`): empty `pred_id_set` → `-1`. Directed:
///   `inSet` = in-neighbours, `neighbors` = in ∪ out neighbours; count, for
///   every out-neighbour `o` (per predicate) and every out-neighbour `w` of
///   `o` (per predicate), `w ∈ inSet`; divide by `ns * (ns - 1)` with `ns =
///   |neighbors|` (0 when that is 0). Undirected: `neiSet` = in ∪ out; count
///   pairs `x ∈ neiSet`, `y ∈ nbrs(x)` (distinct, out ∪ in) with `y ∈ neiSet`,
///   halve (integer), divide by `ns * (ns - 1) / 2`.
pub fn cluster_coeff(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::ClusterCoeff))
}

/// `labelProp(directed, predSet, maxIter=m)` — label-propagation communities.
///
/// GE ~1872/~2938 (`doneOnceOp`); algorithm `labelProp.cpp:13`.
/// `maxIter = (m as f32) as i32`. Deterministic: upstream calls
/// `srand(114514)` and draws ties with `rand() % maxNum` — use
/// [`GlibcRand::new(114514)`](super::cxx::GlibcRand) to reproduce it. Labels
/// start as the node index; each round visits nodes in order, counts the labels
/// of in-neighbours (+ out-neighbours when undirected, per predicate in then
/// out), and, if any, sets the label to a uniformly drawn most-frequent one
/// (candidates in ascending label order). After each round: `coms` = sorted
/// distinct labels; modularity `Q = Σ_com edge/2m - (deg/2m)^2` with `m =
/// n` (*sic*: `edgeNum = getVertNum()`); `maxLabels = labels` whenever `Q >
/// -1` (*sic*: `maxQ` is never updated). Stop when `comNum` is unchanged and
/// `|Q - oldQ| < 0.001` for more than 10 consecutive rounds, or after `maxIter`
/// rounds. Output (`output()`): community `i` = the nodes whose `maxLabels`
/// maps through `lbl2id` (built from the last round's `coms`, `std::map`
/// `operator[]` defaulting unknown labels to 0) to `i`, members ascending;
/// communities in `coms` order. `maxIter <= 0`: upstream reads an empty
/// `maxLabels` (UB) — use the initial labels (every node its own community).
///
/// Output: `{"paths":[["S(a)","S(b)"],["S(c)"]]}` —
/// [`vert_vec_json`](super::json::vert_vec_json) per community, joined by `,`.
pub fn label_prop(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::LabelProp))
}

/// `WCC(predSet)` — weakly connected components.
///
/// GE ~1889/~2955 (`doneOnceOp`); algorithm `WCC.cpp:13`: for every node in
/// ascending order not yet assigned, a BFS (per predicate in-neighbours then
/// out-neighbours) assigns the next component id; isolated nodes are
/// singleton components. Members are emitted by iterating the
/// `unordered_map<int, int> vid2wccid` (keys inserted in assignment order:
/// component by component, BFS discovery order within) — use
/// [`CxxHashOrder`](super::cxx::CxxHashOrder) over that insertion sequence —
/// appending each vertex to its component; components in id order.
///
/// Output: `{"paths":[["S(a)","S(b)"],["S(c)"]]}` (as [`label_prop`]).
pub fn wcc(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::Wcc))
}

/// `louvain(directed, predSet, maxIter=m, increase=x)` — Louvain communities.
///
/// GE ~1961/~3039 (`doneOnceOp`); algorithm `louvain.cpp:14` (only
/// `std::map`/`std::set` — deterministic): `phase1_loop_num = (m as f32) as
/// i32`, `min_modularity_increase = x as f32`. Vertices = nodes with a
/// filtered in- or out-edge. Weights are read per predicate from the
/// in-adjacency (`weights[src][dst] += 1`, plus the reverse when undirected;
/// `m` counts each). Port phase 1 / phase 2 loop for loop (including its
/// `s_tot` assignments that overwrite instead of accumulate).
///
/// Output: `{"paths":[{"count":N, "community":[[id,id],[id]]}]}` — `N` =
/// number of communities, each community the **raw entity ids** of its
/// members ascending, communities in `std::set<std::set<int>>` order
/// (lexicographic), joined by `,` (space only after `"count":N,`). The
/// documentation's `"details"`/`"communityId"` shape is not what the code
/// prints.
pub fn louvain(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::Louvain))
}

/// `JaccardSimilarity(u, predSet, k, retNum)` — the nodes most similar to `u`.
///
/// GE ~2000/~3078 → `GeneralEvaluation::JaccardSimilarity` (GE ~4108): `u <
/// 0` → nothing; `max_depth = hop(k)`, `max_retNum = retNum < 0 ? 10 :
/// retNum`. Algorithm `JaccardSimilarity.cpp:6` in **`f32`** arithmetic:
/// `N(x)` = in ∪ out neighbours (a `std::set`, direction ignored); empty
/// `N(u)` → nothing. For each `x ∈ N(u)` (ascending) a score `num /
/// (total - num)` (0 when `total - num <= 0`) where `total = |N(u)| + |N(x)|
/// - [x ∈ N(u)] - [u ∈ N(x)]` and `num = |N(x) ∩ N(u)|` (excluding `u`);
/// neighbours of `x` outside `N(u) ∪ {u}` are remembered (`no_used`, a set).
/// When `max_depth > 1` the same score is computed for every `no_used` node.
/// Scores are kept sorted descending by binary insertion (a new score goes
/// before existing equal ones). If fewer than `max_retNum`, nodes from
/// `kHopNeighbor(u, false, max_depth, preds, max_retNum)` not yet listed are
/// appended with score 0. The first `max_retNum` entries are returned.
///
/// Items `{"dst":S(x), "value":g(score)}` — the node string **unquoted** —
/// joined by `,`; **no separator between sources**. Documented example:
/// `{"paths":[{"dst":<Francis>, "value":0.5},...]}`.
pub fn jaccard_similarity(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::JaccardSimilarity))
}

/// `degreeCorrelation(u, k, predSet)` — Pearson correlation of adjacent
/// degrees in `u`'s `k`-hop neighbourhood.
///
/// GE ~2004/~3082 → `GeneralEvaluation::degreeCorrelation` (GE ~4126): `u <
/// 0` → nothing; `max_depth = hop(k)`. Algorithm `degreeCorrelation.cpp:5`:
/// empty `pred_id_set` → 0; a BFS from `u` over out ∪ in neighbours (a
/// `std::set` per node) within `max_depth` records each dequeued node's degree
/// (out + in over the filter); `|visited| <= 1` → 0; then over every visited
/// `x` (ascending) and out-neighbour `t` with recorded degree `!= 0`, the
/// Pearson coefficient of the pairs `(deg x, deg t)`, 0 when either standard
/// deviation is 0.
///
/// Per source item: `{"src":"S(u)", "result":g(r)}`; **no separator between
/// sources**.
pub fn degree_correlation(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::DegreeCorrelation))
}

/// `diameterEstimation(predSet)` — the largest BFS eccentricity.
///
/// GE ~1992/~3070 → `GeneralEvaluation::diameterEstimation` (GE ~4093),
/// printed once per pair-loop iteration (once: no node argument). Algorithm
/// `diameterEstimation.cpp:5`: for every node with a filtered out-edge
/// (ascending), `diameterEstimationByuid` (`:35`) — a BFS over
/// **out-edges only** (regardless of any direction flag) — gives the largest
/// distance; the result is the maximum (`unsigned`, 0 for no edges).
///
/// Output: `{"paths":[4]}`.
pub fn diameter_estimation(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::DiameterEstimation))
}

/// `confidenceDegree(n1, ..., n6)` — closeness of every vertex.
///
/// GE ~1538/~2531; algorithm `ConfidenceDegree.cpp:5`. The six numbers are
/// ignored (the parser only stores them for a `"DEGREE"` name that never
/// matches, `QueryParser.cpp` ~575); the predicate set is always *all*
/// predicates and `directed = false`. For every node `x` (ascending; upstream
/// iterates entity ids `0..n`): a BFS over the predicate ids `0..|preds|`
/// (out-neighbours then in-neighbours per predicate) computes `(cnt - 1) /
/// sum` in `unsigned` arithmetic as double; NaN → `0.0`.
///
/// Output — **not** the `{"paths":...}` wrapper: an nlohmann JSON array of
/// `[S(x), value]` pairs, dumped compactly:
/// `[["<a>",0.5],["<b>",0.0]]` (strings escaped by nlohmann, numbers via
/// [`fmt_nlohmann`](super::cxx::fmt_nlohmann)). Upstream prefixes the cell
/// with `pfn_type`, which makes the whole HTTP response `{"results": <this
/// array>}` ([`GFnKind::is_pfn_result`](super::GFnKind::is_pfn_result)).
pub fn confidence_degree(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::ConfidenceDegree))
}

/// `maximumKplex(predSet [, k])` — a maximum k-plex.
///
/// GE ~1580/~2634, emitted once before the pair loop; algorithm
/// `PathQueryHandler::maximumKplex` (`PathQueryHandler.cpp:3012`) over
/// `MaxKPX::Graph` (`:298-3010`): `read()` (`:1696`) builds an undirected
/// simple graph over all `n` nodes from the filtered out- and in-neighbours
/// (sorted, self-loops and duplicates removed), then `search()`. `k` absent →
/// 1. Port the branch-and-bound loop for loop.
///
/// Output: `{"paths":[{"kplex":[S(a),S(b)]}]}` — node strings **unquoted**,
/// joined by `,`.
pub fn maximum_kplex(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::MaximumKplex))
}

/// `coreTruss(predSet, alpha, beta)` — the vertices surviving core/truss
/// peeling.
///
/// GE ~1598/~2652, emitted once before the pair loop; algorithm
/// `PathQueryHandler::coreTruss` (`PathQueryHandler.cpp:3019`) =
/// `MaxKPX::Graph(preds, 1).read()` then `peel(alpha, beta)` (`:1979`) with
/// `alpha = extra[0]`, `beta = extra[1]` (integers). An empty graph trips
/// upstream's `assert(n > 0 && m > 0)` — return an empty list.
///
/// Output: `{"paths":[{"coreTruss":[S(a),S(b)]}]}` — node strings
/// **unquoted**, joined by `,`.
pub fn core_truss(_ctx: &GFunCtx<'_>, _a: &GArgs) -> Result<GFunValue> {
    Err(not_implemented(GFnKind::CoreTruss))
}

/// Jaro similarity of two strings — upstream's `SIMILARITY(a, b)`
/// (`TempResult::doSimilarity`, `src/Query/TempResult.cpp` ~784-843),
/// computed byte-wise in `f32` exactly as upstream and widened to `f64`:
///
/// * equal → 1; either empty → 0; the shorter string is `t`, the longer `s`;
/// * `t` a substring of `s` → `(|t|/|s| + 1 + 1) / 3`;
/// * otherwise match window `|t|/2 - 1`; each byte of `s` (scanning while
///   `i - window <= |t|`) matches the first unmatched equal byte of `t`
///   within the window; `m` matches (`m == 0` → 0); transpositions = the
///   number of positions where the match-order sequences of `s` and `t`
///   differ, halved (integer); result `(m/|s| + m/|t| + (m - tr)/m) / 3`.
///
/// Upstream evaluates it on the arguments' term strings (`term_value`, e.g.
/// `"abc"` with quotes for a literal); callers pass whatever strings upstream
/// would.
pub fn jaro_similarity(a: &str, b: &str) -> f64 {
    f64::from(jaro_f32(a.as_bytes(), b.as_bytes()))
}

fn jaro_f32(s: &[u8], t: &[u8]) -> f32 {
    if s == t {
        return 1.0;
    }
    if s.is_empty() || t.is_empty() {
        return 0.0;
    }
    if s.len() < t.len() {
        return jaro_f32(t, s);
    }
    let (s_len, t_len) = (s.len() as i64, t.len() as i64);
    if contains(s, t) {
        return (t_len as f32 / s_len as f32 + t_len as f32 / t_len as f32 + 1.0) / 3.0;
    }
    let window = t_len / 2 - 1;
    let mut sv = vec![-1i64; s.len()];
    let mut tv = vec![-1i64; t.len()];
    let mut m: i64 = 0;
    for i in 0..s_len {
        if i - window > t_len {
            break;
        }
        for j in 0..t_len {
            if j < i - window || j > i + window {
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
    let transpositions = sm.iter().zip(&tm).filter(|(a, b)| a != b).count() as i64 / 2;
    let mf = m as f32;
    (mf / s_len as f32 + mf / t_len as f32 + (mf - transpositions as f32) / mf) / 3.0
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaro_follows_upstream() {
        assert_eq!(jaro_similarity("abc", "abc"), 1.0);
        assert_eq!(jaro_similarity("", "abc"), 0.0);
        assert_eq!(jaro_similarity("abc", ""), 0.0);
        // Substring shortcut: (2/4 + 1 + 1) / 3 in f32.
        assert_eq!(jaro_similarity("abcd", "bc"), f64::from((0.5f32 + 1.0 + 1.0) / 3.0));
        assert_eq!(jaro_similarity("bc", "abcd"), jaro_similarity("abcd", "bc"));
        // MARTHA / MARHTA: window 2, m = 6, one transposition.
        let expect = (6.0f32 / 6.0 + 6.0 / 6.0 + (6.0 - 1.0) / 6.0) / 3.0;
        assert_eq!(jaro_similarity("MARTHA", "MARHTA"), f64::from(expect));
        // No common byte.
        assert_eq!(jaro_similarity("abc", "xyz"), 0.0);
    }
}
