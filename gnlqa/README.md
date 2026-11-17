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
  conversation history. The rewrite is an LLM call, so an earlier answer is
  only included when the current mode allows its data to leave the machine:
  `gStore` answers (locally computed result rows) only in `graphrag` mode,
  GraphRAG answers in every mode but `structured`, open-mode (`LLM`) answers
  always; prior questions are always sent.
- **Multilingual** — the answer comes back in the question's language; entity
  linking can index labels in multiple languages.
- **Confidence & abstention** — a ranking-derived confidence; below a configurable
  threshold the engine withholds the answer (still exposing the SPARQL).

## Quick start

gNLQA reads a **`gnlqa.conf`** file at startup (a simple `KEY=VALUE` list; real env
vars override it), so you configure once instead of exporting every session. The
file lives in your per-user config directory — `$GNLQA_CONFIG` if set, else
`$XDG_CONFIG_HOME/gnlqa/gnlqa.conf`, else `~/.config/gnlqa/gnlqa.conf`. A
`gnlqa.conf` in the **current directory is not read** (an untrusted checkout could
ship one that points the endpoints, and so your API key and gStore password, at
another host); `GNLQA_CONFIG=./gnlqa.conf` opts in for a directory you trust. A
world-writable config file is refused. The default backend is **DeepSeek** (an
OpenAI-compatible endpoint):

```sh
cargo build -p gnlqa --release
mkdir -p ~/.config/gnlqa
cp gnlqa.conf.example ~/.config/gnlqa/gnlqa.conf   # then set OPENAI_API_KEY=<your key>
chmod 600 ~/.config/gnlqa/gnlqa.conf
```

Only the fake `gnlqa.conf.example` is committed (a `gnlqa.conf` in the checkout is
git-ignored). It presets:

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

To use **Anthropic (Claude)** instead, switch the whole backend block in
`gnlqa.conf`: set `GNLQA_LLM_PROVIDER=anthropic` and `ANTHROPIC_API_KEY`, **and
also change the model names** — the example file pins DeepSeek models, so leaving
`GNLQA_MODEL=deepseek-v4-pro` in place would send a DeepSeek model id to the
Anthropic API and fail. Comment out the DeepSeek lines and uncomment the Anthropic
block (`GNLQA_MODEL=claude-opus-4-8`, `GNLQA_FAST_MODEL=claude-sonnet-4-6`).

> `deepseek-v4-pro` is a *reasoning* model — accurate but slow (~1–2 min/question,
> since the full pipeline makes several calls). Use `deepseek-v4-flash` for the
> fast path (already the default `GNLQA_FAST_MODEL`), or set both to flash for speed.

The LLM and embedding clients are trait objects with mocks, so the crate compiles
and its tests run **without** any API key.

