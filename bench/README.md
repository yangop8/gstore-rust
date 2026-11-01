# gstore-bench

The benchmark harness of gstore-rust: a deterministic LUBM-like data
generator, a runner that measures every storage layout through the real
binaries and the public API, a result format with checked-in baselines, and
the gates CI applies to every change.

```text
cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench
target/release/gstore-bench run --profile smoke            # univ 1, snapshot + disk
target/release/gstore-bench ci                              # smoke, gated against bench/baselines/smoke.json
target/release/gstore-bench run --profile reference --scale 8 --out r.json
target/release/gstore-bench report r.json --markdown --compare bench/results/pre-phase6-lubm8.json
```

Run it from the workspace root: the defaults (`target/release` for `gbuild`,
`bench/baselines/smoke.json`, `bench/thresholds.json`, `target/gstore-bench/`
for data, databases and results) are relative paths.

## Commands

| command | what it does | exit code |
|---|---|---|
| `gen [--workload lubm\|tiny] [--scale N] [--seed S] [--departments D] [--chunk] [--out FILE\|-]` | write a data set (or its update chunk) as N-Triples; prints lines, unique triples, bytes, FNV-1a hash and triples/s | 0 |
| `run [--profile P] [RUN OPTIONS] [--out FILE] [--note TEXT]` | measure a profile, write the result JSON, print a Markdown report | 0, or 1 when the result fails its own exact checks |
| `check [--baseline FILE] [--from RESULT] [--thresholds FILE] [--self-test-slowdown X] [--summary FILE] [RUN OPTIONS] [--out FILE]` | measure what the baseline measured (or take `--from` RESULT) and apply the gates; failing timing/memory gates re-run only those phases (2 retries, minimum of the medians) | 0 pass, 1 fail |
| `ci [same options]` | `check` with the smoke baseline, the summary appended to `$GITHUB_STEP_SUMMARY` | 0 pass, 1 fail |
| `compare BASELINE RESULT [--thresholds FILE] [--self-test-slowdown X] [--markdown] [--summary FILE]` | gate two existing files | 0 pass, 1 fail |
| `report RESULT [--markdown] [--compare BASELINE]` | Markdown tables (and ratios plus gate table against BASELINE) | 0 |
| `bless --to FILE [--from RESULT] [--provisional] [--note TEXT] [RUN OPTIONS]` | turn a result (or a fresh run) into a baseline; refuses one that fails its own exact checks | 0, or 1 when refused |
| `merge --out FILE RESULT RESULT...` | join results of the same workload measured by different binaries (e.g. RocksDB from a `--features rocksdb` build) | 0, or 1 on a failed self-check |
| `calibrate [--quick]` | print this machine's calibration | 0 |

Every command exits **2** on a usage error, a crashed or timed-out phase, or
an unreadable file — never 0.

Run options: `--profile micro|smoke|reference`, `--workload`, `--scale`,
`--seed`, `--departments`, `--backends snapshot,disk,rocksdb`, `--reps N`,
`--open-reps N`, `--singles N`, `--no-update`, `--no-conc`,
`--conc-clients 1,8`, `--conc-secs S`, `--conc-queries q1,q4`,
`--conc-endpoints sparql,api`, `--timeout-s N`, `--bin-dir DIR`,
`--data-dir DIR` (default `$GSTORE_BENCH_CACHE` or
`target/gstore-bench/data`), `--work-dir DIR` (default `$GSTORE_BENCH_WORK`
or `target/gstore-bench/work`), `--keep-db`, `--env K=V` (repeatable; set
for every phase, e.g. `--env GSTORE_DISK_QUERY_MODE=stream`),
`--gbuild-arg X` (repeatable; passed to `gbuild` before the database
name), `--drop-caches` (drop the page cache before the first open; needs
root), `--no-calibrate`, `--verify-data` (re-hash a cached data file),
`--quiet`.

## Profiles

| profile | workload | backends | reps | conc | use |
|---|---|---|---|---|---|
| `micro` | LUBM-like univ 1, one department (7.5K triples) | snapshot | 2 | 1, 2 clients × 0.25 s | the harness's own tests |
| `smoke` | LUBM-like univ 1 (140K triples) | snapshot, disk | 3 | 1, 2 clients × 1 s (report-only) | CI gate |
| `reference` | LUBM-like univ 8 (`--scale 38` for 5.2M triples) | snapshot, disk, rocksdb if compiled | 5 | 1, 8 clients × 5 s | Phase 6 measurements |

