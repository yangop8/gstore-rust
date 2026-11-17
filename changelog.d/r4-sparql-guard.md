# r4-sparql-guard — Executor differential guard, one EvalSettings value at every construction site, optimizer bounds, remaining SPARQL lows, numeric/optimizer test hygiene

## Changelog

Behaviour changes first:

- **Behaviour change (review sparqlconf#15): a non-grouped variable in an
  aggregate query's projection is a query error.** SPARQL 1.1 §11.4: in a
  query with `GROUP BY`, or with an aggregate in the SELECT clause, `HAVING`
  or `ORDER BY` (one implicit group), a variable of the WHERE pattern may be
  projected — or used in a SELECT expression outside an aggregate — only as
  a GROUP BY key. `SELECT ?s ?o … GROUP BY ?s` used to show the value of each
  group's first solution; it now fails with `?o is not grouped: … (SPARQL
  1.1 §11.4); group by ?o or project (SAMPLE(?o) AS ?…)` (HTTP: the usual
  query-error status). Still legal: GROUP BY keys (also `GROUP BY (expr AS
  ?k)`), aggregates, earlier SELECT aliases, trailing `VALUES` variables,
  variables the pattern never binds, and — a documented leniency — a
  sub-expression equal to a GROUP BY expression (`(STR(?o) AS ?k) … GROUP BY
  STR(?o)`). Sub-SELECTs follow the same rule. `HAVING` and `ORDER BY` of an
  aggregate query still see the group's first solution for a non-grouped
  variable (unchanged). No upstream conformance script projects a
  non-grouped variable, so there is no compat switch.
- **Behaviour change (review sparqlconf#17): the string and hash functions
  check their argument types.** `STRLEN`, `SUBSTR`, `UCASE`, `LCASE`,
  `CONTAINS`, `STRSTARTS`, `STRENDS`, `STRBEFORE`, `STRAFTER`, `CONCAT`,
  `REGEX`, `REPLACE`, `STRLANG` and `MD5` / `SHA1` / `SHA256` / `SHA384` /
  `SHA512` used to read the lexical form of any term, so `STRLEN(1)` was 1,
  `CONCAT("a", 1)` "a1", `REGEX(<iri>, "x")` true, `SHA1("abc"@en)` a hash,
  `STRLANG("a"@fr, "en")` "a"@en, and `FILTER(STRLEN(?v) = 2)` kept the
  integer 30. One shared check now follows SPARQL 1.1: a string literal
  (simple, `xsd:string` or language-tagged, §17.4.3.1.1) where the spec says
  so; a simple literal for the regex pattern, flags and replacement, both
  `STRLANG` arguments and the hash input; and the argument-compatibility
  rules (§17.4.3.1.2) for `CONTAINS` / `STRSTARTS` / `STRENDS` /
  `STRBEFORE` / `STRAFTER` (`STRBEFORE("abc"@en, "b"@cy)` is an error). Any
  other argument is an error: unbound in a SELECT expression, false in a
  FILTER. Write `STRLEN(STR(?v))`, `REGEX(STR(?iri), …)` for the old
  reading. Still lenient: `ENCODE_FOR_URI`, `LANGMATCHES`, `STRDT`'s
  lexical form, the gStore extensions (`CONTAINALL`, `CONTAINANY`,
  `SIMILARITY`) and `SUBSTR` positions of any numeric type. The upstream
  conformance queries (regex / contains / ucase / lcase / strstarts on
  strings and tagged strings) are unaffected.
- **Behaviour change (review query#14): `BOUND` of a variable the pattern
  never binds is false, `BOUND` of a substituted term true.** It was an
  error, so `FILTER(!BOUND(?z))` dropped every row, `(BOUND(?z) AS ?b)` was
  unbound, and `FILTER EXISTS { … FILTER(BOUND(?outer)) }` was always false
  (inside EXISTS an outer-bound variable is replaced by its term).
- **The large-BGP plan search is bounded and cancellable (review query#13).**
  A BGP of more than 14 patterns is planned by a greedy order refined with
  2-opt local search, which scored up to 16 passes of n²/2 swaps with fresh
  hash maps each and never looked at the cancel flag (a 400-pattern star
  planned for 6 s, a cancel waited for it). Orders are now scored with
  reusable prefix statistics (exactly the old costs: plans are unchanged),
  the search stops after `OPT_WORK_BUDGET` = 2²¹ work units (one per
  pattern placement scored), skips 2-opt above `MAX_2OPT_PATTERNS` = 256
  patterns, and polls the query's stop check (cancel flag, refused
  `SERVICE`, failed sub-SELECT) every `POLL_EVERY` = 4,096 units; a plan
  cut short is not cached.
- **One `EvalSettings` value at every evaluator construction site (review
  design#9).** New public `gstore::query::engine::EvalSettings` (`SERVICE`
  policy, integer division, Top-K options, graph-function and path budgets,
  PFN limits and own caller, user functions, VS policy; `Default` = what
  `Evaluator::new` starts with), `EvalProviders` (the CSR and substring-index
  caches, both decided per site), `Evaluator::with_settings(settings,
  providers, request_pfn)`, `StringIndexCell` and
  `Database::eval_settings()`. `Database::read_as`, the UPDATE WHERE clause
  (default graph, `WITH`, `USING`), an HTTP transaction's read snapshot and
  `ConcurrentDb` snapshots and version views all apply the one value; the
  snapshots store it instead of seven copied fields. Effects: reads of an
  open transaction's snapshot and of `ConcurrentDb` keep their substring
  index across queries (seeded from the database's cached one) instead of
  rebuilding it per CONTAINS/REGEX query; `WITH` / `USING` updates use the
  database's substring index; a `ConcurrentDb` version view whose state is
  the base snapshot's uses its cached CSR. `Evaluator::with_vstree` reuses
  `new`'s struct literal.
- New `tests/executor_differential.rs` (review query#17 / design#10): the
  materializing and the streaming executors (with and without `LIMIT`,
  bounded heap, DP-B) must answer alike — as bags, blank-node labels erased —
  every `tests/w3c` case (queries and updates), the 14 LUBM queries (as
  written, `+ LIMIT 7`, `+ ORDER BY DESC(?first) LIMIT 7`) and a 92-query
  probe battery, on every layout (memory, snapshot, disk Materialize and
  Stream, RocksDB with `--features rocksdb`). 7,060 runs (8,825 with
  RocksDB), no divergence found (an injected streaming bug is reported on all three batteries). A
  `KNOWN` list (review id per entry, XPASS when gone) is empty. A
  `#[doc(hidden)]` `force_streaming_on_this_thread` hook (in
  `gstore::query::engine`) runs queries as written through the streaming
  executor.
