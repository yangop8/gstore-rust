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

## Install / build

Part of the gStore Cargo workspace:

```sh
cargo build -p gnlqa --release
export ANTHROPIC_API_KEY=sk-ant-...      # required for live answers
```

The LLM and embedding clients are trait objects with mocks, so the crate compiles
and its tests run **without** an API key.

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

## Configuration (environment)

| Variable             | Default                            | Meaning |
|----------------------|------------------------------------|---------|
| `ANTHROPIC_API_KEY`  | — (required for live answers)      | Claude API key (never logged; held in a redacted `Secret`) |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com`        | API base URL |
| `GNLQA_MODEL`        | `claude-opus-4-8`                  | primary model (hard questions) |
| `GNLQA_FAST_MODEL`   | `claude-sonnet-4-6`                | fast model (intent, follow-up rewrite, simple questions) |
| `GSTORE_ENDPOINT`    | `http://127.0.0.1:9000/sparql`     | gStore SPARQL endpoint |
| `GSTORE_USER`        | —                                  | optional HTTP Basic user |
| `GSTORE_PASSWORD`    | —                                  | optional HTTP Basic password (redacted `Secret`) |
| `GNLQA_MAX_TOKENS`   | `1024`                             | default completion cap |
| `GNLQA_TEMPERATURE`  | `0.0`                              | default sampling temperature |
| `GNLQA_TIMEOUT_SECS` | `60`                               | per-request HTTP timeout |

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
