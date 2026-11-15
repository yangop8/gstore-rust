# r1-http-surface — Content-Type-aware request parameters and the browser attack surface (CORS, GET update, Host/Origin, default root password, proxy trust)

## Changelog

Behaviour changes (secure defaults; each has an upstream-compat switch in
`[server]` of `conf.ini`, and gserver logs `upstream-compat: <key>=<value>`
at startup for every switch that is on):

- **Request parameters follow the `Content-Type`** (review H-9: design#1,
  http#5). Both HTTP servers (the single-database `Server` and the REST
  endpoints of `ApiServer`) read parameters from the URL query string and
  from a form body only: `application/x-www-form-urlencoded`, or a body
  with no `Content-Type` that is shaped like a form (only form characters,
  no whitespace / braces / angle brackets / quotes). An
  `application/sparql-query` or `application/sparql-update` body, and any
  other raw body (RDF, JSON, `text/plain`, unlabelled SPARQL text), is the
  statement or document verbatim. An IRI or literal containing
  `&update=…`, `&query=…`, `&sparql=…`, `&db_name=…` or `&db_path=…` no
  longer runs a different statement (`INSERT DATA { … <http://ex/x?a=1&update=CLEAR%20DEFAULT> }`
  used to run `CLEAR DEFAULT`), switches the database or names a
  server-side file. A form body without `query=` / `update=` is now a
  missing statement (400) rather than SPARQL text. The upstream `/api`
  protocol keeps upstream's own parameter rules (`gapi::params`).
