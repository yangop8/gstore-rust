# r2-w3c-harness — W3C-style SPARQL regression harness (D5) and the standard sub-SELECT forms

## Changelog

Behaviour changes first:

- **A sub-SELECT parses as a whole group in every position** (review M14:
  parser#3, sparqlconf#5). SPARQL 1.1's `GroupGraphPattern ::= '{' (
  SubSelect | GroupGraphPatternSub ) '}'` was only honoured in the nested
  form `{ { SELECT … } }`; `WHERE { SELECT … }`, `ASK { SELECT … }`,
  `OPTIONAL` / `MINUS` / `GRAPH ?g` / `SERVICE` / `FILTER (NOT) EXISTS
  { SELECT … }`, `CONSTRUCT` / `DESCRIBE` / `INSERT` / `DELETE … WHERE
  { SELECT … }` failed with "found 'SELECT'", and `{ SELECT … } UNION
  { … }` with "found 'UNION'". They all parse now; the nested form gives
  the same pattern as before. A pattern after the sub-query inside the same
  group (`{ SELECT … } ?s ?p ?o` within one pair of braces) stays a syntax
  error, as the grammar requires, and the message now says to nest the
  sub-query as a group of its own.

New tests and tools:

- **`tests/w3c/` + `tests/w3c_subset.rs`: a hand-authored W3C-style SPARQL
  1.1 subset** (review parser#18, query#18, sparqlconf#18; decision D5).
  Each case is a directory `tests/w3c/<area>/<case>/` with `data.ttl` or
  `data.trig`, `query.rq` or `update.ru`, the expected answer
  (`expected.srj` SPARQL 1.1 Query Results JSON, `expected.nt` for
  CONSTRUCT/DESCRIBE, `expected.nq` for the dataset after an update) and an
  optional `xfail` marker whose first line names a review id; the query
  cites its spec section in a `# spec:` line. The answers are worked out
  from the SPARQL 1.1 Query / Update text, not from the engine. The harness
  runs every case on the in-memory build, a saved-and-reopened snapshot, the
  disk store in Materialize and in Stream mode and (with `--features
  rocksdb`) RocksDB, each through the materializing evaluator
  (`TopKOptions::NONE`) and the streaming executor with the bounded heap and
  with DP-B forced (a SELECT/CONSTRUCT without a top-level `LIMIT` gets
  `LIMIT 1000000` for the streaming settings; a self-test proves through
  `EvalStats` that the streaming paths then run). Updates run on a fresh
  database per layout and setting and are read back through the N-Quads
  export, again after reopening. SELECT answers compare as bags (in order
  under `ORDER BY`, ties as bags), graphs and datasets as sets, blank nodes
  up to a one-to-one relabelling (backtracking search), RDF terms exactly
  (numbers not normalised; a simple literal equals its `xsd:string` form and
  language tags ignore case, as in RDF 1.1). A case that misses without the
  marker fails the run, and so does an `xfail` case that passes everywhere
  (whoever fixes the bug deletes the marker). Self-tests: wrong expectations
  (a row, a count, a typed literal) fail on every layout and executor, a
  passing `xfail` case fails the run, malformed cases are rejected,
  blank-node isomorphism / tie groups / the query scanner have unit tests.
  `W3C_FILTER=<substring>` runs a subset. The format is documented in
  `tests/w3c/README.md`.
- 119 cases: `basic/` 61 (BGP, literals, blank nodes, FILTER, OPTIONAL,
  UNION, (NOT) EXISTS, MINUS, paths, BIND, VALUES, aggregates, ORDER BY /
  LIMIT / OFFSET, DISTINCT, SELECT expressions, CONSTRUCT, ASK, GRAPH,
  functions, seven SPARQL Update forms; all pass), `subquery/` 16 (every
  sub-SELECT position; 14 fail before the parser fix, all pass after),
  `tricky/` 42 (§8.3 MINUS vs NOT EXISTS, filter scope, error semantics of
  `!`, `||`, `&&`, numeric promotion, `=` vs `sameTerm`, ORDER BY over term
  kinds, aggregates over no solutions, empty group, CONSTRUCT skipping
  ill-formed triples, …; 25 pass, 17 are `xfail`). The xfail cases record
  confirmed engine bugs for their owners: query#2, sparqlconf#3, query#9,
  sparqlconf#4, sparqlconf#13 (r3-sparql-paths), query#3, sparqlconf#1,
  query#4, query#6 (r3-sparql-joins), query#7, query#8, sparqlconf#7,
  query#11, sparqlconf#8, query#5 ×2 (r3-sparql-functions), query#14
  (r4-sparql-guard). Run time: about 3 s for the suite in a debug build, 10 s
  with RocksDB.
