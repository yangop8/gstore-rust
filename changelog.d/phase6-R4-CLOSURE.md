# R4-CLOSURE — final runs: reference results, the re-blessed smoke baseline, the exit-gate table

Package R4-CLOSURE of `docs/PHASE6_PLAN.md` (Round 4), item 2 of its
brief ("Final runs"), on phase6 `769c627` (Rounds 1–4 merged, the review
fixes, and the three quiet post-merge stages of R4-RECLAIM, R4-SNAP and
R4-VS). Item 1 (the WatDiv-like workload and the harness changes) is
`phase6-R4-CLOSURE-A.md`; item 3 (the documentation closure: folding the
fragments, `docs/BENCHMARKS.md`, the README / CHANGELOG tables, GAP_PLAN)
is the next stage. Files: `bench/**` and this fragment only; no gstore
code changed.

## Changelog

### Phase 6 closure: final reference results, smoke baseline, size gate

- **Final reference results** (`bench/results/`, gstore-bench/2), every
  run with the reference profile (5 query processes × 5 warm runs, the
  concurrency, update and mixed phases, `--drop-caches`, `--verify-data`):
  - `phase6-final-lubm8.json`, `phase6-final-lubm38.json`: the snapshot
    layout, the disk layout in Stream mode (`disk@stream`) and in
    Materialize mode (`disk@materialize`), and RocksDB, one file per scale
    (`gstore-bench merge`);
  - `phase6-final-lubm82.json`: univ 82 (11,135,159 triples, about
    upstream's lubm10M) on the snapshot layout and the disk layout in both
    modes;
  - `phase6-final-watdiv10.json`: the WatDiv-like workload at scale 10 on
    the four layouts;
  - compare reports: `phase6-final-lubm{8,38}-compare.md` against
    pre-Phase-6 (PASS, 373 gates each), `-compare-r3.md` against Round 3
    (PASS, 365 each), `-compare-r3-stream.md` (the Stream run against
    Round 3's Stream run: PASS, 106 each).
- **Smoke baseline re-blessed** (`bench/baselines/smoke.json`, now
  gstore-bench/2): LUBM-like univ 1 and WatDiv-like scale 1 on the snapshot
  and disk layouts, with the mixed phase and the `/api begin` point. CI
  now gates the WatDiv-like workload instead of reporting it as new. A
  pre-R4 harness cannot read the new baseline (by design).
- **Size gate tightened** (`bench/thresholds.json`): a database larger than
  1.10× the baseline fails (was 1.25×). Database sizes are deterministic, so
  this bound needs no noise margin; the timing and memory gates are
  unchanged.

## GAP_PLAN status line

阶段 6:R4-CLOSURE(最终测量)— 最终参考结果 `bench/results/phase6-final-lubm{8,38,82}.json`、`phase6-final-watdiv10.json` 已提交;smoke 基线重新 bless(gstore-bench/2,含 WatDiv-like 与混合负载),大小闸门收紧为 1.10;退出闸门 G1a、G1b、G1c、G1d、G3、G6、G7 通过,G2(Q11/Q12 的 univ 38 / univ 8 比值 5.58 / 4.91 > 2.0)、G4(`/api stat` 固定 200 ms CPU 采样)、G5(±5% 与内存、Materialize 删除开销)未通过,原因与后续见 `changelog.d/phase6-R4-CLOSURE.md`;默认磁盘查询模式保持 Materialize。

## Docs notes

- `docs/BENCHMARKS.md`: the exit-gate table below (target, measured value,
  verdict, reason and follow-up per gate), the result files it comes from,
  the measurement protocol (reference VM, quiet machine, `--drop-caches`,
  minimum over 5 processes of the median of 5 warm runs, the bench lock),
  and the comparison with upstream lubm10M with its caveats (different
  generator, hardware and harness; upstream's ~23 ms floor per query; our
  numbers are warm medians in-process).
- README / CHANGELOG generated tables: the markers can now point at the
  committed final results, e.g. `<!-- gstore-bench report --table overview
  bench/results/phase6-final-lubm38.json --compare
  bench/results/pre-phase6-lubm38.json -->` (and `phase6-final-lubm82.json`
  without `--compare` for the lubm10M-sized table).
- `docs/STORAGE_BACKEND.md` / `docs/BENCHMARKS.md`: Stream mode's large
  results at univ 82 (below: Q6 / Q13 / Q14 4.6–6.2× the snapshot layout,
  against 2.3–2.5× at univ 38), and that under a steady write load Stream
  readers of a query over long posting lists lose their list cache at every
  commit (WatDiv-like F5: below). Both are reasons Materialize stays the
  default disk query mode.
- `bench/README.md` already states the new size limit (this package).

## Measured numbers

### Quiet final stage (phase6 `769c627`)

Reference VM (4 vCPU Intel Xeon @ 2.10GHz, 16 GB), nothing else running,
2026-11-04. Toolchain 1.99.1, `CARGO_PROFILE_DEV_DEBUG=0
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`. Release builds of
`769c627` (`cargo build --release -p gstore -p gstore-bench --bins`, and the
same with `CARGO_TARGET_DIR=target-rocks … --features
gstore/rocksdb,gstore-bench/rocksdb`); the univ 82 runs used the harness
binary rebuilt at `85686e5` (the size-gate commit; the gstore code is the
same). Every heavy command ran under `flock /tmp/gstore-bench.lock`; data
from `/tmp/gstore-bench-cache` (univ 8 `ed44da2e7b3bbb98`, univ 38
`fcd6493725f3c66a`, WatDiv-like 10 `19b2d2742cc622ad`; univ 82 generated
there for this stage and deleted afterwards). `vmstat` every 15 s: CPU busy
45% on average during the univ 8 / 38 / WatDiv runs (07:19–07:44 UTC, up to
90% in the 8-client concurrency points), 34% during the smoke runs, 31%
during the univ 82 runs (08:06–08:26); steal ≤ 2%; calibration scores
67–79 ms (one later sample 91 ms, noted where used). Commands:

```sh
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache B=target/release/gstore-bench R=target-rocks/release/gstore-bench
for s in 8 38; do
  flock /tmp/gstore-bench.lock $B run --profile reference --scale $s --extra none --backends snapshot \
      --drop-caches --verify-data --out f$s-s.json
  flock /tmp/gstore-bench.lock $B run --profile reference --scale $s --extra none --backends disk \
      --env GSTORE_DISK_QUERY_MODE=stream --label stream --drop-caches --verify-data --out f$s-ds.json
  flock /tmp/gstore-bench.lock $B run --profile reference --scale $s --extra none --backends disk \
      --env GSTORE_DISK_QUERY_MODE=memory --label materialize --drop-caches --verify-data --out f$s-dm.json
  flock /tmp/gstore-bench.lock $R run --profile reference --scale $s --extra none --backends rocksdb \
      --bin-dir target-rocks/release --drop-caches --verify-data --out f$s-r.json
  $B merge --out bench/results/phase6-final-lubm$s.json f$s-s.json f$s-ds.json f$s-dm.json f$s-r.json
  $B compare bench/results/pre-phase6-lubm$s.json bench/results/phase6-final-lubm$s.json --markdown \
      > bench/results/phase6-final-lubm$s-compare.md
  $B compare bench/results/phase6-r3-lubm$s.json bench/results/phase6-final-lubm$s.json --markdown \
      > bench/results/phase6-final-lubm$s-compare-r3.md
  # the Stream run alone (the final file with its other runs removed) against Round 3's Stream run
  $B compare bench/results/phase6-r3-disk-stream-lubm$s.json stream-only-$s.json --markdown \
      > bench/results/phase6-final-lubm$s-compare-r3-stream.md
done
# WatDiv-like scale 10: the same four runs with --workload watdiv (no --scale / --extra), merged into
# bench/results/phase6-final-watdiv10.json
# univ 82: snapshot, disk --label materialize (GSTORE_DISK_QUERY_MODE=memory, the default mode),
# disk --label stream, merged into bench/results/phase6-final-lubm82.json
# G1b: three more Stream open pairs (univ 8, 38 alternating); two Materialize open samples at univ 38:
flock /tmp/gstore-bench.lock $B run --profile reference --scale $s --extra none --backends disk \
    --env GSTORE_DISK_QUERY_MODE=stream --label stream --reps 1 --query-procs 1 --open-reps 3 \
    --no-update --no-conc --drop-caches --out open$s-$i.json
# smoke: ci twice against the Round-2 baseline, bless --from the first result, ci four times against
# the new baseline, and the canary:
flock /tmp/gstore-bench.lock $B ci --baseline bench/baselines/smoke.json --out ci.json
$B bless --from ci.json --to bench/baselines/smoke.json --note "…"
$B check --baseline ci.json --from ci.json --self-test-slowdown 3   # exit 1
```

Warm figures are the minimum over 5 processes of the median of 5 warm
runs; "open first" is the first open after the page cache was dropped.

#### LUBM-like univ 38 (5,181,103 triples) and univ 8 (1,029,275)

| univ 38 | build s | build peak MiB | database MB | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms | vs pre: build / geomean |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 29.60 | 550.1 | 430.8 | 1184.8 / 1033.0 | 489.1 | 601.0 | **10.15** | 0.39× / 0.22× |
| disk, Materialize | 21.06 | 400.3 | 654.0 | 1817.3 / 1720.8 | 351.0 | 457.1 | **7.83** | 0.013× / 0.053× |
| disk, Stream | 18.38 | 398.2 | 654.0 | **7.5 / 0.9** | **6.1** | **207.3** | **11.55** | 0.011× / 0.079× |
| RocksDB | 19.50 | 401.6 | 155.7 | 738.9 / 678.0 | 270.8 | 436.2 | **25.00** | 0.070× / 0.080× |
| pre: snapshot / disk / RocksDB | 76.82 / 1627.59 / 277.76 | 741 / 738 / 634 | 430.8 / 889.8 / 206.3 | 1588 / 1465; 12387 / 7673; 60811 / 59389 | 596.4 / 544.0 / 522.0 | 710.3 / 740.6 / 663.3 | 46.65 / 146.92 / 311.50 | |

| univ 8 | build s | build peak MiB | database MB | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms | vs pre: build / geomean |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| snapshot | 5.41 | 112.8 | 85.5 | 233.6 / 202.6 | 102.2 | 128.4 | 2.16 | 0.35× / 0.27× |
| disk, Materialize | 3.55 | 108.4 | 130.2 | 305.8 / 267.2 | 89.1 | 115.5 | 2.05 | 0.013× / 0.108× |
| disk, Stream | 3.65 | 96.9 | 130.2 | 4.1 / 0.8 | 6.0 | 66.0 | 3.06 | 0.014× / 0.161× |
| RocksDB | 4.31 | 92.6 | 31.1 | 164.4 / 144.1 | 66.4 | 102.0 | 5.16 | 0.080× / 0.119× |

Univ 38, warm ms per query (pre = `pre-phase6-lubm38.json`, whose disk run is the pre-Phase-6 disk layout):

| query | rows | snapshot | Materialize | Stream | RocksDB | pre snapshot | pre disk | pre RocksDB | Stream / snapshot |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Q1 | 3 | 0.013 | 0.013 | 0.019 | 0.036 | 1.298 | 39.06 | 60.72 | 1.51 |
| Q2 | 115 | 37.56 | 28.64 | 56.64 | 585.7 | 50.80 | 189.7 | 729.5 | 1.51 |
| Q3 | 10 | 0.014 | 0.013 | 0.016 | 0.070 | 2.191 | 39.17 | 195.5 | 1.10 |
| Q4 | 9 | 0.049 | 0.051 | 0.071 | 0.262 | 53.63 | 75.19 | 990.0 | 1.43 |
| Q5 | 576 | 234.7 | 232.5 | 274.1 | 353.9 | 333.2 | 621.3 | 537.7 | 1.17 |
| Q6 | 404,399 | 80.77 | 91.93 | 194.8 | 232.1 | 187.6 | 511.4 | 468.3 | 2.41 |
| Q7 | 23 | 256.7 | 256.2 | 278.7 | 394.9 | 403.0 | 1147.7 | 977.6 | 1.09 |
| Q8 | 10,778 | 244.6 | 274.9 | 256.8 | 421.3 | 363.5 | 871.8 | 904.9 | 1.05 |
| Q9 | 5,507 | 343.8 | 347.5 | 521.7 | 742.1 | 529.3 | 1450.6 | 2125.9 | 1.52 |
| Q10 | 3 | 231.7 | 232.6 | 219.3 | 400.0 | 326.6 | 697.1 | 594.9 | 0.95 |
| Q11 | 325 | 1.305 | 0.173 | 0.256 | 2.009 | 1.085 | 1.488 | 16.79 | 0.20 |
| Q12 | 21 | 0.494 | 0.080 | 0.165 | 0.223 | 0.587 | 0.721 | 10.19 | 0.33 |
| Q13 | 432,512 | 96.02 | 107.7 | 215.9 | 257.2 | 223.9 | 906.1 | 511.9 | 2.25 |
| Q14 | 305,503 | 62.22 | 72.48 | 156.0 | 208.6 | 132.1 | 320.3 | 382.7 | 2.51 |

Ratios against pre for every query, with the gates, are in `phase6-final-lubm38-compare.md`.

Concurrency (Q4, closed loop, 5 s per point; qps at 1 → 8 clients):

| layout | univ 38 `/sparql` | univ 38 `/api` | univ 8 `/sparql` | univ 8 `/api` |
|---|---|---|---|---|
| snapshot | 6,223 → 33,158 (**5.33×**) | 2,504 → 11,600 (**4.63×**) | 5,834 → 30,561 (5.24×) | 2,560 → 9,958 (3.89×) |
| disk, Materialize | 5,795 → 32,815 (5.66×) | 2,597 → 11,694 (4.50×) | 5,661 → 31,466 (5.56×) | 2,508 → 11,466 (4.57×) |
| disk, Stream | 4,801 → 19,493 (**4.06×**) | 1,968 → 9,309 (**4.73×**) | 4,660 → 20,255 (4.35×) | 2,018 → 8,832 (4.38×) |
| RocksDB | 2,529 → 10,860 (4.29×) | 1,630 → 6,207 (3.81×) | 2,503 → 10,209 (4.08×) | 1,383 → 6,296 (4.55×) |
| pre: snapshot / disk / RocksDB | 0.78× / 0.93× / 0.97× (8 clients 13.4 / 11.7 / 1.2 qps) | 0.86× / 0.99× / 0.96× | 0.94× / 0.93× / 0.88× | 0.89× / 0.89× / 0.90× |

`/api begin` point (20 samples, each rolled back; the in-process server's
RSS growth while the snapshot is held): univ 38 snapshot median 0.38 ms
(max 1.13), +0.07 MiB; Stream 0.44 ms (max 0.62), +0.06 MiB; Materialize
0.38 ms (max 0.93), +0.00 MiB; RocksDB 2,133 ms (max 3,191), +269 MiB
(RocksDB `/begin` still materializes, plan §8 D4). Univ 8: 0.44 / 0.40 /
0.46 ms, RocksDB 311 ms.

Mixed workload (8 readers of Q4 + one single-triple `INSERT DATA` every
100 ms, 5 s per point; reader qps for `/sparql` and `/api`, check_point
off → on), univ 38: snapshot 31,967 / 10,764 → **12.5 / 10.1** (6 writes
per point instead of 51: each saves the whole snapshot, 1.07–1.09 s);
Materialize 31,265 / 10,306 → 30,305 / 9,101; Stream 20,630 / 8,437 →
19,227 / 8,675; RocksDB 8,552 / 5,692 → 894 / 545. Univ 8: snapshot
29,775 / 10,265 → 42.2 / 38.1. Every point returned one answer per query
and every write changed one triple.

Updates (univ 38): single `INSERT DATA` median snapshot 0.013 ms,
Materialize 1.70 ms, Stream 1.98 ms, RocksDB 0.20 ms; the 6,478-triple
chunk 42 / 950 / 938 / 132 ms; after the round trip every layout is back at
its fresh term counts (842,657 entities: id reclamation).

Against the earlier quiet stages of this round, the same code: snapshot and
disk queries agree with R4-VS's quiet runs (geomean 10.15 against 10.48
(snapshot), 11.55 against 11.66 (Stream), 7.83 against 7.55
(Materialize), 25.00 against 24.44 (RocksDB)). Materialize's warm open
at univ 38 (1,721 ms here; 1,589 / 1,738 ms in two more samples, 2,102
cold with a calibration of 91 ms) sits inside R4-VS's spread for the same
binary (1,351–1,844 ms over five runs); it is not a gate. Snapshot Q11 at
univ 38 (1.305 ms) is above Round 3's 1.082 ms; R4-VS's and R4-SNAP's
same-session A/B pairs found no difference against the base (1.409 vs
1.407 ms; 0.98×), so this is the session-to-session spread of a 1 ms
query, and the compare against Round 3 passes.

#### Univ 82 (11,135,159 triples) and upstream lubm10M (G7)

The univ 82 data (2.04 GB of N-Triples) was generated into
`/tmp/gstore-bench-cache` after removing `target-rocks` (15 GB free before,
13 GB during the runs, ≥ 12 GB as the brief required) and **deleted after
the runs**, together with its update chunk and sidecars; the harness
deleted every database. RocksDB was not run at univ 82 (not part of G7).

| univ 82 | build s | build peak MiB | database MB | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 69.79 | 1175.4 | 926.5 | 2715.3 / 2395.9 | 1046.0 | 1245.0 | 21.94 |
| disk, Materialize (default mode) | 49.53 | 645.1 | 1405.5 | 4412.0 / 3404.1 | 734.4 | 939.5 | 16.09 |
| disk, Stream | 45.88 | 610.0 | 1405.5 | 4.8 / 0.9 | 6.4 | 380.8 | 30.18 |

The disk builds spilled 4 runs (583.6 MiB) within the 1,024 MiB budget
(peak 385 MiB, term map 310 MiB). Rows at univ 82: Q2 262, Q6 870,165, Q9
11,724, Q13 930,549, Q14 658,632; identical on the three runs, every
update round trip, concurrent body and mixed point consistent.

G7 per metric (upstream gStore 0.5.0 at lubm10M, `formal_experiment.tex`:
248.5 s build, 2.86 GB index, 23 µs/triple, 264 B/triple; its queries q8 ≈
Q1 24 ms, q1 ≈ Q2 144 ms, q10 ≈ Q3 29 ms, q4 ≈ Q4 37 ms, q13 ≈ Q14 223 ms;
ratio = ours / upstream; ≤ 10× required, ≤ 3× counts as met):

| metric | upstream | snapshot | disk (Materialize, default) | disk (Stream, opt-in) |
|---|---:|---:|---:|---:|
| build µs/triple | 23 | 6.27 (**0.27×**) | 4.45 (**0.19×**) | 4.12 (0.18×) |
| bytes/triple | 264 | 83.2 (**0.32×**) | 126.2 (**0.48×**) | 126.2 (0.48×) |
| Q1 ms | 24 | 0.013 (0.0005×) | 0.014 (0.0006×) | 0.019 (0.0008×) |
| Q2 ms | 144 | 85.26 (**0.59×**) | 71.20 (**0.49×**) | 145.0 (1.01×) |
| Q3 ms | 29 | 0.014 (0.0005×) | 0.016 (0.0006×) | 0.018 (0.0006×) |
| Q4 ms | 37 | 0.052 (0.0014×) | 0.055 (0.0015×) | 0.075 (0.002×) |
| Q14 ms | 223 | 141.3 (**0.63×**) | 165.3 (**0.74×**) | 876.1 (3.93×) |

Every metric is at or below upstream's on the snapshot layout and on the
disk layout in its default mode: **G7 met**. Caveats: a different
generator (LUBM-like, not UBA; 11.1M against 10.8M triples), different
hardware, upstream's harness floor of ~23 ms per query (our Q1/Q3/Q4 are
sub-millisecond warm medians, so those ratios mostly measure the floor),
and upstream's numbers are for its own (disk) index. Stream mode is within
10× everywhere but above 3× on Q14 (below).

