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
  transaction stays open and can go on or commit. An answer whose
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

## GAP_PLAN status line

none

## Docs notes

- docs/DEPLOY.md (resource limits, closing step): `query_timeout` and
  `max_result_bytes` also bound REST `/tquery` and `/api` `tquery`; a
  timed-out transaction statement leaves the transaction open.
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

- None of the packages' requests to the round-2 integrator is left open:
  `/tquery` is under the statement limits (r2-limits-auth), the reaper
  guard line sits in both `serve_forever` functions (r2-txn /
  r2-index-cancel), the compat warnings are in the helper and its guard
  test (convention 13c), the gfun budget wording is applied
  (r2-analytics), and the W3C subset was re-run after the merge
  (r2-w3c-harness).
- Requests addressed to later rounds stay in the package fragments:
  r2-limits-auth (the single-database `/update` timeout status, for
  r3-rest-parity), r2-terms (REST `/batchInsert` literal rule, SERVICE
  query text, `gadd` exit status), r2-dirlock (remaining `create_dir_all`
  sites for r3-storage-kernel), r2-layout-cli (incremental build install
  warnings for r4-cli-config), r2-index-cancel (`ConcurrentDb`'s CSR
  provider for r4-concurrent), r2-txn (arming the reaper when a user is
  deleted, recorded for the backlog).
