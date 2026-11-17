# r4-integration — Round 4 merge record: cross-package wiring and three load-dependent tests made deterministic

## Changelog

Behaviour changes first:

- **A forced rebuild removes the replaced database's rules — also the
  staged copy of an interrupted rule swap** (review durability#11, where
  r4-ext-docs meets r4-cli-config). r4-ext-docs stages every replacement
  of `reason_rule_files/` and swaps it in (`reason_rule_files.new` →
  `reason_rule_files`); a swap a crash interrupted after the old directory
  was moved aside leaves only the complete `.new`, which the next
  operation holding the directory's rule lock installs. `gbuild --force`
  deleted only `reason_rule_files/`, without that lock, so the staged copy
  survived the rebuild and the rebuilt database's first rule operation
  brought the replaced database's rules back, with ledgers naming triples
  of the old data. New `gstore::reason::store::remove_rule_dir` (takes the
  rule lock) and `remove_rule_dir_locked` (for a caller that holds it):
  the lock finishes the interrupted swap, and the directory is renamed
  aside durably before it is deleted (`fsutil::discard_dir`), so a crash
  part-way no longer leaves rule files without their ledgers either
  (r4-ext-docs' follow-up note). A symbolic link that leads to no
  directory is removed (the link); any other non-directory there is an
  error, as the recursive delete's was (the helper's first version,
  3070b48, skipped it, and three `http_api` unit tests that put a file
  there as their failure injection caught it; c0a3055). `gbuild`'s rule
  removal, the bulk install's and REST `/build`'s rebuild use them. Tests:
  `tests/rule_dir_replacement.rs` (the snapshot rebuild fails before: the
  staged rules were left behind; the bulk rebuild, which already took the
  lock, is a guard) and
  `reason::store::tests::removing_the_rules_also_removes_an_interrupted_swap`.