**Stream at univ 82 (report, not a gate).** Open stays O(1) (0.9 ms warm,
4.8 ms cold, 6.4 MiB; G1b/G1c would hold at univ 82 too, query HWM 380.8
MiB ≤ 400), but the large-result queries grow faster than the data: Q6
984.8 ms (4.65× snapshot; 2.41× at univ 38), Q13 1,303.6 ms (5.10×;
2.25×), Q14 876.1 ms (6.20×; 2.51×), Q9 1,451 ms (1.61×). R4-VS's profile
puts Stream's large-result cost in resolving each result id through the
dictionary's `TermCache` (and `id2*` reads on a miss). At univ 82 these
results hold 0.66–0.93M rows, and their distinct terms no longer fit the
default 64 MiB `TermCache` (`DEFAULT_TERM_CACHE_BYTES`), which would
explain the super-linear growth. Not profiled at this stage (no gate
depends on it; Materialize, the default, is 1.17× snapshot on Q14). It
goes with R4-VS's follow-up (a dense id-indexed decoded-term cache or
batched `id2*` reads, `src/kvstore/disk_dict.rs` / `src/dict`) and a
cache size that scales with the dictionary.

#### WatDiv-like scale 10 (995,333 triples)

| run | build s | database MB | open first / warm ms | RSS after open MiB | query HWM MiB | L/S/F/C geomean ms |
|---|---:|---:|---:|---:|---:|---:|
| snapshot | 3.61 | 44.3 | 119.4 / 100.0 | 60.4 | 79.2 | 0.33 |
| disk, Materialize | 2.63 | 83.2 | 290.5 / 209.7 | 66.0 | 84.5 | 0.26 |
| disk, Stream | 2.78 | 83.2 | 7.0 / 1.0 | 6.3 | 43.3 | 0.41 |
| RocksDB | 3.12 | 27.2 | 92.8 / 77.0 | 35.2 | 66.6 | 1.19 |

