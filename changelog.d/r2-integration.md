# r2-integration — Round 2 merge record: cross-package wiring

## Changelog

Behaviour changes first:

- **A statement inside an HTTP transaction is bounded like every other
  statement** (review sec#10, robust#5; r2-limits-auth's limits meet
  r2-txn's transaction path). REST `/tquery` and `/api` `tquery` ran with
  neither the `query_timeout` deadline nor the `max_result_bytes` budget.
  A transaction statement still running at `query_timeout` is now stopped
  at its next cancellation check and answered as timed out (REST `503`,
  `/api` `1005 "Transaction query fail: query timed out …"`); the
  transaction stays open. A multi-operation UPDATE stopped (or failing)
  part-way keeps the operations before the failing one applied in the
  transaction, as outside one, and since the review fix below the answer
  says so ("operations before the failing one were applied"): roll the
  transaction back to discard them, or commit them knowingly. An answer whose
  serialization passes `max_result_bytes` is refused (REST `503 "result too
  large"`, logged as that failure; `/api` `1005`, checked before the JSON
  document is built). Test: `tests/txn_statement_limits.rs` (four cases,
  all failing on the merged tree before the wiring).
- **No idle-transaction reaper thread outlives `serve_forever`** (review
  conc#14 meets r2-txn's reaper). The reaper guard waited for the reaper
  loop to end but did not join its thread; it now joins it (still bounded:
  a reaper that does not end within 10 s is left detached).
  `tests/cancellation.rs::no_server_thread_outlives_serve_forever` checks
  the `gstore-txn-reap` thread too.
- `config::tests::every_upstream_compat_switch_reports_one_startup_warning`
  lists round 2's two compat entries (`upstream_login_lockout=on`,
  `txn_idle_timeout=0`), so a lost or doubled startup warning fails it
  (convention 3/13c). Both packages had added their line to
  `GStoreConfig::upstream_compat_warnings()` directly: there was no
  standalone `gs_warn!` line to fold.
- `gfun_max_output_bytes`' field doc and `conf/conf.ini.example` say what
  r2-analytics made it: a budget of the graph-function and PFN output of
  one query, not of one call (review gap-gfun-fuzz#5; requested in the
  r2-analytics fragment).

Round-2 review fixes (the land review of the merged round; each with a test
that fails on the code before it, checked by running it against the unfixed
code, except where noted):

- **`query_timeout` counts a statement's evaluation only** (sec#10,
  robust#5). The deadline starts once the statement holds its database's
  guard and stops when the evaluation returns (`StatementClock`; the
  single-database server arms it after its lock too). A trivial statement
  queued behind another holder of the guard — a transaction statement, a
  long write, a save, a backup — was answered "query timed out" once it
  ran; and a deadline passing during the check_point save after an UPDATE
  relabelled the UPDATE's real error as a timeout. Tests:
  `tests/query_timeout_counts_evaluation_only.rs`; `tests/query_limits.rs`'s
  two queued-writer tests now send the writer only once the read holds the
  database, with a `DELETE/INSERT … WHERE` writer that waits longer than
  `query_timeout` (they had sent both at once, so the writer could run
  first and no writer ever waited — corrected with a `review` comment).
- **A failed multi-operation UPDATE inside a transaction names its applied
  part** (REST `/tquery`, `/api` `tquery`): "… (operations before the
  failing one were applied)", also inside the timeout message, as outside a
  transaction. Before, the answer named only the failure and the applied
  operations could be committed unknowingly. Test:
  `tests/txn_failed_update_names_its_applied_part.rs`.
