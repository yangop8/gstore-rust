# R4-CLOSURE-A — gstore-bench: WatDiv-like workload, run labels, mixed workload, gbuild summaries, report tables

Package R4-CLOSURE-A (the code half of plan §6 R4-CLOSURE, item 1, plus
every bench request addressed to R4-CLOSURE) of `docs/PHASE6_PLAN.md`,
Round 4. Base: `d702463` (Phases 0–5 and Phase 6 Rounds 1–3 merged).
Files: `bench/**` and `.github/workflows/bench.yml` only. The final runs,
the re-blessed smoke baseline and the docs are the closure stage's.

## Changelog

### gstore-bench: a second workload, labelled runs, the mixed workload, and documentation tables

- **WatDiv-like workload `watdiv-like/1`** (`bench/src/watdiv.rs`,
  `--workload watdiv`): a deterministic generator in the spirit of the
  Waterloo SPARQL Diversity Test Suite — WatDiv's prefixes (`wsdbm:`,
  `sorg:`, `gr:`, `rev:`, `foaf:`, `og:`, `mo:`, `gn:`, `dc:`) and entity
  classes in a simplified e-commerce / social schema. Scalable classes grow
  with the scale factor (User 1000·s, Product 250·s, Review 1500·s, Offer
  900·s, Purchase 1500·s, Retailer 12·s, Website 50·s), the others keep
  WatDiv's sizes (City 240, Country 25, Topic 250, SubGenre 145, …).
  Social edges (`wsdbm:friendOf`, `wsdbm:follows`) and most references
  pick their targets from a Zipf distribution with integer weights (so the
  output is identical on every platform; `Product0` is the most liked
  product). Scale 1 = 105,620 triples (golden FNV `70c039a88063c158`),
  scale 10 = 995,333 (`19b2d2742cc622ad`); 70–71% are social edges, as in WatDiv. Every line is
  a distinct triple. The templates' fixed constants (`Product0`, `Role2`,
  `Country1`, `Country5`, `Language0`, `ProductCategory2`) always exist,
  and entity 0 of each class carries the triples that make every template
  answerable when its placeholder is bound to it. The update chunk is 100
  users beyond the data set's last one (disjoint from the data).
