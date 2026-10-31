# Changelog

All notable changes to gstore-rust are recorded here.

## Unreleased

### Phase 3: SPARQL 1.1 grammar and semantics

This phase closes the SPARQL 1.1 grammar gaps of `docs/GAP_PLAN.md` §4.1 and
brings evaluation closer to the specification (§3.4). The new syntax and the
new semantics work together: for example `COPY` into a graph followed by a
`WITH` modify of that graph, `IN` over typed numbers with different lexical
forms, or a trailing `VALUES` in a query that is never cached because it
calls `RAND()` (`tests/dt_sparql_phase3_combo.rs`).

#### New SPARQL syntax

- **`IN` / `NOT IN`** (§17.4.1.9-10) in `FILTER`, `BIND`, SELECT
  expressions and `HAVING`, with the spec's error rules: an error only
  counts when no member is equal, and an empty list is false (true for
  `NOT IN`). Members compare by value (`"01"^^xsd:integer IN (1)` is true).
- **Signed numbers and `true` / `false`** as terms in triple patterns,
  `VALUES` and `INSERT/DELETE DATA` (`:p -5`, `VALUES ?x { +1.5e3 }`). The
  sign is part of the lexical form, so `+7` is stored as `"+7"^^xsd:integer`
  and a pattern `+7` does not match a stored `7` (a `FILTER(?x = 7)` does).
  `:p+ 7` is still a property path.
- **Trailing `VALUES`** after the solution modifiers of SELECT (also in a
  sub-SELECT), ASK, CONSTRUCT and DESCRIBE. Without grouping it joins the
  WHERE solutions; after `GROUP BY` / aggregation it joins the groups after
  `HAVING` and before `ORDER BY`, matching on group keys, projected
  variables and SELECT aliases, and binding any other variable (which the
  SELECT expressions can then use). `VALUES () { () }` works.
- **Blank-node property lists `[ … ]` and collections `( … )`** in subject
  and object position, nested, with `()` as `rdf:nil`. In WHERE patterns
  they match like hidden variables (never shown by `SELECT *`); in
  CONSTRUCT and INSERT templates each solution gets fresh blank nodes; in
  `INSERT DATA` they are fresh blank nodes that do not collide with stored
  ones; in `DELETE DATA`, `DELETE WHERE` and DELETE templates (including
  inside `GRAPH` blocks) they are a parse error, as SPARQL Update requires.
- **`ADD` / `MOVE` / `COPY [SILENT]`** between the default graph and named
  graphs. The same graph on both sides is a no-op; `COPY` and `MOVE` clear
  the destination first; `MOVE` drops the source. A named source graph with
  no triples is missing (an error unless `SILENT`). They roll back with a
  transaction and invalidate the query cache.
- **`CONSTRUCT WHERE { triples }`** short form. Its template's `[ … ]` /
  `( … )` blank nodes are fresh per solution, as in the long form.
- **`ORDER BY` keys** may be bare calls (`ORDER BY STRLEN(?x) COUNT(*)
  xsd:integer(?z)`) as well as `ASC/DESC(…)`, `?var` and `(expr)`.
- **`HAVING` with several constraints** (`HAVING (c1) (c2)`, bare calls
  allowed); bare built-ins such as `BOUND(?d)` or `CONTAINANY(…)` over the
  group now evaluate.
- **`GROUP BY (expr AS ?v)`** binds `?v`; `GROUP BY STR(?o)` (a bare call)
  works.
- **gStore's `CONTAINALL` / `CONTAINANY`** (2 or 3 arguments) over
  `xsd:list` literals, following upstream (a multi-character separator and
  typed / language-tagged items are handled as written rather than
  reproducing two upstream quirks).
- **Prefixed names**: backslash escapes (`ex:a\-b`), `%XX` escapes (kept
  as written), `:` inside local names (`ex:a:b`), and `-` / `.` in prefixes.

#### Stricter parsing

