//! Path, reachability and BFS-family graph functions (stubs + upstream
//! specifications).
//!
//! Every function here currently returns [`not_implemented`]; its doc comment
//! is the specification the porting package implements. Conventions used in
//! the specs (see the [module docs](super) for the shared output contract):
//!
//! * "GE:n" = `src/Query/GeneralEvaluation.cpp` line n (upstream 1.4,
//!   `bebe1ea`); first the constant-argument branch (GE:1508-2033, taken when
//!   the WHERE produced no rows), then the variable branch (GE:2547-3107).
//!   Other files are relative to `src/Query/Algorithm/`.
//! * "entry" = one element of the `{"paths":[…]}` array; entries are written
//!   for the [`GArgs::pairs`] (or [`GArgs::distinct_srcs`]) in order,
//!   separated by `,`.
//! * `‹u›` = the printed form of a vertex ([`GFunCtx::gnode_str`]), e.g.
//!   `<http://ex/Alice>`; `‹p›` = a predicate's printed form.
//! * "path entry" = [`push_path_json`](super::fmt::push_path_json) of a
//!   [`GPath`](super::fmt::GPath); "empty-path entry" = the same with
//!   [`GPath::empty`](super::fmt::GPath::empty), i.e.
//!   `{"src":"‹u›","dst":"‹u›","edges":[],"nodes":[]}`.
//! * Port the upstream loops **literally** (same predicate / direction
//!   interleaving — see [`traverse::NbrOrder`](super::traverse::NbrOrder) —,
//!   same queue discipline, same `std::set` / `std::map` (ascending) and
//!   `unordered_map` ([`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder))
//!   iteration orders): they decide which of several equal paths is printed.
//! * Upstream vertex ids are entity ids; CSR algorithms here run on
//!   [`Node`](super::Node)s (same order). "Isolated" / "missing" vertices: see
//!   the module docs; each spec states the special cases.
//!
//! # The shared shortest-path primitive
//!
//! `PathQueryHandler::shortestPath(uid, vid, directed, preds)`
//! (shortestPath.cpp:22-276) is used by `shortestPath`, `shortestPathLen`,
//! `kHopReachablePath` and directed `cyclePath` / `cycleBoolean`; port it
//! once as a private helper returning the upstream path vector:
//!
//! 1. `uid < 0 || vid < 0` (missing) → empty.
//! 2. Bidirectional BFS with distance maps `dis_u` (from `uid`) and `dis_v`
//!    (to `vid`). While no meet and either queue is non-empty: pop the
//!    **whole** current `u` frontier in FIFO order; for each vertex, for each
//!    predicate in filter order: its out-neighbours `t` — if `t ∈ dis_v` →
//!    meet at `t` (`dis_u[t] = dis_u[x] + 1`), stop everything; if `t ∈ dis_u`
//!    skip; else enqueue, `dis_u[t] = dis_u[x] + 1` —, then (undirected only)
//!    its in-neighbours the same way. Then the whole `v` frontier symmetrically
//!    with **in**-neighbours first and (undirected) out-neighbours second,
//!    meeting on `t ∈ dis_u`.
//! 3. Reconstruct from the meet node towards `vid`: repeatedly take, in
//!    predicate order, the first out-neighbour `t` with
//!    `dis_v[t] == dis_v[cur] - 1` (edge `pred`, forward), else (undirected)
//!    the first in-neighbour (edge reversed); then from the meet node back to
//!    `uid`: the first in-neighbour `t` with `dis_u[t] == dis_u[cur] - 1`
//!    (edge forward `t → cur`), else (undirected) the first out-neighbour
//!    (reversed). The result runs `uid → … → vid`.
//! 4. No meet → empty.

use super::{not_implemented, GArgs, GFnKind, GFunCtx, GFunValue};
use crate::error::Result;

/// `simpleCyclePath(u, v, directed, preds)`.
///
/// **Upstream:** parsed (QueryParser.cpp:411-412) but never evaluated: no
/// branch of GE:1619-2016 / GE:2673-3094 handles `simpleCyclePath_type`, and
/// `PathQueryHandler::simpleCycle` is commented out
/// (PathQueryHandler.cpp:109-122).
///
/// **Output:** always `{"paths":[]}` ([`GFunValue::Json`]), whatever the
/// arguments. Do not implement a simple-cycle search: there is no upstream
/// output to match.
pub fn simple_cycle_path(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::SimpleCyclePath))
}

