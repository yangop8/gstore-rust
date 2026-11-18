# r4-ext-docs — Reasoning and PFN defects; architecture documentation

## Changelog
- **Behaviour change (review ext#1): a per-query PFN fuel budget, and
  instantiation is charged.** The new `[pfn]` key `pfn_fuel_per_query`
  (`PfnLimits::fuel_per_query`, default 10^10 = ten calls' worth of the
  default `pfn_fuel_per_call`, `0` = unlimited) bounds the fuel all the PFN
  calls of one query use together: every argument combination, every
  `PFN.name(…)` of the statement, its sub-queries and `GRAPH` blocks. Past
  it the query fails with `run f fail: the instruction budget of N for the
  PFN calls of one query (pfn_fuel_per_query) is exhausted`. Before, one
  query could make 1 000 000 calls (`pfn::call::MAX_COMBINATIONS`) of a
  billion instructions each. A call's instantiation is now charged before
  it runs, one fuel unit per 8 bytes of the module's initial linear memory
  and data section (plus one), against both budgets, and refused before any
  memory is allocated when it does not fit: a module declaring 1024 pages
  (64 MiB) costs 8 Mi units per call (it ran under a budget of 100
  instructions before, 35 ms per call). A deployment running very large PFN
  queries (hundreds of thousands of calls, or calls near the per-call
  budget) raises `pfn_fuel_per_query` or sets it to `0`. `PfnCaller` counts
  the fuel: clones share the count (one query's evaluators), while `new`,
  `with_limits`, `as_user` and the new `for_query()` start a new one; the
  new `PfnCaller::fuel_spent()` reads it. `CallOptions` gains `spent:
  Option<&AtomicU64>` (`None`: the call is a query of its own). Every query
  path installs its caller through `with_limits` or `for_query`
  (`ConcurrentDb` snapshots and version views included), so repeated
  queries never share a budget. **Round-4 review fix: one budget per
  request**, also for an UPDATE: every evaluator installed its caller with
  a fresh count and an UPDATE builds one per `DELETE` / `INSERT … WHERE`
  operation, so a request of N operations could use N budgets. The
  request's entry (`Database::run_update`, `Database::read_as`, a
  transaction snapshot's read) now starts one count (`for_query` on the
  request's caller, else the installed one), and `Evaluator::with_settings`
  keeps a request caller's count (new `PfnCaller::limited(limits)`: same
  caller and count under other limits); the installed caller of a
  snapshot read still gets a count of its own per query. A library caller
  passed to many `query_as` calls still gives each request its own budget.
  `tests/pfn_fuel_is_counted_per_request.rs` (feature `pfn-wasm`) fails
  before the fix: an update of two operations whose PFN calls together
  pass the budget ran.
- **PFN functions of any size run (review ext#2).** The wasmi runtime
  translates modules eagerly, when they are compiled and loaded. With
  wasmi's default lazy translation a function's translation (7 fuel units
  per byte) was charged at its first call and a shortfall of the current
  fuel slice was reported as an error, not as resumable: every call of a
  function body above about 150 KB failed (`ran out of fuel while calling a
  resumable function: required_fuel=1260028`), also with
  `pfn_fuel_per_call=0`, and a 9 KB helper first called after a loop
  failed for some loop lengths (n=114000, 228000, … in the test) and kept
  failing. A function's size no longer counts against its budget.
- **PFN compile builds what `data.json` records (review ext#8).** `funcudb`
  type 4 / `PfnRegistry::compile` compiles `generate_source(record)` and
  rewrites the `.wat` source file from it when it differs; it never
  compiles the file. An update interrupted between writing the source file
  and `data.json` (a crash, or a failed write whose rollback failed too)
  used to make the function run code `funquery` did not show. With no
  record and no source file the upstream message `Function source file dose
  not exist` is kept.
- **Behaviour change (review ext#3): RDFS materialization never makes a
  literal or blank node a predicate.** A literal or blank node in an
  `rdfs:subPropertyOf` (or `domain` / `range`) position — legal RDF; OWL
  writes `:p rdfs:subPropertyOf [ owl:inverseOf :q ]` — was interned as a
  predicate and data was copied under it (`<a> "lit" <b> .`), which neither
  `gbuild` nor the N-Triples reader loads back (the export → `gbuild` round
  trip lost statements, exit status 3). Only IRIs are properties now;
  `subPropertyOf` stays transitive through a blank node; a literal an older
  version interned as a predicate is ignored. (`Database::materialize_rdfs`
  is a library API.)
- **Textual rules (`Database::add_rule` / `RuleSet`, library API; review
  ext#4, ext#7).** Non-ASCII or malformed rule text is an error: the
  tokenizer sliced by bytes and panicked (`é <p> ?y => …`, `"\é"`). Terms
  are checked with the crate's one term validator (`model::term`) and
  decoded with `Term::from_dict_key`, replacing the forked term parser with
  its own escape rules: a rule IRI with a space or `<>"{}|^`\`, a literal
  escape other than `\\ \" \n \r \t`, an invalid language tag, blank label
  or variable name is now refused (behaviour change for such texts). The
  body join is a hash join over dictionary ids instead of a nested loop
  over every binding and stored pair: a two-hop rule over 8000 edges takes
  20 ms instead of 2.6 s. Textual rules are documented as not persisted
  (save, backup and open do not carry them; D2 keeps the API).
- **Rule directories are written all-or-nothing (review durability#11).**
  `reason::store::copy_rule_dir` (backups and restores) and
  `write_memory_rules` (the save or backup of a detached database's
  in-memory rules) stage the new `reason_rule_files/` beside the
  destination, fsync every file and directory, and swap it in with
  `fsutil::StagedDir`; a removal renames the directory aside durably first.
  They used to remove the destination and write file by file without
  fsyncing, so a crash or a full disk lost the old rules and left rule
  files without ledgers (which `reasonManage` takes for upstream rules and
  retracts by label). An interrupted swap is finished by the next operation
  that takes the database's rule lock. `write_memory_rules` takes the
  destination's rule lock without waiting (a save calls it under the
  database's `persist_lock` and the directory's writing exclusion, which
  contract C8 orders after the rule lock, and `reasonManage`'s disable
  saves while it holds the rule lock): while another operation of the
  process holds it, the write is refused with an error and nothing is
  changed. Round-4 review fix: a detached database's `save` (in-memory
  rules, `into_memory()`) takes that lock — still without waiting —
  before it writes its snapshot and holds it through both writes, so a
  refused save leaves the destination as it was; it used to install the
  new snapshot first and then have the rules refused, pairing the new data
  with the directory's old rules and ledgers.
- **Documentation (review ext#15, design#16, parser#17).** Every reasoner
  (RDFS, textual rules, `reasonManage`) reads and writes the default graph
  only — now said in `src/reason` and `docs/DESIGN.md`. `docs/DESIGN.md`
  §1/§3/§5/§6/§7 describe the current architecture (layer overview,
  copy-on-write and out-of-core dictionaries, streaming RDF readers,
  SPARQL 1.1 Query + Update, the `Evaluator` over the `TripleSource` seam
  with its two executors — materializing `eval_*` and streaming / Top-K
  `stream_*` — the DP optimizer, the three storage layouts); §8 is marked as
  the historical record of the initial trunk. The crate docs (`src/lib.rs`)
  carry a module map by layer. `ast.rs` no longer calls OPTIONAL / UNION /
  aggregates / paths / sub-queries deferred and documents SERVICE failures
  as query errors; `stream.rs` states the blank-label guarantee and the
  lookahead the Turtle reader really uses. `docs/PFN.md` documents fuel,
  `pfn_fuel_per_query`, eager translation and compile-from-record.
- **Tests (review ext#14, tests-b#5).** New `tests/reason_pfn_regressions.rs`
  (every finding above, PFN parts under `pfn-wasm`; the durability#11 case
  makes the write fail with a real `EFBIG` in a child process under
  `RLIMIT_FSIZE`); unit tests in `src/reason/mod.rs` (non-IRI
  subPropertyOf, non-ASCII rule text, join work counter, join shapes) and
  `src/reason/store.rs` (a crash at every point of both rule-directory
  writes, an interrupted swap finished) and `src/pfn/wasm.rs` (the
  instantiation charge counts the initial memory and the data section).
  The join-scaling test times its runs by the thread's CPU time, not the
  wall clock, so the machine's load (parallel tests, other builds) cannot
  move its ratio. `tests/dt_phase4_review_fixes.rs`
  `a_cancelled_execute_leaves_the_rule_as_it_was` now requires a
  cancellation: a flag raised before the call must cancel the execute, and
  an error in the timed sweep must be the cancellation (with execute
  ignoring its flag the old test still passed; the new one fails).

## GAP_PLAN status line
none

## Docs notes
- README.md, the `[pfn]` keys paragraph (next to `pfn_fuel_per_call`): add
  `pfn_fuel_per_query` (default 10000000000, `0` = unlimited): the fuel all
  the PFN calls of one query may use together. Say that fuel counts
  instructions, host bytes and each call's instantiation (initial memory
  and data section, one unit per 8 bytes). docs/PFN.md already says it.
- docs/PFN.md (owned here, edited in the round-4 review fix): the
  `pfn_fuel_per_query` row says every request starts from zero and the
  operations of one UPDATE share one budget. README.md / DEPLOY.md: the
  same wording wherever `pfn_fuel_per_query` is described ("per request",
  not "per query or operation").
- docs/DEPLOY.md, the limits paragraph: `pfn_fuel_per_query` is a limit
  key, not an upstream-compat switch (upstream runs PFN as native code
  without limits; the runtime is a Rust extension behind `pfn-wasm`), so it
  needs no entry in the compat-switch table and conformance.sh needs no
  switch. Deployments with very large PFN queries raise it or set `0`.
- README.md, the reasoning paragraph (RDFS materialization, textual rules,
  `reasonManage`): RDFS materialization and textual rules are library APIs
  (no server operation runs them); textual rules are not persisted; every
  reasoner reads and writes the default graph only (review ext#15).
- docs/REFACTOR_BACKLOG.md, deferrals of this package (D3/D8):
  - review ext#1, the per-call overhead outside fuel: every call still
    builds a `Store`, a `Linker` with 28 host functions and an instance
    (a trivial call of a generated one-page module takes 24.7 µs in the
    test profile and is charged 8 197 units, about what its time is worth
    at the measured 3-4 ns per unit). A complete module declaring no memory
    is charged one unit plus its data section for the same fixed work, so a
    million trivial calls of such a module cost seconds of CPU that the
    per-query budget does not count; the work stays bounded by
    `pfn::call::MAX_COMBINATIONS` (1 000 000). Building the linker / an
    `InstancePre` once per (engine, module) and a configurable
    combination cap are the follow-ups. A wall-clock bound per query is
    `[server] query_timeout` (the deadline raises the cancel flag PFN calls
    check between slices).
  - review ext#7: textual rules still run naive (not semi-naive) fixpoint
    evaluation — every round re-derives everything (rounds × O(join)); the
    join itself is linear now. They stay unpersisted by design (library
    API, D2); compiling them to the `reasonManage` / `INSERT … WHERE` path
    and persisting them is the refactor the review suggests.
  - review query#17 / design#10 (recorded in DESIGN.md §6, owned by
    r4-sparql-guard): the two executors.
  - review durability#11 (found in this package's review): `Database::save`
    writes a detached database's in-memory rules
    (`reason::store::write_memory_rules`) with its `persist_lock` and the
    directory's writing exclusion held, after the rule lock in contract
    C8's order, so the write takes the rule lock without waiting and is
    refused while another operation holds it. Taking the destination's
    rule lock in `save` before `persist_lock` (src/db/persist.rs, which no
    package owned in round 4) would let it wait instead.
- docs/STORAGE_BACKEND.md: no format change. The rule-directory swap uses
  the `STORE_SWAP` sibling names inside the database directory
  (`reason_rule_files.tmp` / `.new` / `.old`); an older binary ignores
  them, and a leftover is finished or removed by the next rule operation.

## Measured numbers
Test profile (`opt-level = 1`), 4-vCPU machine shared with three other
agents' builds:
- **ext#7, two-hop textual rule over a chain** (`Database::run_rules`, one
  run each): 1000 edges 53 ms → 2.1 ms; 8000 edges 2.58 s → 20 ms (ratio
  49× before, about 10× after for 8× the data). The unit test's work
  counter (pairs read + bindings built) grows 8.0× for 8× the edges.
- **ext#1**: `spin` (10 000 loop iterations) uses 98 208 fuel per call:
  about 9 units per iteration plus 8 193 for instantiating one page. A
  module declaring 1024 pages is charged 8 388 609 per call before it runs.
  Calibration (scratch measurement, not committed): a trivial call of a
  one-page generated module takes 24.7 µs and is charged 8 197 units
  (3.0 ns per unit); a 100 000-iteration loop takes 3.70 ms for 908 208
  units (4.1 ns per unit). So the default `pfn_fuel_per_query` (10^10) is
  about 30-40 s of interpretation in the test profile, and the charge of
  the generated prelude's one page is about the fixed cost of a call.
- **ext#2**: on the previous code the 1 MB function failed with
  `required_fuel=1260056`, and the late-helper sweep (120 loop lengths,
  6 000 apart) failed at n=114000, 228000, … (period ≈ 115 000
  iterations of one 1 Mi-unit fuel slice). All pass now.
- Bench smoke gate (D9): not run — no hot path is touched (PFN runtime,
  reasoning, rule store and documentation only; `src/concurrent.rs` gets
  one call per query path changed from `clone()` to `for_query()`).
- **Verification at the branch head** (after the package's review):
  - `cargo fmt --all -- --check` clean.
  - `cargo clippy --workspace --all-targets -- -D warnings` clean;
    `cargo clippy -p gstore --all-targets --features "zip pfn-wasm
    readline" -- -D warnings` clean.
  - `RUSTDOCFLAGS='-D warnings' cargo doc -p gstore --no-deps --locked`
    clean.
  - `cargo test --workspace`: 2926 passed, 0 failed, 9 ignored, both with
    `GSTORE_DISK_QUERY_MODE=memory` and with `stream`.
  - `cargo test -p gstore --features pfn-wasm`: 2726 passed, 0 failed, 8
    ignored (`reason_pfn_regressions` 10 passed, `dt_pfn`,
    `dt_phase4_review_fixes`, `dt_phase4_wiring` included).
  - The review reverted every fix at once (the base `src/reason/mod.rs`
    and `store.rs`, lazy translation, a free instantiation, `clone()`
    instead of `for_query()` in `src/concurrent.rs`, compile from the
    source file, execute ignoring its cancel flag): all 10
    `reason_pfn_regressions` tests and the tests-b#5 test failed, among
    them `the_calls_of_one_query_share_a_budget` in its loop of repeated
    snapshot queries (`pfn_fuel_per_query` exhausted).
  - Every new regression test was run against the code before its fix
    (the fixes stashed): all failed there — RDFS export (`left: 4, right:
    2` inferred, gbuild exit 3), add_rule (panic at reason/mod.rs:706),
    rule-dir write (`[("a", false), ("big", true)]`), 1 MB function,
    late helper, instantiation (`Ok`), ext#8 (`"8"`) — except the timing
    ratio test, whose first version (1000 vs 4000 edges, bound 10×) passed
    on the old code under load; it was widened to 1000 vs 8000 edges, bound
    24×, against the measured 49× of the old code (the per-query budget
    test does not compile against the old API). tests-b#5 was checked by
    mutation (execute passing no cancel flag: the new test fails).

## Requests/Not yet
- Edits outside the owned files:
  - `src/config.rs` (sanctioned spot): `pfn_fuel_per_query` directly after
    `pfn_max_functions` in KNOWN_KEYS, the struct, Default, `set()` and
    `value()`, and its one line in `query_settings()`'s `PfnLimits`
    initialiser. Not sanctioned but required by the new `PfnLimits` field:
    the test `every_key_is_read_from_its_section` gets
    `pfn_fuel_per_query=9` on its `[pfn]` text line and `fuel_per_query: 9`
    in its expected `PfnLimits` literal (r4-cli-config owns the rest of
    config.rs; expect a trivial conflict there). The config module doc's
    `[pfn]` key list (`//! * [pfn]: … pfn_max_functions`) does not name the
    new key yet — for r4-cli-config or the integrator.
  - `conf/conf.ini.example`: the commented `pfn_fuel_per_query` line
    (with its comment) after `pfn_max_functions`.
  - `src/concurrent.rs`, `Snapshot::query` and `VersionView::query`
    (r4-sparql-guard's functions this round): one line each,
    `eval.with_pfn(caller.clone())` → `eval.with_pfn(caller.for_query())`.
    Needed: the snapshot keeps one caller for all its queries, so with
    `clone()` they would share one per-query budget.
    `tests/reason_pfn_regressions.rs::pfn::the_calls_of_one_query_share_a_budget`
    guards it; if r4-sparql-guard's EvalSettings moves the caller, the
    integrator keeps `for_query()` (or `with_limits`) there.
  - `src/parser/trig.rs`: one test comment (`non_ascii_graph_label_does_not_panic`)
    no longer names the removed `strip_graph_keyword` (parser#17).
- tests/dt_phase4_review_fixes.rs: only `a_cancelled_execute_leaves_the_rule_as_it_was`
  changed (tests-b#5, sole editor this round); comments `review tests-b#5`.
- Found in this package's review, outside its files (durability#11
  follow-up for whoever owns them next): three paths delete a replaced
  database's `reason_rule_files/` with a plain recursive delete —
  `gbuild --force`'s `drop_old_rules` (src/bin/gbuild.rs), the bulk
  install's `remove_rule_dir` (src/bulk/commit.rs) and the REST rebuild in
  src/http_api.rs. A crash part-way through such a delete can leave rule
  files without their ledgers (which `reasonManage` takes for upstream
  rules); `fsutil::discard_dir` (rename aside durably, then delete) avoids
  that. `drop_old_rules` also takes no rule lock, so a
  `reason_rule_files.new` an interrupted swap left there is not finished
  or removed first and comes back as the rebuilt database's rules at the
  next rule operation (the other two lock, which finishes the swap before
  they delete). Needs a crash inside the swap's last rename followed by a
  forced rebuild; not reproduced.
- ext#14 asks for local-console tests too (`gconsole --local` snapshot
  updates lost on quit, statements split at `;`): those belong with ext#5 /
  ext#9, which are not in this package; this package's tests cover the
  reasoning and PFN parts.