- An empty `GROUP BY` or `ORDER BY`, `ASC`/`DESC` without brackets, tokens
  after a trailing `VALUES`, an empty `_:` label and an exponent without
  digits (`1e`) are now parse errors. `VALUES` rejects variables and blank
  nodes in cells, rows of the wrong width and repeated variables.

#### SPARQL semantics follow SPARQL 1.1 more closely

- **Fixed: built-ins around aggregates.** `ROUND(AVG(?x))`,
  `STR(COUNT(*))`, `(SUM(?a)/COUNT(?a) AS ?avg)`, `IF`/`COALESCE` over
  aggregates and aggregates inside `HAVING` and `ORDER BY` expressions now
  evaluate (they used to be silently unbound). `HAVING`, or an aggregate in
  `ORDER BY`, makes a query an aggregate query even without `GROUP BY`; a
  later SELECT expression may use an earlier alias. `GROUP BY ?v` groups by
  the bound term itself.
- **Fixed: SPARQL Update over named graphs.** The WHERE clause of
  `DELETE/INSERT … WHERE` sees named graphs (`GRAPH <g> { … }`,
  `GRAPH ?g { … }`); templates accept `GRAPH <g> { … }` / `GRAPH ?g { … }`
  blocks (also in `DELETE WHERE`); `WITH <g>` names the default graph of
  the templates and the WHERE clause; `USING` / `USING NAMED` define the
  WHERE clause's dataset; `LOAD <src> INTO GRAPH <g>` loads into `<g>`
  (still subject to the `LoadPolicy`). Template triples that would not be
  RDF (a literal subject, a non-IRI predicate or graph name) are skipped,
  in `CONSTRUCT` too. **API:** `UpdateOp::Modify` gained `with`, `using`
  and `using_named`, its templates are `QuadPattern`s, and
  `UpdateOp::Load` gained `into`.
- **Changed: typed literals are validated.** Ill-typed literals
  (`"abc"^^xsd:integer`, `"300"^^xsd:byte`, `"1e3"^^xsd:decimal`, …; all
  integer-derived types check their range) are stored as they are but are
  never numbers: comparing them with anything but the same term is a type
  error, arithmetic on them is unbound, their effective boolean value is
  false. Valid literals compare by value (`"01"^^xsd:integer = 1`). An IRI
  compared with `=`/`!=` to a literal is now simply unequal (it used to be
  an error). `STR`, `DATATYPE`, `sameTerm` and the copy `BIND(?v AS ?w)` /
  `(?v AS ?w)` binds see a variable's or constant's term as written
  (`"05"^^xsd:unsignedByte` stays that term, with `STR` `"05"`), so
  `sameTerm("01"^^xsd:integer, 1)` is false.
- **Changed: numbers.** `xsd:float` keeps its datatype through expressions
  (`float + integer` is a float), and an integer or decimal meeting a float
  is promoted to single precision (`"0.1"^^xsd:float = 0.1`). Dividing two integers gives an
  `xsd:decimal` (`7/2` = `3.5`; it used to be an `xsd:double`), and so does
  `AVG` over integers; dividing a double by zero gives `INF`/`NaN`. `ABS`,
  `CEIL`, `FLOOR` and `ROUND` keep their argument's type, and `ROUND` rounds
  halves up (`ROUND(-2.5)` = `-2`).
- **New: upstream integer division.** `Database::set_integer_division(
  IntegerDivision::Truncate)` (and `Evaluator::with_integer_division`)
  switches to gStore's truncating integer division (`7/2` = `3`, also in
  `AVG`). Proposed `conf.ini` key: `integer_division = decimal | truncate`.
- **Changed: the query cache is bounded by memory.** It holds about 64 MiB
  of results by default (`Database::set_query_cache_size(bytes)`, 0 turns it
  off; `set_query_cache_capacity` is gone) and never caches queries using
  `NOW()`, `RAND()`, `UUID()`, `STRUUID()`, `BNODE()`, `SERVICE` or a user
  function. `Database::query_cache_len()` / `query_cache_bytes()` report its
  use. Proposed `conf.ini` key: `query_cache_size` (bytes). A CONSTRUCT
  whose template has `[ … ]` / `( … )` nodes is not cached either (it mints
  fresh blank nodes on every run).
