# gNLQA — LLM + gStore natural-language question answering

gNLQA is an LLM front-end over the [`gstore`](../) RDF graph database. It turns a
natural-language question into a **validated SPARQL query** (or a graph-analytics
plan, or a GraphRAG retrieval), executes it against gStore, and returns a
grounded, cited answer — the modern, LLM-driven successor to gAnswer's QAKB
subgraph-matching pipeline.

Its distinguishing move: LLM-generated SPARQL is parsed and repaired with
gStore's **own** SPARQL parser before execution, so a candidate query is never
run unless it is syntactically valid, and the model gets a real error to repair
against. Structured results stay verifiable; free-text answers are grounded on
retrieved triples.

See [`../docs/NLQA_DESIGN.md`](../docs/NLQA_DESIGN.md) for the full design,
gAnswer parity table, and KGQA benchmark references.

## Pipeline

```
question
  └─ understand   intent extraction (fast model): type, mentions, relations, lang
  └─ link         mentions → KG entities/types/predicates (vector + relaxation)
  └─ ground       assemble the live schema neighbourhood of the linked URIs
  └─ generate     N Text-to-SPARQL candidates (validated by gStore's parser)
  └─ repair       execute + self-repair each candidate (bounded rounds)
  └─ rank         best_of: prefer non-empty answers, fewer repair rounds
  └─ answer       render values, gather citations, optional grounded explanation
```

Fallbacks and routing layered on top:

- **Graph analytics** — `analytics`-typed questions (shortest path, centrality,
  PageRank, connected components ("communities"), triangles) run over `gstore::analytics::GraphView`
  built from a retrieved edge sample, rather than Text-to-SPARQL.