- Test hooks (`#[doc(hidden)]`): `optimizer::work_on_this_thread`,
  `bushy_plans_on_this_thread`, `set_bushy_plans_on_this_thread`,
  `set_poll_hook_on_this_thread`; `engine::watch_evaluator_settings` /
  `SeenSettings` (what each evaluation was configured with).
- Tests: `tests/eval_settings_reach_every_read_path.rs` (every read/update
  path, a transaction snapshot over HTTP, ConcurrentDb snapshots and version
  views); `tests/aggregate_projection_must_be_grouped.rs`; `dt_optimizer`
  bounds the plan-search work of 40/120/200-pattern stars and stops it with
  a cancel at its first poll; W3C cases `functions-args/` (BOUND, string /
  hash argument types, argument compatibility), `aggregates-grouping/`
  (legal grouped projections) and `describe/`; the xfail marker of
  `tricky/bound-of-a-variable-not-in-the-pattern` is gone.
- Test hygiene: `dt_optimizer` checks that each bow-tie query got a bushy
  plan and that the left-deep pipeline gives the same answer, and its
  no-bridge data now reaches the optimizer (review tests-b#14); the
  optimizer unit test no longer passes silently without a tree; `dt_num`
  compares against the largest / smallest salaries strictly and non-strictly
  (review tests-b#12); `numeric_tower` asserts the SUM over decimals
  (`"3.75"^^xsd:decimal`), decimal + integer, `ABS(-2.5)` and an exact sum
  beyond i64 (review tests-b#13).
- DESCRIBE's description and the syntax-tree cap are documented in
  `eval_describe`'s rustdoc (review sparqlconf#19).

## GAP_PLAN status line

none

## Docs notes

- README.md (SPARQL 1.1 bullet / known limitations): DESCRIBE describes a
  resource by the triples it is the subject of — not a Concise Bounded
  Description: blank-node objects are not followed (so a blank node's own
  triples are reachable only through a pattern), incoming triples and named
  graphs are not described. A request's syntax tree is capped at 1,024
  links (`MAX_TREE_HEIGHT`), counting every `UNION` branch, `||` / `&&`
  operand and `/` step of a chain: a generated query with more than about a
  thousand chained alternatives is refused as "nested too deeply" (review
  sparqlconf#19).
- README.md: aggregate queries reject non-grouped projections (§11.4); the
  string and hash functions reject non-string arguments, with the lenient
  list above (reviews sparqlconf#15, sparqlconf#17).
- CHANGELOG.md, Phase 3 "Known gaps": the entry "String functions do not
  apply SPARQL's argument-compatibility rules (…), and UCASE / LCASE on an
  IRI or a number return a string instead of an error" is resolved by
  sparqlconf#17; what stays lenient is listed above.
- README.md "Library API": `Database::eval_settings()` and
  `Evaluator::with_settings` / `EvalSettings` / `EvalProviders` /
  `StringIndexCell` for embedders that build their own evaluators.
- docs/REFACTOR_BACKLOG.md (D3, review query#17 / design#10): merge the two
  executors — make the materializing evaluator a consumer of the streaming
  one so every algebra operator has one implementation; until then
  `tests/executor_differential.rs` guards their agreement (rerun it with the
  whole W3C subset after every round). Split `engine.rs`'s 4.2k-line
  `Evaluator` impl into submodules (BGP, expressions/built-ins, aggregation,
  paths, SERVICE, graph functions) — a pure move. Consolidate `Database`'s
  query / build / open entry points (`execute_as(Statement, ExecOptions)`,
  an OpenOptions-based builder). `ResultSet` stores N-Triples strings that
  each serializer re-parses per cell: move it to terms / ids with lazy
  rendering (query#17).
- docs/REFACTOR_BACKLOG.md (design#9 remainder): the static call checks of
  `Database::run_update` and of the reasoning rules, and the cluster /
  store convenience evaluators (`kvstore::store`, `cluster`), still build
  evaluators with `Evaluator::new` (+ `with_functions`): they only check
  calls or run library-internal queries without database settings; move
  them to `with_settings` when one of them needs a setting. The evaluator keeps its settings as individual
  fields (copied by `graph_sub_evaluator`'s full struct literal, which the
  compiler keeps complete); storing one `EvalSettings` field instead is a
  follow-up.
- docs/REFACTOR_BACKLOG.md (sparqlconf#19): Concise Bounded Description for
  DESCRIBE (follow blank-node objects); flatten n-ary `UNION` / `||` / `+`
  chains so they do not count toward the syntax-tree height.
- docs/REFACTOR_BACKLOG.md (sparqlconf#15 remainder): `HAVING` / `ORDER BY`
  of an aggregate query referring to a non-grouped variable see the
  group's first solution instead of an error.
- docs/REFACTOR_BACKLOG.md (tests-b#14 remainder): the engine unit test
  `bushy_execution_matches_left_deep` (`src/query/engine.rs` tests) still
  says the optimizer "may pick" a bushy tree; dt_optimizer and the
  optimizer unit tests now require it, and the engine test can use
  `set_bushy_plans_on_this_thread` the next time that module is edited.
- docs/REFACTOR_BACKLOG.md (query#13): the greedy planner
  (`src/query/planner.rs`) is not counted against the work budget; it is
  O(n²) at worst in the pattern count (no 2-opt term), measured below.

## Measured numbers

- Plan-search scaling (review query#13): a star `SELECT ?s { ?s :p_i ?o_i }`
  of n patterns over 20 subjects, one `Database::select` each, test profile
  (`cargo +1.99.1 test`), other worktrees' builds running. Base baeae13
  (the old optimizer in this worktree): n = 50 / 100 / 200 / 400 →
  14.1 ms / 116 ms / 778 ms / 6,074 ms (×8 per doubling). Branch: 5.3 ms /
  34.6 ms / 146 ms / 6.5 ms (n = 400 is above `MAX_2OPT_PATTERNS`: greedy
  only). Work units (`work_on_this_thread`): unbounded, one 2-opt pass at
  n = 200 is 3.98 M; the branch stays ≤ 2²¹ + n at every size
  (`dt_optimizer::large_star_plan_search_stays_within_its_work_budget`,
  `optimizer::tests::the_2opt_search_stays_within_its_work_budget`; both
  fail against the unbounded search).
- Executor differential suite: 4,380 W3C runs (219 cases × 4 layouts × 5
  settings), 840 LUBM runs, 1,840 probe runs, about 4-8 s wall clock
  (`cargo +1.99.1 test --test executor_differential`, test profile); with
  `--features rocksdb` 5,475 + 1,050 + 2,300 runs in 17 s. No divergence.
- Bench smoke gate, base baeae13 vs branch 540ec3c (the code of this
  fragment's commit), measured one after the other in the same session:
  release builds of `gbuild` + `gstore-bench` (`cargo +1.99.1 build
  --release --locked -p gstore -p gstore-bench --bin gbuild --bin
  gstore-bench`, base from a `git archive` export, each with its own target
  dir), data cache warm, command `gstore-bench ci --baseline
  bench/baselines/smoke.json --bin-dir <its release dir>`. Both PASS, 456
  gates checked, 0 failed. Time geomean vs the committed baseline, base →
  branch: LUBM-like snapshot 0.90 → 0.90, LUBM-like disk 1.23 → 0.86,
  WatDiv-like snapshot 0.97 → 0.92, WatDiv-like disk 1.18 → 0.93 (bound
  1.50). Machine state: the base run overlapped this worktree's
  stream-mode workspace test run and other worktrees' builds (4 CPUs), so
  the base disk figures are high from load, not a branch gain; the point is
  that the hot paths (engine, top-k, optimizer, the read path's
  `with_settings`) did not regress. The smoke workload has no BGP over 14
  patterns, so the 2-opt bound does not show there.
- `scripts/conformance.sh --no-build` (dev binaries of this branch): all
  six suites PASS (parser: the known cases 15 and 17 only), no compat
  switch needed.

## Requests/Not yet

- Edits outside the owned files (all inside the package's sanctioned
  regions unless noted): `src/query/engine.rs` — the `EvalSettings` /
  `EvalProviders` / `StringIndexCell` / `SeenSettings` /
  `watch_evaluator_settings` types next to the Evaluator struct,
  `with_vstree`, `with_settings`, the BOUND arm, the string/hash argument
  checks (`STRLEN`, `CONCAT`, `REGEX`, `SUBSTR`, `REPLACE`, `STRLANG`, `hash1`,
  `str1`, `str_split`, `str2_bool`, the new `string_literal` /
  `simple_literal` / `argument_compatible` helpers replacing
  `str_and_lang`), `eval_aggregation`, `eval_describe`'s rustdoc, the
  optimizer call site in `prepare_bgp`, the `pub use topk::{…}` line; and,
  outside the named regions, one `self.note_settings();` line at the top
  of `Evaluator::evaluate` and of `Evaluator::eval_update_modify` (the test
  hook). `src/query/topk.rs` — `force_streaming_on_this_thread` after
  `TopKOptions`, and `q.limit` / `c.limit` → `.or_else(forced_limit)` in
  `select_fast_path` / `construct_fast_path` (one line each, the forcing
  hook's only effect). `src/db/mod.rs` — `eval_settings()` (new, before
  `read_as`), `read_as`, `eval_modify_changes`' `prepare()` and its three
  call lines plus the substring-index closure moved before the USING
  branch, and the module's `use crate::query::engine::{…}` line.
  `src/http_api.rs` — the `ReadSnapshot` settings fields, `capture`'s
  settings lines, `configure`, two lines in `ReadSnapshot::read_as` (the
  substring-index provider of each branch, passed to `configure`), and the
  module's query imports (a `#[cfg(test)]` `IntegerDivision` import kept for
  the tests module). `src/concurrent.rs` — the `Snapshot` settings fields,
  `from_db`'s settings lines, `Snapshot::query`, `VersionView::query`, the
  module's query imports. r4-concurrent edits the rest of `concurrent.rs`
  this round: the integrator keeps both sides (the `Snapshot` struct and
  `from_db` literal may conflict textually).
- For the round's land step: the executor differential suite and the W3C
  subset must be rerun on the merged tree (cases from parallel packages meet
  only there; convention 13e).
- Flaky, not this package's (each failed once in a full workspace run and
  passed in every other run, five reruns of the second in Stream mode all
  green): `concurrent::tests::concurrent_readers_never_see_torn_state`
  ("readers should have run": the writer finished its 400 commits before
  any reader thread ran a query — it asserts on scheduling; r4-concurrent
  owns `concurrent.rs`'s tests this round) and
  `no_background_thread_outlives_serve_forever::the_asynchronous_operation_runner_is_joined`
  (Stream mode: one `gstore-task` thread still listed right after the join).
- Nothing deferred: every id of the package is fixed; the refactors the
  review suggested (executor merge, engine split, Database entry points,
  CBD) are recorded in the backlog notes above (D3).
