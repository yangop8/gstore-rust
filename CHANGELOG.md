# Changelog

All notable changes to gstore-rust are recorded here.

## Unreleased

The work toward upstream gStore 1.4 parity (`docs/GAP_PLAN.md`) lands in
phases. Phases 0 and 1 (durability and hardening; the upstream `/api`
server) and Phases 2 and 3 (graph functions; SPARQL 1.1 and Top-K) were
developed in parallel from the same base and merged; Phase 5 (RDF import,
gconsole, zip, logging and packaging) was developed from the Phase 1 head
and merged after them; Phase 4 (reasoning rules and PFN functions) was
developed from the Phases 0–3 head and merged after Phase 5; Phase 6 (scale
and performance) was developed on the merged Phases 0–5. A review-fix
campaign then fixed the findings of a code review of the Phase 6 tree; it
comes first below, followed by Phase 6 and the CI notes. Then the
integration sections record what joining the earlier phases changed, newest
first, and the remaining ones each phase, newest first (Phase 5, then
Phase 4, …).

### Review-fix campaign (after Phase 6)

A code review of the Phase 6 tree (`7bbf4ae`: 24 reviewers, 382 finding ids
after 3 refuted ones, merged into 291 items — 2 critical, 19 high, 76
medium, 159 low, 35 info) was fixed in five rounds of parallel work packages
(37 packages). Each round was merged, its merged tree reviewed again and the
confirmed findings fixed before the next round was cut. Round 1 took the
most urgent items (both critical findings and most of the high ones): the
storage failure state, durable acknowledgements, RocksDB statement
atomicity, request ingest caps and framing, the browser-facing HTTP surface,
downloads and the crash / denial-of-service reproductions. Round 2: term
consistency, the transaction lifecycle, limits and authentication,
cancellable index builds, layout replacement and directory locks,
graph-function limits and a W3C-style SPARQL test subset. Round 3: SPARQL
result correctness (property paths, joins, functions and aggregates), REST
parity and typed errors, the redo log and incremental backup, the RDF
parsers and the storage kernel. Round 4: the executor differential guard,
storage validation and cross-version fixtures, command-line tools and
configuration, reasoning and PFN, `ConcurrentDb`, API hygiene and graph
management. Round 5: gconsole, the experimental cluster, gnlqa, the
test-suite audits, gstore-bench and packaging. The 372 finding ids assigned
to packages were worked off, every behavioural fix with a regression test
that fails without it; a few were fixed only in part (for example design#6,
cluster-nlqa#9, cluster-nlqa#12), and their remainders, the 10 deferred ids
(four of the frozen cluster, three info and three low items), the 3 refuted
findings and every follow-up a package left open are in
[`docs/REFACTOR_BACKLOG.md`](docs/REFACTOR_BACKLOG.md) with their review ids
and reasons.

Review ids are written `<area>#<n>` (`sec#1`, `durability#6`, …); C-1, C-2,
H-1 … H-19 and M1 … M15 are the review's priority list. Insecure upstream
behaviour is off by default and comes back only through an explicit
upstream-compat switch, which gserver reports at startup
(`upstream-compat: <key>=<value>`); [`docs/DEPLOY.md`, "Secure defaults and
upstream-compat
switches"](docs/DEPLOY.md#secure-defaults-and-upstream-compat-switches)
lists every switch with its secure default, the upstream value, the risk and
whether `scripts/conformance.sh` turns it on. The upstream `/api` keeps HTTP
200 with its StatusCodes. No database file format changed (the redo log's
new framing is read by older binaries, see below), and no public API or
cargo feature was removed; the new error variants
`GStoreError::StorageFailed`, `GStoreError::Timeout` and
`GStoreError::Locked` (and gnlqa's `Error::Limit`) need an arm in exhaustive
matches.

#### Upgrade notes

- **An API-mode gserver refuses to start while root's password is
  `123456`**, on loopback too (H-10: sec#8, infra#3). Change the password
  first, or start once on `127.0.0.1` with
  `[server] allow_default_root_password=on`, change it (gconsole `setpswd`,
  `/api userPassword`) and remove the switch. Fresh deb / rpm installs get a
  random password in `/etc/gstore/root_password`.
- **Request bodies before authentication are capped at 64 MiB**
  (`[upload] preauth_max_body_size`) and `upload_max_body_size` now defaults
  to 256 MiB (it was upstream's 1 GiB) (C-2). Upstream clients that put
  their credentials in the body (SDK `/api` JSON, the Workbench's
  `/file/upload` form) are held to 64 MiB unless the credentials go in the
  URL query string or the cap is raised.
- **Browsers and reverse proxies** (H-10): no response carries
  `Access-Control-Allow-Origin` unless the origin is listed in
  `[server] cors_allow_origin`; a state-changing request with a foreign
  `Origin`, and a loopback request naming a foreign `Host`, get 403; a
  request a proxy forwards (`Forwarded`, `X-Forwarded-For`, `X-Real-IP`)
  loses loopback privileges. Behind a proxy that rewrites `Host`, list the
  public origin in `cors_allow_origin`.
- **`/file/download` needs credentials** (`[upload] anonymous_download=on`
  restores upstream's public export directory); recorded uploads, exports
  and query-result files are removed after `[upload] file_retention_hours`
  (168) (sec#4, sec#14).
- **GET never runs an UPDATE**, `POST /update` with `text/plain` or
  `multipart/form-data` answers 415 (`[server] allow_get_update=on` restores
  both), and the dataset protocol parameters (`default-graph-uri`, …) are
  refused (H-10, gap-doc-claims#2).
- **Slow clients and crowded addresses**: a request body must arrive at
  `[server] min_body_rate` (64 KiB/s on average after a 30 s grace) and one
  address holds at most `[server] max_conns_per_ip` (64) connections; raise
  them behind NAT or a reverse proxy (H-1, sec#3).
- **Failed logins lock the account at that address** (the 5th failure,
  back-off up to 15 minutes; `[server] upstream_login_lockout=on` restores
  upstream's rule), **idle transactions are rolled back after 600 s**
  (`[system] txn_idle_timeout=0` restores "never"), and **statements stop
  after `[server] query_timeout` (3600 s), answers at
  `[server] max_result_bytes` (1 GiB)** (M6, M3, M7).
- **A write applied in memory but not saved answers a failure** — REST 500,
  `/api` 1005, `"applied in memory but not durable: …"` — instead of success
  with a warning (H-7).
- **SPARQL**: a failed non-`SILENT` `SERVICE` fails the query (add `SILENT`
  to keep the empty answer); blank nodes are refused in `DELETE DATA`,
  `DELETE WHERE` and DELETE templates (delete a stored blank node's triples
  with `FILTER(sameTerm(?o, _:x))`); an aggregate query may project only
  grouped variables; string and hash functions refuse non-string arguments
  (`STRLEN(STR(?v))` gives the old reading); `CREATE` / `DROP` /
  `CLEAR GRAPH` follow the graph-existence rule (`SILENT` keeps the old
  no-op).
- **Exit statuses**: `gbuild` exits 3 when tolerant mode skipped statements
  (the database is built), `gserver` exits 2 on an argument error, and a
  gserver built without the `tls` feature refuses `--tls-cert` / `--tls-key`
  instead of serving plain HTTP.
- **conf.ini**: a `#` or `;` starts an inline comment only as a word of its
  own after the value; a marker glued to text
  (`db_home=/srv/gstore #primary`) is now part of the value, as upstream
  reads it, and `root_password` is never cut. Every cut, and every value an
  earlier version would have cut, is a startup warning.
- **systemd**: files gserver creates are no longer world-readable
  (`UMask=0027`), a port below 1024 needs a drop-in granting
  `CAP_NET_BIND_SERVICE`, and `--load-dir`, `db_path` and `backup_path`
  cannot be under `/home`, `/root` or `/run/user` (`ProtectHome=yes`).
- **Other tools**: `gnode cluster` needs `--state-dir DIR` or `--ephemeral`;
  `gnlqa serve` needs `GNLQA_HTTP_TOKEN` (or
  `GNLQA_HTTP_ALLOW_UNAUTHENTICATED=on`, loopback only), and gnlqa reads its
  configuration from `$GNLQA_CONFIG` or `~/.config/gnlqa/gnlqa.conf` only (a
  `gnlqa.conf` in the working directory is ignored);
  `cargo test -p gstore-bench` on its own needs `cargo build --profile test
  -p gstore --bin gbuild` first; `scripts/conformance.sh` fails when a suite
  is skipped (`--allow-skip` accepts it).

#### Behaviour changes: security defaults and upstream-compat switches

Each `conf.ini` switch set to its upstream value makes gserver print one
`upstream-compat: <key>=<value>` line at startup
(`GStoreConfig::upstream_compat_warnings()`). `scripts/conformance.sh` turns
on exactly `allow_default_root_password=on` and `anonymous_download=on`
(upstream's scripts log in as root / 123456 and download anonymously) and
fails the run when gserver does not report them (`check_compat_switches`).
The risk of each upstream value is in
[`docs/DEPLOY.md`](docs/DEPLOY.md#secure-defaults-and-upstream-compat-switches).

| switch | secure default | upstream value | review |
|---|---|---|---|
| `[server] allow_default_root_password` | `off`: an API-mode gserver does not start while root's password is `123456` | `on`: the password accepted on a loopback address, from local clients only | H-10, sec#8, sec#9, infra#3 |
| `[server] cors_allow_origin` | empty: no CORS headers; listed origins get their own origin echoed | `*`: `Access-Control-Allow-Origin: *` | H-10, infra#3 |
| `[server] allowed_hosts` | empty: a loopback request must name a local host | `*`: no `Host` check | H-10 |
| `[server] allow_get_update` | `off`: GET never updates; `/update` refuses `text/plain` and multipart bodies | `on` | H-10, http#2, design#2, sec#8 |
| `[server] trust_proxy_headers` | `off`: a forwarding loopback peer is remote | `on`: every loopback peer is local | sec#9, gapi#22 |
| `[server] upstream_login_lockout` | `off`: lockout per account and address on every password path | `on`: upstream's per-address lockout | gapi#5, sec#7, http#6 |
| `[system] txn_idle_timeout` | `600`: idle transactions rolled back after 600 s | `0`: transactions never expire | gap-txn-lifecycle#1 |
| `[upload] anonymous_download` | `off`: `/file/download` needs credentials | `on`: a public export directory | sec#4, gapi#2 |

Two opt-ins outside `conf.ini` print their own startup warning instead of an
`upstream-compat:` line: `gserver --allow-local-hosts` (with
`--allow-service`, lets `SERVICE` and `/api remote=true` reach host-local
endpoints again; sec#6, robust#9) and gnlqa's
`GNLQA_HTTP_ALLOW_UNAUTHENTICATED=on` (serves loopback clients without a
token; cluster-nlqa#13, sec#18).

- **Root's default password is refused on every address** (H-10: infra#3,
  sec#9, sec#8). In API mode a root that still has `123456` (a new user
  store seeded with it, or an existing one) makes gserver refuse to start,
  on loopback too (it used to warn there); the message names both remedies.
  With `allow_default_root_password=on` gserver starts on a loopback address
  only (still refused beyond loopback) and accepts the password from local
  clients only.
- **Packages set a random root password on a fresh install** (infra#3,
  sec#8). When no user store exists under `/var/lib/gstore` and
  `/etc/gstore/conf.ini` still names the default password, the deb postinst
  and the rpm `%post` write a random 32-hex-digit password to
  `/etc/gstore/root_password` (0640 root:gstore; written beside it, synced
  and renamed into place, every step checked) and print where it is; the
  systemd unit sets `GSTORE_ROOT_PASSWORD_FILE=/etc/gstore/root_password`,
  so gserver seeds the user store with it at the first start. `conf.ini`
  stays byte-identical (a dpkg conffile); a deb purge removes the password
  file; upgrades, existing stores and set passwords are left alone. gserver
  now skips, with a warning, a `$GSTORE_ROOT_PASSWORD_FILE` that does not
  exist (in Docker it stopped gserver); `--password-file` must still be
  readable.
- **CORS only for listed origins** (H-10, infra#3). No response carries
  `Access-Control-Allow-Origin` by default (it was `*` on every response).
  `[server] cors_allow_origin` lists origins (`scheme://host[:port]`,
  separated by `|`, `,` or spaces) that get their own origin echoed with
  `Vary: Origin`; `*` restores upstream's `ACAO: *`. A preflight from a
  listed origin gets 200 with the `Access-Control-Allow-*` headers, from any
  other origin 403 without CORS headers, without `Origin` 200 without
  `ACAO`. `cors=off` still turns CORS processing off; the shipped
  `conf.ini.example` no longer turns on `ACAO *`.
- **DNS-rebinding guard** (H-10). A request arriving over loopback must name
  a local host in `Host`: an IP literal, `localhost` / `*.localhost`, a
  dotless name, the configured `listen_addr` when it is a name, or a name in
  the new `[server] allowed_hosts` (`*` turns the check off); any other name
  answers 403 (`/api`: HTTP 200, StatusCode 1101). A request without `Host`
  is accepted; non-loopback peers are not checked.
- **CSRF guard** (H-10). On both servers a state-changing request (any
  method but GET, HEAD, OPTIONS) whose `Origin` names neither the server
  itself (`http(s)://` + the request's own `Host`) nor an origin of
  `cors_allow_origin` answers 403 (`/api`: 1101). Clients that send no
  `Origin` (curl, the SDKs, upstream scripts, gconsole) are unaffected; the
  bundled web consoles match when opened at the server's own address. Behind
  a reverse proxy that rewrites `Host` (nginx's default `proxy_pass`) list
  the public origin in `cors_allow_origin`, or keep the client's `Host`
  (`proxy_set_header Host $host;`) and list it in `allowed_hosts`; a front
  end served from another origin (upstream's Workbench) needs its origin in
  `cors_allow_origin`.
- **GET never runs a SPARQL UPDATE, and `/update` takes typed bodies**
  (H-10: http#2, design#2, sec#8). `GET /sparql` with an UPDATE answers 400
  (classified by a parse before anything runs; only texts holding an update
  keyword or a backslash are parsed twice); `POST /update` with `text/plain`
  or `multipart/form-data` — the types a page can POST cross-site without a
  preflight — answers 415, on both servers. Accepted:
  `application/sparql-update`, a form, an unlabelled body, or any other type
  (taken verbatim). An UPDATE POSTed to `/sparql` still runs (REST: with the
  Update privilege). `[server] allow_get_update=on` restores both upstream
  behaviours.
- **Proxies are not local clients** (sec#9, gapi#22). One predicate,
  `server::peer_is_trusted_local`, decides locality: a loopback peer
  forwarding `Forwarded`, `X-Forwarded-For` or `X-Real-IP` is a reverse
  proxy speaking for remote clients and loses loopback privileges. The
  unauthenticated single-database server refuses it (403), root's default
  password is refused from it (REST Basic and `/login`: 401; `/api`: 1001,
  plaintext or `encryption=1`; a constant comparison that does not count
  towards the lockout), `/api`'s `trust_inner_localhost` no longer applies,
  and the pre-body credential check leaves its credentials unchecked (body
  held to the pre-auth cap). `[server] trust_proxy_headers=on` restores
  upstream's rule. Detection relies on the proxy adding one of those
  headers: nginx without `proxy_set_header X-Forwarded-For
  $proxy_add_x_forwarded_for;`, HAProxy without `option forwardfor`, stunnel
  and ssh / socat tunnels cannot be told apart.
- **`/file/download` (and `/grpc/file/download`) needs credentials** (sec#4,
  gapi#2): `username` / `password` parameters as before or HTTP
  `Authorization: Basic`; without them HTTP 401 with
  `WWW-Authenticate: Basic` and a 1001 envelope, before anything is
  resolved. `[upload] anonymous_download=on` restores upstream's public
  export directory.
- **Failed-login lockout per account and client address, on every password
  path** (M6: gapi#5, sec#7, http#6). Failures are counted per account and
  address (an IPv6 client by its /64, an IPv4-mapped one by its IPv4
  address); after more than `login_lockout_failures` failures (default 4, so
  the 5th locks, as upstream's `errornum > 4`) that account is locked for
  that address — 1 s at first, each further failure doubling the lock up to
  `login_lockout_max_seconds` (default 900) — and the counts are forgotten
  after an hour without a failure. Every path that checks a password asks
  the lockout before the PBKDF2 hash runs: `/api` `login`, `testConnect` and
  every authenticated operation (also `/shutdown`, `/restart`), the pre-body
  credential check, REST `/login` and Basic credentials, and the
  single-database server's Basic auth. A refused attempt costs no hash and
  is answered `/api` 1101, REST and single-database `429 Too Many Requests`
  with `Retry-After`. A success clears only its own account's count at that
  address; a lock refuses only that account's password checks (other users,
  other operations and logged-in sessions from the address keep working).
  More than `8 × login_lockout_failures` (32) failures of any accounts from
  one address lock the address's password checks too ("too many failed
  logins from this address"), except for accounts that authenticated from it
  within the last hour. The key is the TCP peer's address (behind a reverse
  proxy all clients share the proxy's). `refreshconf` applies the three
  keys. `[server] upstream_login_lockout=on` restores upstream's rule (per
  address; `login`, `testConnect` and REST `/login` exempt and still
  evaluating guesses; any successful login resets the address; until
  restart).
- Library: `IpFilter::locked_out` (with `note_failure`, upstream's
  per-address lockout) locks out only a filter built
  `with_upstream_lockout(true)`; a filter from `IpFilter::new` counts
  `note_failure`s but `locked_out` stays `false` — the default lockout is
  `auth_refusal` / `note_auth_failure` (gapi#5). Embedders that used `new` +
  `note_failure` + `locked_out` add `.with_upstream_lockout(true)`.
- **`gserver --allow-service` no longer lets SPARQL `SERVICE` reach
  host-local addresses** (M8: sec#6, robust#9, tests-b#1). An endpoint
  resolving only to loopback, link-local or another host-local address is
  refused before anything connects and the query fails with an error naming
  the refusal, `SILENT` or not. The new flag `--allow-local-hosts` (only
  with `--allow-service`) restores the reach for `SERVICE` and
  `/api remote=true` downloads alike and prints a startup warning; given
  alone it only warns that it has no effect. The library default
  (`LoadPolicy::permissive`, used by `gquery` and embedders) still reaches
  every host.
- **SERVICE endpoint IRIs with whitespace or control characters are
  refused** (M8: sec#6, robust#9): the SPARQL lexer decodes `\u000D\u000A`
  inside an IRIREF, and host and path went verbatim into the request line
  and the `Host` header (`SERVICE <http://h/x\u000D\u000AX-Injected: yes>`
  injected a header). Such a pattern fails like any other endpoint failure,
  without connecting.
- **More special-purpose ranges count as host-local** (sec#13):
  `http_client::is_local_addr` adds 100.64.0.0/10 (cloud metadata at
  100.100.100.200), 198.18.0.0/15, 240.0.0.0/4, unique-local fc00::/7 (IPv6
  metadata fd00:ec2::254), site-local fec0::/10, IPv4-compatible ::/96 and
  the NAT64 (64:ff9b::/96) and 6to4 (2002::/16) forms of host-local IPv4,
  besides loopback, 0.0.0.0/8, link-local, multicast and broadcast. It
  applies to `SERVICE` under `--allow-service`, `remote=true` downloads and
  asynchronous-operation callbacks (unless `callback_allow_local`); private
  networks (10/8, 172.16/12, 192.168/16) stay reachable by design.

#### Behaviour changes: HTTP, REST and `/api`

- **Request bodies before authentication are capped** (C-2: sec#1, gapi#1,
  infra#1). Both servers decide a body's cap between head and body: every
  request may carry `[upload] preauth_max_body_size` (64 MiB,
  `HttpOptions::preauth_max_body`); only a request authenticated from its
  head on a payload route may carry `upload_max_body_size` — API mode: HTTP
  Basic, a REST session token (`Authorization: Bearer`, `X-Session-Token`,
  `session=` / `token=` URL parameters) or upstream's `username` /
  `password` (`encryption=1`) in the URL query string, under the IP lists
  and the lockout. Payload routes: `/file/upload` (also `/grpc/`), `/api`
  (and `/grpc/api`, `/ghttp/api`, `POST /`) when the URL names no operation
  or `query` / `tquery`, REST `/upload`, `/build`, `/update`, `/tquery`,
  `/batchInsert`, `/batchRemove`; the single-database server checks Basic
  credentials on `POST /update` and `POST /sparql`. A request the browser
  policy refuses before routing is held to the pre-auth cap whatever it
  carries (answered 403 after its body). A larger `Content-Length` is
  answered 413 without reading a body byte, a chunked body is cut off at its
  cap (413), and `Expect: 100-continue` is answered only after the decision.
  Head credentials that fail are answered 401 before the body and count once
  towards the lockout; a locked-out or blocked address is held to the
  pre-auth cap, also with a valid session token. An upstream inner request
  (`inner=true`) is never checked before its body. The check runs only for a
  body that may exceed the pre-auth cap.
- **All request bodies in memory share a budget per server** (C-2: sec#1,
  gapi#1): `[upload] request_body_budget` (`HttpOptions::body_budget`;
  512 MiB, never below `upload_max_body_size`). Bodies are charged as their
  bytes arrive and held until the response is written; a request whose
  announced body does not fit is answered 503 before its body is read, one
  that runs out part-way 503 too, and the connection is closed after either.
- **`upload_max_body_size` defaults to 256 MiB** in the code and the shipped
  configuration (was 1 GiB, upstream's value) (infra#1); the `/api` upload
  check (`1005 "Upload file more than max_body_size!"`) follows it.
- **`/api` request bodies are parsed within bounds** (gapi#1): a JSON body
  holds at most 65,536 values (a wider one answers `1003 "the request body
  is not valid JSON (… too many JSON values …)"` before more of the tree is
  built), a URL query string, form body or multipart body at most 65,536
  parameters / parts (1003).
- **Multipart boundaries follow RFC 2046** (H-2: sec#2, robust#2): 1 to 70
  `bchars` (letters, digits, `'()+_,-./:=?` and an inner space); a longer
  boundary or another character is answered 1003. The boundary search is
  linear (memchr two-way) instead of body × boundary (a 60,000-character
  boundary made one 8 MiB request cost 11 s of CPU before authentication).
- **A slow client can no longer hold a worker thread** (H-1: sec#3, http#1,
  robust#3, http#3). A request body must arrive at `[server] min_body_rate`
  (bytes/s, default 65,536; `0` = the per-read timeout alone) on average
  after a grace of `read_timeout` from the end of its head: t seconds in, at
  least (t − read_timeout) × min_body_rate bytes received, an allowance
  earned only by bytes actually received. So no body holds a worker longer
  than `read_timeout + length / min_body_rate`; before, only a single read
  was bounded (30 s), one byte every 29 s held a worker indefinitely and
  thread_num + 1 connections disabled the server. Under TLS the handshake
  and every record of the head count against the head deadline and body
  records against the body deadline (enforced below rustls at every socket
  read); the idle wait between kept-alive TLS requests is bounded the same
  way. A request that misses its deadline is answered `400 bad request: the
  request did not arrive in time` and the connection closed. A legitimate
  upload slower than 64 KiB/s on average after the 30 s grace is cut off.
- **A chunked body's framing counts against the body cap** (sec#3, http#1):
  chunk-size lines with extensions, CRLFs and trailers may take 64 KiB plus
  a sixteenth of the cap besides the data; more is answered 413 (80 one-byte
  chunks with 60 KiB extensions each, 4.9 MB on the wire, passed a 1 KiB
  pre-auth cap).
- **A streamed response must be read at `min_body_rate`** after a grace of
  `write_timeout` (gapi#4, sec#3); a client that falls behind loses its
  connection, freeing its worker and its `download_max_concurrent` slot
  (eight slow readers, the default slot count, turned every other download
  away with 503).
- **New `[server] max_conns_per_ip`** (`HttpOptions::max_conns_per_ip`,
  default 64, `0` = no limit) (sec#3): a further connection from an address
  that already has that many open (waiting for a worker included) is closed
  as soon as it is accepted, logged at most once a second (`refused a
  connection from <ip>: it already has <n> open ([server]
  max_conns_per_ip)`). IPv4-mapped IPv6 peers count as their IPv4 address.
  Clients behind one NAT and every client of a reverse proxy share an
  address: raise the key (or set `0`) there.
- **Strict HTTP/1.1 request framing** (sec#16, http#14; RFC 9112 §5, §6,
  §7.1), answered 400 with the connection closed: `Content-Length` together
  with `Transfer-Encoding` (accepted before, connection kept); differing
  (the last won) or non-decimal `Content-Length` values (`+5`, `-5`, `0x5`,
  empty, `5, 5`); any `Transfer-Encoding` other than exactly `chunked`
  (`gzip`, `identity`, `xchunked`, `gzip, chunked`, `chunked, chunked`, a
  `chunked` line followed by an `identity` line) and any `Transfer-Encoding`
  in HTTP/1.0; chunk sizes that are not bare hex; header lines without a
  colon (skipped before), with an empty or non-token name, whitespace before
  the colon, or an obs-fold continuation. Identical duplicate
  `Content-Length`, `Transfer-Encoding: Chunked` in any case, chunk
  extensions and trailers are still accepted.
- **Control characters in request heads are refused** (robust#14): a request
  line, header line, chunk-size line or trailer holding a control character
  other than HTAB (bare CR, NUL, ESC, DEL, …), or ending in CR CR LF, is
  answered `400 bad request: control character in a request line, header or
  chunk line` (a bare CR in a header value was kept and a CORS preflight
  reflected it into `Access-Control-Allow-Headers`).
- **Request parameters follow the Content-Type** on the single-database
  `Server` and the REST endpoints of `ApiServer` (H-9: design#1, http#5).
  Parameters come from the URL query string and from a form body only
  (`application/x-www-form-urlencoded`, or an unlabelled body shaped like a
  form); an `application/sparql-query` or `application/sparql-update` body
  and any other raw body is the statement or document verbatim. An IRI or
  literal containing `&update=…`, `&query=…`, `&sparql=…`, `&db_name=…` or
  `&db_path=…` no longer runs another statement (`INSERT DATA {
  … <http://ex/x?a=1&update=CLEAR%20DEFAULT> }` ran `CLEAR DEFAULT`),
  switches the database or names a server-side file. A form body without
  `query=` / `update=` is a missing statement (400). The upstream `/api`
  keeps upstream's parameter rules (`gapi::params`), except that since round
  4 a body labelled `application/sparql-query` or
  `application/sparql-update` is never read as a form there either
  (design#17), so `&sparql=DELETE WHERE {…}&db_name=other` inside it can no
  longer replace the statement or the database of the URL.
- **Session tokens are read from the request head or URL only** (http#4):
  `Authorization: Bearer`, `X-Session-Token`, `session=` / `token=` URL
  parameters; a `session=` body parameter no longer works. REST `/login`
  reads `username` / `password` with the server-wide state lock released (a
  `Basic` header on `/login` is not looked at); `/update`'s 415 check
  decides by Content-Type alone, before authentication.
- **Dataset parameters are refused** (gap-doc-claims#2): `/sparql` with
  `default-graph-uri` / `named-graph-uri` and `/update` with
  `using-graph-uri` / `using-named-graph-uri` answer
  `400 … dataset parameter … is not supported` on both servers instead of
  silently evaluating over the whole default graph; an empty value is
  ignored. `FROM` / `FROM NAMED` in the query text are still skipped (known
  gap).
- **`/api` accepts a `POST /login` session in place of the password**
  (ext#12; a gstore-rust extension of the upstream protocol). A request
  without a `password` parameter may authenticate with a live session token
  presented as the REST endpoints take it; the same holds for `/file/upload`
  and, when it names a `username`, `/file/download`. A `username` given must
  name the session's user; an unknown, expired, logged-out or revoked token
  is answered `1001 "The session is invalid or has expired: log in again."`.
  A request carrying a password is checked exactly as before (the token is
  then ignored). No password is checked on this path, so it neither adds to
  nor clears the failed-login lockout at the caller's address
  (`Ctx::session_auth`); IP allow / deny lists and the upstream-lockout
  check run before it as for every operation.
- **Every file the server writes for a client has an owner** (sec#4,
  gapi#14). An upload, export or `format=file` / `n-triple-file` query
  result gets an owner record in `<db home>/file_owners/` (path, kind,
  database, user; never served), written atomically and durably before the
  answer names the file. A user other than root / admin reads such a file
  (download, or a `build` / `batchInsert` / `batchRemove` source via `file`
  or a file of `dir`) only when the record allows: an export with the
  `export` privilege on its database, a query result with `query` on it, an
  upload he uploaded. Files in the export and upload directories without a
  record (operator files, older versions) are root's and admin's alone. A
  refused download is upstream's `FileNotFound` (StatusCode 11), a refused
  source 1003 (for a `dir`, the answer names the directory only).
- **Export and query-result file names carry 128 random bits** (sec#4,
  gapi#2): `<db>_<YYYYmmddHHMMSS>_<32 hex>.nt|nq|zip` and
  `<db>_<threadId>_<UTC YYYYmmddHHMMSS>_<32 hex>.json|txt`; clients use the
  reported `filepath` / `FileName` (wire format unchanged).
- **Downloads are streamed** (gapi#4, robust#7, sec#11): the file (or Range)
  is copied to the connection in 64 KiB chunks with its `Content-Length` (it
  was read whole into memory and copied again, about twice the file; backups
  and database files too). A streamed download is never compressed, even
  with the `gzip` feature and `Accept-Encoding: gzip`; JSON envelopes are
  compressed as before. New `[upload] download_max_concurrent` (default 8,
  `0` = no limit); further downloads answer 503 with `Retry-After: 1`.
- **Retention** (sec#14, gapi#13): new `[upload] file_retention_hours`
  (default 168, `0` = keep forever). Recorded uploads, exports and query
  results older than this are removed with their records, records whose file
  is gone are dropped, operator files are never touched; spilled
  asynchronous results under `<root>/tasks` that belong to no task of the
  running server are removed too. The sweep is lazy (uploads, exports,
  downloads, `format=file` queries and async submissions, at most once a
  minute per server). An unparsable entry of `file_owners/` (torn by a crash
  under the old code) is dropped once older than the retention (one hour
  when retention is off), with its file once that has expired too. Exports
  and query results are therefore deleted after a week by default.
- **New `[upload] upload_user_quota`** (bytes of retained uploads per user;
  default `0`, no quota) (gapi#14, sec#14): an upload beyond it answers
  `1005 "Upload quota exceeded: …"`. Uploads stay open to every user, as
  upstream's privilege table says.
- **Login sessions are bounded** (sec#12, http#12): a login sweeps expired
  sessions once the table has grown (next sweep when it has doubled:
  amortised O(1) per login); one user holds at most `MAX_SESSIONS_PER_USER`
  (1024) sessions, a further login ending that user's oldest session, never
  another user's; a session expires once its age reaches the TTL. New
  `UserStore::session_count()` and `UserStore::set_session_ttl()`.
- **User-store changes whose `users.db` save fails change nothing**
  (sec#15): `UserStore::add`, `drop_user`, `set_password`, `forget_database`
  and `copy_database_privileges` roll the in-memory table back (a "failed"
  password change stayed live until a restart), and sessions are revoked
  only after a successful save. A save whose new `users.db` was renamed into
  place but whose directory fsync failed (twice) keeps the change in memory
  too and logs a durability warning.
- **Startup reconcile forgets only databases confirmed gone** (gapi#9): if
  the server root cannot be listed the registry is not reconciled ("cannot
  list the database directories"); a `<db>.db` that exists but cannot be
  examined (a dangling symbolic link to an unmounted volume, a failing stat)
  keeps its registration, creator and grants ("stays registered"); only a
  NotFound directory unregisters a database and forgets its grants.
- **`ipmanage` saves only entries its loader reads back** (gapi#7): the list
  loader keeps a line shorter than 7 characters when it is a valid entry
  (`::1`, `::`, `1::`; upstream skips every such line), and `IpFilter::save`
  refuses a list the loader would not read back whole (an allow list of
  `::1` came back empty and locked everyone out). A save whose file was
  replaced but whose directory fsync failed puts the new list in effect and
  logs a durability warning.
- **Idle transactions are rolled back after `[system] txn_idle_timeout`**
  (M3: gap-txn-lifecycle#1; seconds, default 600, `0` = never, upstream's
  behaviour, reported as `upstream-compat: txn_idle_timeout=0`). A
  transaction with no statement running for that long is rolled back and
  logged `ABORTED` (`txn.log`: `aborted`; "rolled back after
  txn_idle_timeout"); one whose owner was deleted is rolled back whatever
  the timeout ("rolled back because its owner was deleted"). Reaping happens
  at a new `begin` on the database (which is no longer refused by such a
  transaction), on any request once the earliest idle deadline passed (one
  atomic read per request otherwise) and in a background pass every
  `min(txn_idle_timeout / 4, 30 s)`; neither the request path nor the
  background pass waits for a database (a busy one is retried later), so
  `/cancel` and other requests stay immediate. A statement in flight, and a
  `/commit` or `/rollback` in flight, are activity. `/tquery`, `/commit` or
  `/rollback` of an expired transaction answers "not running (rolled back
  after txn_idle_timeout)" (409 / 1005). Before, one `/begin` by any user
  with the update privilege, or that user's crashed client, blocked every
  write and maintenance operation of the database until root found the TID
  or the server restarted.
- **Who may end a transaction** (gap-txn-lifecycle#5): any superuser
  (`root`, or a user with the `admin` privilege) may `/tquery`, `/commit` or
  `/rollback` any transaction (before, only the literal `root`); `rollback`
  no longer needs the update privilege (the handlers let only the owner or a
  superuser end it), so an owner whose update privilege was revoked can roll
  back their own transaction; committing still needs it. Diverges from
  upstream's `check_privilege`.
- **Stale transaction ids never end another transaction** (M4:
  gap-txn-lifecycle#2). REST and `/api` share one TID parser
  (`http_api::parse_tid`): a positive integer or the transaction log's
  `<begin>_<tid>` form (REST no longer accepts a sign, `+2`). The begin time
  of a `<begin>_<tid>` id must be the transaction's; otherwise the id names
  no open transaction and gets the remembered end of the earlier transaction
  it names ("not running (already committed …)"), else "Wrong transaction
  id" (REST 404, `/api` 1005). Before, the begin time was ignored, so after
  a reload or restart the Workbench's "roll back the stale RUNNING row"
  ended the new transaction with the same number. Ended outcomes keep their
  database's load generation and begin time and survive reloads; at start a
  server marks transactions a previous process left `RUNNING` in
  `logs/transaction.json` as `ABORTED`. `begin` still answers the bare id.
- **Open transactions are reported** (gap-txn-lifecycle#1): REST `/show` and
  `/monitor` and `/api` `show` and `monitor` give each database's open
  transaction as a `transaction` object with `TID`, `txnlog_TID`
  (`<begin>_<tid>`), `user`, `isolation`, `begin_time`, `idle_seconds` and
  `running_statements`.
- **Statements stop after `[server] query_timeout`** (sec#10, robust#5;
  seconds, fractions allowed, default 3600, `0` = off). A statement still
  running then — REST `/sparql`, `/update` and `/tquery`, `/api` `query`
  (synchronous or `async`) and `tquery`, the single-database server's
  `/sparql` and `/update` — is stopped at its next cancellation check and
  answered as timed out: REST and single-database 503 `{"error":"query timed
  out after … (query_timeout) …"}`, `/api`
  `1005 "Query fail: query timed out …"` (`"Transaction query fail: …"` in a
  transaction, which stays open). It releases the database it held, so an
  UPDATE queued behind a runaway read completes. The deadline counts the
  statement's evaluation only (from when it holds its database's guard until
  the evaluation returns; neither the wait behind another statement, a save
  or a backup nor the check_point save after an UPDATE counts). A
  multi-operation UPDATE stopped part-way says "operations before the
  failing one were applied" (also in a transaction, where they stay applied
  until rollback); on the single-database server such an UPDATE, like a
  timed-out one, is 503. The evaluator polls the cancel flag inside one join
  step (BGP loop, binding extension, between joins, property-path frontier
  steps), so `/cancel` and a shutdown also stop a cross product or a closure
  promptly.
- **Answers stop at `[server] max_result_bytes`** (sec#10; bytes, default
  1 GiB, `0` = off). An answer whose serialization passes it is refused as
  soon as it does — REST and single-database 503
  `{"error":"result too large: …"}`, `/api` 1005 (an upper bound of the
  returned rows' size counted as escaped JSON strings, `json::escaped_len`,
  summed before the document is built). A streamed single-database answer
  (`?stream=true`) is cut off without its closing chunk; REST `/tquery` and
  `/api` `tquery` are bounded too; `format=file` / `n-triple-file` results
  (written to disk) are not.
- **Failed `/api` queries are recorded in the query log** with their real
  `statusCode` / `statusMsg` (gapi#8): query errors, unknown formats,
  timeouts, too-large answers; before, only successes were recorded.
- **One HTTP status per kind of failure, the same on every endpoint**
  (design#3, disk#9). Errors are classified once (`GStoreError::kind()`,
  mapped by `OpErrorKind::of` and shared by REST, the upstream `/api` layer
  and the single-database server): a database directory or paged file
  another process holds (gserver, gquery, gconsole, a bulk build) is 409 on
  every REST endpoint (`/load` and `/upload` answered 500); an I/O error
  taking that lock (EACCES, a failed lock call) is 500 (409 on `/drop`,
  `/build`, `/restore` before); a store read error while a statement runs
  (contract C2) and a store broken by an earlier write error
  (`storage failed: …`) are 500 on REST `/sparql`, `/update`, `/tquery` and
  the single-database server (400 before); a statement cancelled by
  `/cancel`, `operationTaskCancel` or a shutdown is 503 with an
  `{"error":…}` body (400 before); `query_timeout` stays 503; syntax and
  evaluation errors stay 400; unclassified errors keep their status. The
  upstream `/api` keeps HTTP 200, its StatusCodes and messages.
- Library: `GStoreError::kind()` and `gstore::error::ErrorKind`
  (`#[non_exhaustive]`: `Parse`, `Query`, `Locked`, `Conflict`, `Corrupt`,
  `Storage`, `Cancelled`, `Timeout`, `Other`); the new variant
  `GStoreError::Locked` displays as `Database` did, so messages read as
  before; `DirLock::acquire`, `SideLock::acquire` and `BuildGuard::enter`
  return `Locked` for a lock held elsewhere and `Io` (with the path) when
  the lock cannot be taken at all; `store::read_scope::READ_FAILURE` and
  `read_failure()` name the statement read error (design#3).
- **REST `/export` writes every quad and runs as a cancellable, streamed
  task** (M13: http#11). It uses the `/api` export serializer: N-Quads
  (default graph, then every named graph) when the database has a non-empty
  named graph, else N-Triples; new `format=nt|nq` parameter (another value
  is 400), with Content-Type and attachment name (`<db>.nq` / `<db>.nt`) to
  match. Before, it silently dropped every named graph. In a transaction it
  exports the committed state, named graphs included. It is listed by
  `/tasklist` (operation `export`) and stopped by `/cancel` at its next
  statement (503 `{"error":"export cancelled"}`); it is written to an
  unnamed scratch file in the export directory under the shared guard and
  streamed in bounded chunks once the guard is released (it held a CONSTRUCT
  result and its N-Triples text in memory, guard included).
- **REST `/upload` that creates a database registers it** (http#16): it
  joins `system.json` with the uploading user as creator, and a registered
  name whose directory was removed outside the server loses its grants
  first, as REST `/build` and `/restore` do (a user granted on the vanished
  database could read the new one).
- **A failed REST `/build` keeps the old database's reasoning rules and
  ledgers** (http#10): a snapshot-layout rebuild removes the replaced
  database's `reason_rule_files` only once the new database is saved; a
  failed save serves the previous database again with its rules; a rebuild
  whose rule removal fails is registered but not served (500 naming the
  directory to remove).
- **REST and `/api` lifecycle operations agree** (design#4). `/api load`
  finishes a restore a crash left half-way (`<db>.db.restore-old` /
  `<db>.db.restore-new`) and loads it (it answered 1005 "does not exist");
  REST `/restore` registers a database it creates at the backup's time, as
  `/api restore` does; REST `/batchInsert` / `/batchRemove` report
  `success_num` and `failed_num` as `/api` does (REST's own `failed` field
  unchanged). REST `/backup` records `BACKUP_OF` only once the backup is
  complete — it first writes `BACKUP_OF.partial` — removes a directory it
  created when the backup fails, empties again an empty directory it wrote
  into and keeps the record of an earlier backup it was replacing; a
  directory holding a partial marker of the same database may be backed up
  into again, and REST `/restore` and `/api restore` refuse it (409 /
  "incomplete"); `/api backup` writes the marker too and its listing skips
  such directories.
- **`/api show` lists every database the registry finds** (gapi#15): it
  scanned the root with `/api`'s own name rule (three bytes at least), so a
  database REST `/build` or `gbuild` created as `ab` was missing; it now
  uses `sysdb::database_dirs` plus its storage check. `/api` operations on
  such a name still answer upstream's 1003.
- **The transaction log is rotated at 32 MiB and read incrementally**
  (gapi#18). An append that would take `logs/transaction.json` past
  `TXN_LOG_MAX_BYTES` moves it to `transaction.json.1` (hard-linked first,
  so the log is never missing) and starts a new file holding the records of
  the transactions still running; `txnlog` lists the current generation. A
  `txnlog` read parses only the lines appended since the previous one
  (before, every call parsed the whole file, about 8× its size in memory).
- **A torn last line of an API log no longer swallows the next record**
  (durability#13): an append to the transaction, query or access log first
  ends an unterminated last line, so the torn line stays one unparsable
  line, which readers skip (the first transaction record after a restart
  could be lost and its transaction listed as `RUNNING`).
- **`monitor` / `schema` of an unloaded database use cached statistics**
  (gapi#17): the first request reads the database; later ones walk its
  directory (the walk `diskUsed` needs anyway) and reuse the statistics
  while the files are the same (count, total size, latest modification and a
  digest of each file's path, inode, size and mtime). Statistics are cached
  only under the files they were read from. Before, every request opened the
  database, made a pass over its data and held its name busy, refusing a
  concurrent root `load` and a second `monitor` (1007).
- **Creating a database handle no longer holds the server-wide state lock
  during the directory walk** (http#13, conc#7): REST `/load`, `/build`,
  `/upload` into an unloaded or new name, `/restore`, the reopen of a broken
  store, a failed rebuild's re-serve and `/api` `load` / `restore` lock the
  database directory and measure its size before taking the lock; only
  inserting the handle runs under it, so `/status`, `/cancel`, logins and
  every other request no longer wait for a walk of a RocksDB store's
  thousands of files.
- **Keep-alive is decided with the response** (http#15): while connections
  wait for a worker, a response says `Connection: close` and the connection
  closes after it, instead of promising keep-alive and closing the idle
  connection a moment later while the client may be sending its next
  request; a connection that promised keep-alive yields its worker only
  after a last 1 ms look finds no request on its way.
- **`/cancel` and a shutdown stop CSR and substring-index builds** (conc#5,
  analytics#6): the CSR build of the graph functions (plain read, an
  UPDATE's WHERE clause, a transaction snapshot's read) and the
  substring-index build of a `CONTAINS`-style filter watch the request's
  cancel flag; a cancelled build caches nothing (it ran to completion, about
  0.3 s per million triples in Stream mode, and was cached after the
  cancel). `loadcsr` / `load csr=1` build the CSR as a registered,
  cancellable task listed by `/tasklist`.
- **A shutdown saves every free database first** (conc#5): `ApiServer`
  locked the databases in name order, so one a long read kept busy held up
  the save of every database after it; busy ones are now waited for after
  the others are saved, and the read, cancelled, stops at its next check.
- **Background statistics refreshes** (after a change, started by
  `/api monitor`) run their three passes each under a shared guard of its
  own, so a queued writer waits for one pass instead of the whole refresh; a
  refresh checks a cancel flag before each pass, is stopped and joined by a
  shutdown, and is dropped when the data changed between passes (conc#9).
- **No server thread outlives `serve_forever`** (conc#14):
  `Server::serve_forever` and `ApiServer::serve_forever[_tls]` join their
  connection workers (at most 2 s), the statistics refreshes, the
  cancel-bridge thread, the idle-transaction reaper (bounded at 10 s) and
  the asynchronous operations' runner and callback threads; only a runner
  whose operation ignored its cancel past `shutdown_wait`, or a callback
  still being delivered (bounded by `callback_timeout`), finishes on its
  own.

#### Behaviour changes: durability and storage

- **A failed commit of an on-disk database can no longer tear its file**
  (C-1: durability#1, disk#1). If a commit fails after its write-ahead log
  got its name (applying a page, an fsync, removing the log — say the disk
  filled up while the batch was applied), the pager is *failed*: every later
  write, page allocation and commit is refused with the new
  `GStoreError::StorageFailed`, reads keep working, and nothing writes the
  file again until the database is reopened; the reopen replays the complete
  log, so the file holds exactly the last batch whose log was complete.
  Before, the next commit point, save, shutdown or drop retried the commit,
  truncated the only complete log, failed again while rewriting it and left
  a database that no longer opened
  (`I/O error: failed to fill whole buffer`), data committed long before
  included.
- **Materialize mode breaks on a failed commit point** (durability#1,
  disk#1), as a failed write-through already did: every later commit point
  reports `storage failed: …` instead of retrying, and dropping the database
  commits nothing. In Stream mode a failing commit made by the out-of-core
  dictionary (a term write that outgrew the page cache) breaks the store
  too. Recover by reopening (server: `unload` + `load`, or just `load`).
- **A broken database can be unloaded and reloaded through the servers**
  (M5: gap-txn-lifecycle#4). `/api unload` and REST `/unload` of a database
  whose on-disk store failed retire it without a commit point and add a
  `warning` field (the failure and how many changes since the last
  successful commit point are discarded); `/api` keeps HTTP 200, StatusCode
  0, `"Database unloaded."`, REST answers 200. `load` of a loaded broken
  database reopens it in place (`/api`: "Database loaded successfully." +
  `warning`; REST: `database '…' reopened` + `warning`); a healthy loaded
  database keeps the upstream answer ("The database already load yet." /
  "already loaded") at once, without waiting for or reserving the database.
  REST `/build` over a loaded broken database replaces it instead of failing
  on the save. Before, unload answered 1005 / 500 "it stays loaded" and only
  a server restart recovered.
- **The write-ahead log is never truncated or rewritten in place**
  (durability#1, durability#8, robust#12): a commit stages `<file>.wal.tmp`,
  fsyncs it, renames it to `<file>.wal` and fsyncs the directory before the
  main file is touched, and fsyncs the directory again after removing the
  log. A failure before the rename changes nothing on disk and stays
  retryable; a leftover `*.wal.tmp` (a crash while staging) is not a commit
  and the next open removes it. WAL framing unchanged; older binaries ignore
  a leftover `*.wal.tmp`.
- **An unreadable write-ahead log fails the open** (M1: disk#2,
  durability#7, robust#12): only "not found" means "no log"; any other read
  error (EIO, EACCES, a directory in its place, …), or a log that cannot be
  removed after its replay, fails `Pager::open` and so the database open,
  instead of opening a possibly half-applied file whose log the next commit
  would overwrite.
- **A `kvstore.kv` (or `vstree.kv`) shorter than its header's page count**
  (page_count × 4096) is refused at open as torn or truncated instead of
  failing later reads (robust#12, disk#2).
- **A write applied in memory but not saved is never acknowledged** (H-7:
  durability#2, durability#18). A write whose check_point save failed (or,
  with check_point off, whose commit point failed) answered success with a
  `"warning"`; it now answers a failure with `"applied in memory but not
  durable: <cause>"`, keeping the operation's fields and the `warning` as
  detail. REST `/update`, `/sparql` UPDATE, `/upload`, `/batchInsert`,
  `/batchRemove`, `/commit`: HTTP 500
  `{"StatusCode":500,"StatusMsg":"applied in memory but not durable: …",…}`;
  single-database gserver (`/update`, `/sparql` UPDATE, also with
  `--no-checkpoint`): HTTP 500 `{"error":"applied in memory but not durable:
  …","changed":N,"warning":…}`; `/api` keeps HTTP 200 and answers StatusCode
  1005 for `query` updates, `commit` and `reasonManage` execute (as
  `batchInsert` already did). An UPDATE that fails part-way and whose
  applied part cannot be saved answers REST 500 and `/api` 1005 with the
  same message (new `OpErrorKind::Persist`); the REST query log records such
  a write with statusCode 500. The write stays applied in memory: on the
  snapshot layout the next successful save, `/checkpoint`, `/unload` or
  graceful shutdown saves it, a crash first loses it; on the on-disk layouts
  the database is broken until reopened, and `unload` or the reopen-in-place
  `load` discards those writes (the warning says so).
- **A commit whose commit point fails is rolled back, or answered as not
  durable** (M2: gap-txn-lifecycle#3, http#18, gap-rocksdb-exec#5).
  `Database::commit` dropped the undo log before the write-through, commit
  point and redo flush, so a Materialize-mode commit failing on a full disk
  was answered "commit failed" while its writes stayed visible and became
  durable at the next save, and a RocksDB commit whose undo journal could
  not be deleted stayed visible with its redo records logged. The undo log
  is now kept until the commit point succeeded. When it fails before
  anything of the transaction can have reached the store, the transaction is
  rolled back (RocksDB by replaying its journal) and nothing stays visible
  or becomes durable later; otherwise (log complete and replayed at the next
  open; RocksDB journal already deleted) the writes stay applied and the
  outcome says they are not durable. Only a committed transaction reaches
  the redo log, and the outcome is recorded only after the commit returned.
  HTTP: rolled back → REST 500 / `/api` 1005 `"commit failed and transaction
  N was rolled back: <cause>"` (`txn.log`: `commit-failed-rolled-back`); not
  undoable → REST 500 / `/api` 1005 `"applied in memory but not durable: the
  commit point of transaction N failed: …; the database needs a reload
  (unload, then load)"` (`commit-failed-not-durable`); both logged `ABORTED`
  in the `/api` transaction log, and a retried commit is told which.
  Library: `Database::commit_checked` → `Result<(), CommitFailure>`
  (`NotCommitted`, `RolledBack`, `NotDurable`, `CommittedWithError`);
  `Database::commit` keeps its signature.
- **An unreadable `named.bin` fails the open** (H-6: db#1, robust#4,
  durability#3, db#24). Every read error of a snapshot database's
  `named.bin` (truncation, a decode error, EIO, EACCES, EMFILE, a dangling
  symbolic link) read as "no named graphs", and the next check_point save or
  `gcompact --reclaim` overwrote the graphs with an empty map and freed
  their terms. `Database::open` / `load` / `read_copy` (so gserver loads,
  every CLI tool and `gcompact --reclaim`) now fail with an error naming the
  file; a missing `named.bin` (a database older than named graphs) still
  opens without named graphs; the legacy RocksDB migration refuses an
  unreadable `named.bin` before retiring any file.
- **RocksDB auto-commit statements are crash-atomic** (H-11:
  gap-rocksdb-exec#1). Every SPARQL UPDATE request (`INSERT DATA`, `DELETE`
  / `INSERT … WHERE`, `LOAD`, `CLEAR`, `DROP`, `ADD` / `COPY` / `MOVE`) and
  every `insert_sources` / `remove_sources` import (`gadd`, `gsub`,
  `/api batchInsert` / `batchRemove`) is journaled in the store's undo
  journal, so a crash part-way (SIGKILL, an OOM kill, power loss) is undone
  as a whole at the next open. A statement that fails part-way keeps what it
  applied (documented semantics) and its journal is committed, also when it
  panics (the servers answer 500 and keep serving); a failed `LOAD` stays
  "no change at all". Not covered (atomic per triple, a crash leaves a
  prefix): direct-API writes, REST `/batchInsert` / `/batchRemove` and REST
  `/upload` into a loaded database; on the disk layout a statement is
  crash-atomic only while its dirty pages fit the page cache (D12).
- **No orphan dictionary terms after a crash on RocksDB**
  (gap-rocksdb-exec#4): terms written new inside a transaction or a
  journaled statement are recorded in the journal in the same batch; a
  rollback replays the journal without journaling its own writes and deletes
  the terms it leaves unreferenced in the same batch as the journal. Older
  binaries replay the new term record as a no-op removal.
- **Crash recovery of a long RocksDB journal is bounded and resumable**
  (gap-rocksdb-exec#1, #4): the open replays the newest 65,536 records at a
  time and deletes exactly each chunk's records in one batch with its
  reclamation candidates, written to a pending range of the `meta` column
  family; a crash during recovery resumes where it stopped, and the
  `Database` settles pending candidates in batches of 65,536 before serving.
  Before, the open read the whole journal into memory and deleted it only
  after the full replay, so an open that ran out of memory on a 90M-record
  journal did so again at every open.
- **`check_point` no longer flushes RocksDB's memtables on every write**
  (H-12: gap-rocksdb-exec#2): a save at a RocksDB database's home (the
  servers' check_point save after every acknowledged write) is a commit
  point only, an fsync of RocksDB's WAL. Each save used to flush all seven
  column families, leaving about three table files per write, making writes
  5–13× slower and ending in EMFILE after a few hundred writes under
  `ulimit -n 1024`. Memtables are flushed by an explicit
  `Database::checkpoint` (`gcompact`), a backup and close.
- **RocksDB opens with a bounded number of files** (gap-rocksdb-exec#2): new
  key `[storage] rocksdb_max_open_files` (default 4096, `0` = unlimited,
  RocksDB's own default; `RocksTuning::max_open_files`); four old info logs
  are kept (`LOG.old.*`; each open added one and backups copied them all)
  and the WALs' total is bounded to 256 MiB. Options only, files unchanged.
- **A damaged RocksDB store is reported, never a panic** (disk#11,
  gap-rocksdb-exec#6): every decoded key's length is checked, and a key of
  the wrong length fails the statement with a read error naming the column
  family instead of a slice-index panic in a query, `gcompact --check`,
  `gexport` or an open with `vstree_on_open`. A dictionary id beyond its
  space fails the open ("corrupt RocksDB dictionary: entity id N is beyond
  its id space") instead of aborting the process on a 100 GB allocation
  (db#18).
- **`gcompact --check` checks RocksDB's statistics** (gap-rocksdb-exec#7):
  `Database::verify_integrity` runs the new `RocksStore::verify_stats`
  (every `stats` counter equals a recount from SPO; POS and OSP hold exactly
  the SPO triples; global, cached and mirrored counters agree; streamed,
  constant memory). A wrong, missing or stray counter passed `--check`, and
  `--reclaim` then freed terms the data still used.
- **A failed `gbuild --incremental --force` keeps the old database** (M12:
  durability#5). The incremental disk build deleted the old `kvstore.kv`
  (and every other layout) before importing, and the incremental RocksDB
  build (`create_rocksdb`, behind every `Database::build_rocksdb_*`)
  discarded the old store first, so a strict parse error, a full disk or a
  crash left an empty or unopenable database. The disk build now builds in
  `<db>.db/.bulk-tmp/` and installs with the bulk build's crash-safe
  install; the RocksDB build writes `rocksdb.tmp/` and swaps it in as a
  whole (a build stuck past its swap's commit point is refused, and the next
  open moves it into place). Both hold the in-process build guard and refuse
  a RocksDB store another process has open. `build_disk_incremental` /
  `build_disk_from_sources_incremental` now also remove the replaced
  database's `reason_rule_files/`.
- **Drops never leave a half-deleted database** (durability#10): `gdrop`,
  REST `/drop` and `/api drop` with `is_backup=false` rename `<name>.db` to
  `<name>.db.drop-old`, fsync the parent directory (from then on the
  database is gone), then delete it; a leftover is no database to any scan
  and the next drop of the name removes it. The `/api` logical deletion
  (`is_backup=true`) is durable too. New `Database::drop_dir(dir)`.
- **`gbackup` refuses a destination inside the database** (bulkcli#4):
  `gbackup x x.db` (also `./x.db/`, a directory inside it, a link to it)
  printed "Backed up", made no backup and deleted the database's pending
  `kvstore.kv.wal`. It is refused before anything is touched (exit 1),
  zipped backups too; `Database::backup_dir` refuses the same and takes the
  lock of a destination that holds a database.
- **`/api rename` is durable and reports registry and grant failures**
  (durability#14): the directory rename is fsynced before anything records
  the new name; a failure saving `system.json` or `users.db` (ignored
  before, silently losing the renamed database's grants) answers
  `1005 "Rename fail: …; nothing was renamed"` and is undone.
- **A save into another directory takes that directory's lock, whatever it
  holds** (db#11): two savers creating the same new database (two `gbuild`
  of one name, a build beside a gserver `/build`) both reported success over
  an unopenable directory. The target is now created and locked for every
  save not at home; a directory another process holds fails at once with the
  usual "is locked by another process" error and nothing is written, and
  within one process two saves into one directory are serialized. A new or
  empty save target and a backup directory written by `Database::backup`
  hold a `LOCK` file afterwards, and a zipped backup of one carries a `LOCK`
  entry (a few bytes; no restore path copies it).
- **`Database::load_disk` / `load_disk_with` take the directory lock** as
  `Database::open` does and hold it for the database's life (db#17): they
  open and write an on-disk database but took no lock, so an embedder beside
  a gserver had two writers on `kvstore.kv`.
- **A paged file is locked against other processes** (disk#9): `Pager::open`
  (so every `kvstore.kv` / `vstree.kv` open) takes an advisory exclusive
  lock before it looks at the WAL, held while any pager of the process on
  the file lives. An open from another process fails with `'…/kvstore.kv' is
  in use: another process has it open` (`GStoreError::Locked`, REST 409) and
  touches nothing (two processes could replay and delete each other's log
  and overwrite pages); a file system without advisory locks opens unlocked.
- **One WAL path rule** (disk#9): `Pager::wal_path(path)` is the only
  derivation of `<path>.wal`, and `Pager::remove_with_wal(path)` removes a
  paged file with its log (and a staged `.wal.tmp`), log first, with a
  directory fsync, ignoring only not-found; `Database::use_streaming_vstree`
  uses it, so a stale log that cannot be removed fails the call before
  anything is built.
- **The free-id format of a file is an explicit choice** (disk#8, design#6):
  an existing `kvstore.kv` keeps the format its header records whatever the
  process policy (`GSTORE_ID_RECLAIM`, `[system] id_reclaim`); only
  `gcompact --enable-reclaim` upgrades. The policy chooses the format of new
  files only (`BulkOptions::free_ids`, `OpenOptions::free_ids_if_new`).
- **A panic under the pager's write lock no longer makes every read panic**
  (conc#8): `Pager::lock_read` / `lock_write` recover a poisoned lock and
  mark the pager failed, so reads go on and every later write and commit is
  refused with `StorageFailed`.
- **The out-of-core VS-tree validates what it reads** (disk#4, disk#15).
  `VsTree::open_disk` on a missing path is a `NotFound` error and creates
  nothing, and an empty file or a header naming a root outside the file or
  more entities than it can hold is refused as corrupt ("VS-tree file … is
  corrupt … (remove it to rebuild the index)"); a missing `vstree.kv` is
  rebuilt or skipped as before. A scan checks every node it visits (kind
  byte, at most 32 entries, child ids inside the file and below their
  parent, node signature equal to the union of its entries and to its
  parent's entry, one leaf depth, at most 8 levels, no more visits than the
  file has nodes); a violation is a corruption error (`ErrorKind::Corrupt`)
  and the tree then answers "no filtering", which is sound. Before, an entry
  count above 32 panicked, a cycle overflowed the stack, a redirected child
  or a signature that lost bits hid matches, and a 5-page file could answer
  1,048,576 candidates.
- **A corrupt VList is a read error** (robust#11): `DiskStore::try_o_by_sp`
  reports a compacted `sp2o_vlist` record that does not decode as
  `InvalidData` ("corrupt kvstore: …"), so the statement fails (contract C2)
  instead of `<s> <p> ?o` silently having no values; `vlist::decode_u32s`
  refuses a count its bytes cannot hold before allocating (`u32::MAX` asked
  for 16 GiB), and an overflow record longer than the file can carry, or a
  chain ending before its length, is refused.
- **New directories are created durably at every acknowledged commit point**
  (tests-b#3, db#12). The disk and RocksDB bulk builds (`gbuild`,
  `Database::build_disk`, `gstore::bulk::build`),
  `Database::build_rocksdb_from_str` / `build_rocksdb_from_files`,
  `Database::backup_dir`, REST `/upload` into a new name, the servers'
  backups and exports, a user's PFN directories, a restore's new parent, a
  server's new root directory, saves into a new directory and snapshot
  writes used `create_dir_all` and synced only the new directory, never the
  parent naming it, so a power cut after the success answer could lose the
  whole database, backup or function. All use `fsutil::create_dir_durable`
  (the parent of every created directory is fsynced); builds and saves into
  an existing non-home directory sync its parent too
  (`fsutil::ensure_dir_durable`), and `gbuild` claims its directory with
  `BuildGuard::create_and_lock`. `reason_rule_files/` and its `inferred/`
  ledger directory are fsynced into their parents when created, so a rule's
  ledger is durable before its rule file and the insert it records.
- **The redo log and the update log are crash-consistent** (durability#6,
  db#8; library API only — `Database::enable_redo_log*`,
  `enable_update_log*`, `replay_*`, `backup_incremental`,
  `restore_incremental`; no server or CLI turns them on). The records of one
  commit (an auto-commit UPDATE, a transaction, a direct-API mutation, a
  `materialize_rdfs` / `run_rules` run) are appended with one write — an
  UPDATE's or a transaction's after its commit point, then `fdatasync`ed; a
  direct-API mutation when applied, synced by the next commit point
  (`Database::sync()` is now also the logs' commit point); direct-API batch
  calls (`insert_data`, `try_insert_data`, `delete_data`, `insert_sources`,
  `remove_sources`) are one commit. Before: four unbuffered writes and an
  open / close per triple (a 50-triple `INSERT DATA` opened `redo.log` 50
  times), nothing synced, and an UPDATE logged before its commit point. The
  update log takes committed statements only (a rolled-back transaction's
  statements used to be replayed).
- **Redo record framing** (durability#6): every record carries a CRC-32
  header token `c=<8 hex>`, the last record of a commit `e=<8 hex>`
  (`REDO I 0 44 e=1a2b3c4d`), and replay applies a commit only once it read
  its `e=` record; records without the token (every log written before)
  replay as before, each a commit of its own, and older readers ignore the
  token. An incomplete last commit is skipped with a warning by
  `replay_redo_log` / `replay_update_log` and cut off by the next append or
  backup (the next session used to append after the torn bytes, making every
  later record unreachable); a damaged record in the middle is corruption
  (`redo.log is corrupt at byte N: …`), never skipped. An increment's
  `incr.log` must replay whole.
- **A failed log append is reported and nothing is lost** (robust#10,
  durability#6, db#2): an append failing (ENOSPC, EFBIG, a failed fdatasync)
  is cut off at once and its records stay owed, in order, to the next
  append. An auto-commit UPDATE whose records could not be appended returns
  an error ("the update is applied and committed, but appending to redo.log
  '…' failed: …"), `commit()` reports `CommitFailure::CommittedWithError`,
  `sync()` reports it, and `backup_incremental` refuses while commits are
  owed; an unreadable log is an error for `backup` / `backup_incremental`.
  Committed records survive a later `rollback()` and `disable_redo_log` /
  `disable_update_log`. After a failed commit point the logs follow what a
  reopen keeps; when that is in doubt they are marked so and
  `backup_incremental` refuses until a full `backup()`, which starts a new
  chain. `backup_incremental` walks the copied range as the restore does and
  fails on a damaged commit (it copied it and answered `Ok`).
- **Redo replay refuses what it cannot read** (db#9, gap-doc-claims#1): a
  length field beyond `usize` or an overflowing sum fails `replay_redo_log`,
  `replay_update_log` and `restore_incremental` (it panicked); an op other
  than `I`, `D`, `C`, `N` is refused before its body is read
  (`unknown redo.log op 'X' at byte N`), as are an `I` / `D` record without
  a triple and a malformed clear record. `CLEAR` / `DROP` / `MOVE` / `COPY`
  records now carry the placeholder body
  `<urn:x-gstore:redo> <urn:x-gstore:redo#op> "clear" .`, which pre-Phase-6
  readers refuse with "unknown redo.log op 'C'"; the empty-bodied clear
  records written until now still replay here (and are skipped silently by
  pre-Phase-6 binaries — do not downgrade across one).
- **`restore_incremental` never writes the base backup** (db#13): a snapshot
  base is read in place without taking its lock; a disk or RocksDB base is
  copied into a scratch directory created exclusively beside it
  (`<base>.restore-tmp-<pid>-<n>-<nonce>`, mode 0700; the system temporary
  directory only when the base's parent is not writable), opened there and
  removed, so a base on read-only media can be restored with about its size
  of free space. `Database::backup` inside an open transaction is refused
  (db#14): it captured uncommitted writes no later increment undid.
- **`update.log` files written by earlier versions replay** (sparqlconf#16,
  parser#13): a record is read as the version that logged it did — a
  labelled blank node in `DELETE DATA` / `DELETE WHERE` / a DELETE template
  names the stored node, `CREATE` / `DROP` / `CLEAR GRAPH` are `SILENT`, and
  the older grammar leniencies are accepted; the first record that still
  fails stops the replay with its error.

#### Behaviour changes: SPARQL

- **A failed non-`SILENT` `SERVICE` fails the query** (robust#6, tests-b#2,
  sec#17). A variable endpoint, an inner pattern the request cannot carry
  (anything but triple patterns, groups and `UNION`), a connection, HTTP or
  JSON error used to make the `SERVICE` yield no solutions (200 with no
  rows, or under `OPTIONAL` every remote variable unbound). As SPARQL 1.1
  Federated Query §4 requires, the statement now fails with a query error
  naming the endpoint and the cause (`query error:
  SERVICE <http://host/sparql>: endpoint returned HTTP 500`,
  `… bad JSON: …`, `SERVICE ?ep: a variable endpoint is not supported …`);
  `SERVICE SILENT` is unchanged (the one empty solution). Clients that
  relied on the empty answer add `SILENT`. Once a `SERVICE` has failed the
  query no further request is sent.
- **A sub-SELECT parses as a whole group in every position** (M14: parser#3,
  sparqlconf#5): `WHERE { SELECT … }`, `ASK { SELECT … }`, `OPTIONAL` /
  `MINUS` / `GRAPH ?g` / `SERVICE` / `FILTER (NOT) EXISTS { SELECT … }`,
  `CONSTRUCT` / `DESCRIBE` / `INSERT` / `DELETE … WHERE { SELECT … }` and
  `{ SELECT … } UNION { … }` failed ("found 'SELECT'"); only
  `{ { SELECT … } }` was honoured. A pattern after the sub-query inside the
  same braces stays a syntax error, whose message now says to nest the
  sub-query as a group of its own. A direct `{ SELECT … }` costs one nesting
  level, so twice as many sub-queries can nest (127 direct levels).
- **Negated property sets walk the right direction** (H-16: query#2,
  sparqlconf#3). `!(p1|…|^q1|…)` is evaluated as SPARQL 1.1 §18.2.2.3
  translates it, `alt(NPS({p…}), inv(NPS({q…})))`. Before, a set walked
  backwards (the default for `?x !:p :o`) banned nothing (`?x !:knows :a`
  returned the very subject linked by `:knows`) and inverse members never
  walked reversed edges; `!^p`, `!(p|^q)`, `^(!p)` and `!a` now give the
  spec's answers from a constant subject, a constant object and with both
  ends free, on every layout and in both executors.
- **Sequence and alternative paths are multisets** (M14: query#9,
  sparqlconf#4). `X P/Q Y` is a join through a fresh variable and `P|Q` a
  union (§18.4): `:x :p/:q ?y` with two intermediate nodes has two
  solutions, a node reached by both alternatives of `:p|:r` comes twice, and
  `COUNT(*)` counts every route; `*`, `+`, `?` still return each node once.
  A walk keeps the multiset as counts and hands the copies out lazily, so
  `/` and `|` cost the distinct nodes of each step and `ASK`, `EXISTS` and
  `LIMIT` stop early. Every copy past a pair's first is charged to a
  per-query budget (`Evaluator::with_path_max_duplicates`, default
  `PATH_MAX_DUPLICATES` = 4,194,304, no configuration key); a query that
  would hand out more fails with a query error ("a property path of this
  query has more than … repeated solutions …"). Where only existence or
  distinctness matters (`ASK`, `EXISTS`, `SELECT DISTINCT` without
  aggregates or per-solution functions) each pair is handed out once and
  nothing is charged.
- **Zero-length paths match constants that are not in the data** (M14:
  sparqlconf#13): `<unknown> :p* ?x` and `<unknown> :p? ?x` bind `?x` to
  `<unknown>`, `ASK { <u> :p* <u> }` is true, also for a predicate IRI or a
  literal in no subject / object position (§18.4). A variable end point
  bound by another pattern still matches only the active graph's nodes at
  length zero.
- **Property-path solutions come in a fixed order** (gap-accel-soundness#1):
  reached nodes follow the stores' sorted lists, closures breadth first in
  discovery order, instead of hash sets with per-evaluation random keys, so
  `LIMIT` / `OFFSET` pages of a path query tile its solutions and `ORDER BY`
  ties over path solutions repeat from run to run. This closes the Phase 3
  known gap about property-path solutions coming out of hash sets.
- **A path whose end point another pattern binds is evaluated from the bound
  values** (sparqlconf#9; answers unchanged, cost and plan changed). In a
  group such as `?s :id 2990 . ?s :next* ?x` a path with two variable end
  points ran from every node of the graph and was joined afterwards (4.5
  million path steps, 1.7 s, 290 MB for a 10-row answer on a 3,000-node
  chain); the group's BGPs are now joined smallest and connected first and
  the path extends every solution from the bound value (10 path steps on
  3,000 or 6,000 nodes), also on the right side of an `OPTIONAL` whose left
  side binds the end point. Both executors follow the same plan and give the
  same rows in the same order.
- **`GRAPH ?g { P }` joins `?g` with P's own binding of it** (H-17: query#3,
  sparqlconf#1). Both executors set `?g` to each named graph's name over
  whatever P had bound it to, so `SELECT ?g ?t WHERE { GRAPH ?g { ?g
  :created ?t } }` returned a row for every subject and `INSERT { ?g :meta
  ?t } WHERE { GRAPH ?g { ?g :created ?t } }` wrote wrong triples durably. A
  solution is now kept only when it leaves `?g` unbound or binds it to that
  graph (§18.5), also with `?g` in predicate position.
- **`EXISTS` / `NOT EXISTS` honour the current solution inside `VALUES`
  blocks and sub-SELECTs** (query#4): `?s :p ?o FILTER EXISTS { VALUES ?s {
  :a } }` kept every `?s`, and a sub-SELECT's projected `?s` was accepted
  whatever it bound. A variable the substituted pattern can still bind (a
  `VALUES` variable, a sub-SELECT projection, a `BIND` target) must now
  agree with the current solution; a sub-SELECT's unprojected variables stay
  independent (§12).
- **SELECT expressions extend the solution left to right, each evaluated
  once** (query#6, sparqlconf#6): without `ORDER BY` an expression could not
  see an earlier alias (`SELECT (1 AS ?a) (?a + 1 AS ?b) {}` left `?b`
  unbound); with `ORDER BY` every expression was evaluated a second time at
  projection (`(RAND() AS ?r) … ORDER BY ?r` printed unsorted values). All
  paths (materializing, streaming `LIMIT`, bounded heap, DP-B) now evaluate
  each expression once per solution, seeing exactly the aliases to its left.
- **`BNODE()` and `BNODE(str)` are fresh blank nodes** (H-18: query#5).
  `BNODE()` minted `_:b0`, `_:b1`, … from a counter that restarted with
  every process, so two processes or a restarted server inserting
  `BIND(BNODE() AS ?b)` wrote the same stored node and merged two resources
  for good; it now mints the engine's fresh labels
  (`genid-<per-process nonce>-<counter>`). `BNODE(str)` was
  `_:b<hash of the string>`, one node for every solution, query and process;
  it is now one fresh node per string within a solution mapping (§17.4.2.9),
  each solution of a multiset (a repeated `VALUES` row, a path reached by
  two routes) getting its own. Only labels change; nothing on disk.
- **A labelled blank node in a CONSTRUCT / INSERT template is a fresh node
  per solution** (query#5; Query §16.2.1, Update §3.1.3), the same across
  the template triples of one solution; it was copied verbatim, so every
  solution and request wrote the one stored node `_:v`. A CONSTRUCT whose
  template holds one is no longer served from the query cache. Labelled
  blank nodes in WHERE patterns and `INSERT DATA` are unchanged.
- **`UUID()` / `STRUUID()` / `RAND()` are safe under concurrency** (M10:
  conc#1). They shared one process-wide xorshift state advanced by a
  non-atomic load-then-store, so about 0.5% of concurrent `STRUUID()` calls
  returned a duplicate; `UUID()` / `STRUUID()` now take 16 bytes of OS
  randomness each and format a version-4 UUID, and `RAND()` draws from a
  per-query `xoshiro256**` generator seeded from OS randomness.
- **`NOW()` is one instant per query** (query#16, §17.4.5.1), shared with
  `GRAPH` blocks; every `Evaluator::evaluate` / `eval_update_modify` call is
  a new execution with its own instant and `BNODE` nodes.
- **`REGEX` and `REPLACE` follow XPath `fn:matches` / `fn:replace`** (M14:
  query#11, sparqlconf#8). Flags `s`, `m`, `i`, `x` and `q` are honoured
  (only `i` was); any other flag character is an error (FORX0001), so
  `REGEX(?x, "a", "u")` filters the row out instead of ignoring the flag.
  `REPLACE`'s replacement uses XPath syntax: `$N` with XPath's digit rule
  (`$1x` is group 1 then `x`), `\$` / `\\` for a literal `$` / backslash, a
  group above the count up to 9 is the empty string, a lone `$` or backslash
  is an error (FORX0004), with `q` the replacement is literal; a pattern
  matching the empty string is an error (FORX0003; `REPLACE("abc", "", "-")`
  was `"-a-b-c-"`). Pattern syntax stays the regex crate's (no
  back-references, class subtraction or `\p{IsBlock}`).
- **`MIN` / `MAX` use the ORDER BY order and return the stored term** (M14:
  query#8, sparqlconf#7): they used the partial `<`, so for an IRI and a
  literal, or a number and a string, the first value seen won
  (plan-dependent). They now pick by the ORDER BY order, equal to the first
  / last row of ORDER BY; `MIN`, `MAX` and `SAMPLE` return the group's term
  as stored and `GROUP_CONCAT` joins the terms' lexical forms
  (`"5"^^xsd:int` came back `xsd:integer`, `"01"^^xsd:integer` as `"1"`).
- **`AVG` divides by the number of values it summed** (query#7): `SUM` /
  `AVG` skip non-numeric values (a documented extension), but `AVG` divided
  by the number of all values — `AVG` over 10 and `"x"` was 5.0, now 10.0. A
  group with no numeric value averages `0` (`xsd:integer`, §18.5.1.4).
- **`DISTINCT` in `COUNT` / `SUM` / `AVG` / `GROUP_CONCAT` compares RDF
  terms, in linear time** (query#12, sparqlconf#10): it compared values
  pairwise with `Vec::contains` (80,000 distinct values took 13 s; a million
  minutes to hours of uncancellable CPU), and `"01"^^xsd:integer` and
  `"1"^^xsd:integer` counted as one; they now count as two (a Phase 3 known
  gap).
- **SPARQL UPDATE refuses the literals the tolerant loader refuses**
  (sparqlconf#12): `INSERT DATA` and the instantiated `INSERT` templates of
  `DELETE` / `INSERT … WHERE` apply the loader's numeric check
  (`parser::import::upstream_literal_check`): `"x"^^xsd:integer`,
  `"300"^^xsd:byte`, an `xsd:integer` beyond 64 bits or a `NaN` double fails
  the operation before any of it is applied ("cannot insert the literal …: …
  (the loader refuses it, so it would not load back from an export)").
  Before, UPDATE stored them, `gexport` wrote them and a tolerant `gbuild`
  dropped them. A strict import still stores such a literal as written; an
  `INSERT … WHERE` copying one into a template fails as a whole. Not covered
  (open): REST `POST /batchInsert` and `Database::insert_triple`.
- **`IRI()` / `URI()`, `STRLANG()` and `STRDT()` validate their results**
  (M9: sparqlconf#11): a string with a space, a control character or one of
  `` <>"{}|^`\ `` is no IRI (an expression error, so the variable stays
  unbound and an INSERT template skips the triple), `STRLANG()` requires a
  LANGTAG (`STRLANG("x", "en> bad")`, `STRLANG("x", "")` are errors),
  `STRDT()`'s datatype given as a string must be IRIREF text, and a
  user-defined function returning such an IRI or tag fails.
- **N-Triples output escapes every term** (M9: sparqlconf#11):
  `Term::to_ntriples` / `Triple::to_ntriples` is the one serializer for
  CONSTRUCT / DESCRIBE responses (`/sparql`, REST and `/api`), `gquery`,
  gconsole's local mode, the TSV and `n-triple-file` formats and the exports
  (`gexport`, `/api export`, `Database::export_ntriples`). It escapes an
  IRI's excluded characters and a literal's control characters and makes a
  legacy label or tag valid, so a stored term can no longer end its line and
  forge a statement — `CONSTRUCT` over `IRI(CONCAT(…))` printed a forged
  triple with no write access needed. Well-formed terms print as before;
  `Term`'s `Display` (the stored form) is unchanged.
- **Result serializers decode each cell as a stored term** (query#15): JSON,
  XML, CSV, TSV and upstream's `/api` JSON shapes read each cell with
  `Term::from_dict_key` first, so an IRI a lenient reader stored with `>` in
  it is `"type":"uri"` (it became a literal). XML stays well-formed XML 1.0:
  a carriage return is written `&#xD;`, a tab or newline in an attribute
  `&#x9;` / `&#xA;`, and a character XML 1.0 cannot carry is written as
  U+FFFD; the JSON format carries such values exactly.
- **`sameTerm` follows the store's term identity** (sparqlconf#14), the one
  BGP matching, joins, `DISTINCT` and `GROUP BY` use:
  `sameTerm("abc", "abc"^^xsd:string)` is now false, as the store keeps two
  terms; language tags compare as written.
- **Substring filters over typed literals no longer lose rows** (query#1,
  gap-accel-soundness#2). The dictionary substring index pruning `CONTAINS`
  / `STRSTARTS` / `STRENDS` / plain `REGEX` held each term under its value's
  canonical form while `STR(?v)` compares the lexical form:
  `FILTER(CONTAINS(STR(?c), "001"))` missed `"0012"^^xsd:integer`, and
  leading-zero decimals, signed and derived integers, exponent floats and
  padded or numeric booleans were missed on every layout with an in-memory
  dictionary. A term whose two forms differ is indexed under both.
- **Graph management follows one existence rule** (sparqlconf#16; SPARQL 1.1
  Update §3.1.5, §3.2): a named graph exists while it holds a triple, the
  default graph always. `CREATE GRAPH <g>` of a graph that exists, and
  `DROP GRAPH <g>` / `CLEAR GRAPH <g>` of one that does not, now fail with a
  request error (`CREATE GRAPH failed: the graph <g> already exists (it
  holds triples); use CREATE SILENT GRAPH …`), as `COPY` / `MOVE` / `ADD`
  from a missing source already did; the failing operation changes nothing
  and ends the request (earlier operations stay). `SILENT` makes each a
  no-op; `DEFAULT`, `NAMED`, `ALL` never fail; `ADD` / `MOVE` / `COPY` of a
  graph onto itself stays a no-op. Before, these reported 0 changed triples.
- **Blank nodes are refused at parse time in `DELETE DATA`, `DELETE WHERE`
  and DELETE templates** (sparqlconf#16, parser#13), the labelled ones
  (`_:b`) too (only `[]` was refused). `INSERT DATA`, INSERT templates and
  WHERE patterns keep them, and a labelled blank node in a WHERE pattern
  still names the stored node, so the triples of one stored blank node are
  deleted with `DELETE { <a> <p> ?o } WHERE { <a> <p> ?o FILTER(sameTerm(?o,
  _:x)) }` (`_:x` being the label a query result shows). Clients deleting
  the upstream way (`DELETE DATA { <a> <p> _:x }`) get a parse error; no
  switch restores it (a conformance change with no security side). A
  reasoning rule's retraction is unaffected.
- **The SPARQL grammar's structure is enforced** where a lenient reading
  changed the request (parser#13): `PREFIX foaf:knows <…>` is a syntax error
  (it silently declared `foaf:`); two triple statements need a `.` between
  them (`{ ?s ?p ?o ?a ?b ?c }` was two patterns; stray `.` tokens are still
  skipped); `ORDER BY`, `LIMIT` and `OFFSET` come at most once each,
  `ORDER BY` first (`LIMIT 1 LIMIT 2` was `LIMIT 2`), `LIMIT` and `OFFSET`
  in either order.
- **`SECONDS` returns an exact `xsd:decimal`** (sparqlconf#16, §17.4.5.7):
  `SECONDS("…T14:45:13.815-05:00"^^xsd:dateTime)` is
  `"13.815"^^xsd:decimal`, whole seconds `"7"^^xsd:decimal` (it returned
  `xsd:integer` or an `xsd:double` with a binary fraction); on a value
  without a time it is an error, not `0`.
- **A non-grouped variable in an aggregate query's projection is a query
  error** (sparqlconf#15, §11.4): with `GROUP BY`, or an aggregate in SELECT
  / `HAVING` / `ORDER BY`, a WHERE variable may be projected, or used
  outside an aggregate in a SELECT expression, only as a GROUP BY key.
  `SELECT ?s ?o … GROUP BY ?s` showed each group's first solution; it now
  fails with `?o is not grouped: … group by ?o or project (SAMPLE(?o) AS
  ?…)`. GROUP BY keys (also `GROUP BY (expr AS ?k)`), aggregates, earlier
  aliases, trailing `VALUES` variables, variables the pattern never binds
  and a sub-expression equal to a GROUP BY expression stay legal;
  sub-SELECTs follow the same rule.
- **String and hash functions check their argument types** (sparqlconf#17):
  `STRLEN`, `SUBSTR`, `UCASE`, `LCASE`, `CONTAINS`, `STRSTARTS`, `STRENDS`,
  `STRBEFORE`, `STRAFTER`, `CONCAT`, `REGEX`, `REPLACE`, `STRLANG` and `MD5`
  / `SHA1` / `SHA256` / `SHA384` / `SHA512` read the lexical form of any
  term before (`STRLEN(1)` was 1, `REGEX(<iri>, "x")` true, `SHA1("abc"@en)`
  a hash). Now per SPARQL 1.1: a string literal where the spec says so
  (§17.4.3.1.1), a simple literal for the regex pattern, flags and
  replacement, both `STRLANG` arguments and the hash input, and argument
  compatibility (§17.4.3.1.2) for `CONTAINS` / `STRSTARTS` / `STRENDS` /
  `STRBEFORE` / `STRAFTER`; any other argument is an error (unbound in a
  SELECT expression, false in a FILTER). Write `STRLEN(STR(?v))`,
  `REGEX(STR(?iri), …)` for the old reading. Still lenient:
  `ENCODE_FOR_URI`, `LANGMATCHES`, `STRDT`'s lexical form, the gStore
  extensions (`CONTAINALL`, `CONTAINANY`, `SIMILARITY`) and `SUBSTR`
  positions of any numeric type. This resolves the Phase 3 known gap on
  argument compatibility.
- **`BOUND` of a variable the pattern never binds is false**, and `BOUND` of
  a substituted term is true (query#14): it was an error, so
  `FILTER(!BOUND(?z))` dropped every row and
  `FILTER EXISTS { … FILTER(BOUND(?outer)) }` was always false.

#### Behaviour changes: RDF parsers

- **One term validator for every reader** (H-13: parser#1;
  `gstore::model::term`): RDF/XML, JSON-LD, RDFa, N-Triples, N-Quads, Turtle
  / TriG, the SPARQL lexer and the term built-ins. A language tag is LANGTAG
  (`[a-zA-Z]+(-[a-zA-Z0-9]+)*`). Where a format writes the tag as an
  attribute or string value (RDF/XML `xml:lang`, JSON-LD `@language`, RDFa
  `lang` / `xml:lang`), surrounding whitespace is dropped and `_` reads as
  `-` (`zh_CN` is stored as `zh-CN`), an empty value means no language
  (JSON-LD used to store `"x"@`), and any other value fails the document
  (`invalid language tag "en US" …`). In N-Triples, N-Quads, Turtle and
  SPARQL only LANGTAG is a tag: `"x"@`, `"x"@-`, `"x"@-en`, `"x"@en-` and
  `"x"@1en` are syntax errors.
- **Blank-node labels outside BLANK_NODE_LABEL are stored hashed**
  (parser#1): a JSON-LD `"_:a b"`, an RDF/XML `rdf:nodeID` ending in `.` or
  an RDFa `_:` CURIE holding a control character is stored as `h<16 hex
  digits>-<label with every other character as _>`, the digits being a
  stable FNV-1a hash of the whole label, so one label stays one node and
  `a b` / `a_b` stay two.
- **N-Triples / N-Quads: a trailing `.` ends the statement, not the
  blank-node label** (parser#6): `_:g.` and `_:g .` are the same node or
  graph, as in Turtle / TriG, and the W3C `minimal_whitespace` form
  `_:s<http://p>_:o.` reads `_:o` (a minimal-whitespace dump split a blank
  node or named graph in two).
- **One RFC 3986 resolver for every RDF reader** (parser#4;
  `parser::iri::resolve`, §5.2): Turtle / TriG (`@base`, `BASE`), RDF/XML
  (`xml:base`, `rdf:about`, `rdf:ID`, `rdf:resource`, `rdf:datatype`),
  JSON-LD (`@base`, `@id`) and RDFa (`<base href>`, `xml:base`, `about` /
  `resource` / `href` / `src`). With base `http://a/b/c/d;p?q`, RDF/XML,
  JSON-LD and RDFa gave `http://a/b/c/../g` for `../g`, `http://a/b/c///g`
  for `//g` and `http://a/b/c/?y` for `?y`; now `http://a/b/g`, `http://g`
  and `http://a/b/c/d;p?y`, as Turtle. A relative `xml:base`, a nested
  JSON-LD `@base` or an RDFa `xml:base` resolves against the base in force;
  an empty RDFa `about` / `resource` names the base. Databases loaded before
  keep their stored IRIs; rebuild to fix them. (SPARQL's `BASE` still
  concatenates.)
- **RDF/XML literals keep their data** (parser#9): a literal property
  element's value is its text exactly (§7.2.16; it was trimmed);
  `rdf:parseType="Literal"` gives an `rdf:XMLLiteral` whose value is the
  content as exclusive canonical XML with comments (§7.2.17; only text
  children were kept), written against the document's expansion budget; an
  empty property element with property attributes (§7.2.21) has
  `rdf:resource`, `rdf:nodeID` or a fresh blank node as object, described by
  the attributes (they were dropped).
- **RDF/XML: `xmlns:p` declarations on any element are read** (parser#7
  area): a declaration on a node or property element (not `rdf:RDF`) failed
  the whole document with "undefined namespace prefix 'xmlns:'".
- **Turtle / TriG numbers follow the grammar** (parser#10): `1.e5` and
  `123.E+1` are `xsd:double` (refused before; a tolerant import stored
  `"1"^^xsd:integer`); `1e`, `1E+`, `1e-`, `1.5e`, `1.5.5`, `1e5e5`, `1e5.5`
  fail their statement (stored as numbers silently); a `.` followed by a
  digit never ends a statement.
- **Prefixes with inner dots** (parser#11): `@prefix ex.v2: <…>` (and
  `PREFIX`) declares a prefix that names like `ex.v2:s` can use (PN_PREFIX);
  every statement using it failed. A prefix that starts or ends with `.` or
  holds a term separator is refused.
- **Invalid UTF-8 in a Turtle / TriG comment is an error of the comment's
  line** ("invalid UTF-8 in a comment") and the statements around it load
  (parser#12); the next statement used to be dropped and reported.
- **New limit: one token of a streaming RDF reader** (parser#5): a Turtle /
  TriG string, IRI, name, number or language tag, or one N-Triples / N-Quads
  line, is capped by `[upload] rdf_max_token_size` (default 64 MiB, `0` = no
  limit; `ImportOptions::max_token_bytes`,
  `StatementSink::max_token_bytes`). A longer token fails its statement once
  the reader has held the limit's worth; the rest is skipped without being
  held and a tolerant import continues. Before, an unterminated `"""` string
  or a newline-free line was held whole; the N-Triples / N-Quads scanner now
  reads the line in place (it copied it into a `Vec<char>`, six times the
  line).
- **New limit: one SPARQL request in gserver** (parser#8):
  `[upload] sparql_max_size` (default 16 MiB, `0` = no limit; installed by
  gserver and gbuild, also on `refreshconf`) refuses a longer query or
  update before it is tokenized ("the request is N bytes long, more than the
  limit of M bytes ([upload] sparql_max_size)"). Upstream has no limit;
  processes that install no configuration (`gquery`, gconsole, library
  callers, gnode) have none. The tokenizer lexes in place (it copied the
  request into a `Vec<char>` plus an offset table, 12 bytes per request
  byte).

#### Behaviour changes: command-line tools and configuration

| key | default | effect |
|---|---|---|
| `[server] min_body_rate` | 65536 | bytes/s a request body must average after `read_timeout`; `0` = per-read timeout only |
| `[server] max_conns_per_ip` | 64 | open connections per client address; `0` = no limit |
| `[server] cors_allow_origin`, `allowed_hosts`, `allow_get_update`, `trust_proxy_headers`, `allow_default_root_password`, `upstream_login_lockout` | secure (off / empty) | upstream-compat switches, see above |
| `[server] login_lockout_failures` / `login_lockout_max_seconds` | 4 / 900 | failures before a lock; longest lock |
| `[server] query_timeout` | 3600 | seconds a statement may evaluate; `0` = off |
| `[server] max_result_bytes` | 1073741824 | bytes of one answer; `0` = off |
| `[system] txn_idle_timeout` | 600 | seconds before an idle transaction is rolled back; `0` = never |
| `[upload] upload_max_body_size` | 268435456 | body cap of an authenticated payload request (was 1 GiB) |
| `[upload] preauth_max_body_size` | 67108864 | body cap before authentication |
| `[upload] request_body_budget` | 536870912 | bytes of all request bodies in memory |
| `[upload] anonymous_download` | off | upstream-compat switch |
| `[upload] download_max_concurrent` | 8 | concurrent streamed downloads; `0` = no limit |
| `[upload] file_retention_hours` | 168 | lifetime of recorded uploads, exports, query results; `0` = forever |
| `[upload] upload_user_quota` | 0 | bytes of retained uploads per user; `0` = no quota |
| `[upload] rdf_max_token_size` | 67108864 | one token of a streaming RDF reader; `0` = no limit |
| `[upload] sparql_max_size` | 16777216 | one SPARQL request in gserver; `0` = no limit |
| `[pfn] pfn_fuel_per_query` | 10000000000 | fuel of all PFN calls of one request; `0` = unlimited |
| `[storage] rocksdb_max_open_files` | 4096 | RocksDB's open files; `0` = unlimited |
| `gserver --allow-local-hosts` | off | with `--allow-service`: SERVICE and remote downloads may reach host-local addresses |

- **`gbuild` exits with status 3 when the build completed but tolerant mode
  skipped statements** (sparqlconf#12): each skipped statement is still
  listed in `parse_error.log` and `RDF parse error num N` still printed; it
  exited 0, so a scripted `gexport` → `gbuild` migration could lose data
  unnoticed. Statuses: 0 built, 1 failed, 2 invalid arguments, 3 built with
  statements skipped (also `gbuild --rocksdb`, bulk and `--incremental`).
  `gadd` still exits 0 when it skipped statements.
- **`gserver` answers invalid arguments with exit status 2** (bulkcli#18),
  as every other tool does (it was 1, which a script or a service manager
  could not tell from a failed start), and **a gserver built without the
  `tls` feature refuses `--tls-cert` / `--tls-key`** with an argument error
  (http#19), where it printed a warning and served plain HTTP.
- **A bulk build keeps the rest of its process out of the directory it
  rebuilds** (bulkcli#13). The bulk and incremental builds checked "open in
  this process?" and then took the re-entrant directory lock in a second
  step, so another thread (a REST `/load` of the `foo.db` alias while
  `/build?db_name=foo` ran) shared the build's lock and its acknowledged
  writes went to files the install then replaced. New `BuildGuard::lock(dir)
  -> (BuildGuard, DirLock)` claims the directory and takes its lock in one
  step; while the claim lives, `DirLock::acquire` (every open, save, drop or
  restore of that directory) on any other thread is refused as a lock
  conflict ("… is being rebuilt by another bulk build of this process",
  `ErrorKind::Locked`, REST 409).
- **`gbuild` claims the database directory before it writes anything**
  (bulkcli#2): it decided once at startup that a new directory was its own
  and removed it on any failure without its lock, so of two `gbuild` of one
  new name the loser deleted the winner's directory. `gbuild` now creates
  and claims the directory before the parse error log is reset or a package
  unpacked, removes a directory it created only under that claim and only
  while it holds no database, fails at once with status 1 when another
  process holds the directory, and without `--force` refuses a name another
  process created between its check and its claim.
- **`gbuild` keeps a database it built when its summary cannot be printed**
  (bulkcli#3): `gbuild newdb data.nt > /dev/full` panicked (status 101) and
  deleted `newdb.db`; it now keeps it and exits 1 ("built '…', but its
  summary could not be printed").
- **Zip packages are unpacked into fresh, unpredictable scratch
  directories** (bulkcli#6): `gbuild` / `gadd` / `gsub` unpacked into
  `<db>.db.unzip-tmp-<i>`, removing whatever was there (and `gbuild` before
  taking any lock, wiping a concurrent `gadd`'s scratch); each unpack now
  creates `<db>.db.unzip-tmp-<pid>-<random>` exclusively (mode 0700;
  `zipio::ScratchDir::create_unique`).
- **The zip entry limit holds before the central directory is decoded**
  (bulkcli#5): every central-directory record (about 190 bytes per entry)
  was decoded before `unzip_max_entries` was compared. The
  end-of-central-directory records are now read first, and a package any of
  whose usable records declares more entries than the limit is refused
  (`ZipCode` 3); a zip64 end record that is not where its locator says, or
  fails the consistency checks, is refused as unsupported (code 1).
- **`gquery -f` and `gexport` replace their output only with a complete
  one** (bulkcli#10, bulkcli#11): `gquery -f FILE` truncated FILE before
  opening the database, and `gexport` wrote in place, so a failure destroyed
  an existing file. Both now write `<dest>.part-<pid>-<n>` (created
  exclusively, private `0600` until complete), sync it and rename it over
  the destination; a failure removes it. A symbolic link stays a link, a
  replaced file keeps its permission bits and, where possible, its owner,
  and a FIFO, a device or a dangling link is written in place. `gexport -z`
  and `zipio::pack_file` / `pack_dir` write their archives the same way
  (`gexport -z` went through a fixed `<archive>.part`, following a planted
  link); new `zipio::pack_file_to`.
- **conf.ini's inline-comment rule** (bulkcli#9): an unquoted value was cut
  at any whitespace + `#` / `;`, so `root_password = Tr0ub4dor #3` silently
  set `Tr0ub4dor`. A `#` or `;` now starts an inline comment only when it
  stands as a word of its own (whitespace before, whitespace or end of line
  after) after some of the value (`port=9000  # http` reads `9000`); a
  marker glued to more text stays in the value, as upstream reads it;
  nothing inside double quotes is a comment; `root_password` is never cut;
  every cut is a startup warning. For earlier gstore-rust files:
  `root_password="s3cret"  ; the root password` now reads the whole text (it
  only seeds a new user store), and `db_home=/srv/gstore #primary` reads
  `/srv/gstore #primary` — each such value is reported at startup with its
  line, key and kept text. gserver also warns about a configured
  `ip_allow_path` / `ip_deny_path` it cannot read. Upstream files, which
  have no inline comments, parse as before. New `Ini::inline_comments()`,
  `IpFilter::list_file_warnings`.
- **Settings that cannot take effect are reported** (design#11, bulkcli#8):
  `resource_check=on` (gstore-rust performs no resource check) and a
  `build_threads` other than 1 under `build_multi_thread=off` are startup
  warnings. The shipped `conf/conf.ini.example` no longer forces
  single-threaded bulk builds (it copied upstream's
  `build_multi_thread=off`; existing installations keep their `conf.ini`).
  `gbuild` reports `conf.ini` warnings and read errors on standard error
  (`gbuild: conf.ini warning: …`) and loads the file once (bulkcli#7).
- **`GStoreConfig::db_path` names the directory the servers use**
  (design#11): `<db_home>/<name>.db`, no longer
  `<db_home>/<name><db_suffix>`, which no server opened; new
  `GStoreConfig::db_root()`.
- **The bulk build checks its page ranges in release builds** (bulkcli#12):
  a POS / OSP / statistics / dictionary writer that took more or fewer pages
  than reserved fails the build before its install, the old database
  untouched (a `debug_assert` before).
- **`GSTORE_BULK_CRASH` announces itself** (tests-a#19): every bulk install
  prints what the crash-test failpoint will do, or that its value names no
  install step and is ignored.

#### Behaviour changes: extensions (graph functions, PFN, reasoning, `ConcurrentDb`, gconsole)

- **`gfun_max_output_bytes` is a per-query budget** (gap-gfun-fuzz#5): every
  advanced graph-function and `PFN.name` call of a query is charged against
  one shared counter (each `GROUP BY` group and calls inside `GRAPH` blocks
  and sub-SELECTs too), and each call runs bounded by what is left; a query
  passing it fails with `<function>: more than N bytes of output in this
  query (…)`. Before, the bound applied per call: 2000 groups of a 22 KB
  `WCC({})` produced 88 MB of rows. A graph function whose vertex arguments
  name no variable is computed once per query under `GROUP BY`.
- **`PR`, `louvain` and `labelProp` stop at an iteration-work bound**
  (gap-gfun-fuzz#2): `PR` with `tol < 0` and `louvain` with `increase < 0`
  never converge and ran up to 2^31 sweeps (about 45 minutes on a 3-vertex
  graph). Every sweep beyond the first `gfun::FREE_ITERATION_SWEEPS` = 1024
  is charged with the vertices, edges and comparisons it visits against
  `gfun::MAX_ITERATION_WORK` = 2^34 steps; past it the call fails ("not
  converged within the iteration bound of 17179869184 steps …; lower maxIter
  or use a positive tol"). Every `maxIter` up to 1024 still runs to the end;
  library users set the bound per call with
  `GFunCtx::with_max_iteration_work`.
- **Graph functions stop at `/cancel` from inside their long loops**
  (analytics#8, gap-gfun-fuzz#3): WCC polled only between components,
  triangle counting / clusterCoeff / labelProp per 1024 vertices whatever
  their degree, closeness / betweenness / degreeCorrelation /
  diameterEstimation / confidenceDegree never inside a BFS, the kplex
  `Searcher` never in its O(n²) initialisation. A weighted ticker charges
  each vertex expansion with its degree and polls every 4096 units; the
  legacy `rs:` built-ins and `SIMILARITY` are cancellable too.
- **`maximumKplex`, `coreTruss`, `confidenceDegree` and `louvain` stop at
  the output bound while they build their text** (gap-gfun-fuzz#4):
  `maximumKplex({}, 65536)` with a 200-byte IRI for vertex 0 peaked at 52 MB
  before failing at a 1 MiB bound; it now fails having allocated about the
  bound.
- **`rs:LOUVAIN` is deterministic** (analytics#3): equal modularity gains
  were resolved by `HashMap` iteration order (12 runs gave four partitions);
  ties go to the smallest community id, in `GraphView::louvain_directed`
  too.
- **PFN: a per-request fuel budget, and instantiation is charged** (ext#1).
  New `[pfn] pfn_fuel_per_query` (`PfnLimits::fuel_per_query`, default
  10^10, ten calls' worth of the default `pfn_fuel_per_call`; `0` =
  unlimited) bounds the fuel all PFN calls of one request use together
  (every argument combination, sub-queries, `GRAPH` blocks and all
  operations of one UPDATE); past it `run f fail: the instruction budget of
  N for the PFN calls of one query (pfn_fuel_per_query) is exhausted`.
  Before, one query could make 1,000,000 calls of a billion instructions
  each. A call's instantiation is charged before it runs — one unit per
  8 bytes of the module's initial linear memory and data section — and
  refused before any memory is allocated when it does not fit (a module
  declaring 1024 pages costs 8 Mi units per call). API: `PfnCaller` counts
  fuel (clones share the count; `for_query()`, `fuel_spent()`,
  `limited(limits)`), `CallOptions::spent`.
- **PFN compile builds what `data.json` records** (ext#8): `funcudb` type 4
  / `PfnRegistry::compile` compiles `generate_source(record)` and rewrites
  the `.wat` source file when it differs, so an update interrupted between
  the source file and `data.json` can no longer make a function run code
  `funquery` does not show.
- **RDFS materialization never makes a literal or blank node a predicate**
  (ext#3; `Database::materialize_rdfs`, library API): a literal or blank
  node in an `rdfs:subPropertyOf` (or `domain` / `range`) position
  (`:p rdfs:subPropertyOf [ owl:inverseOf :q ]`) was interned as a predicate
  and data copied under it, which no reader loads back (export → gbuild lost
  statements, exit 3). Only IRIs are properties now; `subPropertyOf` stays
  transitive through a blank node.
- **Textual rules are strict** (ext#4, ext#7; `Database::add_rule` /
  `RuleSet`): non-ASCII or malformed rule text is an error (the tokenizer
  sliced by bytes and panicked on `é <p> ?y => …`); terms go through the
  crate's term validator and `Term::from_dict_key` instead of a forked
  parser, so a rule IRI with a space or `` <>"{}|^`\ ``, a literal escape
  other than `\\ \" \n \r \t`, an invalid language tag, blank label or
  variable name is refused. The body join is a hash join over dictionary ids
  (a two-hop rule over 8000 edges: 20 ms instead of 2.6 s). Textual rules
  are documented as not persisted.
- **Rule directories are written all-or-nothing** (durability#11):
  `reason::store::copy_rule_dir` (backups, restores) and
  `write_memory_rules` stage the new `reason_rule_files/` beside the
  destination, fsync every file and directory and swap it in with
  `fsutil::StagedDir`; a removal renames the directory aside durably first.
  They removed the destination and wrote file by file without fsync, so a
  crash or a full disk lost the old rules and left rule files without
  ledgers (which `reasonManage` takes for upstream rules). An interrupted
  swap is finished by the next operation taking the rule lock;
  `write_memory_rules`, and a detached database's `save` that writes
  in-memory rules, take that lock without waiting and are refused, changing
  nothing, while another operation holds it.
- **A forced rebuild removes the replaced database's rules, also an
  interrupted swap's staged copy** (durability#11): `gbuild --force` deleted
  only `reason_rule_files/`, without the rule lock, so a
  `reason_rule_files.new` left by an interrupted swap brought the old rules
  back at the rebuilt database's first rule operation. New
  `reason::store::remove_rule_dir` / `remove_rule_dir_locked` (rule lock,
  rename aside, then delete), used by `gbuild`, the bulk install and REST
  `/build`.
- **`ConcurrentDb` (library only) publishes exactly what the database
  holds** (gap-txn-lifecycle#3, conc#4, http#9, robust#13): a commit the
  database rolled back publishes nothing (a version-pinned reader saw a
  rolled-back transaction's writes; `apply_txn_ops` asks
  `Database::commit_checked`); an UPDATE that fails part-way is published
  and recorded as a non-transactional write when it changed the database, so
  an overlapping `RepeatableRead` / `Serializable` transaction conflicts
  (readers kept the old snapshot and the change surfaced later inside an
  unrelated version).
- **`ConcurrentDb`: no lock stays poisoned and no double panic** (conc#3,
  http#17): one panicking `write()` closure made every later `update` /
  `sync` / `Txn::commit` panic with `PoisonError`, and a panic inside a
  commit made `Txn::drop` panic again during the unwind (process abort). A
  panicking `write()` closure now has its open database transaction rolled
  back, what it applied otherwise published, the locks released and the
  panic continued; a commit that panics while applying rolls back; poisoned
  locks are recovered (every transaction live at the panic conflicts); `Txn`
  and `VersionView` drops never panic.
- **`gconsole --local` keeps the updates it acknowledged** (ext#5): a
  snapshot-layout or new in-memory database was persisted only by a `save`,
  so leaving it (`quit`, end of input, `load <other>`) dropped acknowledged
  updates. The console now saves before leaving a changed database
  (`saved to <dir>`); when that save fails, `load <other>` keeps the current
  database, an interactive `quit` stays (a second `quit` leaves), and at the
  end of input the failure is reported (exit 1). Ctrl-C no longer ends the
  local console: as in the remote console it discards the statement being
  typed. On-disk and RocksDB databases write each update through. A console
  killed by `SIGTERM`, `SIGHUP` or `SIGKILL` still does not save.
- **`gconsole --local` ends statements like the remote console** (ext#9): a
  `;` ends a statement only outside `{…}` groups, string literals, `<IRI>`s
  and `#` comments (a multi-line SELECT with a `;` predicate-object list, a
  long literal whose first line ends in `;` and two statements on one line
  failed); a script's final statement may lack its `;` (it was dropped
  silently).
- **gconsole logs in once and sends a session, not the password** (ext#12):
  `POST /login`, then the session token (`Authorization: Bearer`) with every
  `/api` request, so the password crosses the network once per login; an
  ended session (`1001`) is renewed and the request repeated once, and the
  console logs out at the end. Against an upstream gserver or an older
  gstore-rust one it falls back to upstream's `/api login` and the
  per-request password.
- **`gconsole --tls [--ca-file FILE]`** (feature `tls`; ext#12) talks to a
  gserver serving HTTPS over rustls, verifying the server certificate
  against `--ca-file`, `$SSL_CERT_FILE` or the system bundle (an unvouched
  certificate is a connection failure and nothing is sent; the handshake is
  bounded at 30 s). Without the feature `--tls` is refused (exit 2), as is
  `--ca-file` without `--tls`. A plain-HTTP connection to a non-loopback
  host warns that the password and every statement cross the network in
  clear text.
- **`setpriv` validates its answer and changes privileges as one step**
  (ext#11): an answer that is not a list of privilege numbers (a typo such
  as `q`, an out-of-range number, an empty line) is refused and nothing is
  sent (it revoked every privilege of the user on the database and printed
  `Privilege set successfully.`); the console grants what is missing first,
  then revokes what is left over, and takes the grant back when the revoke
  fails.
- **Server text cannot drive the operator's terminal** (ext#10): every
  server field gconsole prints (`StatusMsg`, table cells, `backup` /
  `export` paths, counts and times, login refusals, non-envelope answers)
  has its control characters escaped (`\u001B` …).
- **A Ctrl-C typed at a gconsole prompt is never lost** (ext#13,
  tests-a#14): the Ctrl-C mark is taken before the prompt is shown and the
  reader waits for stdin with `poll` (100 ms), checking the counter before
  each wait.

#### Behaviour changes: experimental cluster and gnlqa

- **`gnode cluster <id> <bind_addr> [peer_id@addr …]` requires
  `--state-dir DIR` or `--ephemeral`** (cluster-nlqa#11). Every replica used
  to be ephemeral: one that voted in term T and restarted came back at term
  0 and voted again in term T (two leaders of one term), and with an empty
  log could help elect a leader lacking committed writes. Shard mode is
  unchanged.
- **New `ClusterNode::bind_persistent(id, peers, state_dir, addr, cfg)`**
  (cluster-nlqa#11): term, vote and log changes go to an append-only,
  CRC-checked journal `state_dir/raft.journal` (magic `GSRAFTJ1` and the
  owning node id), fsynced before the node answers a vote or append, sends a
  message or counts its own entry toward a commit; a restart reloads them
  and rebuilds the triple store as the log is re-committed. A torn tail is
  cut off; an intact record that does not decode, or a damaged record a
  crash cannot have left, refuses the start naming the byte offset and
  leaves the file untouched. A directory of another node id or a second
  running copy of the node (advisory lock) is refused; a journal write
  failure stops the node for good (fail-stop). A single-node leader commits
  its log when elected.
- **Forged Raft terms are refused** (cluster-nlqa#2): one `RequestVote` at
  term `u64::MAX` wedged a cluster for good (a panic in debug builds, a wrap
  to 0 in release). A message may move a node's term forward by at most
  2^32; elections use checked arithmetic; a lying `AppendAck` match index is
  clamped to the leader's log; an `InstallSnapshot` beyond index 2^62 is
  refused.
- **`ShardedRaftCluster` reports only committed writes** (cluster-nlqa#4,
  cluster-nlqa#12): `insert` / `remove` return `Some(true)` only once the
  write is committed and `None` when it could not be committed now (retry is
  safe); `insert_all` counts committed triples only. New
  `ReplicaGroup::commit(op) -> Result<index, ProposeError>`.
- **A `ClusterNode` routes shard inserts through the Raft log and refuses
  `PurgeShard`** (cluster-nlqa#3): `Request::Insert` answers `Bool` once
  committed (as `RemoteShard::insert` expects) and `Error` with the leader
  hint otherwise; `PurgeShard`, which deleted a bucket on one replica
  outside the log and diverged the replicas for good, is refused.
- **`ShardCluster` membership changes are resumable and never lose data**
  (cluster-nlqa#1, cluster-nlqa#10): a migration that fails part way leaves
  its buckets stranded (new `ShardCluster::stranded_buckets`) and returns
  the error; repeating the change, or new `ShardCluster::resume_migrations`,
  finishes them, and any other membership change is refused while one stays
  stranded. `remove_node` drains every bucket of the leaving node and
  refuses to remove the last member while it holds triples; nothing is
  purged before the copy reached its target; re-adding a member never
  replaces its endpoint. Before, a failed join's retry was a silent no-op
  and a later leave dropped the stranded triples (128 of 400 in the review's
  run), every call returning `Ok`.
- **Shard-node connections time out and frames are read as they arrive**
  (cluster-nlqa#9, sec#19, conc#11, robust#15): `ShardNode` bounds each read
  and write (`ShardNode::DEFAULT_IO_TIMEOUT` = 60 s, `with_io_timeout`);
  `rpc::read_message` grows its buffer with the bytes received instead of
  allocating the size an unauthenticated length prefix announces (up to
  256 MiB for four bytes); a reply larger than one frame is answered with
  `Response::Error` naming its size (`rpc::encode_reply`) and the connection
  stays usable.
- **`gnlqa serve` requires a client token** (cluster-nlqa#13, sec#18):
  `/ask` and `/gSolve` run LLM-chosen read-only SPARQL with the server's
  gStore credentials and spend the operator's LLM credit, so `gnlqa serve`
  refuses to start unless `GNLQA_HTTP_TOKEN` is set (clients send
  `Authorization: Bearer <token>`, compared in constant time). The only
  alternative, `GNLQA_HTTP_ALLOW_UNAUTHENTICATED=on`, answers loopback
  clients only, refuses a non-loopback bind and prints a startup warning. In
  the library `HttpServer::bind` starts in `HttpAuth::Unconfigured` (401
  with `WWW-Authenticate: Bearer`) until `with_token`,
  `allow_unauthenticated_loopback` or `with_auth`. `/health` stays open.
- **gnlqa rate, size and cost limits** (cluster-nlqa#13, sec#18): each
  client (an IPv6 client per /64) may send `GNLQA_HTTP_RATE_PER_MIN` (30)
  question requests per minute, counted before authentication, and gets 429
  with `Retry-After` beyond; a question over `GNLQA_MAX_QUESTION_BYTES`
  (4096) gets 413; each request runs under a cost budget (`gnlqa::budget`,
  `CostLimits`) of `GNLQA_MAX_LLM_CALLS` (16) LLM calls, `GNLQA_MAX_KB_ROWS`
  (200000) KB rows and `GNLQA_REQUEST_TIMEOUT_SECS` (120 s), the clients'
  network timeouts capped by the time left; an exhausted budget is the new
  `Error::Limit` (`/ask` 503, `/gSolve` `status:"error"`, code
  `limit_exceeded`). MCP tool calls run under the same budget. Connections
  are capped per client at an eighth of the total (8 of 64;
  `HttpServer::with_max_conn_per_client`), so one address holding idle
  connections no longer turns every other client away with 503. The limiter
  tracks at most 10000 clients without ever forgetting one being limited.
- **gnlqa errors are redacted** (cluster-nlqa#18): `/ask`, `/gSolve` and MCP
  tool errors no longer echo internal error text (ureq printed the endpoint
  URL with its `user:password@`; provider bodies were passed through);
  clients get a generic message, a stable `code` (`upstream_unavailable`,
  `kb_error`, `llm_error`, `no_answer`, `limit_exceeded`, `config_error`)
  and a `request_id`, and the full error goes to stderr under that id; URL
  userinfo is stripped from every error and log line.
- **gnlqa reads no configuration from the working directory**
  (cluster-nlqa#15): the config file is `$GNLQA_CONFIG`, else
  `$XDG_CONFIG_HOME/gnlqa/gnlqa.conf`, else `~/.config/gnlqa/gnlqa.conf`; a
  `gnlqa.conf` in the current directory is ignored with a note
  (`GNLQA_CONFIG=./gnlqa.conf` opts in) and a world-writable config file is
  refused. Its clients follow no redirects (ureq kept every header but
  `Authorization` / `Cookie` across hosts, so Anthropic's `x-api-key`
  followed a `302` anywhere), and a credential sent over plain `http://` to
  a non-loopback host draws a startup warning.
- **gnlqa honours its privacy table** (cluster-nlqa#14, cluster-nlqa#21):
  the follow-up rewrite replays an earlier answer to the LLM only where the
  current mode allows that data to leave the machine (a default `auto` chat
  and a `structured` → `open` switch sent locally computed KB answers), and
  structured mode with `with_explain(true)` no longer sends the answer's
  supporting triples.
- **gnlqa retries transient failures and surfaces outages**
  (cluster-nlqa#16): LLM and embedding clients retry 429 / 500 / 502 / 503 /
  504 / 529, `GStoreClient` 429 / 502 / 503 / 504, with exponential back-off
  from 500 ms honouring `Retry-After` (capped at 30 s and by the request
  budget; `GNLQA_RETRIES`, default 3). A persisting provider or KB outage
  ends the request instead of becoming an ungrounded guess; the linker is
  built lazily and retried after a failure (at most once a minute in
  `gnlqa`).
- **gnlqa results over the 100000-row cap are never silently truncated**
  (cluster-nlqa#17): `KbClient::query` / `GStoreClient::query` fail with a
  repairable error asking for a `LIMIT`; `query_capped` returns the first
  rows and a truncation flag (MCP `run_sparql` appends `[truncated]`).
- **gnlqa bounds and hardening** (cluster-nlqa#19, conc#10, cluster-nlqa#20,
  cluster-nlqa#23, cluster-nlqa#22): the answer cache is bounded by
  estimated bytes (`GNLQA_CACHE_MAX_BYTES`, 64 MiB) and
  `GNLQA_CACHE_TTL_SECS` defaults to 3600 (was 0, never expire); a handler
  panic is answered 500 and releases its connection slot (`max_conn` panics
  meant a permanent 503) and the read deadline bounds the socket read
  timeout; the read-only gate refuses `SERVICE` anywhere in a query; the
  linker index is fetched in ordered pages of 10,000 and cached on disk per
  KB fingerprint for a day (0600 files trusted only when owned by the
  process's user; `GNLQA_LINKER_MAX_LABELS`, `GNLQA_LINKER_CACHE`,
  `GNLQA_LINKER_CACHE_DIR`), so a repeated one-shot `gnlqa "<question>"`
  costs two `COUNT` queries instead of full-store `DISTINCT` scans.
- gnlqa library: the `Error::Limit` variant and new `Config` fields break
  exhaustive matches and struct literals in downstream code (none in the
  workspace); `GStoreClient::query` now errors instead of truncating beyond
  100000 rows; `SolveEngine::linker()` may run a deferred build. Nothing was
  removed.

#### Behaviour changes: tests, CI, gstore-bench and packaging

- **The smoke gate sees sub-millisecond regressions** (infra#2, infra#4).
  The per-query timing gate's floor drops from 2 ms to 0.1 ms
  (`(m + 0.1 ms) / (b + 0.1 ms) > 3` fails); the per-backend query geomean
  takes every query as `(m + 0.05 ms) / (b + 0.05 ms)`, each ratio clamped
  to [1/3, 3], instead of only queries with a baseline median of at least
  1 ms, and its limit is `max(1.5, s)` with `s` the baseline run's own
  process spread (`gates::query_spread`), never above `geomean.max_spread`
  1.6. 41 of the smoke baseline's 68 queries answer in 0.01–0.7 ms; the
  review's repro (all regressed to 3 ms, 4–298×) passed all 456 gates
  before. Any single query slowed 50× now fails and the canary fails at 2×
  as well as 3×. `bench/thresholds.json` gains `geomean.floor_ms`,
  `geomean.max_spread` and a `bless` section (files without them keep their
  old meaning).
- **gstore-bench checks how a run was measured** (infra#10): a new exact
  gate `exact.config` requires the baseline run's builder, warm repetitions
  and query processes (`check` takes them from the baseline unless given);
  `bless` refuses (exit 1) a result from a debug build of the harness, a
  dirty tree or no recorded commit, the library builder or a debug `gbuild`,
  a 1-minute load above 1.0 per CPU, or query processes spread wider than
  `geomean.max_spread`, unless `--provisional` marks it; `ci` refuses
  (exit 2) a provisional baseline unless `--allow-provisional`.
- **gstore-bench arguments are validated** (infra#16, infra#5):
  `--conc-secs` and `--mixed-secs` must be finite, positive and at most
  3600, `--conc-clients` at least 1, and a mixed point may not need more
  than 100,000 writes (`--mixed-secs 1e9` asked for 480 GB up front and the
  phase child panicked); stray positional arguments are usage errors (exit
  2; `check --from X other.json` gated X against the default baseline).
- **gstore-bench's update phase detects lost writes** (infra#9): it
  checkpoints and reopens while the update chunk is inserted and re-runs
  every query on the database reopened after the final checkpoint; new
  optional `UpdateResult` fields record it (older result files read as
  before).
- **gstore-bench tests fail on a missing, stale or RocksDB-less `gbuild`**
  (infra#18) instead of silently using the library builder
  (`GSTORE_BENCH_TEST_BUILDER=lib` chooses it explicitly);
  `cargo test -p gstore-bench` alone now needs `cargo build --profile test
  -p gstore --bin gbuild [--features rocksdb]` first. Reports match sections
  by workload (infra#8).
- **CI** (infra#2, infra#6, infra#8, infra#13): the smoke job and the test
  job check the generated tables of every tracked Markdown file with a
  marker (`docs/BENCHMARKS.md` and `bench/README.md` were never checked);
  the canary runs at 2× and 3×; the weekly reference job records the
  reference profile as the Phase 6 closure did and compares with
  `bench/results/phase6-final-*.json`; ci.yml's table check runs
  `target/debug/gstore-bench` instead of compiling gstore a second time.
- **`scripts/conformance.sh` fails when a suite is skipped** (infra#11): the
  Python SDK suite, which needs python3's `requests`, counted as passed when
  skipped; it is now `SKIP` and the run exits 1 unless `--allow-skip`. The
  server picks its own port (tests-b#17): `conf/conf.ini` says `port=0` and
  the script writes the bound port from the pid file into `conf/conf.ini`
  for the upstream scripts (the port came from a probe socket closed before
  gserver bound it).
- **systemd unit hardening** (infra#14): `UMask=0027`, an empty
  `CapabilityBoundingSet`, `ProtectKernelLogs`, `ProtectClock`,
  `ProtectHostname`, `ProtectProc=invisible`, `RestrictRealtime`,
  `RestrictNamespaces`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`,
  `MemoryDenyWriteExecute`, `RemoveIPC`, `SystemCallArchitectures=native`,
  `SystemCallFilter=@system-service` without `@privileged @resources`;
  `systemd-analyze security --offline` rates the unit 1.2 (was 6.8). Files
  gserver creates are no longer world-readable, a port below 1024 needs a
  drop-in granting `CAP_NET_BIND_SERVICE`, and `ProtectHome=yes` (unchanged,
  now documented) hides `/home`, `/root` and `/run/user`. A crash loop ends
  (infra#12): `StartLimitIntervalSec=300`, `StartLimitBurst=5` and
  `RestartPreventExitStatus=1 2`, so a refused configuration is not
  restarted forever.
- **Packages** (infra#15, infra#7, infra#19, infra#17): the deb keeps an
  administrator's enablement across `apt remove` + `apt install`
  (dh_installsystemd's protocol: prerm only stops, postrm masks on remove
  and forgets on purge, postinst unmasks and re-enables; `DPKG_ROOT`
  honoured); the rpm installs `conf.ini` with group `gstore` from a new
  `%pre` that creates the user and group first (`rpm -V` reported a
  mismatch); the image and both packages ship every binary target
  (`cargo install --path .`, the deb's `$auto`, a checked rpm list) and are
  built with `pfn-wasm`; gnlqa's `rust-version` is the workspace MSRV 1.89.
  The container health check is `/usr/local/bin/gstore-healthcheck`
  (`gserver -S` with PID 1's arguments: it connects to the address the
  server bound, without a request), so another port, TLS or an IP allow list
  no longer makes the container unhealthy and probes leave no access-log
  record; the image no longer installs `curl`.

#### Fixes

Crashes and denial of service:

- **A hostile SERVICE reply can no longer abort the process** (H-3: sec#5,
  gapi#3, robust#1; gapi#10, robust#8): replies are read with the crate's
  depth-limited `json::parse` instead of a private recursive reader (about
  50 KB of `[` overflowed an 8 MiB worker stack, SIGABRT); escaped surrogate
  pairs decode to the right non-BMP character (two U+FFFD before) and a lone
  surrogate makes the reply malformed. `SERVICE` requests name the
  endpoint's port in `Host:` when it is not 80 (sec#6).
- **`shortestPathCount` cannot blow up on wrapped path counts** (H-8:
  analytics#1): once a count wrapped to exactly 0 (2^32 paths, a 2-wide
  ladder of 33+ levels) the frontier doubled every level and a 400-triple
  ladder aborted the process on a failed allocation; the search tests the
  maps' stamps and the printed count is still upstream's wrapping C `int`
  value byte for byte.
- **The SPARQL lexer is linear on long name runs** (H-14: parser#2):
  `a-a-a-…` / `a.a.a…` without a `:` re-scanned the rest of the run for
  every word (160 KB about 6 s, 1.6 MB about ten minutes of uncancellable
  CPU per request).
- **Log retention skips foreign file names** (M15: bulkcli#1): a file such
  as `diag.log.<CJK text>` next to the log made gserver exit with 101 at
  startup (and the daily roll of `access.log` / `query.log` panic in the
  first request of a day).
- `maximumKplex` on a graph without vertices answers the empty k-plex for
  every k (gap-gfun-fuzz#1; k ≤ 2 panicked, k = 3 printed
  `{"kplex":[,,,]}`). REST log lines (`access.log`, `query.log`, `txn.log`)
  are written with one `write(2)` and no longer interleave (conc#6).

Storage and durability:

- **Quad-aware frees** (disk#3): a term used only by a named-graph quad's
  subject, predicate or object was freed and its id reused, silently
  changing the quad; the store keeps reference counts of the quads' terms.
  Out-of-core dictionary caches never cache a value read before a free after
  it (disk#6, conc#12, db#20; a racing resolve could cache a freed id's
  string, or make `intern` answer a freed id for a second term).
  `remove_sorted_from` skips a value the list does not hold in release
  builds too (db#19).
- **A backup never pairs a new `kvstore.kv` with an old `vstree.kv`**
  (durability#4): the destination's VS-tree and WALs are removed durably
  before the new store lands, `backup_dir` removes stale files the source
  lacks first and copies `meta.bin` last, and a stale destination `rocksdb/`
  is discarded first (`GSTORE_VS_POLICY=tree` pruned every entity added
  since the previous backup).
- **The legacy-shadow reconcile survives a crash part-way** (durability#9):
  the decision to apply a shadow is recorded first in a durable marker
  `SHADOW.apply` bound to the store file's device and inode, and an open
  that finds it applies the shadow again before retiring it (an open after a
  second crash served a mix).
- Developers: the crate-internal `fsutil` has one stage + swap + fsync
  primitive for replacing and removing directories, `StagedDir` (staged
  beside its target, the whole tree fsynced, swapped in with the parent
  fsynced; `InstallError::committed` tells a failure after the commit point
  apart), used by the incremental builds, drops, backups and rule
  directories (durability#15); crash-test hooks `GSTORE_DROP_PARK` and
  `GSTORE_SHADOW_PARK=<n>`.
- **The directory-lock registry is keyed by the locked file** (db#10): a
  directory moved away with its lock lent its in-process entry to a new
  directory at the old path (`restore_into` an open directory left the
  restored database unlocked); the registry is keyed by the `LOCK` file's
  device and inode, and `DirLock::holds(dir)` is new.
- **Redo log and incremental backups replay every stored term** (parser#1):
  a record names the stored term exactly (`model::term::record_key`); the
  first record holding `"x"@zh_CN`, `"x"@`, `"x"@en US` or `_:a b` failed
  the replay and no later increment could be restored. Records of
  well-formed terms are byte-identical.
- `materialize_rdfs` and `run_rules` log the triples they add (db#3);
  `backup` / `backup_incremental` copy the reasoning rules before taking
  `persist_lock` (contract C8 order) and take the target directory's lock
  first (conc#13); a backup's checkpoint moves only once its `BACKUP_OFFSET`
  marker is written.
- **Commits let readers run and checksum faster** (disk#10):
  `DiskStore::commit` writes, applies and fsyncs the batch under the pager's
  shared guard (`Pager::commit_shared`), only taking the batch and marking
  it clean hold the exclusive guard; the CRC-32 is table-driven (slicing
  by 8) and the log is streamed through a 1 MiB buffer; log format
  unchanged.
- `VsTree::build_disk` and `VsTree::write_disk_sorted` replace an existing
  file and its write-ahead log (disk#5; three builds of 5,000 entities grew
  the file to 2,007,040 bytes and a stale `.wal` was replayed into it);
  `dict::prefix::decode_block` bounds its allocation (db#21; a `u32::MAX`
  count asked for 96 GiB).
- **Per-predicate triple counts saturate instead of wrapping**
  (gap-accel-soundness#4, bulkcli#14): the disk `pred_stats` record stays a
  `u32` (no format change) and stops at `u32::MAX`, the bulk build counts in
  `u64`; a count that wrapped to 0 made `VsPolicy::Auto` drop the predicate
  on an out-of-core VS-tree and answer no rows.
- **The in-memory VS-tree is rebuilt after a clear** (db#4) of the default
  graph (`CLEAR` / `DROP DEFAULT` or `ALL`, the clears of `COPY` / `MOVE`;
  inside a transaction at its end) instead of staying unused until reopen;
  reasoning folds inferred triples into it. `COPY` / `MOVE` / `ADD` copy by
  id in chunks of 65,536 (db#16; every triple was turned into three terms
  and interned again), and `Database::build_from_ntriples_batched` indexes
  in one bulk load (db#15; it re-sorted the whole store per batch,
  O(N²/batch)).
- `Database::try_materialize_rdfs()` / `try_run_rules()` return
  `Result<usize>`, so a Stream-mode or RocksDB read error fails the run
  instead of leaving an incomplete closure with only a log line (design#7);
  `reasonManage` execute's rule ledger reads inside a read scope. An empty
  rollback no longer advances `mutation_count` nor drops the query cache and
  the CSR (db#7).

Server and concurrency:

- **RocksDB `/begin` no longer blocks the database's readers**
  (gap-rocksdb-exec#3): the whole-store copy for the transaction's snapshot
  is taken under the shared guard and registered after checking nothing
  changed (after three copies the last is taken under the exclusive guard);
  concurrent RocksDB `begin`s copy the store one at a time.
- **Password hashing and `users.db` writes no longer hold the server-wide
  lock** (gapi#6, http#7): `/api` `usermanage` / `userpassword`, REST
  `/user/create` and `/user/password` hash and rewrite `users.db` with the
  state lock released; a change overtaken by another save is prepared again,
  and a user dropped meanwhile is reported instead of resurrected.
- **Asynchronous callbacks run on their own threads** (gapi#12;
  `TaskConfig::callback_workers`, default 2), so a slow callback host no
  longer starves other users' asynchronous operations; the queue is bounded
  by count, bytes (64 MiB) and per user (32), and a queued callback holds a
  spilled result by its file name. `http_client` bounds name resolution and,
  with `tls`, every read of the TLS handshake by the request's deadline.
  Asynchronous operations and callbacks run on threads with the HTTP
  workers' stack size (design#8), and a runner that panics outside the
  operation's `catch_unwind` gives its slot back (gapi#20).
- **Index builds no longer block readers** (conc#5, db#5): a CSR build runs
  with the cache's lock released (concurrent callers share one build), and a
  reader finding a substring-index build running goes on without the
  prefilter. A write no longer throws the substring index away: the next
  substring query extends it with the new or changed terms (db#5), a full
  rebuild coming only after a quarter of the dictionary changed. The CSR
  build reads one predicate at a time and copies only entity pairs
  (analytics#6). The cached substring index keeps candidates only, about a
  third less memory (gap-accel-soundness#3; `StringIndex::candidates_only` /
  `build_candidates`; the serde form of `StringIndex` changed, no database
  persists it).
- **`ConcurrentDb` version-chain GC is incremental** (conc#2, http#8): it
  walked every chain on every commit, conflict and drop, quadratic over a
  wrapper's life (8.3 ms per commit after 20,000 transactional inserts); a
  GC now examines only the keys of the versions the oldest live pin has
  passed. New `ConcurrentDb::version_chain_stats()`.
- `format=file` and `n-triple-file` results are written row by row through a
  buffered writer (gapi#11; output byte-identical), and
  `Params::to_redacted_json` builds its object in linear time (gapi#19;
  32,000 parameters took 1.19 s). One form / URL decoder and one base64
  decoder (`gstore::util::form`, `gstore::util::base64`) serve the servers,
  `/api` and PFN (gapi#15, robust#16, design#13; behaviour unchanged).

Query engine and parsers:

- **`OPTIONAL`, `MINUS` and joins over maybe-unbound variables are hash
  joins in both executors** (H-19: sparqlconf#2, query#10; answers and order
  unchanged): the materializing executor compared every left solution with
  every right one for `OPTIONAL`, both scanned the right side per left
  solution for `MINUS`, and a join after an `OPTIONAL` fell back to the
  cross product (14.7 s at 20,000 subjects); new `query::probe::RowIndex`,
  whose partial indexes hold at most 4× its rows together.
- `REGEX` / `REPLACE` compile a pattern once per query (a per-query cache
  bounded by weight: 64 patterns of up to 256 KiB, or 8 larger ones), and
  `REPLACE` reads a long `$` reference in linear time; `DESCRIBE`
  de-duplicates its resources with a hash set (query#11, sparqlconf#8,
  query#12, sparqlconf#10).
- **The large-BGP plan search is bounded and cancellable** (query#13): a BGP
  of more than 14 patterns was refined with up to 16 2-opt passes that never
  looked at the cancel flag (a 400-pattern star planned for 6 s); orders are
  scored with reusable prefix statistics (plans unchanged), the search stops
  after `OPT_WORK_BUDGET` = 2^21 work units, skips 2-opt above
  `MAX_2OPT_PATTERNS` = 256 and polls every 4,096 units; a plan cut short is
  not cached.
- **One `EvalSettings` at every evaluator construction site** (design#9):
  `Database::read_as`, the UPDATE WHERE clause (default graph, `WITH`,
  `USING`), an HTTP transaction's snapshot and `ConcurrentDb` snapshots and
  version views apply one value (`EvalSettings`, `EvalProviders`,
  `Evaluator::with_settings`, `StringIndexCell`,
  `Database::eval_settings()`), so transaction-snapshot and `ConcurrentDb`
  reads keep their substring index across queries and `WITH` / `USING`
  updates use the database's.
- **RDF parsing is linear in the input** (parser#15, parser#7, parser#14):
  relative IRI resolution walks the path once (11 s for one 1.4 MB relative
  IRI before); JSON-LD contexts, RDFa prefix maps and RDF/XML namespace
  bindings and `xml:base` are shared as layers instead of copied per node
  (5000 node objects with `"@context": {}` under 4000 terms 1.8 s → 4 ms);
  the "blank graph label used before" check in large N-Quads / TriG is a
  growing Bloom filter with about 0.5% false positives (the old fixed filter
  reached 4.5% at 1M labels and 51% at 5M, each keeping a graph apart from
  the statements describing its label).
- RDFa: a minted blank node can no longer be forged with `&#0;` in an author
  label (parser#16).

Graph functions and PFN:

- The legacy `rs:` built-ins share the database's CSR (analytics#4):
  `rs:CYCLEBOOLEAN` is computed once per query on the CSR view (44 s → 55 ms
  on 22,264 rows) and `rs:CLOSENESSCENTRALITY(<x>)` runs one BFS.
  `SIMILARITY`'s Jaro loop visits only each character's window and is
  cancellable (analytics#5). Less memory per call (gap-gfun-fuzz#3,
  analytics#9, analytics#7): the kplex `Searcher` keeps bits and 16-bit
  counts (136 MiB instead of 320 MiB at the 8192-vertex limit), and
  degreeCorrelation, closeness, betweenness and JaccardSimilarity allocate
  their working sets once per call. The duplicate unpolled
  `metrics::k_hop_neighbor` is gone (analytics#10, design#15). The rustdoc
  of each graph function documents the upstream quirks it reproduces
  (analytics#2).
- **PFN functions of any size run** (ext#2): wasmi translates modules
  eagerly at compile / load; with lazy translation a function body above
  about 150 KB failed at its first call, also with `pfn_fuel_per_call=0`.

Documentation:

- `docs/DESIGN.md` §1–§7 describe the current architecture and `src/lib.rs`
  carries a module map by layer (design#16); every reasoner is documented as
  working on the default graph only (ext#15); stale parser docs fixed
  (parser#17). `docs/CPP_PARITY.md`'s three status tables are rewritten from
  the code, marking library-only and experimental items and correcting
  earlier claims inline (gap-doc-claims#3, #4, #5); `kvstore::vlist`
  describes the overflow page chain (disk#14) and `eval_describe` DESCRIBE's
  description and the syntax-tree cap (sparqlconf#19); `src/concurrent.rs`
  documents `ConcurrentDb` as a library-only second concurrency model
  (design#14). `docs/PFN.md` documents fuel, `pfn_fuel_per_query`, eager
  translation and compile-from-record. The bulk install's `COMMIT` marker is
  documented as advisory (durability#12, bulkcli#15). The `http_api` module
  docs describe the in-process TLS of the `tls` feature (http#19), and
  `TaskConfig::spill_threshold` names `<root>/tasks` (sec#21). `README.md`
  no longer says that a Stream-mode database opens in milliseconds whatever
  its size (named graphs are read whole at open, gap-doc-claims#6) nor that
  every upstream SDK works unchanged (the Python SDK is checked by
  `scripts/conformance.sh`; the Java, C++, Node.js, PHP and Go SDKs speak
  the same protocol but are not tested, gap-doc-claims#9).

#### New tests, tools and files

- **W3C-style SPARQL subset** (parser#18, query#18, sparqlconf#18):
  `tests/w3c_subset.rs` and `tests/w3c/<area>/<case>/` (`data.ttl` /
  `data.trig`, `query.rq` / `update.ru`, `expected.srj` / `.nt` / `.nq`, an
  optional `xfail` marker naming a review id, or `expected.error` for a
  negative case), hand-authored from the SPARQL 1.1 text with a `# spec:`
  line each; every case runs on the in-memory build, a reopened snapshot,
  the disk store in Materialize and Stream mode and RocksDB, through the
  materializing evaluator and the streaming executor (bounded heap and
  DP-B). 120 cases at round 2, 219 at round 4; every `xfail` marker of a
  fixed bug was removed by its fix, and one stays
  (`tricky/avg-over-an-error-is-unbound`: `SUM` / `AVG` skip non-numeric
  values, a documented extension, query#7). Format in `tests/w3c/README.md`;
  `W3C_FILTER=<substring>` runs a subset.
- **Executor differential suite** (query#17, design#10):
  `tests/executor_differential.rs` requires the materializing and streaming
  executors to answer every W3C case, the 14 LUBM queries (also with `LIMIT`
  and `ORDER BY … LIMIT`) and a 92-query probe battery alike on every layout
  — 7,060 runs (8,825 with RocksDB), no divergence; its `KNOWN` list is
  empty.
- **Layout matrix** (db#6, design#5, gap-doc-claims#7):
  `src/db/layout_matrix_tests.rs` runs every public write and maintenance
  operation on every layout and requires content, `triple_num` and
  `verify_integrity` equal to the snapshot layout's, across a checkpoint and
  a reopen; exhaustive matches over `StorageKind` and `DiskQueryMode` make a
  new layout fail to compile until it joins.
- **Backend conformance suite** (design#17):
  `tests/storage_backend_conformance.rs` runs one generic suite over
  `TripleStore`, every `Backend`, `DiskTriples`, `DiskStore`, `DiskSource`,
  `OverlayView`, `ShardedStore` and `RocksStore` (contract order, write
  answers, contract C1); the REST parity suite `tests/rest_api_parity.rs`
  covers REST, the single-database server and `/api` (design#4).
- **fsync audit** (tests-b#3, tests-a#13): the doc-hidden
  `gstore::sync_probe` records every fsync a commit point relies on, and
  `tests/fsync_audit.rs` (23 tests, 4 more with `rocksdb`) requires around
  every acknowledged commit point — auto-commit updates on every layout,
  commits, saves, checkpoints, backups, restores, the databases the `/api`
  server creates, bulk installs, WAL handling, redo appends, rule and PFN
  writes — that every changed file was synced after its last write (by
  content digest) and every changed directory after its last entry change,
  with each documented intermediate state recorded by a directory sync in
  order. Disabling the 37 sync sites these commit points reach one at a
  time, every one but `write_vstree_kv`'s directory sync fails an audit
  test.
- **Adversarial coverage map** (sec#20, http#20, robust#17, gapi#21):
  `tests/adversarial_coverage.rs` maps every DoS / hostile-input category of
  the review to its regression test and adds the two missing ones (more than
  256 header fields, a header line or request target over 64 KiB, refused
  with 400 before authentication).
- **Shared test helpers** (tests-a#16, tests-a#8, tests-b#11, tests-b#16):
  `tests/common/` (unique temp paths removed also on panic, an HTTP client
  with one overall deadline per request, `Running<S>` guards that stop and
  join in-process servers, guards restoring the process-wide logger and
  default open options, a per-thread allocation counter); the fixed
  temporary paths and bound-and-released ports of the tests the review named
  became unique paths and kernel-chosen ports (db#22, disk#13, tests-a#6,
  tests-b#9, tests-a#7), no HTTP read in the suite blocks without a bound,
  and the audited tests' wall-clock assertions became latches, counters,
  byte bounds and hang guards (http#20, gapi#21, conc#16).
- **Cross-version fixtures** (gap-doc-claims#8):
  `scripts/compat-fixtures.sh` builds the pre-Phase-6 commit `eaf5320` in a
  scratch worktree and checks the compatibility matrix with the real old
  binary (its snapshot and version-1 disk databases open here; it reads a
  snapshot and a `GSTORE_ID_RECLAIM=off` disk database written here and
  refuses version 2 and the new clear records); `--generate` rewrites
  `testdata/compat/eaf5320/`, which `tests/format_compat.rs` checks in every
  `cargo test`. New workflow `.github/workflows/compat.yml` (weekly and on
  demand).
- **Packaging smoke** (infra#7): `scripts/package-smoke.sh`
  (`stage [--deb FILE]`, `installed [--service]`, `image IMAGE`: every
  binary answering `--help`, `systemd-analyze verify`, the first-start root
  password, the health check, `/api` and REST round trips, a graceful stop)
  and `.github/workflows/packaging.yml` (`packages`: deb and rpm built at
  pinned tool versions, installed and smoked on the runner and in a Fedora
  container with `rpm -V`, enable → remove → install → purge; `image`:
  `docker build`, also on the MSRV, and the image smoke; `advisories`:
  `cargo audit`, non-blocking). New `packaging/rpm/pre_install.sh`,
  `packaging/docker/gstore-healthcheck`.
- **Fault-injection and crash suites**: `tests/storage_fault_injection.rs`
  (ENOSPC through `RLIMIT_FSIZE` children, gserver + `kill -9`),
  `tests/save_failure_ack.rs`, `tests/server_broken_store_recovery.rs`,
  `tests/rocksdb_statement_atomicity.rs` (SIGKILL at exact write batches),
  `tests/rocksdb_checkpoint_resources.rs`, `tests/rocksdb_damaged_store.rs`,
  `tests/txn_lifecycle.rs` and `src/txn_commit_tests.rs` (commit outcomes
  under `RLIMIT_FSIZE` in both disk modes), `tests/layout_replacement.rs`,
  `tests/redo_log_durability.rs` (torn tails at every cut, SIGXFSZ
  mid-append, old-format logs), `tests/storage_corruption.rs` and
  `tests/storage_kernel_regressions.rs`, the perturbation battery of
  `tests/dt_vs_policy.rs` (insert churn, id reuse, rolled-back transactions
  and restarts against an oracle database on every layout under every VS
  policy; gap-accel-soundness#5), plus the test-only pager failpoints
  (`kvstore::pager::failpoints`: the apply-phase fault injection disk#12 /
  durability#16 asked for) and `RocksStore::debug_set_write_hook`.
- **Behaviour suites by area** (each failing before its fix): HTTP —
  `http_preauth_limits`, `http_framing_deadlines`, `http_request_semantics`,
  `http_browser_surface`, `gapi_download_policy`, `login_lockout`,
  `query_limits`, `error_status_mapping`, `api_hygiene`,
  `api_session_tokens`; SPARQL — `sparql_paths`, `sparql_join_semantics`,
  `sparql_builtins`, `graph_management`,
  `aggregate_projection_must_be_grouped`,
  `eval_settings_reach_every_read_path`, `term_roundtrip`,
  `accelerator_soundness`; parsers — `rdf_parser_conformance`,
  `parser_limits_from_config`; analytics and extensions — `gfun_limits`
  (analytics#11, gap-gfun-fuzz#6: mid-run cancellation of every long-running
  function, every function on graphs without vertices), `cancellation`,
  `csr_build_memory`, `reason_pfn_regressions` (ext#14), `concurrent_db`
  (conc#15); CLI — `cli_failure_paths`, `zip_central_directory_bound`,
  `dirlock_saves`; crash and DoS — `service_hardening`, `crash_regressions`;
  bench — `bench/tests/smoke_gate_sensitivity.rs`, `baseline_validity.rs`,
  `cli_arguments.rs`, `update_phase_reopen.rs`, `reference_compare.rs`;
  packaging — `conformance_harness`, `packaging_scripts`,
  `packaging_layout`, `container_healthcheck`, `package_smoke`; cluster —
  `cluster::review_fix_tests`, `rpc_frame_reader_allocates_what_arrives`,
  `gnode_restarted_replica_votes_once_per_term`; gnlqa — 77 integration
  tests in the new `gnlqa/tests/` (cluster-nlqa#24), against scripted
  in-process HTTP stubs. Every round's review fixes added a test named after
  the defect.
- **Corrected tests** (D6: never weakened or deleted; each with a
  `review <id>` comment): tests that pinned a wrong or vacuous result now
  check the right one.
  - Storage and durability: `dt_reclaim` bounds each id space by its peak of
    live terms after a final reopen (tests-b#4); `dt_reclaim_crash` waits
    for the child's first acknowledgement (tests-b#8) and also kills
    multi-operation auto-commit requests and rolled-back transactions,
    allowing no orphan term on RocksDB (gap-rocksdb-exec#8, as do
    `dt_rocksdb`'s pid-unique paths); `dt_bulk_crash` proves WAL replay on
    any file system (tests-a#5), checks after every crash that the VS-tree
    the directory opens with agrees with its store (durability#17) and no
    longer depends on a 25 s budget (bulkcli#17); `dt_persist_backends` sets
    the shadow's mtime instead of sleeping 20 ms (tests-b#15); the RocksDB
    crash test of `dt_phase0_review` checks that the child applied its
    writes and died of SIGABRT (tests-b#7); `dt_load_import` runs a failed
    64 Ki-batch `LOAD` on both disk modes and RocksDB with reopens
    (tests-a#1); `a_read_error_fails_only_its_own_statement` checks what it
    claims (tests-b#18).
  - Servers: `dt_http_durability` uses kill-on-drop gserver guards in
    per-test directories and parses StatusCodes instead of `contains("1")`
    (tests-a#9, tests-a#11); `dt_rw_crash`'s acknowledgement rule ignores
    persistence-failure replies (durability#18); `upstream_transaction`
    expects a reload to remember how earlier ids ended
    (gap-txn-lifecycle#6); `dt_rw_server_stress`'s `/api` test requires an
    acknowledged two-operation write (tests-b#19); `dt_snapshot_disk` waits
    for its own server's expired-read count (tests-b#10); `dt_gapi_users`'
    log tests re-run when the local date changed while they ran
    (tests-a#15); `dt_cross_phase_api`, `dt_cross_phase5_api` and
    `dt_phase4_wiring` remove their scratch directories (tests-a#10);
    `dt_security_review` expects the host-local refusal (tests-b#1) and
    `dt_service` a failed non-SILENT SERVICE to fail (tests-b#2).
  - Query and data: `dt_sparql11` and `dt_cross_phase_api` compare full
    typed terms (sparqlconf#18, tests-a#2); `dt_graph` checks the graphs
    left after `CLEAR GRAPH` exactly (tests-a#3); `dt_bulk_import` pins
    blank-node scoping against the memory build (tests-a#4); `dt_num`
    compares against the largest and smallest values strictly and
    non-strictly (tests-b#12); `numeric_tower` asserts exact decimal sums
    (tests-b#13); `dt_optimizer` checks that each bow-tie query got a bushy
    plan and its no-bridge data reaches the optimizer (tests-b#14);
    `dt_disk_stream`'s corpus test reads its own corpus files and requires a
    floor of non-empty answers (tests-a#17).
  - Other: `dt_bulk_rocks`' FIFO feeder can no longer hang (tests-a#12);
    `dt_phase4_review_fixes` requires a cancellation in
    `a_cancelled_execute_leaves_the_rule_as_it_was` (tests-b#5) and bounds
    work by bytes and thread CPU time instead of the wall clock (conc#16);
    `bench/tests/gates.rs` expects a sub-millisecond query slowed 50× to
    fail (infra#2).
  - Tests broken by an intended secure default changed only their setup (an
    explicit root password, credentials or the compat switch). Three
    load-dependent tests were made deterministic.
- **New public API** (additions only, D2), besides the items named above:
  `Pager::{failure, failure_flag, mark_failed, wal_path, remove_with_wal,
  lock_read, lock_write, commit_shared, finish_commit, wal_batches}`,
  `DiskTriples::unsynced_writes`, `Database::{storage_failure,
  read_named_graphs, csr_cancellable, string_index_cancellable,
  preload_csr_cancellable}`; `RocksStore::{verify_stats, pending_reclaim,
  settle_pending, rollback_uncleared_journal}`; `gstore::preauth`
  (`PREAUTH_MAX_BODY`, `DEFAULT_BODY_BUDGET`), `json::parse_with_limits` /
  `json::Limits` / `json::escaped_len`, `gapi::auth::check_before_body`,
  `HttpOptions` fields for the new keys, `server::{peer_is_trusted_local,
  FORWARDING_HEADERS, write_response_streamed}`, `GResp::file`,
  `TaskManager::{remove_orphan_results, join_threads}`, `gstore::deadline`,
  `Server::{with_query_timeout, with_max_result_bytes}`,
  `gapi::ipfilter::{LoginLockout, LockoutPolicy}`; `db::ServicePolicy`,
  `Evaluator::with_service_policy`,
  `EvalStats::{path_steps, join_probe_rows}`; `parser::iri::{resolve,
  is_absolute, is_valid_scheme}`, `parser::stream::{DEFAULT_MAX_TOKEN_BYTES,
  set_default_max_token_bytes}`,
  `parser::sparql::lexer::{DEFAULT_MAX_REQUEST_BYTES, check_request_size}`,
  `parser::import::check_update_literals`; `gstore::util::{form, base64}`;
  `analytics::Interrupt`, `GraphView::{from_csr, closeness_of}`,
  `gfun::{MAX_ITERATION_WORK, FREE_ITERATION_SWEEPS}`;
  `http_client::tls_client_stream` (feature `tls`); and in gnlqa the
  `budget` and `net` modules, `CostLimits`,
  `Error::{is_fatal, code, client_message}`, `error::redact_userinfo`,
  `HttpAuth`, `HttpLimits`, `HttpServer::{with_token,
  allow_unauthenticated_loopback, with_auth, with_limits,
  with_read_deadline, with_max_conn_per_client}`, `McpServer::with_limits`,
  `RetryPolicy`, `KbClient::query_capped`,
  `graphrag::try_retrieve_subgraph`, `SolveEngine::{with_linker_builder,
  prepare_linker, with_cache_max_bytes, cache_usage}`, `LinkerLabels`,
  `LinkerCache`, `kb_fingerprint`, `Linker::build_cached` and new `Config`
  fields. Test hooks are doc-hidden and not a supported API.

#### Measured numbers

Measured on the shared campaign VM (4 vCPUs, 15 GB) while other work
packages built and tested (load averages of 5–15 were common), so every
timing is indicative; base and branch were measured back to back in one
session, and the quiet re-measurements some packages asked for were not
made.

- **Bench smoke gate** (`target/release/gstore-bench ci --baseline
  bench/baselines/smoke.json`, release `gbuild` + `gstore-bench`,
  `GSTORE_BENCH_CACHE` warm): every package on a hot path (pager, RocksDB,
  engine, string index, optimizer, graph functions, HTTP read path) passed
  base and branch back to back (456 gates; 460 since gstore-bench's
  `exact.config`), each round's integration head passed too. At the round-5
  head `03ad265`: PASS, 460 gates, 0 failed; query geomean vs the committed
  baseline LUBM-like snapshot 0.99 (limit 1.56), disk 1.02, WatDiv-like
  snapshot 1.06, disk 1.08 (limit 1.50); calibration 67.85 ms; the 2× and 3×
  canaries each exit 1. Under the new gates one query slowed 50× fails
  (`time.query` 7.03 > 3.00) where the old rules passed it.
- **Commit cost of the staged WAL** (durability#1): a disk single-triple
  update (one commit) rose from 1.285 to 1.878 ms (insert) and 1.290 to
  1.908 ms (delete) on LUBM-like, about +0.4–0.6 ms for the rename and the
  two directory fsyncs; 5,716-triple chunk updates within noise.
- **RocksDB with `check_point` on** (gap-rocksdb-exec#2; 200 single-triple
  `INSERT DATA` requests, LUBM fixture of 100,543 triples, test profile):
  9.01 → 0.73 ms per update; table files after 200 writes 633 → 11; memtable
  flushes 1000 → 0.
- **Pre-auth and download memory**: a 26 MB `{"a":[0,0,…]}` POSTed
  unauthenticated to `/api` is refused in 0.03 s with VmHWM 39,812 kB
  (867,044 kB before, C-2; debug `gserver --api`); a 64 MiB download peaks
  at 83,775 bytes of heap above the request's start (201,344,287 before;
  `cargo test --test gapi_download_policy -- --nocapture`, counting
  allocator, gapi#4); a 60,001-character multipart boundary over 8 MiB is
  refused in 0.01 s (11.5 s, sec#2).
- **SPARQL**: `ASK { FILTER(?x = a-a-…-a) }` at 160 KB 7.70 s → 0.035 s
  (`gquery`, release; H-14); a 400-triple ladder through `shortestPathCount`
  aborted after 11.7 s, now answers in 0.010 s (H-8);
  `?s :id 2990 . ?s :next* ?x` on a 3,000-node chain 1.15–1.27 s → 7 ms for
  the whole `gquery` process (sparqlconf#9); a join after `OPTIONAL` at
  20,000 subjects 14.6 s → 25.5 ms (test profile, H-19);
  `COUNT(DISTINCT ?o)` over 160,000 strings 37.3 s → 39 ms (test profile,
  query#12); a 400-pattern star plans in 6.5 ms (6,074 ms, test profile,
  query#13); `FILTER(rs:CYCLEBOOLEAN())` over 22,264 rows about 44 s → 55 ms
  (release `gquery`, analytics#4); a substring query after an `INSERT`
  277.8 → 7.3 ms (db#5).
- **Other**: a `ConcurrentDb` commit after 20,000 commits
  11.534 ms → 11.0 µs (debug test profile, conc#2); the WAL CRC of a 16 MiB
  batch 68.0 → 11.3 ms (disk#10); a two-hop textual rule over 8000 edges
  2.58 s → 20 ms (ext#7); `systemd-analyze security --offline` 6.8 → 1.2
  (infra#14).
- **Final check of round 5** (`03ad265`, toolchain 1.99.1):
  `cargo test --workspace` 3,316 passed, 0 failed, 10 ignored with
  `GSTORE_DISK_QUERY_MODE=memory` and again with `stream`; feature entries
  gzip 2,961, tls 2,966, zip 2,983, readline 2,957, pfn-wasm 2,986,
  `zip pfn-wasm readline` 3,012 passed (0 failed);
  `cargo test -p gstore --features rocksdb` 3,055 passed; `cargo test -p
  gstore-bench --features rocksdb` 93 passed; `scripts/conformance.sh` all
  six suites (parser's known cases 15 and 17); fmt, clippy `-D warnings` and
  rustdoc `-D warnings` clean; MSRV 1.89 `cargo check --locked --workspace
  --all-targets` clean; the test binaries also pass as an unprivileged uid.

### Phase 6: scale and performance (GAP_PLAN §8 stage 6, `docs/PHASE6_PLAN.md`)

Phase 6 was developed on the merged Phases 0–5 in four rounds of parallel
work packages; each round was merged, reviewed and measured on the idle
reference VM (4 vCPU / 16 GB) before the next was cut. Round 1: the storage
seam and a shared read path (R1-CORE), the kvstore read path and format
registry (R1-KV), the benchmark harness (R1-BENCH). Round 2: the transaction
overlay and `DiskTriples` (R2-DISKLIB), the out-of-core dictionary API
(R2-DICT), the bulk build (R2-BULK). Round 3: Stream mode in `Database`
(R3-STREAM), parallel readers in both servers (R3-SRV), RocksDB (R3-ROCKS).
Round 4: id reclamation (R4-RECLAIM), copy-on-write snapshots and disk
databases from the servers (R4-SNAP), the VS-tree policy (R4-VS), and the
closure (R4-CLOSURE: the WatDiv-like workload, the final measurements, these
docs). The per-package `changelog.d/` fragments are folded into this
section. Numbers come from the committed `bench/results/*.json` (tables made
by `gstore-bench report`) or from a package's quiet measurement stage (the
idle VM, base and branch in one session), named where used;
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) has the method, every table and
the exit-gate verdicts.

#### Behaviour changes

- **The ids of deleted terms are reused** (`[system] id_reclaim=on`, the
  default). After every applied delete — `DELETE DATA` / `DELETE … WHERE`,
  quad deletes, `CLEAR` / `DROP` / `MOVE` / `COPY`, `gsub` / `/api
  batchRemove`, the undo of a failed `LOAD`, the rollback of a
  transaction's inserts — every subject, predicate, object and graph IRI no
  triple or quad references any more leaves the dictionary, and the next new
  term takes its id, **lowest free id first**, on every layout. The
  entity / literal / predicate counts now count exactly the referenced terms
  (they drop with deletes; `DiskStore::entity_num` counts live terms). A
  transaction frees at `commit` and frees the terms it minted at `rollback`.
  An application that cached ids across a delete must not.
  `id_reclaim=off` (or `GSTORE_ID_RECLAIM=off`) keeps the former behaviour
  (ids freed only by `gcompact --reclaim`).
- **New on-disk databases are `kvstore.kv` version 2** (magic `GSTOREK2`,
  feature bit `FREE_IDS`), which binaries older than Phase 6 refuse ("not a
  gStore KV file"). Existing version-1 files are never upgraded implicitly;
  see *Formats and compatibility* below.
- **`gbuild --disk` and `gbuild --rocksdb` build in bulk** (sorted,
  bottom-up, installed crash-safely) and print the counts and a build
  summary instead of reopening the database; `--incremental` keeps the
  previous path. The ids equal the in-memory build's.
  `Database::build_disk` / `build_disk_from_sources` are the bulk build too
  and refuse a directory a database of the process has open.
- **Deletes keep the VS-tree valid.** Signatures only lose bits, so the
  stored entries stay a sound superset (`Database::vs_shrink_debt()` counts
  such deletes until `rebuild_index()`); queries after a delete keep the
  tree's pruning instead of running without it until the next save. Same
  answers; without `ORDER BY` the row order can differ from a
  tree-less evaluation. `CLEAR` and inference still invalidate the tree.
- **A RocksDB database opens without rebuilding its VS-tree**
  (`stats().index_valid == false`, shown as stale by `gshow`, `gmonitor`
  and the monitor endpoints) and evaluates without it until
  `rebuild_index()`; `[storage] vstree_on_open` / `GSTORE_VSTREE_ON_OPEN`
  restore the rebuild at open. RocksDB `iter_all` returns `(p, s, o)` order
  (it returned SPO order).
- **A read error fails the statement** (contract C2): a backend read error
  (on-disk B+ trees in Stream mode, the out-of-core dictionary, RocksDB) is
  recorded in the running statement's `ReadScope` and fails that query,
  update, CSR build, export (`gexport`, `/api export`,
  `Database::export_ntriples`), snapshot query or REST `/batchRemove`
  instead of answering from a shortened list, and the result is never
  cached.
- **The legacy `&str` dictionary API serves in-memory dictionaries only.**
  `Dictionary::id_to_string` / `predicate_to_string` answer `None` and
  record a read error on an out-of-core dictionary; use `term()` / `pred()`
  (`TermStr`). PFN's `GraphCtx::term_str` / `pred_str` now return
  `Option<TermStr<'_>>` (it derefs to `str`).
- **`dict.bin` is reproducible**: its format is unchanged, but the forward
  map is written in id order, so two builds of the same data write the same
  bytes; older binaries read the new files and the reverse. A file whose two
  halves disagree is refused.
- **Every layout locks its directory**, RocksDB included (`<dir>/LOCK`
  beside `rocksdb/`), and so do saves into another directory, the
  incremental disk build, `grestore` (before it stages anything), `gdrop`
  and `gbackup` (for the whole backup); repairs of an interrupted swap or
  restore run only under the lock. A restore that creates its database holds
  `<dir>.restore-lock` until the swap.
- **Monitoring never waits for a database.** `/status`, REST `/monitor` and
  `/show`, `/api show` / `stat` report the figures as of the last completed
  write; `/api monitor` / `schema` recompute their statistics in the
  background after a change (the request waits at most 40 ms, else answers
  with the earlier data's statistics and current counts), report the
  committed state while a transaction is open, and `diskUsed` is measured
  after a write or save. `show` / `monitor` report `layout` and, for a
  loaded on-disk database, `queryMode`.
- **With `check_point` off, `ApiServer` still makes every acknowledged write
  to an on-disk or RocksDB database durable at once** (a commit point after
  `/batchInsert`, `/batchRemove` and `/upload` too, whose writes were
  already written through); the snapshot layout keeps deferring to the
  save.
- **`CLEAR`, `DROP`, `MOVE` and `COPY` are in `redo.log`** (`C` for one
  graph, `N` for all named graphs; a RocksDB clear refused part-way is
  logged per removed triple), so `replay_redo_log` and `restore_incremental`
  reproduce them. *Corrected by the review-fix campaign (durability#6,
  gap-doc-claims#1):* the clear records written by Phase 6 have an empty
  body, which binaries older than Phase 6 skip silently, so restoring such
  an increment with them brings the cleared data back without an error;
  clear records written since the campaign's round 3 carry a placeholder
  triple, which those binaries refuse with "unknown redo.log op 'C'". Do not
  downgrade across one.
- **Command-line tools no longer panic on a closed pipe** (`gquery … |
  head`): the first broken-pipe write closes standard output for the rest of
  the process, which finishes its work and exits with its usual status.
- API: `Database::reclaim_unused()` returns `Result<usize>` (refused inside
  a transaction and on a version-1 `kvstore.kv`); `Database::store()`
  panics for a Stream-mode database (use `backend()`).

#### Formats and compatibility

`src/kvstore/format.rs` is the one registry of the `kvstore.kv` /
`vstree.kv` header: magics `GSTOREKV` (V1) and `GSTOREK2` (V2), 32 root slots
(24 before; older files read the new ones as 0), slot 21 the feature mask,
slot 22 the `FREE_IDS` B+ tree (keys `E|L|P ‖ be32(index)`), slots 23–25 the
free counts; feature bit 0 `FREE_IDS`, bits 1 (`IVARRAY_WIDE`) and 2
(`LONG_KEYS`) reserved. Every open checks the header: unknown bits, and V2
without bits, are refused; `vstree.kv` must have no feature bit. Node,
IvArray, overflow and WAL formats are unchanged. A V1 header written by this
version is byte-identical to a pre-Phase-6 one.

| directory | this version | binaries before Phase 6 |
|---|---|---|
| snapshot (`dict.bin`, `store.bin`, `named.bin`, `vstree.bin`, `meta.bin`) | read-write; bytes unchanged in format (`dict.bin` now reproducible) | read-write |
| `kvstore.kv` V1 (built before Phase 6, or with `id_reclaim=off`) | read-write in either query mode; reclamation off until `gcompact --enable-reclaim` | read-write |
| `kvstore.kv` V2 (new databases, bulk images, `gcompact --enable-reclaim`) | read-write | **refused** |
| RocksDB layout `"1"` (also when bulk-built) | read-write; no VS-tree rebuild at open | read-write |

To go back from V2: `gexport` with this version, `gbuild` with the old one.

#### On-disk databases: Stream and Materialize modes

- **Two query modes for `kvstore.kv` databases** (`DiskQueryMode`).
  **Materialize** stays the default (the single constant
  `gstore::db::DEFAULT_DISK_QUERY_MODE`): the pre-Phase-6 in-memory working
  set with write-through by dictionary key. **Stream** (opt-in:
  `[storage] disk_query_mode=stream`, `GSTORE_DISK_QUERY_MODE=stream`,
  `OpenOptions::stream()`) evaluates over the B+ trees
  (`Backend::Disk(DiskTriples)`) with an out-of-core dictionary (`DiskDict`
  on the same pager, opened once per database). Opening reads only the
  named graphs (kept in memory) and the O(P) predicate statistics. Both
  modes read and write the same `kvstore.kv`, so a directory can be opened
  in either, in turn. At univ 38 (`bench/results/phase6-final-lubm38.json`)
  Stream opens in 7.5 ms cold / 0.9 ms warm (Materialize 1,817.3 / 1,720.8
  ms) with 6.1 MiB RSS after open (351.0) and a 207.3 MiB high-water mark
  through Q1–Q14 (457.1), at a Q1–Q14 geomean of 11.55 ms (7.83). It stays
  opt-in because R3-STREAM's own conditions for making it the default still
  fail there (geomean 1.48× Materialize; Q14 2.15×, Q6 2.12×, Q12 2.07×), and
  its large results scale worse at univ 82 (*Known limitations*).
- **Open options (contract C7)**: `OpenOptions { disk_query_mode,
  disk_cache_pages, dict_cache_bytes, vstree_on_open, default_layout }`,
  `Database::open_with(dir, &options)` (as given), `Database::open` (the
  built-in defaults < those installed by `set_default_open_options`, which
  `gserver` does from `conf.ini`'s `[storage]` at startup and on
  `refreshconf` < the environment), `Database::disk_query_mode()`,
  `ApiServer::with_open_options` (embedded servers).
- **Writes in Stream mode go by id**: a new term is interned first
  (`DiskDict::intern`, one pager operation; an orphan term if its triple
  never comes), then the triple through `DiskTriples`. A transaction is the
  store's overlay: no triple reaches `kvstore.kv` before `commit`, `rollback`
  never touches it. A write or flush error **breaks** the store: every later
  statement — reads, dictionary writes and the CSR included — is refused
  until the database is reopened. `CLEAR DEFAULT` reads the default graph's
  ids in a read scope and removes them under one drain of the writer gate.
  The in-memory substring index is never built over an out-of-core
  dictionary (substring filters scan; answers are exact).
- **`vstree.kv`** is used while fresh and never rebuilt at open unless
  `vstree_on_open`; `rebuild_index()` rewrites it in one streaming pass over
  SPO/OSP after a commit point (the data is durable before the tree is
  signed) and marks it fresh. Under the default VS-tree policy queries no
  longer read its pages (*Query engine* below).
- **Library pieces** (R2-DISKLIB): `store::{Overlay, OverlayView,
  OverlayChanges}` — an exact transaction overlay implementing the whole
  `TripleSource` over `(base − removed) ∪ added`;
  `kvstore::DiskTriples` — `TripleSource + MutableStore` over a writable
  `DiskStore`, with transactions in the overlay, a **writer gate** (a writer
  marks it draining, cancels the snapshot readers inside it, waits for them,
  applies its change and moves the epoch only when data changed), snapshot
  views (`read_view()` → `DiskReadView::with_base(..) -> Result<R,
  ViewError>`: `Expired`, `Closed`, `Io`), the broken state and
  `commit_point()`; `kvstore::DiskSource` wraps any `&DiskStore` and reports
  its read errors.
- **Faster Stream reads** (same results and order): radix sorts of
  re-sorted index ranges; a 32 MiB cache of the committed store's long
  posting lists (2,048 entries and more), dropped at every write; a
  per-thread forward SPO cursor for the engine's sorted point probes.
- Restore validation of an on-disk backup reads the staged copy once,
  streamed, in Stream mode, so a server can restore a database larger than
  its memory.

#### Bulk build

- **`gstore::bulk::build`** (contract C6; `gbuild --disk`, `gbuild
  --rocksdb`, `Database::build_disk*`, REST `/build` and `/api build` with
  `layout=disk|rocksdb`): parse the sources on up to `--threads` parser
  threads (replayed in source order, with the sequential import's report,
  `parse_error.log` lines and strict-mode first error at every thread
  count), encode into term maps with first-seen ids (subject, predicate,
  object, graph: the memory build's ids), sort externally under a memory
  budget, then write:
  - **disk**: the SPO / POS / OSP / GSPO B+ trees, the dictionary trees, the
    `id2*` IvArrays and the statistics as page images, bottom-up at a 90%
    fill, plus `vstree.kv` streamed from the sorted signatures (byte-identical
    to the incremental build's); the output is byte-identical whatever the
    thread count or budget;
  - **RocksDB** (R3-ROCKS): one set of SST files per column family (cut at
    256 MiB), ingested into a fresh store; every key and value equals the
    per-triple build's (layout `"1"`; older binaries open it).
- **Crash-safe install.** The build holds the directory's lock and an
  in-process claim, writes into `<db>/.bulk-tmp/`, and refuses a directory
  whose RocksDB store is open anywhere (Linux: `rocksdb/LOCK` in
  `/proc/locks`, checked at the start and again before the swap). The commit
  point is the rename of the new `kvstore.kv` (disk) or of the staged
  `rocksdb.new` into `rocksdb/` (RocksDB); a crash at any step leaves the
  old or the new database, which an unmodified binary opens, and a failed
  build leaves the old one untouched. A failure after the commit point is reported as "built (the new
  database is in place), but its install did not finish: …" (exit status 1,
  `BulkReport::warnings`). The replaced database's reasoning rules are
  removed by the install, under the rule-directory lock.
- **Memory.** The budget (`--memory MB`, `[system] build_memory_mb`, default
  1024) covers the sort buffers, spools and their I/O buffers; sorted runs
  spill to `--tmp-dir` (default: the staging area) beyond it, in scratch
  directories that are locked while in use and swept once dead. The term
  maps (about `L + 26` bytes per term of `L` bytes) come on top and are
  reported. `--fill PCT` (50–100) is a disk-only knob.
- **Limits**: a dictionary key longer than 2,038 bytes (half a B+ tree page,
  `bptree::MAX_KEY_LEN`) fails a disk build or insert with an error, as does
  an IvArray past 33,554,432 terms per space; RocksDB has neither limit.
  The library helpers `Database::build_rocksdb_*` stay per-triple small-data
  builds (plan D15).
- At univ 38 (`phase6-final-lubm38.json`) the disk build takes 18.38–21.06 s
  (pre-Phase-6: 1,627.59 s) with a 398–400 MiB peak and writes a 542,609,408
  byte `kvstore.kv` (0.697× pre); the RocksDB build 19.50 s (277.76 s).

#### Dictionary

- **A leaner in-memory dictionary**: each string stored once as an
  `Arc<str>`, the string → id side an open-addressing table of ids; the
  interners are `Arc<InternerData>`, so a clone (a `ConcurrentDb` snapshot,
  a transaction's begin-time copy) is O(1) and the first write copies only
  the interner it changes. R2-DICT's quiet stage: snapshot RSS after open
  0.80× the base at univ 8 and 38.
- **Dictionary API (contract C4)**, `gstore::dict`: `TermStr` (borrowed in
  memory, `Arc<str>` from a backend); `Dictionary::term` / `pred` /
  `try_term` / `try_pred` / `try_*_id`, `prefetch`, `resolve_sorted` /
  `resolve_many`, `for_each_term`, `materialize`, `id_limit`, `backing`,
  `generation`, `cache_stats`; `DiskTermSource` is a fallible, `Send +
  Sync` backend (`lookup`, `resolve`, `intern` / `free`, `prefetch`,
  `for_each`, `count`, `id_limit`, `generation`, `sync`). Bounded caches:
  `TermCache` (id → term, 64 MiB by default) and `KeyCache` (term → id,
  16 MiB), CLOCK eviction, emptied when the backend's generation changes.
  `DiskDict` implements it over the shared pager without leaking strings.
- **Engine**: SELECT results are materialized in chunks of 4,096 rows, each
  resolving its distinct ids in one sorted batch on an out-of-core
  dictionary; a per-query id → term / value memo (at most 8 MiB) serves
  out-of-core dictionaries only.
- **The substring index is cached per dictionary generation**
  (`Database::string_index()`): a repeated `CONTAINS` / `STRSTARTS` /
  `STRENDS` / plain `REGEX` query no longer rebuilds it — R2-DICT's quiet
  stage at univ 8: 11.7–16.9 ms for the second identical query against
  2,073.6–2,888.5 ms for the first. The cached index stays resident (87.4 MB
  at univ 8, 431.0 MB at univ 38 in that stage).

#### Query engine and the VS-tree

- **VS-tree policy** (`gstore::query::VsPolicy`, R4-VS): `Auto` (the
  default) computes exactly the candidate sets the tree gives, more cheaply
  where that is provably exact — on an on-disk tree (`vstree.kv`, both disk
  query modes) it tests the remaining predicate bits against the store and
  **never reads `vstree.kv`'s pages**; `Tree` is the previous behaviour;
  `Off` ignores the tree. Switches: `Evaluator::with_vs_policy`,
  `query::scoped_vs_policy`, `query::set_default_vs_policy`,
  `GSTORE_VS_POLICY=auto|tree|off`; counters `query::vs_stats()`. Plans,
  answers and their order are unchanged on a tree consistent with the
  store. R4-VS's quiet stage (univ 38, Stream): Q1–Q14 geomean 11.66 ms
  under `Auto`, 47.70 ms with `GSTORE_VS_POLICY=tree`.
- **The in-memory VS-tree**: an entity → leaf locator (O(height) updates,
  per-entity probes), probe refinement for small candidate sets, an insert
  ORs the new edge's bits up the parent chain (`VsTree::union_into`), an
  entity with no edge before (a new term, a reused id) gets its exact
  signature; the build sorts once on the raw limbs. `vstree.bin` /
  `vstree.kv` bytes are unchanged.
- **Cheaper planning, same plans**: the cost model counts instead of
  materializing (the C1 `count_*` / `distinct_p_of_*` methods, answered
  exactly by every backend), `TripleStore` maintains each predicate's
  distinct subject / object counts, hash joins use a fast deterministic
  hasher, candidate sets are generated shortest list first with member tests
  for long lists, one count memo per BGP, constant-star BGPs skip the plan
  search, and a pattern with few candidates looks them up instead of
  scanning the predicate. LUBM Q4 no longer grows with the data (0.049 ms on
  the snapshot layout at univ 38, pre-Phase-6 53.63 ms).
- **Storage seam (contract C1)**: `Backend` and `backend_dispatch!` live in
  `src/store/backend.rs`; `TripleSource` documents its ordering contract
  (pairs sorted, id lists ascending, `iter_all` in `(p, s, o)` order) and
  gains the count methods and `has_subject` / `has_object` /
  `has_predicate`, which answer `true` on a read error.
- **Per-statement read errors (contract C2)**: `store::read_scope`
  (`ReadScope::enter()`, `take_error()`, `note_read_error(msg)`,
  `unscoped_read_errors()`).

#### kvstore

- **Pager**: shared copy-on-write `Arc` pages, CLOCK eviction, `pread`;
  **operation brackets** — every logical write (a term, a triple or quad
  and its keys) is one pager operation, never committed in part and rolled
  back entirely on an error or a panic (which no longer poisons the store).
- **B+ trees**: zero-copy, bounds-checked node views (a malformed page is an
  error, never a panic), binary search in fixed-shape trees, prefix runs
  bounded by two binary searches; nodes split by bytes, so no node outgrows
  a page, and keys are limited to 2,038 bytes.
- **`DiskStore`** reads come in the C1 contract order, with exact
  allocation-free counts and a per-predicate statistics mirror; a damaged
  header fails the open with an error instead of aborting the process.

#### Concurrency and the servers

- **`Database` is `Send + Sync`** (contract C8) with a `&self` read path:
  `gstore::Statement` (parse once), `Database::read_as`, `query_read`,
  `execute_as`. Query-cache entries are shared (`Arc`); `persist_lock`
  serializes saves, checkpoints and backups (lock order `persist_lock` →
  disk → `persist_error` → pager).
- **Both HTTP servers share a database between readers**: the single-db
  `Server` holds a `RwLock<Database>`, `ApiServer` a `DbCell` per database;
  reads (and export, backup, `loadcsr`) take the shared guard, writes and
  the transaction operations the exclusive one. A thread taking a second
  guard of the same database gets an error instead of deadlocking. The
  transaction contract is unchanged (single writer, `409`, begin-time
  snapshot reads). Q4 at univ 38 (`phase6-final-lubm38.json`), 8 clients
  against 1: 5.33× via `/sparql` and 4.63× via `/api` on the snapshot
  layout, 4.06× / 4.73× on a Stream-mode disk database (pre-Phase-6: 0.78× /
  0.86×).
- **Copy-on-write `TripleStore`**: the indexes live in one `Arc`'d
  `StoreData`; a clone is O(1), the first write copies once, a write that
  changes nothing never copies; `store.bin` / `named.bin` bytes and `Debug`
  are unchanged. `TripleStore::insert_many` / `remove_many` merge a batch
  into each touched list in one pass, and the in-memory write paths now hand
  each statement's default-graph triples over as one batch (with
  lowest-first id reuse, per-triple inserts at the front of long lists had
  made memory-layout inserts 4.7× slower in R4-RECLAIM's churn).
- **`/begin` copies no triple** on in-memory and on-disk databases: the
  snapshot is a copy-on-write clone of the in-memory store (also of a
  Materialize-mode working set) or a Stream database's `DiskReadView`,
  sharing the dictionary and named graphs (RocksDB still materializes, plan
  §8 D4). At univ 38 its median is 0.38 ms with +0.07 MiB RSS (snapshot),
  0.38 ms with +0.00 MiB (Materialize) and 0.44 ms with +0.06 MiB (Stream);
  RocksDB 2,132.55 ms. Snapshot reads of a Stream database run under the
  writer gate with its cancel flag; a commit cancels them and they re-run on
  the committed state, so `/commit` no longer waits for long snapshot
  reads.
- **`ConcurrentDb` publishes in O(1)** for an in-memory database (the
  authoritative store keeps a spare copy, so an update after a publish costs
  about two inserts — R4-SNAP's quiet stage: 387–581× lower one-triple
  update latency than the base); Stream-mode and RocksDB databases are still
  materialized per publish. Its snapshot queries run in a read scope.
- **REST `/build` and `/api build` take `layout=snapshot|disk|rocksdb`**
  (also `memory`, `kvstore`, `rocks`; default `[storage] default_layout`,
  else `snapshot`); `gbuild --layout` likewise. REST `/build` saves a loaded
  database's unsaved writes before it retires it and serves it again when
  the build fails.
- PFN metadata is cached per user and modules load single-flight. Same-
  second `/api backup`s and concurrent REST `/backup`s of one database run
  one after the other. The CSR of a Stream transaction snapshot is built
  cancellably (`CsrGraph::build_cancellable`).

#### RocksDB

- Reads are bounded raw-iterator scans (`iterate_upper_bound`, keys decoded
  in place); a read error fails the statement, and a write that cannot read
  fails instead of treating the key as absent.
- Exact C1 counts from the maintained counters (one point read each), an
  in-memory mirror of the per-predicate counters, `subs_by_p` / `objs_by_p`
  without sorts, `iter_all` in contract order.
- `RocksTuning`: one LRU block cache shared by every column family and store
  of the process (`[storage] rocksdb_block_cache_mb`, default 256,
  `GSTORE_ROCKS_BLOCK_CACHE_MB`; `0` = RocksDB's per-column-family default)
  and bloom filters on the point-lookup column families
  (`rocksdb_bloom`, default 10 bits per key); options only, files open either
  way. `RocksStore::open_with`.
- Id reclamation: frees delete the `dict` entries after the triple batch or
  inside the commit's journal batch; terms a journal replay left
  unreferenced are freed.
- At univ 38 (`phase6-final-lubm38.json`): open 0.74 s cold / 0.68 s warm
  (pre-Phase-6 60.8 / 59.4 s), Q1–Q14 geomean 25.00 ms (311.50 ms), 155.7 MB
  (206.3 MB).

#### Id reclamation

- **Policy** `db::IdReclaim { Immediate, Off }` (`[system] id_reclaim`,
  `GSTORE_ID_RECLAIM`, `Database::set_id_reclaim`). "Referenced" means used
  by the default graph (the C1 `has_*` probes, `true` on a read error) or by
  a named graph (`db::named::NamedGraphs` counts every term's uses, graph
  IRIs included, and serializes exactly as the former map). A candidate
  whose reference probe fails waits for the next check and is given up after
  4 failed checks, left for `gcompact --reclaim`.
- **Persistence per layout**: snapshot at the next save (`dict.bin`'s free
  list is the same sequence of `u32`, now ascending); Materialize as a
  key-level free written through; Stream by id inside the store under the
  writer gate (`DiskTriples::free_ids`, which re-checks the indexes and
  refuses — breaking the store — rather than corrupt it); RocksDB as above.
  Freeing the highest id lowers the high-water mark.
- **Pins**: a removed id is retired, then released once no `DictPin` older
  than the removal lives. A clone of an out-of-core dictionary pins it, so a
  Stream database defers its frees while such a clone (a `ConcurrentDb`
  snapshot) is alive; a disk `/begin` snapshot reads inside its view and
  takes an unpinned clone; in-memory clones need no pin. Frees still
  deferred at a restart are lost (orphans that `gcompact --reclaim` finds).
- **VS-tree**: freed entities stay in the tree (a sound superset); a reused
  id gets its exact signature.
- API: `TermId`, `retire` / `release` / `free_id`, `pin` /
  `clone_unpinned`, `from_slots` (exact ids with holes), `id_limits`,
  `Database::reclaim_unused`, `verify_integrity`, `id_space_stats`,
  `enable_id_reclaim`; `DiskStore::open_with(path, cache,
  free_ids_if_new)`, `RocksStore::delete_terms`, `commit_journal_with`,
  `take_replayed`.
- R4-RECLAIM's quiet stage (`examples/reclaim_churn.rs --base 8 --rounds
  10`: eight LUBM-like universities, then ten rounds that each delete the
  oldest university and insert a new one): live counts equal the referenced
  terms in every round on memory, Stream, Materialize and RocksDB, the
  high-water mark is 1.000× the peak live count, and the dictionary stays
  within +5.20% of round 0 where `Off` grows to 2.27×.

#### Configuration and tools

| key / variable | default | effect |
|---|---|---|
| `[storage] disk_query_mode` / `GSTORE_DISK_QUERY_MODE` | `memory` | `memory` (Materialize) or `stream` |
| `[storage] disk_cache_mb` / `GSTORE_DISK_CACHE_MB` | 16 | MiB of page cache per on-disk database |
| `[storage] dict_cache_mb` | 80 | MiB of a Stream database's dictionary caches |
| `[storage] vstree_on_open` / `GSTORE_VSTREE_ON_OPEN` | off | build a missing or stale VS-tree at open |
| `[storage] default_layout` | unset (snapshot) | the layout of `gbuild`, REST `/build` and `/api build` when none is named |
| `[storage] rocksdb_block_cache_mb` / `GSTORE_ROCKS_BLOCK_CACHE_MB` | 256 | the shared RocksDB block cache, MiB |
| `[storage] rocksdb_bloom` | 10 | bloom-filter bits per key (`on`, `off`) |
| `[system] id_reclaim` / `GSTORE_ID_RECLAIM` | on | id reclamation (gserver and gbuild read the key; the other tools the variable only) |
| `[system] build_memory_mb` | 1024 | the bulk build's sort budget |
| `[system] build_threads` | 0 (every core) | bulk build threads; `build_multi_thread=off` means 1 |
| `[system] build_tmp_dir` | unset (the staging area) | where sorted runs spill |
| `[system] build_fill_factor` | 90 | B+ tree node fill of the disk bulk build |
| `GSTORE_VS_POLICY` | `auto` | `auto`, `tree` or `off` |

`gserver` installs `[storage]`, `id_reclaim` and the RocksDB keys at
startup and again on `refreshconf` (a loaded database keeps its query mode);
`gbuild` reads `./conf/conf.ini` when it exists; `conf/conf.ini.example`
documents every key.

- **`gbuild`**: `--layout snapshot|disk|rocksdb`; `--disk` / `--rocksdb` take
  `--memory MB`, `--threads N`, `--tmp-dir DIR`, `--incremental`, and
  `--disk` also `--fill PCT` (refused, exit 2, without them).
- **`gcompact <db> [--reclaim] [--enable-reclaim] [--check]`** (new; in the
  Docker image and the deb / rpm packages): show the id spaces, free every
  unreferenced id and save, turn reclamation on for a pre-Phase-6
  `kvstore.kv` (it becomes V2), verify the id spaces.
- **`gmonitor`** shows `id_reclaim` and live / free / id-limit per id space.
- **Examples**: `examples/kv_bench.rs` (kvstore and dictionary
  micro-benchmarks), `examples/disk_stream_probe.rs` (Q1–Q14 over
  `DiskTriples`, a view and the materialized store),
  `examples/reclaim_churn.rs` (gate G5). R3-SRV's `examples/srv_mixed_load.rs` is gone: gstore-bench's
  mixed phase covers it on every layout.

#### gstore-bench and CI

- **`gstore-bench`** (new workspace member `bench/`, `publish = false`, no
  new crates; [`bench/README.md`](bench/README.md)): deterministic
  LUBM-like (`lubm-like/1`, a port of the design-stage `gen_lubm.py`
  prototype) and WatDiv-like (`watdiv-like/1`: WatDiv's vocabulary and query
  shapes, its own data; 20 templates bound by a deterministic ASK walk)
  generators; a runner that measures every layout through the real `gbuild`
  and the public API, each phase in a fresh child process (build, open,
  query, concurrency, update, and a mixed read/write phase with an `/api
  begin` point); results in `gstore-bench/2` JSON (`--label` keys a run
  `backend@label`, `merge` joins files); exact, timing, memory and size
  gates (`bench/thresholds.json`) that report new workloads, runs and queries
  instead of failing them; and `report --table KIND` tables that `report
  --update` / `--check` keep current between `<!-- gstore-bench report ARGS
  -->` markers in documents. Commands `gen`, `queries`, `run`, `check`,
  `ci`, `compare`, `report`, `bless`, `merge`, `calibrate`; exit codes 0 /
  1 (a gate failed) / 2 (usage, crash, timeout).
- **Results**: `bench/results/pre-phase6-lubm{8,38}.json` (the pre-Phase-6
  reference every Phase 6 ratio is taken against), each round's results and
  compare reports, and the final `phase6-final-lubm{8,38,82}.json` and
  `phase6-final-watdiv10.json`. `bench/baselines/smoke.json` was re-blessed
  at the closure (gstore-bench/2: LUBM-like univ 1 and WatDiv-like scale 1,
  with the mixed phase); the database-size gate is 1.10× (was 1.25×).
- **CI**: `.github/workflows/bench.yml` — `smoke` (release build, the
  documentation-table check, `gstore-bench ci`, a canary that must fail at
  3×), `rocksdb-correctness`, `reference` (weekly or on demand,
  report-only). `ci.yml` gained a `fmt` job (`rustfmt.toml`; the one
  formatting commit is in `.git-blame-ignore-revs`), runs the workspace
  tests twice (`GSTORE_DISK_QUERY_MODE=memory` and `=stream`), checks the
  generated tables (`report --check README.md CHANGELOG.md`) and the
  rustdoc intra-doc links (`RUSTDOCFLAGS="-D warnings" cargo doc -p gstore
  --no-deps`).

#### Performance

LUBM-like univ 38 (5,181,103 triples) against the code before Phase 6,
generated from `bench/results/phase6-final-lubm38.json` (warm figures: the
minimum over 5 processes of the median of 5 runs; "open first" is the first
open after the page cache was dropped):

<!-- gstore-bench report --table overview bench/results/phase6-final-lubm38.json --compare bench/results/pre-phase6-lubm38.json -->
| workload | backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| lubm-like univ 38 | snapshot | 29.60 | 550.1 | 430.8 | 1184.8 (cold) | 1033.0 | 489.1 | 601.0 | 10.15 | 0.39× | 0.71× | 0.22× |
| lubm-like univ 38 | disk@materialize | 21.06 | 400.3 | 654.0 | 1817.3 (cold) | 1720.8 | 351.0 | 457.1 | 7.83 | 0.01× | 0.22× | 0.05× |
| lubm-like univ 38 | disk@stream | 18.38 | 398.2 | 654.0 | 7.5 (cold) | 0.9 | 6.1 | 207.3 | 11.55 | 0.01× | 0.00× | 0.08× |
| lubm-like univ 38 | rocksdb | 19.50 | 401.6 | 155.7 | 738.9 (cold) | 678.0 | 270.8 | 436.2 | 25.00 | 0.07× | 0.01× | 0.08× |
<!-- /gstore-bench report -->

Exit gates (plan §2), univ 38 unless stated; the measured values come from
`bench/results/phase6-final-*.json`, except G4's monitoring endpoints and
G5's churn, which come from the R4-SNAP and R4-RECLAIM quiet stages of the
same gstore code (`b9e8dc0`), and G1b's three extra open pairs and G6's test
counts, from R4-CLOSURE's quiet stage itself (not committed as result
files). Reasons and follow-ups in
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) §7.

| gate | target | measured | verdict |
|---|---|---|---|
| G1a disk build | ≤ 0.04× pre (≤ 65.1 s); peak ≤ 1.25 GiB with `--memory 1024`; `kvstore.kv` ≤ 0.75× pre | 18.38 / 21.06 s (0.011× / 0.013× of 1,627.6 s); peak 398 / 400 MiB, no spilled run; `kvstore.kv` 0.697× | met |
| G1b disk open (Stream) | warm ≤ 0.5 s, cold ≤ 2.0 s; univ 38 / univ 8 ≤ 1.5 | warm 0.92 ms, cold 7.5 ms; ratio warm 1.13, cold 1.82 in the reference pair and 1.43 by the medians of four pairs (single ms-scale samples whose ranges overlap) | met |
| G1c disk memory (Stream) | RSS after open ≤ 120 MiB; HWM ≤ 400 MiB; RSS ratio ≤ 1.5 | 6.1 MiB; 207.3 MiB; 1.01 (univ 82: 6.4 / 380.8 MiB) | met |
| G1d disk latency (Stream) | geomean ≤ 0.6× pre disk and ≤ 1.5× snapshot; no query > 3× snapshot | 0.079× pre, 1.14× snapshot; worst Q14 2.51×, Q6 2.41×, Q13 2.25× (Materialize 0.053×, 0.77×) | met |
| G2 snapshot layout | build ≤ 0.5×, RSS ≤ 0.9×, geomean ≤ 0.9× pre; Q4 ≤ 25 ms; Q1/Q3/Q4/Q11/Q12 univ 38 / univ 8 ≤ 2.0 | 0.385×, 0.820×, 0.218×; Q4 0.049 ms; Q1 1.03, Q3 1.04, Q4 0.97, **Q11 5.58, Q12 4.91** | not met |
| G3 RocksDB | build ≤ 0.25× pre; open ≤ 10 s; geomean ≤ 0.8× pre | 19.50 s (0.070×); 0.74 s cold / 0.68 s warm; 25.00 ms (0.080×) | met |
| G4 concurrency | Q4 8 / 1 clients ≥ 2.5× (memory, Stream; `/sparql`, `/api`); `/begin` ≤ 5 / 50 ms, RSS ≤ 4 / 16 MiB; monitoring ≤ 100 ms | 5.33 / 4.63 (snapshot), 4.06 / 4.73 (Stream); `/begin` 0.38 / 0.44 ms, +0.07 / +0.06 MiB; monitoring ≤ 48.4 ms, **`/api stat` 201.6–219.8 ms** | not met |
| G5 id reclamation | live = referenced; ±5% of round 0; high-water ≤ 1.05; delete overhead ≤ 10% memory / disk, ≤ 20% RocksDB | live = referenced in every round; high-water 1.000; **+5.20%**; overhead memory **1.13 / 1.12**, Stream 0.63, **Materialize 1.14**, RocksDB 0.98 | not met |
| G6 correctness | identical answers everywhere; test matrix, `conformance.sh`, crash suites green | identical rows and hashes on every layout and mode, equal to pre; tests below, 0 failures | met |
| G7 vs upstream lubm10M (univ 82) | each metric ≤ 10× upstream; ≤ 3× met | snapshot / disk: build 6.27 / 4.45 µs per triple (0.27× / 0.19× of 23), 83.2 / 126.2 bytes per triple (0.32× / 0.48× of 264), Q2 85.3 / 71.2 ms (0.59× / 0.49× of 144), Q14 141.3 / 165.3 ms (0.63× / 0.74× of 223); Q1, Q3, Q4 below 0.01× | met |

None of the misses is a regression of Round 4: G2's Q11 / Q12 (the
in-memory VS-tree's probes grow with the data; the base's ratios were 6.02 /
4.99) and G4's `/api stat` (a blocking 200 ms CPU sample per request, by
design) equal the base in same-session comparisons; G5's misses are costs of
the reclamation design that R4-RECLAIM profiled (the churn's own data grows
+5.23%; each Materialize free is two B+ tree writes).

#### Review fixes

Each round's merged tree was reviewed, and the confirmed findings were
fixed with regression tests. The main ones:

- **Round 1**: B+ tree nodes split by bytes, never past a page (a node of
  long entries could be written over a page and panic, poisoning the shared
  pager); read-only views see VList invalidation at once; a damaged header
  is an error; VS-tree maintenance no longer re-signs hub entities from all
  their edges (it had become quadratic once deletes kept the tree); the
  bench CI canary checks a run against itself slowed 3×, retried phases
  rebuild the database, phase children die with the harness.
- **Round 2**: graph functions and PFN read labels through `term()` /
  `pred()` (they failed on an out-of-core dictionary); a dictionary batch
  decodes only the ids asked for; the per-query memo is bounded by bytes;
  `DiskTriples` refuses its raw store once broken, keeps a transaction a
  self-deadlocking commit would have dropped, and reports `vstree_stale()`
  while a transaction adds triples; the bulk build closes spilled runs (it
  ran out of descriptors with `--memory 1`), charges its I/O buffers, names
  scratch directories exclusively, keeps the sequential import's batches at
  every thread count, never replaces an open RocksDB store, and stops on a
  failed WAL replay of the old store.
- **Round 3**: RocksDB `so_by_p` no longer loses a read error to the next
  seek and `iter_all` no longer trusts a damaged counter; bulk installs
  re-check the RocksDB lock before the swap; the directory lock precedes
  every repair of an interrupted swap or restore, and every writer of a
  directory takes it (`gbuild --force`, `--incremental`, `grestore`,
  `gdrop`, `gbackup`); a broken Stream store refuses dictionary writes;
  deletes look terms up in a read scope; `VersionView` queries and the
  version GC handle read errors; the CSR checks the broken state;
  `rebuild_index` commits before signing; Stream statistics rebuilt at open
  are persisted; the per-thread SPO cursor follows the pager's write
  generation; `/api monitor` / `schema` never wait for a busy database;
  same-second `/api backup`s and concurrent REST `/backup`s are serialized;
  REST `/batchRemove` fails on a read error.
- **Round 4**: a pinned Stream database no longer re-probes its whole free
  backlog at every statement; a read error during the reference check
  defers only that candidate; `ConcurrentDb` over a Stream database frees
  its deferred ids when dropped; a partly refused RocksDB `CLEAR` is logged
  per triple; RocksDB journal replays hand over their terms once and leave
  no orphans; `gbuild` applies `conf.ini`'s `id_reclaim`; REST `/build` in
  every layout saves and re-serves a database whose build fails, keeps a
  body's blank-node labels, answers `500` for an unfinished install and no
  longer holds the rule-directory lock through a build; `/api monitor`
  during a transaction reports the committed state; `/commit` and
  `/rollback` drop the begin-time snapshot first (a freeing commit copied
  the whole interner); `report --update` / `--check` ignore markers in
  fenced code.

#### Tests

New suites: `dt_count_methods`, `dt_read_scope`, `dt_overlay`,
`dt_dict_backends`, `dt_r2_integration`, `dt_bulk_disk`, `dt_bulk_import`,
`dt_bulk_crash`, `dt_bulk_rocks`, `dt_disk_stream`, `dt_disk_stream_txn`,
`dt_disk_stream_crash`, `dt_rw_server_stress`, `dt_rw_crash`,
`dt_r3_integration`, `dt_reclaim`, `dt_reclaim_crash`,
`dt_reclaim_concurrency`, `dt_cow_store`, `dt_snapshot_disk`,
`dt_vs_policy`, `dt_r4_merge`; `dt_http_durability` and
`upstream_transaction` also run over the disk layout. Every test helper
that makes a temporary directory removes it when the test ends, failed or
not. The final run (R4-CLOSURE's quiet stage, on `85686e5`): `cargo test
--workspace` 2,375 passed with on-disk databases in Materialize and again in
Stream mode (crash suites included); `--features rocksdb` 2,202;
`--features "gzip tls zip readline pfn-wasm"` 2,192; gstore-bench 69 (and 69
with RocksDB); `scripts/conformance.sh` 6 / 6 suites; 0 failures.

#### Known limitations

- **Stream mode is opt-in.** Besides the univ 38 gaps above, its large
  results grow faster than the data: at univ 82
  (`phase6-final-lubm82.json`) Q6 / Q13 / Q14 take 984.84 / 1,303.58 /
  876.09 ms, 4.65 / 5.10 / 6.20× the snapshot layout (2.41 / 2.25 / 2.51×
  at univ 38) — each result id is resolved through the dictionary's
  64 MiB `TermCache`, which those results outgrow. Under a steady write
  load its readers lose the long-list cache at every commit: WatDiv-like F5
  with a write every 100 ms reaches 191.6 qps on Stream against 2,550.3 on
  Materialize (`phase6-final-watdiv10.json`).
- **The snapshot layout with `check_point` on** saves the whole snapshot
  under the exclusive guard at every acknowledged write: at univ 38 the
  mixed workload's readers get 12.5 / 10.1 qps (`/sparql` / `/api`).
- `/api stat` takes a blocking 200 ms CPU sample per request (G4).
- Sub-millisecond queries on the snapshot layout (LUBM Q11, Q12) slow down
  with the data (G2).
- RocksDB still loads its dictionary at open and materializes the store for
  `/begin` and `ConcurrentDb` (plan §8 D4); its LUBM Q2 scales worst (5.49
  ms at univ 8, 585.74 ms at univ 38).
- Files only grow (plan §8 D9): R4-RECLAIM's churn left a Stream
  `kvstore.kv` at 1.474× round 0. Disk transactions larger than the page
  cache are durable but not atomic (D12). Named graphs stay in memory in
  Stream mode (D11). The cached substring index of an in-memory dictionary
  stays resident.
- Frees a pin still holds at a restart are lost (orphans until `gcompact
  --reclaim`); a `ConcurrentDb` over a Stream database frees no id while it
  lives.
- The other deferred items (plan §8 D1–D15) and every follow-up are in
  `docs/REFACTOR_BACKLOG.md` §I.

### CI notes

- **CI pins its Rust toolchain.** `.github/workflows/ci.yml` and
  `.github/workflows/bench.yml` set `RUST_TOOLCHAIN: "1.99.1"` and install it
  with `dtolnay/rust-toolchain@master` instead of
  `dtolnay/rust-toolchain@stable`. When `@stable` moved to 1.99.1
  (2026-10-31), its clippy brought new lints, so every `-D warnings` clippy
  step failed — and in the feature matrix, where clippy runs before the
  tests, the `gzip`, `tls`, `zip`, `readline`, `pfn-wasm`, `zip pfn-wasm
  readline` and `rocksdb` tests stopped running. A new stable release no
  longer changes CI without a commit; bumping the pin means fixing the new
  version's clippy and rustfmt findings in the same commit. Both workflows
  must name the same version: bench.yml's `rocksdb-correctness` job shares
  ci.yml's RocksDB cache (`shared-key: rocksdb`), and rust-cache hashes the
  rustc version and every `CARGO*` / `RUST*` variable into the key.
- **Dev and test builds in CI keep line tables only**
  (`CARGO_PROFILE_DEV_DEBUG` / `CARGO_PROFILE_TEST_DEBUG` =
  `line-tables-only`, also in bench.yml's `rocksdb-correctness`); backtraces
  still show file:line. With full debuginfo, linking the about 95
  executables of the `zip pfn-wasm readline` job filled the runner's disk
  and rust-lld died with SIGBUS. Release builds are unaffected.
- **The feature-matrix jobs build each target once**: their build step uses
  the test profile (`cargo build --locked --profile test -p gstore
  --all-targets`), the one `cargo test` uses, so the test step compiles
  nothing and the job holds one set of builds instead of two.
- **Clippy 1.99 fixes, behaviour unchanged**: `src/query/hash.rs` and the
  rule parser in `src/reason/mod.rs` iterate `as_chunks::<N>()` (stable
  since Rust 1.88, within the MSRV 1.89) instead of `chunks_exact(N)`;
  `Parser::parse_gfn_args` passes its error-context closure by value. No
  `#[allow]` was added.
- No CI job builds on the MSRV (1.89); the Docker image (`rust:1.89`) is the
  only MSRV build. (Since the review-fix campaign, `packaging.yml`'s `image`
  job builds that image, `--features pfn-wasm`, in CI; the default feature
  set, the other features and the tests are still not built on 1.89 there,
  review infra#19.)

### Integrating Phase 4 with Phases 0–3 and 5

Phase 4 (reasoning rules and PFN functions) was developed from the
Phases 0–3 head while Phase 5 (import, gconsole, zip, logging, packaging)
landed; this records what joining them changed.

- **Merge.** Both dependency and feature sets (`readline`, `zip`;
  `pfn-wasm`); `Cargo.lock` is main's plus the optional `wasmi` / `wat`
  packages at the versions Phase 4 locked — its package set is exactly the
  union of both parents' locks. Phase 5's SPARQL `LOAD` runs inside Phase
  4's refactored `run_update` (with the request's PFN caller), its
  `insert_sources` / `remove_sources` use the same insert / delete paths
  as rule execution and retraction, and every served database — built,
  uploaded, restored from a zip — goes through `DbSlot::handle`, which sets
  its rule directory.
- **gconsole and Phase 4.** The console's `addreason` … `deletereason` and
  `showpfn` / `addpfn` / `mdfpfn` / `delpfn` / `buildpfn` commands, written
  against a server that did not implement them, were compared with
  upstream's `gconsole.cpp` handlers and now run whole lifecycles against
  `gserver`. **Fixed:** SPARQL typed at the prompt that projects
  `PFN.name({…})` printed the `{"results": …}` answer as `Update ans num
  0`; it prints the value and `pfn query use <ms> ms.`, as upstream prints
  its `pfn_type` cells. `addpfn` / `mdfpfn` accept a file holding the
  `PFNInfo` object as a JSON string too (upstream's `PFNInfo(json)`); the
  body is WebAssembly text, sent unchanged and URL-decoded by the server as
  upstream does (documented). `showpfn`'s third column names the
  WebAssembly source the server keeps (`src/<user>/<name lower-cased>.wat`)
  where upstream shows `./pfn/<name>.cpp`.
- **Backups.** A database's rules and ownership ledgers travel with it
  through every Phase 5 backup path — `gbackup` / `grestore` (directory and
  `-z` zip), `/api` `backup compress=true` / zip `restore`, `drop`'s `.bak`
  — and a restored executed rule still retracts exactly its triples.
  `export` writes data only, as upstream's. PFN functions belong to users:
  `docs/DEPLOY.md` gains a *Backups* section on backing up
  `pfn_base_path`, which the packaged and container layouts keep in
  `/var/lib/gstore/dbhome/pfn` (the data volume; the unit's
  `ReadWritePaths`).
- **Logging.** Reasoning and PFN events reach Phase 5's diagnostic log
  through the `gs_*` macros, as upstream's `SLOG_*` record them: rule
  execute / disable (info), compiled statements (debug), PFN compile
  (info), calls (debug) and failed calls (warn), stale PFN files that cannot
  be removed and unreadable `data.json` lines. Tested: `reasonManage` and
  the PFN operations are in the `/api` access log, the statement a rule
  executes and PFN queries (`/api` `query`, REST `/sparql`) in the query
  log.
- **Import × reasoning.** Rules over the blank nodes Phase 5's per-load
  scopes mint (explicit and anonymous) execute and retract exactly — every
  ledger line parses back to the stored triple — and `batchInsert` / `LOAD`
  after an execution leave the ledger as it was. Imported `<Rule:…>`
  triples (a tolerant import included) are asserted: no rule owns them.
- **Configuration.** A test reads `conf/conf.ini.example` like an operator:
  every key the code reads is documented in its section and every example
  value parses (the `[query]` / `[pfn]` examples are the defaults); it found
  `[system] licensetype` undocumented, now added. One `refreshconf`
  re-applies `[query]`, `[pfn]` and the `[log]` configuration together;
  `pfn_base_path`, like `db_home`, is read at startup.
- **CI and Docker.** One feature matrix builds (`--locked`), lints and
  tests `gzip`, `tls`, `zip`, `readline`, `pfn-wasm` and `zip pfn-wasm
  readline` together (the separate `pfn-wasm` clippy job is folded in);
  `rocksdb` and the conformance job are unchanged. The Docker image builds
  with `FEATURES="pfn-wasm"` by default (pure Rust, within its 1.89
  toolchain), so its server compiles and runs PFN functions; the default
  `cargo build` stays lean.
- **Docs.** README (features, cargo features, Reasoning, PFN, gconsole,
  configuration, test count), `docs/DEPLOY.md`, `docs/PFN.md`,
  `docs/GAP_PLAN.md` (status notes for Phases 0–5; Phase 6 pending) and
  `docs/CPP_PARITY.md` agree.
- **Tests.** `tests/dt_merge4_wiring.rs`; new cases in `dt_gconsole` (a
  rule's and a PFN function's lifecycle through the console, the PFN call
  part with `pfn-wasm`), `config` and `console::commands`. One expectation
  changed intentionally: the fake-server test's `showpfn` third column.
  The default workspace runs 1,960 tests (with the review fixes below);
  the features add their own.

#### Review fixes

A review of the merged tree found these; each has a regression test
(`tests/dt_merge4_fixes.rs`, and unit tests in `reason::manage`,
`pfn::registry`, `console::commands`, `logging`, `dt_cli_compat`).

- **Rule ledgers read back every term exactly.** Since Phase 5 the readers
  let IRIs holding `>`, `\`, a tab or a newline into a database (as `\u`
  escapes), and rules carried them into their ledgers raw: the strict
  N-Triples reader then refused such a line — disable skipped it, reported
  success and emptied the ledger, leaving the triple for good — or a line
  split by a newline named another triple, which disable deleted; an IRI
  holding a backslash-`u` sequence read back as the IRI it names, so
  disable deleted a user's triple and kept the inferred one. A ledger term
  is now written in an escaped N-Triples form that reads back exactly
  (IRIs and datatypes as the exporters escape them; blank node labels and
  language tags with `\uXXXX`), a line must be three terms and `.`, and a
  line that cannot be read fails the disable — nothing retracted, the
  ledger kept.
- **Execute records before it inserts.** The insert is durable at once on
  the on-disk and RocksDB layouts; the ledger was written after it, so a
  failed ledger write (or a crash) left inferred triples no rule owned.
  The ledger now records what the insert may add first, and is trimmed to
  what it added after.
- **Disable saves a snapshot database before emptying the ledger** (also
  with `check_point` off — upstream saves at this point too); a crash after
  the ledger was emptied left the inferred triples on disk with no owner.
  When the save fails the rule keeps its ledger and status.
- **Add writes a rule's (empty) ledger before its file, and delete removes
  the file before the ledger**, so a crash never leaves a rule file without
  a ledger — which is taken for an upstream rule and retracted upstream's
  way, deleting every `<Rule:label>` triple. Rule-directory copies (backups)
  copy the files before the ledger directory.
- **Incremental backups carry the rules and ledgers of their time**, and
  `restore_incremental` restores the last increment's (an increment taken
  before keeps the base's); the replayed inferred triples were paired with
  the base's rules.
- **Directory swaps wait for `reasonManage` file operations.** The REST
  `/drop`, `/restore`, a rebuilding `/build` and the `/api` `rename` now
  take the rule directory's lock, as the `/api` `drop` and `restore` did;
  an add, compile or delete could otherwise act on a directory being
  replaced (e.g. delete the restored copy's rule and ledger).
- **A rebuild in place drops the old database's rules** (REST `/build` over
  an existing database, `gbuild --force`), as a drop and a build do; they
  stayed executed, their ledgers naming triples the new data holds as
  asserted ones.
- **PFN.** `data.json` is the record: create no longer refuses a name whose
  source file exists without an entry (a failed `data.json` write or a
  crash blocked the name for good) and removes its source when the
  `data.json` write fails; update restores the source then; compile
  removes the old module only once `data.json` names the new one (a failed
  write left the function unloadable after a restart). A module loaded
  while its function was recompiled within the same second is no longer
  cached (it was served from then on). `funDesc`, `funBody` and `funSubs`
  are bounded as stored too (JSON-escaped: a control character takes 6
  bytes), so `data.json` stays within its documented bound, corrected to
  `pfn_max_functions` × (3 × `pfn_max_module_bytes` + 64 KiB).
- **Logs.** Every diagnostic record is one line: line breaks in a message
  (a PFN compile error naming an import, a failing function's output) are
  written as `\n` / `\r` and other control characters as `\uXXXX`, so a
  user can no longer forge records.
- **gconsole.** A PFN value that is an object with a top-level `bindings`
  key prints as the value (it printed `Update ans num 0`); `showreason`
  lists the patterns of `{"condition": {…}}`-wrapped conditions and prefers
  `conditions` to a stray `condition` field; a server's error message
  (which may quote a failing PFN function's output) prints with its
  control characters escaped, as answers do.
- **Docs.** `disablereason`'s printed statement and the query log's record
  of a disable are upstream's nominal `delete where`, not the `DELETE DATA`
  of the ledger that ran (README, `docs/DEPLOY.md`); PFN file names are
  lower-cased (README, `docs/DEPLOY.md`); `compilereason` prints
  `insert_sparql` and `delete_sparql` (the README listed `check_sparql`
  too); `gserver -r` starts the server with the options given to `-r`,
  not those of the server it stopped — repeat `--load-dir`, `-p`, … there
  (README, `gserver -h`).

#### Not yet

- The Docker image with `pfn-wasm` was not built in this environment (no
  Docker daemon); `cargo build --locked -p gstore --bins --features
  pfn-wasm`, what its builder runs, was.

### Integrating Phase 5 with Phases 0–3

- **SPARQL `LOAD` reads through the import pipeline, atomically.** The
  format follows the extension (N-Triples, Turtle, N-Quads, TriG, RDF/XML,
  JSON-LD, RDFa), the document streams in, every `LOAD` gets its own
  blank-node scope, and the `LoadPolicy` still confines local files and
  refuses remote ones. **Fixed:** Phase 5's streaming `LOAD` kept the
  batches it had inserted before a parse error, so a `LOAD SILENT` of a
  broken file reported success over a partial load; a failed `LOAD` now
  undoes what it inserted, and `SILENT` only hides the error. A `LOAD` is
  cancelled (`/cancel`, `operationTaskCancel`, shutdown) between import
  batches and then applies nothing either — `SILENT` does not hide a
  cancel.
- **Changed: `LOAD <src> INTO GRAPH <g>` of a dataset.** Without `INTO`, a
  TriG / N-Quads document keeps its named graphs (Phase 5). With `INTO`,
  its default graph goes into `<g>` and no other graph changes (SPARQL 1.1
  Update §3.1.4 inserts "its triples into the specified graph"; Jena does
  the same): the statements of the document's own named graphs are not
  loaded, and the diagnostic log says how many were skipped.
- **Caches stay coherent.** Every write path Phase 5 added or rewrote —
  `batchInsert` / `batchRemove` from a file, a directory, a URL, an upload
  or a zip package, `LOAD`, `build` from several files, an upload, a URL or
  a zip package, `restore` of a zipped backup — invalidates the query cache
  and the CSR (and the lock-free `csr` status `show` reports) like the
  other writes (`tests/dt_cross_phase5_api.rs`; no fix was needed).
- **Configuration.** `conf.ini` holds Phase 3's `[query]` section next to
  Phase 5's `[log]` keys and `[upload]` zip limits. Databases built,
  uploaded, downloaded or restored by Phase 5's paths take the `[query]`
  settings, and `refreshconf` updates them. **New:** `refreshconf` also
  re-reads the diagnostic-log configuration (`log_mode`'s properties file,
  `log_level`, `log_file`, `log_days`, `log_stderr`, `GSTORE_LOG`) and
  re-installs the log when it changed, as upstream's `refreshconf` re-runs
  log4cplus' `doConfigure` (`Util::configure`). **API:**
  `ApiServer::with_log_refresh` (gserver uses it; an embedded server leaves
  the process-wide log alone).
- **Changed: raw line breaks in Turtle short strings are accepted again.**
  Phase 5 made a `"…"` / `'…'` string stop at a raw line break (W3C
  Turtle), but upstream's loader reads one into the value
  (`TurtleParser::Lexer::lexString`), so files it loads must load here.
  Evidence: upstream's exporters never write one (`NodeUtil::node2string`
  escapes `\n`, `\r`, `\t`; neither do ours), and none of the vendored
  datasets (`testdata/upstream/**`, upstream's `data/**`) holds one.
  Recovery stays per line: a statement whose short string ran across a line
  break and then fails (a missing closing quote, usually) is reported on the
  literal's line and read again from that line break, so the lines after it
  are not swallowed (upstream loses them); a string reaching the end of the
  input, or running more than 256 KiB past its first line break, fails its
  statement the same way. Memory stays bounded by that 256 KiB.
- **gconsole.** Every SPARQL form the server accepts is sent as SPARQL —
  `ADD` / `MOVE` / `COPY`, `WITH … DELETE/INSERT`, `LOAD … INTO GRAPH`,
  `CREATE` / `DROP` / `CLEAR GRAPH`, `CONSTRUCT WHERE`, after a `PREFIX` /
  `BASE` prologue too — while `drop <db>`, `create <db> <file>` and
  `clear` stay upstream's commands; `sparql <file>` sends a run of the new
  update forms as one request. **Changed:** a literal in a result table is
  printed as upstream's console prints it (its `format=n-triple` cell): the
  lexical form unescaped, so a graph function's JSON reads
  `"{"paths":[…]}"`; line breaks and tabs stay escaped to keep the table's
  rows. A lone answer cell is shown as indented JSON only when it is a plain
  literal (upstream tests the cell `"{…}"`). Upstream's console has no
  `loadcsr` command and no `csr` column, so neither does this one.
- **Logs.** Graph-function queries and the Phase 3 update forms are in the
  JSON query log like any statement (REST and `/api`). Phases 2 and 3 added
  no `eprintln!` diagnostics to server paths.
- **CI.** One feature matrix — `gzip`, `tls`, `zip`, `readline` — builds,
  clippies and tests each feature; the `rocksdb` job clippies too; the
  conformance job runs `scripts/conformance.sh`. `docs/DEPLOY.md` documents
  `[query]` and `loadcsr`.
- **Tests.** `tests/dt_load_import.rs` (`LOAD` through the pipeline:
  datasets with and without `INTO`, formats, blank-node scopes, atomic
  failure, cancel, policy; `ADD` / `MOVE` / `COPY` / `WITH` / `USING` /
  `GRAPH` / `DROP` on imported named graphs with blank nodes on the memory,
  disk and RocksDB backends, across a reopen), `tests/dt_cross_phase5_api.rs`,
  `tests/dt_settings_phase5.rs`, and new cases in `dt_gconsole`,
  `dt_logging`, `dt_rdf_import` and the Turtle reader's unit tests.

#### Review of the Phase 5 integration

- **A failed `LOAD` is no change.** Once a failed or cancelled `LOAD` has
  undone its inserts, `Database::mutation_count` (and a CSR built for the
  data before it) is put back: the servers no longer answer "(operations
  before the failing one were applied)", save the database, or drop its
  CSR and caches for a `LOAD` that applied nothing. Nor is an empty named
  graph left behind: **changed**, a named graph whose last statement is
  deleted (by `DELETE`, an undo or a rollback) is gone at once — `GRAPH ?g
  {}` no longer finds it, as it never did after a reopen.
- **Fixed: `DROP` / `CLEAR GRAPH <>`** (an empty IRI, `<>` without a
  `BASE`) cleared every named graph, as if it were `NAMED`; it now names the
  graph `<>`. The redo log marks named-graph records (`REDO <op> <glen>
  <tlen> G`), so such a graph is no longer replayed into the default graph
  (logs without the flag read as before).
- **Fixed: terms a lenient reader lets in are no longer lost.** An IRI
  holding `>`, `\` or a space (written `\u003E`, …) survives `MOVE` / `COPY`
  / `ADD`, `DELETE` / `INSERT … WHERE`, `DESCRIBE` and redo replay
  (`Term::from_dict_key` reads a dictionary key back verbatim; redo records
  escape such IRIs as the exporters do). **Changed:** an empty language tag
  (`"x"@`) is a syntax error in Turtle / TriG / `.nt` files, as in N-Triples.
- **Fixed: a blank-node graph name co-refers with the same blank node.** In
  an N-Quads / TriG document, `_:g` as a subject or object is now the
  graph's `urn:gstore:bnode:` IRI too, so `GRAPH ?g { … } ?g <p> ?o` joins —
  unless `_:g` was used as a subject or object before it named a graph:
  then it stays a blank node throughout (a warning says so; nothing read is
  rewritten and no node is split in two). In a document with millions of
  blank labels, the bounded-memory "used before" check also keeps a graph
  whose label was not used before apart, about 0.5% of the time since the
  review-fix campaign (parser#14; the earlier filter reached 4.5% at 1M
  labels and 51% at 5M); a warning names each such graph.
- **Bounded import memory.** Each quad holds its own copy of every IRI a
  prefix, namespace or base expanded, so a small file with a long prefix
  could take gigabytes. An import batch is now handed over at
  `ImportOptions::batch_bytes` (**new**, default 64 MiB) of terms as well as
  at `batch_size` quads, and one Turtle / TriG statement or one RDF/XML /
  JSON-LD / RDFa document fails when it expands past 64 MiB or 64 times its
  size (`parser::stream::expansion_budget`).
- **Bounded `parse_error.log`.** A failed statement's context is cut to 200
  characters and its message to 512 (`SyntaxError`), so a long prefix no
  longer turns a small file into a log hundreds of times its size.
- **Security.** The single-database mode (`gserver DIR`), which has no
  authentication, refuses to listen beyond loopback unless started with
  **`--allow-unauthenticated`** (new). `remote=true` downloads never reach a
  loopback, link-local or other host-local address unless the load policy
  allows it (`LoadPolicy::allow_local_hosts`, **new**; gserver never sets it;
  `--allow-service` documents that it enables these downloads). A zip
  package named twice in one request is unpacked once, and the packages of
  one request share `unzip_max_entries` / `unzip_max_size`; a URL named twice
  is downloaded once. gconsole escapes control characters (C0, DEL, C1) in
  everything it prints from an answer — table cells, IRIs, headers,
  indented JSON, `pfn_type` text — as `\uXXXX`.
- **Logs.** `refreshconf` tries a log file that could not be opened (at
  startup or by an earlier `refreshconf`) again. **Changed:** the plain-text
  `<db_home>/query.log` and `access.log` follow `querylog_mode` /
  `accesslog_mode` and roll over daily, keeping `querylog_days` /
  `accesslog_days` days. The docs now say that `querylog_path` /
  `accesslog_path` are relative to `db_home` and that the HTTP body cap
  (`upload_max_body_size`) needs a restart.
- **Changed: `upload_allow_extensions` defaults to
  `nt|ttl|n3|rdf|txt|nq|trig`**, so the server's own export of a database
  with named graphs (N-Quads) builds again; with the `gzip` feature
  `x.nt.gz` passes the check as `nt`.
- **`gconsole --local`:** `load <iri> …` / `load silent …` in lower case is
  SPARQL's `LOAD`, not the console's `load <name>`; a non-interactive run
  exits 1 when a statement or command failed.
- **Tests.** `tests/dt_merge5_review.rs`, and new cases in
  `dt_gserver_lifecycle`, `dt_zip_io`, `dt_settings_phase5`, `dt_logging`,
  `dt_gconsole` and the unit tests of `console::table` and `model::term`.
- **Fixed: a refused start no longer blocks the next one.** `gserver`
  refused to serve root's default password beyond loopback only after it
  had created the user store, so a container started without a password
  (as the `Dockerfile`'s own example did) left a store with root at
  `123456` in its volume, and the next start — given
  `GSTORE_ROOT_PASSWORD_FILE`, as the refusal advises — found that store
  and was refused again. It now refuses before creating the store
  (`dt_gserver_lifecycle`); the `Dockerfile`'s example passes a password
  file, as `docs/DEPLOY.md`'s does.

### Integrating Phases 2 and 3 with the Phase 1 server

- **Security limits cover the new grammar.** The SPARQL parser's nesting
  limit (128) and tree-height limit (1024) now also bound every construct
  Phases 2 and 3 added: `[ … ]` / `( … )` triples nodes (their own cap of
  256 is gone — they count towards the global nesting limit, in patterns,
  templates, `CONSTRUCT WHERE` and `INSERT DATA`), `IN` / `NOT IN` lists,
  graph-function and `rs:` calls, sub-selects with a trailing `VALUES`,
  `GROUP BY (… AS ?v)` (each binding is one more node) and the `GRAPH`
  blocks of `DELETE WHERE`. `WITH <g> WITH <g> …` is refused up front
  instead of recursing once per `WITH`.
- **`SERVICE` stays disabled on every evaluation path.** The Top-K / early
  termination / DP-B paths, graph-function queries, named-graph updates
  (`WITH`, `USING`, `GRAPH` templates) and — **fixed** — reads answered
  from an open HTTP transaction's snapshot (which used to contact remote
  endpoints on a restricted server) and `ConcurrentDb` snapshots all follow
  the load policy (`gserver --allow-service` to opt in). `LOAD … INTO
  GRAPH` follows the `LoadPolicy` like `LOAD`.
- **`loadcsr` and `load` with `csr=1` build the CSR** the graph functions
  run on (`Database::preload_csr`); `loadcsr` answers upstream's "CSR
  already loaded." when it is current, and `show` reports `csr` truthfully.
- **New: `conf.ini` `[query]` section** — `integer_division`,
  `query_cache_size`, `early_termination`, `topk_bounded_heap`, `topk_dpb`
  and `gfun_max_output_bytes` — applied to every database the servers
  load, build, create or restore (and re-applied by `refreshconf`).
  **API:** `QuerySettings`, `Database::apply_query_settings` /
  `query_settings` / `set_gfun_max_output_bytes`,
  `Evaluator::with_gfun_max_output_bytes`, `GStoreConfig::query_settings`,
  `ApiServer::with_query_settings`, `Server::with_query_settings`.
- **Changed: every graph function's output is bounded**, not only the path
  family's: the metric and community functions (`PR`, `WCC`, `louvain`, …)
  fail with a "… bytes of output" error past the bound (64 MiB on the
  servers, 256 MiB for an embedded `Database`). Graph-function queries are
  cancelled by `operationTaskCancel`, REST `/cancel` and a graceful
  shutdown like any query.
- **Tests.** The `/api` run of upstream's `parser_test.sh`
  (`tests/upstream_parser.rs`) now runs all 50 cases (33–35, 42 and 50
  were waiting for these phases); it and the in-process suite share one
  vendored copy (`testdata/upstream/scripts/test/parser_test/`) and one
  table of the script's numbers and documented divergences (#15, #17,
  `tests/upstream_scripts/parser_cases.rs`). `tests/dt_cross_phase_api.rs`
  checks that every Phase 1 write path (`batchInsert`, `batchRemove`,
  updates, transaction commit and rollback, `restore`, an overwriting
  `/build`, drop and rebuild) leaves no stale query-cache or CSR answer, and
  that the Phase 3 update forms (`ADD` / `MOVE` / `COPY`, `WITH`, `LOAD …
  INTO`, `INSERT DATA` with `[ … ]`) need the update privilege while
  graph-function SELECTs need only query.
- **Fixed (integration review).**
  - The WHERE clause of a `DELETE`/`INSERT … WHERE` now runs under the
    request's cancel flag. A graph function in a sub-SELECT there used to
    ignore `/cancel`, `operationTaskCancel` and shutdown, which kept the
    database locked and hung a graceful shutdown. A cancelled update
    applies nothing.
  - A cancel that arrives after the WHERE clause (while projecting,
    ordering or aggregating, e.g. in a projected `EXISTS`) now fails the
    query. It used to answer with wrong values, which the query cache then
    kept. A refused `SERVICE` there is also reported, instead of reading as
    `false`.
  - `PPR`, `JaccardSimilarity` and the other metric functions stop as soon
    as their output passes `gfun_max_output_bytes`. They used to build the
    whole (quadratic) output first.
  - The single-database `gserver` cancels the statements still running
    when the shutdown grace period ends, so the final save of unsaved
    changes no longer waits for them.
  - `/api` `show` reads `csr` without the database's lock, and
    `refreshconf` no longer waits for busy databases: it swaps the
    configuration first, and a busy database takes the new `[query]`
    settings the next time it is used (the most recent ones win). A
    database created by `/upload` gets the settings in effect when it is
    registered.
  - `load` reports `csr` as whether the CSR is built (`"1"`/`"0"`), as
    upstream does. It used to echo the request.
  - **Changed:** `format=n-triple` prints literals as upstream does, with
    the lexical form unescaped between quotes, so a graph function's value
    is its JSON once the quotes are stripped. `n-triple-file` keeps
    upstream's escaped form.

### Phase 5 — RDF I/O, gconsole and operations (GAP_PLAN §4.2, §5)

#### RDF import

- **Streaming readers.** Turtle, TriG, N-Triples and N-Quads are read one
  statement at a time from a `BufRead` (`src/parser/stream.rs`). Memory stays
  bounded even for one huge TriG block. RDF/XML, JSON-LD and RDFa are still
  parsed whole, up to 256 MiB per document.
- **One import pipeline** (`src/parser/import.rs`) for every bulk load:
  `Database::build_from_*` / `insert_sources` / `remove_sources`, SPARQL
  `LOAD`, `gbuild` (memory, `--disk`, RocksDB), `gadd` / `gsub`, the `/api`
  `build` / `batchInsert` / `batchRemove`, and `gconsole --local`'s `import`.
- **Tolerant import, as upstream does it.** A bad statement is skipped and
  counted, and the reader resumes on the next line. Failures include
  upstream's numeric literal checks and invalid subjects, predicates or
  graphs. Each one is written to `<db>.db/parse_error.log` in upstream's
  format, with the line it was read from. `success_num` / `failed_num` follow
  upstream. Strict mode stays the library default; `gbuild` / `gadd` / `gsub`
  gain `--strict`.
- **Named graphs are kept** by N-Quads and TriG in the in-memory, disk and
  RocksDB backends. A blank-node graph is stored as
  `urn:gstore:bnode:<label>`. *Behaviour change:* quads are no longer
  flattened into the default graph.
- **Blank nodes are scoped per loaded document**, so two batch inserts never
  merge their `_:b`s. Generated labels round-trip through N-Triples. The
  plain `parse_*` APIs keep labels and mint `genid-<n>`.
- **Sources.** Directories (recursive, sorted, policy-checked) and URLs,
  fetched only when the load policy allows remote sources. `/api` accepts
  `remote=true` and answers 1005 when a download fails. `.gz` files need the
  `gzip` feature.
- **Turtle leniency.** Upstream's local-name characters are accepted (loader
  only). An IRI ends at a space or control character, as in upstream's lexer,
  so a missing `>` costs only its own line; so does a short string missing
  its closing quote (since the integration a short string may hold raw line
  breaks, as upstream's lexer allows, and a failed statement is replayed
  from the first one — see "Integrating Phase 5"). After a bad statement in a
  TriG file, a `{` or `}` on the rest of its line still opens or closes the
  block, so no statement is filed under the wrong graph.
- **RDF/XML.** Internal DTD entities, with expansion budgets (every expanded
  entity value counts); a value may refer to an entity declared after it;
  external entities are refused. `rdf:parseType="Collection"` is supported.
- **Exports load back.** `gexport` / `/api export` write IRIs holding a
  space or another character IRIREF excludes as `\uXXXX` escapes.

#### gconsole

- `gconsole` is now a client of gserver's upstream `/api` (`src/console/`).
  It implements all 43 commands of upstream 1.4's command table, plus `exit`
  and `history`.
- Output matches upstream's: PrettyPrint tables, the `Query ans num` /
  `Build RDF database` / `After inserted triples num …,failed num …` lines,
  and `RDF parse error num N` when the server's tolerant import skipped
  statements.
- Login with `-u` / `-p`, `$GSTORE_PASSWORD`, or a prompt with echo off.
  Other options: `--host`, `--port` (falls back to `conf.ini`, then 9000),
  `-c`, `-e`, `-i`.
- Interactive features: multi-line statements, `#` comments, `>` / `>>`
  redirection, and a persistent history (created mode `0600`). Ctrl-C
  cancels the server's query task.
- `drop <name>` always drops a database and `clear …` clears the screen, as
  upstream; SPARQL's `DROP` / `CLEAR` / `CREATE` are recognised only in
  forms the commands cannot have (`DROP GRAPH <g>`, `DROP SILENT ALL`, …).
  `sparql <file>` sends a run of update operations (`op ; op`) as one
  request. `use system;` is refused for root too (it cannot be queried).
- The optional `readline` feature adds line editing and tab completion.
- `gconsole --local <db>` keeps the embedded REPL; the legacy `gconsole <db>`
  still works. Its `import` command now accepts any RDF format, a directory
  or a URL.

#### Zip packages and export (feature `zip`)

- New optional cargo feature `zip` (zip 9, deflate only, pure Rust); the
  default build gains no dependency. `src/zipio.rs`.
- `/api` `build` / `batchInsert` / `batchRemove` read a `.zip` of RDF files
  (local, uploaded or `remote=true`), unpacked as upstream does into the
  upload directory and removed afterwards; every entry needs an allowed RDF
  extension. Errors carry upstream's zip codes (1–6).
- Safety: a package is checked in full before anything is written. Absolute
  or `..` entry names are refused (zip-slip), symlink entries ignored,
  encrypted entries refused. New `[upload]` keys `unzip_max_entries`
  (default 10000) and `unzip_max_size` (default 10 GiB, also enforced on the
  bytes actually written).
- `backup compress=true` writes `<db>.db_<timestamp>.zip` (upstream's
  layout); `restore` takes it (1005 when it does not unpack); rotation and
  the "newest backup" choice include zips.
- `export` gains `format` (`nt` / `nq`); `compress=true` writes a `.zip`.
  *Behaviour change:* without `format`, a database with named graphs is
  exported as N-Quads (`.nq`) instead of N-Triples that dropped them.
- CLI: `gexport --format nt|nq` and `-z` / `out.zip`; `gbackup -z` /
  `x.zip`; `grestore` of a `.zip`; `gbuild`, `gadd` and `gsub` of a `.zip`
  package (upstream 1.3's `-f x.zip`). `--unzip-max-size` /
  `--unzip-max-entries` change the CLI's limits (`0` = none); `grestore`
  has none by default, so any `gbackup -z` archive restores. `gconsole`'s
  `restore` passes a zip path to the server like a directory.
- Without the feature every zip path answers `1003` (the CLI exits 1),
  naming the feature.

#### Diagnostic, query and access logs

- `src/logging.rs`, no new dependency: levels `error`…`trace`, standard
  error and a file rolled at local midnight into `FILE.YYYY-MM-DD`, with
  retention in days. Configured as upstream through `conf.ini`'s `log_mode`
  (a log4cplus properties file; upstream's two are shipped as
  `conf/slog*.properties.example`), plus the keys `log_level`, `log_file`,
  `log_days`, `log_stderr` and the `GSTORE_LOG` variable. As log4cplus,
  `MaxBackupIndex` deletes no daily file (`MaxHistory` / `log_days` set a
  retention), and the file only ever rolls forward.
- `gserver`'s diagnostics go through it (handler panics, accept failures,
  failed saves; each request at `debug`); its startup lines on standard
  output are also recorded in the file.
- The REST `/sparql`, `/update` and `/tquery` now write upstream's JSON
  query-log record (client IP, answer count, latency, status, format,
  database), so `/api querylog` lists them. Each line of
  `<db_home>/access.log` gains the client IP as a sixth field.

#### Legacy command-line tools

- All ten tools take upstream 1.3's options (`-db`/`--database`,
  `-f`/`--file`, `-p`/`--path`, `-q`, `-l`, `-z`, …) besides their own,
  answer `-h`/`--help`, and exit `0` (done), `1` (failed) or `2` (invalid
  arguments).
- *Behaviour change:* `grestore` takes the database first, then the backup,
  as upstream; the old order fails with a hint and restores nothing.
  `grestore -l NAME` lists its backups.
- `gshow` with no argument lists every database (upstream's
  database/creator/built_time table); `gshow NAME` still shows its counts.
- `gbuild` without a source builds an empty database, and refuses to
  overwrite an existing one unless `--force`. A failed build removes the
  database directory it created, as upstream.
- `gexport -db NAME [-f DIR]` writes `<db>_<timestamp>.nt` (`.nq` / `.zip`
  as chosen); `gbackup -db NAME [-p DIR]` creates
  `DIR/<db>.db_<timestamp>` (default `./backups/`).

#### Packaging

- `Dockerfile` (builder on the MSRV toolchain, `debian:bookworm-slim`
  runtime, user `gstore`, port 9000, volumes `/var/lib/gstore` and
  `/etc/gstore`, a health check on `/api?operation=check`, `--build-arg
  FEATURES`), and `.dockerignore`.
- `packaging/systemd/gserver.service` (graceful stop by `SIGTERM`,
  `Restart=on-failure`, hardening), sysusers.d / tmpfiles.d entries, and
  cargo-deb / cargo-generate-rpm metadata with maintainer scripts.
- `docs/DEPLOY.md` covers the container, the service, the packages, the
  configuration and logs, and the tools.
- `gserver` refuses to listen beyond loopback while `root` still has the
  default password `123456`; `--password-file FILE` /
  `$GSTORE_ROOT_PASSWORD_FILE` set root's initial password without putting it
  on the command line (the container's way).

#### CI

- The feature matrix builds, clippies and tests `gzip`, `tls`, `zip` and
  `readline`.

#### Not yet

- The Docker image and the deb / rpm packages were built and linted but not
  yet installed and run on a live host in CI. (Since the review-fix
  campaign, `.github/workflows/packaging.yml` installs and smokes them,
  review infra#7.)
- `gconsole`'s `backup` / `export` have no compress or format switch (as
  upstream's console); use the `/api` or `gbackup` / `gexport`.
- The legacy REST payload endpoints (`/upload`, the `/batchInsert` body)
  still parse whole and drop graph labels.
- JSON-LD `@graph` with an `@id` is flattened into the default graph.

### Phase 4: reasoning rules and PFN functions (GAP_PLAN §3.2, §3.3)

Upstream gStore 1.4's two user-programmable features reach the `/api`
server: inference rules (`reasonManage`) and personalized graph functions
(PFN: `funquery` / `funcudb` / `funreview`, `PFN.name({…})` in queries).
They were built in parallel and merged; *Joining them* records what the
merge changed. Every deliberate difference from upstream is listed at the
end.

#### Reasoning rules (`reasonManage`)

- **`reasonManage` (types 1-8)** on the `/api` server — add, list, compile,
  execute, disable, show, delete, check upstream gStore 1.4's JSON inference
  rules, with upstream's parameters (`db_name`, `type`, `ruleinfo`,
  `rulename`), response fields (`list`/`num`; `insert_sparql`,
  `delete_sparql`, `check_sparql`; `insert_sparql`, `select_sparql`,
  `AnsNum`; `delete_sparql`, `AnsNum`; `ruleinfo`; `check_sparql`,
  `effectNum`), status strings (`新建`, `已编译`, `已执行`, `已失效`,
  `已校验`), codes and messages. Root (and `admin` users) only, as upstream.
  Rules live in `<db>.db/reason_rule_files/<rulename>.json` (upstream's
  layout; compact JSON, sorted keys), survive unload / load and restarts, and
  are carried by `backup` / `restore` (also the library's `Database::backup`,
  `backup_dir`, `restore_into` and `restore_incremental`) and removed by
  `drop`.
- **Compiler** (`reason::compile`): upstream's `insert`, `delete`, `check`
  and `select` statements byte for byte, relation (`type` 1) and property
  (`type` 0) rules, `logic` 0 (`UNION`) and 1 (join), filters. Inferred
  triples use upstream's `<Rule:label>` predicates.
- **Execution** goes through the database's normal update path (query cache,
  CSR, write-through, update / redo logs, commit point, check_point save,
  query log, task registry and cancellation); refused (`1004`) while a
  transaction is open on the database.
- **Exact retraction**: each rule records the triples it owns
  (`reason_rule_files/inferred/<rulename>.nt`, created empty by add); the
  conditions are evaluated once per execution and exactly what that
  evaluation derived is inserted; disabling deletes exactly those no other
  rule owns. A rule file without a record (written by upstream gStore, in
  any status) is retracted upstream's way from its validated label alone,
  and re-executing it takes its existing triples over. An execution that
  fails having inserted nothing leaves the rule as it was.
- **Hardening**: every pattern / `return` term must be one SPARQL term, every
  filter one bracket-balanced expression, the label part of one IRI, and
  `return` variables must be bound — validated with the crate's own lexer and
  parser, and the compiled statements are parsed again and shape-checked.
  Execute only ever runs the statement compiled from the definition. Rule
  names must be safe file names (an existing rule file's name only has to
  stay inside the rule directory); at most 1000 rules per database, 1 MiB
  per rule file (fixed limits: there are no `conf.ini` keys). Backups copy
  the rule directory under its lock; `drop` / `restore` replace it under it,
  and the file operations are refused (`1007`) while a lifecycle operation
  holds the database.
- **Library API**: `Database::reason_add`, `reason_list`, `reason_compile`,
  `reason_execute`, `reason_disable`, `reason_show`, `reason_delete`,
  `reason_check`, `reason_db_dir`, `set_reason_db_dir`; a database without a
  directory keeps its rules in memory (and writes them into its backups and
  saves); `into_memory` reads a database's rules into memory with it.
- **Fix: RDFS range** no longer infers `"lit" rdf:type C` — a triple with a
  literal subject — from `p rdfs:range C` and `x p "lit"`.

#### PFN personalized functions (`docs/PFN.md`)

- **New: `funquery`, `funcudb`, `funreview` on `/api`** (upstream
  `ApiPFNOperation.cpp`), open to every user (as upstream grants them) and
  acting on the caller's own functions only. `funcudb` `type` `1`/`2`/`3`/`4`
  creates, updates, deletes and compiles, with upstream's messages
  (`"Function create success."`, `"Function build fail: …"`, a compilation
  error answered as the compiler's message, the `1003` parameter errors);
  `funquery` lists upstream's six `funInfo` fields filtered by `funName` /
  `funStatus`; `funreview` returns the generated module text, URL-encoded.
  Statuses `"1"` (not compiled), `"2"` (compiled), `"3"` (failed);
  `funBody` / `funSubs` are URL-decoded as upstream does. Functions are
  stored as upstream stores them, per user: `<pfn_base_path>/src/<user>/
  data.json` (+ the generated source) and `lib/<user>/lib<name><md5>.wasm`.
- **New: `SELECT (PFN.name({…}) AS ?v)` is evaluated** (it was a query
  error): with upstream's projection shape and argument handling
  (`pfnQuery` / `pfnQueryByVar`: the argument canonicalized, `"?x"` values
  replaced by every distinct binding, one call per combination in sorted
  order, the outputs as a JSON array; one call with the argument as written
  when there is no solution). The `/api` `query` operation answers upstream's
  `{"results": …}` document (without `AnsNum` / `OutputLimit` / `ThreadId`
  for a statement starting `select (pfn.`); elsewhere the value is a plain
  literal. A query calls the functions of the user it runs for.
- **Runtime: sandboxed WebAssembly, new optional feature `pfn-wasm`**
  (`wasmi` 2.0.0 with `std`, `validate`, `auto-dispatch` only; `wat` 1.263.0
  without the component model). The default build gains no dependency.
  A body is WebAssembly text wrapped into a module that imports the
  `"gstore"` host interface (`GraphCtx`: vertex / edge / predicate counts,
  per-predicate out- / in-degrees and neighbours, id ↔ string, the
  arguments, `emit*` output); a complete `(module …)` or a
  `wasm-base64:` binary module is accepted too. Compilation validates the
  module and its imports / exports and refuses WASI, other imports and
  start functions. Each call runs in a fresh instance under an instruction
  budget (fuel, checked with the query's cancel flag between slices, so
  `operationTaskCancel` and `/cancel` stop a looping function), a memory cap,
  the `gfun_max_output_bytes` output bound and bounds-checked guest
  pointers; traps are query errors. Without the feature, management works and
  compiling / calling fail with an error naming `pfn-wasm`. CI builds,
  lints and tests the feature.
- **New `conf.ini` `[pfn]` section** (Rust extension): `pfn_fuel_per_call`
  (10^9), `pfn_max_memory_pages` (1024), `pfn_max_module_bytes` (1 MiB),
  `pfn_max_functions` (100), applied with the `[query]` settings (and by
  `refreshconf`); upstream's `[system] pfn_base_path` now places the
  functions (default `<db_home>/pfn`, read at startup).
- **API:** `gstore::pfn` (`PfnRegistry`, `PfnCaller`, `PfnInfo`,
  `PfnLimits`, `GraphCtx` / `CsrGraphCtx`, `ResolvedFn`),
  `Evaluator::with_pfn`, `Database::set_pfn_caller` / `pfn_caller` /
  `set_pfn_limits` / `pfn_limits` / `query_as`, `QuerySettings::pfn`,
  `ApiServer::pfn_registry`. `ConcurrentDb` snapshots, version views and
  transactions, and HTTP transaction snapshots carry the caller.
- **Query cache:** PFN queries are never cached (their result depends on
  the caller and on the function's version) — now documented and tested.
- **Changed tests:** `config::every_key_is_read_from_its_section` reads a
  `[pfn]` section too (it requires every known key), and the
  `QuerySettings` literals of two existing tests gained the `pfn` field;
  an engine test calls graph functions through the new projection-call
  dispatcher; the two tests that expected "PFN not supported yet" expect
  the new "no PFN function registry is attached" error. No assertion was
  weakened.

#### Joining them

- **Rules, graph functions and PFN functions share the caches.** Executing
  or disabling a rule changes the data through the update path, so the
  query cache and the CSR the graph functions and PFN functions run on are
  rebuilt: a cached SELECT, `kHopCount` / `shortestPathLen` and a PFN
  function see the inferred `<Rule:label>` edges after execute and lose
  them after disable, on a CSR preloaded with `loadcsr` too
  (`tests/dt_phase4_wiring.rs`, the PFN part with `pfn-wasm`).
- **Fixed: rules of a database the REST layer built.** A database built or
  created in memory by REST has no home directory, so its rules followed
  the served directory only once a `reasonManage` call had reached it, and
  REST `/backup` until then wrote none. Every served database now keeps its
  rules in the directory it is served from (`DbSlot::handle`); the
  per-call fix-up in `reasonManage` and the extra rule copy in `/api`
  `backup` are gone.
- **Configuration.** `conf/conf.ini.example` documents `[system]
  pfn_base_path` (an upstream key its shipped file leaves out), says that
  `refreshconf` re-applies `[pfn]`, and lists the reasoning rules' fixed
  limits; tested: `refreshconf` changes `pfn_max_functions` for the
  registry and `pfn_fuel_per_call` for a database loaded before it.
- **Privileges.** As upstream's `check_privilege`: a user created with
  `usermanage` holds no privilege and still manages their own PFN
  functions, `reasonManage` stays root's (`1004 "You have no reasonManage
  privilege"`), and `showuser` lists upstream's seven privileges — Phase 4
  adds none. Tested.
- **Docs.** `docs/GAP_PLAN.md` records §3.2 and §3.3 as done (the PFN
  runtime decision: WebAssembly in `wasmi`); `docs/CPP_PARITY.md` gains
  rows for both.

#### Deliberate divergences from upstream

Reasoning rules (also in the README's *Reasoning* section):

- Retraction is exact (see above). Upstream's `delete where {?x
  <Rule:label> ?y.}` also deletes triples asserted with that predicate
  before the rule ran and triples another rule with the same label
  inferred; a triple a user inserts *after* the rule inferred it is the
  same RDF triple and is retracted with the rule here too. Disabling a
  compiled rule that was never executed deletes nothing.
- Rules are validated (see *Hardening*); upstream splices the strings into
  SPARQL verbatim, so a rule could inject SPARQL. Prefixed names are
  refused (a rule has no `PREFIX`es). Rule names must be safe file names
  (no `/`, `\`, `..`, leading `.` or control characters, at most 128
  characters and 240 bytes), and the number and size of rules are bounded.
- Execute runs the statement compiled from the rule's definition, never
  text stored in the file: a rule edited after compiling must be compiled
  again, and compiled fields a client sends with `add` are dropped.
- An executed rule cannot be recompiled (upstream allows it, resets the
  status to `已编译` and so lets the rule be deleted with its triples left
  behind); checking an executed rule keeps it `已执行`.
- A top-level `||` filter among several is parenthesized (upstream's plain
  `&&` join would change its meaning).
- `type` may be a JSON number and `ruleinfo` a string holding the object;
  `list` is sorted by name; a missing `rulename` answers `1003`; a database
  that does not exist answers `1005 "database[<db>] does not exist"`
  (upstream creates its directory).
- Execute and disable are refused (`1004`) while a transaction is open;
  check reads the transaction's snapshot. Execute, disable and check write
  a query-log record and register a cancellable task.
- Upstream's documentation shows the compiled text with extra spaces; the
  text here is what its 1.4 code produces.

PFN functions (all in `docs/PFN.md`):

- WebAssembly text run in a sandbox instead of C++ compiled with `g++` and
  `dlopen()`ed: upstream function bodies must be rewritten; the management
  API, the call syntax and the answers are upstream's.
- Limits where upstream has none: instruction budget, memory cap, module
  size, functions per user, output bounded by `gfun_max_output_bytes`.
- A function returning `0` fails the query (upstream ignores the `bool`).
- A call whose argument names no variable runs once next to WHERE
  solutions (upstream answers `[]`).
- `lastTime` is stamped on create / update / compile (upstream: compile
  only); names are SPARQL words of at most 64 characters; sources live
  under `src/` instead of `cpp/`, modules are `.wasm` instead of `.so`, and
  update deletes the old module (upstream leaves the stale `.so`).
- `funquery` paging (`pageNo` / `pageSize`) is an extension; without
  `pfn_base_path` the functions live under `<db_home>/pfn` (upstream:
  `./pfn/` relative to its working directory).
- Outside the `/api` `query` JSON / file answers a PFN value is a plain
  literal (upstream replaces the result document in every JSON form).

### Phase 3: SPARQL 1.1 grammar and semantics

This phase closes the SPARQL 1.1 grammar gaps of `docs/GAP_PLAN.md` §4.1 and
brings evaluation closer to the specification (§3.4). The new syntax and the
new semantics work together: for example `COPY` into a graph followed by a
`WITH` modify of that graph, `IN` over typed numbers with different lexical
forms, or a trailing `VALUES` in a query that is never cached because it
calls `RAND()` (`tests/dt_sparql_phase3_combo.rs`).

#### New SPARQL syntax

- **`IN` / `NOT IN`** (§17.4.1.9-10) in `FILTER`, `BIND`, SELECT
  expressions and `HAVING`, with the spec's error rules: an error only
  counts when no member is equal, and an empty list is false (true for
  `NOT IN`). Members compare by value (`"01"^^xsd:integer IN (1)` is true).
- **Signed numbers and `true` / `false`** as terms in triple patterns,
  `VALUES` and `INSERT/DELETE DATA` (`:p -5`, `VALUES ?x { +1.5e3 }`). The
  sign is part of the lexical form, so `+7` is stored as `"+7"^^xsd:integer`
  and a pattern `+7` does not match a stored `7` (a `FILTER(?x = 7)` does).
  `:p+ 7` is still a property path.
- **Trailing `VALUES`** after the solution modifiers of SELECT (also in a
  sub-SELECT), ASK, CONSTRUCT and DESCRIBE. Without grouping it joins the
  WHERE solutions; after `GROUP BY` / aggregation it joins the groups after
  `HAVING` and before `ORDER BY`, matching on group keys, projected
  variables and SELECT aliases, and binding any other variable (which the
  SELECT expressions can then use). `VALUES () { () }` works.
- **Blank-node property lists `[ … ]` and collections `( … )`** in subject
  and object position, nested, with `()` as `rdf:nil`. In WHERE patterns
  they match like hidden variables (never shown by `SELECT *`); in
  CONSTRUCT and INSERT templates each solution gets fresh blank nodes; in
  `INSERT DATA` they are fresh blank nodes that do not collide with stored
  ones; in `DELETE DATA`, `DELETE WHERE` and DELETE templates (including
  inside `GRAPH` blocks) they are a parse error, as SPARQL Update requires.
- **`ADD` / `MOVE` / `COPY [SILENT]`** between the default graph and named
  graphs. The same graph on both sides is a no-op; `COPY` and `MOVE` clear
  the destination first; `MOVE` drops the source. A named source graph with
  no triples is missing (an error unless `SILENT`). They roll back with a
  transaction and invalidate the query cache.
- **`CONSTRUCT WHERE { triples }`** short form. Its template's `[ … ]` /
  `( … )` blank nodes are fresh per solution, as in the long form.
- **`ORDER BY` keys** may be bare calls (`ORDER BY STRLEN(?x) COUNT(*)
  xsd:integer(?z)`) as well as `ASC/DESC(…)`, `?var` and `(expr)`.
- **`HAVING` with several constraints** (`HAVING (c1) (c2)`, bare calls
  allowed); bare built-ins such as `BOUND(?d)` or `CONTAINANY(…)` over the
  group now evaluate.
- **`GROUP BY (expr AS ?v)`** binds `?v`; `GROUP BY STR(?o)` (a bare call)
  works.
- **gStore's `CONTAINALL` / `CONTAINANY`** (2 or 3 arguments) over
  `xsd:list` literals, following upstream (a multi-character separator and
  typed / language-tagged items are handled as written rather than
  reproducing two upstream quirks).
- **Prefixed names**: backslash escapes (`ex:a\-b`), `%XX` escapes (kept
  as written), `:` inside local names (`ex:a:b`), and `-` / `.` in prefixes.

#### Stricter parsing

- An empty `GROUP BY` or `ORDER BY`, `ASC`/`DESC` without brackets, tokens
  after a trailing `VALUES`, an empty `_:` label and an exponent without
  digits (`1e`) are now parse errors. `VALUES` rejects variables and blank
  nodes in cells, rows of the wrong width and repeated variables.

#### SPARQL semantics follow SPARQL 1.1 more closely

- **Fixed: built-ins around aggregates.** `ROUND(AVG(?x))`,
  `STR(COUNT(*))`, `(SUM(?a)/COUNT(?a) AS ?avg)`, `IF`/`COALESCE` over
  aggregates and aggregates inside `HAVING` and `ORDER BY` expressions now
  evaluate (they used to be silently unbound). `HAVING`, or an aggregate in
  `ORDER BY`, makes a query an aggregate query even without `GROUP BY`; a
  later SELECT expression may use an earlier alias. `GROUP BY ?v` groups by
  the bound term itself.
- **Fixed: SPARQL Update over named graphs.** The WHERE clause of
  `DELETE/INSERT … WHERE` sees named graphs (`GRAPH <g> { … }`,
  `GRAPH ?g { … }`); templates accept `GRAPH <g> { … }` / `GRAPH ?g { … }`
  blocks (also in `DELETE WHERE`); `WITH <g>` names the default graph of
  the templates and the WHERE clause; `USING` / `USING NAMED` define the
  WHERE clause's dataset; `LOAD <src> INTO GRAPH <g>` loads into `<g>`
  (still subject to the `LoadPolicy`). Template triples that would not be
  RDF (a literal subject, a non-IRI predicate or graph name) are skipped,
  in `CONSTRUCT` too. **API:** `UpdateOp::Modify` gained `with`, `using`
  and `using_named`, its templates are `QuadPattern`s, and
  `UpdateOp::Load` gained `into`.
- **Changed: typed literals are validated.** Ill-typed literals
  (`"abc"^^xsd:integer`, `"300"^^xsd:byte`, `"1e3"^^xsd:decimal`, …; all
  integer-derived types check their range) are stored as they are but are
  never numbers: comparing them with anything but the same term is a type
  error, arithmetic on them is unbound, their effective boolean value is
  false. Valid literals compare by value (`"01"^^xsd:integer = 1`). An IRI
  compared with `=`/`!=` to a literal is now simply unequal (it used to be
  an error). `STR`, `DATATYPE`, `sameTerm` and the copy `BIND(?v AS ?w)` /
  `(?v AS ?w)` binds see a variable's or constant's term as written
  (`"05"^^xsd:unsignedByte` stays that term, with `STR` `"05"`), so
  `sameTerm("01"^^xsd:integer, 1)` is false.
- **Changed: numbers.** `xsd:float` keeps its datatype through expressions
  (`float + integer` is a float), and an integer or decimal meeting a float
  is promoted to single precision (`"0.1"^^xsd:float = 0.1`). Dividing two integers gives an
  `xsd:decimal` (`7/2` = `3.5`; it used to be an `xsd:double`), and so does
  `AVG` over integers; dividing a double by zero gives `INF`/`NaN`. `ABS`,
  `CEIL`, `FLOOR` and `ROUND` keep their argument's type, and `ROUND` rounds
  halves up (`ROUND(-2.5)` = `-2`).
- **New: upstream integer division.** `Database::set_integer_division(
  IntegerDivision::Truncate)` (and `Evaluator::with_integer_division`)
  switches to gStore's truncating integer division (`7/2` = `3`, also in
  `AVG`). `conf.ini` key (since the Phase 1 merge): `[query]
  integer_division = decimal | truncate`.
- **Changed: the query cache is bounded by memory.** It holds about 64 MiB
  of results by default (`Database::set_query_cache_size(bytes)`, 0 turns it
  off; `set_query_cache_capacity` is gone) and never caches queries using
  `NOW()`, `RAND()`, `UUID()`, `STRUUID()`, `BNODE()`, `SERVICE` or a user
  function. `Database::query_cache_len()` / `query_cache_bytes()` report its
  use. `conf.ini` key (since the Phase 1 merge): `[query]
  query_cache_size` (bytes). A CONSTRUCT
  whose template has `[ … ]` / `( … )` nodes is not cached either (it mints
  fresh blank nodes on every run).
- **Fixed: snapshot readers use the integer-division mode.**
  `ConcurrentDb` snapshots (`query`, transactions, version views) and the
  HTTP API's transaction read snapshot now divide integers in the
  database's mode instead of always using the default.
- **Fixed: `SELECT *` leaves out variables that only a `MINUS` mentions**
  (§18.2.1: MINUS adds no in-scope variables).
- **Fixed: a variable used as a predicate and bound some other way.**
  Predicates and entities are numbered by separate dictionaries, so a
  variable both in predicate position and as a subject/object, or bound by
  `UNION`, `VALUES`, `BIND`, a sub-SELECT or `MINUS`
  (`?s ?p ?o . ?p rdfs:label ?l`), compared ids from the two numberings and
  matched the wrong terms. Such a variable now always holds the term's
  entity id, its predicate positions bind a hidden predicate-id slot, and
  the join translates between the two (still index-driven). A sub-SELECT's
  predicate column joins with the same IRI bound elsewhere.
- **Fixed: string functions keep the language tag** (§17.4.3): `UCASE`,
  `LCASE`, `STRBEFORE` / `STRAFTER` (an empty simple literal when there is
  no match) and `CONCAT` when every argument has the same tag.

#### Top-K and early termination (GAP_PLAN §3.4)

Every optimization below returns exactly the rows of the full evaluation,
in the same order, ties included (`tests/dt_topk.rs` checks each query with
the optimizations off, heap-only, default and DP-B forced, over randomized
data). Side-effecting expressions (`RAND()`, `BNODE()`, `STRUUID()`, user
functions) are now evaluated only for the solutions actually consumed.

- **New: early termination.** A SELECT or CONSTRUCT with `LIMIT` and no
  `ORDER BY` streams its WHERE solutions and stops after OFFSET+LIMIT rows
  (distinct rows for `SELECT DISTINCT`); `ASK` and `EXISTS` / `NOT EXISTS`
  stop at the first solution; the graph-function `LIMIT` pushdown streams
  its first LIMIT solutions. BGPs (left-deep and bushy plans, and the
  mixed-variable pipeline above), `FILTER`, `BIND`, `UNION` and `GRAPH`
  stream fully; the left side of `OPTIONAL` / `MINUS` streams; a group of
  several conjuncts evaluates each conjunct, then streams the join;
  `VALUES`, sub-SELECTs, property paths and `SERVICE` are evaluated in full.
- **New: bounded-heap `ORDER BY … LIMIT`** (also for CONSTRUCT and with
  `DISTINCT`): O(n log k) time and O(k) memory instead of a full sort.
- **New: DP-B Top-K**, a port of upstream's DPB / FR / FQ / OW lists, for
  `ORDER BY` of an integer linear expression (`?a`, `2*?a - ?b + 3`, …)
  over a tree-shaped BGP (optionally under `FILTER`). It enumerates matches
  in key order and stops after the k-th and its ties. By default it runs
  only when the store's per-predicate statistics predict at least twice as
  many matches as values it must read (a star or one-to-many edge); cycles,
  non-integer values, a predicate variable that is also a subject/object,
  and other shapes take the heap. Upstream's quirks are not copied: it drops
  non-numeric values (SPARQL sorts them first / last), emits ties in its
  internal order, and misranks matches when a scored variable has children.
- **New API:** `TopKOptions { early_termination, bounded_heap, dpb:
  DpbMode::{Off, Auto, Always} }` (`ALL`, the default, and `NONE`),
  `Evaluator::with_topk_options`, `Evaluator::stats` / `reset_stats`
  (`EvalStats` work counters), and `Database::set_topk_options` /
  `topk_options` (clears the query cache; carried into `ConcurrentDb` and
  HTTP transaction snapshots). All are re-exported from `gstore::query`.
- **Benchmark:** `cargo run --release --example topk_bench` compares full,
  heap-only, default and forced-DP-B evaluation and checks the rows agree.
  Measured at opt-level 1 on 100k–200k solutions: `LIMIT 10` 65 ms → 1 ms,
  `ASK` 115 ms → 0.8 ms, `ORDER BY … LIMIT 10` on a fan-out-20 star 469 ms
  → 3.3 ms.
- Cancellation is polled per BGP step, per filtered solution and per DP-B
  match; a cancelled query still reports `Cancelled` on every path.

#### Upstream parser test suite

- **New: `tests/dt_upstream_parser_suite.rs`** runs upstream gStore's
  `scripts/test/parser_test.sh` (50 SPARQL cases, vendored byte-for-byte
  with upstream's BSD-3 licence under
  `testdata/upstream/scripts/test/parser_test/`, the copy the `/api` suite
  `tests/upstream_parser.rs` uses too) in-process: each case imports its Turtle file and checks the triple
  count, the answer count and the rows against upstream's result file.
  **50/50 pass**, each with Top-K / early termination both on and off (the
  two runs must give the same answer). 48 match upstream exactly; cases 15
  and 17 follow W3C and upstream's own result files where the script's
  expected count does not (listed with the reason in one `DIVERGENCES`
  table). No case is deferred to Phase 5. Run it with the per-case table:
  `cargo test --test dt_upstream_parser_suite -- --nocapture`.

#### Known gaps

- A labelled blank node `_:b` in a WHERE pattern still matches only that
  stored node (upstream's reading), and in `INSERT DATA` it names the same
  node for every request. (Since the review-fix campaign a labelled `_:b` in
  a CONSTRUCT / INSERT template is a fresh node per solution, query#5, and
  blank nodes are refused in `DELETE DATA`, `DELETE WHERE` and DELETE
  templates, sparqlconf#16; `[ … ]` and `( … )` were already fresh.)
- CONSTRUCT, ASK and DESCRIBE do not accept `GROUP BY` / `HAVING`; DESCRIBE
  ignores `ORDER BY` and `LIMIT`. `FROM` / `FROM NAMED` are skipped; `BASE`
  resolution is concatenation, not RFC 3986.
- `SELECT (expr AS ?v)` / `BIND(… AS ?v)` with `?v` already in scope is not
  rejected at parse time.
- `SUM` / `AVG` skip non-numeric values instead of failing (since the
  review-fix campaign `AVG` divides by the number of values it summed,
  query#7, and `COUNT(DISTINCT …)` compares terms, so `"01"^^xsd:integer`
  and `"1"^^xsd:integer` count as two, query#12).
- Since the review-fix campaign the string and hash functions check their
  argument types (sparqlconf#17); still lenient: `ENCODE_FOR_URI`,
  `LANGMATCHES`, `STRDT`'s lexical form, the gStore extensions
  (`CONTAINALL`, `CONTAINANY`, `SIMILARITY`) and `SUBSTR` positions of any
  numeric type.
- DP-B handles tree-shaped BGPs and integer keys only (upstream also cuts
  one cycle); other `ORDER BY … LIMIT` queries use the bounded heap. The
  join-order cost model does not yet know that a mixed predicate variable's
  hidden slot and the variable are linked.

### Phase 2 (breaking): SPARQL graph functions follow gStore 1.4

The advanced graph functions of upstream gStore 1.4 (`docs/en-us/SPARQL_QUERY.md`,
"Advanced functions") now parse and evaluate with upstream's signatures,
semantics and output. Their names used to call gstore-rust's own graph
built-ins, which had different arguments and results. Those built-ins still
exist with unchanged behaviour, but you now call them through the `rs:`
namespace.

- **New: 32 upstream functions** — `simpleCyclePath`, `simpleCycleBoolean`,
  `cyclePath`, `cycleBoolean`, `shortestPath`, `shortestPathLen`,
  `shortestPathCount`, `shortestPaths`, `kHopReachable`, `kHopReachablePath`,
  `kHopEnumerate`, `kHopCount`, `kHopNeighbor`, `kHopAllNeighbors`, `bfsCount`,
  `SSSP`, `SSSPLen`, `PR`, `PPR`, `closenessCentrality`,
  `betweennessCentrality`, `triangleCounting`, `diameterEstimation`, `WCC`,
  `labelProp`, `louvain`, `maximumKplex`, `coreTruss`, `clusterCoeff`,
  `JaccardSimilarity`, `degreeCorrelation` and `confidenceDegree`.
  - **Where they go.** Write each one as a SELECT projection,
    `SELECT (shortestPath(?x, <b>, true, {<p>, !<q>}) AS ?v) WHERE { … }`.
  - **Arguments.** They take upstream's predicate sets (`{}`, `{<p>}`,
    `{!<p>}`), named parameters (`alpha=`, `maxIter=`, `tol=`, `increase=`)
    and a `directed` flag.
  - **Results.** A function returns upstream's JSON text (`{"paths":[…]}`) as a
    plain literal; `cycleBoolean` returns an `xsd:boolean`.
  - **Variable arguments.** A variable stands for all of its distinct values in
    the WHERE solutions. With two variables, the function runs over their
    cross product.
  - **Rows.** The function runs once, so a lone function returns one row.
    Next to other projections, only the last row holds the value. With
    `GROUP BY`, it runs once per group.
- **New: a per-predicate CSR for each database.** It leaves out literals,
  just as upstream's does. The database builds it on the first
  graph-function query after a data change, and later queries share it.
  `Database::preload_csr()` builds it in advance, like upstream's `loadcsr`.
- **Changed: the legacy functions moved to `rs:`.** gstore-rust's earlier
  graph functions now live in the IRI namespace `<urn:gstore-rust:fn:>`,
  which is predeclared as the prefix `rs:`. A query's own `PREFIX rs:`
  replaces it. The functions can still be used anywhere (FILTER, BIND,
  SELECT), for example `rs:SSSPLEN(?a, ?b)`, `rs:WCC(?a, ?b)` or
  `rs:PPR(?s, ?t)`. A bare call in the old form, such as `SSSP(?a, ?b)` or
  `SHORTESTPATHLEN(?a, ?b)`, is now a parse error that names the `rs:`
  replacement. `CLUSTERINGCOEFF` and `KHOPENUMERATEPATH` have no upstream
  namesake, so they still work without the prefix. The README has a
  migration table.
- **Changed: `SIMILARITY(a, b)` is now upstream's Jaro string similarity.**
  It compares the arguments' term strings. The old node/string similarity is
  `rs:SIMILARITY(a, b)`.
- **Changed: graph functions inside expressions are an error.** This covers
  the four functions upstream accepts inside FILTER (`cycleBoolean`,
  `simpleCycleBoolean`, `shortestPathLen`, `kHopReachable`). Upstream parses
  them there but never evaluates them, so here they fail with a message that
  points to the `rs:` function instead.
- **New: `PFN.name({…})` calls parse.** Evaluating one fails with "PFN not
  supported yet" for now.
- **New: XSD cast functions.** SPARQL 1.1's `xsd:string`, `xsd:integer`,
  `xsd:decimal`, `xsd:double`, `xsd:float`, `xsd:boolean` and
  `xsd:dateTime`, plus `xsd:date`. A cast that SPARQL does not allow
  leaves the value unbound.
- **Changed: other IRI function calls must name a registered function.** The
  call reaches a function registered with `Database::register_function`
  under its full IRI or its local name; anything else is a query error
  (it used to evaluate to unbound). Registered functions now also work in
  the WHERE clause of `DELETE`/`INSERT … WHERE`.
- **Changed: read snapshots answer like the database.** A transaction's read
  snapshot (HTTP `/begin` … `/tquery`) and the `ConcurrentDb` snapshot,
  transaction and version-view reads now see the functions registered with
  `Database::register_function` (they used to evaluate such calls to
  unbound, which the previous item would have turned into an error). Their
  graph-function queries share one CSR per snapshot, reusing the database's
  cached CSR when it has one, instead of rebuilding it on every query.
- **Fixed: infinite `xsd:double` values print as XSD lexicals.** They are
  written `INF` and `-INF` (they used to print as Rust's `inf` and `-inf`);
  NaN stays `NaN`.

### Phase 1: upstream gStore 1.4's `/api` protocol and server

The multi-database server speaks upstream gStore's wire protocol, so the
upstream SDKs and test scripts run against it unchanged
(`scripts/conformance.sh`).

- **New: the `/api` protocol** (`src/gapi`): `GET|POST /api`, `POST /`,
  `/grpc/api` and `GET /?operation=` dispatched by operation name with
  per-request credentials (`encryption=1` MD5 too) and upstream's HTTP-200
  `StatusCode` envelope, upstream's response fields, codes and messages
  (upstream's *code* wins where its docs differ). Operations: `check`,
  `login`, `testConnect`, `getCoreVersion`, `stat`, `init`, `refreshconf`;
  `query` (JSON / file formats, `max_output_size`), `begin`, `tquery`,
  `commit`, `rollback`, `checkpoint`; `show`, `load`, `loadcsr`, `unload`,
  `monitor`, `build`, `drop`, `rename`, `backup`, `backuppath`, `restore`,
  `export`, `batchInsert`, `batchRemove`, `schema`; `showuser`,
  `usermanage`, `userprivilegemanage`, `userpassword`, `ipmanage`,
  `querylog`, `accesslog`, `txnlog`; `checkOperationState`,
  `operationTaskCancel`, `operationTaskList`; `/file/upload` and
  `/file/download`. The SDKs' encoded `?operation=…` targets are decoded as
  upstream does.
- **New: `conf.ini`** (`src/config.rs`, `conf/conf.ini.example`): every key
  of upstream's file with upstream's defaults; unknown keys and bad values
  warn. Precedence: command-line flag > `conf.ini` > default.
- **New: `gserver` lifecycle** as upstream's: `-s` / `-b` (background) /
  `-t` / `-r` / `-k` / `-S`, `-c conf.ini`, `-p`, `--db-home`,
  `--thread-num`; a pid file with a per-run `system` password; the
  `deamon=on` supervisor (gives up on a crash loop); `/shutdown` and
  `/restart` routes. The default mode is the multi-database API rooted at
  `db_home`.
- **New: HTTP layer** — a bounded worker pool (`thread_num`, a queue with
  back-pressure), HTTP/1.1 keep-alive, CORS / `OPTIONS`, a request body cap
  (`upload_max_body_size`, 413), chunked bodies, `Expect: 100-continue`,
  and a deadline on the whole request head.
- **New: system database** (`src/sysdb.rs`): per-database privileges
  (`query@lubm`, …) with upstream's `check_privilege` table, a database
  registry, IP allow / deny lists and the failed-login lockout (REST
  included), and upstream's query / access / transaction logs.
- **New: asynchronous tasks** (`src/gapi/tasks.rs`): `async=true`
  operations get an `opt_id`, run on bounded runner threads, keep their
  results (bounded, with a TTL) and POST them to a `callback` URL (HTTPS
  with the `tls` feature; host-local addresses refused). Running SPARQL
  statements and operations share one registry with REST `/tasklist` and
  `/cancel`; a shutdown cancels them and waits, bounded.
- **New: database lifecycle and confinement** — every server-side path is
  canonicalized and confined to its configured root; an inter-process
  `LOCK` on each database directory (a second process gets a clear error;
  `drop`, `rename`, `restore` and a rebuilding `/build` take it first);
  `max_database_num` enforced by every path that creates a database;
  backups record their database (`BACKUP_OF`) and a non-admin restore takes
  only its own database's backups.
- **Changed: transaction ids are numbered per loaded database** (the first
  after a load is `1`, as upstream's per-database `Txn_manager`); REST
  requests may name `db_name` to disambiguate.
- **Security** (`8d48fb5`): password checks run off the server's state
  lock (bad credentials cannot stall other clients); SPARQL nesting is
  bounded (128 levels, a 1024-node tree height) and HTTP workers get an
  explicit stack, so a deeply nested request is a 400, not a process-wide
  stack overflow; RDF/XML nesting is limited; `/build?db_path=` follows the
  `LoadPolicy`; `/backup` writes only inside the backup directory and
  `/restore` reads only from there or allowed directories; federated
  `SERVICE` is refused on servers unless `--allow-service` (the client has
  a 30 s deadline and a 64 MiB response cap); changing a password revokes
  the user's sessions.
- **Conformance suite**: upstream's test data, `parser_test` and
  `bfs_test` vendored under `testdata/upstream/`; Rust ports of
  `basic_test.sh`, `parser_test.sh`, `bfs_test`, `update_test.sh`,
  `transaction_test.sh` and `api_test.sh` (`tests/upstream_*.rs`); and
  `scripts/conformance.sh`, which runs upstream's own scripts and Python
  SDK against `gserver` (a CI job).
- Known differences from upstream (README): a database is single-writer
  (one open transaction; other writes answer `1004` meanwhile), and zip
  packages are not supported yet.

### Phase 0: durability, data safety and hardening

- **Persistence.** On-disk (`kvstore.kv`) databases write every committed
  mutation through to the store (WAL commit points; named graphs in a quad
  tree); RocksDB databases keep their dictionary, named graphs and name in
  column families, sync at commit points and make transactions
  crash-atomic (an undo journal in the same write batch); snapshot saves
  are crash-atomic as a set. `Database::open` detects the layout; every
  CLI tool and server opens databases with it.
- **Restore** stages and validates a backup before swapping it in as a
  whole directory, so a corrupt backup no longer destroys the target.
- **HTTP servers** are durable: the `check_point` policy saves every write
  before it is acknowledged (`gserver --no-checkpoint` defers saving), a
  graceful shutdown (SIGINT / SIGTERM, `ShutdownHandle`) waits for
  in-flight requests and saves, partial-update failures are saved and
  reported, no handler waits for a database lock while holding the state
  lock, each request is isolated with `catch_unwind`, and transactions
  give non-owners a begin-time snapshot (no dirty reads).
  `/batchInsert` / `/batchRemove` apply parsed triples, never SPARQL text
  built from input.
- **`LoadPolicy`** for SPARQL `LOAD`: permissive for the library, but
  `LoadPolicy::restricted()` on servers (`gserver --load-dir DIR` re-allows
  a directory); sources are canonicalized and confined, and denials do not
  reveal whether a file exists.
- **Hardening.** Bounded dictionary id spaces (entities, literals,
  predicates) with checked `try_insert_*` paths instead of silent
  misclassification; built-in function arity checked at parse time and the
  crafted-query panics removed (`STR()`, `ORDER BY RAND()`, overflows);
  stable, versioned PBKDF2-HMAC-SHA256 password hashes (legacy hashes
  migrate on login) and atomic `users.db` writes.
- **Cluster** (experimental): single-write RPC framing, client writes
  acknowledged only once committed, per-peer replication senders, fallible
  shard reads; a CI workflow.