The 20 bound templates return identical rows and hashes on the four
layouts (C3 85,286 rows, C2 2,855; the rest 1–95). The slowest are C3
(19.4–69.5 ms), C2 (18.6–38.2 ms), C1 and F3. Concurrency (F5 and S2, 1 →
8 clients): 2.50–5.73× on every layout and endpoint.

**Finding: Stream readers under a write load.** In the mixed phase (8
readers of F5, one write every 100 ms) Stream readers reach 192 / 196 qps
(`/sparql`, check_point off / on) and 240 / 255 (`/api`), against 2,550 /
2,248 / 1,687 / 1,769 on Materialize and 1,971 qps for the same query at 8
clients without writes. LUBM Q4 does not show it (Stream 20,630 qps under
writes, 19,493 without). Every acknowledged write bumps the
`DiskTriples` epoch, and the `ListCache` of long posting lists is emptied
on the next read at a new epoch (`ListCache::get`), so each 100 ms F5's
long lists are read from the B+ trees again, while the writer gate drains
the readers. Follow-up (owner of `src/kvstore/disk_triples.rs`): keep the
cached lists a commit did not touch (invalidate per `ListKey` from the
commit's delta) instead of clearing the whole cache per epoch. Not a gate
(G4's mixed bound is LUBM Q4); RocksDB readers also drop under writes
(F5 84–128 qps against 296 without).

#### Smoke baseline and thresholds

- `gstore-bench ci` against the Round-2 baseline (`0c9bdb6`), twice: PASS,
  307 gates, 0 failed, "1 new workload(s) not in the baseline" (the
  WatDiv-like section); query geomean 0.70 / 0.65 (snapshot) and 0.59 /
  0.60 (disk), build 0.85–0.95, warm open 0.73–1.06, memory 0.95–1.02,
  bytes 1.00. With the disk geomean ≥ 1.5× better and the new workload,
  mixed phase and `/begin` point ungated, the baseline was re-blessed:
  `bless --from` the first passing result (calibration 72.1 ms), commit
  `5faf215`.
- Against the new baseline: `ci` PASS three times (456 gates, 0 failed,
  nothing new), a fourth time after the size-gate change; the canary
  (`check --self-test-slowdown 3` of a result against itself) exits 1
  (8 gates failed: geomeans 3.00, builds 2.68–2.78).
- Spread on the idle machine, the largest gate ratios over the three runs:
  geomean **1.42** (WatDiv-like snapshot, run 1: all five queries of ≥ 1
  ms ran 1.20–1.55× slower in that run's processes; runs 2 and 3 0.94),
  one query 1.41, warm open 1.28, build 1.17, memory 1.07, bytes 1.00
  exactly on every subject. So the timing gates (query 3.0, geomean 1.5,
  build 2.0 / 2.5, open 2.5) and the memory gate (1.5) keep their CI
  margins unchanged — the geomean also stays pinned by `tests/gates.rs`
  (a uniform 1.3× passes, 1.6× fails) and the memory gate must pass 1.32 —
  and only the size gate is tightened, 1.25 → **1.10** (commit `85686e5`:
  `thresholds.json`, the `gates.rs` module doc, `bench/README.md`).
  `cargo fmt --check`, `cargo clippy -p gstore-bench --all-targets -D
  warnings` and `cargo test -p gstore-bench` (69 passed, 1 ignored) are
  clean; `report --check README.md CHANGELOG.md` exits 0.

#### Tests (G6), on this tree

Run after the measurements, on `85686e5` (gstore code of `769c627`):

- `scripts/conformance.sh --release --no-build`: basic, update,
  transaction, api, parser and python (49 SDK checks) all PASS.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `GSTORE_DISK_QUERY_MODE=memory cargo test --workspace`: **2,375 passed,
  0 failed, 6 ignored**; `GSTORE_DISK_QUERY_MODE=stream cargo test
  --workspace`: **2,375 passed, 0 failed, 6 ignored** (the crash suites
  `dt_bulk_crash`, `dt_disk_stream_crash`, `dt_reclaim_crash`,
  `dt_rw_crash` included).
- RocksDB (`CARGO_TARGET_DIR=target-rocks`): `cargo clippy -p gstore
  --all-targets --features rocksdb -- -D warnings` clean; `cargo test -p
  gstore --features rocksdb`: **2,202 passed, 0 failed, 6 ignored**;
  bench.yml's rocksdb-correctness (`cargo build --profile test --locked -p
  gstore --bin gbuild --features rocksdb`, `cargo test -p gstore-bench
  --features rocksdb`): 69 passed, 1 ignored.
- Feature-gated code: `cargo clippy -p gstore --all-targets --features
  "gzip tls zip readline pfn-wasm" -- -D warnings` clean; `cargo test -p
  gstore --features "gzip tls zip readline pfn-wasm"`: **2,192 passed, 0 failed, 5 ignored**. The
  per-feature matrix of ci.yml (gzip, tls, zip, readline, pfn-wasm one at
  a time) last ran on `e941a2b` (R4-merge, all green); since then only
  `rocksdb`-gated code gained `cfg(feature)` lines (`git diff e941a2b
  HEAD -- src tests`), and the combined build above compiles and tests
  every feature's code together.
- One test leaks a temporary directory: `http_api::tests::
  a_commit_that_frees_terms_copies_no_interner` leaves
  `/tmp/gstore_api_unit_commit_no_copy_<pid>_1/m.db` (20 KB) behind per
  run, although its root is registered with the module's `TMP_ROOTS`
  guard (no other test left anything, checked against a listing of `/tmp`
  taken before the runs); the four this stage's test runs left (memory, Stream, RocksDB, features) were deleted.
  Request 3 below.

#### Exit gates (plan §2), univ 38 unless stated

Measured values come from the committed `bench/results/phase6-final-*.json`
of this stage, except where a row names the quiet fragment of the same code
(`b9e8dc0` = this tree's gstore code): G4's monitoring endpoints
(`phase6-R4-SNAP.md`) and G5's churn (`phase6-R4-RECLAIM.md`), which the
reference profile does not measure.

| gate | target | measured | verdict | reason / follow-up |
|---|---|---|---|---|
| **G1a** disk build (`gbuild --disk`, bulk) | ≤ 0.04 × pre (1,627.6 s → ≤ 65.1 s); peak RSS ≤ 1.25 GiB with `--memory 1024`; `kvstore.kv` ≤ 0.75 × pre (≤ 583.8 MB) | 18.38 s / 21.06 s (the Stream / Materialize runs' builds) = **0.011× / 0.013×**; peak 398 / 400 MiB with the default 1,024 MiB budget (budget peak 352 / 384 MiB, 0 spilled runs; univ 82: 4 spills, 645 MiB); `kvstore.kv` 542,609,408 B = **0.697×** | **PASS** | |
| **G1b** disk open (Stream) | warm ≤ 0.5 s, cold ≤ 2.0 s; open(univ 38) / open(univ 8) ≤ 1.5 | warm **0.92 ms**, cold (page cache dropped) **7.5 ms**; ratio warm **1.13** (0.92 / 0.81 ms); cold 1.82 in the reference pair (7.5 / 4.1 ms) — three more pairs: univ 8 5.3 / 5.1 / 8.1 ms, univ 38 7.8 / 7.3 / 4.3 ms, so median of four **7.4 / 5.2 = 1.43**, warm medians 0.91 / 0.85 = 1.07 | **PASS** | The cold figure is one ms-scale sample of device latency per run (the univ 8 and 38 ranges overlap); open reads a fixed set of pages, as univ 82 shows (4.8 ms cold, 0.9 ms warm) |
| **G1c** disk memory (Stream) | RSS after open ≤ 120 MiB; VmHWM through Q1–Q14 ≤ 400 MiB; RSS-after-open ratio univ 38 / univ 8 ≤ 1.5 | **6.1 MiB**; **207.3 MiB**; **1.01** (6.13 / 6.05) (univ 82: 6.4 MiB, 380.8 MiB) | **PASS** | |
| **G1d** disk latency (Stream) | Q1–Q14 geomean ≤ 0.6 × pre disk and ≤ 1.5 × the snapshot layout; no query > 3 × snapshot | 11.55 ms = **0.079×** pre (146.92) and **1.14×** snapshot (10.15); worst Q14 **2.51×**, Q6 2.41×, Q13 2.25× (Materialize: 0.053×, 0.77×, worst Q14 1.16×) | **PASS** | Default disk query mode stays Materialize: R3-STREAM's own flip items (each query ≤ 2.0× Materialize: here Q14 2.15×, Q6 2.12×, Q12 2.07×, Q13 2.00×; geomean ≤ 1.4×: here 1.47×) still fail (`phase6-R4-VS.md`), and Stream's large results scale worse (univ 82, above) |
| **G2** memory / snapshot | build ≤ 0.5 × pre; RSS after open ≤ 0.9 × pre; geomean ≤ 0.9 × pre; Q4 ≤ 25 ms; Q1/Q3/Q4/Q11/Q12 univ 38 / univ 8 ≤ 2.0 | build 29.60 s = **0.385×**; RSS 489.1 MiB = **0.820×**; geomean 10.15 ms = **0.218×**; Q4 **0.049 ms**; ratios Q1 **1.03**, Q3 **1.04**, Q4 **0.97**, Q11 **5.58** (1.305 / 0.234 ms), Q12 **4.91** (0.494 / 0.101 ms) | **FAIL** (Q11, Q12 scaling) | Sub-millisecond queries whose in-memory VS-tree probes grow with the data (R4-VS: probing, the snapshot layout takes 1.27 / 0.53 ms at univ 38, where the disk layouts' store checks take 0.46 / 0.21 ms). Not a regression of Round 4: the same-session A/B pairs against `d702463` give equal times (R4-VS 1.409 vs 1.407 ms; R4-SNAP 0.98× / 0.95×), and the base's ratios were 6.02 / 4.99; on the disk layouts, which check the store instead, the ratios are 1.23 / 1.85 (Stream) and 1.30 / 1.65 (Materialize). Follow-up: R4-VS Request 4 — store checks instead of tree probes on the snapshot layout once a `Database`-side signal proves the tree exact (`vs_shrink_debt() == 0`, `src/db`); plan §8 D6 |
| **G3** RocksDB | build ≤ 0.25 × pre; open ≤ 10 s; Q1–Q14 geomean ≤ 0.8 × pre | build 19.50 s = **0.070×** (277.76 s); open **0.74 s** cold / 0.68 s warm (pre 60.8 / 59.4 s); geomean 25.00 ms = **0.080×** (311.50) | **PASS** | Q2 still scales worst (585.7 ms at univ 38, 5.49 at univ 8; R4-VS Request 6) |
| **G4** concurrency | Q4 qps 8 clients ≥ 2.5 × 1 client via `/sparql` and `/api` on memory and Stream-disk databases; `/begin` ≤ 5 ms (memory), ≤ 50 ms (disk); RSS growth at `/begin` ≤ 4 MiB (memory), ≤ 16 MiB (disk); monitoring ≤ 100 ms while a long query runs | Q4 8/1: snapshot **5.33 / 4.63**, Stream **4.06 / 4.73** (univ 8: 5.24 / 3.89, 4.35 / 4.38; Materialize 5.66 / 4.50; pre 0.78 / 0.86); `/api begin` median **0.38 ms** (max 1.13) memory, **0.44 ms** (max 0.62) Stream; RSS growth **+0.07 / +0.06 MiB**; monitoring (R4-SNAP quiet, same code, univ 38): `/status`, REST `/monitor` and `/show`, `/api show`, `/api monitor` during a long query, after a change and during a save ≤ **48.4 ms** (snapshot) / ≤ **43.4 ms** (Stream); **`/api stat` 201.6–219.8 ms** | **FAIL** (as written: `/api stat`) | `/api stat` takes a blocking 200 ms CPU sample per request by design (upstream semantics); ≤ 18 ms beyond it. Follow-up: a background CPU sampler in `src/gapi/ops_sys.rs` (R4-SNAP follow-up 2). Outside §2's bounds but measured with them: the mixed workload with `check_point` on stays below pre on the snapshot layout (univ 38 12.5 / 10.1 qps against pre's read-only 13.4 / 15.4; univ 8 42.2 / 38.1 against 76.3 / 76.5), equal to the base: every write saves the whole snapshot under the exclusive guard (1.07–1.09 s at univ 38, 2.4–2.9 s at univ 82). Follow-up: save from a captured copy-on-write image (`Database::save_image`, `db/persist.rs`; R4-SNAP follow-up 1, related to §8 D2); the disk layout meets every mixed target (Stream 19,227 / 8,675 qps with check_point on) |
| **G5** id reclamation | 10 churn rounds: live = referenced exactly, within ±5 % of round 0, high-water ≤ 1.05 × peak live; delete overhead ≤ 10 % memory and disk, ≤ 20 % RocksDB | (R4-RECLAIM quiet, `b9e8dc0`) live = referenced in all 11 rounds on memory, Stream, Materialize and RocksDB; high-water **1.000**; **+5.20 %** at round 4 (268,332 / 255,061; the data itself +5.23 %; Off 2.27×); delete Immediate / Off **1.13 / 1.12** memory (two days), Stream **0.63**, **Materialize 1.14**, RocksDB **0.98** | **FAIL** (±5 %, memory and Materialize delete overhead) | ±5 %: the referenced terms themselves move +5.2 % (universities differ in size; live / triples is 0.2480 in every round) — follow-up: equal-size universities or a per-triple bound. Memory (+12–13 %; the reclaim step itself 3.4 %, round 1 1.006): the state id reuse leaves in later rounds — follow-up: one retire / release pass per statement, a bitset free list. Materialize (+14 %, the default disk mode): each free is two B+ tree writes at the existing per-write cost — follow-up: a batched leaf-update path in `kvstore::bptree`. Its RSS and `kvstore.kv` bounds (R4-RECLAIM acceptance, not §2) also miss (1.53×, 1.47×; files only grow, §8 D9) |
| **G6** correctness | identical answer counts and row hashes on every layout and mode; full test matrix green; `conformance.sh` green; crash suites green | Q1–Q14 rows and row hashes identical on the four layouts at univ 8 and 38 and on the three at univ 82, equal to pre-Phase-6's, stable in every process; WatDiv-like answers identical on four layouts; every update round trip, concurrent body and mixed point consistent; tests above: workspace 2,375 passed in both disk modes, RocksDB 2,202, combined features 2,192, gstore-bench 69 (+ RocksDB 69), conformance 6 / 6 suites, 0 failures | **PASS** | Per-feature matrix last run on `e941a2b` (no feature-gated change since but `rocksdb`, which ran here) |
| **G7** "same order as upstream" (univ 82 vs lubm10M) | build µs/triple, bytes/triple and each comparable query ≤ 10× upstream; ≤ 3× counts as met | snapshot / disk (default Materialize): build **6.27 / 4.45 µs** (0.27× / 0.19× of 23); **83.2 / 126.2 B/triple** (0.32× / 0.48× of 264); Q1 0.013 / 0.014 ms (24), Q2 **85.3 / 71.2 ms** (0.59× / 0.49× of 144), Q3 0.014 / 0.016 (29), Q4 0.052 / 0.055 (37), Q14 **141.3 / 165.3 ms** (0.63× / 0.74× of 223) | **PASS** (every metric ≤ 1×) | Stream (opt-in) is within 10× but Q14 is 3.93× (876 ms), Q2 1.01×. Caveats in the univ 82 section |

**Summary: G1a, G1b, G1c, G1d, G3, G6 and G7 pass; G2, G4 and G5 fail** on
the items above, each recorded with its measured value, its reason and a
follow-up (plan §5.7). None of the misses is a regression of this round:
G2's Q11 / Q12 and G4's `/api stat` and snapshot check_point saves are
equal to the base, and G5's misses are costs of the reclamation design
that R4-RECLAIM's quiet stage profiled (callgrind) and recorded.

## Requests/Not yet

Requests (outside this package's files):

1. **Docs closure (the next stage, plan §6 R4-CLOSURE item 3):** fold the
   fragments into `CHANGELOG.md`, write `docs/BENCHMARKS.md` with the gate
   table above and the upstream comparison, add the README Performance
   table between `gstore-bench report` markers over
   `bench/results/phase6-final-lubm38.json` (and `-lubm82.json`), the
   GAP_PLAN §9 status table and Phase 6 paragraph, then `gstore-bench
   report --update README.md CHANGELOG.md docs/BENCHMARKS.md`; CI's
   `report --check` then guards them.
2. **Follow-ups for a later phase**, by owner (the gate table gives the
   numbers):
   - `src/db`, `src/query` (snapshot layout): G2's Q11 / Q12 scaling
     (R4-VS Request 4).
   - `src/gapi/ops_sys.rs`: `/api stat` without the blocking 200 ms
     sample (G4).
   - `src/db/persist.rs`, `src/http_api.rs`: snapshot-layout saves off
     the exclusive guard (check_point on; R4-SNAP follow-up 1).
   - `src/db/reclaim.rs`, `src/dict`, `src/kvstore/bptree.rs`: G5's
     delete overhead on memory and Materialize; the churn's ±5 % bound
     (`examples/reclaim_churn.rs`: equal-size universities or a
     per-triple bound).
   - `src/kvstore/disk_dict.rs`, `src/dict/cache.rs`: Stream's large
     results (R3-STREAM flip items; univ 82 Q6 / Q13 / Q14 at 4.6–6.2×
     the snapshot layout; a `TermCache` sized to the dictionary).
   - `src/kvstore/disk_triples.rs`: keep the `ListCache` entries a commit
     did not touch (Stream readers under writes, WatDiv-like F5 at ~0.1×
     of Materialize).
3. **`src/http_api.rs` test hygiene:**
   `http_api::tests::a_commit_that_frees_terms_copies_no_interner` leaves
   `/tmp/gstore_api_unit_commit_no_copy_<pid>_1/m.db` behind (20 KB per
   run) although `tmp_root` registers the root with `TMP_ROOTS`; the
   database directory is presumably written again after the guard removed
   the root. Make the test end with nothing left (for example shut the
   server down and drop its handle before the guard runs, then remove the
   root explicitly).

Not yet / notes:

- RocksDB was not measured at univ 82 (G7 names disk and snapshot).
- The WatDiv-like workload has no pre-Phase-6 or Round-3 reference, so
  `phase6-final-watdiv10.json` has no compare report; it is the first
  reference for that workload.
- The timing gates were not tightened (the spread above); revisit with CI
  runner data, which this machine cannot provide.
- `target-rocks/` was removed at the end of this stage, as were every
  database and the univ 82 data; `/tmp/gstore-bench-cache` (now also
  holding the WatDiv-like scale 10 data the reference runs generated) and
  the base binaries are kept.
