# r3-sparql-functions — Built-ins and aggregates: fresh BNODE labels, thread-safe RAND/UUID and per-query NOW, XPath REGEX/REPLACE, MIN/MAX/AVG, hashed DISTINCT aggregates

## Changelog

Behaviour changes first:

- **Behaviour change (review query#5, H-18): `BNODE()` and `BNODE(str)` are
  fresh blank nodes.** `BNODE()` minted `_:b0`, `_:b1`, … from a counter
  that restarted with every process, so two processes, or one server before
  and after a restart, inserting `BIND(BNODE() AS ?b)` wrote the same stored
  node and merged two resources for good. It now mints the engine's fresh
  labels (`genid-<per-process nonce>-<counter>`, as `INSERT DATA` and `[]`
  template nodes already did). `BNODE(str)` was `_:b<hash of the string>`,
  one node for every solution, query and process; it is now one fresh node
  per string *within a solution mapping* — the same for every call in that
  solution (also `BIND(BNODE("x") AS ?a) BIND(BNODE("x") AS ?b)`, and across
  a `GRAPH` block) and a different one in every other solution and query
  (SPARQL 1.1 §17.4.2.9). Only the labels change; nothing on disk.
- **Behaviour change (query#5, found by the round-2 w3c harness): a labelled
  blank node in a CONSTRUCT / INSERT template (`_:v`) is a fresh node per
  solution**, the same across the template triples of one solution (Query
  §16.2.1, Update §3.1.3); `[]` / `( … )` template nodes already were. It
  was copied verbatim, so every solution and every later request wrote the
  one stored node `_:v`. A CONSTRUCT whose template holds one is no longer
  served from the query cache. In a DELETE template (where SPARQL forbids
  blank nodes) a labelled node still names the stored node of that label,
  as in WHERE patterns and `DELETE DATA`, so `DELETE WHERE { <a> <p> _:v }`
  with a label a query returned deletes the triple it matched (unchanged).
  Labelled blank nodes in WHERE patterns and in `INSERT DATA` are unchanged
  (see Requests).
- **`UUID()` / `STRUUID()` / `RAND()` are safe under concurrency (review
  conc#1, M10).** They shared one process-wide xorshift state advanced by a
  non-atomic load-then-store, so concurrent queries (the servers' shared read
  path, or updates on two databases) drew the same numbers: about 0.5 % of
  concurrent `STRUUID()` calls were duplicates, minting the same `urn:uuid:`
  IRI for two resources. `UUID()` / `STRUUID()` now take 16 bytes of OS
  randomness each (`/dev/urandom`, read 512 bytes at a time per query; std's
  OS-keyed SipHash where there is none) and format a version-4 UUID;
  `RAND()` draws from a per-query xoshiro256** generator seeded from OS
  randomness. No shared generator state is left.
- **`NOW()` is one instant per query (review query#16).** It read the clock
  at every call, so a query spanning a second boundary returned several
  instants (§17.4.5.1 requires one). The instant is taken at the first call
  and shared with `GRAPH` blocks; every `Evaluator::evaluate` /
  `eval_update_modify` call is a new execution with its own instant and
  `BNODE` nodes, also on an evaluator a library caller reuses.
- **Behaviour change (review query#11, sparqlconf#8, M14): `REGEX` and
  `REPLACE` follow XPath `fn:matches` / `fn:replace`.** Flags: `s`
  (dot matches newline), `m` (multi-line), `i`, `x` (whitespace outside
  character classes removed) and `q` (literal pattern; turns `m`/`s`/`x`
  off) are honoured — only `i` was — and any other flag character is an
  error (FORX0001), so `REGEX(?x, "a", "u")` filters the row out instead of
  ignoring the flag. `REPLACE`'s replacement string uses XPath's syntax:
  `$N` with XPath's digit rule (`$1x` is group 1 followed by `x`; it was the
  regex crate's group named `1x`, i.e. empty), `\$` / `\\` for a literal
  `$` / `\`, a group above the count up to 9 is the empty string, a lone `$`
  or `\` is an error (FORX0004), and with `q` the replacement is literal. A
  pattern that matches the empty string is a `REPLACE` error (FORX0003;
  `REPLACE("abc", "", "-")` was `"-a-b-c-"`). The pattern syntax itself is
  still the regex crate's (back-references, class subtraction and
  `\p{IsBlock}` names stay unsupported).
- **REGEX / REPLACE compile a pattern once per query (query#11,
  sparqlconf#8).** Patterns were compiled for every solution: a
  `FILTER(REGEX(?o, "^1.*9$"))` over 20,000 rows spent nearly all its time
  compiling (36 µs per row in release). A per-query cache keyed by (flags,
  pattern) compiles each once, `GRAPH` blocks included.
  `Evaluator::regex_compilations()` reports the count. The cache is bounded
  by weight, not by count: a compiled pattern keeps its program (up to the
  regex crate's 10 MiB limit) and its search caches for as long as it is
  cached, so it holds at most 64 patterns whose program fits 256 KiB, or 8
  larger ones (each weighs 8), and is emptied when a new pattern would
  exceed that; a constant pattern is then compiled again at most once per
  63 distinct patterns built from the data.
- **Behaviour change (review query#8, sparqlconf#7, M14): `MIN` / `MAX`
  use the `ORDER BY` order and return the stored term.** They used the
  partial `<`, so for an IRI and a literal, or a number and a string, the
  first value seen won: `MIN`/`MAX` over `{ <z> 3 }` gave `<z>`/`<z>`, over
  `{ 3 <z> }` `3`/`3`, depending on the input order and so on the plan.
  They now pick by the order `ORDER BY` uses (blank nodes < IRIs <
  literals, then by value; ties broken by the term), so the answer is
  independent of the input order and equals the first / last row of
  `ORDER BY`. `MIN`, `MAX` and `SAMPLE` return the group's term as stored
  and `GROUP_CONCAT` joins the terms' lexical forms: they went through the
  computed value and back, so `"5"^^xsd:int` came back an `xsd:integer`,
  `"01"^^xsd:integer` as `"1"`, `"1e3"^^xsd:double` as `"1000.0"`.
- **Behaviour change (review query#7): `AVG` divides by the number of
  values it summed.** `SUM` / `AVG` skip non-numeric values (this engine's
  documented extension; SPARQL makes the aggregate an error), but `AVG`
  divided by the number of all values: `AVG` over `10` and `"x"` was `5.0`,
  now `10.0`. A group with no numeric value averages `0` (an
  `xsd:integer`), like an empty group (§18.5.1.4); it was `0.0`.
- **Behaviour change (review query#12, sparqlconf#10): `DISTINCT` in
  `COUNT` / `SUM` / `AVG` / `GROUP_CONCAT` compares RDF terms, in linear
  time.** It compared values pairwise with `Vec::contains`: quadratic
  (80,000 distinct values took 13 s in the test profile, now 25 ms; one
  million was minutes to hours of uncancellable CPU on a request), and
  `"01"^^xsd:integer` and `"1"^^xsd:integer` counted as one. A hash set of
  term ids (or of terms, for a computed argument) now drops repeated terms,
  so the two count as two (the known gap recorded in CHANGELOG Phase 3 is
  closed). `COUNT` / `SUM` / `AVG` without `DISTINCT` still work on values.
- **DESCRIBE de-duplicates its resources with a hash set (query#12,
  sparqlconf#10).** A linear `contains` per solution made it quadratic in
  the number of resources (160,000: 2.1 s, now 33 ms in the test profile).

New files and tests:

- `src/query/xpath_regex.rs`: XPath flags, replacement strings, the
  empty-match rule and the per-query pattern cache (with unit tests).
- The per-query state of these built-ins lives in one value
  (`QueryCtx`: `NOW()`'s instant, the `BNODE(str)` map, the minted blank
  nodes, the `RAND()` generator, the UUID randomness, the pattern cache),
  created by `Evaluator::new` / `with_vstree` and shared by `Rc` with the
  `GRAPH` sub-evaluators; `evaluate` / `eval_update_modify` start a new
  execution in it (instant and `BNODE` nodes forgotten).
- `tests/sparql_builtins.rs`: 19 tests, each failing on the old code (the
  compile-count test needs the new `regex_compilations()`, so on the old
  code it does not build): `BNODE(str)` per solution (projection and
  `BIND` chains), `BNODE()` across two `gquery` processes, blank-node and
  `NOW()` state across `GRAPH`, template blank nodes across solutions,
  requests and CONSTRUCT runs (and the cache), 4 threads × 202,500 rows of
  `UUID()` / `STRUUID()` / `RAND()` without a repeat, `NOW()` before and
  after a clock tick, XPath flags / replacement strings / errors, the
  compile count and REGEX-vs-STRSTARTS cost, `MIN`/`MAX` against `ORDER BY`
  over every rotation of a mixed set, terms kept, the `AVG` divisor,
  `DISTINCT` on terms, and linear scaling of aggregate `DISTINCT` and
  `DESCRIBE` (against the same query without the de-duplication, and
  with 16× the data; D9). Added in review: a labelled blank node in a
  DELETE template / `DELETE WHERE` still deletes the stored node of that
  label while the INSERT template's is fresh
  (`delete_templates_keep_labelled_blank_nodes`; its DELETE half passes on
  the base and failed on the first version of this package), and an
  evaluator reused for a second query gets a new `NOW()` and new
  `BNODE("x")` nodes (`a_reused_evaluator_starts_every_query_afresh`); unit
  test `xpath_regex::tests::the_cache_is_bounded_by_weight`.
- `tests/w3c/functions/` (6 cases: §17.4.3.14 / §17.4.3.15 and the XPath
  F&O §5.6.3 examples, flag `q`, the error cases) and
  `tests/w3c/aggregates/` (5 cases from §18.5.1: MIN/MAX over blank node /
  IRI / literal and across numeric types, terms kept by MIN/MAX/SAMPLE/
  GROUP_CONCAT, COUNT(DISTINCT) on terms, AVG of integers). The `xfail`
  markers of `tricky/bnode-of-a-string-is-fresh-per-solution`,
  `tricky/construct-labelled-blank-node-template`,
  `tricky/update-insert-template-blank-node` (query#5),
  `tricky/regex-flags` (sparqlconf#8),
  `tricky/replace-xpath-replacement-string` (query#11),
  `tricky/min-max-across-term-kinds` (query#8) and
  `tricky/min-max-independent-of-input-order` (sparqlconf#7) are removed:
  those cases pass on every layout and executor.
  `tricky/avg-over-an-error-is-unbound` keeps its `xfail` (query#7) with a
  rewritten note: the divisor is fixed; what remains is the documented
  skip-non-numeric extension.
- Corrected test (D6): `builtin_tests::bnode_uuid_struuid_shapes` asserted
  that `BNODE("seed")` starts with `_:b` (the old hash label); it now checks
  the node kind (`_:`), with a `review query#5` comment.
- Corrected case (review): `aggregates/count-distinct-compares-terms`
  expected `SUM(DISTINCT ?v)` over `"01"`, `"1"` and `"x"` to be `2`, which
  is the engine's skip-non-numeric extension, not SPARQL (the sum over `"x"`
  is an error, §18.5.1.3; `tricky/avg-over-an-error-is-unbound` tracks
  that divergence). The case now filters to numeric values, so every
  expected value is the spec's.

## GAP_PLAN status line

none

## Docs notes

- CHANGELOG.md (Phase 3 "Known gaps"): drop "`COUNT(DISTINCT …)` compares
  values, so `"01"^^xsd:integer` and `"1"^^xsd:integer` count as one there"
  (fixed, query#12); keep "`SUM` / `AVG` skip non-numeric values instead of
  failing" and add that `AVG` divides by the number of values summed
  (query#7). Change the blank-node item to: a labelled `_:b` in a WHERE
  pattern still matches only that stored node (upstream's reading), and in
  `INSERT DATA` it still names the same node for every request; in
  CONSTRUCT / INSERT / DELETE templates it is now a fresh node per solution
  (query#5).
- README.md (SPARQL support / functions): `REGEX` / `REPLACE` take the
  XPath flags `s m i x q` (others are an error) and XPath replacement
  strings (`$1`, `\$`, `\\`); the pattern syntax is the regex crate's
  (no back-references, no class subtraction). `BNODE`, `UUID`, `STRUUID`,
  `RAND` are fresh per call / per solution across processes; `NOW()` is one
  instant per query (whole seconds, UTC).
- docs/REFACTOR_BACKLOG.md (query#7, D3/D8 context): spec-conformant error
  semantics for `SUM` / `AVG` over a non-numeric value (the aggregate is an
  error and the variable unbound, SPARQL 1.1 §18.5.1.3–4) stay an open
  divergence; the engine keeps its documented skip-non-numeric extension
  (fixed decision of this package), tracked by
  `tests/w3c/tricky/avg-over-an-error-is-unbound` (`xfail` query#7).
- docs/REFACTOR_BACKLOG.md (query#11): a translation of XPath regular
  expression syntax to the regex crate's (back-references, character-class
  subtraction `[a-z-[aeiou]]`, `\p{IsBlock}` names, XPath's `.` excluding
  `\r`) is not done; such patterns stay an evaluation error or follow the
  crate's reading.
- docs/REFACTOR_BACKLOG.md (query#5): the `BNODE(str)` per-solution map
  keeps one entry per (solution, string) for the whole query (needed because
  a `BIND` chain is evaluated column by column), so its memory is
  proportional to the solutions that call `BNODE(str)`; duplicate solution
  mappings (identical bindings) share their node, which matches the
  multiset semantics of `Extend` but not every implementation.

## Measured numbers

- **Bench smoke gate (D9)**: `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=/home/user/fix-campaign/bench-cache gstore-bench ci
  --baseline bench/baselines/smoke.json --bin-dir <bins>`, release builds;
  base = the fix-r2 head 4f0e075's `gstore-bench` / `gbuild` (its
  `src/query/` is identical to this branch's base 0f74615), branch = this
  branch. Every run **PASS, 456 gates checked, 0 failed**. Query-time
  geomean vs the committed baseline, three base and three branch runs
  interleaved (load average 4.7–9 on 4 CPUs, other worktrees building):

  | run | LUBM-like snapshot | LUBM-like disk | WatDiv-like snapshot | WatDiv-like disk |
  |-----|--------------------|----------------|----------------------|------------------|
  | base 1 / 2 / 3 | 0.69× / 0.70× / 0.67× | 0.75× / 0.80× / 0.72× | 0.78× / 0.74× / 0.75× | 0.81× / 0.81× / 0.80× |
  | branch 1 / 2 / 3 | 0.76× / 0.68× / 0.70× | 0.82× / 0.76× / 0.75× | 0.78× / 0.79× / 0.76× | 0.80× / 0.82× / 0.78× |

  No difference beyond noise (the smoke queries use none of the changed
  built-ins or aggregates; the per-query `QueryCtx` is one small allocation
  per evaluator).
- **Before/after, test profile (opt-level 1), same machine and session**
  (old code = this branch's tests against the base `engine.rs`; other
  worktrees building at the same time, load average 5–13 on 4 CPUs; best
  of 3 alternating runs, fresh evaluator, no query cache):

  | measurement | before | after |
  |-------------|--------|-------|
  | `COUNT(DISTINCT ?o)`, 10,000 / 160,000 distinct strings | 112 ms / 37.3 s | 2.1 ms / 39 ms |
  | `COUNT(DISTINCT ?o)` vs `COUNT(?o)`, 160,000 values | 37.7 s vs 32 ms | 39 ms vs 31 ms |
  | `GROUP_CONCAT(DISTINCT ?o)`, 10,000 / 160,000 | (quadratic, as above) | 2.5 ms / 44 ms |
  | `COUNT(DISTINCT STR(?o))`, 10,000 / 160,000 | (quadratic, as above) | 3.5 ms / 65 ms |
  | `DESCRIBE ?o WHERE { ?s :v ?o }`, 10,000 / 160,000 resources | 12.5 ms / 3.78 s | 1.8 ms / 31 ms |
  | `DESCRIBE ?o …` vs `SELECT ?o …`, 160,000 resources | 2.57 s vs 46 ms | 31 ms vs 29 ms |
  | `FILTER(REGEX(?o, "^1.*9$"))` vs `STRSTARTS && STRENDS`, 20,000 rows | REGEX ≥ 10× the string filter (test fails) | 8.4 ms vs 9.9 ms |
  | `REGEX` / `REPLACE` patterns compiled for the 20,000-row query | one per row | 2 (one per (pattern, flags)) |
  | duplicate `STRUUID`/`UUID` in 4 threads × 202,500 rows | > 0 (test fails at the first) | 0 of 1,620,000 |

  The scaling tests (D9) assert, at 160,000 values / resources, that the
  DISTINCT aggregate costs under 20× the same aggregate without DISTINCT
  and DESCRIBE under 20× the SELECT of its WHERE clause (about 1–2× now;
  1,200× and 55× before), and that 16× the data costs under 160× the time
  (16–20× now, about 300× before). A first version compared 4× the data
  against a 12× bound and failed once on a loaded machine with
  `--features rocksdb` (other tests of the binary running at the same
  time), hence the same-size baseline and the alternating runs.
- **Pattern cache memory (review)**, measured with regex 1.12.4 in release:
  a compiled `\w{200}` keeps about 11.7 MiB (`\w{300}` exceeds the crate's
  10 MiB program limit), and a small pattern searched over 1 MB of
  adversarial text keeps about 3 MiB of search caches. With the first
  version's bound of 1024 entries one query building its patterns from
  the data could pin about 12 GiB; with the weight bound at most 64 small
  patterns (about 200 MiB at worst, 3 MiB each) or 8 large ones are held.

## Requests/Not yet

- **Edits outside the package's owned files and listed regions** (for the
  integrator): `src/query/mod.rs` gains `mod xpath_regex;` after `pub mod
  vs_policy;`. `src/query/engine.rs`: `instantiate_template` (one new match
  arm for labelled template blank nodes — query#5's template half, which
  the round-2 harness assigned to this package's id), the new
  `Evaluator::regex_compilations()` method after `with_pfn`, the top-level
  `use regex::Regex;` line removed (no non-test use is left), and in
  `builtin_tests` the `bnode_uuid_struuid_shapes` assertion and the regex
  in `every_table_builtin_has_an_engine_arm` (now `regex::Regex::new`).
  `src/db/query_cache.rs`: `is_cacheable`'s CONSTRUCT arm also refuses a
  template with a labelled blank node (`PatternTerm::Term(Term::Blank)`).
  Added in review, also in `src/query/engine.rs`: `instantiate_template`
  and `instantiate_quad` take a `fresh_labels` flag (CONSTRUCT passes
  `true`), `eval_update_modify`'s template loop passes `false` for the
  DELETE template and `true` for the INSERT one, and the first line of
  `evaluate` and of `eval_update_modify` is `self.ctx.begin_execution();`
  (one line each; expect a trivial conflict if another round-3 package
  edits the start of those functions).
- **query#16 is partial here by plan:** its third sub-claim (`SECONDS`
  returns `xsd:integer` / `xsd:double` instead of `xsd:decimal`) is the
  `SECONDS` arm, which the plan gives to r4-db-graph (with sparqlconf#16).
  The racy generator and the per-query `NOW()` are fixed here.
- **For r4-db-graph / the parser owner (not in this package's ids):** a
  labelled blank node in `INSERT DATA` (`INSERT DATA { <a> <p> _:b }`) is
  still stored verbatim as `_:b` (`token_to_term`), so two requests share
  one node; SPARQL 1.1 Update §3.1.1 says blank nodes in `INSERT DATA` are
  fresh. Recorded in CHANGELOG's known gaps; same failure mode as query#5.
- **For r4-db-graph (sparqlconf#16: reject blank nodes in DELETE templates
  and `DELETE DATA` at parse time):** until then a labelled blank node in a
  DELETE template / `DELETE WHERE` names the stored node of that label (as
  before this package; its first version made it fresh, so such a DELETE
  matched and deleted nothing). `tests/sparql_builtins.rs`
  `delete_templates_keep_labelled_blank_nodes` pins that; when the parser
  rejects these requests, its DELETE assertions become "is a parse error"
  (convention 13(b)), and its INSERT assertion (fresh node) stays.
- **query#7** is fixed as the plan specifies (divisor = values summed,
  extension documented); the spec's error semantics stay a backlog item
  (Docs notes), so `tricky/avg-over-an-error-is-unbound` keeps `xfail
  query#7`.
- `BNODE(str)` accepts any argument's lexical form, as before (SPARQL
  requires a simple literal or `xsd:string`); argument-type checks of the
  string built-ins are r4-sparql-guard's in round 4.