/// `simpleCycleBoolean(u, v, directed, preds)`.
///
/// **Upstream:** like [`simple_cycle_path`] — parsed, never evaluated. Only
/// `cycleBoolean_type` is turned into an `xsd:boolean` (GE:2021, GE:3099), so
/// this returns the JSON envelope, not a boolean.
///
/// **Output:** always `{"paths":[]}` ([`GFunValue::Json`]).
pub fn simple_cycle_boolean(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::SimpleCycleBoolean))
}

/// `cyclePath(u, v, directed, preds)` — a cycle through `u` and `v`.
///
/// **Upstream:** GE:1619-1632 / GE:2673-2686; `PathQueryHandler::cycle`
/// (cycle.cpp:192-311).
///
/// **Per pair** `(u, v)`: `u == v` → no entry (skipped); otherwise compute
/// `cycle(u, v)` and, if non-empty, write the path entry with `src = u`,
/// `dst = v`. Pairs without a cycle produce nothing.
///
/// **cycle(u, v):**
/// * directed: `p1 = shortestPath(u, v, true)`, `p2 = shortestPath(v, u, true)`
///   (the shared primitive); either empty → none; else `p1` followed by `p2`
///   without its first and last element, then `u`: the cycle
///   `u → … → v → … → u` (all edges forward).
/// * undirected: `p = shortestPath0(u, v, false)` (cycle.cpp:20-176 — a
///   *different* bidirectional BFS that keeps whole routes per vertex and
///   records every edge with its **plain predicate id**, also for edges
///   traversed against their direction; its meet/join logic is cycle.cpp:44-175);
///   empty → none; else `p` + `p[len-2], p[len-3], …, p[1]` + `u`
///   (cycle.cpp:302-309): the same path walked back. Consequently every edge
///   prints as a forward edge (`fromNode` = earlier node) — reproduce this
///   quirk.
///
/// **Missing / isolated:** no path → no entry.
///
/// **Example** (SPARQL_QUERY.md, sample social data):
/// `cyclePath(?x, <Carol>, true, {<like>})` with `?x ∈ {<Dave>, <Eve>}` →
/// `{"paths":[{"src":"<Eve>","dst":"<Carol>","edges":[{"fromNode":2,"toNode":3,"predIRI":"<like>"},{"fromNode":3,"toNode":1,"predIRI":"<like>"},{"fromNode":1,"toNode":2,"predIRI":"<like>"}],"nodes":[{"nodeIndex":1,"nodeIRI":"<Bob>"},{"nodeIndex":3,"nodeIRI":"<Carol>"},{"nodeIndex":2,"nodeIRI":"<Eve>"}]}]}`
pub fn cycle_path(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::CyclePath))
}

/// `cycleBoolean(u, v, directed, preds)` — is there a cycle through `u`
/// and `v`?
///
/// **Upstream:** GE:1633-1643 / GE:2687-2697, result conversion GE:2021-2027 /
/// GE:3099-3105.
///
/// Iterate the pairs; skip `u == v`; the first pair whose [`cycle_path`]
/// cycle is non-empty makes the result `true` and stops the iteration.
///
/// **Output:** [`GFunValue::Bool`] (`"true"/"false"^^xsd:boolean`); no pairs
/// → `false`. (The upstream docs print the datatype as `XMLSchema#` — a doc
/// typo; the code emits `XMLSchema#boolean`.)
pub fn cycle_boolean(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::CycleBoolean))
}

