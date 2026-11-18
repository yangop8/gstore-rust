# r5-integration — Round 5 merge record: seven packages merged, cross-package checks, the full CI matrix

## Changelog

No behaviour change of its own. Round 5's seven package branches merged
with `git merge --no-ff` in the given order (`fix/r5-console`,
`fix/r5-cluster`, `fix/r5-gnlqa`, `fix/r5-test-audit`,
`fix/r5-test-hygiene`, `fix/r5-bench`, `fix/r5-packaging`), each package's
changes described in its own fragment.

Merges: all seven merged without a textual conflict. The files two of them
touched merged with both sides kept: `src/http_api.rs` (r5-test-audit: the
durable `/backup` and `/upload` directories, the `#[cfg(test)]` parse
hooks and the http#20 latches; r5-test-hygiene: the per-server
`expired_snapshot_reads` counter) and `src/http_client.rs` (r5-console:
`tls_client_stream`; r5-test-audit: its `#[cfg(test)]` module). Three
packages (r5-test-audit, r5-bench, r5-packaging) fork from 9f00e07, before
the round-4 review fixes, and r5-test-audit edits five files those fixes
also changed (`src/db/persist.rs`, `src/gapi/ops_db.rs`,
`src/gapi/tasks.rs`, `src/http_api.rs`, `src/reason/store.rs`): disjoint
regions, both sides kept; the base's CI fix 64fa658 (the spill-file test
of `gapi::tasks` waits for `wait_async_idle`) is in r5-test-audit's test
module of `src/gapi/tasks.rs` unchanged.

Wiring (integration commits):