**Failures are reported, not swallowed.** Transient provider errors (429, 5xx,
Anthropic's 529) and gStore overload (429, 502–504) are retried with exponential
back-off that honours `Retry-After` and never waits past a server request's time
budget. A provider or KB outage that persists ends the request with that error —
including on the intent call and the GraphRAG fallback — instead of degrading into
an ungrounded guess or "no candidate query produced an answer". If the KB is
unreachable when the entity linker is built, the failure is logged and the build is
retried on a later request (at most once a minute).

## CLI

```sh
gnlqa "who directed Alien?"          # answer one question (prints answer; SPARQL to stderr)
gnlqa chat                           # interactive multi-turn REPL (:reset / :quit)
gnlqa mcp                            # MCP server over stdio (see below)
gnlqa eval qald   dataset.json       # score against a QALD benchmark
gnlqa eval lcquad dataset.json       # score against LC-QuAD (gold resolved via KB)
gnlqa serve 127.0.0.1:9100           # HTTP SPARQL-QA server (needs GNLQA_HTTP_TOKEN; see below)
gnlqa                                # print configuration/readiness
```

While a question runs, gNLQA prints short stage logs to stderr (e.g.
`[gnlqa] generating SPARQL… (model)`, `[gnlqa] querying gStore…`) so you can see
whether it's waiting on the model or on gStore.

## Answering modes & provenance

Every answer is tagged with where it came from — a **trust and data-egress**
signal, not just attribution:

- **`gStore`** — structured SPARQL / graph-analytics computed locally; the query
  **result data stays on the machine**. To *generate* the SPARQL, the LLM does
  receive the question, the linked entity/predicate URIs and their labels, and
  small schema samples — but never the answer rows/triples. (Enabling
  `with_explain` additionally sends the answer's supporting triples to the LLM to
  phrase an explanation; it's off by default.)
- **`gStore+LLM (GraphRAG)`** — the LLM composed the answer from private triples
  **retrieved from gStore and sent to the LLM**.
- **`LLM`** — the LLM answered from its **general knowledge**; no KB data involved.

You choose which path(s) are allowed with `GNLQA_MODE` (or `/mode` in chat):

| Mode         | Behaviour | Provenance |
|--------------|-----------|------------|
| `auto` (default) | structured SPARQL, then GraphRAG fallback; **never** pure LLM | gStore / GraphRAG |
| `structured` | SPARQL / analytics **only** — result data stays local; abstains otherwise | gStore |
| `graphrag`   | retrieve a private subgraph → LLM answers from it | GraphRAG |
| `open`       | answer from the LLM's general knowledge; ignore the KB | LLM |

`GNLQA_MODE` (and `/mode` in chat) applies on **every** surface — the one-shot CLI,
`chat`, `serve`, and `mcp` — so a server started with `GNLQA_MODE=structured`
keeps its result data local too.

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
| POST   | `/ask`    | `{"question": "..."}`    | `{answer, sparql, values, citations, confidence, abstained, explanation, rounds, provenance}` |
| POST   | `/gSolve` | `{"question": "..."}`    | gAnswer-compatible shape (honours abstention with status `"abstained"`) |
| GET    | `/health` | —                        | `{"status":"ok"}` |

**Authentication.** `/ask` and `/gSolve` run LLM-chosen read-only SPARQL with
the server's own gStore credentials and spend its LLM credit, so they are not
open by default: `gnlqa serve` refuses to start unless a client token is set
with `GNLQA_HTTP_TOKEN` (clients send `Authorization: Bearer <token>`). The only
alternative is the explicit opt-out `GNLQA_HTTP_ALLOW_UNAUTHENTICATED=on`, which
answers loopback clients only and refuses to bind a non-loopback address (behind
a reverse proxy on the same host every client looks local — use a token there).
`/health` needs no token.

```sh
GNLQA_HTTP_TOKEN=$(openssl rand -hex 24) gnlqa serve 127.0.0.1:9100
curl -H "Authorization: Bearer $GNLQA_HTTP_TOKEN" -d '{"question":"who directed Alien?"}' \
     http://127.0.0.1:9100/ask
```

**Limits.** Each client IP may send `GNLQA_HTTP_RATE_PER_MIN` (default 30)
question requests per minute — counted before authentication, so token guessing
is bounded too — and gets `429` with `Retry-After` beyond that. A question longer
than `GNLQA_MAX_QUESTION_BYTES` (default 4096) gets `413`. The work one request
may cause is budgeted: at most `GNLQA_MAX_LLM_CALLS` LLM calls (default 16),
`GNLQA_MAX_KB_ROWS` KB rows (default 200000) and `GNLQA_REQUEST_TIMEOUT_SECS`
seconds (default 120); a request that exhausts its budget gets `503` (`/gSolve`:
`status: "error"`). The same budget applies to every MCP tool call. The server
also bounds request size and connection count and applies read/write timeouts.

## MCP server

`gnlqa mcp` speaks JSON-RPC 2.0 over stdio (the `initialize` / `tools/list` /
`tools/call` handshake), exposing four tools to any MCP client:

| Tool              | Arguments                              | Purpose |
|-------------------|----------------------------------------|---------|
| `ask_kg`          | `question`                             | NL question → grounded answer |
| `run_sparql`      | `query`                                | raw SPARQL → results |
| `link_entity`     | `mention`, `kind?`, `k?`               | mention → KG candidates |
| `graph_analytics` | `question`, `seeds?`                    | shortest path / centrality / … |

Failures *inside* a correctly-invoked tool are returned as `isError` content;
protocol faults (unknown tool, malformed params) are JSON-RPC `-32602` errors.
`run_sparql` enforces a read-only gate (SELECT/ASK only — UPDATE/DELETE/DROP/LOAD
are rejected before reaching the store), so an injected mutation can't run.

## Evaluation

`gnlqa eval` computes set-based precision / recall / F1 (the QALD empty-set
convention), macro-averaged over the questions:

- **QALD** datasets ship gold answers (and gold SPARQL) — used directly.
- **LC-QuAD** ships only gold queries — gold answers are resolved by executing
  the gold SPARQL against the configured KB. Questions whose gold can't be
  resolved locally are **skipped** (not scored), so the reported F1 isn't
  inflated by data the KB simply doesn't contain.

## Configuration

Settings come from **`gnlqa.conf`** (a `KEY=VALUE` file read from `$GNLQA_CONFIG`,
else `$XDG_CONFIG_HOME/gnlqa/gnlqa.conf`, else `~/.config/gnlqa/gnlqa.conf` — never
from the current directory) with **environment variables taking precedence** — so
you can keep your defaults in the file and still override one for a single run
(`GNLQA_MODEL=… gnlqa …`). Copy `gnlqa.conf.example` there to start. The same names
work as env vars.

The clients never follow HTTP redirects, so an API key or the gStore password is
only ever sent to the configured host (a redirecting endpoint is an error: configure
its final URL). At startup gNLQA warns when a credential would travel in clear text
over `http://` to a non-loopback host.

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
| `GNLQA_MAX_TOKENS`   | `1024`                             | minimum completion-token budget floored onto every LLM request (raise it for reasoning models that spend tokens on hidden reasoning) |
| `GNLQA_TIMEOUT_SECS` | `60`                               | per-request HTTP timeout |
| `GNLQA_RETRIES`      | `3`                                | retries of a transient LLM / embedding failure (429, 5xx, 529) or gStore overload (429, 502–504), with exponential back-off honouring `Retry-After` (`0` = none) |
| `GNLQA_MODE`         | `auto`                             | answering mode: `auto`/`structured`/`graphrag`/`open` (see above); applies to CLI, chat, serve, and mcp |
| `GNLQA_CACHE`        | `256`                              | answer-cache capacity in questions (`0` disables the cache) |
| `GNLQA_CACHE_TTL_SECS` | `0`                              | cache entry lifetime in seconds (`0` = no expiry; set it when the KB changes under a long-running server) |
| `GNLQA_HTTP_TOKEN`   | — (required by `serve`)            | bearer token HTTP clients must send on `/ask` and `/gSolve` (redacted `Secret`) |
| `GNLQA_HTTP_ALLOW_UNAUTHENTICATED` | `off`                | `on` = serve loopback clients without a token (refuses non-loopback binds) |
| `GNLQA_HTTP_RATE_PER_MIN` | `30`                          | `/ask` + `/gSolve` requests per client IP per minute (`0` = unlimited) |
| `GNLQA_MAX_QUESTION_BYTES` | `4096`                       | longest accepted question for `serve` (bytes) |
| `GNLQA_MAX_LLM_CALLS` | `16`                              | LLM calls per `serve` request / MCP tool call (`0` = unlimited) |
| `GNLQA_MAX_KB_ROWS`  | `200000`                           | KB rows fetched per `serve` request / MCP tool call (`0` = unlimited) |
| `GNLQA_REQUEST_TIMEOUT_SECS` | `120`                      | wall-clock budget per `serve` request / MCP tool call (`0` = none) |

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
