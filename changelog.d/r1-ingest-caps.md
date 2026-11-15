# r1-ingest-caps — Pre-auth body caps by route class, global body budget, JSON/form parse budgets, pre-body authentication hook, linear multipart

## Changelog

Behaviour changes first:

- **Before authentication, a request body is capped at 64 MiB**
  (`[upload] preauth_max_body_size`, `HttpOptions::preauth_max_body`; review
  C-2: sec#1, gapi#1, infra#1). Before, every request could carry the whole
  upload cap (1 GiB by default), read into memory before any handler, IP list
  or credential check ran: 30 workers could be made to buffer 30 GiB, and a
  single unauthenticated `/api` JSON body was parsed into ~33 times its size.
  Now both servers decide a body's cap between its head and its body:
  - every request, on every route, may carry `preauth_max_body_size`;
  - only a request **authenticated from its head** on a route that carries
    payloads may carry `upload_max_body_size`. API mode checks HTTP Basic, a
    REST session token (`Authorization: Bearer`, `X-Session-Token`,
    `session=` / `token=` in the URL) or upstream's `username` / `password`
    (`encryption=1`) in the URL query string, under the IP allow / deny lists
    and the failed-login lockout. Payload routes: `/file/upload` (also under
    `/grpc/`), `/api` (and `/grpc/api`, `/ghttp/api`, `POST /`) when the URL
    names no operation or `query` / `tquery`, and REST `/upload`, `/build`,
    `/update`, `/tquery`, `/batchInsert`, `/batchRemove`. The
    single-database server compares its Basic credentials on `POST /update`
    and `POST /sparql`; without credentials configured it serves everyone
    there as before — except a request it refuses after the body anyway: one
    a loopback reverse proxy forwards (`Forwarded` / `X-Forwarded-For` /
    `X-Real-IP`, unless `trust_proxy_headers=on`) is held to the
    pre-authentication cap (round-1 review fix; the API hook already
    applied the loopback rule since the merge fix 12df188);
  - in both modes a request the browser policy refuses before routing — a
    foreign `Host` on a loopback connection, a foreign `Origin` on a
    state-changing request (`allowed_hosts`, `cors_allow_origin`) — is held
    to the pre-authentication cap whatever credentials its head carries:
    it is answered `403` after its body (round-1 review fix; test
    `tests/preauth_refused_requests_get_no_upload_cap.rs`);
  - a larger `Content-Length` is answered `413` without reading a body byte;
    a chunked body is cut off at its cap (`413`); `Expect: 100-continue` is
    answered only after this decision (never `100 Continue` for a body that
    will be refused);
  - credentials in the head that fail the check are answered `401` before the
    body is read, and count once towards the address's lockout (a locked-out
    or blocked address is not checked: its body is held to the
    pre-authentication cap, also when it carries a valid session token);
  - an upstream *inner* request (`inner=true`) is never checked before its
    body (whether its password is checked at all is `verify`'s
    `trust_inner_localhost` decision, and a failure counted here would lock
    out 127.0.0.1): its body is held to the pre-authentication cap;
  - the check runs only for a body that may exceed the pre-authentication
    cap (a larger or a chunked one): ordinary requests pay nothing extra.
  Upstream clients put their credentials inside the body (the SDKs' `/api`
  JSON, the Workbench's `/file/upload` form), so **their uploads are now
  limited to 64 MiB** unless they send the credentials in the URL query
  string (or an operator raises `preauth_max_body_size`).
- **All request bodies in memory share a budget** (`[upload]
  request_body_budget`, `HttpOptions::body_budget`; default 512 MiB, never
  below `upload_max_body_size`), per server. Bodies are charged as their bytes
  arrive (a `Content-Length` reserves nothing, so a slow client cannot hold
  budget it does not send) and held until the response is written. A request
  whose announced body does not fit in what is left is answered `503` before
  its body is read; one that runs out part-way is answered `503` too. The
  connection is closed after either.
- **The default and shipped `upload_max_body_size` is 256 MiB** (was 1 GiB,
  upstream's value; review infra#1). The `/api` upload check
  (`1005 "Upload file more than max_body_size!"`) follows it.
- **`/api` request bodies are parsed within bounds** (gapi#1): a JSON body
  holds at most 65,536 values (every scalar, array and object; a wider one is
  refused with `1003 "the request body is not valid JSON (… too many JSON
  values …)"` before more of its tree is built); a URL query string, a form
  body and a multipart body hold at most 65,536 parameters / parts (`1003`).
- **Multipart boundaries follow RFC 2046** (review H-2: sec#2, robust#2): 1 to
  70 characters of `bchars` (letters, digits, `'()+_,-./:=?` and an inner
  space). A longer boundary or another character is answered `1003`. The
  boundary search is linear (`memchr::memmem`'s two-way search) instead of
  `body × boundary` (a 60,000-character boundary made one 8 MiB request cost
  11 s of CPU before authentication).
- `Params::to_redacted_json` builds its object in linear time (gapi#19; it
  searched the object it was building once per parameter).

New API and files:

- `src/preauth.rs` (module `gstore::preauth`): the pre-body decision —
  `Ingest` (caps, budget, hook; carried by the connection loop's `ConnEnv`),
  the two servers' hooks, the budget and its byte-charging reader, the
  constants `PREAUTH_MAX_BODY` (64 MiB) and `DEFAULT_BODY_BUDGET` (512 MiB);
  hidden test hooks `body_budget_in_use(addr)` and
  `wait_body_budget(addr, timeout, cond)`.
- `HttpOptions::preauth_max_body` and `HttpOptions::body_budget`;
  `GStoreConfig::preauth_max_body_size` and `request_body_budget` (`[upload]`).
- `json::parse_with_limits` and `json::Limits` (`json::parse` is unchanged).
- `gapi::params::MAX_JSON_VALUES`, `MAX_PARAMS`, `MAX_BOUNDARY_LEN`.
- `memchr` is now a direct dependency (it was already built through `regex`).
- Tests: `tests/http_preauth_limits.rs` (pre-body 413/401/100-continue on
  both servers and under TLS, the lockout, no upload cap for a locked-out
  address's session token, no pre-check of inner requests, the budget's 503,
  the bounded
  peak of a chunked body, wide JSON, long boundaries, a 64-character boundary
  over megabytes), unit tests in `src/preauth.rs`, `src/gapi/params.rs` and
  `src/json.rs`.

## GAP_PLAN status line

none

## Docs notes

- **docs/DEPLOY.md** (the secure-defaults table; none of these is a compat
  switch, all are limit keys — conformance.sh enables nothing for them):
  - `preauth_max_body_size` — secure default 64 MiB; upstream: the whole
    upload cap (1 GiB) before authentication. Risk of raising it: every
    unauthenticated client may make the server buffer that much per request
    (bounded in total by `request_body_budget`). Upstream behaviour: set it
    equal to `upload_max_body_size`. Clients keep the full upload cap
    without raising it when their head carries credentials the route reads
    there: upstream routes (`/file/upload`, `/api`) only `username` /
    `password` in the URL query string (`/file/upload?username=…&password=…`;
    an `Authorization` header does not count), the REST payload routes only
    HTTP Basic or a session token (`Authorization: Bearer`,
    `X-Session-Token`, `session=` / `token=` in the URL; URL `username=` /
    `password=` do not count).
  - `upload_max_body_size` — default and shipped value lowered from 1 GiB to
    256 MiB; raise it for larger uploads (and `request_body_budget` with it),
    or use the server-side `build` / `batchInsert` paths or `gbuild` / `gadd`.
  - `request_body_budget` — 512 MiB of request bodies in memory per server;
    a request beyond it gets `503` (retry later). Size it to the memory the
    server may spend on bodies; it is raised to `upload_max_body_size` when
    set lower.
  - Behaviour change to announce: a `401` (not the `/api` envelope's `1001`)
    for a wrong password sent in the URL query string with a body larger than
    the pre-authentication cap; `503` when the body budget is exhausted.
    An upstream *inner* request (`inner=true`, honoured only with
    `trust_inner_localhost`) is never authenticated from its head, so its
    body is held to `preauth_max_body_size` (raise that key for larger
    inner uploads).
- **README.md** (configuration section, the `[upload]` keys and the
  `refreshconf` paragraph): list `preauth_max_body_size` and
  `request_body_budget` with their defaults; the sentence "the cap
  `upload_max_body_size` puts on every HTTP request body need a restart"
  becomes "the HTTP body caps (`upload_max_body_size`,
  `preauth_max_body_size`) and `request_body_budget` need a restart"; note
  that a large upload must carry its credentials in its head — in the URL
  query string for `/file/upload` and `/api` (an `Authorization` header does
  not count there), as HTTP Basic or a session token for the REST payload
  routes — the SDKs' JSON / form credentials only reach 64 MiB.
- **docs/REFACTOR_BACKLOG.md**:
  - C-2 (sec#1, gapi#1) remainder — stream a multipart upload to a temporary
    file under `upload_path` once its request is authenticated, instead of
    keeping `Request.body: Vec<u8>` and copying the file part out of it
    (`UploadedFile.data`): today an upload costs about twice its size in
    memory (the body is charged to the budget, the parsed copy is not; both
    are bounded, the copy by the pre-authentication / upload cap). Needs a
    streaming body type in `server.rs` and an owned-body path through
    `gapi::Params` (D3: not done this round).
  - sec#1 — the pre-body hook verifies head credentials (one slow PBKDF2
    hash) and the handler verifies them again: an authenticated upload past
    the pre-authentication cap is hashed twice. Passing the verified identity
    from the hook to the handler needs a field on `Request` (r1-http-surface's
    struct this round).
  - Budget accounting counts the body bytes received, not the `Vec`'s
    capacity (`read_body` pre-allocates up to 1 MiB of an announced body, and
    a growing `Vec` may hold up to twice its length): the budget bounds
    memory within about a factor of two.
  - gapi#19 (info) remainder, outside this package's files: `SysDb::sync_with_disk`
    has no runtime caller although the sysdb module docs say the registry is
    reconciled on demand (src/sysdb.rs); `TxnState::Aborted` is never
    constructed (src/gapi/apilog.rs); the test-only `__asyncTest` operation is
    registered in production builds behind a process-wide flag
    (src/gapi/ops_task.rs, src/gapi/tasks.rs). `Params::to_redacted_json` is a
    public library helper and stays (D2), now linear.

## Measured numbers

Machine: the campaign's shared 4-vCPU / 15 GB box, other agents building
and testing at the same time (load average 9-10 during the bench runs).

- **The review's repros against this branch** (debug `gserver --api`,
  built-in defaults via `-c /dev/null`, no credentials sent; scratch script,
  not committed):
  - sec#1 / gapi#1: a 26,214,407-byte `{"a":[0,0,…]}` POSTed to `/api` is
    answered `1003 "… too many JSON values (more than 65536)"` in 0.03 s;
    VmHWM 8,596 kB -> 39,812 kB (the body itself). The review measured the
    base at 15,380 kB -> 867,044 kB (`1100` after 2.8 s).
  - infra#1: a 100 MiB unauthenticated `Content-Length` is answered `413`
    from the head alone (0.00 s); VmHWM unchanged.
  - sec#2: 8 MiB of `-` with a 60,001-character boundary is answered `1003
    "the multipart boundary is longer than 70 characters (RFC 2046)"` in
    0.01 s (the review: 11.5 s on the base).
- **Failing before the fix** (the new tests run against 7bbf4ae's sources):
  the 7 tests of `tests/http_preauth_limits.rs` that compile against the
  base (only `max_body` set) fail there — the server waits for bodies it
  should refuse (`400` after its read timeout instead of `413` / `401`),
  sends `100 Continue` to unauthenticated uploads, answers the wide JSON
  body with `0` and the long boundary with "multipart body without its
  boundary"; the other 6 use the new `HttpOptions` fields and the budget
  hooks (no base equivalent). In `src/gapi/params.rs` 6 of the new unit
  tests fail on the base parser, among them the scaling tests: a
  20,000-byte needle over 1 MiB of `-` took 0.317 s with the
  window-by-window `find` (64-byte needle: 0.003 s) against 0.003 s on the
  branch; `to_redacted_json` of 32,000 parameters took 1.19 s on the base
  (4,000: 0.016 s), 16,000 took 0.017 s on the branch.
- **Independent review** (spot-checks on the branch, each fix reverted in
  place and the tests rerun): without the pre-body cap decision 9 of the 14
  integration tests fail; without the budget
  `the_body_budget_bounds_all_bodies_in_memory` fails; without the JSON value
  bound, the RFC 2046 check and the linear search, the two wide-JSON tests,
  the long-boundary integration test, `multipart_boundaries_follow_rfc_2046`
  and `the_boundary_search_is_linear_in_the_boundary_and_the_body` fail.
  `a_64_character_boundary_over_megabytes_parses_in_linear_time` passes on the
  base as well: a fixed-length boundary was linear in the body there too, so
  it is a guard; the scaling unit test is the one that fails before the fix.
  The reviewer's two additional tests (locked-out session token, inner
  request) fail without commit "preauth: no upload cap for a locked-out
  address's token, no pre-check of inner requests".
- **Bench smoke gate** (D9; the HTTP read path changed): release builds of
  `gbuild` + `gstore-bench`, base = 7bbf4ae (binaries built from its
  sources), branch = 4787f90, run back to back with the same data cache:
  `GSTORE_BENCH_CACHE=<cache> gstore-bench ci --baseline
  bench/baselines/smoke.json --bin-dir <dir> --work-dir <dir> --out <json>`.
  Both **PASS** (456 checks, 0 failed). Query geomean vs the committed
  baseline, base / branch: LUBM-like snapshot 0.90x / 0.87x, disk
  1.00x / 0.93x; WatDiv-like snapshot 1.20x / 0.96x, disk 0.99x / 1.00x.
  HTTP concurrency (report-only, qps, base / branch; LUBM-like, snapshot):
  sparql q1 c1 16,345 / 13,262, c2 28,652 / 31,228; api q1 c1 7,266 /
  10,609, c2 9,542 / 11,862; disk sparql q1 c2 24,202 / 26,136, api q1 c2
  12,213 / 14,920 — within the run-to-run noise in both directions, every
  body identical, every mixed-workload writer 10-12 writes / 0 errors.
  (The result files name the branch commit for both runs: the bench reads
  the commit of the directory it runs in.)
- `scripts/conformance.sh --no-build` (this branch's debug binaries, the
  shipped `conf.ini.example` with the new keys): all six suites pass
  (basic, update, transaction, api, parser with the two known failing cases
  15 and 17, python 49/49). No compat switch or limit change was needed.

## Requests/Not yet

- **Edits outside the owned files** (all small):
  - `Cargo.toml` + `Cargo.lock`: `memchr = "2"` as a direct dependency
    (already in the lock file through `regex`; no new crate is built).
  - `src/server.rs` `run_accept_loop` (r1-ingest-framing's function this
    round): one line in its `ConnEnv { .. }` literal,
    `ingest: Arc::new(crate::preauth::Ingest::new(opts, None))` — expect a
    trivial conflict with r1-ingest-framing's own `ConnEnv` field lines.
  - `src/server.rs` `strip_basic_prefix` and `base64_decode`: `fn` →
    `pub(crate) fn` (the hook decodes Basic credentials from the head).
  - `src/server.rs` `read_request` (granted): its signature now takes the
    `Ingest` instead of `max_body` and returns the body's `BodyHold`; it
    wraps the reader in `preauth::Budgeted` for the `read_body` /
    `read_chunked_body` calls (their signatures are unchanged). If
    r1-ingest-framing sets a body deadline in `read_request`, the integrator
    keeps both (the deadline goes on `reader.get_mut()` before the
    `Budgeted` wrapper is built).
  - `src/config.rs` tests (convention 13b, with `review infra#1` notes):
    `defaults_mirror_upstream_util_configure` (the lowered default and the
    two new keys), `the_shipped_example_matches_upstream_and_parses_cleanly`
    (the deliberate `upload_max_body_size` difference),
    `every_key_is_read_from_its_section` (sets the two new keys).
- **For r2-limits-auth**: the pre-body credential check is
  `gapi::auth::check_before_body` (called from `preauth::api_admission`); it
  already honours the IP lists and the lockout and counts a failure once.
- **gapi dispatch / route unchanged**: the body is still parsed before
  authentication (upstream's body-wins rule for duplicated parameters needs
  the body first); what that parse may allocate is now bounded by the
  pre-authentication cap, the body budget and the parameter bounds.
- **gapi#19 partial**: the params.rs part is fixed; the other three dead paths
  are outside this package's files (Docs notes, REFACTOR_BACKLOG).
- Streaming multipart uploads to a temporary file was optional this round and
  is not done (REFACTOR_BACKLOG note above).
