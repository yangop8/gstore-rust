# Changelog

All notable changes to gstore-rust are recorded here.

## Unreleased

The work toward upstream gStore 1.4 parity (`docs/GAP_PLAN.md`) lands in
phases. Phases 0 and 1 (durability and hardening; the upstream `/api`
server) and Phases 2 and 3 (graph functions; SPARQL 1.1 and Top-K) were
developed in parallel from the same base and merged; Phase 5 (RDF import,
gconsole, zip, logging and packaging) was developed from the Phase 1 head
and merged after them; Phase 4 (reasoning rules and PFN functions) was
developed from the Phases 0–3 head and merged last. The integration
sections record what joining them changed, newest first; the following
ones each phase, newest first (Phase 5, then Phase 4, …).

### Integrating Phase 4 with Phases 0–3 and 5

Phase 4 (reasoning rules and PFN functions) was developed from the
Phases 0–3 head while Phase 5 (import, gconsole, zip, logging, packaging)
landed; this records what joining them changed.

- **Merge.** Both dependency and feature sets (`readline`, `zip`;
  `pfn-wasm`); `Cargo.lock` is main's plus the optional `wasmi` / `wat`
  packages at the versions Phase 4 locked — its package set is exactly the
  union of both parents' locks. Phase 5's SPARQL `LOAD` runs inside Phase
  4's refactored `run_update` (with the request's PFN caller), its
  `insert_sources` / `remove_sources` use the same insert / delete paths
  as rule execution and retraction, and every served database — built,
  uploaded, restored from a zip — goes through `DbSlot::handle`, which sets
  its rule directory.