- **Session tokens are read from the request head or the URL only**
  (http#4): `Authorization: Bearer`, `X-Session-Token`, or a `session=` /
  `token=` URL parameter. A token in the body is no longer looked for, so
  authentication never scans (or copies) an unauthenticated body; form
  lookups scan a form body once, without copying it, and never look into a
  raw body. REST `/login` reads its `username` / `password` with the
  server-wide state lock released (it used to scan the body with the lock
  held; a `Basic` header on `/login` is not looked at), and the `415`
  check of `/update` decides by the `Content-Type` alone, before
  authentication, without copying the body.
- **Dataset parameters are refused** (gap-doc-claims#2): `/sparql` with
  `default-graph-uri` / `named-graph-uri`, and `/update` with
  `using-graph-uri` / `using-named-graph-uri`, answer `400 … dataset
  parameter … is not supported` on both servers, instead of silently
  evaluating over the whole default graph. An empty value (a blank
  "default graph" field of a query form: `default-graph-uri=`) names no
  graph and is ignored, as before this round (round-1 review fix: it was
  refused too; test
  `tests/http_request_semantics.rs::empty_dataset_protocol_parameters_are_ignored`).
- **GET never runs a SPARQL UPDATE; `/update` takes typed bodies** (H-10:
  http#2, design#2, sec#8): `GET /sparql` with an UPDATE answers `400` (it
  is classified by a parse alone, before anything runs; only texts that
  contain an update keyword or a backslash are parsed, so a GET query is
  not parsed twice), and `POST /update` with a `text/plain` or
  `multipart/form-data` body — besides forms, the types a web page can
  POST cross-site without a CORS preflight — answers `415`, on both
  servers. Accepted: `application/sparql-update`, a form, an unlabelled
  body, or any other type (a browser sends those only after a preflight,
  which follows `cors_allow_origin`), taken verbatim. An UPDATE POSTed to
  `/sparql` still runs (REST: with the Update privilege). Compat switch:
  `allow_get_update=on` (restores both upstream behaviours; the two types
  are then taken verbatim, never parsed as forms).
- **CORS only for listed origins** (H-10, infra#3): no response carries
  `Access-Control-Allow-Origin` by default (it used to be `*` on every
  response). `cors_allow_origin` lists origins (`scheme://host[:port]`,
  separated by `|`, `,` or spaces) that get their own origin echoed (with
  `Vary: Origin`); `*` restores upstream's `Access-Control-Allow-Origin: *`.
  Preflights follow the same list: a listed origin gets `200` with the
  `Access-Control-Allow-*` headers, any other origin `403` without CORS
  headers, a preflight without `Origin` `200` without
  `Access-Control-Allow-Origin`. `cors=off` still turns CORS processing off
  (OPTIONS then reaches the router). The shipped `conf.ini.example` no
  longer turns on `ACAO *`.
- **DNS-rebinding guard** (H-10): a request arriving over loopback must
  name a local host in its `Host` header — an IP literal, `localhost` /
  `*.localhost`, a name without a dot, the configured `listen_addr` when it
  is a name, or a name listed in the new `allowed_hosts` (`*` turns the
  check off). Any other name answers `403` (`/api`: HTTP 200 with
  `StatusCode` 1101). A request without `Host` is accepted. Requests from
  non-loopback peers are not checked.
- **CSRF guard** (H-10): a state-changing request (any method but GET,
  HEAD, OPTIONS) carrying an `Origin` header that names neither the server
  itself (`http://` or `https://` followed by the request's own `Host`) nor
  an origin of `cors_allow_origin` answers `403` (`/api`: 1101), on both
  servers. Clients that send no `Origin` — curl, the SDKs, the upstream
  scripts, gconsole — are unaffected. The bundled web consoles POST
  `/sparql` from the page, so the browser sends the page's `Origin`: it
  matches when the console is opened at the server's own address. Behind a
  reverse proxy that rewrites `Host` to the upstream address (nginx's
  default `proxy_pass`), the public `Origin` matches no `Host` and the
  console's queries are refused (`403`): list the public origin in
  `cors_allow_origin`, or let the proxy keep the client's `Host` (nginx
  `proxy_set_header Host $host;`) and list that name in `allowed_hosts`
  (test `tests/http_browser_surface.rs::a_host_rewriting_proxy_needs_the_public_origin_listed`).
  A web front end served from another origin (e.g. upstream's Workbench)
  needs its origin in `cors_allow_origin`.
- **Proxies are not local clients** (sec#9, gapi#22): one predicate,
  `server::peer_is_trusted_local`, decides whether a client is local: a
  loopback peer that forwards `Forwarded`, `X-Forwarded-For` or
  `X-Real-IP` is a reverse proxy speaking for remote clients. It loses the
  loopback privileges: the unauthenticated single-database server refuses
  it (`403`), root's default password is refused from it (REST Basic and
  `/login`: `401`; `/api`: `1001`, plaintext or `encryption=1`), and the
  `/api` layer's `trust_inner_localhost` no longer applies to it. Compat
  switch: `trust_proxy_headers=on` (every loopback peer is local, as
  upstream). The detection relies on the proxy adding one of those
  headers: a same-host proxy that adds none (nginx without
  `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`, HAProxy
  without `option forwardfor`, stunnel, ssh / socat tunnels) cannot be told
  apart from a local client. The default-password refusal is a constant comparison (it
  tells nothing about root's actual password) and does not count towards
  the failed-login lockout (which would lock out every client behind the
  proxy).
- **gserver refuses root's default password on every address** (H-10,
  infra#3, sec#9): in API mode, a root that still has `123456` (a new user
  store seeded with it, or an existing one) makes gserver refuse to start
  on loopback too (it used to warn there). The message names both
  remedies. Compat switch: `allow_default_root_password=on` — gserver then
  starts on a loopback address only (still refused beyond loopback) and
  the password is accepted from local clients only.
- **Packages set a random root password on a fresh install**: when no user
  store exists under `/var/lib/gstore` yet and `/etc/gstore/conf.ini` still
  names the default password, the deb postinst and the rpm `%post` write a
  random 32-hex-digit password to `/etc/gstore/root_password` (`0640
  root:gstore`; written beside it, synced and renamed into place, every
  step's status checked) and print where it is (not the password). The
  systemd unit sets `GSTORE_ROOT_PASSWORD_FILE=/etc/gstore/root_password`,
  so gserver seeds the user store with it at the first start. `conf.ini`
  is left byte-identical: it is a dpkg conffile, which maintainer scripts
  must not change (Debian Policy 10.7.3; a rewritten conffile made every
  later upgrade that ships a new `conf.ini` stop at the conffile prompt).
  Round-1 review fix: the block first rewrote `conf.ini` in place (`cat >`,
  unchecked in the rpm scriptlet, so a failed write left it truncated and
  still printed success). A deb purge removes the password file. gserver
  skips a file that `$GSTORE_ROOT_PASSWORD_FILE` names but that does not
  exist, with a warning (an upgrade, or the file deleted after root's
  password was changed: the user store exists; a new store is seeded from
  `conf.ini`'s `root_password`, whose default gserver refuses);
  `--password-file` must still be readable. Upgrades, existing user stores
  and passwords already set are left alone. Test:
  `tests/packaging_first_start.rs` (its fresh-install assertions pinned the
  rewritten `conf.ini` and are corrected).
- **Upgrade note**: an API server whose stored root password is still
  `123456` refuses to start after the upgrade. Change the password first,
  or start once on `127.0.0.1` with `[server]
  allow_default_root_password=on`, change it (gconsole `setpswd`, `/api`
  `userPassword`), and remove the switch. Behind a reverse proxy on the
  same host: make it add `X-Forwarded-For` (nginx: `proxy_set_header
  X-Forwarded-For $proxy_add_x_forwarded_for;`, which its default
  `proxy_pass` does not send) so its clients are not taken for local ones.
  If the proxy passes the client's `Host` (Caddy; nginx with
  `proxy_set_header Host $host;`), list the public name in `allowed_hosts`.
  If it rewrites `Host` to the upstream address (nginx's default
  `proxy_pass`), the `Host` check passes, but a browser page served through
  it — the bundled web console included — sends its public `Origin`, which
  matches no `Host`: list that origin in `cors_allow_origin`. Add every
  other web origin that calls the server from a browser to
  `cors_allow_origin` too.

New configuration keys (`[server]`, all documented in
`conf/conf.ini.example` as commented-out lines showing the secure
default): `cors_allow_origin` (default empty), `allowed_hosts` (default
empty), `allow_get_update` (default off), `trust_proxy_headers` (default
off), `allow_default_root_password` (default off). New `HttpOptions`
fields with the same names (but `allow_default_root_password`, a gserver
startup rule) for embedded servers. New
`GStoreConfig::upstream_compat_warnings()`.

Tests: `tests/http_request_semantics.rs` (H-9, http#4, gap-doc-claims#2),
`tests/http_browser_surface.rs` (H-10, sec#9/gapi#22, the gserver
default-password rule), `tests/packaging_first_start.rs` (the maintainer
scripts' first-start block, run with `sh` against a temp directory).
`scripts/conformance.sh` gained the shared compat block and turns on
`allow_default_root_password` (the upstream scripts and SDK log in as
root/123456); `check_compat_switches` fails the run when a set switch is
not reported active (checked once locally with a misspelled key: exit 1,
"compat switch allow_get_updat did not take effect").

## GAP_PLAN status line

9.3 (SPARQL Protocol dataset): the protocol's `default-graph-uri` /
`named-graph-uri` / `using-graph-uri` / `using-named-graph-uri` are now
refused with 400 on both servers (gap-doc-claims#2); FROM / FROM NAMED in
the query text are still skipped by the parser (unchanged known gap).

## Docs notes

- docs/DEPLOY.md — compat-switch table (closing step), one row per key:
  | key | secure default | upstream value | risk when on | conformance.sh |
  |---|---|---|---|---|
  | `cors_allow_origin` | empty (no ACAO) | `*` | any web page reads answers and sends writes through a visitor's browser | no |
  | `allowed_hosts` | empty (IP literals, localhost, dotless names, listen_addr) | `*` (no check) | DNS rebinding reaches a loopback server | no |
  | `allow_get_update` | off | on | `<img src=…/sparql?query=DROP ALL>` / a text/plain form wipes data | no |
  | `trust_proxy_headers` | off | on | a same-host reverse proxy hands loopback privileges (no-auth single-db, default password, trust_inner_localhost) to the internet | no |
  | `allow_default_root_password` | off | on (upstream accepts 123456 anywhere; here loopback + local clients only) | anyone who can reach the server as a local client is root — including any web page open in a browser on that machine, since `/api` `GET` operations carry the credentials in the URL and the Origin guard covers state-changing methods only | **yes** (upstream scripts log in as root/123456) |
- docs/DEPLOY.md — a "Browser-facing defaults" paragraph: what each guard
  stops (CSRF via GET/text-plain/foreign Origin, DNS rebinding, CORS
  reads), that clients without `Origin` (scripts, SDKs) are unaffected, and
  the reverse-proxy advice: a same-host proxy is not a local client *when
  it adds `Forwarded`, `X-Forwarded-For` or `X-Real-IP`* — the server
  cannot detect a proxy that adds none, so configure it to (nginx:
  `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`, not sent
  by its default `proxy_pass`; HAProxy: `option forwardfor`), and never put
  the unauthenticated single-database mode or
  `allow_default_root_password=on` behind a proxy that does not;
  `allowed_hosts` for proxies that pass the client's Host; behind a proxy
  that rewrites `Host` (nginx's default), the public origin of every
  browser page — the bundled web console included — goes into
  `cors_allow_origin` (or keep the client's Host with `proxy_set_header
  Host $host;` and list it in `allowed_hosts`); the single-database mode
  refuses proxied requests unless `trust_proxy_headers=on` (prefer the API
  mode behind a proxy).
- docs/DEPLOY.md — first start / upgrade: packages generate root's
  password on a fresh install (read it in `/etc/gstore/root_password` as
  root, change it after the first start, then delete the file; the unit's
  `GSTORE_ROOT_PASSWORD_FILE` names it, and a missing file is skipped);
  `conf.ini` stays as shipped, so later package upgrades raise no conffile
  prompt; the upgrade note above for servers whose root still has 123456;
  the Docker image already requires a password (unchanged; a
  `GSTORE_ROOT_PASSWORD_FILE` that names a missing file is now skipped with
  a warning instead of stopping gserver).
- README.md — the quick start must set root's password (gserver refuses
  123456): `gserver --api DIR --password-file FILE`, or `root_password` in
  conf.ini for a new store; the HTTP section: POST `application/sparql-query`
  / `application/sparql-update` bodies are taken verbatim, form bodies
  supply parameters, dataset parameters are refused, GET never updates.
- README.md / CHANGELOG.md — REST session tokens: header or URL only
  (no longer a `session=` body parameter).
- docs/PFN.md (r4-ext-docs owns it) — its `curl … /api` examples log in
  as root/123456, which a gserver now serves only with
  `allow_default_root_password=on`: use a placeholder password there.
- CHANGELOG.md — the Changelog bullets above, behaviour changes first, with
  the upgrade note.
- docs/REFACTOR_BACKLOG.md — (D3) the single-database server, REST and
  `/api` still parse parameters in two places: `server::Request` (URL +
  form body, Content-Type aware) and `gapi::params::Params`, which keeps
  upstream's guessing (any body containing `=` is a form, any body starting
  with `{` is JSON, whatever the Content-Type) for wire compatibility with
  the upstream SDKs; unify only if the upstream clients' content types
  allow it (review design#1, http#5). REST does not read multipart form
  fields (it never did).
- docs/REFACTOR_BACKLOG.md — (D8, sec#9 residue) `gapi` IP allow/deny
  lists and the lockout still key on the TCP peer (a same-host proxy's
  127.0.0.1); trusting `X-Forwarded-For` contents for them is not done
  (it needs a trusted-proxy list) — r2-limits-auth owns the lockout.
- docs/REFACTOR_BACKLOG.md — (D1 residue, infra#3) the upstream `/api`
  protocol keeps its state-changing `GET` operations with credentials in
  the URL (`GET /api?operation=drop&username=…&password=…`): the Origin
  guard covers state-changing methods only, so a web page that knows a
  password can still drive `/api` through a visitor's browser with an
  `<img>`. Not changed for wire compatibility; it matters with
  `allow_default_root_password=on` (the password is known). A later
  package could refuse state-changing `/api` operations over `GET` behind
  a compat switch.

## Measured numbers

- Bench smoke gate, branch vs the committed baseline:
  `target/release/gstore-bench ci --baseline bench/baselines/smoke.json`
  (release build of this branch, `GSTORE_BENCH_CACHE` freshly generated,
  run through the campaign's heavy-job limiter while up to three other
  agents were building — load average ~10-14, so timings are noisy):
  **PASS**, 148 checks passed, 0 failed. LUBM snapshot geomean 0.94× of
  base (gate `time.geomean` 0.92, limit 1.50), disk 0.99×; WatDiv-like
  snapshot 1.02×, disk 1.07×. The HTTP concurrency rows are report-only:
  `sparql` / `api` endpoint qps 0.79×-1.53× of base, every mixed-workload
  writer `11 / 0` (writes / errors; the bench POSTs
  `application/sparql-update`), all bodies identical.
- The changed per-request work: `ConnMeta::new` looks up `Origin`, `Host`
  and three forwarding headers in the parsed header list (no allocation but
  an echoed allowed origin; `Request::header` no longer allocates a
  lowercased name); a `GET /sparql` statement is parsed a second time only
  when its text contains an update keyword or a backslash.

## Requests/Not yet

- Edits outside the owned files and named regions (for the integrator):
  - `src/server.rs` struct `HttpOptions` + `Default`: besides
    `cors_allow_origin`, three more fields in the same block directly after
    `cors` (`allowed_hosts`, `allow_get_update`, `trust_proxy_headers`) —
    the embedded `Server` has no other place for its policy; `cors` keeps
    its type and the `if opts.cors && req.method == "OPTIONS"` line is
    unchanged. `src/config.rs` `http_options()`: the matching four lines
    plus the private helper `allowed_hosts_with_listen_addr()` after
    `upstream_compat_warnings()`; a private `split_words()` before
    `split_list()`.
  - `src/server.rs`: `ConnMeta::new` became `pub(crate)` with the
    signature `(keep_alive, idle, remaining, opts: &HttpOptions, req:
    Option<&Request>)` — the two calls in `serve_connection` changed
    (sanctioned). `Request::header` compares names without allocating.
    New items next to the owned code: `BodyKind`, `QUERY_DATASET_PARAMS`,
    `UPDATE_DATASET_PARAMS`, `dataset_param_refusal`, `looks_like_form`,
    `form_lookup`, `decoded_eq`, `decode_at`, `url_decode_bytes` (replaces
    `url_decode`), the policy helpers after `ConnMeta` (`origin_listed`,
    `admission_refusal`, `host_allowed`, message constants) and
    `ip_is_loopback` after `peer_is_trusted_local`. `body_string()` became
    `update_text(any_body)`; new accessors `body_kind`, `raw_statement`,
    `is_forwarded`, `first_param_of`.
  - `src/http_api.rs` `ApiInner::handle` (r1-download's function): one
    call-site line, `self.respond(&req, peer)` → `self.respond(&req, peer,
    meta)` (respond needs the policy verdict). Private helpers
    `DEFAULT_PASSWORD_REFUSED` and `presents_default_root_password` after
    `pick_db_name`. `respond` itself: the refusal / default-password /
    GET-update / 415 checks (the ipfilter lines untouched), and `/login`
    served before the locked section (it calls `handle_login`, unchanged,
    with no lock held; `dispatch`'s `/login` arm is left as it was).
  - `src/gapi/auth.rs` `verify()`: besides the `trust_inner_localhost`
    condition, a refusal of root's default password from non-local clients
    (a few lines before the user-store check) and a private
    `is_default_password()` helper — the single `/api` credential check is
    where that loopback privilege lives.
  - `src/gapi/tasks.rs` `run_async`: the `Ctx` literal's new field plus
    the one `let local_trusted = ctx.local_trusted;` line feeding it.
  - `conf/conf.ini.example`: besides the `[server]` cors block, two comment
    lines above `[system] root_password` pointing at the refusal.
  - `src/config.rs` test `every_key_is_read_from_its_section`: the five new
    keys added to its text (needed by convention 3), with assertions.
  - `tests/dt_gconsole.rs` (sanctioned, 13b): the `gserver` instances get
    an explicit root password (`GSERVER_PASSWORD`) in their conf.ini and
    gconsole logs in with it — review sec#8 / infra#3.
  - `tests/dt_gserver_lifecycle.rs` (13b, setup lines only, comment
    "review sec#8 / infra#3"): `kill_and_restart_commands` and
    `a_stale_pid_file_does_not_block_a_start` start with `--password pw`;
    `the_deamon_supervisor_restarts_a_killed_server` and
    `stop_during_the_supervisors_backoff_stops_the_service` get
    `root_password=pw` in their conf.ini (they started a fresh API store
    with the default password on loopback).
  - `src/server.rs` test `options_preflight_and_cors_headers` (13b): asserts
    the new default (foreign preflight 403, no ACAO) and the `*` compat
    case (the old assertions, unchanged, under `cors_allow_origin=*`).
- `scripts/conformance.sh`: the compat helper is verbatim; the grep paths
  were not adapted — the `-b` parent prints the configuration warnings
  (to `$LOGS/gserver-start.log`), and the background server logs them to
  `dbhome/gserver.log` too.
- Not done here: the IP lists and lockout behind a proxy (see Docs notes,
  r2-limits-auth); FROM / FROM NAMED (known gap 9.3); REST multipart form
  fields.
