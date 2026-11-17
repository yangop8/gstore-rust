# r3-sparql-joins — Join operators in both executors: hashed OPTIONAL/MINUS, GRAPH ?g compatibility, correlated EXISTS, SELECT alias scoping, SERVICE errors

## Changelog

Behaviour changes first:

- **A failed non-`SILENT` `SERVICE` fails the query** (review robust#6,
  tests-b#2, sec#17). A variable endpoint, an inner pattern the request
  cannot carry (anything but triple patterns, groups and `UNION`), a
  connection, HTTP or JSON error used to make the `SERVICE` yield no
  solutions and the cause was dropped, so a federated query against a dead
  or broken endpoint answered `200` with no rows (or, under `OPTIONAL`,
  with every remote variable unbound) — indistinguishable from an endpoint
  without data. As SPARQL 1.1 Federated Query §4 requires, the statement
  now fails with a query error naming the endpoint and the cause (for
  example `query error: SERVICE <http://host/sparql>: endpoint returned
  HTTP 500`, `… bad JSON: …`, `SERVICE ?ep: a variable endpoint is not
  supported …`), the way a `SERVICE` refused by policy already did.
  `SERVICE SILENT` is unchanged: a failure is the one empty solution, so
  the outer solutions survive. Clients that relied on the empty answer add
  `SILENT`. Once a `SERVICE` has failed the query, no further request is
  sent: an `EXISTS { SERVICE … }` evaluated per solution (in a projection,
  `BIND` or `ORDER BY`) contacted the endpoint once per solution, each up
  to the client's timeouts.
- **`GRAPH ?g { P }` joins `?g` with P's own binding of it** (review
  query#3, sparqlconf#1). Both executors set `?g` to each named graph's
  name over whatever P had bound it to, so `SELECT ?g ?t WHERE { GRAPH ?g
  { ?g :created ?t } }` returned a row for every subject of the graph,
  `ASK { GRAPH ?g { ?s :p ?g } }` was true when no graph names itself, and
  `INSERT { ?g :meta ?t } WHERE { GRAPH ?g { ?g :created ?t } }` wrote
  those wrong triples durably. A solution is now kept only when it leaves
  `?g` unbound or binds it to that graph (SPARQL 1.1 §18.5 Graph(var, P)),
  also when `?g` is used in predicate position inside the graph. Inside
  `EXISTS`, a graph variable the current solution binds to a literal names
  no graph (no solution) instead of ranging over every graph.
- **`EXISTS` / `NOT EXISTS` honour the current solution inside `VALUES`
  blocks and sub-SELECTs** (review query#4). `?s :p ?o FILTER EXISTS {
  VALUES ?s { :a } }` kept every `?s`, the `NOT EXISTS` form dropped every
  row, and a sub-SELECT's projected `?s` (for example `GROUP BY ?s HAVING
  (COUNT(*) > 1)`) was accepted whatever it bound. The current solution is
  still substituted into the pattern (triple patterns, paths, FILTERs and
  BINDs see its values, §18.6); a variable the substituted pattern can
  still bind — a `VALUES` variable, a sub-SELECT's projection, a `BIND`
  target, a graph name bound to a blank node — must now agree with it. A
  sub-SELECT's own, unprojected variables stay independent (§12).
- **SELECT expressions extend the solution left to right, each evaluated
  once, with or without `ORDER BY`** (review query#6, sparqlconf#6).
  Without `ORDER BY` a SELECT expression could not see an earlier alias
  (`SELECT (1 AS ?a) (?a + 1 AS ?b) {}` left `?b` unbound; `SELECT ?s
  (STRLEN(?n) AS ?l) (?l * 10 AS ?m)` left `?m` unbound, with `LIMIT` too);
  with `ORDER BY` every expression was evaluated a second time at
  projection, against all aliases — an earlier expression saw later ones
  (`(?b + 1 AS ?a) (1 AS ?b) … ORDER BY ?b` gave `?a = 2`), a function ran
  twice per row, and `(RAND() AS ?r) … ORDER BY ?r` printed unsorted
  values. All paths (materializing, streaming `LIMIT`, bounded heap, DP-B)
  now evaluate each expression once per solution, seeing exactly the
  aliases to its left, and project those values. An alias that names a
  WHERE variable (a syntax error in SPARQL 1.1, still accepted) is
  evaluated at projection as before.
- **OPTIONAL, MINUS and joins over maybe-unbound variables are hash joins
  in both executors** (review sparqlconf#2, query#10). The materializing
  executor compared every left solution with every right one for
  `OPTIONAL` (20,000 x 10,000 rows: 3.4 s against 9 ms on the streaming
  `LIMIT` path, quadratic growth), both executors scanned the right side
  per left solution for `MINUS`, and a join fell back to the full cross
  product as soon as one solution left a shared variable unbound (a join
  after an `OPTIONAL`: 14.7 s). All of them now probe a hash index of the
  right side that also handles unbound shared variables (see the new
  `src/query/probe.rs`), so they cost about the size of their output; the
  answers and their order are unchanged. (Round-3 review correction: one
  index of the whole right side was built and kept per distinct subset of
  the shared variables a left solution binds, so 1,000 VALUES rows binding
  different subsets cost 1.6 GB on 10,000 triples for a one-row answer; the
  partial indexes are now bounded to 4x the right side's rows, past which
  a group is scanned — see r3-integration's round-3 review fixes.) The `OPTIONAL` loop also polls
  cancellation between left solutions.

New files and tests:

- `src/query/probe.rs`: `RowIndex`, the hash index of a solution set
  grouped by which shared variables each row binds, with lazily built
  indexes for partially bound probes; used by `eval_left_join`,
  `eval_minus`, `join_sets` and the streaming executor's `Probe` / `MINUS`.
  Unit tests compare it with the scan definition and count probe visits.
- `tests/sparql_join_semantics.rs`: every fixed operator through the
  materializing and streaming executors (the streaming settings with a
  `LIMIT` larger than the answer must give the same rows in the same
  order), a scaling test (OPTIONAL, MINUS and a join after an OPTIONAL over
  8x the data, linear in both executors and the full path about as fast as
  the streaming `LIMIT` path), a cancellation poll
  test, GRAPH ?g / INSERT, EXISTS, alias, evaluated-once and SERVICE error
  tests (fake HTTP endpoints; a failed SERVICE is contacted once per
  query), and a randomized differential battery.
- W3C-style cases (`tests/w3c_subset.rs`): `tests/w3c/optional-minus/`
  (6), `graph/` (6), `exists/` (6), `projection/` (6), `service/` (3); the
  `xfail` markers of `tricky/graph-variable-bound-inside-the-graph`,
  `tricky/update-with-graph-variable-bound-inside`,
  `tricky/exists-with-values-sees-outer-bindings` and
  `tricky/select-expression-sees-earlier-alias` are removed.
- `tests/dt_service.rs`: `service_non_silent_dead_endpoint_returns_no_rows`
  pinned the old empty answer; it is corrected (review tests-b#2, D6) to
  `service_non_silent_dead_endpoint_fails_the_query`.

## GAP_PLAN status line

none

## Docs notes

- CHANGELOG.md: the five behaviour changes above, the SERVICE one first
  (it turns an `Ok` answer into an error for clients that relied on it).
- docs/CPP_PARITY.md (row "SERVICE联邦查询"): a non-`SILENT` failure is
  now the query's error naming the endpoint and the cause; a variable
  endpoint is an error (`SILENT`: the empty solution), not supported.
  (r4-concurrent owns the file in R4.)
- docs/REFACTOR_BACKLOG.md:
  - query#17 / design#10 (unchanged scope, for r4-sparql-guard): the two
    executors still implement every algebra operator twice. The operators
    fixed here now share `RowIndex` (probe.rs) and the projection's alias
    columns (`OrderCtx` / `order_extend`), and
    `tests/sparql_join_semantics.rs` runs them through both executors;
    the general differential guard is round 4's.
  - query#3 / sparqlconf#1 (performance only): `GRAPH ?g { P }` evaluates P
    in every named graph with `?g` free and then joins. Substituting the
    graph's name for `?g` in P's triple patterns (not in its FILTERs, which
    per §18.5 see `?g` unbound unless P binds it) would let a BGP use it as
    a constant; not done.
  - query#4 (spec ambiguity, unchanged): `EXISTS` substitutes the current
    solution into the pattern (§18.6 `substitute`), so a `MINUS` inside
    `EXISTS` whose shared variables are all bound outside shares no
    variable after substitution and removes nothing — the literal §18.6
    reading (the known SPARQL 1.1 EXISTS erratum), kept as it was.
  - robust#6 / tests-b#2: `SERVICE ?var` (a variable endpoint, evaluated
    per outer solution) stays unsupported; it is now a clear error.
  - query#6: an alias that names a WHERE variable (`SELECT (?o AS ?s) WHERE
    { ?s ?p ?o }`, a syntax error per SPARQL 1.1 §18.2.1) is still accepted
    and evaluated at projection; rejecting it at parse time is a separate
    decision.
- README.md: nothing beyond the CHANGELOG entries (no option or key).
- docs/DEPLOY.md: no secure default or compat switch in this package.

## Measured numbers

- Scaling (`tests/sparql_join_semantics.rs`
  `optional_minus_and_unbound_joins_scale_linearly`, test profile
  opt-level 1, `cargo +1.99.1 test --test sparql_join_semantics`, other
  worktrees' builds running): full (materializing) path at 5,000 / 20,000
  subjects, and the streaming `LIMIT` path at 20,000.
  - Base 0f74615: OPTIONAL 200 ms / 3.38-3.51 s (streamed 8-9 ms); MINUS
    10 ms / 160-163 ms (streamed 163-171 ms, also quadratic); join after
    an OPTIONAL 0.91-1.09 s / 14.6-15.6 s (streamed 14.5-15.3 s: the
    streaming `Probe` scanned too, the probed side had an unbound key).
  - Branch: OPTIONAL 1.4 ms / 6.8 ms (streamed 8.9 ms); MINUS 1.0 ms /
    5.3 ms (streamed 6.5 ms); join after an OPTIONAL 4.1 ms / 25.5 ms
    (streamed 36.1 ms). All linear (about 4-6x for 4x the data).
  - Independent review: the test as first written measured the streaming
    path at 20,000 only, so a regression of the streaming `Probe` alone
    (join after an OPTIONAL streamed: 7.4 s) still passed it. It now runs
    2,500 / 20,000 subjects and requires both executors to grow at most
    20x (+30 ms) for the 8x data. Same session, same command: base 0f74615
    full / streamed at 2,500 → 20,000: OPTIONAL 49 ms → 3.17 s / 0.97 ms →
    8.1 ms; MINUS 2.7 ms → 156 ms / 2.8 ms → 163 ms; join after an
    OPTIONAL 209 ms → 14.6 s / 204 ms → 13.4 s (all three fail). Branch:
    OPTIONAL 0.67 ms → 6.2 ms / 0.99 ms → 8.7 ms; MINUS 0.51 ms → 4.8 ms /
    0.77 ms → 6.1 ms; join after an OPTIONAL 1.9 ms → 24 ms / 2.9 ms →
    31 ms. With only the streaming `Probe` and both MINUS operators
    reverted to the old scans, the MINUS and the streamed join-after-OPTIONAL
    rows fail (156 ms and 7.4 s against bounds of 82 ms and 2.2 s).
- Bench smoke gate, base 0f74615 vs branch c8d10b9, measured one after
  the other in the same session. Release builds of `gbuild` +
  `gstore-bench` (`cargo +1.99.1 build --release --locked -p gstore -p
  gstore-bench --bin gbuild --bin gstore-bench`, each copied aside), the
  data cache warm, command `gstore-bench ci --baseline
  bench/baselines/smoke.json --bin-dir <its binaries>`. Both PASS, 456
  gates checked, 0 failed. Time geomean vs the committed baseline, base →
  branch: LUBM-like snapshot 1.07 → 0.92, LUBM-like disk 1.06 → 0.86,
  WatDiv-like snapshot 1.03 → 0.93, WatDiv-like disk 1.08 → 0.97 (bound
  1.50). The smoke workload has no OPTIONAL / MINUS / GRAPH ?g query, so
  the branch's lower figures are run-to-run noise, not a gain; the point is
  that the hot paths did not regress. Machine state: other worktrees'
  builds and tests were running (load average 8 to 9 on 4 CPUs).
- `scripts/conformance.sh --no-build` on the branch (dev binaries from the
  workspace test build): all six suites pass (basic, update, transaction,
  api, parser with only the known cases 15 and 17 failing, python
  sdk_check 49 passed). No compat switch was needed.
- With `--features rocksdb` (shared target-rocks, under its lock):
  `w3c_subset` (layouts memory, snapshot, disk/materialize, disk/stream,
  rocksdb), `sparql_join_semantics` and `dt_service` pass.

## Requests/Not yet

- Edits outside the owned files (all inside this package's R3 regions
  unless noted): `src/query/engine.rs` — a `mod probe` declaration and
  `use probe::RowIndex` next to `mod topk`; new private methods
  `graph_name_agrees` (after `eval_graph`) and `same_term_or_unbound`
  (after `eval_exists`); `eval_service`'s doc comment.
  `src/query/topk.rs` — a new method `pattern_has_solution_where` after
  `pattern_has_solution`, and one-line `project_items` call edits (a new
  `where_len` argument) in `heap_top_k_distinct` and `dpb_top_k`, which no
  other R3 package touches. `stream_join` / `join_dfs` are untouched;
  `Probe::new` / `Probe::for_each_compatible` keep their signatures (their
  internals now use `RowIndex`), so r3-sparql-paths' `stream_path_join`
  needs no change. The deleted `minus_removes` had no other caller.
- W3C harness (r2-w3c-harness's format, for r4-sparql-guard or later): it
  has no way to expect an error, so the non-`SILENT` SERVICE failures are
  covered by Rust tests only; the `service/` cases cover `SILENT`, with a
  variable endpoint (no network). An `expected.error` marker would let the
  subset pin query errors too.
- Bench (r5-bench): the smoke workload has no `OPTIONAL` / `MINUS` query,
  which is why the quadratic paths were invisible there; adding one would
  guard this fix.
- All ten ids of the package are fixed; nothing deferred.
