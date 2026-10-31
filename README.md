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

* **~720 tests pass** across the workspace (incl. 142 for gNLQA); `cargo clippy
  --workspace --all-targets -- -D warnings` is clean.

---

## Quick start

### Build

```bash
cargo build --release           # gStore CLIs + gnlqa
cargo test --workspace          # run the tests
```

The default build is dependency-light. Optional cargo features (all off by
default): `zip` (zip packages, zipped backups and exports), `gzip` (HTTP
response compression, `.gz` sources), `tls` (HTTPS listeners), `readline`
(`gconsole` line editing) — all pure Rust — and `rocksdb` (the RocksDB
backend; needs clang). For example `cargo build --release -p gstore
--features "zip gzip"`. To run `gserver` in a container or as a systemd
service, or from `.deb` / `.rpm` packages, see
[`docs/DEPLOY.md`](docs/DEPLOY.md).

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
* **SPARQL 1.1 subset** — `SELECT`/`ASK`/`CONSTRUCT`/`DESCRIBE`, BGP, `UNION`,
  `OPTIONAL`, `MINUS`, `FILTER`, `BIND`, `VALUES`, sub-`SELECT`, aggregates,
  property paths (`/ ^ | * + !`), `ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT`, and
  SPARQL UPDATE (`INSERT`/`DELETE`).
* **RDF input formats** — N-Triples, Turtle, N-Quads, TriG, RDF/XML, and
  (dependency-free, common-subset) **JSON-LD** and **RDFa**; detected by file
  extension (`parser::parse_file_by_ext`).
* **Graph functions callable in SPARQL** (24) — `SSSP`/`SSSPLEN`, `KHOPCOUNT`/
  `KHOPNEIGHBOR`/`KHOPREACHABLE`, `WCC`, `LOUVAIN`, `LABELPROP`,
  `BETWEENNESSCENTRALITY`, `CLOSENESSCENTRALITY`, `TRIANGLECOUNTING`,
  `CLUSTERINGCOEFF`, `PPR`, `JACCARDSIMILARITY`, `DIAMETERESTIMATION`, … (gStore's
  built-in graph-analysis functions).
* **On-disk B+ tree KVstore** (`gbuild --disk`) and an optional **RocksDB backend**
  (build with `--features rocksdb`, then `gbuild --rocksdb` / `gserver --rocksdb`)
  — see [`docs/STORAGE_BACKEND.md`](docs/STORAGE_BACKEND.md).
* **HTTP server** with content negotiation, users/auth, streaming, file
  upload/download, a built-in web console (`GET /`), and a task registry with
  cancellation (`server` / `http_api` / `http_users`).
* **Transactions** — commit/rollback, per-key MVCC + snapshot isolation, four
  named isolation levels, and full + **incremental** backup/restore.
* **Graph analytics** (`analytics`) — BFS/shortest-path, PageRank, connected
  components, degree/betweenness/closeness centrality, communities, triangles,
  k-core, weighted top-k.
* **RDFS/rule reasoning** (`reason`), an **RPC cluster** (`rpc` / `cluster`), and
  **concurrency/MVCC** (`concurrent`).

Remaining large-refactor items are tracked in
[`docs/REFACTOR_BACKLOG.md`](docs/REFACTOR_BACKLOG.md); C++ parity is documented in
[`docs/CPP_PARITY.md`](docs/CPP_PARITY.md).

### CLI tools

