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

Round-5 review follow-ups (after f492b14; each fix with a test that
failed before it, recorded in the fragment of the package whose code it
corrects): the raft journal refuses damage before acknowledged records
instead of cutting them (r5-cluster); gnlqa's limiter never resets a
throttled bucket, connections are capped per client at accept time, a
panicking linker build waits for the retry interval, the linker cache
trusts only regular files of the process's own user, the dead test port
is port 0 (r5-gnlqa); a session-token `/api` `login` leaves the
failed-login lockout alone and Ctrl-C no longer ends `gconsole --local`
with its unsaved updates (r5-console); a database directory that existed
before its build or save is made durable, `gbuild`'s claim creates it
durably, and the fsync audit compares content digests (r5-test-audit);
the rewritten tests' closed-port probes, `/shutdown` hash park and split
doc comment (r5-test-audit), the wide-PFN-argument CPU-time ratio and two
failure-path temp cleanups (r5-test-hygiene). No compat switch, no format
change, no public item removed (new: `BuildGuard::create_and_lock`,
`HttpServer::with_max_conn_per_client`, the `Ctx::session_auth` field,
the doc-hidden `sync_probe::content_digest` / `SyncEvent::digest` and
`http_users::test_hooks::park_verification_attempt`).

Nothing else to wire: no round-5 package added a compat switch (no
standalone `upstream-compat` line to fold into
`GStoreConfig::upstream_compat_warnings`, convention 13c), and only
r5-packaging edited `scripts/conformance.sh`, which keeps the convention-4
helper once with round 1's two `compat_switch` lines and the single
`check_compat_switches || exit 1` after the server-start check.

Final check of the round (after the review follow-ups, from 618401c;
test fixes and one release-build warning, no behaviour change; each fix
was shown on the code before it, see the commit messages):

- `gbuild` no longer imports `std::fs` unused in release builds: its only
  bare `fs::` use is the debug-build park hook, so the bench smoke's, the
  packages' and the image's release builds warned about it.
- `tests/api_hygiene.rs`
  `unloaded_statistics_are_read_again_for_a_look_alike_directory`
  (gapi#17) takes the read count once its directory swap is done: between
  the two renames there is no `cold`, and another test's `monitor` of an
  unloaded database prunes the read counts of directories that are gone,
  so a stream-mode workspace run failed "read anew" with 0 for 1.
- The in-process `ApiServer` harnesses wait for the server's shutdown
  (join the serving thread) before their temp root is removed: a server
  still shutting down (rolling back, saving) or still answering wrote below
  the removed root and recreated it. A gzip feature run left
  `gstore_secrev_deep_api_*` (`tests/dt_security_review.rs`), and a failing
  run of `http_api::tests::statements_and_batch_payloads_are_parsed_off_the_state_lock`
  had left `gstore_api_unit_parse_off_lock_*` in `/tmp`. Fixed in that
  file's harness, twelve more integration-test harnesses (among them the
  shared `tests/upstream_scripts/harness.rs`) and the unit tests of
  `src/http_api.rs`, whose servers were never stopped.
