# Changelog

All notable changes to gstore-rust are recorded here.

## Unreleased

The work toward upstream gStore 1.4 parity (`docs/GAP_PLAN.md`) lands in
phases. Phases 0 and 1 (durability and hardening; the upstream `/api`
server) and Phases 2 and 3 (graph functions; SPARQL 1.1 and Top-K) were
developed in parallel from the same base and merged; Phase 5 (RDF import,
gconsole, zip, logging and packaging) was developed from the Phase 1 head
and merged after them. The integration sections record what joining them
changed; the following ones each phase, newest first.

### Integrating Phase 5 with Phases 0–3

- **SPARQL `LOAD` reads through the import pipeline, atomically.** The
  format follows the extension (N-Triples, Turtle, N-Quads, TriG, RDF/XML,
  JSON-LD, RDFa), the document streams in, every `LOAD` gets its own
  blank-node scope, and the `LoadPolicy` still confines local files and
  refuses remote ones. **Fixed:** Phase 5's streaming `LOAD` kept the
  batches it had inserted before a parse error, so a `LOAD SILENT` of a
  broken file reported success over a partial load; a failed `LOAD` now
  undoes what it inserted, and `SILENT` only hides the error. A `LOAD` is
  cancelled (`/cancel`, `operationTaskCancel`, shutdown) between import
  batches and then applies nothing either — `SILENT` does not hide a
  cancel.
- **Changed: `LOAD <src> INTO GRAPH <g>` of a dataset.** Without `INTO`, a
  TriG / N-Quads document keeps its named graphs (Phase 5). With `INTO`,
  its default graph goes into `<g>` and no other graph changes (SPARQL 1.1
  Update §3.1.4 inserts "its triples into the specified graph"; Jena does
  the same): the statements of the document's own named graphs are not
  loaded, and the diagnostic log says how many were skipped.
- **Caches stay coherent.** Every write path Phase 5 added or rewrote —
  `batchInsert` / `batchRemove` from a file, a directory, a URL, an upload
  or a zip package, `LOAD`, `build` from several files, an upload, a URL or
  a zip package, `restore` of a zipped backup — invalidates the query cache
  and the CSR (and the lock-free `csr` status `show` reports) like the
  other writes (`tests/dt_cross_phase5_api.rs`; no fix was needed).
- **Configuration.** `conf.ini` holds Phase 3's `[query]` section next to
  Phase 5's `[log]` keys and `[upload]` zip limits. Databases built,
  uploaded, downloaded or restored by Phase 5's paths take the `[query]`
  settings, and `refreshconf` updates them. **New:** `refreshconf` also
  re-reads the diagnostic-log configuration (`log_mode`'s properties file,
  `log_level`, `log_file`, `log_days`, `log_stderr`, `GSTORE_LOG`) and
  re-installs the log when it changed, as upstream's `refreshconf` re-runs
  log4cplus' `doConfigure` (`Util::configure`). **API:**
  `ApiServer::with_log_refresh` (gserver uses it; an embedded server leaves
  the process-wide log alone).
