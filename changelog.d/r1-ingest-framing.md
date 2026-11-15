# r1-ingest-framing — Total body and TLS-handshake deadlines, per-IP connection cap, strict HTTP/1.1 framing, header control characters

## Changelog

Behaviour changes (both HTTP servers, the single-database `Server` and the
`/api` + REST `ApiServer`, which share the connection layer):

- **A slow client can no longer hold a worker thread** (review H-1: sec#3,
  http#1, robust#3, http#3). A request body must now arrive at
  `min_body_rate` on average after a grace of `read_timeout` from the end
  of its head: `t` seconds in, at least `(t - read_timeout) *
  min_body_rate` bytes of it must have been received (chunked and
  `Content-Length` bodies alike). The allowance is earned by the bytes
  actually received, never by the declared `Content-Length`, so a client
  announcing a 1 GiB body and dripping it is cut off after about
  `read_timeout`, and no body can hold a worker longer than `read_timeout
  + length / min_body_rate`, however it trickles in;
  before, only no single read could wait longer than `read_timeout` (30 s),
  so one byte every 29 s held a worker indefinitely, before authentication,
  and `thread_num + 1` such connections disabled the server. Under TLS the
  handshake and every TLS record of the request head now count against the
  head deadline (`read_timeout`), as do the records of the body against the
  body deadline: the deadline is enforced below `rustls`, at every socket
  read, where before it was checked once above it (rustls drove a whole
  handshake or waited for a whole record inside that one read). The idle
  wait between two requests on a kept-alive TLS connection is bounded the
  same way. A request that misses its deadline is answered `400 bad
  request: the request did not arrive in time` (if the connection can still
  take it) and the connection is closed.
- **New `[server] min_body_rate`** (`HttpOptions::min_body_rate`, bytes
  per second, default `65536` = 64 KiB/s; `0` restores the per-read timeout
  alone). A legitimate upload slower than 64 KiB/s on average (after the
  30 s grace) is now cut off: lower the key for such clients.
- **New `[server] max_conns_per_ip`** (`HttpOptions::max_conns_per_ip`,
  default `64`, `0` = no limit): a further connection from an address that
  already has that many open — waiting for a worker included — is closed
  as soon as it is accepted (logged at most once a second: `refused a
  connection from <ip>: it already has <n> open ([server]
  max_conns_per_ip)`). IPv4-mapped IPv6 peers count as their IPv4
  address. Clients behind one NAT, and every client of a reverse proxy,
  share an address: raise the key (or set `0`) there.
- **Strict HTTP/1.1 request framing** (sec#16, http#14; RFC 9112 §5, §6,
  §7.1). Answered `400` and the connection closed, where before they were
  accepted (and could desynchronise a front end that frames differently —
  request smuggling): `Content-Length` together with `Transfer-Encoding`
  (before: accepted, and the connection kept open); differing
  `Content-Length` values (before: the last won) or values that are not
  plain decimal digits (`+5`, `-5`, `0x5`, empty, a list such as `5, 5`);
  a `Transfer-Encoding` other than exactly `chunked` — `gzip`,
  `identity`, `xchunked`, `gzip, chunked`, `chunked, chunked`, a
  `chunked` line followed by an `identity` line (before: anything but
  `identity` was decoded as chunked, and a later line overrode an earlier
  one) — and any `Transfer-Encoding` in an HTTP/1.0 request; chunk sizes
  that are not bare hex digits (`+5`, `-5`, `0x5`, a leading space);
  header lines without a colon (before: silently skipped), with an empty or
  non-token name, whitespace before the colon (`Content-Length : 5`), or an
  obs-fold continuation line. Identical duplicate `Content-Length` lines,
  `Transfer-Encoding: Chunked` (any case), chunk extensions and trailers
  are still accepted. The messages name the cause (`bad request: ambiguous
  or unsupported message framing (Content-Length / Transfer-Encoding)`,
  `bad request: malformed header field`).
- **Control characters in request heads are refused** (robust#14): a
  request line, header line, chunk-size line or trailer that holds a
  control character other than HTAB — a bare CR, NUL, ESC, DEL, … — is
  answered `400 bad request: control character in a request line, header
  or chunk line`. Before, a bare CR inside a header value was kept, and a
  CORS preflight reflected it into `Access-Control-Allow-Headers` (a bare
  CR in a response header). A line ending in CR CR LF is refused too.
- Tests: `tests/http_framing_deadlines.rs` — one-worker servers (REST and
  `/api`) against a body-dripping client (`Content-Length` and chunked), a
  trickling TLS handshake, TLS request head and TLS body (`--features
  tls`): the server closes that connection and serves the request queued
  behind it on its only worker, also when the dripped body declares a
  64 MiB `Content-Length`; bodies within their allowance (256 KiB at once,
  5 bytes over 200 ms, and 3000 bytes kept up at twice the rate over 1.5
  times the read timeout) are served; the per-address cap refuses a
  third connection from 127.0.0.1 while 127.0.0.2 is served (Linux: the
  test binds a second loopback source address) and re-admits it once one
  connection ended; a framing rejection table (one `400`, then closed, a
  smuggled follow-up request never served) and its well-formed neighbours;
  the control-character table (no bare CR in any response). All of them
  fail on the previous code except the three positive controls (bodies
  within their allowance, a body keeping up the rate, well-formed framing),
  which pass on both; no existing test needed a change.

## GAP_PLAN status line

none

## Docs notes

- **docs/DEPLOY.md** (hardening / reverse-proxy sections): document the two
  new `[server]` keys. `max_conns_per_ip` (default 64) counts TCP peers: a
  reverse proxy (nginx, Caddy, stunnel) in front of gserver is ONE address,
  so set `max_conns_per_ip` at least to the proxy's upstream connection
  count (or `0`) there, as for a NAT; IPv6 clients are keyed by their full
  address (a client that rotates addresses within its prefix is not held to
  one count — pair with a firewall limit if that matters). `min_body_rate`
  (default 64 KiB/s after a 30 s grace) cuts off slower uploads: lower it
  (or `0`) for clients on very slow links. A client that drips its body is
  cut off after about 30 s whatever `Content-Length` it declares; holding a
  worker longer takes really sending at least `min_body_rate` (one worker
  per 64 KiB/s of the client's bandwidth), and no request holds one longer
  than `30 s + max_body / min_body_rate` — keep `max_conns_per_ip` below
  `thread_num` where untrusted clients with fast links connect. Neither is
  an upstream-compat switch (upstream has no such limit; no conformance
  switch needed).
- **docs/DEPLOY.md / README.md** (HTTP behaviour): request framing is now
  strict RFC 9112 (see the Changelog list); clients sending
  `Transfer-Encoding: identity`, a signed `Content-Length`, header lines
  without a colon or with whitespace before it, or control characters in
  header values now get `400`. No known upstream client (curl, Python
  `requests`, the upstream SDKs and scripts) sends any of them;
  `scripts/conformance.sh` is green.
- **README.md** (configuration table of `[server]`): add `min_body_rate`
  and `max_conns_per_ip` with their defaults.
- **docs/REFACTOR_BACKLOG.md**:
  - (sec#16, http#14) An unsupported transfer coding (`gzip, chunked`) is
    answered `400`, not RFC 9112's suggested `501`, and a request that
    misses its deadline `400`, not `408`: `write_response_ext` (r1-download
    in this round) has no reason phrase for 501/408 and would print `501
    OK`. Add both reason phrases there and switch `BadRequest` to them.
  - (http#3) The deadline below rustls is enforced by `ConnSocket`, the
    transport every accepted connection is served on, which reads the
    deadline the request reader publishes in a worker-thread-local
    `ReadClock` (as `current_peer` does for the peer address), because the
    readers are generic over any `BufRead` (r1-ingest-caps wraps the
    reader in its budget) and their signatures are kept (convention 13a).
    The plan suggested driving `complete_io` manually for the handshake;
    that would have needed the TLS stream type in `serve_connection` (or a
    new trait bound on it and on `read_request`). A later refactor that
    threads a connection context through `read_request` could carry the
    deadline explicitly.
  - (sec#3) `max_conns_per_ip` keys IPv6 peers by full address; keying by
    /64 is an option if IPv6 floods matter.

## Measured numbers

Machine state: the shared 4-vCPU / 15 GB box, with three other agents'
builds and test runs going on (load average 11-13 throughout): every
number below is noisy, and none was measured quietly.

- **Smoke gate** (convention 8; release builds of base 7bbf4ae and of this
  branch in separate target directories, data from the campaign's bench
  cache): `gstore-bench ci --baseline bench/baselines/smoke.json
  --data-dir <bench-cache>` — **PASS on both** (base and branch, same
  session, 148 gate rows each). A direct `gstore-bench compare base.json
  branch.json` flags `time.geomean` snapshot (1.72 > 1.50) and `time.open`
  disk (2.68 > 2.50): query evaluation and database open, which this
  package does not touch; the branch run overlapped this package's own
  stream-mode workspace test run, the base run did not.
- **HTTP read path, A/B** (the path this package changes): the smoke
  profile's concurrency phase alone (`gstore-bench run --profile smoke
  --extra none --backends snapshot --reps 1 --no-update --no-mixed
  --conc-clients 1,8 --conc-queries q1,q4 --conc-secs 3 --conc-endpoints
  sparql,api --no-calibrate`), base and branch alternated three times each;
  median qps (keep-alive GET clients):

  | endpoint/query/clients | base | branch | ratio |
  |---|---:|---:|---:|
  | sparql/q1/c1 | 13064 | 16751 | 1.28 |
  | sparql/q1/c8 | 38639 | 47485 | 1.23 |
  | sparql/q4/c1 | 9239 | 8390 | 0.91 |
  | sparql/q4/c8 | 15147 | 13850 | 0.91 |
  | api/q1/c1 | 6527 | 5997 | 0.92 |
  | api/q1/c8 | 14037 | 12494 | 0.89 |
  | api/q4/c1 | 3219 | 3237 | 1.01 |
  | api/q4/c8 | 5448 | 6632 | 1.22 |

  The run-to-run spread within one build is up to ±30 %, so no change is
  measurable; by construction the head phase makes the same number of
  `setsockopt` calls as before (one per socket read, now in `ConnSocket`
  instead of `Timed`), the idle wait between kept-alive requests one more,
  and a body one per socket read while `min_body_rate > 0`. A quiet re-run
  is left to the round's land step.
- No complexity change (D9 scaling test not applicable).

## Requests/Not yet

- **Outside the package's regions** (each a minimal edit, for the
  integrator):
  - `src/server.rs` `struct Accepted`: the `stream` field's type is now
    `ConnSocket` (it dereferences to `TcpStream`, so `acc.stream.try_clone()`
    and `rustls::StreamOwned::new(conn, acc.stream)` in the four
    `serve_forever[_tls]` closures of `Server` and `ApiServer` compile
    unchanged; r1-ingest-caps edits those closures — keep both sides).
  - `src/config.rs` `tests::every_key_is_read_from_its_section` (one new
    line `min_body_rate=0\nmax_conns_per_ip=5\n\` after the first `[server]`
    line of the text, one new assertion after the `listen_addr` one) and
    the `http_options()` assertions at its end (one new line after
    `keep_alive_max`): every package adding keys extends this test; the
    lines were placed away from the other packages' insertions.
  - New code (no other package's function): the `ConnSocket` / `ReadClock`
    / `IpLimiter` block between `wait_for_request` and `wants_keep_alive`,
    and `split_field` / `is_tchar` / `content_length_value` / `chunk_size`
    next to `read_head`; constants `DEFAULT_MIN_BODY_RATE` /
    `DEFAULT_MAX_CONNS_PER_IP` after `DEFAULT_THREADS`.
- **For the closing step / integrator**: `read_request`'s doc comment
  (r1-ingest-caps' function) still says the body "only must not stall for
  longer than [read_timeout] between reads"; it could add "and must keep up
  `min_body_rate` overall (see `read_body`)". The `[server]` key list in
  `src/config.rs`'s module doc (r1-http-surface rewrote those lines) does
  not name `min_body_rate` / `max_conns_per_ip` yet.
- No signature changed: `read_line_bounded`, `read_head`, `read_body`,
  `read_chunked_body`, `read_request`, `serve_connection` and
  `wait_for_request` keep theirs; no call site outside the package changed.
- Not done: authentication from the request head before the body is read
  (H-1's other suggestion) is r1-ingest-caps' pre-body hook.
- Review note: the plan's approach gave a `Content-Length` body the
  allowance `read_timeout + len / min_rate` from its declared length; the
  package review changed it to the bytes actually received (capped at the
  declared length), as for chunked bodies. With the declared length a
  client announcing `max_body` and sending one byte every 29 s still held a
  worker for `30 s + max_body / 64 KiB/s` — over 4 hours with gserver's
  default 1 GiB `upload_max_body_size`, about 17 minutes with
  r1-ingest-caps' 64 MiB pre-auth cap — which is robust#3's own scenario.
  The acceptance bound (`read_timeout + len / min_rate` at most) still
  holds; tests `a_dripped_body_gains_nothing_by_declaring_a_large_length`
  (fails with the declared-length allowance) and
  `a_body_that_keeps_up_the_rate_may_outlast_the_read_timeout` (positive
  control).
