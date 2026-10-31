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
# upstream's scripts/test/parser_test.sh (50 SPARQL cases), in-process, per-case table:
cargo test --test dt_upstream_parser_suite -- --nocapture
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
* **SPARQL 1.1 subset** — `SELECT`/`ASK`/`CONSTRUCT`/`DESCRIBE`, BGP, `UNION`,
  `OPTIONAL`, `MINUS`, `FILTER`, `BIND`, `VALUES`, sub-`SELECT`, aggregates,
  property paths (`/ ^ | * + !`), `ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT`, and
  SPARQL UPDATE (`INSERT`/`DELETE`).
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
`gsub`, `gdrop`, `gshow`, `gbackup`, `grestore`, `gexport`, `gmonitor`, `gnode`):

```bash
gbuild mydb data.nt          # build a DB from RDF (add --disk for the on-disk B+ tree)
gquery mydb query.rq         # run a SPARQL query (or -e "…" inline)
gconsole mydb                # interactive REPL (end a query with ';')
gserver mydb.db --port 9000  # single-DB HTTP SPARQL server + web console at /
gserver --api . --port 9000  # full multi-DB API server (users/RBAC, upload, txns,
                             # tasks, backup/restore); default login root / 123456
```

Every tool opens a database directory whatever its layout (snapshot, on-disk
`--disk`, RocksDB), and updates are persisted back to it. `gserver` saves each
write before acknowledging it (gStore's `check_point`; `--no-checkpoint` defers
saving to `/checkpoint`, `/unload` and the graceful shutdown on Ctrl-C/SIGTERM).
SPARQL `LOAD` sent to a server reads nothing unless `--load-dir DIR` allows it.

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
eagerly, like upstream's `loadcsr`). Upstream parses `cycleBoolean`,
`simpleCycleBoolean`, `shortestPathLen` and `kHopReachable` inside `FILTER` but
never evaluates them there; here that is a clear error. `PFN.name({…})` parses
but is not evaluated yet.

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
| `rpc` / `cluster` / `concurrent` | RPC cluster, MVCC/transactions      |
| `reason`      | RDFS / rule reasoning                                   |
| `db`          | `Database` facade + persistence (mem & disk)           |
| `gnlqa/`      | LLM natural-language QA layer (separate crate)          |
| `bin/`        | `gbuild`, `gquery`, `gconsole`, `gserver`, …            |

See [`docs/DESIGN.md`](docs/DESIGN.md) for the gStore architecture and the mapping
to gStore's C++ modules.

## License

Follows upstream gStore (BSD-3-Clause). An independent reimplementation for study
and engineering purposes.