## What a run measures

For each backend, every step in a **fresh child process** (`gstore-bench
__child`: one JSON request on standard input, exactly one
`GSTORE_BENCH_RESULT {json}` line on standard output, a per-phase timeout;
a crash or a timeout fails the run with exit code 2):

1. **build** — the real CLI (`src/runner/backends.rs` is the only place that
   knows the command lines): `gbuild lubm <data>`, `gbuild --disk …`,
   `gbuild --rocksdb …`, taken from `--bin-dir` (default `target/release`).
   Wall time, `gbuild`'s peak RSS (`getrusage(RUSAGE_CHILDREN)`) and the
   database directory's apparent size (without `gbuild`'s parse log) and
   the size of each top-level entry.
2. **open** — `Database::open` once (cold when `--drop-caches` could drop the
   page cache), then `--open-reps` more times, each in a fresh process; the
   warm figure is their median. RSS after the first open; the triple,
   entity, literal and predicate counts. `triple_num` must equal the
   generator's unique triples.
3. **query** — open, query cache off (`set_query_cache_size(0)`), then every
   query once plus `reps` warm runs, each text prefixed with a
   `# gstore-bench run k` comment. First and median time, rows and
   `rows_fnv64` (a multiset hash of the rows in which unbound ≠ `""`); the
   rows must be identical in every run and on every backend. The process's
   RSS and high-water mark afterwards.
4. **conc** — the single-database `Server` (`GET /sparql`, TSV) and the
   multi-database `ApiServer` (`GET /api?operation=query`, JSON, after
   `operation=load`), in-process on an ephemeral port, query cache off;
   closed-loop keep-alive clients per client count; qps, mean and p95
   latency. Every response body of one endpoint and query must be identical
   (`/api` bodies without `QueryTime` and `ThreadId`).
5. **update** (last, it writes) — `singles` single-triple `INSERT DATA`
   statements and their `DELETE DATA` (median, p95; each must change exactly
   one triple); the update chunk (a disjoint department) inserted with one
   `INSERT DATA` (`triple_num` must grow by exactly its unique triples) and
   deleted again; every query re-run (answers must be unchanged); a
   checkpoint; a reopen (the triple count must match). Dictionary growth is
   reported only.

The database is deleted after each backend (`--keep-db` keeps it); phase
logs are kept when a run fails. Calibration (`calib/1`: sorting 2 Mi `u64`,
a 1 Mi-key `HashMap`, formatting 500 Ki IRIs, each the minimum of five, plus
an fsync probe) runs first, also in a child.

## Workloads

**lubm-like** (`src/lubm.rs`, version `lubm-like/1`) ports the design-stage
`gen_lubm.py` prototype: UBA cardinalities, the univ-bench IRIs of
`testdata/lubm/lubm.nt`, the duplicate `rdf:type ub:University` lines UBA
writes, and guarantees that all 14 queries of `testdata/lubm/lubm_q*.rq`
(included verbatim, their text hashes pinned) are non-empty at every scale.
One SplitMix64 stream per university and per department; output through a
1 MiB buffer at about 2M triples/s. Sizes at seed 0:

| scale | departments | lines | unique triples |
|---|---|---|---|
| univ 1, 1 department (`micro`) | 1 | 7,521 | 7,491 |
| univ 1 | 21 | 143,930 | 139,905 |
| univ 8 | 156 | 1,064,650 | 1,029,275 |
| univ 38 | 779 | 5,363,690 | 5,181,103 |

The update chunk is Department0 of `University<univ>` without its
University typing lines, so none of its triples is in the data set.
`tiny` is a 285-triple toy graph with five queries for the harness's tests.

Generated files are cached in the data directory as `<stem>.nt` with a
`<stem>.nt.json` sidecar (generator version, parameters, size, hash); a
missing, stale or mismatching sidecar regenerates the file.

## Results and baselines

