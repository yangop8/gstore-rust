# r3-sparql-paths — Property paths in both executors: negated property sets, path multisets, zero-length paths over unknown constants, bound-endpoint evaluation, deterministic order

## Changelog

Behaviour changes first (query results; no wire-format or on-disk change):

- **Negated property sets walk the right direction** (review H-16: query#2,
  sparqlconf#3). `!(p₁|…|^q₁|…)` is evaluated as SPARQL 1.1 §18.2.2.3
  translates it, `alt(NPS({p…}), inv(NPS({q…})))`: the forward members are
  excluded from one step in the walk's own direction, the inverse members
  from one step against it, and the two branches are united. Before, a set
  walked backwards (the default for `?x !:p :o`, a constant object) banned
  nothing — `?x !:knows :a` returned the very subject linked by `:knows` —
  and inverse members never walked reversed edges (`:a !^:knows ?x`,
  `?x !^:name :a`, `:b !(:age|^:age) ?x` were wrong). `!^p`, `!(p|^q)`,
  `^(!p)` and `!a` now give the spec's answers from a constant subject, a
  constant object and with both ends free, on every layout and in both
  executors. A negated step stays a set (§18.4: one solution per pair of
  nodes, however many predicates outside the set link them).
- **Sequence and alternative paths are multisets** (review M14: query#9,
  sparqlconf#4). `X P/Q Y` is a join through a fresh variable and `P|Q` a
  union (§18.4), so `:x :p/:q ?y` with two intermediate nodes has two
  solutions, `:a :p|:r ?z` returns a node reached by both alternatives
  twice, and `COUNT(*)` over such paths counts every route. `*`, `+`, `?`
  still return each node once (ALP / ZeroOrOnePath are sets), whatever the
  multiplicity of their inner path.
- **Zero-length paths match constants that are not in the data** (review
  M14: sparqlconf#13). `<unknown> :p* ?x` and `<unknown> :p? ?x` bind `?x`
  to `<unknown>`, `?x :p* <unknown>` binds `?x` likewise, `ASK { <u> :p*
  <u> }` is true, and the same holds for a predicate IRI or a literal that
  occurs in no subject/object position (§18.4 ZeroLengthPath / ALP: a
  constant end point matches itself whether or not it is a node of the
  graph). A *variable* end point bound by another pattern (VALUES, BIND,
  another graph's node) still matches only the active graph's nodes at
  length zero (`nodes(G)`), as the join semantics require.
- **Property-path solutions come in a fixed order** (review
  gap-accel-soundness#1). Reached nodes no longer come out of hash sets
  with per-evaluation random keys: they follow the stores' sorted lists,
  closures breadth first in discovery order. `LIMIT`/`OFFSET` pages of a
  path query tile its solutions (the review saw six paged rows with only
  four distinct ones) and `ORDER BY` ties over path solutions repeat from
  run to run. The known gap recorded in CHANGELOG.md (Phase 3 "Known gaps":
  "Property-path solutions come out of hash sets …") is closed.
- **A path whose end point another pattern binds is evaluated from the
  bound values** (review sparqlconf#9). In a group such as `?s :id 2990 .
  ?s :next* ?x`, `?x a :Person ; :knows+ ?y` or `?item :id ?k . ?item
  :partOf* ?root`, a path with two variable end points used to run from
  every node of the graph and be joined afterwards (O(nodes × reach): on
  the review's 3,000-node chain 4.5 million path steps, 1.7 s release /
  13 s debug and 290 MB for a 10-row answer). It is now a bind join: the
  group's BGPs are split into connected components, the parts are joined
  smallest/connected first, and as soon as one end point is bound the path
  extends every solution from the value it binds (subject side first,
  memoised per distinct value) — 10 path steps on 3,000 or 6,000 nodes. A
  solution that leaves the end point unbound (OPTIONAL) joins with the
  path's free solutions, so the answer is unchanged. The materializing
  executor (`eval_join`) and the streaming one (`stream_join`, depth first)
  follow the same plan and give the same rows in the same order; groups
  without such a path join exactly as before.

New tests and counters:

- `EvalStats::path_steps` (new public field): one per adjacency lookup a
  path's predicate or negated property set makes from a node — the work a
  path pattern costs.
- `tests/w3c/negated-property-set/` (14 cases) and
  `tests/w3c/property-path/` (32 cases), hand-authored from SPARQL 1.1
  Query §9 / §18.2.2.3 / §18.4 / §18.5: `!p`, `!^p`, `!(p|^q)`, `^(!p)`,
  `!()` and `!a` with a constant subject, a constant object and both ends
  free; `/` and `|` multisets walked forwards, backwards and with both
  ends free (and COUNT over them); closures and `?` as sets over a
  multiset step; zero-length paths over unknown IRIs, predicate IRIs and
  literals; bound-end-point joins (subject side, object side, both, two
  paths in a row, a far-end pattern, an end point OPTIONAL leaves unbound,
  inside GRAPH). 26 of them fail on the base engine on every layout and
  executor; the others are guards for the bind join (removing its node
  check makes three of them fail). The xfail markers of
  `tricky/negated-property-set-constant-object`,
  `tricky/negated-property-set-inverse`,
  `tricky/path-alternative-is-a-multiset`,
  `tricky/path-sequence-is-a-multiset` and
  `tricky/zero-length-path-from-a-node-not-in-the-data` are removed (their
  ids are fixed).
- `tests/sparql_paths.rs`: path solutions in the same order on every
  evaluation (fresh evaluators, both executors); LIMIT/OFFSET pages tile
  the full answer; ORDER BY ties repeat; both executors return the same
  rows in the same order, also for bind-joined groups (all four fail on
  the base engine); the scaling test on the review's chain (3,000 and
  6,000 nodes; subject side, object side, both ends, a far-end pattern;
  both executors) asserts the same, small `path_steps` on both sizes
  (4,504,500 on 3,000 nodes before).

## GAP_PLAN status line

none

## Docs notes

- CHANGELOG.md: the five behaviour bullets above under the campaign's
  behaviour changes (SPARQL results); remove the Phase 3 "Known gaps" line
  about property-path solutions coming out of hash sets in no defined
  order (CHANGELOG.md ~1641), now fixed.
- README.md (SPARQL support / property paths): property paths follow
  SPARQL 1.1 §18.4 — `/` and `|` keep duplicates, `*` `+` `?` and negated
  sets match distinct nodes, zero-length paths match constants absent from
  the data, solutions come in a deterministic order, and a path with an end
  point bound by the rest of the group is evaluated from that end.
- README.md / docs/BENCHMARKS.md (EvalStats): mention `path_steps` next
  to the other work counters if they are listed.
- docs/REFACTOR_BACKLOG.md (sparqlconf#9, D3): the bind join covers a path
  conjunct of a group (`Join`). A path inside an OPTIONAL / MINUS / EXISTS
  operand whose end point the other side binds is still evaluated with
  both ends free (`eval_left_join` / `eval_minus` / `eval_exists` are
  r3-sparql-joins' this round; the same `PathJoin`/`bound_path_pairs`
  machinery can serve them), and a lone path pattern under `LIMIT` is
  still evaluated in full before the first row streams (the Path arm of
  `stream_pattern`). The parts of a bind-joined group that are evaluated on
  their own (e.g. `?x :label ?l` beyond the path) are still materialized in
  full, as every conjunct of a group was before (linear, not quadratic).
- docs/REFACTOR_BACKLOG.md (D3, query#17 / design#10 context): eval_join
  and stream_join now share `PathJoin::order` for groups with a
  bind-joined path, as they share `greedy_join_order` otherwise; the
  executor merge stays with r4-sparql-guard.

## Measured numbers

Release builds of base 0f74615 and of this branch on the shared 4-vCPU
campaign VM (other worktrees building at the same time, load average 4.6 to
7.2):

- **Bench smoke gate** (`gstore-bench ci --baseline
  bench/baselines/smoke.json`, the shared bench cache): base PASS, branch
  PASS. Branch compared with base (`gstore-bench compare`): PASS, 456 gates
  checked, 0 failed. The time.geomean ratios of branch to base in these
  single runs were snapshot 1.21, disk 1.10, WatDiv-like snapshot 1.08 and
  WatDiv-like disk 1.13 (the limit is 1.50). To separate noise from
  regression, an ABBA rerun of LUBM-like univ 1 (snapshot and disk, 30
  reps, run in the order base, branch, branch, base) gave branch/base
  snapshot 1.15 then 0.90, disk 1.05 then 0.98, and base/base 0.91
  (snapshot) and 0.76 (disk). That is noise, not a regression. No smoke
  query uses a property path; on their path the only new code is the
  `PathJoin::new` check at the top of `eval_join` / `stream_join`, one
  `pattern_vars` pass over the conjuncts.
- **The review's repro** (sparqlconf#9): a 3,000-node `:next` chain with
  `:id` literals, release `gquery` on a disk database. `SELECT (COUNT(*)
  AS ?c) { ?s :id 2990 . ?s :next* ?x }`: base 1.15 to 1.27 s, branch
  0.000 s (7 ms for the whole process); `"10"` on both.
- **Path steps** (`EvalStats::path_steps`, tests/sparql_paths.rs, both
  executors) on 3,000 and 6,000 nodes alike: subject side `*` and `+` 10,
  object side 11, both ends bound 10, far-end pattern 10. The base engine's
  walk with both ends free, counted the same way, makes 4,504,500
  adjacency lookups on 3,000 nodes (Σ(3000−i) for the closures plus one
  per literal node) and about four times as many on 6,000.
- **Paths with both ends free are not slower**: on the same chain, `?s
  :next* ?x` (4.5 million solutions, COUNT) took base 1.26 to 1.38 s and
  branch 1.02 to 1.06 s; `:n0 :next+ ?x` took 1 ms on both and `?s
  :next/:next ?x` 1 to 2 ms on both.

## Requests/Not yet

- **Edits outside the named regions** (smallest possible; the integrator
  should expect them):
  - `src/query/topk.rs` `struct EvalStats`: one new public field
    `path_steps` (with its doc comment), appended after `dpb_matches`.
  - `src/query/topk.rs` module doc: the "A group of several conjuncts"
    bullet gains two sentences on the bind-joined path.
  - `src/query/engine.rs`: new private items outside the listed functions
    — `type PathSeen` directly before `bind_node`; `struct PathJoin`, `enum
    PathJoinStep`, `struct BoundPathCache`, `fn extend_by_path` and `fn
    bgp_components` directly before `struct MixedCaches`; new `Evaluator`
    methods `path_pattern_pairs`, `path_reach_into`, `negated_step`,
    `note_path_step` (path section) and `eval_path_join`, `path_join_plan`,
    `bound_path_pairs`, `is_graph_node` (directly after `eval_join`); `use
    std::borrow::Cow`. No existing function outside the grant was edited.
  - `src/query/topk.rs`: new `Evaluator` methods `stream_path_join` and
    `path_join_dfs` directly after `stream_join`, and `enum JoinProbe`
    directly after `join_dfs`; `join_dfs` itself and `Probe` are unchanged
    (stream_path_join calls `Probe::new` / `Probe::for_each_compatible`
    with their current signatures).
  - `tests/w3c/tricky/*/xfail`: five markers deleted (convention 7).
- **For a later round** (test hygiene, not edited here): the
  `unordered_paths` exemption in `tests/dt_disk_stream.rs` (~194-215; it
  compares path queries without ORDER BY only as multisets because their
  order used to be random) can now be removed, so path queries are
  compared in order between the Stream and Materialize databases.
- Not yet: see the REFACTOR_BACKLOG notes above (paths inside
  OPTIONAL/MINUS/EXISTS operands and a lone path under LIMIT are not
  bind-joined / streamed). Every id of this package is fixed.
