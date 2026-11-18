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

* **About 3,316 tests pass** across the workspace (incl. 266 for gNLQA:
  189 unit, 77 integration; `cargo test --workspace` after the review-fix
  campaign, run once with on-disk databases in each query mode), among them
  ports of upstream gStore's own test scripts; the `pfn-wasm`, `zip`,
  `gzip`, `tls`, `readline` and `rocksdb` features add their own. `cargo clippy
  --workspace --all-targets -- -D warnings` is clean on the toolchain CI pins
  (Rust 1.99.1, `RUST_TOOLCHAIN` in `.github/workflows/ci.yml`; a newer local
  clippy may report lints CI does not see until the pin is bumped), and CI
  checks `cargo fmt --all -- --check`. Progress toward gStore 1.4 parity is
  planned in [`docs/GAP_PLAN.md`](docs/GAP_PLAN.md) and recorded per phase in
  [`CHANGELOG.md`](CHANGELOG.md); measurements are in
  [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

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

The default build is dependency-light. Optional cargo features (all off by
default): `pfn-wasm` (the sandboxed WebAssembly runtime that compiles and
runs [PFN user functions](#pfn-user-graph-functions)), `zip` (zip packages,
zipped backups and exports), `gzip` (HTTP response compression, `.gz`
sources), `tls` (HTTPS listeners), `readline` (`gconsole` line editing) —
all pure Rust — and `rocksdb` (the RocksDB backend; needs clang). For
example `cargo build --release -p gstore --features "pfn-wasm zip gzip"`.
To run `gserver` in a container (the image enables `pfn-wasm`) or as a
systemd service, or from `.deb` / `.rpm` packages, see
[`docs/DEPLOY.md`](docs/DEPLOY.md); its [Package
smoke](docs/DEPLOY.md#package-smoke) section (`scripts/package-smoke.sh`)
installs and runs what the packages and the image ship, as
[`.github/workflows/packaging.yml`](.github/workflows/packaging.yml) does in
CI.

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
mkdir -p ~/.config/gnlqa && cp gnlqa.conf.example ~/.config/gnlqa/gnlqa.conf
                                      # then edit it: set OPENAI_API_KEY=<your key>

# 2) Point it at a running gserver (step A) on port 9000, and ask:
cargo run -q -p gnlqa -- "who directed Inception?"        # → Christopher Nolan
cargo run -q -p gnlqa -- chat                             # interactive, multi-turn
```

`gnlqa` reads its configuration from `$GNLQA_CONFIG`, else
`$XDG_CONFIG_HOME/gnlqa/gnlqa.conf`, else `~/.config/gnlqa/gnlqa.conf`; a
`gnlqa.conf` in the working directory is no longer read (it is ignored with
a note; `GNLQA_CONFIG=./gnlqa.conf` opts in explicitly), and a world-writable
file is refused. The file holds your key; only the fake
`gnlqa.conf.example` is committed. `gnlqa serve` (the HTTP server) needs a
client token, `GNLQA_HTTP_TOKEN`, and refuses to start without one (see
[`docs/DEPLOY.md`](docs/DEPLOY.md#gnlqa-server)). To use Claude instead,
switch the whole backend block in
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
`GNLQA_MODE` applies on every surface (CLI, chat, `serve`, `mcp`). The
follow-up rewrite of a multi-turn chat honours the table too: an earlier
answer is replayed to the LLM only where the current mode lets that data
leave the machine (a locally computed gStore answer only in `graphrag`
mode), and `structured` mode never sends an answer's supporting triples,
not even with `with_explain`.

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
  extension, read by one streaming import pipeline with upstream's tolerant
  bulk loading (see [Importing RDF](#importing-rdf)).
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
* **Three storage layouts** — the in-memory snapshot, an **on-disk B+ tree
  KVstore** (`gbuild --disk`) that answers queries from an in-memory working
  set (Materialize, the default) or straight off the B+ trees with an
  out-of-core dictionary (**Stream**: the default graph opens in
  milliseconds with a few MiB of memory whatever its size; named graphs are
  loaded into memory when the database opens, so open time and memory grow
  with their quads — [`docs/PHASE6_PLAN.md`](docs/PHASE6_PLAN.md) §8 D11),
  and an optional **RocksDB backend**
  (build with `--features rocksdb`, then `gbuild --rocksdb` / `gserver
  --rocksdb`). `gbuild --disk` / `--rocksdb` build in bulk (external sort,
  bottom-up B+ tree pages or SST ingest, a crash-safe install), and the ids
  of deleted terms are reclaimed on every layout — see [Storage layouts and
  query modes](#storage-layouts-and-query-modes),
  [Performance](#performance) and
  [`docs/STORAGE_BACKEND.md`](docs/STORAGE_BACKEND.md).
* **HTTP server** speaking upstream gStore's **`/api` protocol** (upstream's
  Python SDK works unchanged, as `scripts/conformance.sh` checks; the other
  upstream SDKs speak the same protocol but are not tested) and a
  REST/SPARQL-protocol API with secure defaults (see [HTTP
  behaviour](#http-behaviour-and-secure-defaults)): content negotiation,
  users with per-database privileges, IP lists, query/access/transaction logs,
  asynchronous operations with callbacks, file upload/download, a built-in web
  console (`GET /`), task cancellation, and upstream's `conf.ini` and
  `gserver` lifecycle (`server` / `http_api` / `gapi` / `sysdb` / `config`).
* **gconsole** — upstream gStore 1.4's console as a client of a running
  server's `/api`: all 43 commands of upstream's table (databases, users and
  privileges, backups, reasoning rules, PFN functions) and SPARQL typed at
  the prompt, printed as upstream prints them (see [gconsole](#gconsole)).
* **Operations** — upstream's `conf.ini` (plus `[query]`, `[pfn]`, `[log]`,
  `[storage]` and zip-limit extension keys, re-read by `refreshconf`), a daily-rolling
  diagnostic log configured like upstream's log4cplus, JSON query / access
  logs, zip packages and zipped backups (feature `zip`), upstream 1.3's
  command-line spellings, and a container image, a systemd unit and deb /
  rpm packages ([`docs/DEPLOY.md`](docs/DEPLOY.md)).
* **Transactions** — `begin` / `commit` / `rollback` on both HTTP protocols
  (REST `/begin`, `/tquery`, `/commit`, `/rollback` and the `/api`
  operations), one transaction per database at a time: a database is
  single-writer (while one is open, a second `begin` and every other write
  to it are refused — `/api` codes `1008` / `1004`, REST `409`); upstream's
  three `isolevel` values are validated, logged and echoed, and all behave
  serializably. Reads of one database run in parallel on both HTTP servers
  (writes are exclusive), and an HTTP transaction's begin-time snapshot
  copies no triple on in-memory and on-disk databases (copy-on-write stores,
  disk read views; RocksDB still copies). Full backup/restore (`gbackup` /
  `grestore`, `/api` `backup` / `restore`). The library additionally offers
  `concurrent::ConcurrentDb` — snapshot-isolated readers, optimistic
  multi-writer transactions with four named isolation levels
  (`ReadUncommitted` behaves as `ReadCommitted`) and per-key MVCC version
  chains — and redo-log incremental backup / restore
  (`Database::backup_incremental`); no server or command-line tool uses
  either.
* **Graph analytics** (`analytics`) — BFS/shortest-path, PageRank, connected
  components, degree/betweenness/closeness centrality, communities, triangles,
  k-core, weighted top-k.
* **Reasoning** (`reason`) — upstream's JSON inference rules over `/api`
  (`reasonManage`: add, compile, execute, disable, … with exact retraction;
  see [Reasoning](#reasoning-reasonmanage)) and, as library APIs, RDFS
  materialization and textual rules; the
  inferred `<Rule:…>` edges are ordinary data to SPARQL, the graph functions
  and PFN functions. Also an experimental **RPC cluster** (`rpc` /
  `cluster`, see [Experimental: cluster](#experimental-cluster)) and the
  library-only multi-writer **MVCC** wrapper (`concurrent::ConcurrentDb`).

Remaining large-refactor items are tracked in
[`docs/REFACTOR_BACKLOG.md`](docs/REFACTOR_BACKLOG.md); C++ parity is documented in
[`docs/CPP_PARITY.md`](docs/CPP_PARITY.md) (per component, with library-only
and experimental items marked; the upstream gaps still open are listed in
[`docs/GAP_PLAN.md`](docs/GAP_PLAN.md) §9.3).

### CLI tools

A database is a `<name>.db` directory. Main tools (there are more — `gadd`,
`gsub`, `gdrop`, `gshow`, `gbackup`, `grestore`, `gexport`, `gmonitor`,
`gcompact`, `gnode`; the [command-line tools section of
`docs/DEPLOY.md`](docs/DEPLOY.md#command-line-tools) lists them in upstream
1.3's spelling):

```bash
gbuild mydb data.nt          # build a DB from RDF (the snapshot layout, or conf.ini's
                             # [storage] default_layout)
gbuild --disk mydb data.nt   # the on-disk B+ tree, built in bulk (--layout disk;
                             # --memory MB, --threads N, --tmp-dir DIR, --fill PCT)
gbuild --rocksdb mydb data.nt  # RocksDB, built in bulk by SST ingest (feature rocksdb)
gcompact mydb                # show the id spaces; --reclaim frees every unused id,
                             # --enable-reclaim turns reclamation on for an older
                             # kvstore.kv, --check verifies them
gquery mydb query.rq         # run a SPARQL query (or -e "…" inline)
gconsole -u root             # upstream's console, a client of a running gserver
gconsole --local mydb        # the embedded REPL on mydb.db (no server)
gserver mydb.db --port 9000  # single-DB HTTP SPARQL server + web console at /
                             # (no authentication: loopback only, unless
                             # --allow-unauthenticated)
gserver --api . --port 9000 --password-file pw.txt
                             # full multi-DB API server (users/RBAC, upload, txns,
                             # tasks, backup/restore); root's password is read from
                             # pw.txt when the user store is created (or conf.ini's
                             # root_password): gserver refuses root's default
                             # 123456 on every address, unless the loopback-only
                             # compat switch allow_default_root_password=on
gserver                      # the same API server rooted at db_home (./dbhome/)
                             # on port 9000, settings from ./conf/conf.ini
gserver -b                   # … started in the background (-s: foreground)
gserver -S | -t | -r | -k    # status / stop / restart / kill it (via its pid file;
                             # -r starts it with the options given to -r, not
                             # the stopped server's: repeat --load-dir, -p, …)
gexport mydb out.nq          # N-Quads (every graph); out.nt: the default graph;
                             # -z / out.zip: zipped (feature zip)
gbackup [-z] mydb bak        # a backup directory (or bak.zip)
grestore mydb2 bak           # the database first, then the backup (dir or .zip)
```

The tools also take upstream 1.3's options (`-db NAME`, `-f FILE`, `-p PATH`,
`-l`, `-z 0|1`, …), answer `-h`/`--help`, and exit with `0` (done), `1`
(failed) or `2` (invalid arguments; `gserver` too, which used to exit `1`).
`gbuild` also exits `3` when the database was built but tolerant mode
skipped statements (listed in `parse_error.log`) — check for it in
migration scripts. `gbuild` claims the database directory before it writes
anything (a directory another process holds makes it fail at once), keeps
a database it built even when its summary cannot be printed, and removes on
failure only a directory it created; `gquery -f FILE` and `gexport` replace
their output only with a complete one (written to `<dest>.part-<pid>-<n>`,
then renamed); `gdrop` renames the directory aside durably
(`<name>.db.drop-old`) before deleting it; `gbackup` refuses a destination
inside the database. `gbuild` without a source builds an
empty database and refuses to overwrite an existing one unless `--force`;
`gbuild x.zip` reads a zip package (feature `zip`); `gshow` with no argument
lists every database in the current directory. *Changed:* `grestore` used
to take the backup first. `gbuild --disk` / `--rocksdb` print the counts and a
summary of the bulk build (phases, memory budget and peak, term map, spilled
runs) instead of reopening the database; `--incremental` builds them the
previous way, one insert at a time.

Every tool opens a database directory whatever its layout (snapshot, on-disk
`--disk`, RocksDB), and updates are persisted back to it. `gserver` saves each
write before acknowledging it (gStore's `check_point`; `--no-checkpoint` defers
saving to `/checkpoint`, `/unload` and the graceful shutdown on Ctrl-C/SIGTERM).
That save is the whole snapshot on the snapshot layout; the on-disk and
RocksDB layouts commit each acknowledged write incrementally either way. A
write whose save fails is never acknowledged: it is answered as a failure —
HTTP `500` from REST and the single-database server, StatusCode `1005` from
`/api` — with `"applied in memory but not durable: <cause>"`, and clients
must treat such a reply as "not committed". The write stays applied in
memory: on the snapshot layout the next successful save, `/checkpoint`,
`/unload` or shutdown saves it (it is lost if the process dies first); on
the on-disk layouts a failed commit point breaks the database (`storage
failed: …`) until it is reopened (unload + load), which discards the
non-durable writes (see [`docs/DEPLOY.md`](docs/DEPLOY.md#write-failures-and-storage-failed)).
SPARQL `LOAD` (and `/build?db_path=`) sent to a server reads nothing unless
`--load-dir DIR` allows it, `/backup` and `/restore` stay inside the server's
backup directory (`<root>/backups`, or `conf.ini`'s `backup_path`), and SPARQL
`SERVICE` contacts no remote endpoint unless `--allow-service` is given —
which also lets `/api` `build` / `batchInsert` / `batchRemove` with
`remote=true` download `http(s)://` sources. Neither `SERVICE` nor those
downloads ever reach a loopback, link-local, cloud-metadata or other
host-local address unless `--allow-local-hosts` is given too (unsafe on a
shared server; the ranges are listed in
[`docs/DEPLOY.md`](docs/DEPLOY.md#deploying-gstore-rust)).

A database directory is open in **one process at a time**: opening it takes an
operating-system lock on `<name>.db/LOCK` (every layout; a RocksDB database
also holds RocksDB's own `rocksdb/LOCK`), held for as long as the database is
open; builds, saves, restores, `gbackup` and `gdrop` take it too. So while a
`gserver` has a database loaded, `gquery`, `gadd`, `gconsole --local`, `gbackup`, … on
the same directory fail with "… is locked by another process (pid N)" instead
of racing it — unload it there (or use the server's API) first. The server's
`drop`, `rename`, `restore` and rebuilding `/build` refuse a directory another
process holds, too. `Database::read_copy` reads a detached copy of a snapshot
database without the lock.

### Storage layouts and query modes

Every tool and both servers detect a directory's layout (`rocksdb/` first,
then `kvstore.kv`, then the snapshot's `meta.bin`):

| layout | built by | queries run on | writes |
|---|---|---|---|
| snapshot | `gbuild` (`--layout snapshot`) | the in-memory store, loaded whole at open | a save rewrites the snapshot files |
| disk | `gbuild --disk` (`--layout disk`) | **Materialize** (default): an in-memory working set; **Stream**: the B+ trees and an out-of-core dictionary | written to `kvstore.kv` incrementally, at WAL-protected commit points |
| RocksDB | `gbuild --rocksdb` (`--layout rocksdb`, feature `rocksdb`) | RocksDB's column families, read by bounded scans | RocksDB write batches |

* **Which layout a build makes.** `gbuild` without `--layout` / `--disk` /
  `--rocksdb`, REST `/build` and `/api build` without `layout=` use
  `conf.ini`'s `[storage] default_layout`, else the snapshot layout.
* **Disk query mode.** `[storage] disk_query_mode=memory|stream`,
  `GSTORE_DISK_QUERY_MODE=stream|memory`, or
  `Database::open_with(dir, &OpenOptions::stream())`. Stream opens the
  default graph in milliseconds (the named graphs are read whole at open,
  linear in their quads: [`docs/PHASE6_PLAN.md`](docs/PHASE6_PLAN.md) §8
  D11) and keeps only the named graphs, the per-predicate statistics
  and bounded caches in memory; Materialize stays the default because Stream
  is still slower on queries with large results (see
  [Performance](#performance)). Both modes read and write the same
  `kvstore.kv`, so a directory can be opened in either, in turn.
* **RocksDB statements are crash-atomic.** Every SPARQL UPDATE request
  (`INSERT DATA`, `DELETE` / `INSERT … WHERE`, `LOAD`, `CLEAR`, `DROP`,
  `ADD` / `COPY` / `MOVE`) and every import of `gadd`, `gsub` and `/api`
  `batchInsert` / `batchRemove` is journaled in the store's undo journal,
  so a crash part-way (SIGKILL, an OOM kill, a power loss) is undone as a
  whole at the next open, without orphan dictionary terms; recovery of a
  long journal is bounded and resumable. A statement that fails part-way
  keeps what it applied. Not covered (atomic per triple; a crash leaves a
  prefix): REST `/batchInsert` / `/batchRemove`, REST `/upload` into a
  loaded database, and direct library writes (`insert_triple`,
  `materialize_rdfs`, …). A `check_point` save of a RocksDB database is a
  commit point (an fsync of RocksDB's write-ahead log); memtables are
  flushed by `gcompact` / an explicit `Database::checkpoint`, a backup and
  close. `[storage] rocksdb_max_open_files` bounds the files each RocksDB
  database keeps open.
* **Damaged stores are reported**, never a panic: a `kvstore.kv` shorter
  than its header says, an unreadable write-ahead log, an empty or corrupt
  `vstree.kv` header or an out-of-range RocksDB dictionary id fails the
  open; a corrupt VList record or a wrong-length RocksDB key fails the
  statement — each with an error naming the file or column family (a
  corrupt VS-tree node met by a query only turns its candidate filtering
  off). `gcompact --check` also verifies RocksDB's statistics against the
  data.
* **Id reclamation** (`[system] id_reclaim=on`, the default): after a delete,
  every term no triple or quad references any more leaves the dictionary and
  its id is reused by the next new term, lowest first, on every layout; the
  entity / literal / predicate counts count live terms. A transaction frees
  at `commit`. Applications must not cache ids across a delete.
* **Formats.** Snapshot files and RocksDB stores keep their formats, and
  older binaries open them. A **new** `kvstore.kv` is created in version 2
  (magic `GSTOREK2`, with a free-id list) unless `id_reclaim=off`; binaries
  older than Phase 6 refuse it. An existing version-1 `kvstore.kv` is never
  upgraded implicitly: reclamation stays off on it until `gcompact
  --enable-reclaim`. To go back, `gexport` with the new binary and `gbuild`
  with the old one. In such an export → build migration, check `gbuild`'s
  exit status: `3` means it built the database but skipped statements its
  tolerant mode refuses (listed in `parse_error.log`), such as ill-typed
  literals (`"x"^^xsd:integer`) a strict import or an older version stored;
  rebuild with `--strict`, which keeps such literals as written (it stops
  only at a statement that does not parse). The readers normalise
  language tags (`zh_CN` → `zh-CN`, an empty tag → none) and refuse other
  malformed ones, and SPARQL UPDATE refuses the numeric literals the
  tolerant loader refuses, so new data exports and builds back whole.

Details: [`docs/STORAGE_BACKEND.md`](docs/STORAGE_BACKEND.md).

### Performance

Measured with `gstore-bench`, the workspace's benchmark harness
([`bench/README.md`](bench/README.md)), on the 4 vCPU / 16 GB reference VM
with nothing else running: LUBM-like univ 38 (5,181,103 triples), each layout
built by `gbuild` and opened, queried and updated through the public API in
fresh processes; "vs base" is the ratio to the code before Phase 6
(`bench/results/pre-phase6-lubm38.json`). Generated by `gstore-bench report`
from the committed `bench/results/phase6-final-lubm38.json`:

<!-- gstore-bench report --table overview bench/results/phase6-final-lubm38.json --compare bench/results/pre-phase6-lubm38.json -->
| workload | backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| lubm-like univ 38 | snapshot | 29.60 | 550.1 | 430.8 | 1184.8 (cold) | 1033.0 | 489.1 | 601.0 | 10.15 | 0.39× | 0.71× | 0.22× |
| lubm-like univ 38 | disk@materialize | 21.06 | 400.3 | 654.0 | 1817.3 (cold) | 1720.8 | 351.0 | 457.1 | 7.83 | 0.01× | 0.22× | 0.05× |
| lubm-like univ 38 | disk@stream | 18.38 | 398.2 | 654.0 | 7.5 (cold) | 0.9 | 6.1 | 207.3 | 11.55 | 0.01× | 0.00× | 0.08× |
| lubm-like univ 38 | rocksdb | 19.50 | 401.6 | 155.7 | 738.9 (cold) | 678.0 | 270.8 | 436.2 | 25.00 | 0.07× | 0.01× | 0.08× |
<!-- /gstore-bench report -->

At univ 82 (11,135,159 triples, about the size of upstream's lubm10M;
`bench/results/phase6-final-lubm82.json`):

<!-- gstore-bench report --table overview bench/results/phase6-final-lubm82.json -->
| workload | backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| lubm-like univ 82 | snapshot | 69.79 | 1175.4 | 926.5 | 2715.3 (cold) | 2395.9 | 1046.0 | 1245.0 | 21.94 |
| lubm-like univ 82 | disk@materialize | 49.53 | 645.1 | 1405.5 | 4412.0 (cold) | 3404.1 | 734.4 | 939.5 | 16.09 |
| lubm-like univ 82 | disk@stream | 45.88 | 610.0 | 1405.5 | 4.8 (cold) | 0.9 | 6.4 | 380.8 | 30.18 |
<!-- /gstore-bench report -->

Upstream gStore 0.5.0's published lubm10M run built its index in 23 µs per
triple, into 264 bytes per triple, and answered LUBM Q2 in 144 ms and Q14 in
223 ms; at univ 82 the snapshot / disk (Materialize) layouts build in 6.27 /
4.45 µs per triple, into 83.2 / 126.2 bytes per triple, and answer Q2 in
85.3 / 71.2 ms and Q14 in 141.3 / 165.3 ms — with a different data
generator, hardware and harness. The per-query
tables, the WatDiv-like workload, the concurrency and mixed read/write
figures, the comparison's caveats and the Phase 6 exit gates are in
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

To reproduce the univ 38 file and the tables (from the workspace root;
`--drop-caches` needs root):

```sh
cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench
CARGO_TARGET_DIR=target-rocks cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench \
    --features gstore/rocksdb,gstore-bench/rocksdb
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache B=target/release/gstore-bench
s=38
$B run --profile reference --scale $s --extra none --backends snapshot --drop-caches --verify-data --out f$s-s.json
$B run --profile reference --scale $s --extra none --backends disk --env GSTORE_DISK_QUERY_MODE=stream \
    --label stream --drop-caches --verify-data --out f$s-ds.json
$B run --profile reference --scale $s --extra none --backends disk --env GSTORE_DISK_QUERY_MODE=memory \
    --label materialize --drop-caches --verify-data --out f$s-dm.json
target-rocks/release/gstore-bench run --profile reference --scale $s --extra none --backends rocksdb \
    --bin-dir target-rocks/release --drop-caches --verify-data --out f$s-r.json
$B merge --out bench/results/phase6-final-lubm$s.json f$s-s.json f$s-ds.json f$s-dm.json f$s-r.json
$B report --update README.md CHANGELOG.md docs/BENCHMARKS.md
```

`target/release/gstore-bench ci` runs the smaller smoke profile (LUBM-like
univ 1 and WatDiv-like scale 1) and gates it against
`bench/baselines/smoke.json`, as the `bench` CI workflow does.

### Importing RDF

Every bulk load — `gbuild`, `gadd` / `gsub`, the `/api` `build` /
`batchInsert` / `batchRemove`, `gconsole`'s `create` / `batchinsert`, and
SPARQL `LOAD` — reads RDF through one streaming pipeline
(`parser::import`):

* **Format by extension** (`.nt`, `.ttl`, `.n3`, `.nq`, `.trig`, `.rdf` /
  `.owl` / `.xml`, `.jsonld`, `.html`; `.gz` with the `gzip` feature).
  Turtle, TriG, N-Triples and N-Quads stream with bounded memory.
* **Sources**: files, directories (every RDF file below them, sorted), URLs
  (`/api` `remote=true`, only where the load policy allows remote sources)
  and zip packages (feature `zip`).
* **Tolerant, as upstream**: the server's `build` / `batchInsert` /
  `batchRemove` skip a statement that does not parse, log it to
  `<db>.db/parse_error.log` in upstream's format and answer `success_num` /
  `failed_num`; the CLI tools take `--strict` to stop at the first error.
  SPARQL `LOAD` is strict and atomic: a statement that does not parse fails
  it and nothing of it stays (`SILENT` only hides the error).
* **Named graphs** of N-Quads / TriG are kept in every backend. `LOAD <src>`
  keeps them too; `LOAD <src> INTO GRAPH <g>` loads the document's default
  graph into `<g>` and changes no other graph (SPARQL 1.1 Update).
* **Blank nodes** are scoped per loaded document: two loads never merge their
  `_:b`s. In an N-Quads / TriG document a blank node that names a graph
  (`_:g`) co-refers with the same label as a subject or object, unless the
  label was used as a subject or object before it named a graph: such a
  graph is kept apart (a warning names it). In a document with millions of
  blank labels, a graph whose label was not used before is also kept apart
  about 0.5 % of the time — a false positive of the bounded-memory check
  (about 4 bytes per label), warned about the same way.
* **Terms are validated by one validator** for every reader and SPARQL: a
  language tag is `[a-zA-Z]+(-[a-zA-Z0-9]+)*`. Where a format writes the tag
  as an attribute or string (RDF/XML `xml:lang`, JSON-LD `@language`, RDFa
  `lang`), surrounding whitespace is dropped, `_` reads as `-` (`zh_CN` is
  stored as `zh-CN`) and an empty value means no tag; any other malformed
  tag fails the document, and in N-Triples, N-Quads, Turtle and SPARQL it is
  a syntax error. A blank-node label outside the grammar (a JSON-LD
  `"_:a b"`) is stored as `h<16 hex digits>-<label>` (a stable hash, so one
  label stays one node).
* **Relative IRIs** resolve per RFC 3986 in every RDF format (Turtle / TriG
  `@base`, RDF/XML `xml:base`, JSON-LD `@base`, RDFa `<base>`): with base
  `http://a/b/c/d;p?q`, `../g` is `http://a/b/g`, `//g` is `http://g`.
  RDF/XML keeps a literal's whitespace exactly, and `rdf:parseType="Literal"`
  gives an `rdf:XMLLiteral` of the content as exclusive canonical XML. A
  database built by an earlier version from RDF/XML, JSON-LD or RDFa with
  such relative references keeps the old, wrong IRIs until it is rebuilt.
  (SPARQL's own `BASE` still concatenates — a known gap.)
* **Turtle / TriG details**: numbers follow the grammar (`1.e5` is an
  `xsd:double`, `1e` or `1.5.5` fail their statement), a prefix may hold
  inner dots (`@prefix ex.v2: <…>`), and invalid UTF-8 in a comment is an
  error of the comment's line only. N-Triples / N-Quads: a trailing `.` ends
  the statement, not a blank-node label (`_:o.` is `_:o`).
* **Token limit**: one token of a streaming reader (a string, IRI, name,
  number or language tag, or one N-Triples / N-Quads line) may be at most
  64 MiB (`gserver` and `gbuild` take `[upload] rdf_max_token_size`, `0` =
  no limit); a longer one fails its statement, which a tolerant load skips
  and logs.
* **Upstream's Turtle leniency**: its extra local-name characters, an IRI
  ending at a space, and raw line breaks inside `"…"` strings (which W3C
  Turtle allows only in `"""…"""`). Recovery stays per line: a statement
  whose string ran across a line break and then failed is reported on the
  string's line and read again from that line break (a string may run on for
  at most 256 KiB).

### SPARQL semantics and known deviations

The evaluator follows SPARQL 1.1 where earlier versions were lenient; the
W3C-style subset in `tests/w3c/` pins each rule (see
[Upstream `/api` compatibility and conformance](#upstream-api-compatibility-and-conformance)).

* **Property paths** follow §18.4: `/` and `|` keep duplicates (a node
  reached by two routes is two solutions), `*`, `+`, `?` and negated sets
  match distinct nodes, negated property sets with inverse members
  (`!^p`, `!(p|^q)`) walk the right direction, zero-length paths match
  constants absent from the data (`ASK { <u> :p* <u> }` is true), solutions
  come in a deterministic order (so `LIMIT` / `OFFSET` pages tile), and a
  path whose end point the rest of the group (or the left side of an
  `OPTIONAL`) binds is evaluated from that end. A query whose `/` or `|`
  paths would hand out more than 4,194,304 repeated solutions (copies of a
  pair reached by several routes) fails with a query error; `DISTINCT`,
  `ASK`, `EXISTS`, a `LIMIT` or a closure (`*`, `+`) avoid it (the budget is
  `Evaluator::with_path_max_duplicates`; the servers use the default, there
  is no configuration key).
* **Joins and scoping.** `GRAPH ?g { P }` keeps a solution only when P
  leaves `?g` unbound or binds it to that graph; `EXISTS` / `NOT EXISTS`
  honour the current solution inside `VALUES` blocks and sub-SELECTs; SELECT
  expressions are evaluated once each, left to right, seeing the aliases to
  their left; a sub-SELECT parses as a whole group in every position
  (`WHERE { SELECT … }`, `OPTIONAL { SELECT … }`, …).
* **`SERVICE`**: a failed non-`SILENT` `SERVICE` (a variable endpoint, an
  inner pattern the request cannot carry, a connection, HTTP or JSON error)
  fails the query with an error naming the endpoint, as SPARQL 1.1
  Federated Query §4 requires — it used to answer no solutions; add
  `SILENT` for the old reading.
* **Functions.** `REGEX` / `REPLACE` take the XPath flags `s`, `m`, `i`,
  `x`, `q` (another flag is an error) and XPath replacement strings (`$1`,
  `\$`, `\\`); the pattern syntax is the regex crate's (no
  back-references, no class subtraction). `BNODE()`, `UUID()`, `STRUUID()`
  and `RAND()` give fresh values on every call, distinct across processes
  (`BNODE(str)`: one fresh node per string within a solution), and a
  labelled blank node in a `CONSTRUCT` / `INSERT` template is a fresh node
  per solution; `NOW()` is one instant per
  query (whole seconds, UTC); `SECONDS` returns an exact `xsd:decimal`.
  `sameTerm` is the store's term identity (`"v"` and `"v"^^xsd:string` are
  two terms); `IRI()` / `URI()` / `STRLANG()` / `STRDT()` reject text that
  is not an IRI or a language tag. The string and hash functions (`STRLEN`,
  `SUBSTR`, `UCASE`, `CONTAINS`, `STRSTARTS`, `CONCAT`, `REGEX`, `REPLACE`,
  `STRLANG`, `MD5`, `SHA*`, …) reject non-string arguments as §17.4.3
  requires (`STRLEN(1)` is an error: write `STRLEN(STR(?v))`); still
  lenient: `ENCODE_FOR_URI`, `LANGMATCHES`, `STRDT`'s lexical form,
  `CONTAINALL` / `CONTAINANY` / `SIMILARITY` and numeric `SUBSTR`
  positions. `BOUND` of a variable the pattern never binds is false.
* **Aggregates.** A non-grouped variable in an aggregate query's projection
  is a query error (§11.4; `HAVING` and `ORDER BY` still see the group's
  first solution); `MIN` / `MAX` follow the `ORDER BY` order and, like
  `SAMPLE`, return the stored term; `AVG` divides by the number of numeric
  values it summed; `DISTINCT` inside an aggregate compares RDF terms (so
  `"01"^^xsd:integer` and `"1"^^xsd:integer` are two values), in linear
  time.
* **Updates.** One existence rule for graph management (§3.1.5, §3.2): a
  named graph exists while it holds a triple, so `CREATE GRAPH` of an
  existing graph and `DROP` / `CLEAR GRAPH` of a missing one fail unless
  `SILENT`. Blank nodes are refused in `DELETE DATA`, `DELETE WHERE` and
  DELETE templates; delete one stored blank node's triples with
  `DELETE { <a> <p> ?o } WHERE { <a> <p> ?o FILTER(sameTerm(?o, _:x)) }`
  (`FILTER isBlank` deletes the triples of every matching blank node).
  `INSERT DATA`
  and INSERT templates refuse the numeric literals the tolerant loader
  refuses (`"x"^^xsd:integer`, an `xsd:integer` beyond 64 bits, `NaN`), so
  what an update stores exports and builds back (REST `/batchInsert` and
  `Database::insert_triple` do not check this yet).
* **Grammar.** `PREFIX foaf:knows <…>` (a prefix with a local part), two
  triple patterns without a `.` between them, and repeated or misplaced
  `ORDER BY` / `LIMIT` / `OFFSET` are syntax errors.
* **Output.** N-Triples output (CONSTRUCT / DESCRIBE answers, exports, TSV)
  escapes every term, so a stored term can never end its line and forge a
  statement; the XML results format stays well-formed XML 1.0.
* **Known gaps.** `DESCRIBE` describes a resource by the triples it is the
  subject of — not a Concise Bounded Description: blank-node objects are
  not followed, incoming triples and named graphs are not described. A
  request's syntax tree is capped at 1,024 links (`MAX_TREE_HEIGHT`),
  counting every `UNION` branch, `||` / `&&` operand and `/` step of a
  chain, so a generated query with more than about a thousand chained
  alternatives is refused as "nested too deeply". `FROM` / `FROM NAMED` are
  skipped, and SPARQL's `BASE` resolution is concatenation, not RFC 3986.

### Upstream `/api` compatibility and conformance

The multi-DB server speaks upstream gStore 1.4's `/api` protocol (`GET|POST
/api` or `POST /` with `operation=…`, per-request `username`/`password`,
`encryption=1`, the `StatusCode` JSON envelope): the database, user, privilege,
transaction, log and task operations, `reasonManage` and the PFN operations
(`funquery`, `funcudb`, `funreview`) with upstream's parameters, fields, codes
and messages, so upstream's Python SDK (`GstoreConnector.py`) works against
it unchanged (`scripts/conformance.sh`); upstream's Java, C++, Node.js, PHP
and Go SDKs speak the same protocol but are not tested. Upstream's
`/shutdown` and `/restart` routes are served too. Its graph
functions answer as upstream's (see below), and `loadcsr` / `load` with
`csr=1` build the adjacency they run on ahead of the first query. Known
differences from upstream: a database is **single-writer** — one transaction
may be open on it at a time (a second `begin` answers `1008`), and while it is
open other writes to it (`query` updates, `batchInsert`, …) answer `1004`
until it is committed or rolled back, where upstream runs concurrent
transactions; zip packages (`build`/`batchInsert`/`batchRemove`/`restore` from
a `.zip`, `compress=true` for `backup`/`export`) need the `zip` feature —
without it they answer `1003` naming the feature — and are unpacked only within
`conf.ini`'s `unzip_max_entries` / `unzip_max_size`, refusing unsafe entry
names; `export` takes an optional `format` (`nt` / `nq`) and, without it,
writes N-Quads (`.nq`) for a database with named graphs where upstream's
N-Triples would drop them; a user holding the restore privilege on a database
restores only that database's own backups; and the export directory is not
public. `/file/download` needs credentials (parameters, HTTP Basic, or a
session token) and serves a file only to root / admin or to the user its
owner record allows — an export to holders of the `export` privilege on its
database, a query result to holders of `query`, an upload to its uploader;
`[upload] anonymous_download=on` restores upstream's public directory (an
upstream-compat switch, see [`docs/DEPLOY.md`](docs/DEPLOY.md#downloads-and-exports)).
Export and query-result file names carry 128 random bits, exports and query
results are deleted after `file_retention_hours` (a week by default), and
spilled asynchronous results a previous run left under `<root>/tasks` are
removed by the same lazy sweep. Further differences, all gstore-rust
extensions or hardening:

- **Credentials.** Root's default password `123456` is refused (see [HTTP
  behaviour](#http-behaviour-and-secure-defaults)); failed logins are
  locked out per account and client address on every password path, not
  just per address (`/api` answers a refused attempt `1101`); and a request
  without a `password` may present a `POST /login` session token instead
  (`Authorization: Bearer`, `X-Session-Token`, `session=` / `token=`; also
  for `/file/upload` and, with a `username`, `/file/download`) — as good as
  the password until it expires (24 h), the user logs out or the password
  changes. A request carrying a password is checked exactly as upstream's.
- **Bodies.** A body labelled `application/sparql-query` or
  `application/sparql-update` is never read as a form, so text inside the
  statement (`&sparql=…&db_name=other`) cannot replace the statement or the
  database; JSON, form and multipart bodies are read as before, and a body
  with another or no `Content-Type` keeps upstream's sniffing. A JSON body
  holds at most 65,536 values, a form, query string or multipart body at
  most 65,536 parameters (`1003`). Before authentication a body may be
  64 MiB: an upload larger than that needs `username` / `password` in the
  URL query string (see [Configuration](#configuration-confini)).
- **Transactions.** An open transaction idle for `txn_idle_timeout` (600 s)
  is rolled back; any superuser (root or an `admin` user) may end any
  transaction, and `rollback` needs no update privilege; a stale
  `<begin>_<tid>` id never ends a newer transaction with the same number;
  `show` and `monitor` report the open transaction as a `transaction`
  object; a failed commit answers `1005` saying whether it was rolled back
  or applied but not durable. Details in
  [`docs/DEPLOY.md`](docs/DEPLOY.md#transactions).
- **Operations.** `show` lists every database the registry finds (also
  names shorter than three bytes a REST `/build` or `gbuild` created;
  other `/api` operations on such a name still answer `1003`); `monitor` /
  `schema` of an unloaded database are answered from cached statistics
  while its files are unchanged; `ipmanage` keeps short IPv6 entries such
  as `::1` in the list files (upstream skips lines shorter than 7
  characters); `rename` is durable and undone when the registry or the
  grants cannot be saved; failed `query` statements are recorded in the
  query log with their real `statusCode` / `statusMsg`; and the
  transaction log rotates at 32 MiB.
- **Status codes.** HTTP stays 200 with upstream's `StatusCode`s: a
  statement cancelled, timed out (`query_timeout`) or too large
  (`max_result_bytes`), and a store read error, answer `1005 "Query fail:
  …"`; a write applied in memory but not saved answers `1005` with
  `"applied in memory but not durable: …"`.

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
  scripts and Python SDK against it unmodified. The server binds a port the
  kernel picks (`port=0`; the script writes the port from the server's pid
  file into `conf/conf.ini` for upstream's scripts), with exactly two
  upstream-compat switches on (`allow_default_root_password=on`,
  `anonymous_download=on`, which it checks gserver reported); a suite that
  cannot run — the Python one without python3's `requests` — fails the run
  unless `--allow-skip` is given.

Further suites keep the executors and storage backends in step:

- `cargo test --test w3c_subset` runs the hand-authored SPARQL 1.1
  conformance subset on every storage layout and both executors
  (`--features rocksdb` adds RocksDB), negative cases included
  (`expected.error`: `syntax`, or `failure` for an update that must fail and
  leave the dataset unchanged, also after a reopen); the case format is in
  [`tests/w3c/README.md`](tests/w3c/README.md). SPARQL behaviour changes
  should come with a case there, citing the spec section.
- `tests/executor_differential.rs` checks that the materializing and
  streaming executors answer every w3c case, LUBM and a probe battery alike
  on every layout; `tests/storage_backend_conformance.rs` is the one
  storage-contract suite over every backend. Rerun both after merges.
- `tests/fsync_audit.rs` checks the fsyncs of every commit point through
  `gstore::sync_probe`; `tests/adversarial_coverage.rs` maps the review's
  adversarial categories to their regression tests.
- New integration tests use `tests/common` (`mod common;`) for temporary
  paths (`tmp::TempDir`, `tmp::thread_dir`), HTTP requests (`http::request`,
  with an overall deadline per request) and in-process servers
  (`server::Running`, stopped and joined on drop): no fixed temporary names,
  no client read without a deadline, no server left running; bounds on
  memory or work are asserted by bytes (`tests/common/alloc_counter.rs`) or
  counters rather than wall-clock time.

### HTTP behaviour and secure defaults

Both HTTP servers — the multi-database API server (REST endpoints and
`/api`) and the single-database `gserver DIR` — keep upstream's wire
format, but default to the safe behaviour where upstream's is unsafe on a
network. Where such a default changes what upstream clients see, an
upstream-compat switch in `conf.ini` restores upstream's behaviour and makes
`gserver` print an `upstream-compat: <key>=<value>` line at startup (the
stricter framing, status codes and limits below have none); the table of
switches, their risks and the two that `scripts/conformance.sh` turns on is
in [`docs/DEPLOY.md`](docs/DEPLOY.md#secure-defaults-and-upstream-compat-switches).

* **Root's password.** The API server refuses to start while root has the
  default password `123456`, on every address: give it one with
  `--password-file FILE` (or `$GSTORE_ROOT_PASSWORD_FILE`, or `conf.ini`'s
  `root_password`) when the user store is created. The deb / rpm packages
  write a random one to `/etc/gstore/root_password` on a fresh install.
  `[server] allow_default_root_password=on` lets the server start on a
  loopback address only, accepting `123456` from local clients only.
* **Parameters follow the `Content-Type`** (REST endpoints and the
  single-database server): parameters come from the URL query string and
  from a form body (`application/x-www-form-urlencoded`, or an unlabelled
  body shaped like a form) only. An `application/sparql-query` or
  `application/sparql-update` body, and any other raw body, is the
  statement or document verbatim, so text inside it (`&update=…`,
  `&db_name=…`) never runs another statement or switches the database; a
  form body without `query=` / `update=` is a missing statement (`400`).
  The dataset parameters `default-graph-uri` / `named-graph-uri` (`/sparql`)
  and `using-graph-uri` / `using-named-graph-uri` (`/update`) are refused
  with `400` instead of being ignored (empty values are ignored; `FROM` /
  `FROM NAMED` in the query text are still skipped, a known gap). `/api`
  keeps upstream's parameter rules, except that a raw SPARQL body is never
  read as a form.
* **No updates by GET, no cross-site writes.** `GET /sparql` never runs an
  UPDATE (`400`), and `POST /update` with `text/plain` or
  `multipart/form-data` answers `415` (an UPDATE POSTed to `/sparql` still
  runs); compat switch `allow_get_update`. A state-changing request whose
  `Origin` is neither the server itself nor listed in `cors_allow_origin`
  is refused (`403`; `/api`: `1101`), and a request arriving over loopback
  must name a local host in `Host` (DNS-rebinding guard; `allowed_hosts`
  lists more). No response carries `Access-Control-Allow-Origin` unless
  `cors_allow_origin` lists the origin (`*` restores upstream's `ACAO: *`).
  Clients that send no `Origin` (scripts, the SDKs, `gconsole`) are
  unaffected; the bundled web console works when opened at the server's own
  address. Behind a reverse proxy, see
  [`docs/DEPLOY.md`](docs/DEPLOY.md#browser-facing-defaults-and-reverse-proxies):
  a loopback peer forwarding `Forwarded` / `X-Forwarded-For` / `X-Real-IP`
  is not a local client unless `trust_proxy_headers=on`.
* **Sessions.** REST session tokens are read from the request head or URL
  only (`Authorization: Bearer`, `X-Session-Token`, `session=` / `token=` URL
  parameters), no longer from a body parameter; `/login` reads `username` /
  `password` parameters (not a `Basic` header). Failed logins are counted
  per account and client address on every password path and locked out
  with a back-off (REST and single-database `429` with `Retry-After`; see
  [`docs/DEPLOY.md`](docs/DEPLOY.md#login-lockout)).
* **Bodies.** Before authentication a request body may be 64 MiB
  (`preauth_max_body_size`); only a request authenticated from its head —
  HTTP Basic or a session token on the REST payload routes,
  `username` / `password` in the URL query string on `/file/upload` and
  `/api` — may carry up to `upload_max_body_size` (256 MiB). A larger
  `Content-Length` is answered `413` before the body is read. All bodies in
  memory share `request_body_budget` (512 MiB; beyond it `503`). A body must
  arrive at `min_body_rate` (64 KiB/s) on average after a 30 s grace, or the
  request is answered `400` and the connection closed; one address may hold
  `max_conns_per_ip` (64) connections.
* **Strict framing.** Request framing follows RFC 9112 strictly:
  `Content-Length` together with `Transfer-Encoding`, a signed, non-decimal
  or conflicting `Content-Length`, any `Transfer-Encoding` but `chunked`
  (`identity` included), header lines without a colon or with whitespace
  before it, obs-fold, and control characters (other than tab) in a request
  line, header or chunk line are answered `400` and the connection closed.
  No known upstream client sends any of them.
* **Keep-alive under load.** While connections wait for a worker, a
  response says `Connection: close` and the connection closes after it; an
  idle kept-alive connection yields its worker to a waiting one after a
  1 ms look for a request already on its way.
* **Status codes** (REST and the single-database server, the same on every
  endpoint): `400` a syntax or evaluation error; `409` a lock another
  process holds (a database directory or a paged file) or a transaction
  conflict; `500` a storage or persistence failure (also a store read error
  during a query, an I/O error on a lock file, a write applied in memory but
  not durable); `503` a statement cancelled (`/cancel`, shutdown), timed out
  (`query_timeout`), too large (`max_result_bytes`) or turned away by the
  body budget. `/api` keeps HTTP 200 with upstream's `StatusCode`s.
* **REST `/export`** exports the whole database: N-Triples, or N-Quads when
  it has a non-empty named graph (`format=nt|nq` picks one; `Content-Type`
  and file name follow). It is a task listed by `/tasklist` and stopped by
  `/cancel` (`503`), written to a scratch file under the database's shared
  guard and streamed once the guard is released.
* **Downloads** (`/file/download`) need credentials and an owner check
  (compat switch `anonymous_download`), are streamed in 64 KiB chunks and
  never compressed: with the `gzip` feature only buffered responses (the
  JSON envelopes, query results) are.
* **HTTPS** needs a build with the `tls` feature
  (`--tls-cert FILE --tls-key FILE`); a `gserver` built without it refuses
  those options with an argument error (exit status `2`) instead of serving
  plaintext HTTP.

### Upgrading from an earlier version

Most changes keep data and wire formats; what an upgraded server, client or
script may notice is listed in [`docs/DEPLOY.md`'s upgrade
notes](docs/DEPLOY.md#upgrade-notes) and, in full, in
[`CHANGELOG.md`](CHANGELOG.md). In short: the API server refuses root's
default password `123456` on every address; browser front ends served from
another origin (e.g. upstream's Workbench) need their origin in
`cors_allow_origin`; downloads need credentials, and exports and
query results are deleted after a week; uploads whose credentials travel in
the body stop at 64 MiB and every upload at 256 MiB; idle transactions are
rolled back after 10 minutes; `conf.ini` inline comments follow a narrower
rule (a note glued to its marker is now part of the value);
`DELETE DATA { <a> <p> _:x }` is a parse error (use the `sameTerm` form
above); `gbuild` may exit `3`; `gnode cluster` needs `--state-dir` or
`--ephemeral`; and `gnlqa` reads no `gnlqa.conf` from the working
directory.

Data an older version loaded with a malformed language tag or blank-node
label (`"x"@zh_CN`, a JSON-LD `_:a b`) is stored as it was, but the readers
now normalise the tag (`zh-CN`) and hash the label, so removing it by
re-reading its source — `gsub`, `/api` `batchRemove`, REST `/batchRemove` —
does not match it: the command succeeds, counting those statements as read
but not removed, and the legacy terms stay. Remove them by their stored
form, e.g.
`DELETE { ?s ?p ?o } WHERE { ?s ?p ?o FILTER(lang(?o) = "zh_CN") }`
(`LANG()` returns the stored tag), or export, normalise and rebuild.

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
| `gfun_max_output_bytes` | `67108864` | most bytes of JSON the graph-function and `PFN.name` calls of one query may produce together — every call, each `GROUP BY` group and `GRAPH` block included (an embedded `Database` allows 256 MiB) |

In code: `GStoreConfig::query_settings()`, `ApiServer::with_query_settings`,
`Server::with_query_settings`, `Database::apply_query_settings`.

Other gstore-rust keys (the secure defaults and their upstream-compat
switches are tabulated in
[`docs/DEPLOY.md`](docs/DEPLOY.md#secure-defaults-and-upstream-compat-switches)):

* `[server]`: `listen_addr`, `keep_alive_timeout`, `keep_alive_max`,
  `queue_size`, `cors`; `min_body_rate` (65536 bytes/s a request body must
  average after a 30 s grace; `0` = the per-read timeout alone) and
  `max_conns_per_ip` (64 open connections per client address; `0` = no
  limit); `query_timeout` (3600 s of evaluation per statement, fractions
  allowed; `0` = off) and `max_result_bytes` (1 GiB per answer; `0` = off),
  answered REST / single-database `503`, `/api` `1005`;
  `login_lockout_failures` (4: the fifth failed login locks the account for
  that client address) and `login_lockout_max_seconds` (900, the longest
  back-off); and the upstream-compat switches, all off or empty by default:
  `cors_allow_origin`, `allowed_hosts`, `allow_get_update`,
  `trust_proxy_headers`, `allow_default_root_password` and
  `upstream_login_lockout`.
* `[system]`: `txn_idle_timeout` (600 s: an open transaction idle that long
  is rolled back; `0` = never, upstream's behaviour, an upstream-compat
  value).
* `[upload]`: `upload_max_body_size` is upstream's key, but its default and
  shipped value is 256 MiB (upstream: 1 GiB); `preauth_max_body_size`
  (64 MiB, the body cap before authentication) and `request_body_budget`
  (512 MiB of request bodies in memory per server, never below
  `upload_max_body_size`); `unzip_max_entries` (default 10000) and
  `unzip_max_size` (default 10 GiB), the limits a zip package must stay
  within; `rdf_max_token_size` (64 MiB, the longest token the streaming RDF
  readers hold and the longest N-Triples / N-Quads line; `0` = no limit) and
  `sparql_max_size` (16 MiB, the longest SPARQL query or update `gserver`
  and `gbuild` accept, refused before parsing; `0` = no limit; `gquery`,
  `gconsole --local`, library callers and `gnode` have no limit);
  `anonymous_download` (off: downloads need credentials, an upstream-compat
  switch), `download_max_concurrent` (8 streaming downloads at once),
  `file_retention_hours` (168: uploads, exports and query results the
  server recorded are deleted after a week; `0` = keep) and
  `upload_user_quota` (bytes of retained uploads per user; `0` = none).
* `[log]`: `log_level`, `log_file`, `log_days`, `log_stderr` (below).

A large upload must carry its credentials in its head to get past the
64 MiB pre-authentication cap: in the URL query string for `/file/upload`
and `/api` (an `Authorization` header does not count there), as HTTP Basic
or a session token for the REST payload routes; the upstream SDKs, which
send credentials in a JSON or form body, reach 64 MiB only.

`refreshconf` re-reads the file: the `[query]` and `[pfn]` settings, the
log configuration (as upstream re-runs log4cplus; a log file that could not
be opened is tried again), the IP lists, the failed-login lockout keys (the
counts are kept), the database and user limits and the upload, backup and
query / access-log settings take effect at once; the address, the worker
pool, `check_point`, `db_home`, the
HTTP body caps (`upload_max_body_size`, `preauth_max_body_size`) and
`request_body_budget` need a restart (the upload operation's own size check
follows the new value). A new `txn_idle_timeout` applies at once to `begin`
and the request path, but the background pass that reaps idle transactions
keeps its interval until a restart. A relative `querylog_path` /
`accesslog_path` is taken relative to `db_home`. In code, the
single-database server takes the statement limits with
`Server::with_query_timeout` / `Server::with_max_result_bytes`.

`conf.ini` inline comments: a `#` or `;` starts a comment only when it
stands as a word of its own after the value (`port=9000  # http`); a marker
glued to more text stays in the value, as upstream reads it, nothing inside
double quotes is a comment, `root_password` is never cut, and each cut is a
startup warning. Command-line flags win over the file, and the `GSTORE_*`
storage variables below win over their keys whenever a database is opened
(details: [`docs/DEPLOY.md`](docs/DEPLOY.md#configuration-and-logs)).

A `[pfn]` section (gstore-rust only) sets the sandbox limits of
[PFN user functions](#pfn-user-graph-functions), applied the same way (to
every served database, and re-applied by `refreshconf` to the loaded ones and
to the function registry): `pfn_fuel_per_call` (instructions per call,
default 10^9, `0` = unlimited), `pfn_fuel_per_query` (the fuel all the PFN
calls of one request may use together — every request starts from zero and
the operations of one UPDATE share one budget; default 10000000000, `0` =
unlimited; fuel counts instructions, host bytes and each call's
instantiation, one unit per 8 bytes of its initial memory and data
section), `pfn_max_memory_pages` (default 1024 =
64 MiB), `pfn_max_module_bytes` (default 1 MiB) and `pfn_max_functions` (per
user, default 100). Upstream's `[system] pfn_base_path` (default here:
`<db_home>/pfn`) is where the functions are stored; it is read at startup.

A `[storage]` section (gstore-rust only) sets how databases are opened and
built ([Storage layouts](#storage-layouts-and-query-modes)). `gserver`
installs it as the process's defaults at startup and `refreshconf` installs
it again: databases loaded afterwards use it, a loaded database keeps the
query mode it was opened in.

| key | default | meaning |
|-----|---------|---------|
| `disk_query_mode` | `memory` | how an on-disk database answers queries: `memory` (Materialize) or `stream` |
| `disk_cache_mb` | `16` | MiB of page cache per on-disk database |
| `dict_cache_mb` | `80` | MiB a Stream-mode database's dictionary may cache (four fifths id → term, one fifth term → id) |
| `vstree_on_open` | `off` | build the VS-tree when a database is opened without a fresh one (RocksDB; a Stream-mode database whose `vstree.kv` is stale) instead of querying without it until `rebuild_index` |
| `default_layout` | unset (`snapshot`) | the layout `gbuild`, REST `/build` and `/api build` create when the request names none: `snapshot`, `disk` or `rocksdb` |
| `rocksdb_block_cache_mb` | `256` | MiB of the block cache a RocksDB database shares across its column families; `0` = RocksDB's own default (32 MiB per column family) |
| `rocksdb_bloom` | `10` | bloom-filter bits per key on RocksDB's point-lookup column families; `on` (= 10) or `off` |
| `rocksdb_max_open_files` | `4096` | the most files a RocksDB database keeps open; `0` = unlimited (RocksDB's default) |

`[system]` gains `id_reclaim` (`on`, the default, or `off`: ids are then only
freed by `gcompact --reclaim`; read by `gserver` and `gbuild`, the other
command-line tools follow `GSTORE_ID_RECLAIM` only) and the bulk-build keys
`gbuild` reads from `./conf/conf.ini`: `build_memory_mb` (1024, the sort
budget; the dictionary under construction comes on top), `build_threads` (`0`
= every core; `build_multi_thread=off` means one), `build_tmp_dir` (where
sorted runs spill; default inside the database directory) and
`build_fill_factor` (90, percent B+ tree node fill). Environment variables
win over the file: `GSTORE_DISK_QUERY_MODE=stream|memory`,
`GSTORE_DISK_CACHE_MB`, `GSTORE_VSTREE_ON_OPEN`, `GSTORE_ID_RECLAIM=on|off`,
`GSTORE_ROCKS_BLOCK_CACHE_MB`; `GSTORE_VS_POLICY=auto|tree|off` (no file
key) selects how queries use the VS-tree (`auto`, the default, computes the
tree's candidate sets from the store on disk databases).

[Reasoning rules](#reasoning-reasonmanage) have no `conf.ini` keys: their
limits are fixed (1000 rules per database, 1 MiB per rule file, 64
conditions per rule, 256 patterns and 64 filters per condition, rule names of
at most 128 characters and 240 bytes), and executing or disabling a rule is
an ordinary update (`check_point`, the `[query]` cache and the CSR behave as
for any other write).

**Logs.** `gserver` writes a diagnostic log (errors, warnings, lifecycle
events) configured like upstream's: `conf.ini`'s `log_mode` names a log4cplus
properties file (`conf/slog.properties.example`), with the gstore-rust keys
`log_level`, `log_file`, `log_days`, `log_stderr` and the `GSTORE_LOG`
variable on top; files roll over daily. Every SPARQL statement — `/api`
`query` and the REST `/sparql`, `/update`, `/tquery` — is recorded in the
JSON query log with the client IP, answer count and latency, and the access
logs carry the client IP (see [`docs/DEPLOY.md`](docs/DEPLOY.md#configuration-and-logs)).

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
eagerly, as the server's `loadcsr` and `load` with `csr=1` do — as a task
`/tasklist` lists and `/cancel` stops). A call is cancelled like any query
(`/cancel`, `operationTaskCancel`, shutdown), from inside its loops, and so
are the CSR build and the substring-index build of a `CONTAINS`-style
filter (a cancelled build caches nothing). The JSON of one query is bounded
as a whole: `[query] gfun_max_output_bytes` (64 MiB on a server) bounds
every graph-function and `PFN.name` call of the query together, each
`GROUP BY` group and `GRAPH` block included, and a query past it fails
(`<function>: more than N bytes of output in this query`). The iterative
functions stop at a documented work bound (`gfun::MAX_ITERATION_WORK`,
2^34 vertex / edge / community visits beyond the first 1024 sweeps, which
are free, so `maxIter <= 1024` always runs to the end) with an error naming
it — the typical cause is `tol < 0` / `increase < 0` with a huge `maxIter`.
Upstream parses `cycleBoolean`,
`simpleCycleBoolean`, `shortestPathLen` and `kHopReachable` inside `FILTER` but
never evaluates them there; here that is a clear error. `PFN.name({…})` calls a
user's own function (see [PFN](#pfn-user-graph-functions) below).

**Upstream quirks, reproduced** for gStore 1.4 compatibility (wrong by
design; `docs/GAP_PLAN.md` §3.1; details in the rustdoc of
`src/analytics/gfun/{paths,metrics}.rs` and the `kplex` module):

- `kHopReachable(u, v, directed, k, preds)` searches one round only (at most
  10 forward pops from `u`, then one backward expansion of `v`): distant
  pairs answer `false` although reachable (on a 16-node chain, `n0 → n11`
  within 20 hops is `false`, `n0 → n10` `true`), and a `v → u` edge or a
  predicate with more in- than out-edges can make an unreachable pair
  `true`.
- `cycleBoolean` / `cyclePath` with `directed = false` walk the path back
  and call it a cycle (every connected pair "has a cycle", an acyclic chain
  answers `true`, a single edge gives `u → v → u`), every edge printed as
  forward.
- `triangleCounting` returns `-1` when the raw total is not divisible by 3
  (directed) / 6 (undirected), e.g. a self-loop or parallel edges next to a
  triangle.
- `maximumKplex` starts from a fake solution of `2k - 2` copies of CSR
  vertex 0 and returns it when no larger k-plex is found (`k = 2` on an
  edgeless predicate set gives `[<v0>,<v0>]`), maps ego-network ids through
  an array the pruning overwrote (repeats or non-members possible) and
  prints IRIs unquoted; `coreTruss` appends (shrunk) vertex 0 once per
  peeled vertex while it has edges, IRIs unquoted.
- `PPR` on graphs with fewer than 4 vertices returns the default slot
  `{"dst":"<vertex 0>","PPR":0}` for every source, an all-zero result still
  prints vertex 0, and `kth_ppr` indexes by PPR value (values are
  deterministic here; upstream seeds with the time).
- `labelProp` computes modularity with the vertex count as edge count and
  maps the result through the last iteration's labels (stale labels fall
  into community 0, a current label nobody carries prints `[]`); `louvain`
  keeps upstream's `s_tot` assignment (the last edge's weight, not a sum)
  and prints members as entity ids in a `{"count":C, "community":[…]}`
  shape that differs from upstream's documentation.
- `closenessCentrality` of an isolated or missing source prints
  `"result":-nan`; `clusterCoeff` with a missing constant source answers the
  global coefficient; `PR`'s entries use the key `"result"` (the docs say
  `"results"`); `shortestPathCount` and closeness sums wrap like C `int`;
  `JaccardSimilarity` and `degreeCorrelation` report missing sources as
  nothing.

The one deliberate deviation: a `,` between consecutive entries where
upstream writes none (invalid JSON upstream).

**The `rs:` namespace (breaking change).** The functions gstore-rust offered
before — bare names such as `SSSP(?a, ?b)`, usable in `FILTER`/`BIND` and
computed per row over a predicate-blind graph — now live in the IRI namespace
`<urn:gstore-rust:fn:>`, predeclared as the prefix `rs:` (a query's own
`PREFIX rs:` replaces it), with unchanged answers and usable anywhere —
except that `rs:LOUVAIN` is now deterministic (ties go to the smallest
community id), and cycles and entity-to-entity paths run on the database's
cached CSR:

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
carried by `backup` / `restore` (zipped backups and `gbackup` / `grestore`
too; `export` writes data only, as upstream's), kept in `<db>.db.bak` by
`drop` and removed by `drop` with `is_backup=false` — and by a rebuild in
place (REST `/build` over an existing database, `gbuild --force`, a bulk
rebuild), which replaces the database as a drop and a build would — under
the rule lock, renaming the rules aside before deleting them, so neither an
interrupted rule swap nor a crash during the removal can bring the old
rules back to the rebuilt database. Rule directories are written
all-or-nothing (staged, synced and swapped in). Incremental backups
(`Database::backup_incremental`) carry the rules and ledgers of their time,
which `restore_incremental` restores with the replayed triples; a database
without a directory (built in memory through the library) keeps them in
memory. The library offers the same lifecycle: `Database::reason_add`, `reason_list`,
`reason_compile`, `reason_execute`, `reason_disable`, `reason_show`,
`reason_delete`, `reason_check`.

Execution is an ordinary autocommit update (query cache, CSR, write-through,
durability and the query log behave as for a `query` update, and it can be
cancelled); execute and disable are refused (`1004`) while a transaction is
open on the database, and execute / disable / check need it loaded.

RDFS materialization (`Database::materialize_rdfs` / `try_materialize_rdfs`)
and textual rules (`Database::add_rule`, `RuleSet`; `run_rules` /
`try_run_rules`) are library APIs only — no server operation runs them.
Textual rules are not persisted (save, backup and open do not carry them),
and their text is validated with the crate's one term validator (non-ASCII
or malformed rule text is an error, not a panic). Every reasoner reads and
writes the default graph only, and RDFS materialization takes only IRIs as
properties: a literal or blank node in an `rdfs:subPropertyOf`, `domain` or
`range` position (as OWL writes `[ owl:inverseOf :q ]`) is never made a
predicate.

**Retraction is exact.** Each execution records which triples the rule owns
(`reason_rule_files/inferred/<rulename>.nt`): those it added, and those it
derived that another executed rule already owns. Disabling deletes exactly the
owned triples no other rule also owns. The record holds each term in a form
that reads back exactly — an IRI as N-Triples escapes it (`\u003E` for a
`>`, `\u0009` for a tab…), whatever a loader let into the database — and is
written *before* the triples are inserted, so a crash or a failed write never
leaves an inferred triple no rule owns; a record line that cannot be read
back fails the disable (nothing is retracted) instead of being skipped.
Disabling a rule of a snapshot database saves the database before its record
is emptied (whatever `check_point` says; upstream saves there too). So a fact asserted with an ordinary
predicate is never touched; a `<Rule:…>` triple that was already in the
database before the rule ran (asserted by a user) survives; a triple two rules
infer survives until both are disabled. A triple a user inserts *after* the
rule inferred it is the same RDF triple and is retracted with the rule. A
rule's conditions are evaluated once per execution and exactly the triples
that evaluation derived are inserted, so a random filter cannot leave a triple
unrecorded. Imported data is asserted data: `<Rule:…>` triples in a file
given to `build`, `batchInsert` or SPARQL `LOAD` (tolerant or strict) belong
to no rule, and a rule that derives one of them neither counts nor retracts
it. Inferred triples over blank nodes are recorded under the labels the
import pipeline stored (each load's own scope), so they retract exactly; a
later import of the same document mints new blank nodes, which no rule owns. A rule file without an ownership record was written by upstream
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

### gconsole

`gconsole` is upstream gStore 1.4's console: an interactive client of a
running `gserver`'s `/api` at `127.0.0.1:<port>`, the port read from
`./conf/conf.ini` like the server's (or `--port N`, `--host H`, `-c FILE`).
It logs in once (`POST /login`) and sends the session token, not the
password, with every `/api` request, so the password crosses the network
once per login and the server checks the user's privileges; when the
session ends (it expires after 24 h, or the user's password changed) the
console logs in again and repeats the request once, and it logs out when it
leaves. Against a server without `POST /login` (upstream's gserver) or
whose `/api` does not take sessions (an older gstore-rust) it falls back to
upstream's per-request password. `--tls [--ca-file FILE]` (feature `tls`)
talks to a gserver serving HTTPS, verifying its certificate against
`--ca-file`, else `$SSL_CERT_FILE`, else the system CA bundle (an unvouched
certificate is a connection failure and nothing is sent); a plain-HTTP
connection to a non-loopback host prints a warning that the password and
every statement cross the network in clear text.

```text
$ gserver -b --load-dir data --password-file pw.txt
                                      # let `create` read the data sets in ./data; root's
                                      # password from pw.txt (123456 is refused)
$ gconsole -u root
Enter password:                       # (not echoed; -p PW or $GSTORE_PASSWORD for scripts)
...
gStore[no database]> create lubm data/lubm/lubm.nt;
Build RDF database lubm successfully! use 1043 ms.

gStore[no database]> use lubm;
Current database switch to lubm successfully.

gStore[lubm]> SELECT ?x WHERE {
     ->   ?x <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ?t .
     -> } LIMIT 2;
...
Query ans num 2, use 3 ms.

gStore[lubm]> showdbs > dbs.txt;      # redirect a command's output (>> appends)
gStore[lubm]> quit;
```

* Statements end with `;` and may span lines; `#` starts a comment; several
  statements may share a line. SPARQL typed directly runs on the current
  database: queries (`SELECT`, `ASK`, `CONSTRUCT`, also `CONSTRUCT WHERE`,
  `DESCRIBE`) and every update form (`INSERT` / `DELETE`, `WITH …`, `LOAD …
  [INTO GRAPH …]`, `ADD` / `MOVE` / `COPY`, `CREATE` / `DROP` / `CLEAR
  GRAPH …`), also after a `PREFIX` / `BASE` prologue. As upstream, `drop
  <db>` drops a database, `create <db> [<file>]` builds one and `clear`
  clears the screen: SPARQL's `DROP` / `CLEAR` / `CREATE` are recognised in
  the forms those commands cannot have (`DROP GRAPH <g>`, `DROP SILENT ALL`,
  `CREATE GRAPH <g>`, …).
* Results print as upstream's: a table of N-Triples terms whose literals
  show their lexical form unescaped (a graph function's JSON next to other
  columns reads `"{"paths":[…]}"`; line breaks and tabs stay escaped), a
  lone graph-function answer as indented JSON, and a PFN call's
  `{"results": …}` answer as its value's compact JSON followed by `pfn
  query use <ms> ms.`, as upstream prints its `pfn_type` answers.
* Reasoning and functions, as upstream's console: `addreason <file>` sends
  the rule JSON in the file (the `reasonManage` `ruleinfo`) and prints the
  server's message; `listreason` prints `ruleid | rulename | description`;
  `compilereason` / `executereason` / `disablereason <rule>` print the
  statements the server answers, upstream's: the compiled `insert_sparql` and
  `delete_sparql`, the `insert_sparql` executed — and, for
  `disablereason`, upstream's nominal `delete where {?x <Rule:label> ?y.}`,
  which is **not** what ran for a rule with a ledger: such a rule retracts
  exactly the triples of its ledger (a `DELETE DATA`; see Reasoning), and
  the answer's `AnsNum` counts them; `showreason <rule>`
  prints the number of conditions and one `rulename | description |
  patterns | filters` row per pattern (upstream prints the subject twice and
  no predicate: `<s,o,s>`; here `<s,p,o>`), for every condition shape the
  server accepts (also `{"condition": {…}}`-wrapped ones). `addpfn` / `mdfpfn <file>`
  send upstream's `PFNInfo` JSON from the file — `funName`, `funDesc`,
  `funBody`, `funSubs` — unchanged; the body is WebAssembly text (upstream:
  C++; see [`docs/PFN.md`](docs/PFN.md)), which the server URL-decodes as
  upstream does, so write a literal `+` / `%` as `%2B` / `%25`. `showpfn`
  prints `name | desc | returnType | status | lastBuildTime` — upstream's
  headers — with the function's source file `src/<user>/<name
  lower-cased>.wat` (under the server's `pfn_base_path`) in the third
  column, where upstream shows `./pfn/<name>.cpp`. `delpfn` / `buildpfn <name>` delete / compile it.
* Commands (`help;` lists them, `help <command>;` shows one): `create <db>
  [<file|dir|url>]`, `use`, `show [<db>]`, `showdbs`, `pdb`, `unload`, `drop`,
  `backup [<path>]`, `restore <db> <path>`, `export [<path>]`, `batchinsert` /
  `batchremove <file>`, `sparql <file>`, `init <db>[,…]`, `refreshconf`; users
  and privileges: `addusr`, `delusr`, `setpswd`, `setpriv <user> <db>`,
  `clearpriv`, `pusr`, `showusrs`, `flushpriv`; reasoning rules (`addreason`,
  `listreason`, `compilereason`, `executereason`, `disablereason`,
  `showreason`, `deletereason`) and custom functions (`showpfn`, `addpfn`,
  `mdfpfn`, `delpfn`, `buildpfn`; see [Reasoning](#reasoning-reasonmanage)
  and [PFN](#pfn-user-graph-functions) above); `version`, `pwd`,
  `clear`, `quit` / `exit` (or Ctrl-D), and `history [<n>]`.
* Ctrl-C abandons the statement being typed (also one typed just before
  the prompt appeared), or the request in flight: the connection is dropped
  and, for SPARQL, the server's task is cancelled (`operationTaskCancel`);
  the console stays open. Every server field the console prints is printed
  with control characters escaped, so server text cannot drive the
  terminal.
* Scripts: `gconsole -u root -p PW -e "use lubm; select * where {?s ?p ?o} limit 5"`,
  or commands on stdin (`gconsole -u root -p PW < script.txt`) run without
  banner or prompts, and the exit code is 1 if any command failed. A command
  that prompts (`addusr`, `setpriv`, `setpswd`, …) reads its answers from the
  following lines.
* History: statements typed at a terminal are kept in `~/.gstore_history`
  (`$GSTORE_HISTFILE`). Build with `--features readline` for line editing,
  history recall (arrow keys) and tab completion of command and database
  names; the default build reads plain lines.
* Differences from upstream: privileges are checked by the server (a
  non-root user's `pusr <db>` needs `showuser`, which only root may call);
  `sparql <file>` runs each `;`-separated query of the file on its own (a
  run of update operations, `op ; op`, is still one request); results are
  fetched as JSON (an unbound cell is shown empty); a local file given to
  `create` / `batchinsert` is sent as an absolute path when the server runs
  on the same machine — the server still only reads files under its
  `--load-dir` directories and its upload / export directories; `setpriv`
  accepts `0` alone for "no privilege", refuses an answer that is not a
  list of privilege numbers (sending nothing), and grants what is missing
  before revoking what is left over (upstream revokes all, then grants),
  taking the grant back when the revoke fails.
* `gconsole --local <db>` (gstore-rust extension; also the older `gconsole
  <db>`) is the embedded REPL that opens `<db>.db` directly, without a
  server. It ends statements by the remote console's rule (a `;` outside
  `{…}`, strings, `<IRI>`s and comments; a script's last statement may lack
  it) and keeps the updates it acknowledged: a snapshot-layout or new
  in-memory database with unsaved updates is saved when the console leaves
  it (`quit`, end of input, `load <other>`; it prints `saved to <dir>`, and
  when the save fails `load` keeps the current database and an interactive
  `quit` stays — a second `quit` leaves without saving). On-disk and
  RocksDB databases write each update through. Ctrl-C discards only the
  statement being typed; a console killed by `SIGTERM`, `SIGHUP` or
  `SIGKILL` does not save.

### Library API

```rust
use gstore::Database;

let mut db = Database::build_from_files("demo", &["data.nt"])?;
db.query("INSERT DATA { <a> <p> <b> }")?;
let rs = db.select("SELECT ?o WHERE { <a> <p> ?o }")?;
println!("{}", rs.to_table_string());
db.save("demo.db")?;                 // persist
let db = Database::load("demo.db")?; // reload (any layout)

// an on-disk database, built in bulk, opened in Stream mode; `&Database`
// reads may run on several threads at once (`Database` is Send + Sync)
use gstore::OpenOptions;
Database::build_disk("big.db", &["data.nt"])?;
let big = Database::open_with("big.db", &OpenOptions::stream())?;
let r = big.query_read("SELECT ?o WHERE { <a> <p> ?o }", None, None)?;
```

* **Errors.** `GStoreError::kind()` classifies an error
  (`gstore::error::ErrorKind`, `#[non_exhaustive]`: `Parse`, `Query`,
  `Locked`, `Conflict`, `Corrupt`, `Storage`, `Cancelled`, `Timeout`,
  `Other`); the servers map it to one HTTP status per kind. A lock another
  process holds is the variant `GStoreError::Locked` (code that matches
  `GStoreError` exhaustively needs an arm for it); a database whose on-disk
  store failed reports `GStoreError::StorageFailed` for every write until
  it is reopened. `Database::commit_checked` returns `Result<(),
  CommitFailure>` (`NotCommitted`, `RolledBack`, `NotDurable`,
  `CommittedWithError`); `commit` reports each as an error.
* **Inference with errors.** `try_materialize_rdfs()` / `try_run_rules()`
  report read errors where `materialize_rdfs()` / `run_rules()` count
  silently.
* **Builds.** `build_from_ntriples_batched` streams the parse but buffers
  the encoded ids (12 bytes a triple) and indexes them once: its bounded
  memory is the parse's, not a bounded id buffer.
* **Evaluators.** `Database::eval_settings()` and `Evaluator::with_settings`
  (`EvalSettings`, `EvalProviders`, `StringIndexCell`) are for embedders
  that build their own evaluators; `Evaluator::stats()` returns the work
  counters (`EvalStats`), among them `bgp_bindings`, `path_steps` (one per
  adjacency lookup of a path step) and `join_probe_rows` (rows the hashed
  operands of `OPTIONAL`, `MINUS` and joins looked at).
* **Redo log and incremental backup** (`Database::enable_redo_log*`,
  `enable_update_log*`, `replay_*`, `backup_incremental`,
  `restore_incremental`; no server or command-line tool turns them on).
  The records of one commit (an auto-commit UPDATE, a transaction, a
  `materialize_rdfs` / `run_rules` run) are appended with one write after
  its commit point and `fdatasync`'ed; a direct-API mutation is appended
  when applied (a batch call such as `insert_data` or `insert_sources` as
  one commit) and synced by the next commit point, and `sync()` syncs the
  logs. The update log takes committed statements only (a rolled-back
  transaction's never reach it). Every record carries a CRC-32; replay
  applies a commit only once it read the commit's last record, skips an
  incomplete last commit (a torn tail, cut off by the next append) with a
  warning, and reports a damaged record in the middle as corruption. A
  failed append is reported (an UPDATE answers an error saying it is
  applied and committed but not logged; `commit` reports
  `CommittedWithError`) and its records stay owed to the next append;
  `backup_incremental` refuses while records are owed, when the copied
  range holds a corrupt record, and after a commit point that failed with
  an unknown outcome (the logs are "in doubt") — until a full `backup()`
  starts a new chain. `backup()` is refused inside an open transaction.
  `restore_incremental` reads its base read-only: a disk or RocksDB base is
  copied into `<base>.restore-tmp-<pid>-<n>-<nonce>` beside it (the system
  temporary directory only when the base's parent cannot be written),
  which needs about the base's size of free space there besides the memory
  the restored database takes. `replay_update_log` replays records written
  by earlier versions with the readings those versions had (a labelled
  blank node in a DELETE names the stored node, `CREATE` / `DROP` / `CLEAR
  GRAPH` as `SILENT`, the older grammar leniencies).
* **Shared decoders.** `gstore::util::form` (`url_decode`,
  `url_decode_bytes`, `parse_form`, `form_lookup`) and
  `gstore::util::base64::decode(s, Rules)` (`BASIC_AUTH`, `LENIENT`) are
  the decoders behind the REST endpoints, `/api` and PFN.
* **`ConcurrentDb`** (`concurrent`) wraps a database for snapshot-isolated
  readers and optimistic multi-writer transactions (library only): an
  UPDATE that fails part-way is still published (overlapping transactions
  conflict), and a panicking writer leaves no lock poisoned.

### Experimental: cluster

The `cluster` / `rpc` modules and the `gnode` binary (sharded scatter-gather
over TCP, Raft-style replication, elastic membership) are an **experimental
library feature**: they are not wired into `gserver` or the HTTP API, speak an
unauthenticated wire protocol that may change (trusted networks only), and
have no counterpart in upstream gStore 1.4 (whose `src/Cluster` is not built,
and whose distributed edition is not open source). Shard nodes and
coordinators keep their data in memory. A `gnode cluster` replica needs
`--state-dir DIR` — its Raft term, vote and log are journaled there
(`raft.journal`, CRC-checked, fsynced before it answers), so it can be
restarted safely — or `--ephemeral` (in memory, as before; never restart it
under the same id); without either it exits with an error. A replica whose
`raft.journal` is damaged anywhere but in the tail a crash leaves refuses to
start, naming the byte offset: restore its state directory from a copy, or
replace the replica (a new id, an empty state directory) rather than cutting
the file by hand. Forged Raft terms are refused, `ShardedRaftCluster`
reports only committed writes, a failed `ShardCluster` membership change is
resumed by repeating it (or `resume_migrations`) and blocks other changes
until then, and shard-node connections idle or stalled for 60 s are closed.
A shard that cannot answer fails the query with an error rather than
yielding a partial result. Use it for experiments, not for data you need to
keep.

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
| `dict`        | bidirectional string↔id dictionaries (copy-on-write in memory; `TermStr`, bounded caches for out-of-core backends) |
| `store`       | in-memory `s2xx`/`o2xx`/`p2xx` six-way index (copy-on-write); the `TripleSource` seam, `Backend`, per-statement read errors, the transaction overlay |
| `kvstore` / `backend` | on-disk pager + B+ trees, the format registry, `DiskTriples` / `DiskDict` (Stream mode); RocksDB backend |
| `signature`   | signatures + VS-tree candidate filter                  |
| `parser`      | N-Triples/Turtle/N-Quads/TriG/RDF-XML/JSON-LD/RDFa; SPARQL (query + update) |
| `query`       | algebra eval, joins, optimizer, FILTER, aggregates, Top-K |
| `analytics`   | CSR graph algorithms; `analytics::gfun`: upstream's 32 advanced functions |
| `server` / `http_api` / `http_users` | HTTP SPARQL server, conneg, auth |
| `gapi` / `sysdb` / `config` | upstream `/api` protocol, system database (privileges, logs), `conf.ini` |
| `console`     | `gconsole`, the `/api` client (and `--local` REPL)      |
| `logging`     | diagnostic log: levels, daily rolling file, retention   |
| `zipio`       | zip packages, zipped backups / exports (feature `zip`) |
| `rpc` / `cluster` / `concurrent` | experimental RPC cluster; `ConcurrentDb`: library-only snapshot isolation + OCC multi-writer transactions |
| `reason`      | upstream inference rules (`reasonManage`), RDFS, textual rules |
| `pfn`         | PFN user graph functions: per-user registry, `GraphCtx`, sandboxed WebAssembly runtime (`pfn-wasm`) |
| `bulk`        | bulk builds (`gbuild --disk` / `--rocksdb`): external sort, page images / SST ingest, crash-safe install |
| `db`          | `Database` facade + persistence (every layout), open options, id reclamation, directory locks |
| `gnlqa/`      | LLM natural-language QA layer (separate crate)          |
| `bench/`      | `gstore-bench`, the benchmark harness and CI gates (separate crate; [`bench/README.md`](bench/README.md)) |
| `bin/`        | `gbuild`, `gquery`, `gconsole`, `gserver`, …            |
| `packaging/`, `Dockerfile` | systemd unit, deb/rpm scripts, container image ([`docs/DEPLOY.md`](docs/DEPLOY.md)) |

See [`docs/DESIGN.md`](docs/DESIGN.md) for the gStore architecture and the mapping
to gStore's C++ modules.

## License

Follows upstream gStore (BSD-3-Clause). An independent reimplementation for study
and engineering purposes.