`gstore-bench/1` JSON (`src/schema.rs`): environment (commit, host, CPU,
memory, load), calibration, workload (generator version, data hash, query
text hashes, chunk), and per backend the build, open, query, query memory,
update and concurrency figures, plus the binaries and options that measured
it (`config`). Times are ms, memory MiB, sizes bytes, hashes 16 hex digits.
A baseline is the same document with `"kind": "baseline"`;
`"provisional": true` marks one recorded on a busy machine.

* `bench/baselines/smoke.json` — the CI baseline (re-bless it when a change
  makes things ≥ 1.5× faster, or after an intended format change).
* `bench/results/pre-phase6-lubm8.json`, `pre-phase6-lubm38.json` — the
  pre-Phase-6 reference every Phase 6 acceptance ratio is measured against.

## Gates (`bench/thresholds.json`, `gstore-bench-gates/1`)

* **Exact** (never retried): generator version and data hash, query text
  hashes (also against the pinned values), `triple_num == unique_triples`,
  rows and row hash per query equal to the baseline and across backends,
  stable across runs and non-empty, the update round trip, identical
  concurrent bodies, every baseline backend and phase present.
* **Timing**, one-sided and calibration-normalized (measured times are
  divided by the calibration ratio, clamped to [0.5, 2]): per query
  `(m + 2 ms) / (b + 2 ms) > 3.0` fails; the per-backend geomean of the raw
  warm medians `> 1.5` fails (it covers the queries whose baseline median is
  ≥ 1 ms, all of them when fewer than three are: sub-millisecond jitter
  would dominate it, and the per-query gate covers those); build `> 2.0` (`> 2.5` for disk, 100 ms floor);
  warm open `> 2.5` (20 ms floor).
* **Memory**: `(m + 32 MiB) / (b + 32 MiB) > 1.5` fails (build peak, after
  open, query high-water mark). **Size**: database bytes `> 1.25×` fails.
* Concurrency throughput and update times are report-only.

A failing timing or memory gate re-runs only the failing phases of that
backend, up to `retries` (2) times, keeping the minimum of the medians.
`--self-test-slowdown X` multiplies every measured time by X: CI checks that
3× fails (exit 1). A uniform 1.3× slowdown passes, 1.6× fails, one query of
≥ 20 ms slowed 4× fails, and a row-hash change fails without a retry
(`tests/gates.rs`).

## CI (`.github/workflows/bench.yml`)

* `smoke` (push, PR): release build, `gstore-bench ci`, the canary, results
  and phase logs as an artifact.
* `rocksdb-correctness` (push, PR): `cargo test -p gstore-bench --features
  rocksdb` after building `gbuild` with RocksDB; the RocksDB build cache is
  shared with ci.yml's rocksdb job.
* `reference` (manual with a `scale` input, weekly): report-only run of the
  reference profile with RocksDB, compared with the pre-Phase-6 file.

## Recording a reference (for example the pre-Phase-6 one)

Snapshot and disk come from a default build, RocksDB from a
`--features rocksdb` build in its own target directory; `merge` joins them:

```text
cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench
CARGO_TARGET_DIR=target-rocks cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench \
    --features gstore/rocksdb,gstore-bench/rocksdb
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
for s in 8 38; do
  target/release/gstore-bench run --profile reference --scale $s --backends snapshot,disk \
      --out target/gstore-bench/ref$s-sd.json
  target-rocks/release/gstore-bench run --profile reference --scale $s --backends rocksdb \
      --bin-dir target-rocks/release --out target/gstore-bench/ref$s-r.json
  target/release/gstore-bench merge --out bench/results/pre-phase6-lubm$s.json \
      target/gstore-bench/ref$s-sd.json target/gstore-bench/ref$s-r.json
done
```

## Tests

`cargo test -p gstore-bench` (about 15 s in a debug build): golden values of
the generator stream, the hashes and the univ-1 data; all 14 queries
non-empty at univ 1 and the micro row counts; the chunk's disjointness; the
gates on synthetic results; and the binary end to end with the `micro`
profile (bless → check, the canary, an injected crash and timeout exiting 2,
the data cache, merging, every backend answering alike). The end-to-end
tests use the `gbuild` next to the test binary when it exists (`cargo test
--workspace` builds it) and otherwise the library build, and say so;
`GSTORE_BENCH_TEST_BUILDER=cli|lib` forces one. The LUBM micro run on the
disk layout is `#[ignore]` (slow in a debug build).