- **Changed: raw line breaks in Turtle short strings are accepted again.**
  Phase 5 made a `"…"` / `'…'` string stop at a raw line break (W3C
  Turtle), but upstream's loader reads one into the value
  (`TurtleParser::Lexer::lexString`), so files it loads must load here.
  Evidence: upstream's exporters never write one (`NodeUtil::node2string`
  escapes `\n`, `\r`, `\t`; neither do ours), and none of the vendored
  datasets (`testdata/upstream/**`, upstream's `data/**`) holds one.
  Recovery stays per line: a statement whose short string ran across a line
  break and then fails (a missing closing quote, usually) is reported on the
  literal's line and read again from that line break, so the lines after it
  are not swallowed (upstream loses them); a string reaching the end of the
  input, or running more than 256 KiB past its first line break, fails its
  statement the same way. Memory stays bounded by that 256 KiB.
- **gconsole.** Every SPARQL form the server accepts is sent as SPARQL —
  `ADD` / `MOVE` / `COPY`, `WITH … DELETE/INSERT`, `LOAD … INTO GRAPH`,
  `CREATE` / `DROP` / `CLEAR GRAPH`, `CONSTRUCT WHERE`, after a `PREFIX` /
  `BASE` prologue too — while `drop <db>`, `create <db> <file>` and
  `clear` stay upstream's commands; `sparql <file>` sends a run of the new
  update forms as one request. **Changed:** a literal in a result table is
  printed as upstream's console prints it (its `format=n-triple` cell): the
  lexical form unescaped, so a graph function's JSON reads
  `"{"paths":[…]}"`; line breaks and tabs stay escaped to keep the table's
  rows. A lone answer cell is shown as indented JSON only when it is a plain
  literal (upstream tests the cell `"{…}"`). Upstream's console has no
  `loadcsr` command and no `csr` column, so neither does this one.
- **Logs.** Graph-function queries and the Phase 3 update forms are in the
  JSON query log like any statement (REST and `/api`). Phases 2 and 3 added
  no `eprintln!` diagnostics to server paths.
- **CI.** One feature matrix — `gzip`, `tls`, `zip`, `readline` — builds,
  clippies and tests each feature; the `rocksdb` job clippies too; the
  conformance job runs `scripts/conformance.sh`. `docs/DEPLOY.md` documents
  `[query]` and `loadcsr`.
- **Tests.** `tests/dt_load_import.rs` (`LOAD` through the pipeline:
  datasets with and without `INTO`, formats, blank-node scopes, atomic
  failure, cancel, policy; `ADD` / `MOVE` / `COPY` / `WITH` / `USING` /
  `GRAPH` / `DROP` on imported named graphs with blank nodes on the memory,
  disk and RocksDB backends, across a reopen), `tests/dt_cross_phase5_api.rs`,
  `tests/dt_settings_phase5.rs`, and new cases in `dt_gconsole`,
  `dt_logging`, `dt_rdf_import` and the Turtle reader's unit tests.

### Integrating Phases 2 and 3 with the Phase 1 server

- **Security limits cover the new grammar.** The SPARQL parser's nesting
  limit (128) and tree-height limit (1024) now also bound every construct
  Phases 2 and 3 added: `[ … ]` / `( … )` triples nodes (their own cap of
  256 is gone — they count towards the global nesting limit, in patterns,
  templates, `CONSTRUCT WHERE` and `INSERT DATA`), `IN` / `NOT IN` lists,
  graph-function and `rs:` calls, sub-selects with a trailing `VALUES`,
  `GROUP BY (… AS ?v)` (each binding is one more node) and the `GRAPH`
  blocks of `DELETE WHERE`. `WITH <g> WITH <g> …` is refused up front
  instead of recursing once per `WITH`.
- **`SERVICE` stays disabled on every evaluation path.** The Top-K / early
  termination / DP-B paths, graph-function queries, named-graph updates
  (`WITH`, `USING`, `GRAPH` templates) and — **fixed** — reads answered
  from an open HTTP transaction's snapshot (which used to contact remote
  endpoints on a restricted server) and `ConcurrentDb` snapshots all follow
  the load policy (`gserver --allow-service` to opt in). `LOAD … INTO
  GRAPH` follows the `LoadPolicy` like `LOAD`.
- **`loadcsr` and `load` with `csr=1` build the CSR** the graph functions
  run on (`Database::preload_csr`); `loadcsr` answers upstream's "CSR
  already loaded." when it is current, and `show` reports `csr` truthfully.
- **New: `conf.ini` `[query]` section** — `integer_division`,
  `query_cache_size`, `early_termination`, `topk_bounded_heap`, `topk_dpb`
  and `gfun_max_output_bytes` — applied to every database the servers
  load, build, create or restore (and re-applied by `refreshconf`).
  **API:** `QuerySettings`, `Database::apply_query_settings` /
  `query_settings` / `set_gfun_max_output_bytes`,
  `Evaluator::with_gfun_max_output_bytes`, `GStoreConfig::query_settings`,
  `ApiServer::with_query_settings`, `Server::with_query_settings`.