- **GraphRAG** — when no structured query returns anything, retrieve a bounded
  subgraph around the linked entities and answer from it (cited, or "I don't
  know").
- **Multi-turn** — follow-ups are rewritten into standalone questions against the
  conversation history.
- **Multilingual** — the answer comes back in the question's language; entity
  linking can index labels in multiple languages.
- **Confidence & abstention** — a ranking-derived confidence; below a configurable
  threshold the engine withholds the answer (still exposing the SPARQL).

## Quick start

gNLQA reads a **`gnlqa.conf`** file at startup (a simple `KEY=VALUE` list; real env
vars override it), so you configure once instead of exporting every session. The
default backend is **DeepSeek** (an OpenAI-compatible endpoint):

```sh
cargo build -p gnlqa --release
cp gnlqa.conf.example gnlqa.conf     # then edit gnlqa.conf: set OPENAI_API_KEY=<your key>
```

`gnlqa.conf` is git-ignored (it holds your key); only the fake `gnlqa.conf.example`
is committed. It presets:

```ini
GNLQA_LLM_PROVIDER=openai
OPENAI_API_KEY=sk-...
OPENAI_BASE_URL=https://api.deepseek.com
GNLQA_MODEL=deepseek-v4-pro
GNLQA_FAST_MODEL=deepseek-v4-flash
GSTORE_ENDPOINT=http://127.0.0.1:9000/sparql
```

Load some RDF and serve it, then ask a question:

```sh
cargo run -q -p gstore --bin gbuild  -- movies movies.nt      # build movies.db
cargo run -q -p gstore --bin gserver -- movies.db --port 9000 # serve SPARQL
cargo run -q -p gnlqa  -- "who directed Inception?"           # → Christopher Nolan
```

To use **Anthropic (Claude)** instead, set `GNLQA_LLM_PROVIDER=anthropic` and
`ANTHROPIC_API_KEY` (models e.g. `claude-opus-4-8` / `claude-sonnet-4-6`).

> `deepseek-v4-pro` is a *reasoning* model — accurate but slow (~1–2 min/question,
> since the full pipeline makes several calls). Use `deepseek-v4-flash` for the
> fast path (already the default `GNLQA_FAST_MODEL`), or set both to flash for speed.

The LLM and embedding clients are trait objects with mocks, so the crate compiles
and its tests run **without** any API key.

## CLI

```sh
gnlqa "who directed Alien?"          # answer one question (prints answer; SPARQL to stderr)
gnlqa chat                           # interactive multi-turn REPL (:reset / :quit)
gnlqa mcp                            # MCP server over stdio (see below)
gnlqa eval qald   dataset.json       # score against a QALD benchmark
gnlqa eval lcquad dataset.json       # score against LC-QuAD (gold resolved via KB)
gnlqa serve 127.0.0.1:9100           # HTTP SPARQL-QA server (see below)
gnlqa                                # print configuration/readiness
```

While a question runs, gNLQA prints short stage logs to stderr (e.g.
`[gnlqa] generating SPARQL… (model)`, `[gnlqa] querying gStore…`) so you can see
whether it's waiting on the model or on gStore.

## Answering modes & provenance

Every answer is tagged with where it came from — a **trust and data-egress**
signal, not just attribution:

- **`gStore`** — structured SPARQL / graph-analytics computed locally; the
  underlying **data never left the machine** (only the question + schema went to
  the LLM).
- **`gStore+LLM (GraphRAG)`** — the LLM composed the answer from private triples
  **retrieved from gStore and sent to the LLM**.
- **`LLM`** — the LLM answered from its **general knowledge**; no KB data involved.

You choose which path(s) are allowed with `GNLQA_MODE` (or `/mode` in chat):

| Mode         | Behaviour | Provenance |
|--------------|-----------|------------|
| `auto` (default) | structured SPARQL, then GraphRAG fallback; **never** pure LLM | gStore / GraphRAG |
| `structured` | SPARQL / analytics **only** — data never leaves; abstains otherwise | gStore |
| `graphrag`   | retrieve a private subgraph → LLM answers from it | GraphRAG |
| `open`       | answer from the LLM's general knowledge; ignore the KB | LLM |

> Design note: a SPARQL-generation *failure* on an in-domain question is **not**
> silently answered from LLM world-knowledge (that risks confident hallucination
> on private-data questions) — `auto` falls back to GraphRAG or abstains. LLM-direct
> only happens when you explicitly choose `open`.

```sh
gnlqa chat
[auto]> /mode structured      # data never leaves the machine
[structured]> who directed Alien?
[open]> what is SPARQL?        # /mode open → answered from the LLM
```

## HTTP server

`gnlqa serve [addr]` (default `127.0.0.1:9100`) exposes:

| Method | Path      | Body                     | Response |
|--------|-----------|--------------------------|----------|
| POST   | `/ask`    | `{"question": "..."}`    | `{answer, sparql, confidence, abstained, explanation, ...}` |
| POST   | `/gSolve` | `{"question": "..."}`    | gAnswer-compatible shape (honours abstention with status `"abstained"`) |
| GET    | `/health` | —                        | `{"status":"ok"}` |

The server bounds request size and connection count and applies read/write
timeouts.

## MCP server

`gnlqa mcp` speaks JSON-RPC 2.0 over stdio (the `initialize` / `tools/list` /
`tools/call` handshake), exposing four tools to any MCP client:

| Tool              | Arguments                              | Purpose |
|-------------------|----------------------------------------|---------|
| `ask_kg`          | `question`                             | NL question → grounded answer |
| `run_sparql`      | `query`                                | raw SPARQL → results |
| `link_entity`     | `mention`, `kind?`, `k?`               | mention → KG candidates |
| `graph_analytics` | `question`, `seeds?`                    | shortest path / centrality / … |

Tool-level failures are returned as `isError` content; protocol faults as
JSON-RPC errors. Point `run_sparql` at a **read-only** endpoint if untrusted
clients may call it.

## Evaluation

`gnlqa eval` computes set-based precision / recall / F1 (the QALD empty-set
convention), macro-averaged over the questions:

- **QALD** datasets ship gold answers (and gold SPARQL) — used directly.
- **LC-QuAD** ships only gold queries — gold answers are resolved by executing
  the gold SPARQL against the configured KB. Questions whose gold can't be
  resolved locally are **skipped** (not scored), so the reported F1 isn't
  inflated by data the KB simply doesn't contain.

## Configuration

Settings come from **`gnlqa.conf`** (a `KEY=VALUE` file; path overridable with
`GNLQA_CONFIG`) with **environment variables taking precedence** — so you can keep
your defaults in the file and still override one for a single run
(`GNLQA_MODEL=… gnlqa …`). Copy `gnlqa.conf.example` to `gnlqa.conf` to start. The
same names work as env vars.

| Variable             | Default                            | Meaning |
|----------------------|------------------------------------|---------|
| `GNLQA_LLM_PROVIDER` | `anthropic`                        | LLM backend: `anthropic`, or `openai` for any OpenAI-compatible endpoint (DeepSeek, OpenAI, …) |
| `OPENAI_API_KEY`     | — (required when provider=openai)  | key for the OpenAI-compatible backend (redacted `Secret`) |
| `OPENAI_BASE_URL`    | `https://api.openai.com/v1`        | base URL for it, e.g. `https://api.deepseek.com` |
| `ANTHROPIC_API_KEY`  | — (required when provider=anthropic)| Claude API key (never logged; held in a redacted `Secret`) |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com`        | API base URL |
| `GNLQA_MODEL`        | `claude-opus-4-8`                  | primary model (hard questions), e.g. `deepseek-v4-pro` |
| `GNLQA_FAST_MODEL`   | `claude-sonnet-4-6`                | fast model (intent, follow-up rewrite, simple questions), e.g. `deepseek-v4-flash` |
| `GSTORE_ENDPOINT`    | `http://127.0.0.1:9000/sparql`     | gStore SPARQL endpoint |
| `GSTORE_USER`        | —                                  | optional HTTP Basic user |
| `GSTORE_PASSWORD`    | —                                  | optional HTTP Basic password (redacted `Secret`) |
| `GNLQA_MAX_TOKENS`   | `1024`                             | default completion cap |
| `GNLQA_TEMPERATURE`  | `0.0`                              | default sampling temperature |
| `GNLQA_TIMEOUT_SECS` | `60`                               | per-request HTTP timeout |
| `GNLQA_MODE`         | `auto`                             | answering mode: `auto`/`structured`/`graphrag`/`open` (see above) |

## Library

```rust
use gnlqa::{AnthropicClient, GStoreClient, SolveEngine, Config};

let cfg = Config::from_env();
let llm = AnthropicClient::new(/* key */ String::new(), cfg.anthropic_base_url.clone(), cfg.model.clone());
let kb = GStoreClient::from_config(&cfg);
let engine = SolveEngine::new(Box::new(llm), Box::new(kb))
    .with_fast_model(cfg.fast_model.clone())
    .with_cache(256);

let answer = engine.ask("which cities are in Germany?")?;
println!("{}", answer.text);
```

`Session::new(&engine)` wraps it for multi-turn; `McpServer`/`HttpServer` wrap it
for the respective transports; `gnlqa::eval` scores it against a benchmark.
