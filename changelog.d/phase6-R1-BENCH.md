# R1-BENCH — gstore-bench harness, CI bench gate, rustfmt config

## Changelog

- **gstore-bench**, a new workspace member `bench/` (package `gstore-bench`,
  `publish = false`; dependencies `gstore` (path), `serde`, `serde_json` —
  no new crates; `Cargo.lock` gains only the member's entry; feature
  `rocksdb = ["gstore/rocksdb"]`). The Dockerfile copies `bench/` so cargo can
  load the workspace (`cargo metadata --locked` checked on the COPY set); the
  image is unchanged.
- **Commands:** `gen`, `run`, `check`, `ci`, `compare`, `report --markdown
  [--compare]`, `bless`, `merge`, `calibrate`. Exit codes: 0 pass, 1 a gate
  failed, 2 usage error / crashed or timed-out phase (never a pass).
- **LUBM-like generator `lubm-like/1`** (`bench/src/lubm.rs`), a Rust port of
  the design-stage `gen_lubm.py` prototype, which is not part of the
  repository: UBA cardinalities, the univ-bench IRIs, the duplicate
  `rdf:type ub:University` lines, guaranteed non-empty answers for all 14
  `testdata/lubm` queries (Q1/Q2/Q7/Q9/Q10 forced), one SplitMix64 stream per
  university and department (Lemire bounded integers), output through a
  1 MiB buffer at ~1.6–1.9M triples/s. The university stream tag was chosen
  so that the department counts reproduce the prototype's scale points
  (University0 21 departments; univ 8 156, univ 38 779). Golden hash for
  univ 1 / seed 0 (`b45d2e4d786f1a33`); the 14 query texts are included
  verbatim and their FNV-1a hashes pinned. An update chunk (Department0 of
  `University<univ>` without University typings) is disjoint from the data.
  A 285-triple `tiny` workload serves the harness's tests.
- **Runner:** every phase runs in a fresh `gstore-bench __child` (JSON
  request on stdin, exactly one `GSTORE_BENCH_RESULT {json}` line, per-phase
  timeout, process-group kill). The build phase runs the real `gbuild`
  (`gbuild`, `gbuild --disk`, `gbuild --rocksdb` from `--bin-dir`, default
  `target/release`; the command lines live only in
  `bench/src/runner/backends.rs`) and records wall time, gbuild's peak RSS
  (`getrusage(RUSAGE_CHILDREN)`) and the database's bytes. Open (first —
  cold with `--drop-caches` — and `--open-reps` warm opens, each in a fresh
  process), query (cache off, `# gstore-bench run k` prefix, first + median
  of `reps`, rows and an order-independent row hash that must be stable and
  equal across backends), concurrency (in-process `Server` `GET /sparql` TSV
  and `ApiServer` `GET /api?operation=query`, closed-loop keep-alive
  clients, identical bodies required) and update (single INSERT/DELETE DATA
  median/p95, the chunk inserted and deleted with exact `triple_num`
  deltas, answers re-checked, checkpoint, reopen) use the public API.
  `--env K=V` and `--gbuild-arg X` pass through to every phase / gbuild.
  Generated data is cached with a sidecar (`$GSTORE_BENCH_CACHE`);
  databases are deleted after each backend; phase logs are kept on failure.
- **Formats:** results and baselines `gstore-bench/1` (`bench/src/schema.rs`),
  calibration `calib/1`, gates `gstore-bench-gates/1`
  (`bench/thresholds.json`): exact gates (generator version and data hash,
  query texts, `triple_num == unique_triples`, rows and row hashes against
  the baseline and across backends, update round trip, concurrent body
  equality) and one-sided, calibration-normalized timing gates (per query
  `(m+2 ms)/(b+2 ms) > 3`, geomean > 1.5 over the queries of ≥ 1 ms, build
  ratio over 2.0 (2.5 for disk), warm open over 2.5), memory
  `(m+32 MiB)/(b+32 MiB) > 1.5`, bytes over 1.25×; two targeted retries
  (minimum of the medians); `check`/`compare --backends` gate a subset;
  `--self-test-slowdown X` canary. Profiles `micro` (tests), `smoke` (CI:
  univ 1, snapshot + disk) and `reference` (univ 8/38, every compiled
  backend, reps 5, 1 and 8 clients).