- **Fixed: snapshot readers use the integer-division mode.**
  `ConcurrentDb` snapshots (`query`, transactions, version views) and the
  HTTP API's transaction read snapshot now divide integers in the
  database's mode instead of always using the default.
- **Fixed: `SELECT *` leaves out variables that only a `MINUS` mentions**
  (§18.2.1: MINUS adds no in-scope variables).
- **Fixed: a variable used as a predicate and bound some other way.**
  Predicates and entities are numbered by separate dictionaries, so a
  variable both in predicate position and as a subject/object, or bound by
  `UNION`, `VALUES`, `BIND`, a sub-SELECT or `MINUS`
  (`?s ?p ?o . ?p rdfs:label ?l`), compared ids from the two numberings and
  matched the wrong terms. Such a variable now always holds the term's
  entity id, its predicate positions bind a hidden predicate-id slot, and
  the join translates between the two (still index-driven). A sub-SELECT's
  predicate column joins with the same IRI bound elsewhere.
- **Fixed: string functions keep the language tag** (§17.4.3): `UCASE`,
  `LCASE`, `STRBEFORE` / `STRAFTER` (an empty simple literal when there is
  no match) and `CONCAT` when every argument has the same tag.

#### Top-K and early termination (GAP_PLAN §3.4)

Every optimization below returns exactly the rows of the full evaluation,
in the same order, ties included (`tests/dt_topk.rs` checks each query with
the optimizations off, heap-only, default and DP-B forced, over randomized
data). Side-effecting expressions (`RAND()`, `BNODE()`, `STRUUID()`, user
functions) are now evaluated only for the solutions actually consumed.

- **New: early termination.** A SELECT or CONSTRUCT with `LIMIT` and no
  `ORDER BY` streams its WHERE solutions and stops after OFFSET+LIMIT rows
  (distinct rows for `SELECT DISTINCT`); `ASK` and `EXISTS` / `NOT EXISTS`
  stop at the first solution; the graph-function `LIMIT` pushdown streams
  its first LIMIT solutions. BGPs (left-deep and bushy plans, and the
  mixed-variable pipeline above), `FILTER`, `BIND`, `UNION` and `GRAPH`
  stream fully; the left side of `OPTIONAL` / `MINUS` streams; a group of
  several conjuncts evaluates each conjunct, then streams the join;
  `VALUES`, sub-SELECTs, property paths and `SERVICE` are evaluated in full.
- **New: bounded-heap `ORDER BY … LIMIT`** (also for CONSTRUCT and with
  `DISTINCT`): O(n log k) time and O(k) memory instead of a full sort.
- **New: DP-B Top-K**, a port of upstream's DPB / FR / FQ / OW lists, for
  `ORDER BY` of an integer linear expression (`?a`, `2*?a - ?b + 3`, …)
  over a tree-shaped BGP (optionally under `FILTER`). It enumerates matches
  in key order and stops after the k-th and its ties. By default it runs
  only when the store's per-predicate statistics predict at least twice as
  many matches as values it must read (a star or one-to-many edge); cycles,
  non-integer values, a predicate variable that is also a subject/object,
  and other shapes take the heap. Upstream's quirks are not copied: it drops
  non-numeric values (SPARQL sorts them first / last), emits ties in its
  internal order, and misranks matches when a scored variable has children.
- **New API:** `TopKOptions { early_termination, bounded_heap, dpb:
  DpbMode::{Off, Auto, Always} }` (`ALL`, the default, and `NONE`),
  `Evaluator::with_topk_options`, `Evaluator::stats` / `reset_stats`
  (`EvalStats` work counters), and `Database::set_topk_options` /
  `topk_options` (clears the query cache; carried into `ConcurrentDb` and
  HTTP transaction snapshots). All are re-exported from `gstore::query`.
