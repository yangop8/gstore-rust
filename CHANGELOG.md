# Changelog

All notable changes to gstore-rust are recorded here.

## Unreleased

### SPARQL semantics follow SPARQL 1.1 more closely

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
  an error).
- **Changed: numbers.** `xsd:float` keeps its datatype through expressions
  (`float + integer` is a float). Dividing two integers gives an
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
  use. Proposed `conf.ini` key: `query_cache_size` (bytes).

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