- **Changed: every graph function's output is bounded**, not only the path
  family's: the metric and community functions (`PR`, `WCC`, `louvain`, …)
  fail with a "… bytes of output" error past the bound (64 MiB on the
  servers, 256 MiB for an embedded `Database`). Graph-function queries are
  cancelled by `operationTaskCancel`, REST `/cancel` and a graceful
  shutdown like any query.
- **Tests.** The `/api` run of upstream's `parser_test.sh`
  (`tests/upstream_parser.rs`) now runs all 50 cases (33–35, 42 and 50
  were waiting for these phases); it and the in-process suite share one
  vendored copy (`testdata/upstream/scripts/test/parser_test/`) and one
  table of the script's numbers and documented divergences (#15, #17,
  `tests/upstream_scripts/parser_cases.rs`). `tests/dt_cross_phase_api.rs`
  checks that every Phase 1 write path (`batchInsert`, `batchRemove`,
  updates, transaction commit and rollback, `restore`, an overwriting
  `/build`, drop and rebuild) leaves no stale query-cache or CSR answer, and
  that the Phase 3 update forms (`ADD` / `MOVE` / `COPY`, `WITH`, `LOAD …
  INTO`, `INSERT DATA` with `[ … ]`) need the update privilege while
  graph-function SELECTs need only query.
- **Fixed (integration review).**
  - The WHERE clause of a `DELETE`/`INSERT … WHERE` now runs under the
    request's cancel flag. A graph function in a sub-SELECT there used to
    ignore `/cancel`, `operationTaskCancel` and shutdown, which kept the
    database locked and hung a graceful shutdown. A cancelled update
    applies nothing.
  - A cancel that arrives after the WHERE clause (while projecting,
    ordering or aggregating, e.g. in a projected `EXISTS`) now fails the
    query. It used to answer with wrong values, which the query cache then
    kept. A refused `SERVICE` there is also reported, instead of reading as
    `false`.
  - `PPR`, `JaccardSimilarity` and the other metric functions stop as soon
    as their output passes `gfun_max_output_bytes`. They used to build the
    whole (quadratic) output first.
  - The single-database `gserver` cancels the statements still running
    when the shutdown grace period ends, so the final save of unsaved
    changes no longer waits for them.
  - `/api` `show` reads `csr` without the database's lock, and
    `refreshconf` no longer waits for busy databases: it swaps the
    configuration first, and a busy database takes the new `[query]`
    settings the next time it is used (the most recent ones win). A
    database created by `/upload` gets the settings in effect when it is
    registered.
  - `load` reports `csr` as whether the CSR is built (`"1"`/`"0"`), as
    upstream does. It used to echo the request.
  - **Changed:** `format=n-triple` prints literals as upstream does, with
    the lexical form unescaped between quotes, so a graph function's value
    is its JSON once the quotes are stripped. `n-triple-file` keeps
    upstream's escaped form.

### Phase 5 — RDF I/O, gconsole and operations (GAP_PLAN §4.2, §5)

#### RDF import

- **Streaming readers.** Turtle, TriG, N-Triples and N-Quads are read one
  statement at a time from a `BufRead` (`src/parser/stream.rs`). Memory stays
  bounded even for one huge TriG block. RDF/XML, JSON-LD and RDFa are still
  parsed whole, up to 256 MiB per document.
- **One import pipeline** (`src/parser/import.rs`) for every bulk load:
  `Database::build_from_*` / `insert_sources` / `remove_sources`, SPARQL
  `LOAD`, `gbuild` (memory, `--disk`, RocksDB), `gadd` / `gsub`, the `/api`
  `build` / `batchInsert` / `batchRemove`, and `gconsole --local`'s `import`.
- **Tolerant import, as upstream does it.** A bad statement is skipped and
  counted, and the reader resumes on the next line. Failures include
  upstream's numeric literal checks and invalid subjects, predicates or
  graphs. Each one is written to `<db>.db/parse_error.log` in upstream's
  format, with the line it was read from. `success_num` / `failed_num` follow
  upstream. Strict mode stays the library default; `gbuild` / `gadd` / `gsub`
  gain `--strict`.
