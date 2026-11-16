# r2-limits-auth — Query time and result limits, login lockout on every authentication path, password hashing off the global lock, async task isolation

## Changelog

Behaviour changes first:

- **Failed-login lockout per account and address, on every password path
  (review gapi#5, sec#7, http#6; D1 secure default).** Failures are now
  counted per account *and* client address: after `login_lockout_failures`
  (default 4, upstream's count) failures that account is locked for that
  address — 1 s at first, each further failure doubling the lock up to
  `login_lockout_max_seconds` (default 900) — and the counts are forgotten
  after an hour without a failure. Every path that checks a password asks the
  lockout **before** the PBKDF2 hash runs: `/api` `login`, `testConnect` and
  every authenticated operation (also `/shutdown`, `/restart`), the pre-body
  credential check of `src/preauth.rs`, REST `/login` and Basic credentials,
  and the single-database server's Basic auth. A refused attempt costs no
  hash and is answered `/api` `1101` / REST and single-database `429 Too Many
  Requests` with `Retry-After`. A success clears only its own account's count
  at that address (one account's login no longer unlocks another), and a lock
  refuses only that account's password checks — other users, other
  operations and logged-in sessions from the address keep working, so one
  client behind a proxy or NAT can no longer lock everyone out until a
  restart. Failures of every account from one address also lock the address's
  password checks once there are more than `8 × login_lockout_failures`
  (default 32) of them, with the same back-off, so guessing across many user
  names gets no free hashes either. **Upstream-compat switch**
  `upstream_login_lockout=on` restores upstream's rule (per address; `login`,
  `testConnect` and REST `/login` exempt and still evaluating guesses; any
  successful login resets the address; until restart); `gserver` then warns
  `upstream-compat: upstream_login_lockout=on` at startup.
  `scripts/conformance.sh` does not need it.
- **`query_timeout` (review sec#10, robust#5).** New `[server]` key (seconds,
  fractions allowed; default 3600, `0` = off). A statement still running then
  — REST `/sparql` and `/update`, `/api` `query` (synchronous or `async`), the
  single-database server's `/sparql` and `/update` — is stopped at its next
  cancellation check and answered as timed out: REST and single-database
  `503` with a `{"error":"query timed out after … (query_timeout) …"}` body,
  `/api` `1005 "Query fail: query timed out …"`. It releases the database it
  held, so an UPDATE queued behind a runaway read (which, with std's
  writer-preferring lock, blocked every later reader) now completes. The
  evaluator polls the cancel flag inside one join step too (left-deep BGP
  loop, binding extension, between joins, property-path frontier steps), so
  `/cancel` and shutdown also stop a cross product or a closure promptly.
- **`max_result_bytes` (review sec#10).** New `[server]` key (bytes; default
  1 GiB, `0` = off). An answer whose serialization passes it is refused as
  soon as it does — REST and single-database `503 {"error":"result too
  large: …"}`, `/api` `1005` (the cells of the returned rows are summed up
  before the JSON document is built); a streamed single-database answer
  (`?stream=true`) is cut off without its closing chunk. `format=file` /
  `n-triple-file` results (written to disk) are not limited.
- **Failed `/api` queries are in the query log (review gapi#8)** with their
  real `statusCode` / `statusMsg` (query errors, unknown formats, timeouts,
  too-large answers); before, only successes were recorded.
- **Password hashing and users.db writes no longer hold the server-wide lock
  (review gapi#6, http#7).** `/api` `usermanage` (add user, set password),
  `userpassword`, REST `/user/create` and `/user/password` hash the new
  password and rewrite/fsync `users.db` with the state lock released (the
  REST endpoints now run deferred; their privilege and target checks stay
  under the lock); every other request — `/cancel` included — is served
  meanwhile. A change overtaken by another save is prepared again, so the
  file never drops a concurrent change, and a user dropped (or a name taken)
  meanwhile is reported instead of being resurrected.
- **Asynchronous callbacks on their own bounded pool (review gapi#12).**
  Callbacks are delivered by dedicated callback threads
  (`TaskConfig::callback_workers`, default 2; queue bounded by
  `max_pending`, beyond which a delivery is recorded as failed), never by a
  task runner, so a slow or silent callback host no longer starves other
  users' asynchronous operations. Name resolution of `http_client` requests
  (callbacks, `SERVICE`, downloads) is bounded by the request's deadline, and
  with the `tls` feature the TLS handshake is too (an `https://` peer that
  never answered the ClientHello used to block forever).
- Asynchronous operations and callbacks run on threads with the HTTP
  workers' stack size (`WORKER_STACK_SIZE`), not the 2 MiB default: a deep
  but allowed async query no longer aborts an unoptimized server (review
  design#8). A runner that panics outside the operation's `catch_unwind`
  gives its slot back (and is replaced when work waits) instead of leaking
  it until no runner starts any more (review gapi#20).
- Library: `gapi::ipfilter::{LoginLockout, LockoutPolicy}` and
  `IpFilter::{auth_refusal, note_auth_failure, note_auth_success,
  note_login_success, with_upstream_lockout, with_lockout_policy}`;
  `http_users::{NewPassword, PendingWrite, WrittenChange, write_off_lock}`,
  `UserStore::{prepare_add, prepare_set_password, apply_write}`;
  `gstore::deadline` (the query deadline timer); `GStoreError::Timeout`;
  `Server::with_query_timeout`, `Server::with_max_result_bytes`;
  `TaskConfig::callback_workers`; hidden test hooks
  `http_users::test_hooks` (park a password hash, count verifications).
- Docs: `TaskConfig::spill_threshold` names `<root>/tasks` (not the export
  directory) and why (review sec#21).
- New tests: `tests/login_lockout.rs`,
  `tests/password_changes_do_not_block_other_requests.rs`,
  `tests/query_limits.rs`, `tests/runaway_evaluation_stops_at_cancellation.rs`,
  `tests/slow_callbacks_do_not_starve_async_operations.rs`,
  `tests/async_operations_run_on_worker_stacks.rs`, plus unit tests (lockout
  policy, off-lock user writes, deadline timer, counting writer, runner
  unwind guard, TLS handshake deadline).

## GAP_PLAN status line

none

## Docs notes

- **docs/DEPLOY.md** (secure defaults and upstream-compat switches table):
  `upstream_login_lockout` — secure default `off` (lockout per account and
  address with expiring back-off, every password path, no hash for a refused
  attempt); upstream value `on` (per address, login/testConnect/REST `/login`
  exempt, reset by any login, until restart); risk: unlimited online guessing
  through `login`/`testConnect` with one PBKDF2 per guess (an unauthenticated
  CPU amplifier), and behind a proxy or NAT any client's failures lock every
  client out until a restart; `scripts/conformance.sh` does **not** enable
  it. Also document `login_lockout_failures` / `login_lockout_max_seconds`,
  and that REST and the single-database server answer a refused attempt
  `429` with `Retry-After` (`/api`: `1101`).
- **docs/DEPLOY.md** (resource limits): `query_timeout` (default 3600 s; the
  "long read + waiting writer freezes the database" scenario is bounded by
  it — lower it on shared servers) and `max_result_bytes` (default 1 GiB;
  `format=file` results are exempt); answers `503` / `1005`.
- **README.md** (configuration keys): `query_timeout`, `max_result_bytes`,
  `upstream_login_lockout`, `login_lockout_failures`,
  `login_lockout_max_seconds` in `[server]`; the single-database server's
  `with_query_timeout` / `with_max_result_bytes` builders.
- **docs/REFACTOR_BACKLOG.md**:
  - review robust#5 / sec#10 (remaining): an intermediate-result *memory*
    budget (rows/bytes during evaluation) is not implemented — only time and
    the serialized answer are bounded; the bushy join tree
    (`eval_join_tree` / `join_sets_hash`), the streaming executor (`topk`)
    and `path_pairs`' both-ends-free enumeration poll the cancel flag only
    between their steps (this round's grant covered the left-deep loop,
    `extend`, `eval_join`, `path_reach`, `path_closure`); for the R3/R4
    Evaluator owners.
  - review gapi#6 (remaining): `grant` / `revoke` / `drop_user` /
    `clear_privileges` / database-registry saves and `IpFilter::save` still
    fsync under the state lock (no hash involved; one fsync each); the
    legacy-hash migration in `UserStore::finish_check` too (rare).
  - review gapi#12 (remaining): no per-user quota of pending asynchronous
    operations (`max_pending` is global); one user can still fill the
    runner queue with expensive operations.
  - review sec#7 (note): session-token requests are not throttled (a token is
    128 random bits, nothing to guess); invalid tokens do not count towards
    the lockout, so expired sessions behind a NAT lock nobody out.
  - review sec#7 / gapi#5 (remaining): the lockout is asked before the hash
    and told the outcome after it, so attempts of one account and address
    sent *concurrently* all pass the check before the first failure is
    recorded: a burst of up to `thread_num` guesses (bounded by
    `max_conns_per_ip`) per lock window instead of one. Concurrent PBKDF2
    verifications are not bounded either (the review's suggestion). Closing
    it needs an in-flight count per key on every password path (refuse
    while count + in-flight checks reach the threshold); not in this
    round's grant.

## Measured numbers

All on the shared 4-vCPU / 15 GB machine with other packages' builds and
test runs going on (load average 5-8; numbers are indicative). Base =
b8b1c8e (a `git archive` copy, its own target dir), branch = this
package's head; release binaries (`cargo +1.99.1 build --release --locked
-p gstore -p gstore-bench --bin gbuild --bin gstore-bench`), measured back
to back in one session.

- **Bench smoke gate (D9; the evaluator's join loops, the REST/`/api` query
  path and the users store changed)**: `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=/home/user/fix-campaign/bench-cache
  target/release/gstore-bench ci --baseline bench/baselines/smoke.json
  --bin-dir target/release`: base PASS, branch PASS (456 gates checked, 0
  failed). Time geomean vs the committed baseline (bound 1.50): base
  LUBM-like snapshot 1.10 / disk 1.15, WatDiv-like snapshot 1.23 / disk
  1.31; branch 1.05 / 1.05 / 1.14 / 1.17. REST single-client qps
  (report-only): `snapshot/sparql/q1` base 14 083 / branch 15 232,
  `snapshot/api/q1` 6 805 / 7 255 — noise-level differences on a loaded
  machine (the new cancellation polls run once per 64 bindings / 1024
  candidates, the deadline is one map insert and removal per statement).
- **Runaway statement (sec#10 / robust#5)**: `tests/query_limits.rs`'s
  three-way cross product over 100 triples (N³ = 10⁶ rows, an expensive
  FILTER) ran 2.2 s to completion before the fix (`"QueryTime":"2218"` in
  the pre-fix run of `a_runaway_api_query_times_out_with_1005_and_is_logged`);
  with `query_timeout=0.05` it is answered "query timed out" and the whole
  test file finishes in 0.7 s. Work counters
  (`tests/runaway_evaluation_stops_at_cancellation.rs`, no clock): a
  three-way cross product of 60 triples whose cancel flag is raised at the
  200th index lookup produced all 219 660 partial solutions before the fix,
  and fewer than a tenth of them after (the test's bound; it stops within one
  64-binding batch, at most 2 048 lookups after the flag); a `*` closure
  over a 400-node chain made 80 501 index lookups after the flag before the
  fix, at most 16 after.
- **Lockout (gapi#5 / sec#7 / http#6)**: password verifications counted per
  user name (`http_users::test_hooks`): refused attempts of a locked account
  run none (`tests/login_lockout.rs`), on `/api`, REST and the pre-body
  check.
- `scripts/conformance.sh --no-build` (dev binaries of this head): all six
  suites pass (basic, update, transaction, api, parser with the two known
  failing cases 15 and 17, python 49/49) — no compat switch needed for the
  new lockout.

## Requests/Not yet

- **Edits outside the package's owned files and granted regions** (each the
  smallest possible; the integrator should expect them):
  - `src/server.rs`: `ServerInner` — the `auth` field's type
    (`Option<SingleAuth>`, carrying its lockout) and two new fields
    `query_timeout` / `max_result_bytes` with their initialisers in
    `Server::bind`; `Server::with_basic_auth` (one line); new
    `Server::with_query_timeout` / `with_max_result_bytes`, `SingleAuth` /
    `SingleAuthCheck` and `Request::basic_token`; in `ServerInner::handle`
    besides the granted Basic-auth part, the `Reply::Stream` arm's
    `stream_select` call gains the budget argument (signature of the private
    `stream_select`, granted, changed).
  - `src/preauth.rs`: `single_db_ingest` / `single_db_hook` take
    `Option<SingleAuth>` (the `serve_forever[_tls]` call lines are textually
    unchanged) and the hook's credential comparison consults the lockout; the
    `the_single_database_hook` unit test's two setup lines.
  - `src/bin/gserver.rs` `main`: two builder lines on the single-database
    `Server` (`with_query_timeout`, `with_max_result_bytes`).
  - `src/config.rs`: besides the granted key anchors and the
    `upstream_compat_warnings()` entry: `DEFAULT_QUERY_TIMEOUT` /
    `DEFAULT_MAX_RESULT_BYTES` after `DEFAULT_ROOT_PASSWORD`,
    `parse_seconds` / `seconds` after `parse_num`, and the five new keys in
    the text and assertions of `tests::every_key_is_read_from_its_section`
    (it requires every key; r2-txn's `txn_idle_timeout` goes there too).
  - `src/lib.rs`: `pub mod deadline;` (new `src/deadline.rs`).
  - `src/http_client.rs` `exchange`: name resolution through the new
    `resolve_within` and the read timeout armed before the TLS handshake (the
    plan's "connect" region); one new `tls` unit test.
  - `src/http_api.rs`: new items next to the granted functions —
    `render_result_within`, `CountingWriter`, `write_ntriples`,
    `result_too_large_msg`, `timed_out_err`, `lockout_refused` — and one unit
    test; `src/gapi/ops_query.rs`: new `result_bytes_exceed` /
    `CELL_OVERHEAD` next to `read_response`.
  - Tests that pinned upstream's lockout now set `upstream_login_lockout`
    in their setup (convention 13b, `review gapi#5` comments):
    `tests/dt_gapi_users.rs::five_failed_logins_lock_the_address_until_a_successful_login`,
    `tests/dt_gapi_async_wiring.rs::a_locked_out_address_cannot_submit_asynchronous_operations`,
    `tests/dt_p1_review.rs::the_ip_lists_and_the_lockout_hold_on_the_rest_endpoints`;
    `tests/http_preauth_limits.rs::a_session_token_from_a_locked_out_address_gets_no_upload_cap`,
    `src/gapi/ipfilter.rs`'s `lockout_after_more_than_four_failures_until_reset`
    likewise.
  - Tests corrected because they codified the reviewed behaviour (D6, with
    `review <id>` comments): `src/http_api.rs`
    `tests::bad_credentials_never_hash_under_the_state_lock` accepts the
    lockout's `429` (http#6); `tests/dt_merge4_wiring.rs::reason_and_pfn_operations_reach_the_access_and_query_logs`
    expects the failed `/api` PFN query in the query log with its status
    (gapi#8: it asserted the record was missing).
- **For r2-txn / r3-rest-parity:** `/tquery` (`run_in_txn`, and its
  `render_result` call) is neither under `query_timeout` nor under
  `max_result_bytes` — those functions belong to r2-txn this round; arming
  is one `crate::deadline::arm(&cancel, config.query_timeout)` and rendering
  through `render_result_within`.
- **For r3-rest-parity:** the single-database server's `/update` endpoint
  (`ServerInner::run_update`, not in this package's regions) answers a
  timed-out UPDATE `400` with the "query timed out" message; `/sparql`
  answers it `503`.
- Partial / deferred ids: none (remaining refactors listed under Docs notes
  → REFACTOR_BACKLOG).