- **`gserver` built without the `tls` feature refuses `--tls-cert` /
  `--tls-key` with exit status 2** (r4-api-hygiene's http#19 meets
  r4-cli-config's bulkcli#18): the refusal is an argument error, and every
  argument error of `gserver` now exits 2. `tests/api_hygiene.rs::
  a_gserver_without_tls_refuses_the_tls_options` pins the status (it
  asserted only a failure; with status 1 it fails).

Merge conflict resolved (both sides kept in effect):

- `src/concurrent.rs`, `Snapshot::query` and `VersionView::query`:
  r4-sparql-guard moved the user-defined functions and the PFN caller into
  `EvalSettings`, applied by `Evaluator::with_settings`; r4-ext-docs gave
  each query its own PFN fuel budget with `caller.for_query()`.
  `with_settings` installs the caller through `PfnCaller::with_limits`,
  which also starts a new per-query count, so the settings-based side
  carries ext#1's per-query budget (comments say so at both sites);
  `tests/reason_pfn_regressions.rs::pfn::the_calls_of_one_query_share_a_budget`
  passes on the merged tree (pfn-wasm run below).

Load-dependent tests made deterministic (the resume note's three, each
with its root cause; none was a race in the code under test):

- `db::concurrency_tests::generation_oracle_under_a_rwlock` ("checked >
  0"): the writer made its 60 inserts back to back (write lock, insert,
  unlock, `yield_now`). std's `RwLock` wakes the waiting readers when the
  writer unlocks, but the writer takes the lock again at once; on a loaded
  machine the woken readers were not scheduled before that, so the writer
  could take all 60 write locks before any reader took a read lock — the
  test then failed without a stale answer. The writer now takes the next
  write lock only once a reader has checked the generation it just made
  (a `Mutex` + `Condvar` holds the newest generation checked), so every
  generation 0..=60 is checked under the query cache whatever the
  scheduling, and the final assertion is stronger (more checks than
  inserts). A reader that panics releases the writer (drop guard), the
  writer stops the readers when it returns or panics, the wait has a hang
  guard only.
- `concurrent::tests::concurrent_readers_never_see_torn_state` ("readers
  should have run"): the same shape — 400 commits back to back, and only
  "some read happened" required, so readers that were not scheduled before
  the writer finished made no read. After each commit the writer now waits
  until a read that began after it has finished (`READERS + 1` reads
  finished since the commit, as at most `READERS` were in flight), so
  reads interleave with all 400 commits; the test now also requires that
  the readers saw both states (1 and 2 triples). Same guards.
- `no_background_thread_outlives_serve_forever::
  the_asynchronous_operation_runner_is_joined` (a `gstore-task` thread
  still listed right after the join): the runner was joined.
  `JoinHandle::join` (`pthread_join`) returns when the exiting thread's
  kernel task clears its thread id, which happens inside `do_exit` after
  `PF_EXITING` is set and before the task is removed from
  `/proc/self/task`; so a joined thread can still be listed there for a
  moment, longer when the machine preempts it in between. Measured with a
  small program: 52 of 20,000 joined threads were still listed right after
  the join, every one with `PF_EXITING` set. `threads_named` (in this file
  and in `tests/cancellation.rs`, whose shutdown checks have the same
  race) now skips tasks whose `/proc/…/stat` flags have `PF_EXITING`; a
  thread still running or parked by `exit_park` counts as before, and the
  joining checks (`wait_joining`, `serve_forever` returning) are
  unchanged.

Integration fixes without behaviour change: `src/config.rs`' module docs
name `pfn_fuel_per_query` in the `[pfn]` key list (r4-ext-docs' request);
the comment above the modify template loop in `src/query/engine.rs` no
longer says `DELETE WHERE { … _:v }` deletes what it matched — the parser
refuses blank nodes there now (r4-db-graph's request); `src/bulk/commit.rs`'
docs link `DirLock` by its path (the `-D warnings` rustdoc job failed on
the merged tree and on r4-cli-config's branch).

No standalone `upstream-compat` startup line and no `scripts/conformance.sh`
change came with round 4: no package added a compat switch, so there was
nothing to fold into `GStoreConfig::upstream_compat_warnings` (convention
13c), and the harness still turns on exactly round 1's two switches.

## GAP_PLAN status line

none

## Docs notes

- README.md / CHANGELOG.md (closing step): `gbuild --force`, a bulk
  rebuild and REST `/build` over an existing database remove the replaced
  database's reasoning rules under the rule lock and rename them aside
  before deleting them, so neither an interrupted rule swap nor a crash
  during the removal can bring old rules back to the rebuilt database.
- CHANGELOG.md: `gserver --tls-cert/--tls-key` without the `tls` feature is
  an argument error with exit status 2 (r4-api-hygiene's and
  r4-cli-config's fragments describe the two halves).
- docs/DEPLOY.md (closing step): round 4 adds no compat switch;
  `[pfn] pfn_fuel_per_query` (r4-ext-docs) is a tuning key.
- docs/REFACTOR_BACKLOG.md:
  - (bulkcli#6, D8) `Database::restore_from_zip` (`grestore x.zip`) and
    `/api` restore still unpack into the fixed `<target>.unzip-tmp`
    (`ScratchDir::fresh`, removing what is there), which r4-cli-config
    offered to switch to `ScratchDir::create_unique`. Not changed: both run
    under the database's restore lock / the `/api` name reservation, so the
    race of bulkcli#6 does not apply, and `fresh` removes the leftover of a
    crashed restore at the next one, which `create_unique` (a new name per
    run) would leave behind without a sweep.
  - (durability#11) `fsutil::discard_dir` renames a directory aside only
    when it is one; the rule-directory helper above handles the other
    cases itself. Other `discard_dir` callers (database drops) keep their
    behaviour for a non-directory at the target.
- Nothing else: every package's Docs notes stand as written in its
  fragment.

## Measured numbers

Merge: the seven package branches merged with `git merge --no-ff` in the
given order (`fix/r4-sparql-guard`, `fix/r4-storage-format`,
`fix/r4-cli-config`, `fix/r4-ext-docs`, `fix/r4-concurrent`,
`fix/r4-api-hygiene`, `fix/r4-db-graph`). One textual conflict
(`src/concurrent.rs`, above); every other file two packages touched
(`conf/conf.ini.example`, `src/config.rs`, `src/bin/gserver.rs`,
`src/bulk/rocks_sink.rs`, `src/db/mod.rs`, `src/http_api.rs`, `src/lib.rs`,
`src/pfn/mod.rs`, `src/query/engine.rs`, `src/store/mod.rs`) merged cleanly
with both sides kept. Four branches (storage-format, cli-config,
ext-docs, concurrent) fork from b147336, before the round-3 review fixes;
their merges keep those fixes (in the runs below). Convention 13e on the
merged tree before any wiring: `tests/executor_differential.rs` 5 passed,
`tests/w3c_subset.rs` 18 passed (r4-db-graph's negative cases and
r4-sparql-guard's new case dirs meet only there; the differential requires
a failing case to fail under every executor setting, which the negative
cases do).

Failing-before checks: `tests/rule_dir_replacement.rs` with
`src/bin/gbuild.rs` at the merge head: 1 passed, 1 failed
(`a_forced_snapshot_rebuild_removes_an_interrupted_rule_swap`: "the
replaced database's staged rules were left behind"); with the fix 2
passed. `api_hygiene::a_gserver_without_tls_refuses_the_tls_options` with
`main()`'s argument-error status set back to 1: failed (exit status 1,
expected 2); restored, passed. The first memory-mode workspace run (at
211ba7d) failed 3 `http_api` unit tests: a file at the rule directory's
place no longer made the removal fail (3070b48); c0a3055 fixed that
before the matrix below.

The three load-dependent tests: with 12 busy loops on the 4 CPUs, 25 of
25 runs of the two rewritten unit tests passed, and 10 of 10 pinned to
one CPU (`taskset -c 0`); an injected stale answer / torn read fails each
at once (no hang). The `/proc` window: a small program spawning and
joining 20,000 threads found 52 still listed in `/proc/self/task` right
after the join (idle machine) and 6 under load, all with `PF_EXITING`.

Full CI matrix at c0a3055 (4-vCPU machine shared with the three round-5
implementers, which ran workspace tests and builds throughout — load
average 5-13; at most two heavy jobs at once through the slot limiter;
toolchain 1.99.1, `CARGO_PROFILE_{DEV,TEST}_DEBUG=0`,
`CARGO_INCREMENTAL=0`):

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `RUSTDOCFLAGS='-D warnings' cargo doc -p gstore --no-deps --locked`:
  clean (after fdedd9e; it failed on the merged tree with the unresolved
  `DirLock` link).
- `cargo test --workspace`: `GSTORE_DISK_QUERY_MODE=memory` 200 test
  binaries, 3077 passed, 0 failed, 10 ignored;
  `GSTORE_DISK_QUERY_MODE=stream` 200 binaries, 3077 passed, 0 failed,
  10 ignored.
- Feature entries (clippy `-D warnings`, then `cargo test -p gstore
  --features …`, each in a fresh target directory under the scratch
  directory, deleted afterwards; TMPDIR in the scratch directory, empty
  after each run): gzip 2853 passed; tls 2858; zip 2872; readline 2849;
  pfn-wasm 2877 (`pfn::the_calls_of_one_query_share_a_budget` among
  them); `zip pfn-wasm readline` 2900 — 188 test binaries each, 0 failed,
  9 ignored, clippy clean. (The runner of the six entries hit its
  two-hour limit while the last test step waited for a slot; that step
  was started again on its own, in the same target directory.)
- RocksDB (`CARGO_TARGET_DIR=…/target-rocks`, `src/lib.rs`,
  `bench/src/lib.rs`, `bench/src/main.rs` touched and the rocksdb lock held
  for each command): `cargo clippy -p gstore --all-targets --features
  rocksdb -- -D warnings` clean; `cargo test -p gstore --features rocksdb`
  188 binaries, 2943 passed, 0 failed, 10 ignored (the executor
  differential on the RocksDB layout included); `cargo test -p
  gstore-bench --features rocksdb` 9 binaries, 69 passed, 0 failed, 1
  ignored; TMPDIR empty afterwards.
- `scripts/conformance.sh` (TMPDIR in the scratch directory, toolchain
  1.99.1): all six suites pass — basic 10 s, update 4 s, transaction 2 s,
  api 39 s, parser 5 s (failing cases 15 and 17, the known ones), python
  8 s; `check_compat_switches` saw both round-1 switches reported
  (`allow_default_root_password=on`, `anonymous_download=on`); TMPDIR
  empty afterwards.
- Bench smoke gate: `cargo build --release --locked -p gstore -p
  gstore-bench --bin gbuild --bin gstore-bench` clean; `gstore-bench report
  --check README.md CHANGELOG.md` clean; `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=… target/release/gstore-bench ci --baseline
  bench/baselines/smoke.json`: PASS, 456 gates checked, 0 failed; time
  geomean vs the committed baseline: LUBM-like snapshot 0.86, disk 0.93,
  WatDiv-like snapshot 0.98, disk 1.08 (bound 1.50); calibration score
  69.32 ms; load average 5.64 when it started.
- `/tmp` leak check: `/tmp` listed before and after the
  `GSTORE_DISK_QUERY_MODE=memory` workspace run and after the stream-mode
  run (54 entries): nothing these runs created was left. The only
  differences were a compiler's transient temporary files (`cc*.o`, gone
  again) and `gsb-cli-{micro,tiny,watdiv}-27764`, the bench CLI test
  directories of another worktree's test process (pid 27764, r5-gnlqa's
  `bench/tests/cli.rs` binary, running then), removed when it finished.

## Requests/Not yet

- For r5-test-hygiene (r4-cli-config's note, not changed here):
  `tests/dt_zip_io.rs`' assertions `!dir.join("z.db.unzip-tmp-0").exists()`
  became vacuous with the unpredictable scratch names; r4-cli-config's
  `gbuild_unpacks_into_a_fresh_scratch_directory_after_taking_the_lock`
  checks that no `*.unzip-tmp*` is left, but the old assertions want a
  prefix scan.
- For r5-console (r4-ext-docs' note): ext#14's local-console cases
  (`gconsole --local` snapshot updates lost on quit, statements split at
  `;`) belong with ext#5 / ext#9.
- `gapi::tasks::spawn_pool_thread` drops (detaches) the handles of pool
  threads whose `is_finished()` is true when it starts a new one; such a
  thread has returned from its function but may still be running its
  thread-local destructors, so it can outlive a later `serve_forever` by
  that much. No test sees it (each test starts one runner); for the owner
  of `gapi/tasks.rs` in round 5 (join them instead of dropping them).