- **Baseline:** `bench/baselines/smoke.json`, blessed **provisional** (recorded
  on a busy shared machine; the measure stage re-blesses it).
- **CI:** new `.github/workflows/bench.yml` — `smoke` (release build,
  `gstore-bench ci` into `$GITHUB_STEP_SUMMARY`, the 3× canary must exit 1,
  results and logs uploaded), `rocksdb-correctness` (`cargo test -p
  gstore-bench --features rocksdb` after building gbuild with RocksDB; rust
  cache `shared-key: rocksdb`, shared with ci.yml's rocksdb job), `reference`
  (dispatch with a scale input, weekly; report-only); `paths-ignore` for
  docs, cancel-in-progress. `ci.yml` gains a `fmt` job
  (`cargo fmt --all -- --check`) and the shared RocksDB cache key.
- **rustfmt:** `rustfmt.toml` (`style_edition = "2021"`) and
  `#[rustfmt::skip]` on `SHA512_K` in `src/query/hash.rs`. No other existing
  file was reformatted; the new `bench/` files are rustfmt-clean.
- `changelog.d/README.md` describes the per-package fragment convention (C10).

## GAP_PLAN status line

阶段 6:进行中 — R1-BENCH:基准工具 gstore-bench(LUBM-like 生成器、各布局的
build/open/query/update/并发阶段、闸门与 CI 冒烟)已就绪;pre-Phase-6 参考结果
`bench/results/pre-phase6-lubm{8,38}.json` 由安静测量阶段记录。

## Docs notes

- `README.md`: a short "Benchmarks" paragraph — `cargo build --release -p
  gstore -p gstore-bench --bin gbuild --bin gstore-bench`, then
  `target/release/gstore-bench run --profile smoke` / `ci`; point to
  `bench/README.md`.
- `docs/BENCHMARKS.md` (new, R4-CLOSURE): take the methodology from
  `bench/README.md` (phases, child processes, calibration, gates, profiles)
  and the tables from `bench/results/pre-phase6-lubm{8,38}.json`
  (`gstore-bench report … --markdown`). State that the LUBM-like data is not
  byte-identical to the prototype's Python data (sizes: univ 1 139,905 /
  univ 8 1,029,275 / univ 38 5,181,103 unique triples) and that every Phase 6
  ratio is taken against the pre-Phase-6 JSON.
- `docs/GAP_PLAN.md` §8: the status line above; §9/Phase 6 gates: name
  `gstore-bench` as the measuring tool.