- **20 query templates** `bench/queries/watdiv/{L1-5,S1-7,F1-5,C1-3}.rq`
  (linear, star, snowflake, complex — WatDiv's basic testing shapes), each
  with a `#mapping vN wsdbm:Class uniform` header (`#mapping none` for the
  constant-free C1–C3). They are **bound by a deterministic ASK walk**: per
  template up to 32 candidate bindings from the template's own SplitMix64
  stream (uniform over the class; the last try is entity 0), and the first
  whose `ASK` is true against the generated data wins. The runner binds
  them in an `instantiate` child (an in-memory database) once per data file
  and caches the result next to it (`<stem>.queries.json`, keyed by the
  generator version, the parameters, the data hash and the templates;
  `--verify-data` re-runs the walk and fails on a difference). Results
  record each query's binding and tries. `gstore-bench queries --workload
  watdiv --scale N` prints the bound queries.
- **Profiles:** `smoke` (CI) measures the WatDiv-like workload at scale 1
  after LUBM univ 1, `reference` at scale 10 after LUBM, each into its own
  section of the result (`extra_workloads`). `--extra none|watdiv:10[,…]`
  sets the further workloads, `--workload watdiv` measures WatDiv alone
  (at the profile's scale for it), `--shrink K` divides the scalable
  classes (tests).
- **Result schema `gstore-bench/2`**, a strict superset of `/1` (every
  committed `/1` result and baseline loads, self-checks and compares
  unchanged; a test reads all of them): `extra_workloads`; a run `label`
  (`--label L` keys every run `backend@L`, so one backend measured twice —
  the disk layout with `--env GSTORE_DISK_QUERY_MODE=stream` and `=memory`
  — shares a file through `merge`, and an unlabelled baseline run is
  compared with each labelled run of its backend); the build's `summary`
  (gbuild's bulk-build report parsed from its output: phase times, memory
  budget and peak, term map, threads, spilled runs — disk and RocksDB
  builds); a query's `bindings`/`tries`; the `mixed` phase.
- **Gates:** each workload of a result is gated on its own against the
  baseline's workload of the same id (subjects `watdiv-like:snapshot/L1`).
  What the baseline lacks — a whole workload, a run, a query — is reported
  as `info.new` and **not gated** (so CI stays green when the smoke profile
  gains the WatDiv-like workload before `smoke.json` is re-blessed); a
  workload, run or phase the baseline has and the result lacks still fails.
  Retries name the workload (`RetryTarget { section, run, phase }`). A run
  key twice in one workload, or a workload twice in one result, fails the
  self-check.
- **Mixed-workload phase** (after the update phase; it writes): per
  endpoint (`/sparql`, `/api`) and `check_point` setting (off, on),
  `readers` closed-loop readers of one query (LUBM `q4`, WatDiv `F5`) while
  one writer sends a single-triple `INSERT DATA` every 100 ms; reader qps
  and latency, acknowledged writes and their latency, and an exact gate
  (every reader got the same answer — compared as a multiset of rows,
  because a write changes the counts the planner costs joins with and so
  the row order — and every write reported one changed triple). Profiles:
  reference 8 readers × 5 s per point, smoke 2 × 1 s, micro 2 × 0.25 s;
  `--no-mixed`, `--mixed-readers`, `--mixed-write-ms`, `--mixed-secs`,
  `--mixed-checkpoint off,on`, `--mixed-endpoints`, `--mixed-query`. It
  runs on every layout, so the disk layout's incremental commit can be
  compared with the snapshot layout's full save. It also takes 20 **`/api`
  `begin` samples** (each rolled back) on the loaded database: latency and
  the server process's RSS growth while the transaction's snapshot is held
  (G4's `/begin` row). This replaces R3-SRV's `examples/srv_mixed_load.rs`.
- **Database bytes** no longer count the disk layout's `LOCK` (it holds
  the builder's pid, whose digit count varied the size between identical
  builds) nor RocksDB's info logs (`rocksdb/LOG*`); both stay listed in
  `files`.
- **Reports:** `report --table overview|queries|build|update|conc|mixed|gates|full
  RESULT... [--compare BASE]...` prints one deterministic table over one or
  more results (a `workload` column; ratios against each result's
  baseline). `report --update DOC...` regenerates every table between
  `<!-- gstore-bench report ARGS -->` and `<!-- /gstore-bench report -->`
  from the committed JSON (ARGS are `report`'s own arguments), byte for
  byte; `report --check DOC...` exits 1 when one is stale. The bench
  workflow's smoke job runs `report --check README.md CHANGELOG.md`.

## GAP_PLAN status line

阶段 6:R4-CLOSURE-A — gstore-bench 增加 WatDiv-like 负载(watdiv-like/1,20 个模板,ASK 遍历确定性实例化;smoke 1 / reference 10)、结果格式 gstore-bench/2(运行标签、多负载分节、gbuild 摘要、混合读写负载与 /begin 点)、新增项只报告不判失败、可按标记逐字节再生成的报告表格 — 完成;最终测量与文档归 closure 阶段。

## Docs notes

- `docs/BENCHMARKS.md` (closure): the WatDiv-like workload — schema,
  scalable and fixed classes, Zipf references, the 20 templates and their
  shapes, the ASK walk (32 tries, entity-0 guarantee, the cache next to the
  data), and that it is **not** WatDiv's generator (no byte or answer
  compatibility with WatDiv data; only the shapes and the vocabulary
  follow it). Sizes: scale 1 105,620 triples, scale 10 995,333 (see
  Measured numbers).
- `docs/BENCHMARKS.md`: the result schema `/2` (labels, `extra_workloads`,
  build summary, mixed phase, `/begin` point) and that the gates report new
  workloads / runs / queries instead of failing; the mixed phase's method
  (multiset bodies; it writes, so it runs last and a retried phase rebuilds
  the database).
- README / CHANGELOG generated tables: write the markers
  `<!-- gstore-bench report --table overview bench/results/phase6-final-lubm38.json --compare bench/results/pre-phase6-lubm38.json -->`
  … `<!-- /gstore-bench report -->` and run `gstore-bench report --update
  README.md CHANGELOG.md`; CI then checks them (`report --check`). Paths in
  a marker are relative to the workspace root.
- `bench/README.md` is updated here (commands, options, profiles, phases,
  WatDiv-like workload, schema, gates, reports, recording a reference with
  labels).

## Measured numbers

**Implementer run, noisy.** 4 vCPU VM shared with another implementer
(its univ 38 base runs held the bench lock in between; load average 1.3–2.5
at the starts of the smoke runs, up to 8.8 during the scale-10 runs while
this package's own workspace tests compiled). Every run under `flock
/tmp/gstore-bench.lock`; release builds of `gbuild` and `gstore-bench`;
data in this package's scratch directory (deleted afterwards), databases
deleted by the harness. Base = `d702463` (`/tmp/gstore-base-d702463/bin`).
The gstore library is unchanged by this package, so engine numbers are not
an A/B; the A/B is the harness's own cost.

**Generator and binding** (release):

| workload | triples | bytes | generation | binding (ASK walk, in-memory db) | tries |
|---|---:|---:|---:|---:|---:|
| WatDiv-like scale 1 | 105,620 | 14,399,328 | 0.1 s | 0.6 s | 81 (S4 27, F4 15, F1 10, F2 7) |
| WatDiv-like scale 10 | 995,333 | 137,160,546 | 0.9 s (1.1M triples/s) | 6.0 s | 34 (S4 13) |

Composition: at scale 1 every class is within ±10% of the design (the
test; users +5% from the friendOf draw), at scale 10 within 2% (User
785,617 / 790,000, Offer 60,915 / 60,750, Review 55,462 / 55,500, Product
43,908 / 43,625, Purchase 37,606 / 37,500, Retailer 9,224 / 9,210, Website
1,719 / 1,750); social edges 71.0% (scale 1) and 69.9% (scale 10).

**Smoke profile, wall time of `gstore-bench run --profile smoke`** (base:
LUBM univ 1 on snapshot + disk with the concurrency phase; branch: the same
plus the WatDiv-like workload at scale 1 and the mixed phase with its
`/begin` point on every run), alternating:

| run | base `d702463` | this branch |
|---|---:|---:|
| 1 | 25.6 s | 66.2 s |
| 2 | 25.0 s | 65.0 s |

`gstore-bench ci --baseline bench/baselines/smoke.json` with this branch:
**PASS, 307 gates, 0 failed, "1 new workload(s) not in the baseline"** (the
WatDiv-like section, its two runs reported as `info.new`), 64.8 s; the
canary (`check --from` the result against itself `--self-test-slowdown 3`)
exits 1. A CI smoke job therefore stays well under its 15 min budget (the
release build dominates).

**WatDiv-like scale 10** (`run --profile reference --workload watdiv`:
snapshot; disk with `GSTORE_DISK_QUERY_MODE=memory --label materialize`;
disk with `=stream --label stream`; joined by `merge`, whose self-check
passed — all 20 answers identical on the three runs). 85–87 s per run.

| run | build s | dir MB | open warm ms | RSS after open MiB | query HWM MiB | geomean ms |
|---|---:|---:|---:|---:|---:|---:|
| snapshot | 5.81 | 44.3 | 154.6 | 61.4 | 79.8 | 0.46 |
| disk@materialize | 4.39 | 83.2 | 289.9 | 66.6 | 88.9 | 1.05 |
| disk@stream | 4.55 | 83.2 | 1.4 | 7.2 | 44.4 | 1.19 |

| query | rows | snapshot ms | disk@materialize ms | disk@stream ms |
|---|---:|---:|---:|---:|
| L1 / L2 / L3 / L4 / L5 | 2 / 23 / 22 / 8 / 2 | 0.97 / 0.49 / 0.02 / 0.02 / 0.22 | 1.27 / 0.62 / 0.26 / 0.23 / 0.28 | 1.50 / 0.95 / 0.28 / 0.25 / 0.40 |
| S1 / S2 / S3 / S4 | 9 / 8 / 55 / 2 | 1.97 / 0.06 / 0.12 / 0.26 | 2.01 / 0.36 / 0.28 / 0.52 | 2.11 / 0.40 / 0.38 / 0.55 |
| S5 / S6 / S7 | 7 / 4 / 1 | 0.05 / 0.02 / 0.02 | 0.22 / 0.12 / 0.09 | 0.25 / 0.13 / 0.11 |
| F1 / F2 / F3 / F4 / F5 | 1 / 1 / 51 / 8 / 37 | 0.16 / 1.20 / 9.13 / 4.05 / 1.51 | 0.38 / 1.36 / 13.41 / 6.81 / 1.66 | 0.42 / 1.43 / 14.27 / 6.66 / 1.83 |
| C1 / C2 / C3 | 95 / 2,855 / 85,286 | 5.94 / 28.72 / 39.24 | 9.03 / 35.69 / 31.43 | 9.63 / 38.84 / 34.66 |

So the templates bind to selective constants (as WatDiv's L/S/F mostly
are) and finish in well under a second; the complex ones and F3/F4 carry
the work. Further figures of the same runs (report-only):

- gbuild summary (disk): `parse+encode` 3,983 / 4,129 ms of the ~4.4 s,
  budget 1024 MiB with a 44.0 MiB peak, term map 8.3 MiB, 0 spills.
- Mixed workload (8 readers of F5 + 1 writer / 100 ms, 5 s per point;
  reader qps for /sparql cp-off, /sparql cp-on, /api cp-off, /api cp-on):
  snapshot 1,409 / 55 / 934 / 34 (with check_point on each write saves
  the whole snapshot: 212–276 ms per write, 20–25 writes instead of 51);
  disk@materialize 1,826 / 1,802 / 1,161 / 1,080; **disk@stream 270 / 272
  / 212 / 293** — under writes Stream-mode readers drop to ~0.2× of
  Materialize (without writes, F5 at 8 clients: 1,182 vs 1,172 qps), a
  finding for the closure stage / R4 owners (every acknowledged write
  invalidates Stream's per-query caches and waits on the writer gate).
- `/api begin` (20 samples, rolled back; the base's `/begin` copies the
  store): median 52.0 ms and +35 MiB RSS (snapshot), 221 ms / +31 MiB
  (disk@materialize), 307 ms / +48–59 MiB (disk@stream). R4-SNAP's O(1)
  `/begin` should show here once merged.
- Update: single INSERT median 0.01 ms (snapshot) and 12.0 ms (both disk
  modes: the fsync per acknowledged write); chunk (6,631 triples) insert
  63 / 1,157 / 1,229 ms.

**Tests** (debug, `cargo test -p gstore-bench`, after compiling): 28 unit +
8 end to end (1 ignored) + 14 gates + 8 golden + 3 report + 5 WatDiv tests,
all passing, 15–33 s wall depending on the load (the WatDiv end-to-end run
on snapshot and disk, mixed phase included, is the longest at ~12 s).

**For the quiet measure / closure stage** (after R4-RECLAIM, R4-SNAP and
R4-VS are merged; `B=target/release/gstore-bench`, `export
GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache`, every run under `flock
/tmp/gstore-bench.lock`, nothing else running):

```text
cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench
CARGO_TARGET_DIR=target-rocks cargo build --release -p gstore -p gstore-bench --bin gbuild \
    --bin gstore-bench --features gstore/rocksdb,gstore-bench/rocksdb
# LUBM univ 8 and 38: snapshot, disk in both query modes (labelled), RocksDB; one file per scale
for s in 8 38; do
  $B run --profile reference --scale $s --extra none --backends snapshot --drop-caches --verify-data --out f$s-s.json
  $B run --profile reference --scale $s --extra none --backends disk --env GSTORE_DISK_QUERY_MODE=stream \
      --label stream --drop-caches --verify-data --out f$s-ds.json
  $B run --profile reference --scale $s --extra none --backends disk --env GSTORE_DISK_QUERY_MODE=memory \
      --label materialize --drop-caches --verify-data --out f$s-dm.json
  target-rocks/release/gstore-bench run --profile reference --scale $s --extra none --backends rocksdb \
      --bin-dir target-rocks/release --drop-caches --verify-data --out f$s-r.json
  $B merge --out bench/results/phase6-final-lubm$s.json f$s-s.json f$s-ds.json f$s-dm.json f$s-r.json
  $B compare bench/results/pre-phase6-lubm$s.json bench/results/phase6-final-lubm$s.json --markdown \
      > bench/results/phase6-final-lubm$s-compare.md
done
# WatDiv-like scale 10, the same layouts
$B run --profile reference --workload watdiv --backends snapshot --verify-data --out w-s.json
$B run --profile reference --workload watdiv --backends disk --env GSTORE_DISK_QUERY_MODE=stream --label stream --out w-ds.json
$B run --profile reference --workload watdiv --backends disk --env GSTORE_DISK_QUERY_MODE=memory --label materialize --out w-dm.json
target-rocks/release/gstore-bench run --profile reference --workload watdiv --backends rocksdb \
    --bin-dir target-rocks/release --out w-r.json
$B merge --out bench/results/phase6-final-watdiv10.json w-s.json w-ds.json w-dm.json w-r.json
# re-bless the smoke baseline (LUBM univ 1 + WatDiv-like 1, mixed phase), then check it
$B bless --profile smoke --to bench/baselines/smoke.json
$B ci --baseline bench/baselines/smoke.json        # PASS, no "new" entries any more
# documentation tables: add markers to README.md / CHANGELOG.md / docs/BENCHMARKS.md, then
$B report --update README.md CHANGELOG.md docs/BENCHMARKS.md && $B report --check README.md CHANGELOG.md
```

Delete the `f*.json` / `w*.json` intermediates and the databases (the
harness removes its work directories) afterwards. Univ 82 (≈ lubm10M) on
disk and snapshot is the same `run` with `--scale 82` if RAM allows.

## Requests/Not yet

Requests (outside this package's files):

1. **Integrator — remove `examples/srv_mixed_load.rs`** (R3-SRV request 5):
   the mixed phase covers it (same servers, same writer statement, 8
   readers + 1 writer every 100 ms, check_point off and on, `/sparql` and
   `/api`), on every layout and in the result JSON. Its doc comment is the
   only place that names it besides R3-SRV's fragment.
2. **Fragment name.** This package's brief says to write
   `changelog.d/phase6-R4-CLOSURE-A.md` (this file), while the file list of
   plan §6 R4-CLOSURE names `phase6-R4-CLOSURE.md`; the closure stage folds
   this fragment in with the others (and may write `phase6-R4-CLOSURE.md`
   for its own part).
3. **Closure stage — re-bless `bench/baselines/smoke.json`** on the idle
   machine with the new tool (`gstore-bench bless --profile smoke --to
   bench/baselines/smoke.json`): until then the smoke job reports the
   WatDiv-like workload, the mixed phase and the `/begin` point as new
   (`info.new`, not gated) and still gates LUBM univ 1 against the R1
   baseline. After the re-bless the baseline is `gstore-bench/2`, which a
   pre-R4 harness refuses (by design: it cannot read labels or sections).
4. **R4-SNAP / server owners (note, no change needed):** besides the
   constructors R1-BENCH listed (`Server::bind`, `shutdown_handle`,
   `serve_forever`, `ApiServer::bind`, `with_query_settings`,
   `QuerySettings`), the mixed phase uses `Server::with_db_dir`,
   `Server::with_check_point`, `ApiServer::with_check_point` and the `/api`
   `begin` / `rollback` operations (`TID` in the answer). All exist on
   `d702463` and on `p6/r4-snap` as of this package.

Not yet / notes:

- The final reference runs, the WatDiv-like scale 10 reference file, the
  README / CHANGELOG / `docs/BENCHMARKS.md` tables and the G1–G7 verdicts
  are the closure stage's (commands under Measured numbers).
- WatDiv-like templates bind uniformly only (`#mapping … uniform`); WatDiv's
  `zipfian` / `normal` mappings and its stress-testing query generator are
  not ported. A template's placeholder must be an entity of a `wsdbm:`
  class.
- The mixed phase's writer inserts new subjects with a predicate no query
  uses, so the answers do not change while it writes; a workload whose
  concurrency query would see those triples needs another
  `single_insert`.
- The `/begin` point measures the in-process server's RSS (the harness
  process); its client threads are idle while it samples, so the growth is
  the snapshot's.