- **Named graphs are kept** by N-Quads and TriG in the in-memory, disk and
  RocksDB backends. A blank-node graph is stored as
  `urn:gstore:bnode:<label>`. *Behaviour change:* quads are no longer
  flattened into the default graph.
- **Blank nodes are scoped per loaded document**, so two batch inserts never
  merge their `_:b`s. Generated labels round-trip through N-Triples. The
  plain `parse_*` APIs keep labels and mint `genid-<n>`.
- **Sources.** Directories (recursive, sorted, policy-checked) and URLs,
  fetched only when the load policy allows remote sources. `/api` accepts
  `remote=true` and answers 1005 when a download fails. `.gz` files need the
  `gzip` feature.
- **Turtle leniency.** Upstream's local-name characters are accepted (loader
  only). An IRI ends at a space or control character, as in upstream's lexer,
  so a missing `>` costs only its own line; so does a short string missing
  its closing quote (since the integration a short string may hold raw line
  breaks, as upstream's lexer allows, and a failed statement is replayed
  from the first one — see "Integrating Phase 5"). After a bad statement in a
  TriG file, a `{` or `}` on the rest of its line still opens or closes the
  block, so no statement is filed under the wrong graph.
- **RDF/XML.** Internal DTD entities, with expansion budgets (every expanded
  entity value counts); a value may refer to an entity declared after it;
  external entities are refused. `rdf:parseType="Collection"` is supported.
- **Exports load back.** `gexport` / `/api export` write IRIs holding a
  space or another character IRIREF excludes as `\uXXXX` escapes.

#### gconsole

- `gconsole` is now a client of gserver's upstream `/api` (`src/console/`).
  It implements all 43 commands of upstream 1.4's command table, plus `exit`
  and `history`.
- Output matches upstream's: PrettyPrint tables, the `Query ans num` /
  `Build RDF database` / `After inserted triples num …,failed num …` lines,
  and `RDF parse error num N` when the server's tolerant import skipped
  statements.
- Login with `-u` / `-p`, `$GSTORE_PASSWORD`, or a prompt with echo off.
  Other options: `--host`, `--port` (falls back to `conf.ini`, then 9000),
  `-c`, `-e`, `-i`.
- Interactive features: multi-line statements, `#` comments, `>` / `>>`
  redirection, and a persistent history (created mode `0600`). Ctrl-C
  cancels the server's query task.
- `drop <name>` always drops a database and `clear …` clears the screen, as
  upstream; SPARQL's `DROP` / `CLEAR` / `CREATE` are recognised only in
  forms the commands cannot have (`DROP GRAPH <g>`, `DROP SILENT ALL`, …).
  `sparql <file>` sends a run of update operations (`op ; op`) as one
  request. `use system;` is refused for root too (it cannot be queried).
- The optional `readline` feature adds line editing and tab completion.
- `gconsole --local <db>` keeps the embedded REPL; the legacy `gconsole <db>`
  still works. Its `import` command now accepts any RDF format, a directory
  or a URL.

#### Zip packages and export (feature `zip`)

- New optional cargo feature `zip` (zip 9, deflate only, pure Rust); the
  default build gains no dependency. `src/zipio.rs`.
- `/api` `build` / `batchInsert` / `batchRemove` read a `.zip` of RDF files
  (local, uploaded or `remote=true`), unpacked as upstream does into the
  upload directory and removed afterwards; every entry needs an allowed RDF
  extension. Errors carry upstream's zip codes (1–6).
- Safety: a package is checked in full before anything is written. Absolute
  or `..` entry names are refused (zip-slip), symlink entries ignored,
  encrypted entries refused. New `[upload]` keys `unzip_max_entries`
  (default 10000) and `unzip_max_size` (default 10 GiB, also enforced on the
  bytes actually written).
- `backup compress=true` writes `<db>.db_<timestamp>.zip` (upstream's
  layout); `restore` takes it (1005 when it does not unpack); rotation and
  the "newest backup" choice include zips.