- `docs/DEPLOY.md` (Docker section): the build context now includes `bench/`
  (manifest only; the image's binaries are unchanged).
- Contributor notes (README or docs): CI runs `cargo fmt --all -- --check`;
  the one-time formatting commit is listed in `.git-blame-ignore-revs`;
  `changelog.d/` fragments during Phase 6.
- `CHANGELOG.md`: fold the Changelog section above.

## Measured numbers

Implementer run, noisy (other agents compiling): a shared 4-vCPU / 16 GB VM
(`Intel Xeon @ 2.10GHz`), load average 1.2–2.3 during the runs, every heavy
run under `flock /tmp/gstore-bench.lock`. Release builds. Nothing at univ 38
was run here (left to the measure stage).

**Generator** (`gstore-bench gen`, release, writing to /tmp):

| scale | lines | unique triples | target ±5% | rate |
|---|---|---|---|---|
| univ 1 | 143,930 | 139,905 | 143K (−2.4%) | 1.67M lines/s |
| univ 8 | 1,064,650 | 1,029,275 | 1.05M (−2.0%) | 1.64M lines/s |
| univ 38 | 5,363,690 | 5,181,103 | 5.19M (−0.2%) | 1.90M lines/s |

All 14 queries are non-empty at univ 1, at the micro size (1 department)
and at univ 8. Row counts at univ 8: Q1 3, Q2 25, Q3 10, Q4 9, Q5 576,
Q6 80,056, Q7 23, Q8 10,778, Q9 1,081, Q10 3, Q11 325, Q12 21, Q13 85,670,
Q14 60,576 (prototype data: 8/28/9/10/481/82,111/34/11,137/1,050/8/333/21/
87,795/62,096).

**Smoke profile, univ 1** (`gstore-bench run --profile smoke`, whole run
54 s): snapshot build 1.48 s, gbuild peak RSS 41.6 MiB, 11.8 MB; open
26.5 ms first / 29.0 ms warm; disk build 28.6 s, 77.7 MiB, 24.0 MB; open
113 / 108 ms; every query equal on both layouts; update round trip ok
(single insert median 0.11 ms snapshot, 1.67 ms disk). `gstore-bench ci`
against the provisional baseline right after: PASS (193 gates; snapshot
query geomean 1.27, disk 0.97 — machine noise). The real smoke baseline
gated against itself: `check --from <baseline> --self-test-slowdown 1.3`
exits 0, `1.6` exits 1, `3` (the canary) exits 1.

**univ 8, snapshot, A/B in one session** (`run --profile reference --scale 8
--backends snapshot`, ~72 s per run): *base* = 90b92d6 with this harness
copied in and built in a throwaway worktree; *branch* = this branch (same
gstore code apart from a `#[rustfmt::skip]`).

| | base | branch |
|---|---|---|
| build (gbuild) | 14.27 s, 154.2 MiB peak | 14.30 s, 154.3 MiB peak |
| database bytes (then with the parse log) | 85,480,278 | 85,480,278 (identical) |
| open first / warm | 215.6 / 221.7 ms | 233.0 / 223.6 ms |
| RSS after open / query HWM | 126.7 / 152.0 MiB | 126.7 / 152.5 MiB |
| Q1–Q14 geomean ratio branch/base | — | 1.071 (all 14); 1.00 (the 10 of ≥ 1 ms, the gate's set) |
| checkpoint after the update phase | 10.2 s | 8.2 s (VS-tree rebuilt after deletes) |
| Q4 qps 1 → 8 clients, /sparql | 91.8 → 80.7 (0.88×) | 87.6 → 80.6 (0.92×) |
| Q4 qps 1 → 8 clients, /api | 64.4 → 60.5 | 91.4 → 80.6 |

| query | base median ms | branch median ms | prototype (ad-hoc) ms | base vs prototype |
|---|---|---|---|---|
| Q2 | 7.86 | 7.98 | 9.6 | −18% |
| Q4 | 9.78 | 10.29 | 10.7 | −9% |
| Q5 | 33.41 | 37.91 | 45.5 | −27% |
| Q6 | 29.60 | 30.89 | 31.5 | −6% |
| Q7 | 49.20 | 52.70 | 55.7 | −12% |
| Q8 | 53.69 | 55.83 | 54.3 | −1% |
| Q9 | 68.87 | 82.38 | 77.0 | −11% |
| Q10 | 37.47 | 40.19 | 43.2 | −13% |
| Q13 | 33.81 | 41.36 | 35.6 | −5% |
| Q14 | 30.33 | 28.30 | 26.6 | +14% |

(Q1 0.29/0.31, Q3 0.44/0.47, Q11 0.38/0.39, Q12 0.17/0.18 ms.)

**Whole pipeline at univ 1, every layout** (`run --profile reference
--scale 1`): snapshot and disk from the default release build (121 s),
rocksdb from the `--features rocksdb` build in `target-rocks` with
`--drop-caches` (61 s), joined by `merge` (the merged self-check — rows equal
on all three layouts, update round trips, identical concurrent bodies —
passes), `report --markdown`, `compare` against itself exit 0.

| | build | peak RSS | bytes | open first / warm | RSS after open |
|---|---|---|---|---|---|
| snapshot | 1.55 s | 41.4 MiB | 11,822,479 | 27 / 26 ms | 23.3 MiB |
| disk | 30.57 s | 77.6 MiB | 24,006,662 | 155 / 107 ms | 30.5 MiB |
| rocksdb | 6.05 s | 90.6 MiB | 6,286,368 | 1171 (cold) / 1172 ms | 32.7 MiB |

Row counts at univ 1 (all layouts): 3, 4, 10, 9, 576, 10,778, 23, 10,778,
157, 3, 325, 21, 11,540, 8,049.

**Provisional smoke baseline** (`bench/baselines/smoke.json`): blessed from a
final `run --profile smoke` of this branch (load 0.9–2.5); snapshot build
1.60 s, disk build 29.9 s. The earlier `ci` result passes against it
(geomean 1.01 snapshot, 0.81 disk); the canary on it exits 1.

**Tests:** `cargo test -p gstore-bench`: 42 tests (18 unit, 8 golden, 10
gates, 6 end to end of which 1 is `#[ignore]`d), ~9 s of test time in a
debug build plus compilation. With `--features rocksdb` (release,
`target-rocks`, gbuild built with RocksDB) all 42 pass, the ignored one
included. Workspace: `cargo build --workspace --all-targets`, `cargo clippy
--workspace --all-targets -- -D warnings` clean, `cargo test --workspace`
2001 passed, 0 failed.

**Acceptance (R1-BENCH), as of this branch:**
- sizes univ 1/8/38 within ±5% — met (−2.4/−2.0/−0.2%); all 14 queries
  non-empty — met at univ 1 and 8 (univ 38 by the measure stage);
  generation ≥ 1M triples/s — met (1.6–1.9M);
- fidelity, snapshot univ 8 — met on this noisy run (−27% … +16%; base
  and branch); RocksDB open and disk Q5/Q9/Q13 at univ 8 — measure stage;
- repeatability — noisy here (1.071 over all 14 queries, 1.00 over the ten
  ≥ 1 ms queries; byte counts identical) — measure stage on an idle
  machine;
- gates (1.3× passes, 1.6× fails, a ≥ 20 ms query ×4 fails, a row-hash
  change fails without retry, canary exits 1) — met (`tests/gates.rs`, and
  on the real smoke baseline);
- CI smoke ≤ 15 min warm — expected (release build of gstore + harness
  ≈ 3–4 min, smoke run ≈ 1 min) but not observed on GitHub from here;
- `pre-phase6-lubm{8,38}.json` committed — left to the measure stage (by
  instruction); default `cargo test -p gstore-bench` ≤ 60 s — met (~9 s of
  test time).

## Requests/Not yet

- **Measure stage (not done here, by instruction):** record
  `bench/results/pre-phase6-lubm8.json` and `pre-phase6-lubm38.json`
  (snapshot, disk from the default build; rocksdb from the `--features
  rocksdb` build; joined with `gstore-bench merge`), re-bless
  `bench/baselines/smoke.json` on an idle machine (drops `provisional`), and
  check fidelity (snapshot Q2/Q4–Q10/Q13/Q14 within ±30% of the ad-hoc
  numbers, RocksDB open at univ 8 9.7 s ± 30%, disk Q5/Q9/Q13 within ±30% of
  139/337/207 ms), repeatability (two univ 8 snapshot runs, geomean in
  [0.93, 1.07]; identical byte counts) and the CI smoke duration. The exact
  commands are in `bench/README.md` ("Recording a reference").
- **Integrator:** the new `fmt` job fails until the planned one-time
  `cargo fmt --all` commit (and `.git-blame-ignore-revs` with its SHA) lands
  after the R1 merge.
- **R3-SRV / R4-SNAP:** `bench/src/runner/conc.rs` uses `Server::bind(db,
  addr)`, `Server::shutdown_handle`/`serve_forever`, `ApiServer::bind(root,
  password, addr)`, `with_query_settings`, `shutdown_handle`/`serve_forever`,
  and `QuerySettings { query_cache_bytes, .. }`. If those signatures change,
  `conc.rs` must follow; `bench/**` belongs to R4-CLOSURE after R1, so such a
  change needs an ownership exception or keeps the old constructors.
- **R3-STREAM:** measure Stream vs Materialize with `--env
  GSTORE_DISK_QUERY_MODE=stream|memory`. Results key runs by backend name,
  so the two modes go into separate result files (`merge` refuses a backend
  twice); a run label would be a small later addition (R4-CLOSURE).
- **Not yet:** the `/api` concurrency point logs every query to the API
  query log of its scratch root (the real server path, kept on purpose); no
  WatDiv workload (R4-CLOSURE).
