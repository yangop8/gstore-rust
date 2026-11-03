# gstore-bench

The benchmark harness of gstore-rust: deterministic LUBM-like and
WatDiv-like data generators, a runner that measures every storage layout
through the real binaries and the public API, a result format with
checked-in baselines, the gates CI applies to every change, and reports that
regenerate documentation tables from committed results.

```text
cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench
target/release/gstore-bench run --profile smoke            # LUBM univ 1 + WatDiv-like 1, snapshot + disk
target/release/gstore-bench ci                              # smoke, gated against bench/baselines/smoke.json
target/release/gstore-bench run --profile reference --scale 8 --extra none --out r.json
target/release/gstore-bench run --profile reference --workload watdiv --out w.json   # WatDiv-like scale 10
target/release/gstore-bench report r.json --markdown --compare bench/results/pre-phase6-lubm8.json
target/release/gstore-bench report --check README.md        # are the generated tables current?
```

Run it from the workspace root: the defaults (`target/release` for `gbuild`,
`bench/baselines/smoke.json`, `bench/thresholds.json`, `target/gstore-bench/`
for data, databases and results) are relative paths.

## Commands

| command | what it does | exit code |
|---|---|---|
| `gen [--workload lubm\|watdiv\|tiny] [--scale N] [--seed S] [--departments D] [--shrink K] [--chunk] [--out FILE\|-]` | write a data set (or its update chunk) as N-Triples; prints lines, unique triples, bytes, FNV-1a hash and triples/s | 0 |
| `queries [--workload watdiv] [--scale N] [--seed S] [--shrink K] [--data-dir DIR] [--verify-data]` | print the workload's queries as the runs use them: the WatDiv-like templates bound against the (cached) data, with each binding and its ASK tries | 0 |
| `run [--profile P] [RUN OPTIONS] [--out FILE] [--note TEXT]` | measure a profile, write the result JSON, print a Markdown report | 0, or 1 when the result fails its own exact checks |
| `check [--baseline FILE] [--from RESULT] [--thresholds FILE] [--self-test-slowdown X] [--summary FILE] [RUN OPTIONS] [--out FILE]` | measure what the baseline measured (or take `--from` RESULT) and apply the gates; failing timing/memory gates re-run only those phases (2 retries, minimum of the medians); `--backends` also limits which baseline backends are gated; workloads, runs and queries the baseline lacks are reported as **new**, not gated | 0 pass, 1 fail |
| `ci [same options]` | `check` with the smoke baseline, the summary appended to `$GITHUB_STEP_SUMMARY` | 0 pass, 1 fail |
| `compare BASELINE RESULT [--thresholds FILE] [--self-test-slowdown X] [--backends a,b] [--markdown] [--summary FILE]` | gate two existing files (`--backends`: only those backends or run keys) | 0 pass, 1 fail |
| `report RESULT... [--markdown] [--table KIND] [--compare BASELINE]...` | Markdown: the full report of one result (ratios and the gate table against BASELINE), or with `--table` one compact table over several results (see [Reports](#reports-and-documentation-tables)) | 0 |
| `report --update DOC... \| --check DOC...` | regenerate (`--update`) or verify (`--check`) the tables between `<!-- gstore-bench report ARGS -->` and `<!-- /gstore-bench report -->` in documents | 0, or 1 when `--check` finds a stale table |
| `bless --to FILE [--from RESULT] [--provisional] [--note TEXT] [RUN OPTIONS]` | turn a result (or a fresh run) into a baseline; refuses one that fails its own exact checks | 0, or 1 when refused |
| `merge --out FILE RESULT RESULT...` | join results of the same workloads measured by different binaries or settings (RocksDB from a `--features rocksdb` build; the disk layout in both query modes under two `--label`s); a run key may appear once per workload | 0, or 1 on a failed self-check |
| `calibrate [--quick]` | print this machine's calibration | 0 |

Every command exits **2** on a usage error, a crashed or timed-out phase, or
an unreadable file — never 0.

Run options: `--profile micro|smoke|reference`, `--workload lubm|watdiv|tiny`
(alone: drops the profile's extra workloads), `--scale`, `--seed`,
`--departments` (LUBM), `--shrink K` (WatDiv: divide the scalable classes by
K, for tests), `--extra none|watdiv:10[,…]` (the profile's further workloads;
`watdiv:1/10` also shrinks), `--label L` (every run's key becomes
`backend@L`), `--backends snapshot,disk,rocksdb`, `--reps N`,
`--query-procs N`, `--open-reps N`, `--singles N`, `--no-update`, `--no-conc`
(also drops the mixed phase), `--conc-clients 1,8`, `--conc-secs S`,
`--conc-queries q1,q4`, `--conc-endpoints sparql,api`, `--no-mixed`,
`--mixed-readers N`, `--mixed-write-ms MS`, `--mixed-secs S`,
`--mixed-checkpoint off,on`, `--mixed-endpoints sparql,api`,
`--mixed-query Q`, `--timeout-s N`, `--bin-dir DIR`,
`--data-dir DIR` (default `$GSTORE_BENCH_CACHE` or
`target/gstore-bench/data`), `--work-dir DIR` (default `$GSTORE_BENCH_WORK`
or `target/gstore-bench/work`), `--keep-db`, `--env K=V` (repeatable; set
for every phase, e.g. `--env GSTORE_DISK_QUERY_MODE=stream`),
`--gbuild-arg X` (repeatable; passed to `gbuild` before the database
name), `--drop-caches` (drop the page cache before the first open; needs
root), `--no-calibrate`, `--verify-data` (re-hash a cached data file and
re-run the ASK walk of cached WatDiv bindings), `--quiet`. The concurrency
and mixed-workload query choices apply to the main workload; extra
workloads use their own defaults.

## Profiles

| profile | workloads | backends | reps × query processes | conc | mixed (readers × s per point) | use |
|---|---|---|---|---|---|---|
| `micro` | LUBM-like univ 1, one department (7.5K triples) | snapshot | 2 × 2 | 1, 2 clients × 0.25 s | 2 × 0.25 s, a write every 50 ms | the harness's own tests |
| `smoke` | LUBM-like univ 1 (140K triples) **+ WatDiv-like scale 1** (106K) | snapshot, disk | 3 × 3 | 1, 2 clients × 1 s (report-only) | 2 × 1 s | CI gate |
| `reference` | LUBM-like univ 8 (`--scale 38` for 5.2M triples) **+ WatDiv-like scale 10** (1.06M) | snapshot, disk, rocksdb if compiled | 5 × 5 | 1, 8 clients × 5 s | 8 × 5 s | Phase 6 measurements |

The extra workload is measured after the main one, on the same backends,
into its own section of the result (`extra_workloads`). `--extra none`
skips it (e.g. for a second LUBM scale), `--workload watdiv` measures the
WatDiv-like workload alone at the profile's scale for it.

## What a run measures

Per workload, first the data: generated into the data directory (or taken
from it when its sidecar matches) and, for the WatDiv-like workload, the
query templates bound against it by an `instantiate` child (an in-memory
database and the ASK walk; cached next to the data as
`<stem>.queries.json`, keyed by the generator version, the parameters, the
data hash and the templates). Then for each backend, every step in a
**fresh child process** (`gstore-bench __child`: one JSON request on
standard input, exactly one `GSTORE_BENCH_RESULT {json}` line on standard
output, a per-phase timeout; a crash or a timeout fails the run with exit
code 2):

1. **build** — the real CLI (`src/runner/backends.rs` is the only place that
   knows the command lines): `gbuild lubm <data>`, `gbuild --disk …`,
   `gbuild --rocksdb …`, taken from `--bin-dir` (default `target/release`).
   Wall time, `gbuild`'s peak RSS (`getrusage(RUSAGE_CHILDREN)`), the
   database directory's apparent size and the size of each top-level
   entry. The size leaves out what is not database content and varies
   between identical builds: `gbuild`'s parse log, the disk layout's `LOCK`
   (it holds the builder's pid) and RocksDB's info logs (`rocksdb/LOG*`).
   A bulk build's report (`gbuild --disk` / `--rocksdb`: the phase times,
   the memory budget and its peak, the term map, the threads and the
   spilled runs) is read from `gbuild`'s output into `build.summary`.
2. **open** — `Database::open` once (cold when `--drop-caches` could drop the
   page cache), then `--open-reps` more times, each in a fresh process; the
   warm figure is their median. RSS after the first open; the triple,
   entity, literal and predicate counts. `triple_num` must equal the
   generator's unique triples.
3. **query** — in each of `--query-procs` fresh processes: open, query
   cache off (`set_query_cache_size(0)`), then every query once plus `reps`
   warm runs, each text prefixed with a `# gstore-bench run k` comment.
   Per query the result keeps the **minimum over the processes of the warm
   median** (and of the first run), each process's median
   (`proc_medians_ms`) and every warm run; rows and `rows_fnv64` (a
   multiset hash of the rows in which unbound ≠ `""`) must be identical in
   every run, in every process and on every backend. The largest RSS and
   high-water mark of the processes afterwards. Several processes are
   needed because each draws its own hash seeds and heap layout: on the
   pre-Phase-6 engine some univ 8 queries take up to ~1.5× longer in one
   process than in the next while varying by a few percent within one, so
   a single process's median moved the Q1–Q14 geomean by ±10% between
   runs.
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
6. **mixed** (it writes too) — readers plus a paced writer on the bundled
   servers, per endpoint (`/sparql`, `/api`) and `check_point` setting (off,
   on): `readers` closed-loop keep-alive clients run one query (the
   workload's second concurrency query, LUBM `q4`) for `secs` while one
   writer sends a single-triple `INSERT DATA` (`POST /update`, or `POST
   /api` `operation=query`) every `write_ms` (100 ms). Reader qps, mean and
   p95 latency; writes acknowledged (each must report one changed triple)
   and their latency. Every reader body of a point must hold the same
   answer — compared as a multiset of rows, since a write changes the
   triple counts the planner costs joins with and so the row order. This
   replaces R3-SRV's `examples/srv_mixed_load.rs` (same servers, writer and
   settings) and runs on every layout, disk included.

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
backend, up to `retries` (2) times, keeping the minimum of the medians. The
gates run after the update phase, so a retried open or query phase first
rebuilds the database that phase changed (and a retried open must see the
first attempt's term counts).
`--self-test-slowdown X` multiplies every measured time by X: CI checks that
the run's own result, slowed 3× and checked against itself, fails (exit 1). A uniform 1.3× slowdown passes, 1.6× fails, one query of
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
  flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale $s \
      --backends snapshot,disk --drop-caches --verify-data --out target/gstore-bench/ref$s-sd.json
  flock /tmp/gstore-bench.lock target-rocks/release/gstore-bench run --profile reference --scale $s \
      --backends rocksdb --bin-dir target-rocks/release --drop-caches --verify-data \
      --out target/gstore-bench/ref$s-r.json
  target/release/gstore-bench merge --out bench/results/pre-phase6-lubm$s.json \
      target/gstore-bench/ref$s-sd.json target/gstore-bench/ref$s-r.json
done
```

On the 4-vCPU reference VM the pre-Phase-6 recording took about 7.5 min
(snapshot + disk) and 4.5 min (RocksDB) at univ 8, and 39 min (the disk
build alone 27 min) and 24 min (RocksDB: 9 opens of about 60 s, each in a
fresh process, plus 5 query processes) at univ 38. Run the long ones in the
background and nothing else meanwhile.

## Tests

`cargo test -p gstore-bench` (about 10 s in a debug build, after compiling): golden values of
the generator stream, the hashes and the univ-1 data; all 14 queries
non-empty at univ 1 and the micro row counts; the chunk's disjointness; the
gates on synthetic results; and the binary end to end with the `micro`
profile (bless → check, the canary, an injected crash and timeout exiting 2,
the data cache, merging, every backend answering alike). The end-to-end
tests use the `gbuild` next to the test binary when it exists (`cargo test
--workspace` builds it) and otherwise the library build, and say so;
`GSTORE_BENCH_TEST_BUILDER=cli|lib` forces one. The LUBM micro run on the
disk layout is `#[ignore]` (slow in a debug build).
