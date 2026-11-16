# r2-analytics — Graph functions: bounded output while building and per query, iteration caps, inner-loop cancellation, GraphView from the cached CSR, determinism, documented upstream quirks

## Changelog

Behaviour changes first:

- **`gfun_max_output_bytes` is a per-query budget** (review
  gap-gfun-fuzz#5). Every advanced graph-function and `PFN.name` call of a
  query is charged against one shared counter — each `GROUP BY` group, and
  calls inside `GRAPH` blocks (sub-SELECTs) too — and each call runs
  bounded by what is left of it, so it still stops while it builds its
  text. A query whose graph-function output passes the bound fails with
  `<function>: more than N bytes of output in this query (…)`. Before, the
  bound applied per call: 2000 groups of a 22 KB `WCC({})` produced 88 MB of
  rows. A graph function whose vertex arguments name no variable is now
  computed once per query under `GROUP BY` (its output is still charged
  for every group that shows it), instead of once per group.
- **`PR`, `louvain` and `labelProp` stop at an iteration-work bound**
  (gap-gfun-fuzz#2). `PR` with `tol < 0` and `louvain` with `increase < 0`
  never converge and ran up to 2^31 sweeps (≈45 minutes on a 3-vertex
  graph, hours on real data) until an operator cancelled them. Every sweep
  is now charged with the vertices, edges and community comparisons it
  visits against `gfun::MAX_ITERATION_WORK` = 2^34 steps (the first 64
  sweeps are free, so a huge graph still runs that many); when it is spent
  before convergence the call fails with `<function>: not converged within
  the iteration bound of 17179869184 steps (…); lower maxIter or use a
  positive tol` (`… a non-negative increase` for louvain). Upstream's
  parameters stay accepted and converging calls are unaffected; the bound
  is a documented constant (no configuration key). Library users can set
  it per call with `GFunCtx::with_max_iteration_work`.
- **Graph functions stop at `/cancel` from inside their long loops**
  (analytics#8). WCC polled only between components, triangle counting /
  clusterCoeff / labelProp per 1024 vertices whatever their degree,
  closeness / betweenness / degreeCorrelation / diameterEstimation /
  confidenceDegree never inside a BFS, the kplex `Searcher` never in its
  `O(n²)` initialisation (gap-gfun-fuzz#3): a giant component or a hub ran
  to the end (1.1 s after the flag on a 4M-vertex chain). A weighted
  ticker now charges each vertex expansion with its degree and polls every
  4096 units; the commit drain and graceful shutdown, which wait on the
  same flag, no longer wait for a whole traversal.
- **`maximumKplex` and `coreTruss` stop at the output bound while they
  build their text** (gap-gfun-fuzz#4), as `confidenceDegree` and
  `louvain` now do too. `maximumKplex({}, 65536)` with a 200-byte IRI for
  vertex 0 peaked at 52 MB (417 MB for k = 524289) before failing at a
  1 MiB bound; it now fails having allocated about the bound. The JSON
  envelope is written in place (no second copy).
- **`rs:LOUVAIN` is deterministic** (analytics#3): equal modularity gains
  were resolved by `HashMap` iteration order, so the partition changed from
  run to run (12 runs gave four different answers). Ties now go to the
  smallest community id; `GraphView::louvain_directed` likewise.
- **The legacy `rs:` built-ins share the database's CSR and are
  cancellable** (analytics#4). `rs:CYCLEBOOLEAN` / `rs:SIMPLECYCLEBOOLEAN`
  and the paths between two entities (`rs:SHORTESTPATHLEN`, `rs:SSSP`,
  `rs:SSSPLEN`, `rs:KHOPREACHABLE`, `rs:SHORTESTPATHS`) run on a view built
  from the query's per-predicate CSR (the database's, cached per data
  version) instead of rescanning the whole store per query; the others
  (which count literal objects as nodes) still build their view from the
  store once per query, now one predicate at a time. Both builds and every
  algorithm poll the cancel flag; a cancel fails the query.
  `rs:CYCLEBOOLEAN` is computed once per query (it ran per row) and
  `rs:CLOSENESSCENTRALITY(<x>)` runs one BFS from `<x>` (memoised per node)
  instead of the all-sources pass. Answers are unchanged.
- **`SIMILARITY` is cancellable** (analytics#5): the Jaro match loop visits
  only each character's window and polls the flag every ~4096
  comparisons, the containment test is a linear `memmem` search. Values
  are bit-identical; two long literals still cost `O(|a|·|b|)` per row
  (upstream's window is ~`|b|` wide), now interruptible.
- **Less memory per call**: the kplex `Searcher` keeps its adjacency as
  bits and its common-neighbour counts as 16-bit integers (≈2.1 n² bytes
  instead of 5 n²: 136 MiB instead of 320 MiB at the 8192-vertex ego
  network limit; gap-gfun-fuzz#3); the `unordered_map` order emulation
  behind WCC / PPR / the path family keeps no per-key `HashMap` index any
  more (it finds keys along their bucket's run, like libstdc++): 1M keys
  peak below 40 bytes per key instead of 72 (analytics#9);
  degreeCorrelation, closeness, betweenness and JaccardSimilarity allocate
  their `n`-sized working sets once per call instead of once per source
  (400 sources on 200k vertices allocated 1.28 GB in degreeCorrelation;
  analytics#7).
- JaccardSimilarity's `kHopNeighbor` fallback uses the path family's port;
  the duplicate (unpolled) `metrics::k_hop_neighbor` is gone
  (analytics#10 / design#15). Outputs are unchanged.

New library API (additions only): `gfun::MAX_ITERATION_WORK`,
`GFunCtx::with_max_iteration_work` / `max_iteration_work` / `poll`,
`gfun::metrics::jaro_similarity_cancellable`, `analytics::Interrupt`,
`GraphView::from_csr`, `GraphView::closeness_of`, `GraphView::contains`,
`traverse::bfs_with`; the hidden test hook `gfun::debug_poll_hook`
(process-wide, one atomic load per poll while unset). No public API was
removed or changed.

New tests: `tests/gfun_limits.rs` (mid-run cancellation of every
long-running metric and of the legacy built-ins with the flag raised from
inside a loop, a REST `/cancel` while the computation is parked, the
iteration bound, output bounds measured with a counting allocator, the
per-query budget across GROUP BY groups and GRAPH blocks, no store scan
for the CSR-backed legacy built-ins, equivalence with the full view,
`rs:LOUVAIN` determinism, per-source allocations, graphs without
vertices, a 2^40-path ladder), unit tests in `src/analytics/mod.rs`,
`gfun/metrics.rs`, `gfun/metrics/kplex.rs` and `gfun/cxx.rs`.

## GAP_PLAN status line

§3.1 / §9.3: the legacy `rs:` graph built-ins no longer rebuild a
whole-store GraphView per query for cycles and entity-to-entity paths (they
use the database's cached CSR); the others still do (they need literal
objects), now cancellably — review analytics#4.

## Docs notes

- **README.md (graph functions section, near "its JSON is bounded")**:
  `gfun_max_output_bytes` bounds the graph-function (and PFN) output of a
  whole query — all calls together, each GROUP BY group and GRAPH block
  included — not one call. Iterative functions stop at a documented work
  bound (`gfun::MAX_ITERATION_WORK`, 2^34 vertex/edge/community visits,
  first 64 sweeps free) with an error; mention `tol < 0` / `increase < 0`
  as the typical cause. Every function polls `/cancel` inside its loops.
- **README.md: reproduced upstream quirks of the advanced graph functions
  (analytics#2).** These answers are wrong by design (gStore 1.4
  compatibility, GAP_PLAN 3.1) and must be listed for users:
  - `kHopReachable(u, v, directed, k, preds)` searches only **one round**:
    at most 10 forward pops from `u`, then one backward expansion of `v`.
    Distant pairs answer `false` although reachable (on a 16-node chain,
    `n0 → n11` within 20 hops is `false`, `n0 → n10` is `true`); a `v → u`
    edge or a predicate with more in- than out-edges can make an
    unreachable pair `true` (upstream reads neighbour positions with
    `getOutVertID`).
  - `cycleBoolean` / `cyclePath` with `directed = false` walk the path back
    and call it a cycle: every connected pair "has a cycle" (an acyclic
    chain answers `true`; a single edge `u – v` gives `u → v → u`), and
    every edge prints as forward.
  - `triangleCounting` returns **-1** when the raw total is not divisible
    by 3 (directed) / 6 (undirected) — e.g. a self-loop or parallel edges
    next to a triangle — instead of a count.
  - `maximumKplex` starts from a fake solution of `2k - 2` copies of CSR
    vertex 0 and returns it when no larger k-plex is found (`k = 2` on an
    edgeless predicate set → `[<v0>,<v0>]`; a 6-cycle with k = 3 →
    `[<a>,<a>,<a>,<a>]`); ego-network ids are mapped through an array the
    pruning overwrote, so the reported set can contain repeats or
    non-members. IRIs are printed unquoted.
  - `coreTruss` appends (shrunk) vertex 0 once per peeled vertex while it
    has edges; IRIs unquoted.
  - `PPR` on graphs with fewer than 4 vertices returns the default slot
    `{"dst":"<vertex 0>","PPR":0}` for every source; an all-zero result
    still prints vertex 0; `kth_ppr` indexes by PPR *value* and can surface
    vertex 0; values are deterministic here (upstream seeds with the time).
  - `labelProp` computes modularity with the vertex count as edge count
    and maps the result through the last iteration's labels (stale labels
    fall into community 0, a current label nobody carries prints `[]`).
  - `louvain` keeps upstream's `s_tot` assignment (the last edge's weight,
    not a sum) and prints members as entity ids, in a
    `{"count":C, "community":[…]}` shape that differs from the upstream docs.
  - `closenessCentrality` of an isolated or missing source prints
    `"result":-nan`; `clusterCoeff` with a missing constant source answers
    the global coefficient; `PR`'s entries use the key `"result"` (the docs
    say `"results"`); `shortestPathCount` / closeness sums wrap like C
    `int`; `JaccardSimilarity` and `degreeCorrelation` report missing
    sources as nothing.
  - The one deliberate deviation stays: a `,` between consecutive entries
    where upstream writes none (invalid JSON upstream).
  The rustdoc of each function in `src/analytics/gfun/{paths,metrics}.rs`
  and the `kplex` module docs carry the details.
- **README.md (the `rs:` namespace table)**: the legacy built-ins'
  answers are unchanged; `rs:LOUVAIN` is now deterministic (ties → smallest
  community id); cycles and entity-to-entity paths use the database's CSR.
- **docs/DEPLOY.md (`[query] gfun_max_output_bytes`, line ~257)**: "the
  most JSON one graph-function call may produce" becomes "the most JSON the
  graph-function and PFN calls of one query may produce together"; mention
  the iteration bound and that the kplex `Searcher` holds up to ≈136 MiB
  per call at its 8192-vertex ego-network limit (one call per worker).
- **docs/REFACTOR_BACKLOG.md**:
  - analytics#10 / design#15 (remaining): the two graph layers (the
    legacy `GraphView` — predicates merged, literal objects as nodes — and
    the per-predicate entity-only CSR) are not unified; the legacy
    built-ins that count literal objects still build a per-query view from
    the store. Unifying them needs a CSR with literal objects (or a
    literal-edge side table) — a layout change, out of scope (D3).
    `WeightedGraphView`, the top-k subgraph matcher (memory `O(matches)`),
    `GraphView::k_core`, `louvain_directed`, `pagerank`,
    `strongly_connected_components` and `bfs_distances` have no server
    caller; kept as library API (D2), documented as such in the module docs.
  - analytics#4 (remaining, by design): betweenness (`O(V·(V+E))` even for
    one node), local clustering / per-node triangles (`O(d² log d)`) are
    inherently quadratic; documented and cancellable.
  - analytics#7 (remaining): `PPR` keeps upstream's per-source `O(n)` work
    (`upper` / `lower` over all vertices and the `if_stop` scan each round);
    its forward push allocates two `n`-sized flags per round.
  - gap-gfun-fuzz#3 (remaining): the `Searcher` matrices are still `n²`
    per ego network (now ≈2.1 n² bytes); a concurrency limit on maximumKplex
    calls or a configurable vertex cap would bound the total further.
  - gap-gfun-fuzz#2: a general per-query time / work budget (all query
    shapes, not only iterative graph functions) remains an engine feature
    for later.
  - analytics#5: Jaro stays `O(|a|·|b|)` per row (upstream's window is
    ~|b| wide); a length cap would change upstream answers. The legacy
    `rs:SIMILARITY` string mode (the engine's own windowed Jaro in
    `src/query/engine.rs`, outside this package's regions) is the same
    cost and does not poll the cancel flag yet.
- **docs/CPP_PARITY.md (row "CSR邻接 + 图算法 + topk", for its owner)**:
  upstream `src/Query/topk` is the `ORDER BY … LIMIT` query top-k, which
  this repo implements in `src/query/topk.rs`; the branch-and-bound top-k
  *subgraph* matcher of `WeightedGraphView` is a library-only extra with
  no SPARQL or server caller (review analytics#10).

## Measured numbers

- **Bench smoke gate** at this branch (release `gbuild` + `gstore-bench`,
  `GSTORE_BENCH_CACHE` warm, `flock /tmp/gstore-bench.lock
  target/release/gstore-bench ci --baseline bench/baselines/smoke.json`):
  **PASS, 456 gates checked, 0 failed**; time geomean vs the committed
  baseline: LUBM-like snapshot 1.01, disk 1.07, WatDiv-like snapshot 1.19,
  disk 1.23 (bound 1.50). Machine state: other worktrees' builds and tests
  running, load average 11.6 on 4 CPUs at the start, so pessimistic; the
  same order as round 1's figures (1.02 / 1.06 / 1.15 / 1.17). The smoke
  gate has no graph-function query, so those were timed separately:
- **Graph functions, base (b8b1c8e) vs branch**, release `gquery` on a
  `gbuild` of `bench-cache/lubm-like1-u1-s0.nt` (143,930 triples), best of
  3 wall-clock runs per query (process start and database open, ~40 ms,
  included), two interleaved rounds, load average 7.6–10 on 4 CPUs:

  | query | base round 1 / 2 | branch round 1 / 2 |
  |-------|------------------|--------------------|
  | `WCC({})` | 89 / 80 ms | 73 / 79 ms |
  | `PR(true, {}, 10, alpha=0.85, maxIter=20, tol=0.000001)` | 59 / 68 ms | 63 / 61 ms |
  | `triangleCounting(false, {})` | 210 / 221 ms | 190 / 205 ms |
  | `labelProp(false, {}, maxIter=5)` | 105 / 111 ms | 112 / 111 ms |
  | `clusterCoeff(false, {})` | 587 / 595 ms | 561 / 564 ms |
  | `closenessCentrality(?x, false, {})`, 21 departments | 77 / 85 ms | 76 / 75 ms |
  | `maximumKplex({}, 2)` | 69 / 91 ms | 65 / 66 ms |
  | `rs:WCC(?x)` over 50 rows | 61 / 62 ms | 61 / 83 ms |
  | `rs:SHORTESTPATHLEN(?x, ?x)` over 50 rows | 77 / 73 ms | 58 / 60 ms |
  | `FILTER(rs:CYCLEBOOLEAN())` over the 22,264 rows of `?x ub:name ?n` (no cycle, so no row passes) | 43,797 / 44,987 ms | 54 / 55 ms |

  The inner-loop polls and the output / iteration accounting cost nothing
  measurable; `rs:CYCLEBOOLEAN` ran Tarjan over the full store view once
  per row and now runs once per query on the CSR view (≈800×).
- Counted, not timed (tests/gfun_limits.rs, counting allocator):
  `maximumKplex({}, 65536)` with a 200-byte IRI at a 1 MiB bound peaked at
  54.7 MB before, < 4 MB after; `coreTruss(2, 1)` with a 2000-byte vertex 0
  and 2000 peeled squares at a 16 KiB bound allocated 32.9 MB before, under
  a quarter of the 16 MB full text after; 400 degreeCorrelation sources on
  200,000 vertices allocated 1.28 GB before, < 8 MB after; 1M inserts into
  `CxxUnorderedOrder` peaked at 71.8 B/key before, < 40 B/key after; the
  `Searcher` matrices of a 1000-vertex ego network take 5.0 MB before,
  2.1 MB after.
- Workspace tests (`cargo test --workspace`, debug): 140 test binaries,
  2578 tests passed, 0 failed, in `GSTORE_DISK_QUERY_MODE=memory` and in
  `stream`.

## Requests/Not yet

- **Edits outside the package's files** (all one-line or minimal, list
  for the integrator):
  - `src/query/engine.rs` `eval_legacy_similarity` (the `rs:SIMILARITY`
    node path): its one `self.graph_view()` call now goes through
    `self.legacy_ok(…)?` because `graph_view()` returns `Result` (it is
    cancellable).
  - `src/query/engine.rs` `eval_builtin` arm `"SIMILARITY"` (upstream's
    Jaro): calls `jaro_similarity_cancellable` with the evaluator's cancel
    flag (analytics#5); not one of the `rs:` arms granted to this package.
  - `src/query/engine.rs` `eval_aggregation`: besides the per-group
    graph-function call loop, one `fixed_gfn` map declaration right before
    the group loop (the memo of variable-free calls).
  - `src/query/engine.rs` `AlgoCache` (struct): new fields `entity_view`,
    `has_cycle`; `closeness` is now per node.
  - `tests/dt_gfun_metric.rs` `cancellation_is_honoured` and
    `tests/dt_gfun_path.rs` `every_path_function_honours_cancellation`:
    a doc comment only ("review analytics#11: …"), assertions unchanged.
- **For the closing step / other owners**: `conf/conf.ini.example`
  (`[query] gfun_max_output_bytes` comment, line ~295) and the doc comment
  of `GStoreConfig::gfun_max_output_bytes` in `src/config.rs` still say
  "one graph-function call"; they should say "one query" (not this
  package's files, no config key was added).
- **Deferred within the ids**: see the REFACTOR_BACKLOG notes above
  (analytics#10 layer unification, PPR per-source O(n), inherent quadratic
  costs). `analytics#2` is documentation only, behaviour unchanged (upstream
  compatibility).
- The mid-run-cancellation tests use the new hook API, so against the
  unfixed code they fail to compile rather than fail an assertion; every
  other new behavioural test was run against the unfixed code and failed
  there (output peaks, per-query budget, Louvain determinism, scan counter,
  allocation totals, Searcher matrix size, unordered-order peak, SIMILARITY
  and legacy mid-run polls).
