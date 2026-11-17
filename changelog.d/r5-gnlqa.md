# r5-gnlqa — gnlqa: authentication and cost limits, privacy, configuration trust, robustness, real-client tests

## Changelog

Behaviour changes first:

- **`gnlqa serve` requires a client token (review cluster-nlqa#13, sec#18;
  D1 secure default).** `/ask` and `/gSolve` run LLM-chosen read-only
  SPARQL with the server's own gStore credentials and spend the operator's
  LLM credit, so they are no longer open to anyone who can connect.
  `gnlqa serve` refuses to start unless `GNLQA_HTTP_TOKEN` is set (clients
  send `Authorization: Bearer <token>`, compared in constant time). The only
  alternative is the explicit switch `GNLQA_HTTP_ALLOW_UNAUTHENTICATED=on`,
  which answers loopback clients only (others get `403`), refuses to bind a
  non-loopback address and prints a warning at startup. In the library,
  `HttpServer::bind` starts in `HttpAuth::Unconfigured` (question endpoints
  answer `401` with `WWW-Authenticate: Bearer`) until `with_token`,
  `allow_unauthenticated_loopback` or `with_auth` is called. `/health` stays
  open.
- **Per-client rate limit, question size cap and per-request cost budget
  (cluster-nlqa#13, sec#18).** Each client IP may send
  `GNLQA_HTTP_RATE_PER_MIN` (default 30) question requests per minute —
  counted before authentication, so token guessing is bounded too — and
  gets `429` with `Retry-After` beyond that. A question longer than
  `GNLQA_MAX_QUESTION_BYTES` (4096) gets `413`. The engine work one request
  causes runs under the new thread-local `gnlqa::budget` (`CostLimits`):
  at most `GNLQA_MAX_LLM_CALLS` (16) LLM calls, `GNLQA_MAX_KB_ROWS` (200000)
  KB rows and `GNLQA_REQUEST_TIMEOUT_SECS` (120 s); the real HTTP clients cap
  their network timeouts (and retry waits) by the time left. An exhausted
  budget is the new `Error::Limit`, which, like a transport outage, ends the
  request (`Error::is_fatal`): `/ask` answers `503`, `/gSolve`
  `status:"error"`. MCP tool calls run under the same budget
  (`McpServer::with_limits`; `gnlqa mcp` uses the configured limits).
- **Error responses are redacted (cluster-nlqa#18).** `/ask`, `/gSolve`
  and MCP tool errors no longer echo the internal error text (ureq printed
  the endpoint URL with its `user:password@`; provider bodies such as an
  OpenAI 401 were passed through). Clients get a generic message, a stable
  `code` (`upstream_unavailable`, `kb_error`, `llm_error`, `no_answer`,
  `limit_exceeded`, `config_error`) and a `request_id`; the full error goes
  to stderr under that id (`Error::code`, `Error::client_message`). MCP
  `run_sparql` keeps the text of SPARQL-validation and gStore query errors
  (the feedback needed to fix a query). URL userinfo is stripped from every
  error and log line (`error::redact_userinfo`), and an endpoint URL with
  credentials draws a startup warning.
- **No configuration from the working directory (cluster-nlqa#15).** The
  config file is read from `$GNLQA_CONFIG`, else
  `$XDG_CONFIG_HOME/gnlqa/gnlqa.conf`, else `~/.config/gnlqa/gnlqa.conf`; a
  `gnlqa.conf` in the current directory is ignored with a note
  (`GNLQA_CONFIG=./gnlqa.conf` opts in). A world-writable config file is
  refused; an unreadable explicit `GNLQA_CONFIG` is reported.
  **Upgrade note:** a `gnlqa.conf` kept next to the checkout must be moved
  to `~/.config/gnlqa/gnlqa.conf` or named with `GNLQA_CONFIG`.
- **Credentials only reach the configured hosts (cluster-nlqa#15).** The
  gStore, Anthropic, OpenAI-compatible and embedding clients follow no
  redirects (ureq kept every header but `Authorization`/`Cookie` across
  hosts, so Anthropic's `x-api-key` followed a `302` anywhere); a `3xx` is an
  `Error::Http`. A credential sent over plain `http://` to a non-loopback
  host draws a startup warning (`Config::security_warnings`).
- **Follow-up rewrite honours the privacy table (cluster-nlqa#14).**
  `Session` records each answer's provenance and replays an earlier answer
  to the LLM only where the current mode allows that data to leave the
  machine: `gStore` answers in `graphrag` mode only, GraphRAG answers in
  every mode but `structured`, open-mode answers always. Before, a default
  `auto` chat and a `structured`→`open` switch both sent locally computed
  KB answers.
- **Structured mode keeps supporting triples local (cluster-nlqa#21).** With
  `with_explain(true)`, structured mode no longer sends the answer's
  supporting triples to the LLM (the explanation is skipped there); the
  structured-mode documentation now says precisely what the LLM does see
  (question, linked URIs and labels, schema samples, repair error text and
  the "no rows" signal).
- **Transient provider failures are retried, outages surfaced
  (cluster-nlqa#16).** LLM and embedding clients retry `429/500/502/503/504/529`,
  `GStoreClient` `429/502/503/504`, with exponential back-off from 500 ms
  honouring `Retry-After` (capped at 30 s, never past the request budget;
  `GNLQA_RETRIES`, default 3; `RetryPolicy`, `with_retry` on every client).
  A persisting provider or KB outage now ends the request — also on the
  intent call (a malformed intent still degrades, logged), the GraphRAG
  fallback (`graphrag::try_retrieve_subgraph`) and seeded analytics —
  instead of becoming an ungrounded guess or "no candidate query produced
  an answer". Citation and explanation failures are logged. The linker is
  built lazily (`SolveEngine::with_linker_builder`, `prepare_linker`): a KB
  unreachable at startup is logged and retried on a later request (at most
  once a minute in `gnlqa`), even after a panicking build.
- **Results over the 100000-row cap are never silently truncated
  (cluster-nlqa#17).** `KbClient::query` / `GStoreClient::query` /
  `parse_results` fail with a repairable error asking for a `LIMIT`; the new
  provided method `KbClient::query_capped` (and `parse_results_capped`)
  returns the first rows plus a truncation flag. MCP `run_sparql` appends a
  `[truncated]` note, analytics reports `truncated` when the client capped
  the edge sample. `kb::MAX_RESULT_ROWS` is public.
- **Bounded answer cache (cluster-nlqa#19).** The shared cache is bounded
  by estimated bytes (`GNLQA_CACHE_MAX_BYTES`, default 64 MiB,
  `with_cache_max_bytes`); an answer above an eighth of the budget is not
  cached, expired entries are removed, and `GNLQA_CACHE_TTL_SECS` now
  defaults to 3600 (was 0 = never expire; 0 still disables expiry).
- **HTTP server robustness (conc#10, cluster-nlqa#20).** A handler panic is
  caught, answered `500` and releases its connection slot (it used to leak
  the slot, so `max_conn` panics meant a permanent `503`); a failed thread
  spawn answers `503` instead of panicking the accept loop. The request
  read deadline also bounds the socket read timeout, so a silent client is
  cut off with `400` at the deadline (`with_read_deadline`).
- **Read-only gate refuses SERVICE (cluster-nlqa#23).**
  `validate_readonly_sparql` (generation, repair, MCP `run_sparql`) rejects
  a `SERVICE` clause anywhere in the query, so the LLM or an MCP client
  cannot make a federating store fetch an arbitrary URL.
- **Linker index paged and cached (cluster-nlqa#22).** Label, predicate and
  type lists are fetched in `ORDER BY … LIMIT 10000 OFFSET k` pages
  (`LinkerLabels::fetch`) — a capped index is a deterministic prefix, sizes
  and truncation are logged — and cached on disk per KB fingerprint
  (`kb_fingerprint`: endpoint, label settings and two predicate-bound
  `COUNT`s; `LinkerCache`, 0600 files, one day maximum age;
  `Linker::build_cached`). A repeated one-shot `gnlqa "<question>"` costs two
  `COUNT` queries instead of full-store `DISTINCT` scans. New keys
  `GNLQA_LINKER_MAX_LABELS` (10000), `GNLQA_LINKER_CACHE` (on),
  `GNLQA_LINKER_CACHE_DIR` (`$XDG_CACHE_HOME/gnlqa`).

New files and tests:

- `gnlqa/src/budget.rs` (per-request cost budget), `gnlqa/src/net.rs`
  (no-redirect agent, `RetryPolicy`, retry helper).
- `gnlqa/tests/` (new; cluster-nlqa#24): `common/mod.rs` — a scripted
  in-process HTTP stub that records requests, a raw HTTP client and a
  self-removing temp dir; `real_http_clients.rs` (the real gStore,
  Anthropic, OpenAI-compatible and embedding clients over a socket: request
  shape, status vs transport error classification, N-Triples, 32 MiB cap,
  retries, redirects, row cap), `http_server_auth_and_limits.rs`,
  `http_server_robustness.rs` (panic slot, body cap, `max_conn` 503 with a
  channel latch, read deadline), `privacy_egress.rs` (recording LLM per
  mode), `config_trust.rs` (runs the binary with a planted `gnlqa.conf`),
  `provider_failures.rs`, `error_redaction.rs`, `answer_cache_bounds.rs`,
  `readonly_gate.rs` (SERVICE placements, repair proposing UPDATE),
  `linker_cache.rs`. gnlqa now has 185 unit and 67 integration tests.
- Corrected tests (D6): `session::tests::auto_mode_rewrite_includes_prior_answer`
  asserted the cluster-nlqa#14 leak; it is now
  `auto_mode_rewrite_omits_prior_gstore_answer` (review comment), and the
  new `graphrag_mode_rewrite_includes_prior_answer` keeps its contrast role.
  `config::tests::default_has_cache_and_token_floor` pinned the old TTL
  default 0 (cluster-nlqa#19 comment). The in-module `http_server` tests
  now present a token (setup lines marked with cluster-nlqa#13 comments).

## GAP_PLAN status line

none

## Docs notes

- **README.md** (gNLQA quick start, section "B) Ask in natural language"):
  replace `cp gnlqa.conf.example gnlqa.conf` with
  `mkdir -p ~/.config/gnlqa && cp gnlqa.conf.example ~/.config/gnlqa/gnlqa.conf`
  (a `gnlqa.conf` in the working directory is no longer read; mention
  `GNLQA_CONFIG=./gnlqa.conf` as the explicit opt-in), and say that
  `gnlqa serve` needs `GNLQA_HTTP_TOKEN`. In "Answering modes & privacy",
  the follow-up rewrite now honours the table (a locally computed answer is
  never replayed outside `graphrag` mode) and structured mode never sends
  supporting triples, even with `with_explain`. The test count line
  ("incl. 159 for gNLQA") becomes 252 (185 unit + 67 integration).
- **docs/DEPLOY.md** (secure defaults and switches table): `gnlqa serve`
  client authentication — secure default: refuses to start without
  `GNLQA_HTTP_TOKEN`; switch `GNLQA_HTTP_ALLOW_UNAUTHENTICATED=on` (an
  environment / `gnlqa.conf` key, not a `conf.ini` key; there is no
  upstream gnlqa behaviour to restore) serves loopback clients without a
  token and refuses non-loopback binds; risk: every local user queries the
  KB with the server's credentials and spends its LLM credit; not used by
  `scripts/conformance.sh`. Also list the gnlqa limits
  (`GNLQA_HTTP_RATE_PER_MIN` 30, `GNLQA_MAX_QUESTION_BYTES` 4096,
  `GNLQA_MAX_LLM_CALLS` 16, `GNLQA_MAX_KB_ROWS` 200000,
  `GNLQA_REQUEST_TIMEOUT_SECS` 120) and that gnlqa reads its config only
  from `$GNLQA_CONFIG` / `~/.config/gnlqa/gnlqa.conf`.
- **CHANGELOG.md**: the bullets above, behaviour changes first; the
  `gnlqa.conf` location and the serve token are upgrade notes.
- **docs/REFACTOR_BACKLOG.md** (remaining, not refactors done here):
  cluster-nlqa#13 — the rate limiter keys on the TCP peer address, so
  behind a reverse proxy all clients share one bucket (gnlqa has no
  trusted-proxy setting; use the proxy's own limits there). cluster-nlqa#22
  — the KB fingerprint (two `COUNT`s) does not see a change that keeps the
  number of `rdfs:label` and `rdf:type` triples (e.g. a new predicate
  without labels or types); the one-day maximum age bounds that staleness,
  and deleting the cache directory forces a rebuild. cluster-nlqa#21 — the
  optional stricter mode that sends no KB-derived text at all (no labels or
  schema samples) was not added; structured mode's documentation now states
  exactly what it sends.
- **gnlqa/README.md, docs/NLQA_DESIGN.md §12, gnlqa.conf.example**: updated
  directly by this package (owned documents).

## Measured numbers

No hot path of the gstore crate was touched (gnlqa only), so the bench
smoke gate (convention 8) does not apply; gstore-bench was not run.
cluster-nlqa#22 is covered by a work counter instead of timing: on a
cache hit the linker build issues 2 KB queries (the fingerprint `COUNT`s)
instead of 3+ paged `DISTINCT` scans (`tests/linker_cache.rs`
`a_cached_index_skips_the_full_store_scans`); a 25000-label KB capped at
15000 is fetched in 2 ordered entity pages.

## Requests/Not yet

- No edits outside the package's ownership (gnlqa/**, gnlqa.conf.example,
  docs/NLQA_DESIGN.md, this fragment).
- New public API (D2: nothing removed): `budget` module and `CostLimits`,
  `Error::Limit`, `Error::{is_fatal, code, client_message}`,
  `error::redact_userinfo`, `HttpAuth`, `HttpLimits`,
  `HttpServer::{with_token, allow_unauthenticated_loopback, with_auth,
  with_limits, with_read_deadline, auth}`, `McpServer::with_limits`, `net`
  module and `RetryPolicy`, `with_retry` on the four clients,
  `KbClient::query_capped` (provided method), `GStoreClient::query_capped`,
  `parse_results_capped`, `MAX_RESULT_ROWS`,
  `graphrag::try_retrieve_subgraph`, `SolveEngine::{with_linker_builder,
  prepare_linker, with_cache_max_bytes, cache_usage}`, `CacheUsage`,
  `answer_bytes`, `DEFAULT_CACHE_MAX_BYTES`, `LinkerLabels`, `LinkerCache`,
  `kb_fingerprint`, `Linker::{build_cached, from_label_set}`,
  `Config::{from_lookup, cost_limits, http_limits, http_auth, retry_policy,
  security_warnings}`, `config_file_path`, new `Config` fields. Adding the
  `Error::Limit` variant and `Config` fields breaks exhaustive matches /
  struct literals of downstream code; none exist in the workspace.
- Behaviour notes for library users: `SolveEngine::linker()` may now run a
  deferred build; `GStoreClient::query` errors (instead of truncating) on
  more than 100000 rows.