- **gconsole and Phase 4.** The console's `addreason` … `deletereason` and
  `showpfn` / `addpfn` / `mdfpfn` / `delpfn` / `buildpfn` commands, written
  against a server that did not implement them, were compared with
  upstream's `gconsole.cpp` handlers and now run whole lifecycles against
  `gserver`. **Fixed:** SPARQL typed at the prompt that projects
  `PFN.name({…})` printed the `{"results": …}` answer as `Update ans num
  0`; it prints the value and `pfn query use <ms> ms.`, as upstream prints
  its `pfn_type` cells. `addpfn` / `mdfpfn` accept a file holding the
  `PFNInfo` object as a JSON string too (upstream's `PFNInfo(json)`); the
  body is WebAssembly text, sent unchanged and URL-decoded by the server as
  upstream does (documented). `showpfn`'s third column names the
  WebAssembly source the server keeps (`src/<user>/<name lower-cased>.wat`)
  where upstream shows `./pfn/<name>.cpp`.
- **Backups.** A database's rules and ownership ledgers travel with it
  through every Phase 5 backup path — `gbackup` / `grestore` (directory and
  `-z` zip), `/api` `backup compress=true` / zip `restore`, `drop`'s `.bak`
  — and a restored executed rule still retracts exactly its triples.
  `export` writes data only, as upstream's. PFN functions belong to users:
  `docs/DEPLOY.md` gains a *Backups* section on backing up
  `pfn_base_path`, which the packaged and container layouts keep in
  `/var/lib/gstore/dbhome/pfn` (the data volume; the unit's
  `ReadWritePaths`).
- **Logging.** Reasoning and PFN events reach Phase 5's diagnostic log
  through the `gs_*` macros, as upstream's `SLOG_*` record them: rule
  execute / disable (info), compiled statements (debug), PFN compile
  (info), calls (debug) and failed calls (warn), stale PFN files that cannot
  be removed and unreadable `data.json` lines. Tested: `reasonManage` and
  the PFN operations are in the `/api` access log, the statement a rule
  executes and PFN queries (`/api` `query`, REST `/sparql`) in the query
  log.
- **Import × reasoning.** Rules over the blank nodes Phase 5's per-load
  scopes mint (explicit and anonymous) execute and retract exactly — every
  ledger line parses back to the stored triple — and `batchInsert` / `LOAD`
  after an execution leave the ledger as it was. Imported `<Rule:…>`
  triples (a tolerant import included) are asserted: no rule owns them.
- **Configuration.** A test reads `conf/conf.ini.example` like an operator:
  every key the code reads is documented in its section and every example
  value parses (the `[query]` / `[pfn]` examples are the defaults); it found
  `[system] licensetype` undocumented, now added. One `refreshconf`
  re-applies `[query]`, `[pfn]` and the `[log]` configuration together;
  `pfn_base_path`, like `db_home`, is read at startup.
- **CI and Docker.** One feature matrix builds (`--locked`), lints and
  tests `gzip`, `tls`, `zip`, `readline`, `pfn-wasm` and `zip pfn-wasm
  readline` together (the separate `pfn-wasm` clippy job is folded in);
  `rocksdb` and the conformance job are unchanged. The Docker image builds
  with `FEATURES="pfn-wasm"` by default (pure Rust, within its 1.89
  toolchain), so its server compiles and runs PFN functions; the default
  `cargo build` stays lean.
- **Docs.** README (features, cargo features, Reasoning, PFN, gconsole,
  configuration, test count), `docs/DEPLOY.md`, `docs/PFN.md`,
  `docs/GAP_PLAN.md` (status notes for Phases 0–5; Phase 6 pending) and
  `docs/CPP_PARITY.md` agree.
- **Tests.** `tests/dt_merge4_wiring.rs`; new cases in `dt_gconsole` (a
  rule's and a PFN function's lifecycle through the console, the PFN call
  part with `pfn-wasm`), `config` and `console::commands`. One expectation
  changed intentionally: the fake-server test's `showpfn` third column.
  The default workspace runs 1,960 tests (with the review fixes below);
  the features add their own.

#### Review fixes

A review of the merged tree found these; each has a regression test
(`tests/dt_merge4_fixes.rs`, and unit tests in `reason::manage`,
`pfn::registry`, `console::commands`, `logging`, `dt_cli_compat`).

- **Rule ledgers read back every term exactly.** Since Phase 5 the readers
  let IRIs holding `>`, `\`, a tab or a newline into a database (as `\u`
  escapes), and rules carried them into their ledgers raw: the strict
  N-Triples reader then refused such a line — disable skipped it, reported
  success and emptied the ledger, leaving the triple for good — or a line
  split by a newline named another triple, which disable deleted; an IRI
  holding a backslash-`u` sequence read back as the IRI it names, so
  disable deleted a user's triple and kept the inferred one. A ledger term
  is now written in an escaped N-Triples form that reads back exactly
  (IRIs and datatypes as the exporters escape them; blank node labels and
  language tags with `\uXXXX`), a line must be three terms and `.`, and a
  line that cannot be read fails the disable — nothing retracted, the
  ledger kept.
- **Execute records before it inserts.** The insert is durable at once on
  the on-disk and RocksDB layouts; the ledger was written after it, so a
  failed ledger write (or a crash) left inferred triples no rule owned.
  The ledger now records what the insert may add first, and is trimmed to
  what it added after.
- **Disable saves a snapshot database before emptying the ledger** (also
  with `check_point` off — upstream saves at this point too); a crash after
  the ledger was emptied left the inferred triples on disk with no owner.
  When the save fails the rule keeps its ledger and status.
- **Add writes a rule's (empty) ledger before its file, and delete removes
  the file before the ledger**, so a crash never leaves a rule file without
  a ledger — which is taken for an upstream rule and retracted upstream's
  way, deleting every `<Rule:label>` triple. Rule-directory copies (backups)
  copy the files before the ledger directory.
- **Incremental backups carry the rules and ledgers of their time**, and
  `restore_incremental` restores the last increment's (an increment taken
  before keeps the base's); the replayed inferred triples were paired with
  the base's rules.
- **Directory swaps wait for `reasonManage` file operations.** The REST
  `/drop`, `/restore`, a rebuilding `/build` and the `/api` `rename` now
  take the rule directory's lock, as the `/api` `drop` and `restore` did;
  an add, compile or delete could otherwise act on a directory being
  replaced (e.g. delete the restored copy's rule and ledger).
- **A rebuild in place drops the old database's rules** (REST `/build` over
  an existing database, `gbuild --force`), as a drop and a build do; they
  stayed executed, their ledgers naming triples the new data holds as
  asserted ones.
- **PFN.** `data.json` is the record: create no longer refuses a name whose
  source file exists without an entry (a failed `data.json` write or a
  crash blocked the name for good) and removes its source when the
  `data.json` write fails; update restores the source then; compile
  removes the old module only once `data.json` names the new one (a failed
  write left the function unloadable after a restart). A module loaded
  while its function was recompiled within the same second is no longer
  cached (it was served from then on). `funDesc`, `funBody` and `funSubs`
  are bounded as stored too (JSON-escaped: a control character takes 6
  bytes), so `data.json` stays within its documented bound, corrected to
  `pfn_max_functions` × (3 × `pfn_max_module_bytes` + 64 KiB).
- **Logs.** Every diagnostic record is one line: line breaks in a message
  (a PFN compile error naming an import, a failing function's output) are
  written as `\n` / `\r` and other control characters as `\uXXXX`, so a
  user can no longer forge records.
- **gconsole.** A PFN value that is an object with a top-level `bindings`
  key prints as the value (it printed `Update ans num 0`); `showreason`
  lists the patterns of `{"condition": {…}}`-wrapped conditions and prefers
  `conditions` to a stray `condition` field; a server's error message
  (which may quote a failing PFN function's output) prints with its
  control characters escaped, as answers do.
- **Docs.** `disablereason`'s printed statement and the query log's record
  of a disable are upstream's nominal `delete where`, not the `DELETE DATA`
  of the ledger that ran (README, `docs/DEPLOY.md`); PFN file names are
  lower-cased (README, `docs/DEPLOY.md`); `compilereason` prints
  `insert_sparql` and `delete_sparql` (the README listed `check_sparql`
  too); `gserver -r` starts the server with the options given to `-r`,
  not those of the server it stopped — repeat `--load-dir`, `-p`, … there
  (README, `gserver -h`).

#### Not yet

- The Docker image with `pfn-wasm` was not built in this environment (no
  Docker daemon); `cargo build --locked -p gstore --bins --features
  pfn-wasm`, what its builder runs, was.

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

#### Review of the Phase 5 integration

- **A failed `LOAD` is no change.** Once a failed or cancelled `LOAD` has
  undone its inserts, `Database::mutation_count` (and a CSR built for the
  data before it) is put back: the servers no longer answer "(operations
  before the failing one were applied)", save the database, or drop its
  CSR and caches for a `LOAD` that applied nothing. Nor is an empty named
  graph left behind: **changed**, a named graph whose last statement is
  deleted (by `DELETE`, an undo or a rollback) is gone at once — `GRAPH ?g
  {}` no longer finds it, as it never did after a reopen.
- **Fixed: `DROP` / `CLEAR GRAPH <>`** (an empty IRI, `<>` without a
  `BASE`) cleared every named graph, as if it were `NAMED`; it now names the
  graph `<>`. The redo log marks named-graph records (`REDO <op> <glen>
  <tlen> G`), so such a graph is no longer replayed into the default graph
  (logs without the flag read as before).
- **Fixed: terms a lenient reader lets in are no longer lost.** An IRI
  holding `>`, `\` or a space (written `\u003E`, …) survives `MOVE` / `COPY`
  / `ADD`, `DELETE` / `INSERT … WHERE`, `DESCRIBE` and redo replay
  (`Term::from_dict_key` reads a dictionary key back verbatim; redo records
  escape such IRIs as the exporters do). **Changed:** an empty language tag
  (`"x"@`) is a syntax error in Turtle / TriG / `.nt` files, as in N-Triples.
- **Fixed: a blank-node graph name co-refers with the same blank node.** In
  an N-Quads / TriG document, `_:g` as a subject or object is now the
  graph's `urn:gstore:bnode:` IRI too, so `GRAPH ?g { … } ?g <p> ?o` joins —
  unless `_:g` was used as a subject or object before it named a graph:
  then it stays a blank node throughout (a warning says so; nothing read is
  rewritten and no node is split in two).
- **Bounded import memory.** Each quad holds its own copy of every IRI a
  prefix, namespace or base expanded, so a small file with a long prefix
  could take gigabytes. An import batch is now handed over at
  `ImportOptions::batch_bytes` (**new**, default 64 MiB) of terms as well as
  at `batch_size` quads, and one Turtle / TriG statement or one RDF/XML /
  JSON-LD / RDFa document fails when it expands past 64 MiB or 64 times its
  size (`parser::stream::expansion_budget`).
- **Bounded `parse_error.log`.** A failed statement's context is cut to 200
  characters and its message to 512 (`SyntaxError`), so a long prefix no
  longer turns a small file into a log hundreds of times its size.
- **Security.** The single-database mode (`gserver DIR`), which has no
  authentication, refuses to listen beyond loopback unless started with
  **`--allow-unauthenticated`** (new). `remote=true` downloads never reach a
  loopback, link-local or other host-local address unless the load policy
  allows it (`LoadPolicy::allow_local_hosts`, **new**; gserver never sets it;
  `--allow-service` documents that it enables these downloads). A zip
  package named twice in one request is unpacked once, and the packages of
  one request share `unzip_max_entries` / `unzip_max_size`; a URL named twice
  is downloaded once. gconsole escapes control characters (C0, DEL, C1) in
  everything it prints from an answer — table cells, IRIs, headers,
  indented JSON, `pfn_type` text — as `\uXXXX`.
- **Logs.** `refreshconf` tries a log file that could not be opened (at
  startup or by an earlier `refreshconf`) again. **Changed:** the plain-text
  `<db_home>/query.log` and `access.log` follow `querylog_mode` /
  `accesslog_mode` and roll over daily, keeping `querylog_days` /
  `accesslog_days` days. The docs now say that `querylog_path` /
  `accesslog_path` are relative to `db_home` and that the HTTP body cap
  (`upload_max_body_size`) needs a restart.
- **Changed: `upload_allow_extensions` defaults to
  `nt|ttl|n3|rdf|txt|nq|trig`**, so the server's own export of a database
  with named graphs (N-Quads) builds again; with the `gzip` feature
  `x.nt.gz` passes the check as `nt`.
- **`gconsole --local`:** `load <iri> …` / `load silent …` in lower case is
  SPARQL's `LOAD`, not the console's `load <name>`; a non-interactive run
  exits 1 when a statement or command failed.
- **Tests.** `tests/dt_merge5_review.rs`, and new cases in
  `dt_gserver_lifecycle`, `dt_zip_io`, `dt_settings_phase5`, `dt_logging`,
  `dt_gconsole` and the unit tests of `console::table` and `model::term`.
- **Fixed: a refused start no longer blocks the next one.** `gserver`
  refused to serve root's default password beyond loopback only after it
  had created the user store, so a container started without a password
  (as the `Dockerfile`'s own example did) left a store with root at
  `123456` in its volume, and the next start — given
  `GSTORE_ROOT_PASSWORD_FILE`, as the refusal advises — found that store
  and was refused again. It now refuses before creating the store
  (`dt_gserver_lifecycle`); the `Dockerfile`'s example passes a password
  file, as `docs/DEPLOY.md`'s does.

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

### Phase 4: reasoning rules and PFN functions (GAP_PLAN §3.2, §3.3)

Upstream gStore 1.4's two user-programmable features reach the `/api`
server: inference rules (`reasonManage`) and personalized graph functions
(PFN: `funquery` / `funcudb` / `funreview`, `PFN.name({…})` in queries).
They were built in parallel and merged; *Joining them* records what the
merge changed. Every deliberate difference from upstream is listed at the
end.

#### Reasoning rules (`reasonManage`)

- **`reasonManage` (types 1-8)** on the `/api` server — add, list, compile,
  execute, disable, show, delete, check upstream gStore 1.4's JSON inference
  rules, with upstream's parameters (`db_name`, `type`, `ruleinfo`,
  `rulename`), response fields (`list`/`num`; `insert_sparql`,
  `delete_sparql`, `check_sparql`; `insert_sparql`, `select_sparql`,
  `AnsNum`; `delete_sparql`, `AnsNum`; `ruleinfo`; `check_sparql`,
  `effectNum`), status strings (`新建`, `已编译`, `已执行`, `已失效`,
  `已校验`), codes and messages. Root (and `admin` users) only, as upstream.
  Rules live in `<db>.db/reason_rule_files/<rulename>.json` (upstream's
  layout; compact JSON, sorted keys), survive unload / load and restarts, and
  are carried by `backup` / `restore` (also the library's `Database::backup`,
  `backup_dir`, `restore_into` and `restore_incremental`) and removed by
  `drop`.
- **Compiler** (`reason::compile`): upstream's `insert`, `delete`, `check`
  and `select` statements byte for byte, relation (`type` 1) and property
  (`type` 0) rules, `logic` 0 (`UNION`) and 1 (join), filters. Inferred
  triples use upstream's `<Rule:label>` predicates.
- **Execution** goes through the database's normal update path (query cache,
  CSR, write-through, update / redo logs, commit point, check_point save,
  query log, task registry and cancellation); refused (`1004`) while a
  transaction is open on the database.
- **Exact retraction**: each rule records the triples it owns
  (`reason_rule_files/inferred/<rulename>.nt`, created empty by add); the
  conditions are evaluated once per execution and exactly what that
  evaluation derived is inserted; disabling deletes exactly those no other
  rule owns. A rule file without a record (written by upstream gStore, in
  any status) is retracted upstream's way from its validated label alone,
  and re-executing it takes its existing triples over. An execution that
  fails having inserted nothing leaves the rule as it was.
- **Hardening**: every pattern / `return` term must be one SPARQL term, every
  filter one bracket-balanced expression, the label part of one IRI, and
  `return` variables must be bound — validated with the crate's own lexer and
  parser, and the compiled statements are parsed again and shape-checked.
  Execute only ever runs the statement compiled from the definition. Rule
  names must be safe file names (an existing rule file's name only has to
  stay inside the rule directory); at most 1000 rules per database, 1 MiB
  per rule file (fixed limits: there are no `conf.ini` keys). Backups copy
  the rule directory under its lock; `drop` / `restore` replace it under it,
  and the file operations are refused (`1007`) while a lifecycle operation
  holds the database.
- **Library API**: `Database::reason_add`, `reason_list`, `reason_compile`,
  `reason_execute`, `reason_disable`, `reason_show`, `reason_delete`,
  `reason_check`, `reason_db_dir`, `set_reason_db_dir`; a database without a
  directory keeps its rules in memory (and writes them into its backups and
  saves); `into_memory` reads a database's rules into memory with it.
- **Fix: RDFS range** no longer infers `"lit" rdf:type C` — a triple with a
  literal subject — from `p rdfs:range C` and `x p "lit"`.

#### PFN personalized functions (`docs/PFN.md`)

- **New: `funquery`, `funcudb`, `funreview` on `/api`** (upstream
  `ApiPFNOperation.cpp`), open to every user (as upstream grants them) and
  acting on the caller's own functions only. `funcudb` `type` `1`/`2`/`3`/`4`
  creates, updates, deletes and compiles, with upstream's messages
  (`"Function create success."`, `"Function build fail: …"`, a compilation
  error answered as the compiler's message, the `1003` parameter errors);
  `funquery` lists upstream's six `funInfo` fields filtered by `funName` /
  `funStatus`; `funreview` returns the generated module text, URL-encoded.
  Statuses `"1"` (not compiled), `"2"` (compiled), `"3"` (failed);
  `funBody` / `funSubs` are URL-decoded as upstream does. Functions are
  stored as upstream stores them, per user: `<pfn_base_path>/src/<user>/
  data.json` (+ the generated source) and `lib/<user>/lib<name><md5>.wasm`.
- **New: `SELECT (PFN.name({…}) AS ?v)` is evaluated** (it was a query
  error): with upstream's projection shape and argument handling
  (`pfnQuery` / `pfnQueryByVar`: the argument canonicalized, `"?x"` values
  replaced by every distinct binding, one call per combination in sorted
  order, the outputs as a JSON array; one call with the argument as written
  when there is no solution). The `/api` `query` operation answers upstream's
  `{"results": …}` document (without `AnsNum` / `OutputLimit` / `ThreadId`
  for a statement starting `select (pfn.`); elsewhere the value is a plain
  literal. A query calls the functions of the user it runs for.
- **Runtime: sandboxed WebAssembly, new optional feature `pfn-wasm`**
  (`wasmi` 2.0.0 with `std`, `validate`, `auto-dispatch` only; `wat` 1.263.0
  without the component model). The default build gains no dependency.
  A body is WebAssembly text wrapped into a module that imports the
  `"gstore"` host interface (`GraphCtx`: vertex / edge / predicate counts,
  per-predicate out- / in-degrees and neighbours, id ↔ string, the
  arguments, `emit*` output); a complete `(module …)` or a
  `wasm-base64:` binary module is accepted too. Compilation validates the
  module and its imports / exports and refuses WASI, other imports and
  start functions. Each call runs in a fresh instance under an instruction
  budget (fuel, checked with the query's cancel flag between slices, so
  `operationTaskCancel` and `/cancel` stop a looping function), a memory cap,
  the `gfun_max_output_bytes` output bound and bounds-checked guest
  pointers; traps are query errors. Without the feature, management works and
  compiling / calling fail with an error naming `pfn-wasm`. CI builds,
  lints and tests the feature.
- **New `conf.ini` `[pfn]` section** (Rust extension): `pfn_fuel_per_call`
  (10^9), `pfn_max_memory_pages` (1024), `pfn_max_module_bytes` (1 MiB),
  `pfn_max_functions` (100), applied with the `[query]` settings (and by
  `refreshconf`); upstream's `[system] pfn_base_path` now places the
  functions (default `<db_home>/pfn`, read at startup).
- **API:** `gstore::pfn` (`PfnRegistry`, `PfnCaller`, `PfnInfo`,
  `PfnLimits`, `GraphCtx` / `CsrGraphCtx`, `ResolvedFn`),
  `Evaluator::with_pfn`, `Database::set_pfn_caller` / `pfn_caller` /
  `set_pfn_limits` / `pfn_limits` / `query_as`, `QuerySettings::pfn`,
  `ApiServer::pfn_registry`. `ConcurrentDb` snapshots, version views and
  transactions, and HTTP transaction snapshots carry the caller.
- **Query cache:** PFN queries are never cached (their result depends on
  the caller and on the function's version) — now documented and tested.
- **Changed tests:** `config::every_key_is_read_from_its_section` reads a
  `[pfn]` section too (it requires every known key), and the
  `QuerySettings` literals of two existing tests gained the `pfn` field;
  an engine test calls graph functions through the new projection-call
  dispatcher; the two tests that expected "PFN not supported yet" expect
  the new "no PFN function registry is attached" error. No assertion was
  weakened.

#### Joining them

- **Rules, graph functions and PFN functions share the caches.** Executing
  or disabling a rule changes the data through the update path, so the
  query cache and the CSR the graph functions and PFN functions run on are
  rebuilt: a cached SELECT, `kHopCount` / `shortestPathLen` and a PFN
  function see the inferred `<Rule:label>` edges after execute and lose
  them after disable, on a CSR preloaded with `loadcsr` too
  (`tests/dt_phase4_wiring.rs`, the PFN part with `pfn-wasm`).
- **Fixed: rules of a database the REST layer built.** A database built or
  created in memory by REST has no home directory, so its rules followed
  the served directory only once a `reasonManage` call had reached it, and
  REST `/backup` until then wrote none. Every served database now keeps its
  rules in the directory it is served from (`DbSlot::handle`); the
  per-call fix-up in `reasonManage` and the extra rule copy in `/api`
  `backup` are gone.
- **Configuration.** `conf/conf.ini.example` documents `[system]
  pfn_base_path` (an upstream key its shipped file leaves out), says that
  `refreshconf` re-applies `[pfn]`, and lists the reasoning rules' fixed
  limits; tested: `refreshconf` changes `pfn_max_functions` for the
  registry and `pfn_fuel_per_call` for a database loaded before it.
- **Privileges.** As upstream's `check_privilege`: a user created with
  `usermanage` holds no privilege and still manages their own PFN
  functions, `reasonManage` stays root's (`1004 "You have no reasonManage
  privilege"`), and `showuser` lists upstream's seven privileges — Phase 4
  adds none. Tested.
- **Docs.** `docs/GAP_PLAN.md` records §3.2 and §3.3 as done (the PFN
  runtime decision: WebAssembly in `wasmi`); `docs/CPP_PARITY.md` gains
  rows for both.

#### Deliberate divergences from upstream

Reasoning rules (also in the README's *Reasoning* section):

- Retraction is exact (see above). Upstream's `delete where {?x
  <Rule:label> ?y.}` also deletes triples asserted with that predicate
  before the rule ran and triples another rule with the same label
  inferred; a triple a user inserts *after* the rule inferred it is the
  same RDF triple and is retracted with the rule here too. Disabling a
  compiled rule that was never executed deletes nothing.
- Rules are validated (see *Hardening*); upstream splices the strings into
  SPARQL verbatim, so a rule could inject SPARQL. Prefixed names are
  refused (a rule has no `PREFIX`es). Rule names must be safe file names
  (no `/`, `\`, `..`, leading `.` or control characters, at most 128
  characters and 240 bytes), and the number and size of rules are bounded.
- Execute runs the statement compiled from the rule's definition, never
  text stored in the file: a rule edited after compiling must be compiled
  again, and compiled fields a client sends with `add` are dropped.
- An executed rule cannot be recompiled (upstream allows it, resets the
  status to `已编译` and so lets the rule be deleted with its triples left
  behind); checking an executed rule keeps it `已执行`.
- A top-level `||` filter among several is parenthesized (upstream's plain
  `&&` join would change its meaning).
- `type` may be a JSON number and `ruleinfo` a string holding the object;
  `list` is sorted by name; a missing `rulename` answers `1003`; a database
  that does not exist answers `1005 "database[<db>] does not exist"`
  (upstream creates its directory).
- Execute and disable are refused (`1004`) while a transaction is open;
  check reads the transaction's snapshot. Execute, disable and check write
  a query-log record and register a cancellable task.
- Upstream's documentation shows the compiled text with extra spaces; the
  text here is what its 1.4 code produces.

PFN functions (all in `docs/PFN.md`):

- WebAssembly text run in a sandbox instead of C++ compiled with `g++` and
  `dlopen()`ed: upstream function bodies must be rewritten; the management
  API, the call syntax and the answers are upstream's.
- Limits where upstream has none: instruction budget, memory cap, module
  size, functions per user, output bounded by `gfun_max_output_bytes`.
- A function returning `0` fails the query (upstream ignores the `bool`).
- A call whose argument names no variable runs once next to WHERE
  solutions (upstream answers `[]`).
- `lastTime` is stamped on create / update / compile (upstream: compile
  only); names are SPARQL words of at most 64 characters; sources live
  under `src/` instead of `cpp/`, modules are `.wasm` instead of `.so`, and
  update deletes the old module (upstream leaves the stale `.so`).
- `funquery` paging (`pageNo` / `pageSize`) is an extension; without
  `pfn_base_path` the functions live under `<db_home>/pfn` (upstream:
  `./pfn/` relative to its working directory).
- Outside the `/api` `query` JSON / file answers a PFN value is a plain
  literal (upstream replaces the result document in every JSON form).

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