- `gnlqa/tests/linker_cache.rs` (cluster-nlqa#22) takes as "another user"
  a uid other than its own: run as `nobody` (65534) it gave its own file to
  itself and failed.

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
- docs/REFACTOR_BACKLOG.md (final check, no action needed now): the
  single-database `Server` harnesses (`tests/http_browser_surface.rs`,
  `tests/http_request_semantics.rs`,
  `tests/chunked_framing_counts_against_the_body_cap.rs`) still drop their
  server without joining it; they serve in-memory databases and write
  nothing below a temp directory, so no leak follows. A harness that gives
  one an on-disk database should join it as the `ApiServer` ones now do.

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
  round: `tests/fsync_audit.rs` 21 (23 after the review follow-ups), `tests/adversarial_coverage.rs` 2,
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

- Round-5 review follow-ups (head of the follow-up commits; toolchain
  1.99.1, debug info off, heavy steps through the two-slot limiter; other
  campaign jobs ran meanwhile, load average up to about 7):
  `cargo fmt --all -- --check` clean; `cargo clippy --workspace
  --all-targets -- -D warnings` clean, and `-p gstore --features "zip
  pfn-wasm readline tls gzip"` clean; `cargo test --workspace
  --no-fail-fast`: `GSTORE_DISK_QUERY_MODE=memory` 236 runs, 3316 passed,
  0 failed, 10 ignored; `stream` 236 runs, 3316 passed, 0 failed, 10
  ignored (`tests/fsync_audit.rs` 23, gnlqa 189 unit + 77 integration).
  RocksDB (target-rocks, sources touched, lock held): clippy `-p gstore
  --all-targets --features rocksdb` clean; `cargo test -p gstore --features
  rocksdb --no-fail-fast` 208 runs, 3055 passed, 0 failed, 10 ignored
  (`fsync_audit` 27). `--features "zip pfn-wasm readline"`:
  `dt_phase4_review_fixes`, `dt_gconsole`, `fsync_audit`,
  `api_session_tokens` all pass. `scripts/conformance.sh` (TMPDIR in the
  scratch directory, empty afterwards): all six suites PASS (parser's known
  cases 15 and 17). `/tmp` listed before and after the two workspace runs
  and the RocksDB run: identical (72 entries). The bench smoke gate was not
  run: no hot path changed (one extra directory fsync per build and per
  save elsewhere; the probe's digest only while a probe is alive).
  MSRV 1.89 was not rechecked (that toolchain is not installed here); the
  new code uses no API newer than 1.82.

- Final check of the round, at 03ad265 (the commit adding this paragraph
  changes only this file). Toolchain 1.99.1, `CARGO_PROFILE_{DEV,TEST}_DEBUG=0`,
  `CARGO_INCREMENTAL=0`, heavy steps through the two-slot limiter (the
  feature, RocksDB and workspace runs partly overlapped; the bench smoke
  ran alone). Every commit since 9eb72a2 carries the two trailer lines;
  no package worktree is left.
  `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings` and `RUSTDOCFLAGS='-D warnings' cargo doc -p gstore
  --no-deps --locked`: clean. `cargo test --workspace`:
  `GSTORE_DISK_QUERY_MODE=memory` 236 runs, 3316 passed, 0 failed, 10
  ignored; `stream` the same; `/tmp` listed before and after each run:
  identical (73 entries, all left by earlier runs). CI's table step
  (`target/debug/gstore-bench report --check` over CHANGELOG.md,
  README.md, bench/README.md, docs/BENCHMARKS.md): clean.
  Feature entries (build `--profile test`, clippy `-D warnings`, test;
  fresh target directory and TMPDIR each, both removed, TMPDIR empty
  afterwards), 208 runs each, 0 failed, 9 ignored: gzip 2961 passed, tls
  2966, zip 2983, readline 2957, pfn-wasm 2986, `zip pfn-wasm readline`
  3012. RocksDB (target-rocks, sources touched, lock held for the whole
  sequence, `cargo build --profile test -p gstore --bin gbuild --features
  rocksdb` before the gstore-bench tests as bench.yml does): clippy clean;
  `-p gstore` 208 runs, 3055 passed, 0 failed, 10 ignored; `-p
  gstore-bench` 14 runs, 93 passed, 0 failed, 1 ignored.
  `scripts/conformance.sh` (TMPDIR in the scratch directory, empty
  afterwards): all six suites PASS (parser's known cases 15 and 17),
  round 1's two compat switches reported. Bench smoke: release build
  without warnings; `report --check README.md CHANGELOG.md` and over every
  marked doc clean; `gstore-bench ci --baseline bench/baselines/smoke.json`
  PASS, 460 gates checked, 0 failed, warm-median time geomean LUBM-like
  snapshot 0.99 (limit 1.56), disk 1.02, WatDiv-like snapshot 1.06, disk
  1.08 (limit 1.50 each), calibration 67.85 ms; the 2x and 3x canaries
  each exit 1. MSRV 1.89: `cargo +1.89 check --locked --workspace
  --all-targets` and `check --release --locked -p gstore --bins --features
  pfn-wasm` (the image's build) clean, without warnings.
  Not root, as GitHub's runner: the 230 test binaries of `cargo test
  --workspace` run as uid 65534 (`setpriv`, a `CARGO_TARGET_TMPDIR` and
  TMPDIR it owns): all pass, nothing left in TMPDIR; linker_cache also as
  uid 1001. `systemd-analyze verify` of the unit is silent as root and as
  uid 65534.

## Requests/Not yet

- For the closing step: the GitHub runs of `packaging.yml` (r5-packaging:
  the Docker image, which is also the MSRV build, the .deb/.rpm smokes,
  the hardened unit under a real systemd) and `bench.yml` (r5-bench: the
  new smoke gates and the 2x canary on a runner, the weekly reference
  job) are the final proof of those packages; neither can run here.
