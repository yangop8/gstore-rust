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
| `ci [same options] [--allow-provisional]` | `check` with the smoke baseline, the summary appended to `$GITHUB_STEP_SUMMARY`; refuses (exit 2, before measuring) a provisional baseline or one `bless` would refuse, unless `--allow-provisional` (`check` only warns) | 0 pass, 1 fail |
| `compare BASELINE RESULT [--thresholds FILE] [--self-test-slowdown X] [--backends a,b] [--markdown] [--summary FILE]` | gate two existing files (`--backends`: only those backends or run keys) | 0 pass, 1 fail |
| `report RESULT... [--markdown] [--table KIND] [--compare BASELINE]...` | Markdown: the full report of one result (ratios and the gate table against BASELINE), or with `--table` one compact table over several results (see [Reports](#reports-and-documentation-tables)) | 0 |
| `report --update DOC... \| --check DOC...` | regenerate (`--update`) or verify (`--check`) the tables between `<!-- gstore-bench report ARGS -->` and `<!-- /gstore-bench report -->` in documents | 0, or 1 when `--check` finds a stale table |
| `bless --to FILE [--from RESULT] [--provisional] [--note TEXT] [--thresholds FILE] [RUN OPTIONS]` | turn a result (or a fresh run) into a baseline; refuses one that fails its own exact checks, and one measured by a debug build, from a dirty tree or no commit, with the library builder or a `gbuild` from a `debug` directory, on a busy machine (1-minute load above `bless.max_load_per_cpu` × CPUs) or with noisy query processes (spread above `geomean.max_spread`), unless `--provisional`, which blesses it marked provisional with the reasons in the note | 0, or 1 when refused |
| `merge --out FILE RESULT RESULT...` | join results of the same workloads measured by different binaries or settings (RocksDB from a `--features rocksdb` build; the disk layout in both query modes under two `--label`s); a run key may appear once per workload | 0, or 1 on a failed self-check |
| `calibrate [--quick]` | print this machine's calibration | 0 |

Every command exits **2** on a usage error, a crashed or timed-out phase, or
an unreadable file — never 0. Only `compare`, `merge`, `report` and its
`--update` / `--check` take positional arguments; any other command refuses
one (review infra#5: `check --from X other.json` used to gate X against the
default baseline).

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
workloads use their own defaults. `--conc-secs` and `--mixed-secs` must be
finite, positive and at most 3600, `--conc-clients` at least 1, and a mixed
point may not need more than 100 000 writes (`--mixed-secs` × 1000 ÷
`--mixed-write-ms`): checked before anything is generated (review
infra#16: a negative or NaN value used to panic the phase child after the
build and query phases, and the writer built all its statements up front —
it now makes them as it sends them).

## Profiles

| profile | workloads | backends | reps × query processes | conc | mixed (readers × s per point) | use |
|---|---|---|---|---|---|---|
| `micro` | LUBM-like univ 1, one department (7.5K triples) | snapshot | 2 × 2 | 1, 2 clients × 0.25 s | 2 × 0.25 s, a write every 50 ms | the harness's own tests |
| `smoke` | LUBM-like univ 1 (140K triples) **+ WatDiv-like scale 1** (106K) | snapshot, disk | 3 × 3 | 1, 2 clients × 1 s (report-only) | 2 × 1 s | CI gate |
| `reference` | LUBM-like univ 8 (`--scale 38` for 5.2M triples) **+ WatDiv-like scale 10** (995K) | snapshot, disk, rocksdb if compiled | 5 × 5 | 1, 8 clients × 5 s | 8 × 5 s | Phase 6 measurements |

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
   `INSERT DATA` (`triple_num` must grow by exactly its unique triples), a
   checkpoint and a reopen with it inserted (the original count plus the
   chunk's), the chunk's `DELETE DATA` (it must remove every chunk triple,
   and the count must be the original again); every query re-run (answers
   must be unchanged); a checkpoint; a reopen (the original count); every
   query re-run on the reopened database (answers must be unchanged).
   Before review infra#9 every write was undone before the only checkpoint
   and the answers were not re-checked after the reopen, so a checkpoint
   that lost pending inserts and deletes alike passed; `--inject
   lose-writes:update` (a checkpoint that restores the database as the phase
   found it) now fails `exact.update_round_trip`. The first checkpoint and
   reopen are recorded as `grown_checkpoint_ms` / `grown_reopen_ms`;
   `checkpoint_ms` / `reopen_ms` remain the final ones. Dictionary growth is
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

**watdiv-like** (`src/watdiv.rs`, version `watdiv-like/1`) follows the
Waterloo SPARQL Diversity Test Suite (WatDiv) in vocabulary, schema shape
and query shapes, but is its own generator (its data is not WatDiv's). The
prefixes are WatDiv's (`wsdbm:`, `sorg:`, `gr:`, `rev:`, `foaf:`, `og:`,
`mo:`, `gn:`, `dc:`); the classes and their sizes at scale `s`:

| class | entities | | class | entities |
|---|---:|---|---|---:|
| User | 1000·s | | City | 240 |
| Product | 250·s | | Country | 25 |
| Review | 1500·s | | Language | 25 |
| Offer | 900·s | | Topic | 250 |
| Purchase | 1500·s | | Genre / SubGenre | 21 / 145 |
| Retailer | 12·s | | ProductCategory | 15 |
| Website | 50·s | | Role / AgeGroup / Gender | 3 / 9 / 2 |

Each class's properties have a probability and a count range
(`watdiv::props`); `Retailer gr:offers Offer`, `Product rev:hasReview
Review` and `User wsdbm:makesPurchase Purchase` give every child exactly
one parent. `wsdbm:friendOf` and `wsdbm:follows` (71% of the triples, as in
WatDiv) and most references pick their target from a Zipf distribution
with integer weights (`2^40/k`, `2^48/isqrt(k·2^32)`), so the data is the
same on every platform; `Product0` is the most liked product. Entity 0 of
each class carries the triples that make every template answerable with
its placeholder bound to that entity (`watdiv::GUARANTEED`), which also
creates the templates' fixed constants (`Product0`, `Role2`, `Country1`,
`Country5`, `Language0`, `ProductCategory2`). Every line is a distinct
triple, written subject by subject. The update chunk is 100 users after the
last one (`User{n}`…), with references into the data: every chunk triple
has a new subject.

| scale | triples | bytes | data fnv64 |
|---|---:|---:|---|
| 1 (`smoke`) | 105,620 | 14,399,328 | `70c039a88063c158` (golden) |
| 10 (`reference`) | 995,333 | 137,160,546 | `19b2d2742cc622ad` |

At scale 10 each class's triples are within 2% of the design (User
785,617 of 790,000; Offer 60,915 of 60,750; Review 55,462 of 55,500;
Product 43,908 of 43,625; Purchase 37,606 of 37,500), and 69.9% are social
edges. Most bound L, S and F queries are selective (1–55 rows, under 2 ms
on the snapshot layout at scale 10); F3, F4, C1, C2 and C3 do more work
(C2 2,855 rows, C3 85,286).

The 20 templates `queries/watdiv/{L1-5,S1-7,F1-5,C1-3}.rq` (linear, star,
snowflake and complex shapes after WatDiv's basic testing set) start with a
`#mapping vN wsdbm:Class uniform` line per `%vN%` placeholder (`#mapping
none` for C1–C3). **ASK walk:** per template, up to 32 candidate bindings
come from the template's own SplitMix64 stream (uniform over the class's
entities; the 32nd is entity 0); the first whose `ASK` form is true against
the generated data (an in-memory database, the `instantiate` child) binds
the template. The walk is deterministic, so the bound queries are a
function of the workload parameters; they are cached as
`<stem>.queries.json` next to the data file and recorded in results (each
query's `bindings` and `tries`). A template no candidate answers fails the
run. `gstore-bench queries --workload watdiv --scale N` prints them.

`tiny` is a 285-triple toy graph with five queries for the harness's tests.

Generated files are cached in the data directory as `<stem>.nt` with a
`<stem>.nt.json` sidecar (generator version, parameters, size, hash); a
missing, stale or mismatching sidecar regenerates the file.

## Results and baselines

`gstore-bench/2` JSON (`src/schema.rs`; every `gstore-bench/1` file reads
as a `/2` document without the newer fields): environment (commit, host,
CPU, memory, load), calibration, workload (generator version, data hash,
query text hashes and, for bound templates, bindings, chunk), and per run
the build (with `gbuild`'s bulk-build `summary`), open, query, query
memory, update, concurrency and mixed-workload figures (with the `/api`
`begin` point), plus the binaries and options that measured it (`config`).
Further workloads of the run follow in `extra_workloads`, each with its own
workload and runs. A run is keyed by its backend, or `backend@label` with
`--label` (`BackendRun::key`). Times are ms, memory MiB, sizes bytes,
hashes 16 hex digits. A baseline is the same document with `"kind":
"baseline"`; `"provisional": true` marks one `bless --provisional` accepted
although it was unfit (the note says why): `ci` refuses to gate against it
(review infra#10).

* `bench/baselines/smoke.json` — the CI baseline (re-bless it when a change
  makes things ≥ 1.5× faster, or after an intended format change).
* `bench/results/pre-phase6-lubm8.json`, `pre-phase6-lubm38.json` — the
  pre-Phase-6 reference every Phase 6 acceptance ratio is measured against.
* `bench/results/phase6-final-lubm{8,38,82}.json`,
  `phase6-final-watdiv10.json` — the Phase 6 closure's reference results
  (with their `-compare*.md` reports); `docs/BENCHMARKS.md` and the README
  embed tables generated from them.

## Gates (`bench/thresholds.json`, `gstore-bench-gates/1`)

* **Exact** (never retried): generator version and data hash, query text
  hashes (also against the pinned values), `triple_num == unique_triples`,
  rows and row hash per query equal to the baseline and across backends,
  stable across runs and non-empty, the update round trip, identical
  concurrent bodies, every baseline backend and phase present.
* **Timing**, one-sided and calibration-normalized (measured times are
  divided by the calibration ratio, clamped to [0.5, 2]): per query
  `(m + 0.1 ms) / (b + 0.1 ms) > 3.0` fails; the per-backend geomean over
  **every** query of `(m + 0.05 ms) / (b + 0.05 ms)`, each ratio clamped to
  `[1/3, 3]`, fails above `max(1.5, s)`, where `s` is the baseline run's
  own process-to-process spread (the same geomean of each query's slowest
  process median against its fastest, `gates::query_spread`) and the limit
  never exceeds `max_spread` 1.6; build `> 2.0` (`> 2.5` for disk, 100 ms
  floor); warm open `> 2.5` (20 ms floor). Until review infra#2/#4 the
  per-query floor was 2 ms and the geomean left out queries under 1 ms, so
  most smoke queries (0.01–0.7 ms) could regress 50–300× with CI green,
  while the 1.5 limit sat inside the baseline's own spread (1.68 for the
  LUBM snapshot run): `tests/smoke_gate_sensitivity.rs` pins both at the
  smoke baseline's magnitudes.
* **Configuration** (exact): every run is measured with the baseline run's
  builder, warm repetitions and query processes (`exact.config`; `check`
  takes `--reps` and `--query-procs` from the baseline unless given).
* **Memory**: `(m + 32 MiB) / (b + 32 MiB) > 1.5` fails (build peak, after
  open, query high-water mark). **Size**: database bytes `> 1.10×` fails
  (1.25× until the Phase 6 closure; the bytes are deterministic, so the
  tighter bound needs no noise margin — re-bless after an intended size
  change).
* Concurrency and mixed-workload throughput, `/begin` latency and update
  times are report-only (`info.*`); the mixed phase's identical answers and
  acknowledged writes are exact.
* **New entries are reported, not gated** (`info.new`): a workload the
  baseline does not have (the WatDiv-like section of a smoke run against a
  baseline blessed before it existed), a run whose key and backend the
  baseline lacks, a query the baseline run lacks. A workload, run, query or
  phase the baseline has and the result lacks still fails.

Each workload is gated on its own against the baseline's workload of the
same id; an extra workload's subjects start with its id
(`watdiv-like:snapshot/L1`). Runs are matched by key; an unlabelled
baseline run is compared with every labelled run of its backend, so
`disk@stream` and `disk@materialize` are both gated against a pre-Phase-6
`disk` run.

A failing timing or memory gate re-runs only the failing phases of that
run (of that workload), up to `retries` (2) times, keeping the minimum of
the medians. The gates run after the update and mixed phases, so a retried
open or query phase first rebuilds the database those phases changed (and
a retried open must see the first attempt's term counts).
`--self-test-slowdown X` multiplies every measured time by X: CI checks that
the run's own result, slowed 2× and 3× and checked against itself, fails
(exit 1). A uniform 1.3× slowdown passes, 1.6× fails, one query of
≥ 20 ms slowed 4× fails, and a row-hash change fails without a retry
(`tests/gates.rs`); on `bench/baselines/smoke.json` itself
(`tests/smoke_gate_sensitivity.rs`) the baseline against its own slowest
query processes passes, any single query slowed 50× fails (also a 0.01 ms
one), a 0.02 ms query slowed 20× fails, every sub-millisecond query
regressed to 3 ms fails, and the canary fails at 2× in every workload's
geomean.

## Reports and documentation tables

`report RESULT` prints the full Markdown report of a result (every
workload, every phase; `--compare BASELINE` adds ratios and the gate
table). `report --table KIND RESULT... [--compare BASELINE]...` prints one
compact table over several results — `overview` (build, size, open, memory,
geomean, and ratios), `queries` (one table per workload), `build` (gbuild's
bulk-build summary), `update`, `conc`, `mixed` (with the `/begin` point),
`gates` or `full` — with a `workload` column; give no `--compare`, one for
all results, or one per result in order. The output is a pure function of
the JSON files: the same files give the same bytes.

Documents embed generated tables between markers whose first line holds
`report`'s arguments (paths relative to the workspace root):

```text
<!-- gstore-bench report --table overview bench/results/phase6-final-lubm38.json --compare bench/results/pre-phase6-lubm38.json -->
| workload | backend | build s | … |
<!-- /gstore-bench report -->
```

`gstore-bench report --update README.md CHANGELOG.md` regenerates every
marked table from the committed results; `report --check …` exits 1 when
one is stale. CI (ci.yml's test job and bench.yml's smoke job) checks every
tracked Markdown file with a marker at the start of a line (`git grep -l -E
'^[[:space:]]*<!-- gstore-bench report ' -- '*.md'`: README.md,
CHANGELOG.md, docs/BENCHMARKS.md and this file; review infra#6 — the
listed README.md and CHANGELOG.md used to be all).

A report compares each workload section of a result with the baseline's
section of the same id and workload, else with the baseline's section of
the same workload and parameters wherever it sits: the WatDiv-like section
of a reference run compares with `phase6-final-watdiv10.json`, whose main
workload it is.

## CI (`.github/workflows/bench.yml`)

* `smoke` (push, PR): release build, the documentation-table check (every
  marked Markdown file, see above), `gstore-bench ci` (LUBM-like univ 1 and
  WatDiv-like scale 1, both gated since the Phase 6 closure re-blessed
  `smoke.json` with them), the canary at 2× and at 3×, results and phase
  logs as an artifact.
* `rocksdb-correctness` (push, PR): `cargo test -p gstore-bench --features
  rocksdb` after building `gbuild` with RocksDB; the RocksDB build cache is
  shared with ci.yml's rocksdb job.
* `reference` (manual with a `scale` input, weekly): report-only run of the
  reference profile with RocksDB, recorded as the Phase 6 closure was
  (snapshot and RocksDB, then the disk layout under `--label materialize`
  and `--label stream`, merged), so its run keys match
  `phase6-final-lubm<scale>.json`, which it is compared with; its
  WatDiv-like section is compared with `phase6-final-watdiv10.json`, and the
  pre-Phase-6 file follows as a second table (review infra#8: only the
  pre-Phase-6 file was used, so a regression since the Phase 6 closure did
  not show, and the WatDiv-like section was always new).

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

The disk layout in both query modes goes into one file with labels (the
environment variable selects the mode, the label keys the run); the
WatDiv-like workload rides along once (`--extra none` elsewhere):

```text
B=target/release/gstore-bench
flock /tmp/gstore-bench.lock $B run --profile reference --scale 8 --backends snapshot --out s.json
flock /tmp/gstore-bench.lock $B run --profile reference --scale 8 --extra none --backends disk \
    --env GSTORE_DISK_QUERY_MODE=stream --label stream --out ds.json
flock /tmp/gstore-bench.lock $B run --profile reference --scale 8 --extra none --backends disk \
    --env GSTORE_DISK_QUERY_MODE=memory --label materialize --out dm.json
$B merge --out final-lubm8.json s.json ds.json dm.json
$B compare bench/results/pre-phase6-lubm8.json final-lubm8.json --markdown   # disk@stream and disk@materialize vs pre disk
```

## Tests

`cargo test -p gstore-bench` (about 15 s in a debug build, after compiling): golden values of
the generator stream, the hashes, the univ-1 LUBM data and the scale-1
WatDiv-like data and bound queries; all 14 LUBM queries non-empty at univ 1
and the micro row counts; the WatDiv-like composition (each class's triples
within ±10% of the design at scales 1 and 2), every template parsing,
binding and answering non-empty at scale 1, and snapshot and disk answering
alike; the chunks' disjointness; the gates on synthetic results (new
workloads, labelled runs, `/1` files); the reports (a golden table, marker
splicing, every committed result still loading and comparing); and the
binary end to end with the `micro` profile (bless → check, the canary, an
injected crash and timeout exiting 2, the data cache, merging, every backend
answering alike, a WatDiv-like run on snapshot and disk with the mixed phase,
a check that reports a new workload and passes). The end-to-end tests
run the `gbuild` next to the test binary (`cargo test --workspace` builds
it; alone, build it first with `cargo build --profile test -p gstore --bin
gbuild`, adding `--features rocksdb` for a RocksDB test run): a missing
`gbuild`, one older than a source file it is compiled from (the files
rustc's dep-info in its target directory lists for `gbuild` and the gstore
library, so editing a `#[cfg(test)]` module or another binary does not
count), or one without RocksDB for `--features rocksdb` fails them with
that instruction (review infra#18:
they used to fall back to the library build, or measure a stale binary,
saying so only on captured stderr). `GSTORE_BENCH_TEST_BUILDER=lib`
chooses the library build explicitly. The tests of the update phase
(`tests/update_phase_reopen.rs`) build with the library on purpose. The
LUBM micro run on the disk layout is `#[ignore]` (slow in a debug build).