- **A `/commit` or `/rollback` in flight is transaction activity**
  (gap-txn-lifecycle#1/#5; REST and `/api`): accepted while its transaction
  was live, it counts as activity until it has ended it, so waiting for the
  database's guard past `txn_idle_timeout` (behind a save, a long read, a
  CSR build) no longer turns it into an idle rollback (409 / 1005 "not
  running (rolled back after txn_idle_timeout)", writes lost), and no reaper
  rolls the transaction back meanwhile. Test:
  `tests/txn_end_in_flight_is_activity.rs`.
- **Concurrent RocksDB `begin`s copy the store one at a time**
  (gap-rocksdb-exec#3 regression): a begin lock per database is held from
  before the whole-store copy until the transaction is registered; a later
  begin waits, finds the transaction open and is refused without copying
  (the review's probe: 4 concurrent begins held 4 copies at once). Test:
  `tests/rocksdb_begins_copy_one_at_a_time.rs` (rocksdb feature).
- **A RocksDB build stuck past its swap's commit point is refused**
  (durability): when `rocksdb.new` could not be renamed into place (in the
  install, its recovery and a retry), the build no longer opens — and so
  creates — an empty `rocksdb/` (which the next open's recovery kept while
  deleting `rocksdb.new`, the real build) nor removes the other layouts; it
  fails and the next open moves the build into place. Test:
  `fsutil::tests::a_rocksdb_build_left_as_new_is_refused_and_the_next_open_finishes_it`
  (rocksdb feature; failpoint `FAIL_RENAME_INTO_PLACE`).
- **`Database::backup_dir` discards a stale destination `rocksdb/` before
  copying** (durability#4's ordering): a copy failing part-way no longer
  leaves an old RocksDB store to be detected in the destination. Test:
  `tests/backup_dir_discards_a_stale_store_first.rs`.
- **`loadcsr` / `load csr=1` build the CSR as a cancellable registered
  task** (conc#5): listed by `/tasklist`, stopped by `/cancel` and by a
  shutdown past its grace (nothing cached); the shutdown's final pass no
  longer waits for a whole build. Test:
  `tests/loadcsr_is_a_cancellable_task.rs`.
- **No background thread outlives `serve_forever`** (conc#14): the
  asynchronous operations' runner and callback threads are joined (bounded:
  one still busy after `shutdown_wait` finishes on its own); a statistics
  refresh registers its thread under the registry's lock and none starts
  once the server is closing. New test hook `gstore::exit_park` (park a
  thread at its exit, announce its join). Tests:
  `tests/no_background_thread_outlives_serve_forever.rs` (also the first
  failing-before test of the reaper join above, 51245bb, whose extended
  guard did not fail without it), and
  `gapi::ops_db::tests::no_stats_refresh_starts_once_the_server_is_closing`.
- **Queued callbacks no longer pin spilled results** (gapi#12): a result
  spilled to `<root>/tasks` waits as its file's name and is read when it is
  delivered; the queue is also bounded by bytes
  (`TaskConfig::callback_queue_bytes`, 64 MiB) and per user
  (`TaskConfig::callback_pending_per_user`, 32). Test:
  `tests/queued_callbacks_hold_no_spilled_results.rs`.
- **Login lockout** (gapi#5, sec#7, http#6): the address-wide lock spares
  accounts that authenticated from the address within the last hour and
  has its own message ("too many failed logins from this address"); IPv6
  clients count per /64 (IPv4-mapped as IPv4); `refreshconf` applies
  `upstream_login_lockout`, `login_lockout_failures` and
  `login_lockout_max_seconds` (counts kept; the compat warning is printed
  when it turns the switch on). Tests:
  `tests/login_lockout_address_lock_and_refresh.rs`, ipfilter unit tests;
  the unit test `guessing_across_accounts_locks_the_address` asserted that
  a successful account stays refused by the address lock — corrected.
- **The `/api` result byte budget counts escaped JSON** (sec#10):
  `json::escaped_len` for names and values; an escape-heavy answer was sent
  at up to about six times its counted size. Test:
  `tests/api_result_budget_counts_escapes.rs`.
- **`http_client`: every TLS handshake read is bounded by the request's
  deadline** (`DeadlineStream`): a peer trickling its handshake held the
  caller indefinitely. Test (tls feature):
  `http_client::tests::an_https_peer_trickling_its_handshake_hits_the_deadline`.
  The deadline-bounded name resolution of round 2 gets its test
  (`a_hanging_resolver_costs_the_caller_only_its_deadline`, test-only
  `resolve_hook`).
- **The single-database `/update` answers a timed-out UPDATE `503`**, as
  `/sparql` does (it answered `400`). Test:
  `tests/single_database_update_timeout_is_503.rs`.
- Tests made independent of wall-clock speed: `tests/login_lockout.rs`'s
  servers get a one-hour first lock (doc-hidden
  `ApiServer::with_lockout_policy`); their "still locked" assertions ran
  after one or two further password hashes and failed under heavy CPU
  contention (about 9× oversubscription; no deterministic failing-before
  run exists for a timing dependency).
- Fragment corrections: r2-limits-auth (single-database `/update` status,
  the account locks after *more than* `login_lockout_failures` failures,
  the library behaviour change of `IpFilter::locked_out`, the callback and
  budget wording), r2-index-cancel (which threads are joined), r2-terms
  (removing legacy terms by re-reading their source); `conf.ini.example`'s
  lockout comment.
- Test hooks (doc-hidden): `TxnParkPoint::{StatementQueued,
  StatementHolding, BeginQueued, EndQueued}`, `TxnPark::{parked,
  wait_parked_n}`, `ApiServer::{with_lockout_policy, ip_filter}`,
  `TaskManager::queued_callbacks`, `gstore::exit_park`. Library:
  `Database::preload_csr_cancellable`, `TaskManager::join_threads`,
  `json::escaped_len`, `ipfilter::{LockRefusal, lockout_address,
  address_lockout_msg}`, `LoginLockout::{refusal_detail, set_policy}`,
  `IpFilter::{auth_refusal_detail, set_lockout, lockout_of}`.

## GAP_PLAN status line

none

## Docs notes

- docs/DEPLOY.md (resource limits, closing step): `query_timeout` and
  `max_result_bytes` also bound REST `/tquery` and `/api` `tquery`; a
  timed-out transaction statement leaves the transaction open — with the
  operations a multi-operation UPDATE applied before it was stopped (the
  answer names them; roll back to discard them). `query_timeout` measures
  the statement's evaluation only: neither the wait for its database (behind
  a write, a save, a backup) nor the save after an UPDATE counts.
- docs/DEPLOY.md (login lockout): the address-wide lock spares accounts
  known good at the address (successful login within the last hour); IPv6
  clients count per /64; the lockout keys are applied by `refreshconf`.
  Behind a reverse proxy every client shares the proxy's address.
- docs/DEPLOY.md (asynchronous operations): queued callbacks hold spilled
  results by file; `TaskConfig::callback_queue_bytes` and
  `callback_pending_per_user` bound the queue (library settings, no
  `conf.ini` key).
- docs/REFACTOR_BACKLOG.md:
  - Statement atomicity inside a transaction (round-2 review, correctness):
    a failed multi-operation UPDATE keeps its applied operations in the
    transaction (and says so). Undoing back to the statement's start needs
    a savepoint in `Database`'s undo log that covers every layout (the
    Stream-mode overlay, the RocksDB journal, minted terms) — a db-layer
    change beyond this fix (D3).
  - Lockout key behind a reverse proxy (round-2 review, security-limits,
    item 1 of the finding): keying the lockout on a trusted
    `X-Forwarded-For` hop needs a configured notion of which hop to trust
    and the same client address on every lockout path (REST Basic,
    `/login`, `/api`, the pre-body check) — a new configuration decision
    (convention 3), not a local fix; mitigated by the known-good exemption.
  - Callback bounds per callback host (round-2 review): the queue is bounded
    per user and by bytes; a per-host bound was not added.
- docs/DEPLOY.md (secure defaults table, closing step): round 2 adds no
  switch to `scripts/conformance.sh`; the harness still turns on exactly
  round 1's two (`allow_default_root_password=on`, `anonymous_download=on`).
  Round 2's compat values `upstream_login_lockout=on` and
  `txn_idle_timeout=0` print their `upstream-compat:` startup line.

## Measured numbers

Merge: the eight package branches merged in the given order
(`fix/r2-w3c-harness`, `fix/r2-terms`, `fix/r2-txn`, `fix/r2-limits-auth`,
`fix/r2-index-cancel`, `fix/r2-layout-cli`, `fix/r2-dirlock`,
`fix/r2-analytics`) without a textual conflict; the sanctioned overlap
(r2-txn's `start_txn_reaper` line in r2-index-cancel's
`ApiServer::serve_forever[_tls]`) merged cleanly with both sides kept.
`tests/w3c_subset.rs` passed right after the merge (convention 13e; no
`xfail` case turned into an XPASS); `tests/executor_differential.rs` does
not exist yet (r4-sparql-guard).

Full CI matrix at the integration head (4-vCPU machine, at most two heavy
jobs at once):

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `RUSTDOCFLAGS='-D warnings' cargo doc -p gstore --no-deps --locked`:
  clean.
- `cargo test --workspace`: `GSTORE_DISK_QUERY_MODE=memory` 158 test
  binaries, 2729 passed, 0 failed, 7 ignored; `GSTORE_DISK_QUERY_MODE=stream`
  2729 passed, 0 failed, 7 ignored.
- Feature entries (`cargo build --locked --profile test`, clippy `-D
  warnings`, `cargo test -p gstore --features …`, each in a fresh target
  directory): gzip 2505 passed; tls 2510 passed; zip 2517 passed;
  readline 2501 passed; pfn-wasm 2523 passed; `zip pfn-wasm readline`
  2539 passed — all with 0 failed (6 ignored each), builds and clippy
  clean.
- RocksDB: `cargo clippy -p gstore --all-targets --features rocksdb -- -D
  warnings` clean; `cargo test -p gstore --features rocksdb` 2585 passed, 0
  failed, 7 ignored (the layout matrix with its fourth layout and the W3C
  subset on RocksDB included); `cargo test -p gstore-bench --features
  rocksdb` 69 passed, 0 failed, 1 ignored.
- `scripts/conformance.sh` (TMPDIR in the scratch directory): all six
  suites pass (basic, update, transaction, api, parser with the known
  failing cases 15 and 17, python 49/49); `check_compat_switches` saw both
  round-1 switches reported.
- Bench smoke gate (release `gbuild` + `gstore-bench`, `gstore-bench
  report --check README.md CHANGELOG.md` clean, `flock /tmp/gstore-bench.lock
  env GSTORE_BENCH_CACHE=… target/release/gstore-bench ci --baseline
  bench/baselines/smoke.json`): PASS, 456 gates checked, 0 failed (148 table
  rows); time geomean vs the committed baseline: LUBM-like snapshot 1.01,
  disk 1.04, WatDiv-like snapshot 1.13, disk 1.20 (bound 1.50). Machine
  state: the zip feature test had just finished (load average 5.2 at the
  start, falling), nothing else running.
- `/tmp` leak check: `/tmp` listed before and after one full
  `GSTORE_DISK_QUERY_MODE=memory cargo test --workspace` run with nothing
  else running (2729 passed, 0 failed, 7 ignored): 37 entries both times,
  none added, none removed. (An earlier check during the stream-mode run
  showed two `gstore_*` entries of the tls feature run going on in the
  other slot; they were gone once it ended.)

## Requests/Not yet

- Round-2 review fixes without a deterministic failing-before test: the
  statistics refresh's spawn window (its handle is now stored under the
  registry's lock — a structural fix; the window between spawn and storing
  was not reproducible without a hook in the middle of the spawn), and the
  `tests/login_lockout.rs` timing fix (a wall-clock dependency, failing only
  under heavy CPU oversubscription). The reaper join of 51245bb has one now
  (`tests/no_background_thread_outlives_serve_forever.rs`).
- Deferred to the backlog (Docs notes): statement atomicity inside a
  transaction, the lockout key behind a reverse proxy, a per-callback-host
  bound.

- None of the packages' requests to the round-2 integrator is left open:
  `/tquery` is under the statement limits (r2-limits-auth), the reaper
  guard line sits in both `serve_forever` functions (r2-txn /
  r2-index-cancel), the compat warnings are in the helper and its guard
  test (convention 13c), the gfun budget wording is applied
  (r2-analytics), and the W3C subset was re-run after the merge
  (r2-w3c-harness).
- Requests addressed to later rounds stay in the package fragments:
  r2-terms (REST `/batchInsert` literal rule, SERVICE
  query text, `gadd` exit status), r2-dirlock (remaining `create_dir_all`
  sites for r3-storage-kernel), r2-layout-cli (incremental build install
  warnings for r4-cli-config), r2-index-cancel (`ConcurrentDb`'s CSR
  provider for r4-concurrent), r2-txn (arming the reaper when a user is
  deleted, recorded for the backlog).