- **`tests/dt_sparql11.rs` compares full typed terms** (sparqlconf#18,
  query#18): its `contains("8000")` / `contains('2')` assertions (which
  accepted "18000", "12" or another datatype) now compare the whole result
  cell, e.g. `"2"^^xsd:integer` per department by name and
  `"3166.666666666666666667"^^xsd:decimal` for AVG; GROUP_CONCAT is checked
  as a simple literal holding exactly the three names.
- `src/parser/stream_tests` documents that its frozen-copy oracle only
  checks self-consistency (parser#18) and points to `tests/w3c/` for
  semantic expectations.

## GAP_PLAN status line

none

## Docs notes

- README.md (testing section): `cargo test --test w3c_subset` runs the
  hand-authored SPARQL 1.1 conformance subset on every storage layout and
  both executors (`--features rocksdb` adds RocksDB); the case format is in
  `tests/w3c/README.md`. Mention that SPARQL behaviour changes should come
  with a case there, citing the spec section.
- docs/REFACTOR_BACKLOG.md (parser#18, D3/D8): the W3C rdf-tests (Turtle /
  TriG / N-Triples / N-Quads / RDF/XML syntax suites) and the official
  SPARQL 1.1 syntax manifests are not vendored (the machine had no reliable
  access to w3.org / GitHub, and D5 asks for hand-authored cases); the
  reader-side items of parser#18 — a term round-trip property test (reader
  term → dict_key → N-Triples → reader) and a lexer complexity test — are
  left to r3-parser (parser#4/#5/#8/#10 own those code paths) and
  r2-terms' shared term validator. tests/w3c covers SPARQL only.
- docs/REFACTOR_BACKLOG.md (query#17 / design#10 context): the streaming
  and materializing executors are cross-checked case by case by
  tests/w3c_subset.rs (settings `full`, `stream`, `dpb`); the executor merge
  itself stays with r4-sparql-guard.
- CHANGELOG.md: the sub-SELECT bullet above under behaviour changes
  (parser; no wire-format or on-disk change).

## Measured numbers

No hot path changed (the parser change only moves where `SELECT` is
recognised inside a group), so no bench smoke run was needed (convention 8).
Suite run times on this machine (4 vCPU, other builds running at the same
time): `cargo test --test w3c_subset` 3-4 s wall in the test profile
(119 cases × 4 layouts × 3 settings, plus self-tests); with `--features
rocksdb` 8-10 s.

## Requests/Not yet

- **New divergence found by the harness (for r3-sparql-functions, owner of
  query#5):** a *labelled* blank node in a CONSTRUCT or INSERT template
  (`_:v`) is copied verbatim instead of becoming a fresh blank node for
  every solution (SPARQL 1.1 Query §16.2.1, Update §3.1.3); anonymous `[]`
  template nodes are fine. `instantiate_template` (src/query/engine.rs)
  treats `PatternTerm::Term(Term::Blank)` as a constant. For INSERT this is
  durable: every solution, and every later request, writes the same stored
  node — the same failure mode as query#5's BNODE() labels. Recorded as
  `tests/w3c/tricky/construct-labelled-blank-node-template` and
  `tests/w3c/tricky/update-insert-template-blank-node`, both `xfail`
  naming query#5 with a note; the fix for query#5 should cover both (and
  delete both markers).
- **Round-2 ids deliberately without a w3c case:** sparqlconf#14 (simple
  literal vs `xsd:string`, r2-terms) and query#1 (substring index on
  non-canonical numerals, r2-index-cancel) are fixed in this same round by
  other packages; an `xfail` case for them would turn into an XPASS at the
  round-2 merge. From round 3 on (convention 7), a case for each belongs in
  tests/w3c (data from the review: `"Dave"^^xsd:string` vs `"Dave"`;
  `"0012"^^xsd:integer` with ≥ 64 dictionary strings and
  `CONTAINS(STR(?c), "001")`).
- **For the round-2 integrator:** tests/w3c_subset.rs reads result cells
  through `QueryResult::Select(ResultSet { vars, rows: Vec<Vec<Option<
  String>>> })` and `gstore::parser::ntriples::parse_term`; if r2-terms
  (query#15) changes the cell type, `answer_of()` / `T::from_cell()` adapt
  in a line or two. After the merge, re-run `cargo test --test w3c_subset`
  (convention 13e).
- **Not covered by the subset, on purpose:** DESCRIBE (its answer is
  implementation-defined), `SERVICE` (needs a network endpoint), whether
  the default graph contains the named graphs (implementation-defined,
  §13), `RAND` / `NOW` / `UUID` / `BNODE` values, and negative syntax tests
  (the format has no "expect an error" file; the sub-SELECT negatives are
  Rust tests in w3c_subset.rs).