/// `shortestPath(u, v, directed, preds)` — one shortest path.
///
/// **Upstream:** GE:1644-1665 / GE:2698-2719; the shared primitive (module
/// docs, shortestPath.cpp:22-276).
///
/// **Per pair:** `u == v` (same entity, missing included) → the empty-path
/// entry; otherwise the primitive's path, if non-empty, as a path entry
/// (`src = u`, `dst = v`; edges traversed against their direction print
/// reversed `fromNode`/`toNode`). Unreachable → no entry. Missing `u` or `v`
/// → no entry (primitive guard).
///
/// **Example** (SPARQL_QUERY.md): `shortestPath(<Francis>, ?x, true, {<like>, <focus>})`
/// with `?x = <Alice>` →
/// `{"paths":[{"src":"<Francis>","dst":"<Alice>","edges":[{"fromNode":4,"toNode":3,"predIRI":"<like>"},{"fromNode":3,"toNode":1,"predIRI":"<like>"},{"fromNode":1,"toNode":0,"predIRI":"<focus>"}],"nodes":[{"nodeIndex":0,"nodeIRI":"<Alice>"},{"nodeIndex":1,"nodeIRI":"<Bob>"},{"nodeIndex":3,"nodeIRI":"<Carol>"},{"nodeIndex":4,"nodeIRI":"<Francis>"}]}]}`
pub fn shortest_path(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::ShortestPath))
}

/// `shortestPathLen(u, v, directed, preds)` — the shortest-path length.
///
/// **Upstream:** GE:1720-1745 / GE:2720-2745.
///
/// **Per pair:** `u == v` → `{"src":"‹u›","dst":"‹u›","length":0}`; otherwise
/// the shared primitive's path; if non-empty →
/// `{"src":"‹u›","dst":"‹v›","length":L}` with `L` = number of edges;
/// unreachable / missing → no entry.
///
/// **Example:** `shortestPathLen(<Francis>, ?x, true, {<like>, <focus>})` →
/// `{"paths":[{"src":"<Francis>","dst":"<Alice>","length":3}]}`.
pub fn shortest_path_len(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::ShortestPathLen))
}

/// `shortestPathCount(u, v, directed, preds)` — the number of shortest paths.
///
/// **Upstream:** GE:1954-1960 / GE:3028-3038; shortestPathCount.cpp:15-163.
///
/// **Per pair** (every pair, also `u == v` and unreachable ones):
/// `{"src":"‹u›", "dst":"‹v›", "count":N}` — note the spaces after the
/// commas, exactly as upstream writes them.
///
/// **N:** a level-synchronous bidirectional BFS with per-vertex path counts
/// `scnt` / `tcnt` and depths `sd` / `td`. Each round expands the smaller
/// frontier (`qsz < tqsz` → source side, otherwise — ties included — the
/// target side). Expanding a vertex `x` (count `c`, depth `d`): directed →
/// source side follows out-edges, target side in-edges; undirected → all
/// predicates' in-edges first, then all predicates' out-edges
/// (`NbrOrder::AllInThenAllOut`; the target side mirrors it: out first,
/// then in). Each edge is processed separately (multi-edges of different
/// predicates count as different paths): a fresh neighbour `n`
/// (`scnt[n] == 0`) gets `scnt[n] = c`, `sd[n] = d + 1`, and if it was already
/// reached by the other side it is a *joint* (meet; the current round still
/// finishes but nothing more is enqueued); a neighbour at depth `d + 1` gets
/// `scnt[n] += c`. Result `Σ_joint scnt[j] · tcnt[j]`, computed in C `int`
/// (wrap like `i32`), `0` without a meet. `u == v` → 0 (the two sides never
/// meet). Missing `u` / `v`: upstream indexes arrays with `-1` (UB) → no
/// entry; isolated → `count` 0.
pub fn shortest_path_count(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::ShortestPathCount))
}

/// `shortestPaths(u, v, directed, preds)` — all shortest paths.
///
/// **Upstream:** GE:2008-2011 / GE:3086-3089 → wrapper
/// `GeneralEvaluation::kHopShortestPaths` (GE:4134-4159);
/// `PathQueryHandler::kHopShortestPaths` + `djstl_dfs`
/// (kHopShortestPaths.cpp:5-193).
///
/// **Per pair:** missing `u` or `v` → nothing; `u == v` → the empty-path
/// entry; otherwise one path entry per path, in this order:
///
/// 1. BFS from `u` over everything reachable (out-edges, plus in-edges when
///    undirected, per predicate in filter order), recording for every vertex
///    the list `dis_pre[t]` of *distinct* predecessors at shortest distance,
///    in the order they were found. Per expanded vertex the newly seen
///    neighbours are collected in a `std::set` and enqueued **ascending**.
/// 2. Depth-first from `v` back to `u` over `dis_pre` (in list order) yields
///    vertex sequences `u … v` (`v` unreachable → none).
/// 3. Each vertex sequence fans out into one path per combination of
///    predicates: for every hop `x → y` the candidate predicates are those
///    with `x -p-> y` or (undirected) `y -p-> x`, ascending (`std::set`). The
///    first candidate extends the current path list in place; each further
///    candidate appends copies of the pre-hop list extended with it
///    (kHopShortestPaths.cpp:141-171). All edges are recorded **forward**
///    (plain predicate id) even when they were in-edges — reproduce.
pub fn shortest_paths(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::ShortestPaths))
}