- `export` gains `format` (`nt` / `nq`); `compress=true` writes a `.zip`.
  *Behaviour change:* without `format`, a database with named graphs is
  exported as N-Quads (`.nq`) instead of N-Triples that dropped them.
- CLI: `gexport --format nt|nq` and `-z` / `out.zip`; `gbackup -z` /
  `x.zip`; `grestore` of a `.zip`; `gbuild`, `gadd` and `gsub` of a `.zip`
  package (upstream 1.3's `-f x.zip`). `--unzip-max-size` /
  `--unzip-max-entries` change the CLI's limits (`0` = none); `grestore`
  has none by default, so any `gbackup -z` archive restores. `gconsole`'s
  `restore` passes a zip path to the server like a directory.
- Without the feature every zip path answers `1003` (the CLI exits 1),
  naming the feature.

#### Diagnostic, query and access logs

- `src/logging.rs`, no new dependency: levels `error`…`trace`, standard
  error and a file rolled at local midnight into `FILE.YYYY-MM-DD`, with
  retention in days. Configured as upstream through `conf.ini`'s `log_mode`
  (a log4cplus properties file; upstream's two are shipped as
  `conf/slog*.properties.example`), plus the keys `log_level`, `log_file`,
  `log_days`, `log_stderr` and the `GSTORE_LOG` variable. As log4cplus,
  `MaxBackupIndex` deletes no daily file (`MaxHistory` / `log_days` set a
  retention), and the file only ever rolls forward.
- `gserver`'s diagnostics go through it (handler panics, accept failures,
  failed saves; each request at `debug`); its startup lines on standard
  output are also recorded in the file.
- The REST `/sparql`, `/update` and `/tquery` now write upstream's JSON
  query-log record (client IP, answer count, latency, status, format,
  database), so `/api querylog` lists them. Each line of
  `<db_home>/access.log` gains the client IP as a sixth field.

#### Legacy command-line tools

- All ten tools take upstream 1.3's options (`-db`/`--database`,
  `-f`/`--file`, `-p`/`--path`, `-q`, `-l`, `-z`, …) besides their own,
  answer `-h`/`--help`, and exit `0` (done), `1` (failed) or `2` (invalid
  arguments).
- *Behaviour change:* `grestore` takes the database first, then the backup,
  as upstream; the old order fails with a hint and restores nothing.
  `grestore -l NAME` lists its backups.
- `gshow` with no argument lists every database (upstream's
  database/creator/built_time table); `gshow NAME` still shows its counts.
- `gbuild` without a source builds an empty database, and refuses to
  overwrite an existing one unless `--force`. A failed build removes the
  database directory it created, as upstream.
- `gexport -db NAME [-f DIR]` writes `<db>_<timestamp>.nt` (`.nq` / `.zip`
  as chosen); `gbackup -db NAME [-p DIR]` creates
  `DIR/<db>.db_<timestamp>` (default `./backups/`).

#### Packaging

- `Dockerfile` (builder on the MSRV toolchain, `debian:bookworm-slim`
  runtime, user `gstore`, port 9000, volumes `/var/lib/gstore` and
  `/etc/gstore`, a health check on `/api?operation=check`, `--build-arg
  FEATURES`), and `.dockerignore`.
- `packaging/systemd/gserver.service` (graceful stop by `SIGTERM`,
  `Restart=on-failure`, hardening), sysusers.d / tmpfiles.d entries, and
  cargo-deb / cargo-generate-rpm metadata with maintainer scripts.
- `docs/DEPLOY.md` covers the container, the service, the packages, the
  configuration and logs, and the tools.
- `gserver` refuses to listen beyond loopback while `root` still has the
  default password `123456`; `--password-file FILE` /
  `$GSTORE_ROOT_PASSWORD_FILE` set root's initial password without putting it
  on the command line (the container's way).

#### CI

- The feature matrix builds, clippies and tests `gzip`, `tls`, `zip` and
  `readline`.

#### Not yet

- The Docker image and the deb / rpm packages were built and linted but not
  yet installed and run on a live host in CI.
- `gconsole`'s `backup` / `export` have no compress or format switch (as
  upstream's console); use the `/api` or `gbackup` / `gexport`.
- The legacy REST payload endpoints (`/upload`, the `/batchInsert` body)
  still parse whole and drop graph labels.
- JSON-LD `@graph` with an `@id` is flattened into the default graph.

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
  `AVG`). `conf.ini` key (since the Phase 1 merge): `[query]
  integer_division = decimal | truncate`.