- **Benchmark:** `cargo run --release --example topk_bench` compares full,
  heap-only, default and forced-DP-B evaluation and checks the rows agree.
  Measured at opt-level 1 on 100k–200k solutions: `LIMIT 10` 65 ms → 1 ms,
  `ASK` 115 ms → 0.8 ms, `ORDER BY … LIMIT 10` on a fan-out-20 star 469 ms
  → 3.3 ms.
- Cancellation is polled per BGP step, per filtered solution and per DP-B
  match; a cancelled query still reports `Cancelled` on every path.

#### Upstream parser test suite

- **New: `tests/dt_upstream_parser_suite.rs`** runs upstream gStore's
  `scripts/test/parser_test.sh` (50 SPARQL cases, vendored byte-for-byte
  with upstream's BSD-3 licence under `tests/data/upstream/parser_test/`)
  in-process: each case imports its Turtle file and checks the triple
  count, the answer count and the rows against upstream's result file.
  **50/50 pass**, each with Top-K / early termination both on and off (the
  two runs must give the same answer). 48 match upstream exactly; cases 15
  and 17 follow W3C and upstream's own result files where the script's
  expected count does not (listed with the reason in one `DIVERGENCES`
  table). No case is deferred to Phase 5. Run it with the per-case table:
  `cargo test --test dt_upstream_parser_suite -- --nocapture`.

#### Known gaps

- A labelled blank node `_:b` in a WHERE pattern still matches only that
  stored node (upstream's reading), and in templates / `INSERT DATA` it
  names the same node for every solution and request; only `[ … ]` and
  `( … )` have SPARQL's fresh-node semantics.
- CONSTRUCT, ASK and DESCRIBE do not accept `GROUP BY` / `HAVING`; DESCRIBE
  ignores `ORDER BY` and `LIMIT`. `FROM` / `FROM NAMED` are skipped; `BASE`
  resolution is concatenation, not RFC 3986.
- `SELECT (expr AS ?v)` / `BIND(… AS ?v)` with `?v` already in scope is not
  rejected at parse time.
- `COUNT(DISTINCT …)` compares values, so `"01"^^xsd:integer` and
  `"1"^^xsd:integer` count as one there; `SUM` /
  `AVG` skip non-numeric values instead of failing.
- The proposed `conf.ini` keys (`integer_division`, `query_cache_size`)
  are not read yet: there is no `conf.ini` loader in this crate.
- String functions do not apply SPARQL's argument-compatibility rules
  (`STRBEFORE("abc"@en, "b"@cy)` should be an error; `CONTAINS`,
  `STRSTARTS`, `STRENDS` do not check either), and `UCASE` / `LCASE` on an
  IRI or a number return a string instead of an error.
- DP-B handles tree-shaped BGPs and integer keys only (upstream also cuts
  one cycle); other `ORDER BY … LIMIT` queries use the bounded heap. The
  join-order cost model does not yet know that a mixed predicate variable's
  hidden slot and the variable are linked.
- Property-path solutions come out of hash sets, so
  `{ ?a :p+ ?b } LIMIT n` without a total `ORDER BY` can return different
  rows on different runs (with or without the Top-K optimizations).

### Breaking: SPARQL graph functions follow gStore 1.4

The advanced graph functions of upstream gStore 1.4 (`docs/en-us/SPARQL_QUERY.md`,
"Advanced functions") now parse and evaluate with upstream's signatures,
semantics and output. Their names used to call gstore-rust's own graph
built-ins, which had different arguments and results. Those built-ins still
exist with unchanged behaviour, but you now call them through the `rs:`
namespace.

- **New: 32 upstream functions** — `simpleCyclePath`, `simpleCycleBoolean`,
  `cyclePath`, `cycleBoolean`, `shortestPath`, `shortestPathLen`,
  `shortestPathCount`, `shortestPaths`, `kHopReachable`, `kHopReachablePath`,
  `kHopEnumerate`, `kHopCount`, `kHopNeighbor`, `kHopAllNeighbors`, `bfsCount`,
  `SSSP`, `SSSPLen`, `PR`, `PPR`, `closenessCentrality`,
  `betweennessCentrality`, `triangleCounting`, `diameterEstimation`, `WCC`,
  `labelProp`, `louvain`, `maximumKplex`, `coreTruss`, `clusterCoeff`,
  `JaccardSimilarity`, `degreeCorrelation` and `confidenceDegree`.
  - **Where they go.** Write each one as a SELECT projection,
    `SELECT (shortestPath(?x, <b>, true, {<p>, !<q>}) AS ?v) WHERE { … }`.
  - **Arguments.** They take upstream's predicate sets (`{}`, `{<p>}`,
    `{!<p>}`), named parameters (`alpha=`, `maxIter=`, `tol=`, `increase=`)
    and a `directed` flag.
  - **Results.** A function returns upstream's JSON text (`{"paths":[…]}`) as a
    plain literal; `cycleBoolean` returns an `xsd:boolean`.
  - **Variable arguments.** A variable stands for all of its distinct values in
    the WHERE solutions. With two variables, the function runs over their
    cross product.
  - **Rows.** The function runs once, so a lone function returns one row.
    Next to other projections, only the last row holds the value. With
    `GROUP BY`, it runs once per group.
- **New: a per-predicate CSR for each database.** It leaves out literals,
  just as upstream's does. The database builds it on the first
  graph-function query after a data change, and later queries share it.
  `Database::preload_csr()` builds it in advance, like upstream's `loadcsr`.
- **Changed: the legacy functions moved to `rs:`.** gstore-rust's earlier
  graph functions now live in the IRI namespace `<urn:gstore-rust:fn:>`,
  which is predeclared as the prefix `rs:`. A query's own `PREFIX rs:`
  replaces it. The functions can still be used anywhere (FILTER, BIND,
  SELECT), for example `rs:SSSPLEN(?a, ?b)`, `rs:WCC(?a, ?b)` or
  `rs:PPR(?s, ?t)`. A bare call in the old form, such as `SSSP(?a, ?b)` or
  `SHORTESTPATHLEN(?a, ?b)`, is now a parse error that names the `rs:`
  replacement. `CLUSTERINGCOEFF` and `KHOPENUMERATEPATH` have no upstream
  namesake, so they still work without the prefix. The README has a
  migration table.
- **Changed: `SIMILARITY(a, b)` is now upstream's Jaro string similarity.**
  It compares the arguments' term strings. The old node/string similarity is
  `rs:SIMILARITY(a, b)`.
- **Changed: graph functions inside expressions are an error.** This covers
  the four functions upstream accepts inside FILTER (`cycleBoolean`,
  `simpleCycleBoolean`, `shortestPathLen`, `kHopReachable`). Upstream parses
  them there but never evaluates them, so here they fail with a message that
  points to the `rs:` function instead.
- **New: `PFN.name({…})` calls parse.** Evaluating one fails with "PFN not
  supported yet" for now.
- **New: XSD cast functions.** SPARQL 1.1's `xsd:string`, `xsd:integer`,
  `xsd:decimal`, `xsd:double`, `xsd:float`, `xsd:boolean` and
  `xsd:dateTime`, plus `xsd:date`. A cast that SPARQL does not allow
  leaves the value unbound.
- **Changed: other IRI function calls must name a registered function.** The
  call reaches a function registered with `Database::register_function`
  under its full IRI or its local name; anything else is a query error
  (it used to evaluate to unbound). Registered functions now also work in
  the WHERE clause of `DELETE`/`INSERT … WHERE`.
- **Changed: read snapshots answer like the database.** A transaction's read
  snapshot (HTTP `/begin` … `/tquery`) and the `ConcurrentDb` snapshot,
  transaction and version-view reads now see the functions registered with
  `Database::register_function` (they used to evaluate such calls to
  unbound, which the previous item would have turned into an error). Their
  graph-function queries share one CSR per snapshot, reusing the database's
  cached CSR when it has one, instead of rebuilding it on every query.
- **Fixed: infinite `xsd:double` values print as XSD lexicals.** They are
  written `INF` and `-INF` (they used to print as Rust's `inf` and `-inf`);
  NaN stays `NaN`.
