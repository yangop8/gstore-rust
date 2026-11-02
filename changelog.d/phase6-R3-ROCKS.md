# Phase 6 R3-ROCKS — RocksDB: SST-ingest bulk build, bounded streaming reads, exact stats

Package R3-ROCKS of `docs/PHASE6_PLAN.md` §6 (Round 3). Base: `0fb8842`
(Phase 6 Rounds 1 and 2 merged). Files: `src/backend/rocks.rs`,
`src/bulk/{mod,types,rocks_sink}.rs`, `src/bin/gbuild.rs`,
`tests/dt_rocksdb.rs`, `tests/dt_bulk_rocks.rs` (new), this fragment — and
one assertion in `tests/dt_bulk_disk.rs` (see Requests/Not yet, item 0).

## Changelog

### Phase 6 R3-ROCKS: `gbuild --rocksdb` builds in bulk; RocksDB reads stream and count exactly

- **`gbuild --rocksdb` is a bulk build** (`gstore::bulk::build` with
  `BulkTarget::RocksDb`, contract C6). The R2 pipeline (ordered parse on up
  to `--threads` parser threads, external sorts under `--memory`) feeds one
  set of `SstFileWriter`s per column family, each with that column family's
  own options and cut at 256 MiB: `spo`, `pos`, `osp`, `gspo`, `dict`
  (`tag ‖ be32(id) → key`) and every statistics counter, exact (`T`/`P`/`S`/
  `O`, `c|`/`u|`/`v|p`, `s|s`, `o|o`, `x|p|s` through a `(p, s, count)`
  re-sort, `y|p|o`; zero counters absent). A fresh store in
  `<db>/.bulk-tmp/rocksdb` ingests them (`ingest_external_file_cf` with
  `move_files`; the files of one column family never overlap, so they land
  in the bottom level), the dictionary first; the `meta` name and then
  `layout = "1"` are written last; flush, WAL sync, close, reopen and
  check the counters, `fsync` every file. The quads are encoded exactly as
  the one-write-per-triple build encodes them (subject, predicate, object,
  then the graph, first seen first; only the id-space bounds are checked —
  RocksDB has none of the disk layout's key limits, so a 5000-byte literal
  builds), so **every key and value of every column family is byte-equal to
  `Database::build_rocksdb_*`'s** (tested on LUBM + N-Quads + blank nodes +
  a long literal, at 1 and 4 threads, and through `gbuild` with many small
  SST files). Layout `"1"`, the column family set and all encodings are
  unchanged: an older binary opens a bulk-built store (checked by hand at
  univ 1, see Measured numbers).
  - New flags for `--rocksdb`: `--memory MB`, `--threads N`, `--tmp-dir DIR`
    (as for `--disk`; `--fill` stays a `--disk` knob and is refused) and
    `--incremental` (the previous path, which reopens the store to print
    its counts). The bulk build prints the counts and its phase / memory /
    spill summary (`rocksdb/ … MiB` instead of the disk files).
  - `Database::build_rocksdb_*` (library helpers, used by tests and the
    server) stay on the per-triple path (plan D15).
- **Crash-safe install** (`bulk::ROCKS_CRASH_POINTS`): the build holds the
  directory's `DirLock` (another process holding the directory is refused)
  and the in-process `BuildGuard`, refuses a directory whose RocksDB store
  is open anywhere (Linux: its `rocksdb/LOCK` in `/proc/locks`, the R2
  check) or that a database of this process holds, finishes a swap a crash
  interrupted, and installs with `fsutil::install_staged_dir` (`staged` →
  `rocksdb.new` → `rocksdb/`; the rename into place is the commit point),
  then removes the replaced database's reasoning rules, the other layouts
  (snapshot files, `kvstore.kv` + `vstree.kv`) and the staging area. A crash
  at `staged` leaves the old database; from `swapped` on, the new one
  (tested at every step, over a RocksDB and over a disk-layout database).
  A failed build (strict parse error, store in use) leaves the old database
  untouched. `GSTORE_BULK_CRASH=<step>` and `bulk::debug_crash_at` cover the
  RocksDB steps too.
- **RocksDB reads are bounded raw-iterator scans** (`src/backend/rocks.rs`):
  `iterate_upper_bound` = the prefix's successor, keys decoded in place (no
  `Box<[u8]>` per key); an iterator status error or a failed point read is
  reported through `note_read_error` (contract C2), so the statement fails
  instead of answering from a shortened list (previously a silent `break`).
  Writes that cannot read (the existence check, a counter) now fail the
  write instead of treating the key as absent — which could have
  double-counted a triple.
