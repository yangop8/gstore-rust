# r5-bench — Bench gates that see sub-millisecond regressions, validated bless, bench CLI/phase hygiene, CI report checks

## Changelog

Behaviour changes first (all in the `gstore-bench` harness and the CI
workflows; no gstore library, server or on-disk change):

- **The smoke gate sees sub-millisecond regressions** (review infra#2,
  infra#4). The per-query timing gate's floor drops from 2 ms to 0.1 ms
  (`(m + 0.1 ms) / (b + 0.1 ms) > 3` fails), and the per-backend query
  geomean takes **every** query as `(m + 0.05 ms) / (b + 0.05 ms)`, each
  ratio clamped to `[1/3, 3]`, instead of only the queries whose baseline
  median is at least 1 ms. Its limit is `max(1.5, s)`, where `s` is the
  baseline run's own process-to-process spread (`gates::query_spread`: the
  same geomean of each query's slowest process median against its
  fastest), never above `geomean.max_spread` 1.6 — the limit sits outside
  the variance the baseline itself shows (the old 1.5 was inside the LUBM
  snapshot run's 1.68). In `bench/baselines/smoke.json` 41 of 68 queries
  answer in 0.01–0.7 ms; before, the review's repro (every one of them
  regressed to 3 ms, 4–298×) passed all 456 gates, and a 4× slowdown of a
  2.3 ms query passed too. Now any single query slowed 50× fails, a 0.02 ms
  query slowed 20× fails, the canary fails at 2× as well as 3×, and the
  baseline against its own slowest query processes passes.
  `bench/thresholds.json` gains `geomean.floor_ms`, `geomean.max_spread`
  and a `bless` section; thresholds files without them keep their old
  meaning (serde defaults; `min_base_ms` stays, now 0).
- **New exact gate `exact.config`** (infra#10): a run must be measured with
  the baseline run's builder, warm repetitions and query processes (files
  that did not record them are not compared). `check` takes `--reps`,
  `--query-procs` and `--builder` from the baseline unless given.
