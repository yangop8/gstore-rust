# gStore-rust

A Rust workspace with **two** things:

1. **gStore** — a Rust rewrite of [pkumod/gStore](https://github.com/pkumod/gStore)
   (Peking University's RDF triple store / graph database that answers SPARQL by
   subgraph matching): a library + CLIs + an HTTP SPARQL server.
2. **gNLQA** — an LLM-powered **natural-language question-answering** layer over
   gStore: it turns a plain-English (or other-language) question into a validated
   SPARQL query (or a graph-analytics / GraphRAG plan), runs it against gStore,
   and returns a grounded, source-labelled answer.

```
                     ┌─────────────────────────── gNLQA (crate: gnlqa) ───────────────────────────┐
  natural language ─▶│ understand → link → schema → Text-to-SPARQL (N candidates) → self-repair → │
                     │ rank │ + graph-analytics routing │ + GraphRAG fallback │ + LLM-direct (opt.)│
                     └──────────────────────────────────┬──────────────────────────────────────────┘
                                                         │ SPARQL / subgraph queries
  RDF file ─▶ parse ─▶ dictionary (string↔id) ─▶ six-way triple index ─▶ disk   (gStore, the library)
  SPARQL   ─▶ parse ─▶ graph-pattern algebra ─▶ index match + join ─▶ FILTER ─▶ results
```

gNLQA is a separate workspace member (`gnlqa/`) so its heavier dependencies (an
HTTPS client, JSON) stay out of the lean `gstore` crate. It can talk to Claude
**or** any OpenAI-compatible model (OpenAI, **DeepSeek**, …), and only sends data
off-box on the paths you allow — see [Answering modes](#answering-modes--privacy).

* **About 1,770 tests pass** across the workspace (incl. 159 for gNLQA), among them
  ports of upstream gStore's own test scripts; the `pfn-wasm`, `gzip`, `tls` and
  `rocksdb` features add their own. `cargo clippy --workspace --all-targets --
  -D warnings` is clean. Progress toward gStore 1.4 parity is
  planned in [`docs/GAP_PLAN.md`](docs/GAP_PLAN.md) and recorded per phase in
  [`CHANGELOG.md`](CHANGELOG.md).

---

## Quick start

### Build

```bash
cargo build --release           # gStore CLIs + gnlqa
cargo test --workspace          # run the tests
# upstream's scripts/test/parser_test.sh (50 SPARQL cases), in-process, per-case table:
cargo test --test dt_upstream_parser_suite -- --nocapture
# upstream's own test scripts and Python SDK against a real gserver
scripts/conformance.sh
```

### A) Use gStore directly (SPARQL)

```bash
# Build a database from RDF (Turtle / N-Triples) → produces movies.db/
cargo run -q -p gstore --bin gbuild -- movies data.nt

# Serve it over HTTP (SPARQL protocol), or query from the CLI
cargo run -q -p gstore --bin gserver -- movies.db --port 9000   # POST /sparql, /update; GET /status
cargo run -q -p gstore --bin gquery  -- movies -e "SELECT ?o WHERE { <a> <p> ?o }"
```

### B) Ask in natural language (gNLQA)

```bash
# 1) Configure the LLM backend once (default: DeepSeek, an OpenAI-compatible API)
cp gnlqa.conf.example gnlqa.conf      # then edit gnlqa.conf: set OPENAI_API_KEY=<your key>

# 2) Point it at a running gserver (step A) on port 9000, and ask:
cargo run -q -p gnlqa -- "who directed Inception?"        # → Christopher Nolan
cargo run -q -p gnlqa -- chat                             # interactive, multi-turn
```

`gnlqa.conf` holds your key and is git-ignored; only the fake `gnlqa.conf.example`
is committed. To use Claude instead, switch the whole backend block in
`gnlqa.conf` — `GNLQA_LLM_PROVIDER=anthropic`, `ANTHROPIC_API_KEY`, **and** the
model names (`GNLQA_MODEL`/`GNLQA_FAST_MODEL`), since the example pins DeepSeek
models. Full details: **[`gnlqa/README.md`](gnlqa/README.md)**.

### Answering modes & privacy

Because gStore is often used to keep **private data local**, every gNLQA answer is
tagged with where it came from — a *data-egress* signal, not just attribution —
and you choose which paths are allowed (`GNLQA_MODE`, or `/mode` in chat):

| Mode | Path | Does result data leave the box? | Provenance |
|------|------|----------------------|------------|
| `auto` (default) | structured SPARQL → GraphRAG fallback; never pure LLM | only on the GraphRAG fallback | `gStore` / `GraphRAG` |
| `structured` | SPARQL / analytics only; abstain otherwise | **no** — the answer rows stay local | `gStore` |
| `graphrag` | retrieve a private subgraph → LLM answers from it | yes (triples sent to the LLM) | `GraphRAG` |
| `open` | LLM general knowledge; ignore the KB | no KB data involved | `LLM` |

In every mode the LLM sees the question (and, to build a query, the linked
entity/predicate URIs, their labels, and small schema samples). The distinction
above is about the **answer data** — the result rows/triples: `structured` keeps
them local, `graphrag` sends a retrieved subgraph, `open` uses no KB at all.
`GNLQA_MODE` applies on every surface (CLI, chat, `serve`, `mcp`).

A SPARQL-generation *failure* is never silently answered from LLM world-knowledge
(that risks confident hallucination on private-data questions) — LLM-direct only
happens when you explicitly choose `open`.

---

## gStore (the graph database)

Loads RDF, encodes it with gStore's integer-id scheme (entities, literals offset
by `LITERAL_FIRST_ID`, a separate predicate space), stores it in the classic
`s2xx` / `o2xx` / `p2xx` indexes, and evaluates SPARQL through a cost-based
optimizer and a VS-tree signature filter.

* **Cost-based optimizer** — Selinger DP join ordering over predicate statistics.
* **VS-tree signature index** — a port of gStore's 944-bit signatures + signature
  tree, used as a sound query-time candidate filter.
* **SPARQL 1.1** — `SELECT`/`ASK`/`CONSTRUCT` (also `CONSTRUCT WHERE`)/
  `DESCRIBE`, BGP, `UNION`, `OPTIONAL`, `MINUS`, `FILTER` (with `IN` / `NOT
  IN`), `BIND`, `VALUES` (inline and trailing), sub-`SELECT`, aggregates (also
  inside expressions, several `HAVING` constraints, `GROUP BY (expr AS ?v)`),
  property paths (`/ ^ | * + !`), `[ … ]` / `( … )` blank-node syntax, named
  graphs, `ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT`, and SPARQL UPDATE (`INSERT`/
  `DELETE` `DATA` and `WHERE` with `WITH` / `USING` / `GRAPH` templates,
  `LOAD [INTO GRAPH]`, `CLEAR`, `DROP`, `CREATE`, `ADD`, `MOVE`, `COPY`). Typed
  literals are validated and compare by value; a byte-bounded query cache;
  gStore's `CONTAINALL` / `CONTAINANY`.
* **Top-K and early termination** — `LIMIT` / `ASK` / `EXISTS` stop as soon as
  they have their answer, `ORDER BY … LIMIT` uses a bounded heap or upstream's
  DP-B enumeration, with exactly the rows of a full evaluation.
* **RDF input formats** — N-Triples, Turtle, N-Quads, TriG, RDF/XML, and
  (dependency-free, common-subset) **JSON-LD** and **RDFa**; detected by file
  extension (`parser::parse_file_by_ext`).
* **gStore 1.4's advanced graph functions in SPARQL** (32) — `shortestPath`,
  `kHopReachable`, `kHopEnumerate`, `SSSP`, `bfsCount`, `PR`, `PPR`, `WCC`,
  `louvain`, `labelProp`, `closenessCentrality`, `betweennessCentrality`,
  `triangleCounting`, `clusterCoeff`, `JaccardSimilarity`, `maximumKplex`,
  `coreTruss`, … with upstream's syntax, semantics and JSON output; the older
  gstore-rust graph functions remain available as `rs:NAME(…)` (see
  [Graph functions in SPARQL](#graph-functions-in-sparql)).
* **PFN user graph functions** (`pfn`) — upstream's `funquery` / `funcudb` /
  `funreview` and `SELECT (PFN.name({…}) AS ?v)`: each user's own graph
  functions, written in WebAssembly text and run sandboxed over the graph
  (feature `pfn-wasm`; see [PFN](#pfn-user-graph-functions) and
  [`docs/PFN.md`](docs/PFN.md)).
* **On-disk B+ tree KVstore** (`gbuild --disk`) and an optional **RocksDB backend**
  (build with `--features rocksdb`, then `gbuild --rocksdb` / `gserver --rocksdb`)
  — see [`docs/STORAGE_BACKEND.md`](docs/STORAGE_BACKEND.md).
* **HTTP server** speaking upstream gStore's **`/api` protocol** (the upstream
  SDKs work unchanged) and a REST/SPARQL-protocol API: content negotiation,
  users with per-database privileges, IP lists, query/access/transaction logs,
  asynchronous operations with callbacks, file upload/download, a built-in web
  console (`GET /`), task cancellation, and upstream's `conf.ini` and
  `gserver` lifecycle (`server` / `http_api` / `gapi` / `sysdb` / `config`).
* **Transactions** — commit/rollback, per-key MVCC + snapshot isolation, four
  named isolation levels, and full + **incremental** backup/restore.
* **Graph analytics** (`analytics`) — BFS/shortest-path, PageRank, connected
  components, degree/betweenness/closeness centrality, communities, triangles,
  k-core, weighted top-k.
* **Reasoning** (`reason`) — upstream's JSON inference rules over `/api`
  (`reasonManage`: add, compile, execute, disable, … with exact retraction;
  see [Reasoning](#reasoning-reasonmanage)) and RDFS materialization; the
  inferred `<Rule:…>` edges are ordinary data to SPARQL, the graph functions
  and PFN functions. Also an **RPC cluster** (`rpc` / `cluster`), and
  **concurrency/MVCC** (`concurrent`).

Remaining large-refactor items are tracked in
[`docs/REFACTOR_BACKLOG.md`](docs/REFACTOR_BACKLOG.md); C++ parity is documented in
[`docs/CPP_PARITY.md`](docs/CPP_PARITY.md).

### CLI tools

A database is a `<name>.db` directory. Main tools (there are more — `gadd`,
`gsub`, `gdrop`, `gshow`, `gbackup`, `grestore`, `gexport`, `gmonitor`, `gnode`):

```bash
gbuild mydb data.nt          # build a DB from RDF (add --disk for the on-disk B+ tree)
gquery mydb query.rq         # run a SPARQL query (or -e "…" inline)
gconsole mydb                # interactive REPL (end a query with ';')
gserver mydb.db --port 9000  # single-DB HTTP SPARQL server + web console at /
gserver --api . --port 9000  # full multi-DB API server (users/RBAC, upload, txns,
                             # tasks, backup/restore); default login root / 123456
gserver                      # the same API server rooted at db_home (./dbhome/)
                             # on port 9000, settings from ./conf/conf.ini
gserver -b                   # … started in the background (-s: foreground)
gserver -S | -t | -r | -k    # status / stop / restart / kill it (via its pid file)
```

Every tool opens a database directory whatever its layout (snapshot, on-disk
`--disk`, RocksDB), and updates are persisted back to it. `gserver` saves each
write before acknowledging it (gStore's `check_point`; `--no-checkpoint` defers
saving to `/checkpoint`, `/unload` and the graceful shutdown on Ctrl-C/SIGTERM).
SPARQL `LOAD` (and `/build?db_path=`) sent to a server reads nothing unless
`--load-dir DIR` allows it, `/backup` and `/restore` stay inside the server's
backup directory (`<root>/backups`, or `conf.ini`'s `backup_path`), and SPARQL
`SERVICE` contacts no remote endpoint unless `--allow-service` is given.

A database directory is open in **one process at a time**: opening it takes an
operating-system lock on `<name>.db/LOCK` (snapshot and `--disk` layouts;
RocksDB locks itself), held for as long as the database is open. So while a
`gserver` has a database loaded, `gquery`, `gadd`, `gconsole`, `gbackup`, … on
the same directory fail with "… is locked by another process (pid N)" instead
of racing it — unload it there (or use the server's API) first. The server's
`drop`, `rename`, `restore` and rebuilding `/build` refuse a directory another
process holds, too. `Database::read_copy` reads a detached copy of a snapshot
database without the lock.

### Upstream `/api` compatibility and conformance

The multi-DB server speaks upstream gStore 1.4's `/api` protocol (`GET|POST
/api` or `POST /` with `operation=…`, per-request `username`/`password`,
`encryption=1`, the `StatusCode` JSON envelope): the database, user, privilege,
transaction, log and task operations with upstream's parameters, fields, codes
and messages, so the upstream SDKs such as `GstoreConnector.py` work against
it unchanged, plus upstream's `/shutdown` and `/restart` routes. Its graph
functions answer as upstream's (see below), and `loadcsr` / `load` with
`csr=1` build the adjacency they run on ahead of the first query. Known
differences from upstream: a database is **single-writer** — one transaction
may be open on it at a time (a second `begin` answers `1008`), and while it is
open other writes to it (`query` updates, `batchInsert`, …) answer `1004`
until it is committed or rolled back, where upstream runs concurrent
transactions; zip packages (`build`/`batchInsert`/`restore` from a `.zip`,
`compress=true` for `backup`/`export`) are refused with `1003` for now; a user
holding the restore privilege on a database restores only that database's own
backups; and asynchronous results are kept out of the public export directory.

Conformance is tested against upstream's own material, vendored under
[`testdata/upstream/`](testdata/upstream/README.md):

- `tests/upstream_*.rs` replay upstream's `basic_test.sh`, `parser_test.sh`
  (all 50 cases), `bfs_test`, `update_test.sh`, `transaction_test.sh` and
  `api_test.sh` over `/api`;
- `tests/dt_upstream_parser_suite.rs` runs the same 50 parser cases
  in-process, comparing every row with upstream's result files, with the
  Top-K optimizations on and off (two cases follow W3C where upstream's
  script records a different count; both suites document them in one table);
- `scripts/conformance.sh` starts the real `gserver` and runs upstream's
  scripts and Python SDK against it unmodified.

### Configuration (`conf.ini`)

`gserver` reads upstream gStore's `conf.ini` (`./conf/conf.ini`, or `-c FILE`;
see the commented [`conf/conf.ini.example`](conf/conf.ini.example)): every
upstream key of `[server]`, `[system]`, `[log]`, `[backup]` and `[upload]`,
with upstream's defaults, plus a few gstore-rust keys. Command-line flags win
over the file; `operation=refreshconf` re-reads it. The `[query]` section
(gstore-rust only — upstream hard-codes these) sets how **every database the
server serves** evaluates queries; it is applied to each database the server
loads, builds, creates or restores (REST and `/api` alike, transaction
snapshots included), and `refreshconf` re-applies it to the loaded ones:

| key | default | meaning |
|-----|---------|---------|
| `integer_division` | `decimal` | `7/2` = `3.5` (SPARQL 1.1); `truncate` = upstream's `7/2` = `3` |
| `query_cache_size` | `67108864` | per-database read-query cache budget in bytes; `0` = off |
| `early_termination` | `on` | stop `LIMIT` / `ASK` / `EXISTS` evaluations once enough solutions are found |
| `topk_bounded_heap` | `on` | `ORDER BY … LIMIT` keeps the best rows in a bounded heap |
| `topk_dpb` | `auto` | DP-B Top-K for `ORDER BY <linear expr> LIMIT`: `auto`, `always`, `off` |
| `gfun_max_output_bytes` | `67108864` | most bytes of JSON one graph-function call may produce (an embedded `Database` allows 256 MiB) |

In code: `GStoreConfig::query_settings()`, `ApiServer::with_query_settings`,
`Server::with_query_settings`, `Database::apply_query_settings`.

A `[pfn]` section (gstore-rust only) sets the sandbox limits of
[PFN user functions](#pfn-user-graph-functions), applied the same way (to
every served database, and re-applied by `refreshconf` to the loaded ones and
to the function registry): `pfn_fuel_per_call` (instructions per call,
default 10^9, `0` = unlimited), `pfn_max_memory_pages` (default 1024 =
64 MiB), `pfn_max_module_bytes` (default 1 MiB) and `pfn_max_functions` (per
user, default 100). Upstream's `[system] pfn_base_path` (default here:
`<db_home>/pfn`) is where the functions are stored; it is read at startup.

[Reasoning rules](#reasoning-reasonmanage) have no `conf.ini` keys: their
limits are fixed (1000 rules per database, 1 MiB per rule file, 64
conditions per rule, 256 patterns and 64 filters per condition, rule names of
at most 128 characters and 240 bytes), and executing or disabling a rule is
an ordinary update (`check_point`, the `[query]` cache and the CSR behave as
for any other write).

### Graph functions in SPARQL

gStore 1.4's advanced functions (upstream `docs/en-us/SPARQL_QUERY.md`,
"Advanced functions") are written as a SELECT projection, with upstream's
argument syntax: vertices as variables or IRIs, a `directed` boolean, a
predicate set (`{}` = every predicate, `{<p>, <q>}`, `{!<p>}` = all but `<p>`)
and, for some, integers or named parameters (`alpha=`, `maxIter=`, `tol=`,
`increase=`). Keywords are case-insensitive.

```sparql
SELECT (shortestPath(<Francis>, ?x, true, {<like>, <focus>}) AS ?y)
WHERE { <Bob> ?pred ?x . MINUS { <Francis> <dislike> ?x . } }
```

returns one row whose `?y` is the plain literal

```json
{"paths":[{"src":"<Francis>","dst":"<Alice>","edges":[{"fromNode":4,"toNode":3,"predIRI":"<like>"},…],"nodes":[…]}]}
```

| family | functions |
|--------|-----------|
| paths | `simpleCyclePath`, `simpleCycleBoolean`, `cyclePath`, `cycleBoolean`, `shortestPath`, `shortestPathLen`, `shortestPathCount`, `shortestPaths`, `kHopReachable`, `kHopReachablePath`, `kHopEnumerate`, `kHopCount`, `kHopNeighbor`, `kHopAllNeighbors`, `bfsCount`, `SSSP`, `SSSPLen` |
| importance | `PR`, `PPR`, `closenessCentrality`, `betweennessCentrality`, `triangleCounting`, `diameterEstimation` |
| communities | `WCC`, `labelProp`, `louvain`, `maximumKplex`, `coreTruss` |
| correlation | `clusterCoeff`, `JaccardSimilarity`, `degreeCorrelation`, `confidenceDegree` |

As upstream: a variable argument stands for all its distinct values in the
WHERE solutions (two vertex arguments: their cross product), and the result is
computed once — a lone function yields one row; next to other projected
variables its value is on the last row; with `GROUP BY`, once per group. The
functions run on a per-predicate CSR that each database builds on the first
such query after a change and then reuses (`Database::preload_csr` builds it
eagerly, as the server's `loadcsr` and `load` with `csr=1` do). A call is
cancelled like any query (`/cancel`, `operationTaskCancel`, shutdown) and its
JSON is bounded (`[query] gfun_max_output_bytes`, 64 MiB on a server). Upstream parses `cycleBoolean`,
`simpleCycleBoolean`, `shortestPathLen` and `kHopReachable` inside `FILTER` but
never evaluates them there; here that is a clear error. `PFN.name({…})` calls a
user's own function (see [PFN](#pfn-user-graph-functions) below).

**The `rs:` namespace (breaking change).** The functions gstore-rust offered
before — bare names such as `SSSP(?a, ?b)`, usable in `FILTER`/`BIND` and
computed per row over a predicate-blind graph — now live in the IRI namespace
`<urn:gstore-rust:fn:>`, predeclared as the prefix `rs:` (a query's own
`PREFIX rs:` replaces it), with unchanged semantics and usable anywhere:

| before | now | upstream function of that name |
|--------|-----|--------------------------------|
| `SSSP(?a, ?b)` (path as text) | `rs:SSSP(?a, ?b)` | `SSSP(u, directed, preds)` |
| `SSSPLEN(?a, ?b)`, `SHORTESTPATHLEN(?a, ?b)` | `rs:SSSPLEN(…)`, `rs:SHORTESTPATHLEN(…)` | `SSSPLen(u, …)`, `shortestPathLen(u, v, …)` |
| `KHOPREACHABLE(?a, ?b, k)` | `rs:KHOPREACHABLE(…)` | `kHopReachable(u, v, directed, k, preds)` |
| `KHOPCOUNT(?a, k)`, `KHOPNEIGHBOR(?a, k)`, `KHOPALLNEIGHBORS(?a, k)` | `rs:KHOPCOUNT(…)` … | `kHopCount(u, directed, k, preds)` … |
| `BFSCOUNT(?a [, k])`, `SHORTESTPATHS(?a, ?b)` (path count) | `rs:BFSCOUNT(…)`, `rs:SHORTESTPATHS(…)` | `bfsCount(u, …)`, `shortestPaths(u, v, …)` |
| `CYCLEBOOLEAN()`, `SIMPLECYCLEBOOLEAN()` (graph has a cycle) | `rs:CYCLEBOOLEAN()` … | `cycleBoolean(u, v, directed, preds)` … |
| `WCC(?a [, ?b])`, `LOUVAIN(…)`, `LABELPROP(…)` (component size / same community) | `rs:WCC(…)` … | `WCC(preds)`, `louvain(…)`, `labelProp(…)` |
| `PPR(?src, ?target)` (one score) | `rs:PPR(…)` | `PPR(u, hopCnt, preds, retNum)` (FORA top-k) |
| `BETWEENNESSCENTRALITY(?a)`, `CLOSENESSCENTRALITY(?a)` | `rs:…(?a)` | `betweennessCentrality(u, directed, preds)`, … |
| `TRIANGLECOUNTING([?a])`, `DEGREECORRELATION()`, `DIAMETERESTIMATION()`, `JACCARDSIMILARITY(?a, ?b)` | `rs:…(…)` | `triangleCounting(directed, preds)`, … |
| `CLUSTERINGCOEFF([?a])`, `KHOPENUMERATEPATH(?a, k)` | unchanged, or `rs:…` | — (upstream: `clusterCoeff`, `kHopEnumerate`) |
| `SIMILARITY(?a, ?b)` (node / string similarity) | `rs:SIMILARITY(?a, ?b)` | `SIMILARITY(a, b)` = upstream's Jaro string similarity |

A bare call with the old argument shape (`SSSP(?a, ?b)`) is a parse error
naming its `rs:` replacement. Other IRI function calls are SPARQL 1.1 casts
(`xsd:integer(?x)`, `xsd:decimal`, `xsd:double`, `xsd:float`, `xsd:boolean`,
`xsd:string`, `xsd:dateTime`, `xsd:date`) or functions registered with
`Database::register_function` under the IRI or its local name; anything else is
an error.

### Reasoning (`reasonManage`)

Upstream gStore 1.4's inference rules (`src/Reason`, `/api` operation
`reasonManage`): a rule is a JSON object whose conditions are triple patterns
and filters, compiled to a SPARQL `INSERT … WHERE`; executing it adds the
inferred triples under the predicate `<Rule:label>`, disabling it retracts
them.

```json
{"rulename": "舅舅", "description": "母亲的男性同胞是舅舅", "isenable": 1,
 "type": 1, "logic": 1,
 "conditions": [{"patterns": [
     {"subject": "?x", "predicate": "<母亲>", "object": "?y"},
     {"subject": "?y", "predicate": "<父亲>", "object": "?z"},
     {"subject": "?k", "predicate": "<父亲>", "object": "?z"},
     {"subject": "?k", "predicate": "<性别>", "object": "\"男\"^^<http://www.w3.org/2001/XMLSchema#string>"}],
   "filters": []}],
 "return": {"source": "?x", "target": "?k", "label": "舅舅"}}
```

`type` `1` infers a relation (`source <Rule:label> target`), `0` a property
(`source <Rule:label> value`); `logic` `0` makes the conditions alternatives
(`UNION`), `1` requires them all; a condition's `filters` are SPARQL
expressions that must all hold. The rule above compiles — byte for byte as
upstream 1.4 — to

```sparql
insert { ?x <Rule:舅舅> ?k. } where { ?x <母亲> ?y. ?y <父亲> ?z. ?k <父亲> ?z. ?k <性别> "男"^^<http://www.w3.org/2001/XMLSchema#string>.}
```

| `type` | operation | answer | rule `status` |
|---|---|---|---|
| `1` | add (`ruleinfo`) | `StatusMsg` names the rule file | `新建` |
| `2` | list | `list`, `num` | — |
| `3` | compile (`rulename`) | `insert_sparql`, `delete_sparql`, `check_sparql` | `已编译` |
| `4` | execute | `insert_sparql`, `select_sparql`, `AnsNum` (triples added) | `已执行` |
| `5` | disable | `delete_sparql`, `AnsNum` (triples retracted) | `已失效` |
| `6` | show | `ruleinfo` | — |
| `7` | delete | — | (file removed) |
| `8` | check | `check_sparql`, `effectNum` (matches) | `已校验` |

Only `root` (and `admin` users) may call it, as upstream. Rules are stored per
database in `<db>.db/reason_rule_files/<rulename>.json` (upstream's layout and
format), so they survive unload / load and restarts, move with `rename`, are
carried by `backup` / `restore` and removed by `drop`; a database without a
directory (built in memory through the library) keeps them in memory. The
library offers the same lifecycle: `Database::reason_add`, `reason_list`,
`reason_compile`, `reason_execute`, `reason_disable`, `reason_show`,
`reason_delete`, `reason_check`.

Execution is an ordinary autocommit update (query cache, CSR, write-through,
durability and the query log behave as for a `query` update, and it can be
cancelled); execute and disable are refused (`1004`) while a transaction is
open on the database, and execute / disable / check need it loaded.

**Retraction is exact.** Each execution records which triples the rule owns
(`reason_rule_files/inferred/<rulename>.nt`): those it added, and those it
derived that another executed rule already owns. Disabling deletes exactly the
owned triples no other rule also owns. So a fact asserted with an ordinary
predicate is never touched; a `<Rule:…>` triple that was already in the
database before the rule ran (asserted by a user) survives; a triple two rules
infer survives until both are disabled. A triple a user inserts *after* the
rule inferred it is the same RDF triple and is retracted with the rule. A
rule's conditions are evaluated once per execution and exactly the triples
that evaluation derived are inserted, so a random filter cannot leave a triple
unrecorded. A rule file without an ownership record was written by upstream
gStore: whatever its status (upstream's check and compile overwrite `已执行`),
disabling it retracts with upstream's `delete where {?x <Rule:label> ?y.}`
(built from the validated label alone, so a rule whose conditions this
implementation refuses can still be disabled, and its name only has to stay
inside the rule directory), and executing it takes over the triples it derives
that are already there.

**Differences from upstream**, deliberate:

- Every rule is validated when it is added and compiled: each pattern or
  `return` term must be exactly one SPARQL term (variable, full `<IRI>`,
  literal, number, boolean, `a`), a filter one complete expression whose
  brackets balance, the label part of one IRI, and a `return` variable must
  occur in the patterns. Upstream splices the strings into SPARQL verbatim,
  so a rule could inject SPARQL; here such a rule is refused (`1005 "Add
  Reason Rule Fail. …"`). Prefixed names are refused (a rule has no
  `PREFIX`es).
- Rule names must be safe file names (no `/`, `\`, `..`, leading `.`, control
  characters; at most 128 characters and 240 bytes of UTF-8), at most 1000
  rules per database and 1 MiB per rule file.
- Execute runs the statement compiled from the rule's definition, never text
  stored in the file: a rule edited after compiling must be compiled again;
  compiled fields a client sends with `add` are dropped.
- An executed rule cannot be recompiled (upstream allows it, resets the
  status to `已编译` and thereby lets the rule be deleted while its triples
  stay); checking an executed rule keeps it `已执行`.
- When a condition has several filters and one has a top-level `||`, that
  filter is parenthesized (upstream's plain `&&` join would change its
  meaning).
- Upstream's documentation shows the compiled text with extra spaces
  (`where {  ?x … . }`); the text here is what upstream 1.4's code produces.
- `type` may be a JSON number and `ruleinfo` a string holding the object;
  `list` is sorted by rule name; operations on a database that does not exist
  answer `1005 "database[<db>] does not exist"` (upstream creates its
  directory).

### PFN: user graph functions

gStore 1.4's personalized functions — each user's own graph functions,
managed with the `/api` operations `funquery`, `funcudb` (create / update /
delete / compile) and `funreview`, and called as
`SELECT (PFN.name({"src":"<…>", …}) AS ?v) WHERE {…}` — are supported with
upstream's requests, messages, statuses, storage layout and answers (a
`query` answers `{"results": …}`). The difference is the runtime: upstream
compiles a C++ body with `g++` and `dlopen()`s it, gstore-rust compiles a
**WebAssembly text** body and runs it **sandboxed** in an interpreter — it
can only call the gStore host interface (vertices, edges, per-predicate
neighbours, id ↔ string, its arguments, an output buffer), under an
instruction budget, a memory cap, the output bound and the query's cancel
flag. Every user reaches only their own functions. PFN calls are evaluated
by the multi-database `gserver` (API mode) and by embedded databases with a
caller installed (`Database::set_pfn_caller`); the single-database
`gserver <db dir>` and the command-line tools answer a PFN call with an
error.

The runtime is the cargo feature `pfn-wasm` (`wasmi` + `wat`, off by
default): without it, function management works but compiling and calling
fail with an error naming the feature.

```sh
cargo build --release --features pfn-wasm
```

[`docs/PFN.md`](docs/PFN.md) is the guide: the host interface, two complete
examples (out-degree; k-hop count along a predicate set), the limits and every
difference from upstream.

### Library API

```rust
use gstore::Database;

let mut db = Database::build_from_files("demo", &["data.nt"])?;
db.query("INSERT DATA { <a> <p> <b> }")?;
let rs = db.select("SELECT ?o WHERE { <a> <p> ?o }")?;
println!("{}", rs.to_table_string());
db.save("demo.db")?;                 // persist
let db = Database::load("demo.db")?; // reload
```

### Experimental: cluster

The `cluster` / `rpc` modules and the `gnode` binary (sharded scatter-gather
over TCP, Raft-style replication, elastic membership) are an **experimental
library feature**: they are not wired into `gserver` or the HTTP API, keep all
state in memory, speak an unauthenticated wire protocol that may change, and
have no counterpart in upstream gStore 1.4 (whose `src/Cluster` is not built,
and whose distributed edition is not open source). A shard that cannot answer
fails the query with an error rather than yielding a partial result. Use it for
experiments, not for data you need to keep.

---

## gNLQA (natural-language QA)

An LLM front-end over gStore. Its distinguishing move: LLM-generated SPARQL is
parsed and repaired with gStore's **own** parser before execution, so a candidate
never runs unless it's valid (and read-only). Highlights:

* **Text-to-SPARQL** with schema grounding, N candidates, self-repair, and
  multi-candidate disambiguation.
* **Graph-analytics routing** (shortest path / centrality / … → `gstore::analytics`)
  and a **GraphRAG** fallback for open questions.
* **Multi-turn** conversation, **multilingual** answers, confidence + **abstention**,
  and grounded **citations**.
* **Pluggable LLM backend** — Claude or any OpenAI-compatible endpoint (DeepSeek…).
* Interfaces: CLI (`ask` / `chat`), **HTTP** (`/ask`, gAnswer-compatible `/gSolve`),
  an **MCP** server (stdio), and an **eval** harness (QALD / LC-QuAD → P/R/F1).

Design: [`docs/NLQA_DESIGN.md`](docs/NLQA_DESIGN.md). Usage: [`gnlqa/README.md`](gnlqa/README.md).

---

## Layout

| module        | role                                                    |
|---------------|--------------------------------------------------------|
| `model`       | RDF terms, triples, id conventions                     |
| `dict`        | bidirectional string↔id dictionaries                   |
| `store`       | in-memory `s2xx`/`o2xx`/`p2xx` six-way index           |
| `kvstore` / `backend` | on-disk pager + B+ trees; RocksDB backend      |
| `signature`   | signatures + VS-tree candidate filter                  |
| `parser`      | N-Triples/Turtle/N-Quads/TriG/RDF-XML/JSON-LD/RDFa; SPARQL (query + update) |
| `query`       | algebra eval, joins, optimizer, FILTER, aggregates, Top-K |
| `analytics`   | CSR graph algorithms; `analytics::gfun`: upstream's 32 advanced functions |
| `server` / `http_api` / `http_users` | HTTP SPARQL server, conneg, auth |
| `gapi` / `sysdb` / `config` | upstream `/api` protocol, system database (privileges, logs), `conf.ini` |
| `rpc` / `cluster` / `concurrent` | RPC cluster, MVCC/transactions      |
| `reason`      | upstream inference rules (`reasonManage`), RDFS, textual rules |
| `pfn`         | PFN user graph functions: per-user registry, `GraphCtx`, sandboxed WebAssembly runtime (`pfn-wasm`) |
| `db`          | `Database` facade + persistence (mem & disk)           |
| `gnlqa/`      | LLM natural-language QA layer (separate crate)          |
| `bin/`        | `gbuild`, `gquery`, `gconsole`, `gserver`, …            |

See [`docs/DESIGN.md`](docs/DESIGN.md) for the gStore architecture and the mapping
to gStore's C++ modules.

## License

Follows upstream gStore (BSD-3-Clause). An independent reimplementation for study
and engineering purposes.