A database is a `<name>.db` directory. Main tools (there are more — `gadd`,
`gsub`, `gdrop`, `gshow`, `gbackup`, `grestore`, `gexport`, `gmonitor`, `gnode`;
the [command-line tools section of `docs/DEPLOY.md`](docs/DEPLOY.md#command-line-tools)
lists them in upstream 1.3's spelling):

```bash
gbuild mydb data.nt          # build a DB from RDF (add --disk for the on-disk B+ tree)
gquery mydb query.rq         # run a SPARQL query (or -e "…" inline)
gconsole -u root             # upstream's console, a client of a running gserver
gconsole --local mydb        # the embedded REPL on mydb.db (no server)
gserver mydb.db --port 9000  # single-DB HTTP SPARQL server + web console at /
gserver --api . --port 9000  # full multi-DB API server (users/RBAC, upload, txns,
                             # tasks, backup/restore); default login root / 123456
gserver                      # the same API server rooted at db_home (./dbhome/)
                             # on port 9000, settings from ./conf/conf.ini
gserver -b                   # … started in the background (-s: foreground)
gserver -S | -t | -r | -k    # status / stop / restart / kill it (via its pid file)
gexport mydb out.nq          # N-Quads (every graph); out.nt: the default graph;
                             # -z / out.zip: zipped (feature zip)
gbackup [-z] mydb bak        # a backup directory (or bak.zip)
grestore mydb2 bak           # the database first, then the backup (dir or .zip)
```

The tools also take upstream 1.3's options (`-db NAME`, `-f FILE`, `-p PATH`,
`-l`, `-z 0|1`, …), answer `-h`/`--help`, and exit with `0` (done), `1`
(failed) or `2` (invalid arguments). `gbuild` without a source builds an
empty database and refuses to overwrite an existing one unless `--force`;
`gbuild x.zip` reads a zip package (feature `zip`); `gshow` with no argument
lists every database in the current directory. *Changed:* `grestore` used
to take the backup first.

The multi-DB server also speaks upstream gStore's `/api` protocol (`GET|POST
/api` or `POST /` with `operation=…`, per-request `username`/`password`,
`encryption=1`, the `StatusCode` JSON envelope), so the upstream SDKs such as
`GstoreConnector.py` work against it unchanged, plus upstream's `/shutdown`
and `/restart` routes. Settings come from `conf.ini` (see
`conf/conf.ini.example`; `-c FILE` picks another file, and command-line flags
win over it; `operation=refreshconf` re-reads it). Known differences from
upstream: a database is **single-writer** — one transaction may be open on it
at a time (a second `begin` answers `1008`), and while it is open other
writes to it (`query` updates, `batchInsert`, …) answer `1004` until it is
committed or rolled back, where upstream runs concurrent transactions; zip
packages (`build`/`batchInsert`/`batchRemove`/`restore` from a `.zip`,
`compress=true` for `backup`/`export`) need the `zip` feature — without it
they answer `1003` naming the feature — and are unpacked only within
`conf.ini`'s `unzip_max_entries` / `unzip_max_size`, refusing unsafe entry
names; `export` takes an optional `format` (`nt` / `nq`) and, without it,
writes N-Quads (`.nq`) for a database with named graphs where upstream's
N-Triples would drop them; a user holding the restore
privilege on a database restores only that database's own backups; and
asynchronous results are kept out of the public export directory.

**Logs.** `gserver` writes a diagnostic log (errors, warnings, lifecycle
events) configured like upstream's: `conf.ini`'s `log_mode` names a log4cplus
properties file (`conf/slog.properties.example`), with the gstore-rust keys
`log_level`, `log_file`, `log_days`, `log_stderr` and the `GSTORE_LOG`
variable on top; files roll over daily. Every SPARQL statement — `/api`
`query` and the REST `/sparql`, `/update`, `/tquery` — is recorded in the
JSON query log with the client IP, answer count and latency, and the access
logs carry the client IP (see [`docs/DEPLOY.md`](docs/DEPLOY.md#configuration-and-logs)).

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
`gserver` has a database loaded, `gquery`, `gadd`, `gconsole --local`, `gbackup`, … on
the same directory fail with "… is locked by another process (pid N)" instead
of racing it — unload it there (or use the server's API) first. The server's
`drop`, `rename`, `restore` and rebuilding `/build` refuse a directory another
process holds, too. `Database::read_copy` reads a detached copy of a snapshot
database without the lock.

### gconsole

`gconsole` is upstream gStore 1.4's console: an interactive client of a
running `gserver`'s `/api` at `127.0.0.1:<port>`, the port read from
`./conf/conf.ini` like the server's (or `--port N`, `--host H`, `-c FILE`).
Every request carries the logged-in user's credentials, so the server checks
that user's privileges.

```text
$ gserver -b --load-dir data          # let `create` read the data sets in ./data
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
  statements may share a line. SPARQL typed directly (`SELECT`, `ASK`,
  `CONSTRUCT`, `DESCRIBE`, `INSERT`, `DELETE`, `PREFIX`, `BASE`, `LOAD`,
  `CLEAR`, `WITH`, …) runs on the current database.
* Commands (`help;` lists them, `help <command>;` shows one): `create <db>
  [<file|dir|url>]`, `use`, `show [<db>]`, `showdbs`, `pdb`, `unload`, `drop`,
  `backup [<path>]`, `restore <db> <path>`, `export [<path>]`, `batchinsert` /
  `batchremove <file>`, `sparql <file>`, `init <db>[,…]`, `refreshconf`; users
  and privileges: `addusr`, `delusr`, `setpswd`, `setpriv <user> <db>`,
  `clearpriv`, `pusr`, `showusrs`, `flushpriv`; reasoning rules (`addreason`,
  `listreason`, `compilereason`, `executereason`, `disablereason`,
  `showreason`, `deletereason`) and custom functions (`showpfn`, `addpfn`,
  `mdfpfn`, `delpfn`, `buildpfn`) — the server answers these with "The
  operation is not defined" until it implements them; `version`, `pwd`,
  `clear`, `quit` / `exit` (or Ctrl-D), and `history [<n>]`.
* Ctrl-C abandons the statement being typed, or the request in flight: the
  connection is dropped and, for SPARQL, the server's task is cancelled
  (`operationTaskCancel`); the console stays open.
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
  `sparql <file>` runs each `;`-separated statement of the file; results are
  fetched as JSON (an unbound cell is shown empty); a local file given to
  `create` / `batchinsert` is sent as an absolute path when the server runs
  on the same machine — the server still only reads files under its
  `--load-dir` directories and its upload / export directories.
* `gconsole --local <db>` (gstore-rust extension; also the older `gconsole
  <db>`) is the embedded REPL that opens `<db>.db` directly, without a
  server.

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
| `query`       | algebra eval, joins, optimizer, FILTER, aggregates     |
| `analytics`   | CSR graph algorithms (paths, centrality, communities)  |
| `server` / `http_api` / `http_users` | HTTP SPARQL server, conneg, auth |
| `gapi`        | upstream gStore 1.4's `/api` operations                 |
| `console`     | `gconsole`, the `/api` client (and `--local` REPL)      |
| `logging`     | diagnostic log: levels, daily rolling file, retention   |
| `zipio`       | zip packages, zipped backups / exports (feature `zip`) |
| `rpc` / `cluster` / `concurrent` | RPC cluster, MVCC/transactions      |
| `reason`      | RDFS / rule reasoning                                   |
| `db`          | `Database` facade + persistence (mem & disk)           |
| `gnlqa/`      | LLM natural-language QA layer (separate crate)          |
| `bin/`        | `gbuild`, `gquery`, `gconsole`, `gserver`, …            |
| `packaging/`, `Dockerfile` | systemd unit, deb/rpm scripts, container image ([`docs/DEPLOY.md`](docs/DEPLOY.md)) |

See [`docs/DESIGN.md`](docs/DESIGN.md) for the gStore architecture and the mapping
to gStore's C++ modules.

## License

Follows upstream gStore (BSD-3-Clause). An independent reimplementation for study
and engineering purposes.