- `src/console/commands.rs` `tests::history_is_kept_and_capped` and
  `src/console/mod.rs` `tests::ports_come_from_the_configuration` remove
  their per-process scratch file / directory under the temp directory with
  a drop guard, so a failing assertion no longer leaves them behind
  (r5-console's note; test-only).
- r5-console added an authentication path to the upstream `/api` (a
  `POST /login` session token in place of the password, ext#12) while
  r5-test-audit mapped every authentication path to its regression test
  (sec#20); the map in `tests/adversarial_coverage.rs`' module docs gets
  that path's row, pointing at
  `api_session_tokens::the_upstream_api_takes_a_login_session_in_place_of_the_password`
  (no password is checked there, so the path neither counts towards the
  failed-login lockout nor — since the round-5 review follow-up, with
  `a_session_login_leaves_the_failed_login_lockout_alone` added to the
  row — clears it; an unknown, logged-out or revoked token is refused). The copy of
  the table in `changelog.d/r5-test-audit.md` is left as that package
  wrote it (fragments are folded by the closing step).

Nothing else to wire: no round-5 package added a compat switch (no
standalone `upstream-compat` line to fold into
`GStoreConfig::upstream_compat_warnings`, convention 13c), and only
r5-packaging edited `scripts/conformance.sh`, which keeps the convention-4
helper once with round 1's two `compat_switch` lines and the single
`check_compat_switches || exit 1` after the server-start check.

## GAP_PLAN status line

none

## Docs notes

- docs/REFACTOR_BACKLOG.md (requests of this round no package owned; each
  with its review id and reason):
  - tests-a#7 (remainder): `free_port()` in `tests/dt_gserver_lifecycle.rs`
    (six callers) still binds port 0, drops the listener and starts
    `gserver -b` on that port. r5-console fixed the `dt_gconsole.rs` sites,
    where a collision ran the console against another test's server; here
    a collision fails loudly (`gserver -b` exits non-zero, the port is read
    back from the pid file), as the verifier noted. `-p 0` with the port
    read from the pid file, as `background_start_status_refusal_and_stop`
    does, would remove it.
  - ext#12 follow-ups (r5-console): the pre-body admission of the `/api`
    payload route (`src/preauth.rs`) takes credentials from the URL query
    only, not a session token, so a session-authenticated `/api` body stays
    under `preauth_max_body` (64 MiB), as bodies carrying the password
    always did; and a plain-HTTP request to a `gserver --tls-cert` waits
    about 31 s (the read timeout) before the connection closes — a prompt
    close or a clear-text 400 in `src/server.rs`'s TLS accept path would
    help users who forget `--tls`. Neither is a correctness problem.
  - `gapi::tasks::spawn_pool_thread` (r4-integration's request for round
    5, not taken up: r5-test-audit owned only that file's `#[cfg(test)]`
    module): it drops the handles of pool threads whose `is_finished()` is
    true when it starts a new one; such a thread has returned from its
    function but may still be running its thread-local destructors, so it
    can outlive a later `serve_forever` by that much. Joining those handles
    outside the lock fixes it; no test can observe the window without a
    hook in the runner's thread exit.
- docs/DEPLOY.md / README.md (closing step): round 5 adds no gserver
  compat switch; `scripts/conformance.sh` still turns on exactly round 1's
  two. The adversarial-coverage table the closing step takes from
  `changelog.d/r5-test-audit.md` gets the `/api` session-token row above
  (as in `tests/adversarial_coverage.rs`).
- Every package's Docs notes stand as written in its fragment.

## Measured numbers

Full CI matrix at f492b14 (the seven merges plus the console-test
commit). The commit after it, which adds this fragment and one row to the
module docs of `tests/adversarial_coverage.rs`, changes no code; fmt,
workspace clippy and that test binary (2 passed) were run again on it. Toolchain 1.99.1,
`CARGO_PROFILE_{DEV,TEST}_DEBUG=0`, `CARGO_INCREMENTAL=0`, heavy steps
through the two-slot limiter; the 4-vCPU machine had no other campaign
jobs, but the feature, RocksDB and stream-mode runs overlapped each other
(load average up to about 8).

- `cargo fmt --all -- --check`: clean. `cargo clippy --workspace
  --all-targets -- -D warnings`: clean (run on the merged tree and again at
  f492b14). `RUSTDOCFLAGS='-D warnings' cargo doc -p gstore --no-deps
  --locked`: clean.
- `cargo test --workspace --no-fail-fast`: `GSTORE_DISK_QUERY_MODE=memory`
  232 test runs (binaries and doc-tests), 3304 passed, 0 failed, 10
  ignored; `GSTORE_DISK_QUERY_MODE=stream` 232 runs, 3304 passed, 0
  failed, 10 ignored. Convention 13e inside them:
  `tests/executor_differential.rs` 5 passed, `tests/w3c_subset.rs` 18
  passed (no round-5 package added case dirs); the new suites of the
  round: `tests/fsync_audit.rs` 21, `tests/adversarial_coverage.rs` 2,
  `tests/api_session_tokens.rs` 1, `tests/conformance_harness.rs` 2,
  `tests/package_smoke.rs` 1 (curl, `systemd-analyze` and
  `deb-systemd-helper` are installed here, so the packaging tests ran
  rather than skipped). CI's table step, `target/debug/gstore-bench
  report --check` over every marked doc (CHANGELOG.md, README.md,
  bench/README.md, docs/BENCHMARKS.md): clean.
- Feature entries (`cargo build --locked --profile test -p gstore
  --all-targets`, `cargo clippy -p gstore --all-targets -- -D warnings`,
  `cargo test -p gstore`, each with `--features …` in a fresh target
  directory under the scratch directory, deleted afterwards; TMPDIR in the
  scratch directory, empty after each run): every build and clippy clean;
  206 test runs each, 0 failed, 9 ignored — gzip 2955 passed, tls 2960,
  zip 2977, readline 2951, pfn-wasm 2980, `zip pfn-wasm readline` 3006.
- RocksDB (`CARGO_TARGET_DIR=…/target-rocks`, `src/lib.rs`,
  `bench/src/lib.rs`, `bench/src/main.rs` touched before each command,
  the rocksdb lock held for the whole sequence): `cargo clippy -p gstore
  --all-targets --features rocksdb -- -D warnings` clean; `cargo test -p
  gstore --features rocksdb` 206 runs, 3048 passed, 0 failed, 10 ignored;
  `cargo test -p gstore-bench --features rocksdb` 13 runs, 93 passed, 0
  failed, 1 ignored. r5-bench's e2e tests refuse a `gbuild` older than
  its sources (infra#18), and the third touch made the one the gstore
  tests had built stale, so `cargo build --profile test -p gstore --bin
  gbuild --features rocksdb` ran between the two test commands (the CLI
  builder was used: `tests/cli.rs` 10 passed, 1 ignored).
- `scripts/conformance.sh` (TMPDIR in the scratch directory, empty
  afterwards): all six suites PASS — basic 9 s, update 4 s, transaction
  1 s, api 40 s, parser 7 s (failing cases 15 and 17, the known ones),
  python 10 s (`sdk_check: 49 passed, 0 failed`; python3 `requests` is
  installed, so r5-packaging's skip rule did not apply); the server took
  its own port (46431); `check_compat_switches` saw round 1's two switches
  reported (`allow_default_root_password=on`, `anonymous_download=on`).
- Bench smoke gate: `cargo build --release --locked -p gstore -p
  gstore-bench --bin gbuild --bin gstore-bench` clean; `gstore-bench
  report --check README.md CHANGELOG.md` clean; `flock
  /tmp/gstore-bench.lock env GSTORE_BENCH_CACHE=… target/release/
  gstore-bench ci --baseline bench/baselines/smoke.json` (r5-bench's new
  gates): PASS, 460 gates checked, 0 failed; warm-median time geomean vs
  the committed baseline: LUBM-like snapshot 0.98 (limit 1.56, the
  baseline's spread), disk 1.01, WatDiv-like snapshot 1.06, disk 1.08
  (limit 1.50 each); calibration 69.28 ms; load average 3.07 at its start.
- `/tmp` leak check: `/tmp` listed before the memory-mode workspace run
  and after the memory-mode and stream-mode runs (72 entries each time):
  identical, nothing left behind (the feature, RocksDB and conformance
  runs used scratch TMPDIRs, each empty afterwards).
- MSRV (r5-packaging made 1.89 every member's `rust-version`; the console,
  cluster and gnlqa code merged here was written without it):
  `cargo +1.89.0 check --locked -p gstore --bins --features pfn-wasm` (what
  the Dockerfile's builder compiles) and `cargo +1.89.0 check --locked -p
  gnlqa`: both pass on the merged tree.
- The console-test guard commit: `cargo test -p gstore --lib console::`
  28 passed.

## Requests/Not yet

- For the closing step: the GitHub runs of `packaging.yml` (r5-packaging:
  the Docker image, which is also the MSRV build, the .deb/.rpm smokes,
  the hardened unit under a real systemd) and `bench.yml` (r5-bench: the
  new smoke gates and the 2x canary on a runner, the weekly reference
  job) are the final proof of those packages; neither can run here.