/// `kHopReachable(u, v, directed, k, preds)` — is `v` reachable from `u`
/// within `k` hops (`k < 0`: unbounded)?
///
/// **Upstream:** GE:1746-1769 / GE:2746-2770; kHopReachable.cpp:16-373.
///
/// **Per pair** (every pair yields an entry): `u == v` →
/// `{"src":"‹u›","dst":"‹u›","value":"true"}`; otherwise
/// `k' = if k < 0 { 999 } else { k }` and
/// `{"src":"‹u›","dst":"‹v›","value":"true"}` or `…"value":"false"}` — the
/// value is a quoted string. Missing `u` prints `"src":""` with `"false"`.
/// (In the variable branch upstream forgets the `,` between these entries;
/// we write it — see the separator rule.)
///
/// **Boolean (port literally — the upstream code deviates from plain
/// reachability):**
/// * `false` immediately if `u` has no out-edges or `v` no in-edges over
///   `preds` (undirected: no incident edges at all) (:20-32); `k' == 0` → `false`.
/// * A "partial push" phase runs only while `rmax > 1000 / n^0.75` with
///   `rmax` starting at 0.001, i.e. only for graphs with more than ~10^8
///   vertices; its `directed` / `!directed` branches are swapped (:85-166).
/// * The full BFS phase alternates batches of 10 pops between a forward BFS
///   (levels `f1`) and a backward BFS (levels `f2`), returning `true` when a
///   vertex has `f1 + f2 <= k'` (or meets the push levels). Bugs to keep:
///   the undirected forward step reads in-neighbour *positions* with
///   `getOutVertID` (:206), the backward step does the same for every
///   in-neighbour (:340) and pushes its discoveries into the **forward**
///   next-queue (:348, :366). `getOutVertID(v, p, i)` with `i` beyond the
///   out-degree returns `-1`, which then acts as a pseudo-vertex (it can
///   "meet" when both sides recorded it). Model `-1` as a sentinel vertex
///   with no edges.
///
/// The docs' examples (`kHopReachable(<Francis>, ?x, true, 2, {<like>,<focus>})`
/// → `"false"`, with `k = -1` → `"true"`) must come out unchanged; note the
/// deviation from true k-hop reachability in the port's docs.
pub fn k_hop_reachable(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::KHopReachable))
}

/// `kHopReachablePath(u, v, directed, k, preds)` — a path of at most `k` hops.
///
/// **Upstream:** GE:1666-1690 / GE:2771-2795;
/// `PathQueryHandler::kHopReachablePath` (PathQueryHandler.cpp:236-243).
///
/// **Per pair:** `u == v` → the empty-path entry; otherwise
/// `k' = if k < 0 { 999 } else { k }`, take the shared primitive's shortest
/// path and drop it if it has more than `k'` edges; a remaining path → path
/// entry; else nothing. (So the path is always a *shortest* one; the docs'
/// remark that it "could also be a non-shortest path with a ring" does not
/// apply to the code.)
pub fn k_hop_reachable_path(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::KHopReachablePath))
}