- **Changed: the query cache is bounded by memory.** It holds about 64 MiB
  of results by default (`Database::set_query_cache_size(bytes)`, 0 turns it
  off; `set_query_cache_capacity` is gone) and never caches queries using
  `NOW()`, `RAND()`, `UUID()`, `STRUUID()`, `BNODE()`, `SERVICE` or a user
  function. `Database::query_cache_len()` / `query_cache_bytes()` report its
  use. `conf.ini` key (since the Phase 1 merge): `[query]
  query_cache_size` (bytes). A CONSTRUCT
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
  with upstream's BSD-3 licence under
  `testdata/upstream/scripts/test/parser_test/`, the copy the `/api` suite
  `tests/upstream_parser.rs` uses too) in-process: each case imports its Turtle file and checks the triple
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

### Phase 2 (breaking): SPARQL graph functions follow gStore 1.4

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

### Phase 1: upstream gStore 1.4's `/api` protocol and server

The multi-database server speaks upstream gStore's wire protocol, so the
upstream SDKs and test scripts run against it unchanged
(`scripts/conformance.sh`).

- **New: the `/api` protocol** (`src/gapi`): `GET|POST /api`, `POST /`,
  `/grpc/api` and `GET /?operation=` dispatched by operation name with
  per-request credentials (`encryption=1` MD5 too) and upstream's HTTP-200
  `StatusCode` envelope, upstream's response fields, codes and messages
  (upstream's *code* wins where its docs differ). Operations: `check`,
  `login`, `testConnect`, `getCoreVersion`, `stat`, `init`, `refreshconf`;
  `query` (JSON / file formats, `max_output_size`), `begin`, `tquery`,
  `commit`, `rollback`, `checkpoint`; `show`, `load`, `loadcsr`, `unload`,
  `monitor`, `build`, `drop`, `rename`, `backup`, `backuppath`, `restore`,
  `export`, `batchInsert`, `batchRemove`, `schema`; `showuser`,
  `usermanage`, `userprivilegemanage`, `userpassword`, `ipmanage`,
  `querylog`, `accesslog`, `txnlog`; `checkOperationState`,
  `operationTaskCancel`, `operationTaskList`; `/file/upload` and
  `/file/download`. The SDKs' encoded `?operation=…` targets are decoded as
  upstream does.
- **New: `conf.ini`** (`src/config.rs`, `conf/conf.ini.example`): every key
  of upstream's file with upstream's defaults; unknown keys and bad values
  warn. Precedence: command-line flag > `conf.ini` > default.
- **New: `gserver` lifecycle** as upstream's: `-s` / `-b` (background) /
  `-t` / `-r` / `-k` / `-S`, `-c conf.ini`, `-p`, `--db-home`,
  `--thread-num`; a pid file with a per-run `system` password; the
  `deamon=on` supervisor (gives up on a crash loop); `/shutdown` and
  `/restart` routes. The default mode is the multi-database API rooted at
  `db_home`.
- **New: HTTP layer** — a bounded worker pool (`thread_num`, a queue with
  back-pressure), HTTP/1.1 keep-alive, CORS / `OPTIONS`, a request body cap
  (`upload_max_body_size`, 413), chunked bodies, `Expect: 100-continue`,
  and a deadline on the whole request head.
- **New: system database** (`src/sysdb.rs`): per-database privileges
  (`query@lubm`, …) with upstream's `check_privilege` table, a database
  registry, IP allow / deny lists and the failed-login lockout (REST
  included), and upstream's query / access / transaction logs.
- **New: asynchronous tasks** (`src/gapi/tasks.rs`): `async=true`
  operations get an `opt_id`, run on bounded runner threads, keep their
  results (bounded, with a TTL) and POST them to a `callback` URL (HTTPS
  with the `tls` feature; host-local addresses refused). Running SPARQL
  statements and operations share one registry with REST `/tasklist` and
  `/cancel`; a shutdown cancels them and waits, bounded.
- **New: database lifecycle and confinement** — every server-side path is
  canonicalized and confined to its configured root; an inter-process
  `LOCK` on each database directory (a second process gets a clear error;
  `drop`, `rename`, `restore` and a rebuilding `/build` take it first);
  `max_database_num` enforced by every path that creates a database;
  backups record their database (`BACKUP_OF`) and a non-admin restore takes
  only its own database's backups.
- **Changed: transaction ids are numbered per loaded database** (the first
  after a load is `1`, as upstream's per-database `Txn_manager`); REST
  requests may name `db_name` to disambiguate.
- **Security** (`8d48fb5`): password checks run off the server's state
  lock (bad credentials cannot stall other clients); SPARQL nesting is
  bounded (128 levels, a 1024-node tree height) and HTTP workers get an
  explicit stack, so a deeply nested request is a 400, not a process-wide
  stack overflow; RDF/XML nesting is limited; `/build?db_path=` follows the
  `LoadPolicy`; `/backup` writes only inside the backup directory and
  `/restore` reads only from there or allowed directories; federated
  `SERVICE` is refused on servers unless `--allow-service` (the client has
  a 30 s deadline and a 64 MiB response cap); changing a password revokes
  the user's sessions.
- **Conformance suite**: upstream's test data, `parser_test` and
  `bfs_test` vendored under `testdata/upstream/`; Rust ports of
  `basic_test.sh`, `parser_test.sh`, `bfs_test`, `update_test.sh`,
  `transaction_test.sh` and `api_test.sh` (`tests/upstream_*.rs`); and
  `scripts/conformance.sh`, which runs upstream's own scripts and Python
  SDK against `gserver` (a CI job).
- Known differences from upstream (README): a database is single-writer
  (one open transaction; other writes answer `1004` meanwhile), and zip
  packages are not supported yet.

### Phase 0: durability, data safety and hardening

- **Persistence.** On-disk (`kvstore.kv`) databases write every committed
  mutation through to the store (WAL commit points; named graphs in a quad
  tree); RocksDB databases keep their dictionary, named graphs and name in
  column families, sync at commit points and make transactions
  crash-atomic (an undo journal in the same write batch); snapshot saves
  are crash-atomic as a set. `Database::open` detects the layout; every
  CLI tool and server opens databases with it.
- **Restore** stages and validates a backup before swapping it in as a
  whole directory, so a corrupt backup no longer destroys the target.
- **HTTP servers** are durable: the `check_point` policy saves every write
  before it is acknowledged (`gserver --no-checkpoint` defers saving), a
  graceful shutdown (SIGINT / SIGTERM, `ShutdownHandle`) waits for
  in-flight requests and saves, partial-update failures are saved and
  reported, no handler waits for a database lock while holding the state
  lock, each request is isolated with `catch_unwind`, and transactions
  give non-owners a begin-time snapshot (no dirty reads).
  `/batchInsert` / `/batchRemove` apply parsed triples, never SPARQL text
  built from input.
- **`LoadPolicy`** for SPARQL `LOAD`: permissive for the library, but
  `LoadPolicy::restricted()` on servers (`gserver --load-dir DIR` re-allows
  a directory); sources are canonicalized and confined, and denials do not
  reveal whether a file exists.
- **Hardening.** Bounded dictionary id spaces (entities, literals,
  predicates) with checked `try_insert_*` paths instead of silent
  misclassification; built-in function arity checked at parse time and the
  crafted-query panics removed (`STR()`, `ORDER BY RAND()`, overflows);
  stable, versioned PBKDF2-HMAC-SHA256 password hashes (legacy hashes
  migrate on login) and atomic `users.db` writes.
- **Cluster** (experimental): single-write RPC framing, client writes
  acknowledged only once committed, per-peer replication senders, fallible
  shard reads; a CI workflow.
