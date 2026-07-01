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
is committed. To use Claude instead, set `GNLQA_LLM_PROVIDER=anthropic` +
`ANTHROPIC_API_KEY`. Full details: **[`gnlqa/README.md`](gnlqa/README.md)**.

### Answering modes & privacy

Because gStore is often used to keep **private data local**, every gNLQA answer is
tagged with where it came from — a *data-egress* signal, not just attribution —
and you choose which paths are allowed (`GNLQA_MODE`, or `/mode` in chat):

| Mode | Path | Data leaves the box? | Provenance |
|------|------|----------------------|------------|
| `auto` (default) | structured SPARQL → GraphRAG fallback; never pure LLM | only on the GraphRAG fallback | `gStore` / `GraphRAG` |
| `structured` | SPARQL / analytics only; abstain otherwise | **no** — result computed locally | `gStore` |
| `graphrag` | retrieve a private subgraph → LLM answers from it | yes (triples sent to the LLM) | `GraphRAG` |
| `open` | LLM general knowledge; ignore the KB | no KB data involved | `LLM` |

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
* **On-disk B+ tree KVstore** (`gbuild --disk`) and an optional **RocksDB backend**
  (`--features rocksdb`) — see [`docs/STORAGE_BACKEND.md`](docs/STORAGE_BACKEND.md).
* **HTTP server** with content negotiation, users/auth, and streaming
  (`server` / `http_api` / `http_users`).
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
gserver mydb.db --port 9000  # HTTP SPARQL server
```

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
| `parser`      | N-Triples / Turtle / SPARQL (query + update)           |
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