/// `kHopEnumerate(u, v, directed, k, preds [, retNum])` — all simple paths
/// of at most `k` hops.
///
/// **Upstream:** GE:1691-1719 / GE:2796-2823;
/// `kHopEnumeratePath` + `bc_dfs` (kHopEnumeratePath.cpp:5-174).
///
/// **Per pair:** `u == v` → the empty-path entry; missing `u`/`v` → nothing;
/// otherwise `k' = if k < 0 { 999 } else { k }`, `retNum = a.ret_num`
/// (`None` → `-1` = unlimited) and one path entry per enumerated path in
/// discovery order:
///
/// 1. `bar[x]` = hop distance from `x` to `v` by a reverse BFS from `v`
///    (in-edges, plus out-edges when undirected, per predicate) that stops
///    expanding at distance `k'`. Unreached vertices keep `bar = 0` (so they
///    are not pruned — keep that).
/// 2. DFS from `u` with a stack of `(vertex, edge)` (`u` with edge `-1`):
///    on reaching `v` record the path and pop; otherwise, while the stack
///    holds fewer than `k' + 1` vertices, for each predicate in filter order:
///    out-neighbours `t` (edge `pred`), then (undirected) in-neighbours
///    (edge `-pred-1`, i.e. reversed); skip `t` already on the stack (simple
///    paths) and `t` with `stack_len + bar[t] > k'`; recurse only while the
///    remaining budget is non-zero.
/// 3. Budget: starts at `retNum`, decremented per recorded path while
///    positive; `-1` never reaches 0 (unlimited), `0` enumerates nothing.
///
/// **Example** (SPARQL_QUERY.md): `kHopEnumerate(<Alice>, ?x, true, 3, {<like>, <focus>})`
/// with `?x = <Carol>` → two paths `Alice -like-> Bob -like-> Eve -like-> Carol`
/// then `Alice -focus-> Bob -like-> Eve -like-> Carol`, each with
/// `"nodes":[{"nodeIndex":3,…<Carol>},{"nodeIndex":2,…<Eve>},{"nodeIndex":0,…<Alice>},{"nodeIndex":1,…<Bob>}]`.
pub fn k_hop_enumerate(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::KHopEnumerate))
}

/// `kHopCount(u, directed, k, preds)` — how many vertices lie exactly `k`
/// hops away.
///
/// **Upstream:** GE:1924-1931 / GE:2990-3001; `kHopCount` =
/// `kHopNeighbor(uid, directed, k, preds, -1).size()`
/// (PathQueryHandler.cpp:3435-3438).
///
/// **Per source:** `{"src":"‹u›","depth":K, "count":N}` (no space before
/// `"depth"`, one before `"count"`), `K` = `k` as written (no `999`
/// substitution), `N` = size of [`k_hop_neighbor`]'s list without a cap. The
/// docs say "within k layers" but the code counts vertices at distance
/// *exactly* `k` (follow the code). `k <= 0` → 0. Missing `u` → `"src":""`,
/// count 0.
///
/// **Example** (SPARQL_QUERY.md): `{"paths":[{"src":"<Alice>","depth":3, "count":1}]}`.
pub fn k_hop_count(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::KHopCount))
}

/// `kHopNeighbor(u, directed, k, preds [, retNum])` — the vertices exactly
/// `k` hops away.
///
/// **Upstream:** GE:1932-1953 / GE:3002-3027; kHopNeighbor.cpp:14-76.
///
/// **Per source:** `{"src":"‹u›","depth":K, "dst":["‹a›","‹b›",…]}` when
/// [`GArgs::where_empty`] (constant branch, GE:1946), otherwise the key is
/// `"neighbor"` (GE:3016): `{"src":"‹u›","depth":K, "neighbor":[…]}`.
/// `K` = `k` as written; `retNum` = `a.ret_num`, `None` or negative → 100.
///
/// **List:** level-synchronous BFS from `u` marking vertices visited when
/// discovered; directed → out-edges; undirected → for each expanded vertex
/// all predicates' in-neighbours, then all predicates' out-neighbours
/// (`NbrOrder::AllInThenAllOut`). Vertices first discovered on level `k`
/// are collected in discovery order (levels `< k` are only expanded);
/// `k <= 0` → empty. After each collected vertex, stop if
/// `len >= retNum` — so `retNum = 0` still returns one vertex (quirk).
/// Missing `u` → `[]`; isolated → `[]`.
pub fn k_hop_neighbor(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::KHopNeighbor))
}