- **Exact C1 counts** from the maintained counters, one point read each:
  `count_po_by_s` = `s|s`, `count_ps_by_o` = `o|o`, `count_o_by_sp` =
  `x|p|s`, `count_s_by_po` = `y|p|o`, `count_so_by_p` = `c|p`;
  `has_subject` / `has_object` read `s|` / `o|` (true on a read error),
  `has_predicate` the mirror; `count_p_by_so` counts OSP keys without
  decoding; `distinct_p_of_s` seeks past long predicate runs;
  `distinct_p_of_o` scans OSP for a small object and reads `y|p|o` per
  predicate for a large one. All equal the C1 defaults exactly (randomized
  test against the defaults and against `TripleStore`).
- **In-memory mirror of `c|`/`u|`/`v|`**: loaded at open (and after the
  journal replay), updated only after a successful write batch, reloaded
  after every journal replay (rollback, heal, open). `pred_card`,
  `pred_distinct_subj`, `pred_distinct_obj`, `predicates()` and
  `has_predicate` no longer touch RocksDB.
- **Contract order, mostly without sorts**: `subs_by_p` / `objs_by_p` read
  the `x|p` / `y|p` keys (no POS scan + sort + dedup); `subject_keys` /
  `object_keys` drop their sort; `iter_all` now returns the contract's
  `(p, s, o)` order (it returned SPO order before), placing every SPO
  triple straight into its predicate's slice sized by `c|p`. `so_by_p`
  walks the predicate's `x|p` subjects with one SPO seek each when its
  subjects have long object runs (`c|p ≥ 32 × u|p`), and otherwise reads
  its POS range and sorts it: measured at univ 8, the seek walk for every
  predicate was slower (Q1–Q14 geomean 11.44 vs 10.61 ms, Q12 0.42 vs
  0.19 ms) — a seek costs as much as reading tens of keys and LUBM's
  predicates have 1–3 objects per subject. (`ps_by_o` still sorts the
  object's own in-degree-sized list.)
- **`RocksTuning`** (options only, no format change): one LRU block cache
  shared by every column family and every store of the process with the
  same capacity (default 256 MiB, environment `GSTORE_ROCKS_BLOCK_CACHE_MB`,
  `0` = RocksDB's per-CF default), and whole-key bloom filters (10 bits per
  key) on the column families read by point lookups (`spo`, `stats`,
  `gspo`). `RocksStore::open(path)` is unchanged (it uses
  `RocksTuning::default()`); `RocksStore::open_with(path, &tuning)` is new.
  Files written with or without filters open either way.
- **Diagnostics** (doc-hidden, for tests and tools): `RocksStore::check_stats`
  (every counter recounted from the orderings, POS/OSP = SPO, mirror and
  cached globals = the `stats` CF), `debug_dump_cf`, `debug_fail_reads`.
- `bulk::{BulkInput, BulkTarget, BulkOptions, BulkReport, PhaseTime}` and
  the defaults moved to `src/bulk/types.rs` (re-exported unchanged);
  `BulkReport::summary_for(target)`.

## GAP_PLAN status line

Phase 6 R3-ROCKS (RocksDB): done — `gbuild --rocksdb` bulk-builds by SST
ingest (layout `"1"`, byte-equal to the per-triple build, crash-safe
install); RocksDB reads are bounded raw-iterator scans with exact C1 counts,
a `c|/u|/v|` mirror, contract order and per-query read errors; shared block
cache + bloom filters. Quiet post-merge stage, univ 38: build 22 s (was
175 s; pre 278 s), 402 MiB peak, 156 MB (0.76× pre), open 0.9 s, Q1–Q14
geomean 0.21× pre / 0.26× the base (univ 8: 4.2 s, 0.24× pre).

## Docs notes

- **README.md** (the `gbuild` section): `--rocksdb` builds in bulk; it
  takes `--memory`, `--threads`, `--tmp-dir` and `--incremental` (not
  `--fill`); the printed summary ends with `rocksdb/ N MiB`.
- **docs/STORAGE_BACKEND.md** (RocksDB):
  - The bulk build: SST files per column family from the sorted streams,
    ingested (moved) into a fresh store in `.bulk-tmp/rocksdb`, installed
    with the `rocksdb.new` swap; the crash table of `bulk::ROCKS_CRASH_POINTS`;
    keys and values are those of the per-triple build (layout `"1"`; old
    binaries open it); terms of any length.
  - Reads: bounded raw iterators; a RocksDB read error fails the statement
    (C2) instead of truncating a list; C1 counts from the counters; the
    `c|/u|/v|` mirror; `iter_all` in `(p, s, o)` order.
  - Tuning: the shared LRU block cache (`GSTORE_ROCKS_BLOCK_CACHE_MB`,
    default 256; `0` = RocksDB's per-column-family 32 MiB caches) and the
    bloom filters on `spo` / `stats` / `gspo` (options only; R4 adds config
    keys).
  - The library helpers `Database::build_rocksdb_*` stay per-triple
    small-data helpers (D15).
- **docs/DEPLOY.md**: `GSTORE_ROCKS_BLOCK_CACHE_MB` (memory a RocksDB
  server may use for its block cache, shared by all its RocksDB databases).
- **CHANGELOG.md**: fold the Changelog section above.

## Measured numbers

### Quiet post-merge stage (phase6 `e1c7131`: R3 merged, review fixes)

Reference VM (4 vCPU Intel Xeon @ 2.10GHz, 16 GB), nothing else running:
CPU busy 24–37% while measuring (the one measured process; logged every 10
s from `/proc/stat`), steal ≤ 2%, load average ≤ 1.3. Branch = release
build of `e1c7131` with `--features rocksdb` (`target-rocks`); base = the
prebuilt release binaries of `0fb8842` with `--features rocksdb`
(`/tmp/gstore-base-0fb8842/bin-rocks`: per-triple `gbuild --rocksdb`, old
read code in its harness), measured in the same session, each harness with
its own `--bin-dir` (the base results' `git_commit` field names the working
tree, `e1c7131`; their `config.bin_dir` names the base binaries). Every
heavy command under `flock /tmp/gstore-bench.lock`; data from
`/tmp/gstore-bench-cache` (univ 8 `ed44da2e7b3bbb98`, 1,029,275 triples;
univ 38 `fcd6493725f3c66a`, 5,181,103 triples). `gstore-bench run --profile
reference --scale {8,38} --backends rocksdb --no-conc --drop-caches
--verify-data`, in the order base 8, branch 8, branch 8 `--gbuild-arg
--incremental`, base 38, branch 38, branch 38 `--incremental` (18:34–18:52
UTC, 18 min). The two branch runs feed the RocksDB rows of the round's
result files (a later part of this measure stage joins them).

| univ 8 | build s | gbuild peak MiB | `rocksdb/` bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| pre-Phase-6 | 53.93 | 285.5 | 41,162,410 | 9959 / 9989 | 145.8 | 184.9 | 43.27 |
| R2 quiet (`0c9bdb6`) | 35.60 | 261.5 | 41,140,275 | 202 / 180 | 64.5 | 112.5 | 33.28 |
| base `0fb8842` | 32.30 | 261.5 | 41,141,324 | 262 / 205 | 64.3 | 113.8 | 32.90 |
| **`e1c7131` bulk** | **4.21** | **93.4** | **31,266,467** | 255 / **184** | 68.3 | 102.6 | **10.56** |
| `e1c7131` `--incremental` | 29.89 | 276.8 | 44,017,197 | 246 / 214 | 68.2 | 102.4 | 10.82 |

| univ 38 | build s | gbuild peak MiB | `rocksdb/` bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| pre-Phase-6 | 277.76 | 634.2 | 206,253,032 | 60811 / 59389 | 522.0 | 663.3 | 311.50 |
| R2 quiet (`0c9bdb6`) | 164.35 | 510.6 | 206,221,265 | 1444 / 937 | 199.8 | 386.4 | 262.79 |
| base `0fb8842` | 174.70 | 548.9 | 206,232,710 | 939 / 970 | 199.7 | 387.0 | 261.58 |
| **`e1c7131` bulk** | **22.25** | **401.6** | **155,868,540** | 1006 / **901** | 276.6 | 440.2 | **66.91** |
| `e1c7131` `--incremental` | 162.76 | 653.3 | 220,659,064 | 937 / 874 | 276.4 | 440.5 | 71.31 |

| query (univ 38) | rows | pre ms | R2 ms | base ms | bulk ms | `--incremental` ms |
|---|---:|---:|---:|---:|---:|---:|
| Q1 | 3 | 60.72 | 62.72 | 64.24 | 9.43 | 10.41 |
| Q2 | 115 | 729.49 | 1152.14 | 1090.01 | 589.62 | 715.02 |
| Q3 | 10 | 195.54 | 185.81 | 182.92 | 30.74 | 34.56 |
| Q4 | 9 | 989.95 | 438.05 | 437.28 | 1.53 | 1.81 |
| Q5 | 576 | 537.67 | 465.94 | 397.01 | 355.15 | 355.57 |
| Q6 | 404,399 | 468.29 | 346.18 | 324.34 | 239.14 | 229.17 |
| Q7 | 23 | 977.60 | 763.28 | 751.73 | 379.34 | 385.56 |
| Q8 | 10,778 | 904.88 | 733.13 | 645.73 | 403.36 | 409.99 |
| Q9 | 5,507 | 2125.90 | 1900.32 | 1911.56 | 740.81 | 875.07 |
| Q10 | 3 | 594.89 | 436.99 | 485.77 | 381.82 | 350.61 |
| Q11 | 325 | 16.79 | 16.99 | 17.28 | 2.15 | 2.37 |
| Q12 | 21 | 10.19 | 10.85 | 11.61 | 0.31 | 0.39 |
| Q13 | 432,512 | 511.89 | 347.93 | 373.68 | 266.48 | 268.39 |
| Q14 | 305,503 | 382.71 | 264.89 | 288.15 | 208.40 | 191.24 |

Univ 8 per query (warm median ms, base → bulk): Q1 9.68 → 1.89, Q2 37.15 →
5.80, Q3 29.37 → 6.02, Q4 59.39 → 0.47, Q5 51.71 → 38.84, Q6 48.89 → 39.45,
Q7 102.37 → 46.14, Q8 133.83 → 93.06, Q9 212.94 → 106.81, Q10 48.76 →
38.30, Q11 2.49 → 1.04, Q12 1.41 → 0.19, Q13 53.25 → 42.35, Q14 41.23 →
30.65. Every query is faster than the base and than pre-Phase-6 at both
scales.

`gstore-bench compare … --backends rocksdb` (ratios as `compare` prints
them: times calibration-normalized, memory as `(m+32)/(b+32)`):

- against `pre-phase6-lubm{8,38}.json`: 97 gates, **1 failed, `exact.phase
  rocksdb/conc`** (the pre file ran the concurrency phase; the brief's runs
  use `--no-conc`); every other exact gate passes (triple count, rows and
  row hashes of all 14 queries, the update round trip). Univ 38: geomean
  ratio **0.21**, build 0.08, warm open 0.01, `rocksdb/` bytes 0.76, build
  peak 0.65, RSS after open 0.56, query HWM 0.68. Univ 8: geomean 0.24,
  build 0.08, open 0.02.
- against R2's quiet `phase6-r2-lubm{8,38}.json`: the same single `conc`
  failure. Univ 38: geomean 0.29, build 0.16, open 1.11, RSS after open
  1.33, query HWM 1.13. Univ 8: geomean 0.32, build 0.12, open 1.03.
- against the same-session base: **PASS, 96 gates** at both scales. Univ
  38: geomean 0.24, build 0.12, open 0.87, bytes 0.76, build peak 0.75, RSS
  after open 1.33, query HWM 1.13. Univ 8: geomean 0.30, build 0.13, open
  0.87, build peak 0.43.
- bulk against `--incremental` (same binary, same session): every exact
  gate passes at both scales (identical rows and row hashes); only the
  incremental build's own gates fail (build 6.40× / 7.54×, peak 2.46× /
  1.58×, bytes 1.41× / 1.42× at univ 8 / 38: the per-triple path, with
  bloom filters written by compaction). Query geomean of the incrementally
  built store 0.94× (univ 8) and 1.09× (univ 38) the bulk-built one's, warm
  open 1.06× / 1.00×: the layout is not what made queries faster.

Update phase (report-only), univ 38, base → bulk: single insert / delete
median 0.24 / 0.20 → 0.22 / 0.21 ms, 6,478-triple chunk insert 189.6 →
153.3 ms, chunk delete 135.7 → **191.7 ms** (the incrementally built store
133.3 ms; univ 8: 149.8 → 103.8 ms; one sample each, not investigated
further), checkpoint 30.8 → 37.2 ms, reopen 870.6 → 730.3 ms. Every round
trip ok (singles, chunk delta, answers unchanged, reopen count).

`gbuild --rocksdb` directly (same session, `flock`; wall from the shell):

| build | wall s | phases (gbuild's summary) | budget peak | term map | `rocksdb/` bytes |
|---|---:|---|---:|---:|---:|
| univ 38, bulk | 21.79 | parse+encode 18532 ms (85%), spo 1726, index 1410 (pos 1046, osp 975, x-counts 910, dictionary 230, in parallel), ingest 89, install 1 | 314.7 MiB of 1024 | 155.0 MiB | 155,868,508 |
| univ 8, bulk | 4.54 | parse+encode 3875 ms (85%), spo 368, index 246, ingest 47, install 1 | 60.0 MiB of 1024 | 24.1 MiB | 31,266,380 |
| univ 8, base per-triple | 31.85 | — | — | — | 41,140,606 |

0 spills at either scale. Everything RocksDB-specific (SST writes, the four
parallel index tasks, ingest, the counter check) takes about 3.2 s at univ
38; the single Turtle parser thread is the rest (R2-BULK request 3).

**Old binary on a bulk-built store** (univ 8, by hand): this branch's
`gbuild --rocksdb` built `n8.db`, the base's built `o8.db` (per-triple).
For each of `testdata/lubm/lubm_q{1..14}.rq`, the base `gquery`
(`/tmp/gstore-base-0fb8842/bin-rocks/gquery`) printed on `n8` exactly what
it printed on `o8` (**14/14 byte-identical, same order**, timing stripped;
5 to 85,672 lines), and this branch's `gquery` printed the same on both
stores.

**RSS after open** (not an acceptance item): 276.6 MiB at univ 38 against
the base's 199.7 MiB (+77 MiB, 1.38×; the memory gate's `(m+32)/(b+32)` is
1.33 ≤ 1.5; 0.53× pre), the same on the incrementally built store (276.4),
+4 MiB at univ 8. `gquery` with a query that matches nothing, on the univ 38
bulk store, peak RSS: 314 MiB with the default 256 MiB shared block cache,
252 MiB with `GSTORE_ROCKS_BLOCK_CACHE_MB=32` or `0`, base `gquery` 237
MiB. So about 62 MiB is blocks read during open that stay in the larger
shared cache (`RocksTuning`, as designed; a config key is request 4), and
about 15 MiB the bloom filters and the rest.

**Acceptance (R3-ROCKS), quiet:**
- Build ≤ 10 s at univ 8: **met** — 4.21 s (0.078× pre 53.93 s, 0.13× the
  same-session base 32.30 s); 4.54 s direct.
- Build ≤ 45 s at univ 38: **met** — 22.25 s (21.79 s direct; 0.080× pre
  277.76 s, 0.13× the base 174.70 s).
- Peak RSS ≤ 1.25 GiB at univ 38: **met** — 401.6 MiB (budget peak 314.7
  MiB + term map 155.0 MiB; no spills).
- On-disk size ≤ 1.1 × pre: **met** — 0.756× at univ 38 (155,868,540 vs
  206,253,032 bytes), 0.760× at univ 8 (31,266,467 vs 41,162,410).
- Q1–Q14 geomean ≤ 0.8 × pre: **met** — univ 38 66.91 vs 311.50 ms
  (**0.215×**; compare 0.21), univ 8 10.56 vs 43.27 ms (0.244×); 0.256× /
  0.321× the same-session base.
- Open ≤ 10 s at univ 38: **met** — warm 0.90 s, cold first 1.01 s (pre
  59.4 / 60.8 s; base 0.97 / 0.94 s).
- Identical answers: **met** — rows and row hashes of Q1–Q14 equal to
  pre-Phase-6 (its RocksDB and snapshot rows) and to R2's snapshot and disk
  rows at both scales, in every one of the 5 query processes and after the
  update round trip; the bulk-built and the incrementally built store
  answer identically at both scales; the base `gquery` answers identically
  on a bulk-built univ 8 store.
- Stats exactness and the bulk build's DirLock: **met** — `cargo test
  --release -p gstore --features rocksdb --test dt_rocksdb --test
  dt_bulk_rocks --test dt_bulk_crash` and the `rocks` lib tests pass at
  `e1c7131` (see the verification line below); the update phase's exact
  checks pass on bulk-built stores at both scales.

**G3 (RocksDB), univ 38: met** — build 22.25 s ≤ 0.25 × pre (69.4 s; the
plan's ≈ 50 s); open 0.90 s warm ≤ 10 s; Q1–Q14 geomean 0.215× pre ≤ 0.8×.
G3 holds without a VS-tree at open: Q2 (589.6 ms, the slowest-scaling query:
5.80 ms at univ 8, ×102 for 5× the data; base ×29) is still 0.54× the base
and 0.81× pre.

Verification at `e1c7131` (this stage, release profile in `target-rocks`):
`cargo test --release --locked -p gstore --features rocksdb --test
dt_rocksdb --test dt_bulk_rocks --test dt_bulk_crash`: 10 + 9 + 9 passed, 0
failed; `cargo test … --lib -- rocks` (the `backend::rocks` and
`bulk::rocks_sink` unit tests): 16 passed, 0 failed. No code changed at
this stage; every database it built was deleted.

### Implementer run

**Implementer run, noisy (other agents compiling).** 4 vCPU / 16 GB VM,
load average 3.7–8.6 at the starts of the runs; every heavy command under
`flock /tmp/gstore-bench.lock`; base and branch measured in the same
session, alternated (base, branch, branch `--incremental`, branch with the
all-seek `so_by_p`, base again, final branch). Base = the prebuilt release
binaries of `0fb8842` with `--features rocksdb`
(`/tmp/gstore-base-0fb8842/bin-rocks`, `gbuild --rocksdb` = the per-triple
build); branch = release build of this branch with `--features rocksdb`
(final = `fa5a48e`, later commits only touch this fragment). Data:
`lubm-like1-u8-s0.nt` from `/tmp/gstore-bench-cache` (1,064,650 lines,
1,029,275 unique triples, data fnv `ed44da2e7b3bbb98`). **univ 38 was not
measured** (left to the quiet measure stage; commands below).

`gstore-bench run --profile reference --scale 8 --backends rocksdb
--no-conc` (each harness with its own `--bin-dir`):

| run | build s | gbuild peak MiB | `rocksdb/` MB | open first / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| base `0fb8842` (1st) | 35.42 | 263.1 | 41.1 | 314 / 354 | 66.5 | 114.1 | 33.74 |
| base `0fb8842` (2nd) | 33.86 | 262.2 | 41.1 | 188 / 253 | 66.3 | 113.7 | 36.40 |
| branch, bulk, all-seek `so_by_p` | 4.79 | 95.1 | 31.3 | 242 / 191 | 69.0 | 101.6 | 11.44 |
| branch, bulk, POS + sort `so_by_p` | 4.99 | 93.9 | 31.3 | 165 / 172 | 68.8 | 103.1 | 10.61 |
| **branch, bulk, final (adaptive)** | **5.75** | **93.3** | **31.3** | 272 / 280 | 69.0 | 103.4 | **10.97** |
| branch, `--incremental` | 31.07 | 277.9 | 44.0 | 174 / 232 | 68.9 | 101.6 | 11.61 |

Per query, warm median ms, base (2nd) → final: Q1 9.51 → 1.81, Q2 35.33 →
5.89, Q3 29.16 → 6.42, Q4 59.33 → 0.49, Q5 59.32 → 44.46, Q6 51.28 →
40.90, Q7 108.48 → 49.30, Q8 202.84 → 94.95, Q9 229.86 → 108.73, Q10 57.43
→ 41.44, Q11 2.64 → 1.11, Q12 1.50 → 0.19, Q13 56.50 → 44.09, Q14 62.14 →
30.58 — every query faster. Rows and row hashes of all 14 queries equal to
the base (`gstore-bench compare base-2nd final --backends rocksdb`: **PASS,
96 gates**, geomean ratio 0.30, build 0.17). Against
`bench/results/pre-phase6-lubm8.json`: geomean ratio **0.24** (raw 10.97
vs 43.27 ms), build 0.10 (53.93 s pre), open 0.03 (pre rebuilt the VS-tree
at open), `rocksdb/` bytes **0.76×** (31,266,468 vs 41,162,410); every exact
gate passes but one, `exact.phase rocksdb/conc` (the pre file ran the
concurrency phase; these runs used `--no-conc` as the brief says). The
bulk-built and the incrementally built store query alike (compare final vs
`--incremental`: PASS, geomean ratio 1.06 — within this machine's noise):
**the query speed-up comes from the read path, not from the layout.** The
`--incremental` store is 7% larger than the base's (44.0 vs 41.1 MB): the
bloom filters it writes during compaction; the bulk store has them too and
is still 0.76×.

Update phase (report-only), base (2nd) → final: single insert median /
p95 2.86 / 5.03 → 4.00 / 5.92 ms (the first branch run: 0.22 / 0.38 — this
number swings with the machine's fsync latency), single delete 0.24 / 4.00
→ 0.16 / 0.26 ms, 5,330-triple chunk insert 147.6 → 121.3 ms, chunk delete
116.8 → 121.4 ms, checkpoint 114.9 → 20.7 ms, reopen 338.9 → 253.9 ms.

`gbuild --rocksdb` directly (same session, `flock`; wall from the shell):

| build | wall s | phases (gbuild's summary) | budget peak | term map | `rocksdb/` bytes |
|---|---:|---|---:|---:|---:|
| branch, bulk | 6.12 | parse+encode 4610 ms (75%), spo 475, index 617 (pos 377, osp 353, x-counts 213, dictionary 172, in parallel), ingest 375, install 22 | 60.0 MiB of 1024 | 24.1 MiB | 31,266,380 |
| base, per-triple | 37.24 | — | — | — | 41,139,404 |

Parsing is three quarters of the bulk build (the single Turtle parser
thread per source, R2-BULK's request 3 to the parser owner); everything
RocksDB-specific — the SST writes, the four parallel index tasks, the
ingest and the counter check — takes 1.5 s at univ 8.

**Old binary on a bulk-built store** (by hand, univ 1,
`lubm-like1-u1-s0.nt`, 139,905 triples): this branch's `gbuild --rocksdb`
built `n1.db`, the base's `gbuild --rocksdb` built `o1.db`; for each of
`testdata/lubm/lubm_q{1..14}.rq`, the base `gquery` (`/tmp/gstore-base-
0fb8842/bin-rocks/gquery`) on `n1` printed exactly what it printed on `o1`
(**14/14 identical**, same order), and this branch's `gquery` on `n1` too
(14/14 identical in order). The test side proves the stronger property for
this binary: every column family of a bulk-built store is byte-equal to
the per-triple build's (`dt_bulk_rocks`).

**Acceptance (R3-ROCKS):**
- Build ≤ 10 s at univ 8 — **met**: 4.79–5.75 s in gstore-bench, 6.12 s
  direct (0.17× the base, 0.10× pre). ≤ 45 s at univ 38 — **left to the
  measure stage** (R2's disk bulk build, the same parse front end, took
  19.8 s at univ 38; the RocksDB-specific part is 1.5 s at univ 8).
- Peak RSS ≤ 1.25 GiB at univ 38 — **left to the measure stage** (univ 8:
  93–95 MiB; budget peak 60 MiB + term map 24 MiB, both linear).
- On-disk size ≤ 1.1 × pre — **met at univ 8**: 0.76× (31.3 vs 41.2 MB);
  univ 38 left to the measure stage (pre 206,253,032 bytes).
- Q1–Q14 geomean ≤ 0.8 × pre — **met at univ 8**: 0.24× pre (0.30× the
  same-session base); univ 38 left to the measure stage (pre 311.50 ms;
  R2's measure stage had 262.79 ms).
- Open ≤ 10 s at univ 38 — **left to the measure stage** (univ 8 warm
  0.17–0.28 s; R2 measured 0.94 s warm at univ 38 before this change).
- Identical answers — **met**: rows and row hashes of Q1–Q14 equal to the
  base and to pre-Phase-6 (compare exact gates), the old binary answers
  identically on a bulk-built store, and the tests compare every column
  family byte for byte and every read in order.

**Commands for the quiet measure stage (univ 38):**

```sh
CARGO_TARGET_DIR=target-rocks cargo build --release --locked -p gstore -p gstore-bench --bins \
    --features gstore/rocksdb,gstore-bench/rocksdb
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
flock /tmp/gstore-bench.lock target-rocks/release/gstore-bench run --profile reference --scale 38 \
    --backends rocksdb --drop-caches --bin-dir target-rocks/release --out /tmp/r3rocks-u38.json
target-rocks/release/gstore-bench compare bench/results/pre-phase6-lubm38.json /tmp/r3rocks-u38.json \
    --backends rocksdb --markdown
# optional: the per-triple path with the new read code (about 3 min of build)
flock /tmp/gstore-bench.lock target-rocks/release/gstore-bench run --profile reference --scale 38 \
    --backends rocksdb --no-conc --bin-dir target-rocks/release --gbuild-arg --incremental \
    --out /tmp/r3rocks-u38-incr.json
# gbuild's own summary (phases, budget peak, term map, rocksdb/ size):
mkdir -p /tmp/r3r38 && cd /tmp/r3r38 && flock /tmp/gstore-bench.lock \
    "$OLDPWD/target-rocks/release/gbuild" --rocksdb u38 /tmp/gstore-bench-cache/lubm-like1-u38-s0.nt
du -sb /tmp/r3r38/u38.db/rocksdb; cd "$OLDPWD"; rm -rf /tmp/r3r38
```

G3 reads off the first run: build wall (≤ 45 s; plan G3 ≤ 50 s), gbuild
peak RSS (≤ 1.25 GiB), `rocksdb/` bytes against pre's 206,253,032 (≤
1.1×), warm open (≤ 10 s), Q1–Q14 geomean against pre's 311.50 ms (≤
0.8×), and the exact gates.

Tests (optimized test profile, this machine): `tests/dt_bulk_rocks.rs` 7
tests ~13 s, `tests/dt_rocksdb.rs` 10 tests ~1 s (4 new), the
`rocks_sink` and `rocks` unit tests.

Verification (`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
CARGO_INCREMENTAL=0`, toolchain 1.99.1): `cargo fmt --all -- --check`;
`cargo build --workspace --all-targets` and `cargo clippy --workspace
--all-targets -- -D warnings` clean; `cargo test --workspace
--no-fail-fast`: 2,174 passed, 0 failed (run at `be7e47f`; the later
commits change only `rocksdb`-gated code and this fragment);
`CARGO_TARGET_DIR=/tmp/p6r3-target-rocks cargo clippy -p gstore
--all-targets --features rocksdb -- -D warnings` clean and `cargo test -p
gstore --features rocksdb --no-fail-fast`: 2,011 passed, 0 failed, 2
ignored (at `fa5a48e`). No other feature-gated code was touched.

## Requests/Not yet

0. **Edit outside the ownership list (integrator, please accept or redo):**
   `tests/dt_bulk_disk.rs` (R2-BULK's test; no Round-3 owner),
   `unsupported_target_and_bad_options` asserted that `BulkTarget::RocksDb`
   fails — the behaviour this brief replaces. With `--features rocksdb` the
   target now builds, so that one assertion now runs only without the
   feature (`if !cfg!(feature = "rocksdb") { … }`; the RocksDB target is
   tested in `dt_bulk_rocks.rs`). Nothing else in the file changed.
1. **R3-STREAM (`src/db/persist.rs`) / R3-SRV (server) — DirLock for the
   RocksDB layout** (the R2-review request, still open): `Database::open`
   / `open_rocksdb` and `create_rocksdb` should take the directory's
   `DirLock` for the RocksDB layout too, and the server's `hold_dir_lock`
   calls should cover RocksDB databases. The bulk RocksDB build takes the
   `DirLock` (another process holding the directory is refused), refuses a
   store open anywhere through `rocksdb/LOCK` in `/proc/locks` (Linux) and
   a directory a database of this process holds; but a process that opens
   the RocksDB database *after* the build's check is not excluded until
   `Database::open` takes the lock. Once it does, the two `/proc/locks`
   readers (`bulk/commit.rs`, R2's file, and `bulk/rocks_sink.rs`, a copy)
   become a second line of defence and can share one function.
2. **R3-STREAM (`src/db/persist.rs`, `rocks_layout`)**: use
   `backend::rocks::{META_NAME_KEY, META_LAYOUT_KEY, LAYOUT_VERSION}`
   instead of its own byte-identical `NAME_KEY` / `LAYOUT_KEY` /
   `LAYOUT_VERSION`; document `Database::build_rocksdb_*` as the per-triple
   small-data helpers (D15).
3. **Integrator / R3-SRV (`src/gapi`)**: `/api build` with the RocksDB
   layout should call `bulk::build(…, BulkTarget::RocksDb, …)` (plan D15:
   gbuild and `/api build` use the bulk path), after unloading the
   database (a bulk build refuses a directory in use).
4. **R4 (config)**: a `[storage]` key for the block cache feeding
   `RocksTuning` (today `RocksTuning::default()` reads
   `GSTORE_ROCKS_BLOCK_CACHE_MB`; `open_rocksdb` keeps calling
   `RocksStore::open`).
5. **R4-CLOSURE (`bench/**`)**: gstore-bench could record gbuild's summary
   (R2-BULK request 4) — for RocksDB too (phases, budget peak, term map).

Not yet:
- univ 38 numbers (build, peak RSS, size, open, geomean) — the measure
  stage (done; see below).
- The 256 MiB SST cut is not reached below roughly 20M triples per column
  family; the multi-file ingest is tested through the doc-hidden
  `GSTORE_BULK_SST_SPLIT_BYTES` hook (`gbuild_rocksdb_uses_the_bulk_build`
  cuts many small files and compares every column family).
- `ps_by_o` still sorts the object's own list (bounded by its in-degree);
  `count_p_by_so` counts OSP keys (no counter covers `(s, o)`).
- A RocksDB database still opens without a VS-tree (R1-CORE's note); at
  univ 8 Q2 is now 5.9 ms (base 35 ms), so the univ 38 Q2 of the measure
  stage decides whether `vstree_on_open` matters for G3.

From the quiet post-merge stage (`e1c7131`; numbers under Measured numbers):

- univ 38 measured: every acceptance item and G3 met (build 22.25 s, peak
  401.6 MiB, 0.756× pre bytes, open 0.90 s warm, geomean 0.215× pre).
  `vstree_on_open` is not needed for G3. Q2 is the query that scales worst
  on RocksDB (5.80 → 589.6 ms from univ 8 to 38, ×102 for 5× the data; the
  base ×29), still 0.54× the base and 0.81× pre: a candidate for R4-CLOSURE
  to look at.
- Parsing is 85% of the bulk build at both scales (18.5 of 21.8 s at univ
  38); R2-BULK request 3 (parser owner) stands.
- RSS after open is 77 MiB above the base at univ 38 (276.6 vs 199.7 MiB,
  inside the memory gate): mostly blocks read during open kept by the 256
  MiB shared block cache (about 62 MiB; 252 MiB peak with
  `GSTORE_ROCKS_BLOCK_CACHE_MB=32`). Request 4's `[storage]` key lets a
  deployment size it.
- Chunk delete on the bulk-built store at univ 38: 191.7 ms against the
  base's 135.7 ms (one sample, report-only; 133.3 ms on the incrementally
  built store, and faster than the base at univ 8). G5 (R4-RECLAIM)
  measures delete overhead.
