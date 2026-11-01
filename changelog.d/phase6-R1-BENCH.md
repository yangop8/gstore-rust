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
  of `reps` in each of `query_procs` fresh processes — 5 in `reference`, 3
  in `smoke` — keeping per query the minimum of the per-process medians,
  because hash seeds and heap layout move some query times by up to ~1.5×
  between processes of the pre-Phase-6 engine; rows and an
  order-independent row hash that must be stable across runs and
  processes and equal across backends), concurrency (in-process `Server` `GET /sparql` TSV
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
- **Baseline and reference:** `bench/baselines/smoke.json` (the CI gate's
  baseline) and `bench/results/pre-phase6-lubm8.json` /
  `pre-phase6-lubm38.json` (snapshot, disk and RocksDB on the pre-Phase-6
  code; every Phase 6 ratio is taken against them), all recorded by the
  quiet measure stage on the idle reference VM.
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
`bench/results/pre-phase6-lubm{8,38}.json`(snapshot/disk/RocksDB)已在空闲的参考机上记录。

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

### Quiet measure stage — the pre-Phase-6 reference

Recorded on the reference VM (4 vCPU `Intel Xeon @ 2.10GHz`, 16 GB, Linux),
idle apart from this stage (load average about 1, the measured process
itself); every heavy command under `flock /tmp/gstore-bench.lock`, nothing
built while measuring. Commit `e859f7e` = base `90b92d6` + this harness:
the gstore code is the pre-Phase-6 base (one `#[rustfmt::skip]` apart).
Release builds; snapshot and disk from the default build, RocksDB from the
`--features rocksdb` build in `target-rocks`; `run --profile reference
--drop-caches --verify-data` (first open cold, cached data re-hashed),
joined with `merge` (cross-backend self-check passes; each file passes
`compare` against itself, 281 gates). Reference profile: reps 5 in each of
5 query processes, 3 warm opens, 100 single updates, 1 and 8 clients × 5 s.
Wall time: univ 8 7.7 min (snapshot + disk) + 4.4 min (RocksDB); univ 38
39.4 min (disk build 27.1 min) + 23.7 min (RocksDB).

**Generator** (`gstore-bench gen --scale N --out /tmp/…`, release):

| scale | lines | unique triples | target ±5% | data fnv64 | time | rate (lines / unique triples per s) |
|---|---:|---:|---|---|---:|---:|
| univ 1 | 143,930 | 139,905 | 143K (−2.2%) | `b45d2e4d786f1a33` (golden) | 0.063 s | 2.28M / 2.22M |
| univ 8 | 1,064,650 | 1,029,275 | 1.05M (−2.0%) | `ed44da2e7b3bbb98` | 0.494 s | 2.15M / 2.08M |
| univ 38 | 5,363,690 | 5,181,103 | 5.19M (−0.2%) | `fcd6493725f3c66a` | 2.518 s | 2.13M / 2.06M |

The cached univ 1/8 files in `/tmp/gstore-bench-cache` matched their
sidecars and these hashes (`--verify-data`); univ 38 was generated there by
the first univ 38 run (3.5 s) with the same hash. Every query is non-empty
at univ 1 (smoke: snapshot, disk), 8 and 38 (snapshot, disk, RocksDB), with
identical rows and row hashes on every layout.

**Reference results** (`bench/results/pre-phase6-lubm{8,38}.json`; query
times are warm medians, the minimum over the 5 query processes):

**univ 8** (`pre-phase6-lubm8.json`, commit `e859f7e`, calibration 67.4 ms):

| backend | build s | gbuild peak MiB | database bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 15.32 | 154.2 | 85,480,180 | 336 / 292 | 125.8 | 152.5 | 7.91 |
| disk | 263.74 | 164.7 | 175,005,702 | 2240 / 1447 | 129.7 | 167.5 | 18.99 |
| rocksdb | 53.93 | 285.5 | 41,162,410 | 9959 / 9989 | 145.8 | 184.9 | 43.27 |

| query | rows | snapshot ms | disk ms | rocksdb ms |
|---|---:|---:|---:|---:|
| Q1 | 3 | 0.31 | 0.42 | 9.47 |
| Q2 | 25 | 8.04 | 38.4 | 87.1 |
| Q3 | 10 | 0.44 | 0.48 | 29.3 |
| Q4 | 9 | 9.50 | 9.73 | 123.3 |
| Q5 | 576 | 37.3 | 115.5 | 69.9 |
| Q6 | 80,056 | 31.0 | 97.3 | 68.2 |
| Q7 | 23 | 54.0 | 216.5 | 131.8 |
| Q8 | 10,778 | 51.0 | 164.9 | 162.1 |
| Q9 | 1,081 | 70.4 | 266.3 | 241.4 |
| Q10 | 3 | 36.1 | 114.0 | 69.6 |
| Q11 | 325 | 0.39 | 0.54 | 2.71 |
| Q12 | 21 | 0.16 | 0.15 | 1.52 |
| Q13 | 85,670 | 35.2 | 172.1 | 76.8 |
| Q14 | 60,576 | 21.1 | 60.2 | 50.0 |

| backend | single INSERT median ms | chunk insert / delete ms | checkpoint ms | reopen ms | Q4 qps /sparql 1 → 8 clients | Q4 qps /api 1 → 8 |
|---|---:|---:|---:|---:|---:|---:|
| snapshot | 0.86 | 29 / 26 | 8436 | 187 | 81.0 → 76.3 | 85.6 → 76.5 |
| disk | 1.74 | 1679 / 1419 | 0 | 10006 | 84.9 → 79.3 | 85.9 → 76.4 |
| rocksdb | 1.54 | 172 / 107 | 22 | 10121 | 7.2 → 6.3 | 7.4 → 6.7 |

**univ 38** (`pre-phase6-lubm38.json`, commit `e859f7e`, calibration 62.6 ms):

| backend | build s | gbuild peak MiB | database bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 76.82 | 741.0 | 430,848,180 | 1588 / 1465 | 596.4 | 710.3 | 46.65 |
| disk | 1627.59 | 738.1 | 889,778,182 | 12387 / 7673 | 544.0 | 740.6 | 146.92 |
| rocksdb | 277.76 | 634.2 | 206,253,032 | 60811 / 59389 | 522.0 | 663.3 | 311.50 |

| query | rows | snapshot ms | disk ms | rocksdb ms |
|---|---:|---:|---:|---:|
| Q1 | 3 | 1.30 | 39.1 | 60.7 |
| Q2 | 115 | 50.8 | 189.7 | 729.5 |
| Q3 | 10 | 2.19 | 39.2 | 195.5 |
| Q4 | 9 | 53.6 | 75.2 | 990.0 |
| Q5 | 576 | 333.2 | 621.3 | 537.7 |
| Q6 | 404,399 | 187.6 | 511.4 | 468.3 |
| Q7 | 23 | 403.0 | 1147.7 | 977.6 |
| Q8 | 10,778 | 363.5 | 871.8 | 904.9 |
| Q9 | 5,507 | 529.3 | 1450.6 | 2125.9 |
| Q10 | 3 | 326.6 | 697.1 | 594.9 |
| Q11 | 325 | 1.09 | 1.49 | 16.8 |
| Q12 | 21 | 0.59 | 0.72 | 10.2 |
| Q13 | 432,512 | 223.9 | 906.1 | 511.9 |
| Q14 | 305,503 | 132.1 | 320.3 | 382.7 |

| backend | single INSERT median ms | chunk insert / delete ms | checkpoint ms | reopen ms | Q4 qps /sparql 1 → 8 clients | Q4 qps /api 1 → 8 |
|---|---:|---:|---:|---:|---:|---:|
| snapshot | 5.04 | 49 / 45 | 46004 | 1122 | 17.1 → 13.4 | 17.8 → 15.4 |
| disk | 2.33 | 4830 / 5024 | 0 | 51873 | 12.6 → 11.7 | 11.6 → 11.5 |
| rocksdb | 4.35 | 186 / 149 | 39 | 55702 | 1.2 → 1.2 | 1.2 → 1.2 |

Against the plan's §0 numbers (eaf5320, Python data, 5.19M): snapshot
build 76.8 vs 67.8 s, 431 vs 432 MB, open 1.59 / 1.46 vs 1.53 / 1.36 s,
RSS after open 596 vs 596 MiB; disk build 1628 vs 1522 s, 890 vs 891 MB,
open 12.4 / 7.7 vs 11.7 / 7.7 s, RSS 544 vs 544 MiB; RocksDB build 278 vs
199 s, 206 vs 206 MB, open 60.8 / 59.4 vs 57.1 / 53.2 s; Q1/Q4/Q9/Q14
snapshot 1.3/53.6/529/132 vs 2.8/80.6/495/125 ms, disk 39/75/1451/320 vs
49/71/1544/382, RocksDB 61/990/2126/383 vs 61/1031/2153/356. **gbuild's
peak RSS is no longer 6.77 GiB**: 741 (snapshot) / 738 (disk) / 634
(RocksDB) MiB at univ 38, because this base already has the phase 5
streaming import — G1a's "pre" peak is therefore 738 MiB, not 6.77 GiB.
Q4 univ 38 / univ 8 on snapshot: 53.6 / 9.50 = 5.6× (plan §0: 7.5×).
Read scaling at univ 8, Q4 8 vs 1 clients: snapshot 0.94× (/sparql) and
0.89× (/api), disk 0.93× / 0.89×.

**Fidelity against the ad-hoc baseline** (univ 8; the Rust generator's data
is not the Python prototype's, e.g. Q5 576 vs 481 rows, Q2 25 vs 28):

| query | ad-hoc ms | reference (min of 5 processes) | Δ | median of the 5 process medians | Δ |
|---|---:|---:|---:|---:|---:|
| snapshot Q2 | 9.6 | 8.04 | −16% | 8.77 | −9% |
| snapshot Q4 | 10.7 | 9.50 | −11% | 9.72 | −9% |
| snapshot Q5 | 45.5 | 37.3 | −18% | 39.9 | −12% |
| snapshot Q6 | 31.5 | 31.0 | −2% | 33.6 | +7% |
| snapshot Q7 | 55.7 | 54.0 | −3% | 75.4 | +35% |
| snapshot Q8 | 54.3 | 51.0 | −6% | 53.0 | −2% |
| snapshot Q9 | 77.0 | 70.4 | −9% | 75.6 | −2% |
| snapshot Q10 | 43.2 | 36.1 | −16% | 40.8 | −6% |
| snapshot Q13 | 35.6 | 35.2 | −1% | 37.3 | +5% |
| snapshot Q14 | 26.6 | 21.1 | −21% | 21.8 | −18% |
| disk Q5 | 139 | 115.5 | −17% | 118.3 | −15% |
| disk Q9 | 337 | 266.3 | −21% | 269.8 | −20% |
| disk Q13 | 207 | 172.1 | −17% | 175.6 | −15% |
| RocksDB open (univ 8) | 9.7 s | 9.96 s first (cold) / 9.99 s warm | +3% / +3% | | |

All within ±30% on the reference figures. (Single-process figures from the
first, pre-fix runs: disk Q5/Q9/Q13 110–113 / 266–282 / 166–183 ms,
RocksDB open 9.61 s cold / 9.10 s warm.)

**Repeatability, and a tool fix.** The harness as delivered ran the whole
query phase in one process. Four univ 8 snapshot runs with it gave
geomean ratios between pairs of them (the ten ≥ 1 ms queries) of 0.80,
0.91, 0.92 and 0.99 (all 14: 0.81–0.93) — outside [0.93, 1.07]. Cause: each process of
the pre-Phase-6 engine draws its own hash seeds and heap layout, and some
queries are up to ~1.5× slower in one process than in the next while
varying by a few percent within one. Fourteen fresh query processes on one
univ 8 snapshot database (ASLR on or off alike): Q2 8.3–12.5 ms, Q4
9.8–15.8, Q7 52–80, Q9 73–109, Q13 34–53; the per-process geomean of the
ten ≥ 1 ms queries 31.4–40.9 ms. Fix (`e859f7e`): the query phase runs in
`query_procs` fresh processes (reference 5, smoke 3, micro 2;
`--query-procs N`) and keeps per query the minimum of the per-process
medians (each process's median is kept in `proc_medians_ms`). With it, two
univ 8 snapshot runs: **geomean 1.011** (the ten ≥ 1 ms queries; 1.013 over
all 14; per query 0.94–1.12). Byte counts are identical across runs:
snapshot 85,480,180 in all seven univ 8 builds, disk 175,005,702 in all
three univ 8 builds, univ 1 11,822,479 / 24,006,662 in all four smoke runs.
(R1-CORE's deterministic join hasher may narrow the per-process spread;
the minimum over processes is robust either way.)

**Gates on real files:**
- `compare pre-phase6-lubm8.json` against itself: ×1 and ×1.3 exit 0
  (281 gates); ×1.6 exits 1 (the three geomean gates, 1.60 > 1.50); ×3
  exits 1.
- The same file with snapshot Q9 (70.4 ms) ×4: exit 1, `time.query`
  ratio 3.92 > 3.0 (the only failure).
- The same file with one changed row hash (snapshot Q5): exit 1, exact
  failures (`exact.rows`, `exact.cross_backend` ×2).
- A live `check` against the smoke baseline with that Q5 hash changed:
  exit 1, exact failure, **no retry** (no phase re-run).
- New smoke baseline: `check --from` it with `--self-test-slowdown 1.3`
  exits 0, `1.6` exits 1, `3` (the canary) exits 1; the same on the `ci`
  result.
- `tests/gates.rs` covers the same cases on synthetic results.

**Smoke baseline** (`bench/baselines/smoke.json`, no longer provisional):
blessed from a `run --profile smoke` on the idle VM (whole run 62 s):
snapshot build 1.65 s, 11,822,479 bytes, open 27.8 / 26.8 ms; disk build
32.3 s, 24,006,662 bytes, open 105.5 / 131.9 ms; every query equal on both
layouts, update round trip ok. (The run before it had disk warm opens of
149/228/215 ms; ten more fresh-process opens of a smoke disk database took
107–151 ms, median 112, so the second run was blessed.) `gstore-bench ci`
right after: **PASS, 193 gates** (geomean ratio snapshot 1.04, disk 0.89),
62.7 s.

**CI smoke duration:** not observed on GitHub (nothing pushed). Locally:
release build of gbuild + gstore-bench from an empty target directory
83 s, `gstore-bench ci` 63 s, canary < 1 s — well under 15 min.

**Tests** (after `e859f7e`): `cargo test -p gstore-bench`: 44 tests (20 unit,
8 golden, 10 gates, 6 end to end of which 1 `#[ignore]`d), 8.5 s of test
time in a debug build after compiling; `cargo test --release -p
gstore-bench --features rocksdb -- --include-ignored`: all 44 pass;
`cargo clippy -p gstore-bench --all-targets -- -D warnings` and `cargo fmt
-p gstore-bench -- --check` clean. (Only `bench/**` changed after the
implementer's workspace-wide verification.)

**Acceptance (R1-BENCH):**
- sizes univ 1/8/38 within ±5% — **met** (−2.2/−2.0/−0.2%); all 14 queries
  non-empty at every scale — **met** (univ 1, 8, 38, every layout);
  generation ≥ 1M triples/s — **met** (2.06–2.22M unique triples/s);
- fidelity — **met**: snapshot Q2/Q4–Q10/Q13/Q14 −21% … −1%; RocksDB open
  9.96 s (+3%); disk Q5/Q9/Q13 −17/−21/−17%;
- repeatability — **met** after the multi-process fix (geomean 1.011;
  bytes identical); **not met** by the single-process harness (0.80–0.99);
- gates — **met** (1.3× passes, 1.6× fails, a ≥ 20 ms query ×4 fails, a
  row-hash change fails without retry, canary exits 1);
- CI smoke ≤ 15 min warm — expected (≈ 2.5 min locally), not observed on
  GitHub;
- `pre-phase6-lubm8.json` and `pre-phase6-lubm38.json` committed — **met**;
  default `cargo test -p gstore-bench` ≤ 60 s — **met** (8.5 s).

### Implementer run (noisy machine; superseded by the tables above)

Other agents were compiling (load 1.2–2.3). Univ 8 snapshot A/B of the
base `90b92d6` (harness copied in) against this branch: build 14.27 vs
14.30 s, identical database bytes, open and memory equal, query geomean
1.00 over the ten ≥ 1 ms queries — the branch's gstore code performs as the
base. Univ 1, every layout (`run --profile reference --scale 1`): build
1.55 / 30.57 / 6.05 s, 11,822,479 / 24,006,662 / 6,286,368 bytes, open
27 / 155 / 1171 (cold) ms for snapshot / disk / RocksDB; row counts 3, 4,
10, 9, 576, 10,778, 23, 10,778, 157, 3, 325, 21, 11,540, 8,049 on all
layouts. Its provisional smoke baseline is replaced by the one above.

## Requests/Not yet

- **Every later round (measuring against the pre-Phase-6 JSON):** keep the
  reference profile's 5 query processes (`--query-procs` ≥ 3 at least) —
  a single query process moves the pre-Phase-6 query geomean by about ±10%
  — and note that G1a's "pre" gbuild peak RSS is 738 MiB (disk, univ 38),
  not the 6.77 GiB of the plan's §0 (the base already streams its import).
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