/// `kHopAllNeighbors(u, directed, k, preds [, retNum])` — the vertices within
/// `k` hops (the source included).
///
/// **Upstream:** GE:2012-2015 / GE:3090-3093 → wrapper GE:4161-4177;
/// kHopAllNeighbors.cpp:5-86.
///
/// **Per source:** missing `u` → nothing; otherwise
/// `k' = if k < 0 { 999 } else { k }`, `retNum` `None`/negative → 100, and
/// `{"results":["‹a›","‹b›",…]}`. (Upstream writes consecutive sources'
/// objects without a `,`; we separate them.)
///
/// **List:** BFS from `u` with a distance map (out-edges, plus in-edges when
/// undirected, per predicate in filter order); for each dequeued vertex `x`:
/// collect its neighbours into a `std::set` `nl` and lower their recorded
/// distances; **then** stop if `|result| >= retNum`; else, if
/// `dist(x) <= k'`, add `x` to the result and enqueue the unvisited members of
/// `nl` (ascending) whose distance is `<= k'`. The result is a `std::set`, so
/// it prints **ascending by entity id** (which vertices make it in is decided
/// by the BFS order). `retNum = 0` → `[]`.
pub fn k_hop_all_neighbors(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::KHopAllNeighbors))
}

/// `bfsCount(u, directed, preds)` — vertices per BFS level.
///
/// **Upstream:** GE:1799-1811 / GE:2861-2877; bfsCount.cpp:15-85.
///
/// **Per source:**
/// `{"src":"‹u›","results":[{"depth":0, "count":1}, {"depth":1, "count":2}]}`
/// — inside an element `", \"count\":"` has a space, and elements are
/// separated by `", "` (comma **and** space).
///
/// **Counts:** BFS from `u` (directed: out-edges per predicate; undirected:
/// all predicates' in-edges, then all predicates' out-edges); `count[d]` =
/// vertices first discovered at depth `d`; depth 0 is `u` itself (1); no
/// trailing zero level. Missing / isolated `u` → `[{"depth":0, "count":1}]`.
///
/// **Example** (SPARQL_QUERY.md): `bfsCount(<Alice>, true, {<like>,<focus>,<dislike>})` →
/// `{"paths":[{"src":"<Alice>","results":[{"depth":0, "count":1}, {"depth":1, "count":2}, {"depth":2, "count":1}, {"depth":3, "count":1}, {"depth":4, "count":1}]}]}`.
pub fn bfs_count(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::BfsCount))
}

/// `SSSP(u, directed, preds)` — a shortest path from `u` to every reachable
/// vertex.
///
/// **Upstream:** GE:1832-1851 / GE:2898-2917; SSSP.cpp:14-58.
///
/// **Per source** (sources separated by `,`): one path entry per reachable
/// vertex `t` (`u` itself included, as the single-node path), comma-separated,
/// with `src = u`, `dst = t`.
///
/// * Paths: plain BFS from `u` (FIFO, visited-on-discovery; per predicate in
///   filter order: out-neighbours, then in-neighbours when undirected —
///   `NbrOrder::PerPredOutThenIn`); `path[t] = path[parent] + [pred, t]`.
///   **Every edge is recorded with its plain predicate id**, also in-edges,
///   so it prints forward (`fromNode` = earlier node) — reproduce.
/// * Order: the result is an `unordered_map<int, vector<int>>` filled in
///   discovery order (`u` first); iterate it in
///   [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder) order of the
///   discovered entity ids.
/// * Missing / isolated `u`: only its own single-node path (a missing one
///   prints `"src":"","dst":""` and `{"nodeIndex":-1,"nodeIRI":""}`).
pub fn sssp(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::Sssp))
}

/// `SSSPLen(u, directed, preds)` — shortest-path lengths from `u`.
///
/// **Upstream:** GE:1852-1871 / GE:2918-2937; SSSPLen.cpp:14-54.
///
/// **Per source** (sources separated by `,`): one
/// `{"src":"‹u›","dst":"‹t›","length":L}` per reachable vertex `t` (`u`
/// itself with length 0), comma-separated, in the
/// [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder) order of the
/// entity ids in BFS discovery order (same BFS as [`sssp`]). Missing /
/// isolated `u` → only its own `length 0` entry.
pub fn sssp_len(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = (ctx, a);
    Err(not_implemented(GFnKind::SsspLen))
}
