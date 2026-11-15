# r1-crash-dos — Single-request process killers and outbound SERVICE safety

## Changelog

Behaviour changes first:

- **`gserver --allow-service` no longer lets SPARQL `SERVICE` reach
  host-local addresses** (review M8: sec#6, robust#9, tests-b#1). An
  endpoint that resolves only to loopback, link-local or another host-local
  address (see the `is_local_addr` bullet) is refused before anything
  connects, and the query fails with an error naming the refusal, `SILENT`
  or not (like a `SERVICE` the restricted policy denies). The new flag
  **`--allow-local-hosts`** (only together with `--allow-service`) restores
  the reach, for `SERVICE` and `/api` `remote=true` downloads alike, and
  prints a start-up warning; given alone it only warns that it has no
  effect. The library default (`LoadPolicy::permissive`, used by `gquery`
  and embedders) still reaches every host.
- **`SERVICE` endpoint IRIs with whitespace or control characters are
  refused** (M8). The SPARQL lexer decodes `\u000D\u000A` inside an IRIREF,
  and the host and path went into the outbound request line and `Host:`
  header verbatim, so `SERVICE <http://h/x\u000D\u000AX-Injected:\u0020yes>`
  injected a request header. Such a pattern now fails like any other
  endpoint failure (no solution; `SILENT`: the identity solution) without
  connecting.
- **`http_client::is_local_addr` covers more special-purpose ranges**
  (sec#13): `100.64.0.0/10` (shared address space; a cloud metadata service
  lives at `100.100.100.200`), `198.18.0.0/15`, `240.0.0.0/4`, unique-local
  `fc00::/7` (an IPv6 cloud metadata endpoint is `fd00:ec2::254`),
  site-local `fec0::/10`, the IPv4-compatible block `::/96`, and the NAT64
  (`64:ff9b::/96`) and 6to4 (`2002::/16`) forms of host-local IPv4
  addresses, besides loopback, `0.0.0.0/8`, link-local, multicast and
  broadcast. This applies to every outbound path that refuses host-local
  addresses: `SERVICE` under `--allow-service`, `remote=true` downloads and
  asynchronous-operation callbacks (unless `callback_allow_local`). Private
  networks (`10/8`, `172.16/12`, `192.168/16`) stay reachable, by design.
- **A hostile `SERVICE` reply can no longer abort the process** (H-3:
  sec#5, gapi#3, robust#1). Replies are read with the crate's
  depth-limited `json::parse` instead of a private recursive reader: about
  50 KB of `[` used to overflow an 8 MiB worker stack and kill the whole
  server (SIGABRT, exit 134); it is now a parse error (a non-`SILENT`
  endpoint failure still answers "no solution", see Requests/Not yet).
  Escaped surrogate pairs decode to the right non-BMP character instead of
  two U+FFFD (gapi#10, robust#8), so such bindings join with local data; a
  lone surrogate makes the reply malformed.
- **`SERVICE` requests name the endpoint's port in `Host:`** when it is not
  80 (they sent `Host: 127.0.0.1` for `http://127.0.0.1:9000/…`).
- **`shortestPathCount` cannot blow up on wrapped path counts** (H-8,
  analytics#1). The search used upstream's `count == 0` as "not reached";
  once a count wrapped to exactly 0 (2^32 paths, e.g. a 2-wide ladder of 33+
  levels) the frontier doubled every level and the 400-triple ladder
  aborted the process on a failed allocation. It now tests the maps'
  stamps; the printed count is upstream's C `int` (wrapping) value, byte for
  byte, wherever upstream does not blow up itself.
- **`maximumKplex` on a graph without vertices** (an empty or literal-only
  database) answers the empty k-plex for every k (gap-gfun-fuzz#1; `k <= 2`
  panicked inside the worker and answered "internal error", `k = 3` printed
  `{"kplex":[,,,]}`).
- **Log retention skips foreign file names** (M15, bulkcli#1): a file such as
  `diag.log.<CJK text>` next to the log, whose tenth byte after `diag.log.`
  falls inside a character, made `gserver` exit with 101 at start-up (and
  the daily roll of `access.log` / `query.log` panic in the first request of
  a day).
- **The SPARQL lexer is linear on long name runs** (H-14, parser#2):
  `a-a-a-…` / `a.a.a…` without a `:` re-scanned the rest of the run for every
  word (160 KB took ~6 s in release, 1.6 MB about ten minutes of CPU per
  request, before parsing and uncancellable).
- **REST log lines are written with one `write(2)`** (conc#6): `access.log`,
  `query.log` and `txn.log` lines could interleave under concurrent requests
  (the line and its newline were two writes).

New library API (additions only): `db::ServicePolicy` (`Denied` /
`RemoteHosts` / `AnyHost`) and `LoadPolicy::service_policy()`;
`Evaluator::with_service_policy` (`with_service_allowed(bool)` keeps its
meaning: `true` = any host); `http_client::sparql_post_opts` (SERVICE under
`PostOptions`) and `http_client::is_host_local_refusal`;
`logging::append_line`. `LoadPolicy::allow_local_hosts` now also governs
`SERVICE`.

New tests: `tests/service_hardening.rs` (deep reply on an 8 MiB thread,
surrogate pairs end to end, CRLF / whitespace / control characters in
endpoint IRIs, host-local refusal on `Database` reads and updates,
`ConcurrentDb` snapshots and version views, REST `/sparql` and transaction
snapshot reads, the `is_local_addr` ranges), `tests/crash_regressions.rs`
(ladders of 20-100 levels, empty-CSR `maximumKplex`, non-ASCII log siblings,
1 MB lexer runs, concurrent log lines, concurrent REST access-log lines),
unit tests for the frontier bound, the lexer's look-ahead work counter,
`service_policy()`, the gserver flag and `sparql_post_opts`.
`tests/dt_security_review.rs` corrected (tests-b#1, see Docs notes).

## GAP_PLAN status line

none

## Docs notes

- **docs/DEPLOY.md** (the `--allow-service` paragraph): `SERVICE` is now
  under the same host-local rule as the downloads — "never from (or to) a
  loopback, link-local or other host-local address" applies to both; name
  the ranges (as in the `is_local_addr` bullet above) and the cloud
  metadata examples. Document `--allow-local-hosts` (with `--allow-service`:
  `SERVICE` and `remote=true` downloads may reach host-local addresses;
  unsafe on a shared server: clients can then reach services listening on
  loopback and cloud metadata endpoints). Add the sec#13 operator note the
  review asked for: asynchronous-operation callbacks (any user holding
  load/update/backup/restore on a database) are not gated by
  `--allow-service`; they refuse host-local addresses (now the wider set)
  unless `callback_allow_local`, but reach other internal hosts — restrict
  the server's outbound connections if that matters.
- **docs/DEPLOY.md, secure-defaults / upstream-compat table**: one row for
  `--allow-local-hosts` — a command-line flag, not a conf.ini key (no
  `upstream-compat:` start-up line: upstream gStore has no `SERVICE`, so
  this is not an upstream-compat switch); secure default off; enabling it
  restores the previous `--allow-service` reach of host-local endpoints;
  `scripts/conformance.sh` does not enable it (no upstream script uses
  `SERVICE`).
- **README.md** (the paragraph ending "…unless `--allow-service` is given —
  which also lets `/api` … download `http(s)://` sources (never from a
  loopback, link-local or other host-local address)"): say that `SERVICE`
  follows the same host-local rule unless `--allow-local-hosts` is given.
- **README.md** (gserver option list, if it repeats `gserver --help`): add
  `--allow-local-hosts`.
- **docs/REFACTOR_BACKLOG.md**:
  - SERVICE URL parsing (M8 follow-up, D3): `http_client` keeps two URL
    splitters, `parse_http_url` (SERVICE, `http://` only) and `parse_url`
    (callbacks, downloads). Both now refuse whitespace and control
    characters, but `parse_http_url` still does not accept IPv6 literals
    (`http://[::1]:p/`), a query without a path (`http://h?x`) or strip a
    fragment, and does not refuse user info. Folding SERVICE onto
    `parse_url` (keeping `https://` refused for SERVICE) is a small
    refactor left out of this crash/security package; review ids sec#6,
    robust#9.
  - The review's test-correction note: `tests/dt_security_review.rs`
    `service_cannot_reach_other_hosts_from_a_restricted_server` asserted
    that `allow_remote` alone reached a loopback endpoint and returned its
    data (tests-b#1); corrected to expect the refusal, plus the reach with
    `allow_local_hosts`.

## Measured numbers

All on the shared 4-vCPU / 15 GB machine with other packages' builds and
test runs going on (numbers are indicative; base and branch were measured
back to back in the same session). Base = 7bbf4ae, branch = this package's
head; release binaries (`cargo build --release -p gstore -p gstore-bench
--bin gbuild --bin gstore-bench --bin gquery`).

- **Lexer (H-14)**: `gquery <db> q.rq` with `q.rq` = `ASK { FILTER(?x =
  a-a-…-a) }` (whole process; it ends in a parse error right after lexing).
  Base, `-` separator: 40 KB 0.46 s, 80 KB 1.98 s, 160 KB 7.70 s, 320 KB
  30.1 s (×4 per doubling; `.`: 0.64 / 2.24 / 10.0 / 38.2 s). Branch: 160 KB
  0.035 s, 1 MB 0.153 s, 2 MB 0.169 s (`.`: 0.020 / 0.104 / 0.217 s) —
  linear, 1 MB far below 1 s. Work counter (unit test): look-ahead steps ≤
  input length for 1000-4000 words; the old scan took 1 002 010 steps for
  1000 words of `a-`.
- **shortestPathCount (H-8)**: the 400-triple ladder (L = 100) via `gquery`
  under `ulimit -v 1500000`: base aborts after 11.7 s ("memory allocation of
  536870912 bytes failed" in `shortest_path_count_nodes`); branch answers
  `"count":0` (2^100 as a C `int`) in 0.010 s. Unit test: peak frontier 2
  (3 with the extra chain) on the 100-level ladder; without the stamp fix
  the frontier guard fires.
- **Deep SERVICE reply (H-3)**: 100 000 nested `[` from a loopback
  endpoint, query on an 8 MiB thread: base SIGABRT (exit 134, "has
  overflowed its stack"), branch a parse error in milliseconds
  (`tests/service_hardening.rs`).
- **Bench smoke gate (D9)**: `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=/home/user/fix-campaign/bench-cache gstore-bench ci
  --baseline bench/baselines/smoke.json --bin-dir <bins>`: base PASS, branch
  PASS (456 gates checked, 0 failed). LUBM-like query geomean vs the
  committed baseline: base snapshot 0.90× / disk 0.90×, branch snapshot
  0.85× / disk 0.91×; REST `q1` single-client qps: base snapshot sparql
  16 509 / api 6 336, branch 19 425 / 7 765 (noise-level differences on a
  loaded machine; the changed hot-path code is the per-word lexer
  look-ahead, the access-log write and the `SERVICE` branch of the
  evaluator).

## Requests/Not yet

- **Edits outside the package's named regions** (each the smallest
  possible; the integrator should expect them):
  - `src/db/mod.rs`: the `pub use load_policy::{…}` line re-exports
    `ServicePolicy` (besides the two granted `with_service_*` lines in
    `read_as()` and `eval_modify_changes`'s `prepare()`).
  - `src/concurrent.rs`: struct `Snapshot`'s `allow_service: bool` field
    became `service_policy: ServicePolicy`, with its initialiser in
    `Snapshot::from_db` (besides the granted lines in `Snapshot::query` and
    `VersionView::query`): the local-host policy could not travel in a
    `bool`.
  - `src/http_api.rs`: the same field change in struct `ReadSnapshot` and
    its initialiser (the struct literal in the `ReadSnapshot` constructor),
    besides the granted `ReadSnapshot::configure` line and `append_log`.
  - `src/query/engine.rs`: the `Evaluator` field `allow_service: bool`
    became `service_policy: ServicePolicy` in the struct and its three
    literals (`new`, `with_vstree`, `graph_sub_evaluator`) — the granted
    case; round-2's Evaluator owner (r2-analytics) builds on it.
  - `src/bin/gserver.rs`: the call line `load_policy_for(&cli.load_dirs,
    cli.allow_service, cli.allow_local_hosts)` in `serve()` (r1-http-surface's
    function; a one-line sanctioned call-site edit per convention 13a), the
    new `Cli` field, the module docs' option list, and one new test fn in
    the test module.
  - `src/parser/sparql/lexer.rs`: besides `lex_word_or_pname`, struct
    `Lexer` got two fields and a `new` constructor (used by
    `tokenize_with_spans`), `run` takes `&mut self`, and the test module
    one new test.
- **Not yet, for another package**: a non-`SILENT` `SERVICE` whose endpoint
  fails (unreachable, malformed or too-deep reply, malformed IRI) still
  answers "no solution" rather than an error — review M14 (robust#6,
  tests-b#2, sec#17), r3-sparql-joins. `tests/service_hardening.rs`
  `a_service_endpoint_replying_100k_nested_brackets_does_not_abort` accepts
  either an error or no rows; tighten it to an error when M14 lands. The
  host-local refusal already is an error (a policy decision).
- **Plan deviation, H-8**: the plan suggested accumulating counts in i64 and
  truncating to i32 on output. After the stamp fix no decision reads a
  count, and wrapping `+` / `*` modulo 2^64 truncated to 32 bits equals the
  same operations modulo 2^32, so an i64 accumulator would print identical
  bytes; the counts stay `i32` (half the scratch memory). The frontier
  guard and peak-frontier counter are in as planned.