- **`bless` refuses unfit results; `ci` refuses provisional baselines**
  (infra#10). `bless` used to run only the exact self-check. It now also
  refuses (exit 1) a result measured by a debug build of the harness, from
  a dirty tree or without a recorded commit, with the library builder or a
  `gbuild` from a `debug` directory, with a 1-minute load above
  `bless.max_load_per_cpu` (1.0) per CPU when the run started, or with
  query processes spread wider than `geomean.max_spread`
  (`gates::baseline_problems`). `--provisional` blesses it anyway, marked,
  with the reasons appended to the note. `ci` refuses (exit 2, before
  measuring) a provisional baseline or one `bless` would refuse unless
  `--allow-provisional`; `check` warns. `bless` reads `--thresholds`.
- **Point durations are validated** (infra#16). `--conc-secs` and
  `--mixed-secs` must be finite, positive and at most 3600,
  `--conc-clients` at least 1, and a mixed point may not need more than
  100 000 writes (its insert-number stride); checked in the option parser
  before anything is generated, and again in each phase child (which
  returns an error instead of panicking in `Duration::from_secs_f64`). The
  mixed writer makes its statements as it sends them (`--mixed-secs 1e9`
  asked for 480 GB up front), on a scoped thread that is stopped as soon
  as the readers' drive returns or unwinds (a panic there would otherwise
  leave the scope waiting for the writer until the phase timeout).
- **Stray positional arguments are usage errors** (infra#5): `gen`,
  `queries`, `run`, `check`, `ci`, `bless` and `calibrate` exit 2 on one
  (`check --from X other.json` gated X against the default baseline, `run
  --profile reference 38` ran univ 8).
- **The update phase detects lost writes** (infra#9). It now checkpoints
  and reopens while the update chunk is inserted (the original count plus
  the chunk's, and the chunk's `DELETE DATA` must then remove every chunk
  triple), and re-runs every query on the database reopened after the final
  checkpoint. New optional `UpdateResult` fields: `grown_checkpoint_ms`,
  `grown_reopen_ms`, `grown_reopen_triple_num`,
  `chunk_deleted_after_reopen`, `grown_reopen_ok`, `reopen_mismatched`,
  `answers_after_reopen_ok` (older files read as before). New child test
  hook `--inject lose-writes:update`.
- **Bench end-to-end tests fail on a missing, stale or RocksDB-less
  `gbuild`** (infra#18) instead of silently using the library builder or a
  binary older than the sources it is compiled from (the files rustc's
  dep-info in the target directory lists for `gbuild` and the gstore
  library; a `#[cfg(test)]` module, another binary or any other file under
  `src/` does not count, since cargo does not relink `gbuild` for it);
  `GSTORE_BENCH_TEST_BUILDER=lib` chooses the library builder explicitly.
  `cargo test -p gstore-bench` alone now needs
  `cargo build --profile test -p gstore --bin gbuild [--features rocksdb]`
  first (`cargo test --workspace` and both CI jobs already build it).
- **Reports match sections by workload** (infra#8): a section is compared
  with the baseline section of the same id and workload, else with the
  baseline's section of the same workload, parameters and generator version
  wherever it sits (a main workload is no longer paired with another
  workload's).
- **CI** (infra#2, #6, #8, #13): bench.yml's smoke job and ci.yml's test
  job check the generated tables of every tracked Markdown file with a
  marker (`git grep`; docs/BENCHMARKS.md and bench/README.md were never
  checked); the smoke canary runs at 2× and 3×; the weekly reference job
  records the reference profile as the Phase 6 closure did (snapshot and
  RocksDB, the disk layout under `--label materialize` / `--label stream`,
  merged) and compares with `bench/results/phase6-final-lubm<scale>.json`,
  its WatDiv-like section with `phase6-final-watdiv10.json`, then
  pre-Phase-6; ci.yml's report check runs `target/debug/gstore-bench`, the
  binary the test steps built, instead of `cargo run -p gstore-bench`, which
  compiled gstore and gstore-bench again (in the dev profile; and even
  `--profile test` would, because features resolved for gstore-bench alone
  differ from the workspace's — measured locally: two `libgstore-*.rlib`
  units after `cargo test --workspace` and `cargo run --profile test -p
  gstore-bench`).
- New tests: `bench/tests/smoke_gate_sensitivity.rs`,
  `bench/tests/baseline_validity.rs`, `bench/tests/cli_arguments.rs`,
  `bench/tests/update_phase_reopen.rs`, `bench/tests/reference_compare.rs`;
  unit tests in `gates.rs`, `runner/cli.rs`, `runner/conc.rs`;
  `a_missing_or_stale_gbuild_is_refused` and
  `only_the_sources_gbuild_is_compiled_from_make_it_stale` in
  `bench/tests/cli.rs`.
- Corrected tests (D6): `bench/tests/gates.rs`'s
  `one_slow_query_of_20ms_or_more_fails` asserted that a sub-millisecond
  query slowed 4× and four of them slowed 5× pass — the blindness of
  infra#2; it now asserts that a 0.2 ms query slowed 50× fails and that 2×
  jitter of the four sub-ms queries passes inside a geomean over all 14
  (comment `review infra#2`). `bench/tests/cli.rs`'s
  `new_workloads_pass_check_and_labelled_runs_merge` blesses a debug test
  build, which `bless` now refuses: its setup line passes `--provisional`
  (comment `review infra#10`).

## GAP_PLAN status line

none

## Docs notes

- docs/BENCHMARKS.md (owned, edited directly): §1 update phase, §4 gates
  (0.1 ms floor, all-query clamped geomean with the spread-bounded limit,
  `exact.config`, the 2×/3× canaries, CI's doc check, weekly comparison,
  the test-built gstore-bench, the strict `gbuild` of the bench tests), §5
  bless rules
  (refusals, `--provisional`, `ci --allow-provisional`; the committed
  `smoke.json` is fit). Generated tables unchanged.
- bench/README.md (owned, edited directly): commands table (`ci
  --allow-provisional`, `bless` refusals), positional arguments, point
  duration limits, the update phase, the gates, the canary, reports'
  section matching and CI's doc check, the CI jobs, the tests' `gbuild`
  requirement.
- CHANGELOG.md: the Changelog bullets above.
- README.md: nothing required (it names `gstore-bench ci` and the smoke
  baseline without describing the gates; its generated tables are
  unchanged).
- docs/REFACTOR_BACKLOG.md §I.7: replace "基准:计时闸门未收紧(空闲机上的波动达
  1.42),等有 CI runner 的数据再看" with: the timing gates were reworked by
  review infra#2/#4 (0.1 ms per-query floor, all-query clamped geomean whose
  limit is max(1.5, the baseline's own spread) ≤ 1.6); still open: the
  limits are calibrated on the reference VM, not on GitHub runner data
  (infra#4's suggestion to bless from CI artifacts of the gating runner
  class is not done: the smoke baseline is blessed on the reference VM and
  CI gates against it, with calibration normalizing); keep the WatDiv
  zipfian/normal and mixed-writer notes.
- docs/REFACTOR_BACKLOG.md (new entry, D3): the update phase's lost-write
  check is a harness-level check through the public API; a backend-level
  fault-injection test of `checkpoint` itself belongs to the storage tests
  (review infra#9 asked for the harness check only).

## Measured numbers

No gstore engine code changed, so the smoke timings are the base's; what
changed is what the gates see. Machine: the shared 4-vCPU / 15 GB VM with
other packages building (1-minute load 9.71 and 7.66 at the starts of the
two runs, calibration 74.6 and 79.6 ms against the baseline's 72.1).

- `target/release/gstore-bench ci --baseline bench/baselines/smoke.json`
  (release build of this branch, `GSTORE_BENCH_CACHE` with the cached univ-1
  and WatDiv-1 data), twice: **PASS, 460 gates, 0 failed, no retries** both
  times (456 before plus 4 `exact.config`). Query geomean (new rule; limit)
  run 1 / run 2: LUBM snapshot 0.88 / 0.80 (1.56, raised by the baseline's
  spread 1.56), disk 0.93 / 0.86 (1.50), WatDiv-like snapshot 1.13 / 0.99
  (1.50), disk 0.95 / 1.14 (1.50); largest per-query ratio 1.40
  (watdiv-like:snapshot/S1) / 1.33 (watdiv-like:disk/F3), limit 3.0.
- The same run-1 result under the old rules (`compare` with the old
  thresholds: 2 ms floor, geomean over queries ≥ 1 ms): PASS, geomeans
  0.86 / 0.90 / 1.11 / 0.91.
- Canary on the run-1 result against itself: `--self-test-slowdown 2` exit
  1 (all four geomeans fail: 1.75, 1.75, 1.62, 1.55 against 1.50),
  `--self-test-slowdown 3` exit 1 (2.47, 2.46, 2.22, 2.06, plus build
  gates).
- One query slowed 50× in the run-1 result (watdiv-like:snapshot/L4, 0.014
  → 0.70 ms): new gates exit 1 (`time.query` 7.03 > 3.00); old rules PASS.
- Gate statistics of the committed `smoke.json` (tests/
  smoke_gate_sensitivity.rs, deterministic): process spread 1.56 / 1.19 /
  1.25 / 1.12 (LUBM snapshot, disk, WatDiv-like snapshot, disk) — all under
  `max_spread` 1.6; the 68 single-query 50× cases all fail; under the old
  rules the baseline against its own slowest processes failed
  `time.geomean` (1.68 > 1.5) and most 50× cases passed.
- Tests at the branch head after review: `cargo test --workspace`
  (default features) 3101 passed, 0 failed, 10 ignored with
  `GSTORE_DISK_QUERY_MODE=memory` and again with `stream`; `cargo test -p
  gstore-bench --features rocksdb` (gbuild built with RocksDB first,
  target-rocks) 93 passed, 0 failed, 1 ignored;
  `cargo clippy --workspace --all-targets -- -D warnings` and `cargo clippy
  -p gstore-bench --all-targets --features rocksdb -- -D warnings` clean.
- CI doc check: with one row of a docs/BENCHMARKS.md table changed, the new
  step (git grep over marked docs) exits 1 naming the stale table; the old
  step (`report --check README.md CHANGELOG.md`) exited 0. After `cargo test
  --workspace`, `cargo run --profile test -p gstore-bench` compiled
  gstore-bench and needed a second gstore unit (features resolved for the
  package alone); the step now runs `target/debug/gstore-bench`, built by
  the test step (0 compiles).

## Requests/Not yet

- No edits outside the package's files. `.github/workflows/ci.yml`: only the
  test job's report-check step (infra#6, infra#13) changed.
- infra#4 is fixed to the extent the plan asks (the geomean limit is never
  inside the baseline's own spread; bless refuses noisy or busy machines);
  its other suggestion, blessing the smoke baseline from CI artifacts of the
  runner class that gates it, is not done: there are no GitHub-runner smoke
  results in the repository, and re-blessing on the shared 4-vCPU machine
  while other packages build would record exactly the noisy baseline the new
  rules refuse. The current `bench/baselines/smoke.json` passes every new
  baseline check and is kept.
- infra#18 residual (left as is): the staleness check takes the union of
  the files every gstore library unit in the target directory was compiled
  from, so in a target directory shared by builds with different features
  (say with and without `--features rocksdb`), editing a file only the
  other feature set compiles makes a current `gbuild` look stale; cargo's
  dep-info does not say which library unit a `gbuild` linked (the
  uplifted `target/debug/gbuild.d` is not written by `cargo test`). The
  test then fails naming that file; touching `src/lib.rs` and rebuilding
  clears it. It never misses an edit of a source file the current
  `gbuild` is compiled from (a dependency change in `Cargo.lock` alone is
  not seen: rustc's dep-info lists no manifest), and CI and the campaign's
  separate RocksDB target directory do not mix feature sets.
- The closing step's CI run must confirm on GitHub Actions that the smoke job
  passes with the new gates on a runner (the 2× canary relies on the LUBM
  geomean, about 1.75 against a limit of at most 1.6) and that the weekly
  reference job's three invocations and merge work there (simulated locally
  with the micro profile and the committed results).
