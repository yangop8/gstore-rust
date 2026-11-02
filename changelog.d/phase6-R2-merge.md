# Phase 6 R2 merge — integration of R2-DISKLIB, R2-DICT and R2-BULK

Merged in plan order on `phase6` from `aaea9f8`: `p6/r2-disklib` (174898d),
`p6/r2-dict` (33305d8), `p6/r2-bulk` (0969a2a), each with `git merge
--no-ff`. All three merged without conflicts and built at every merge
point; every changed file is inside its package's §6 file list and the §7
ownership table (no ownership exceptions were reported or needed). The
commits after the merges are the integrator's own, listed below.

## Changelog

- **The legacy `&str` dictionary API fails closed on an out-of-core
  dictionary (contract C4).** `Dictionary::id_to_string` /
  `predicate_to_string` on a backed dictionary now answer `None` and record a
  read error for every id (C2), and never read the backend. R2-DICT had kept a
  bounded per-dictionary pin table (up to 4,096 strings) only because a test in
  `src/kvstore/store.rs`, owned by R2-DISKLIB this round, expected `Some` there;
  that test now uses `term()` / `pred()`, and the pin table and
  `CacheStats::legacy_pinned` are gone. In-memory dictionaries are unchanged.
  (Corrected by `phase6-R2-review.md`: the graph functions on `DiskStore`'s
  out-of-core query path still called the legacy API and failed there; they,
  and PFN's `GraphCtx`, now use `term()` / `pred()`, so no production code
  calls the legacy API any more.)
  This supersedes the "pin … then fail closed" wording in
  `phase6-R2-DICT.md` (its Changelog bullet and its docs note on the legacy
  API) when the fragments are folded.
- **`DiskStore::query` fails on a read error.** It evaluates over
  `DiskSource` (R2-DISKLIB) with its out-of-core `DiskDict` dictionary
  (R2-DICT) inside a `ReadScope`: a read error of a triple index or of the
  dictionary fails the query with "a read from the store failed while
  evaluating the query: …" instead of answering partially. Healthy answers
  and their order are unchanged. The doc of `impl TripleSource for DiskStore`
  now says that impl does not report errors (use `DiskSource` or
  `DiskTriples`).
- **New test `tests/dt_r2_integration.rs`** (3 tests) runs the three
  packages together on bulk-built stores:
  - the out-of-core dictionary over a bulk-built LUBM `kvstore.kv` resolves
    every id of every space to the memory build's term and back
    (`resolve_many` included), and `materialize()` gives a dictionary whose
    `dict.bin` encoding is byte-identical to the memory build's;
  - Q1–Q14 over `DiskTriples` with the out-of-core dictionary, over a
    `DiskReadView` snapshot (`DiskSource`) and through `DiskStore::query`
    answer the memory build's rows in the same order, with no read error;
  - a transaction on the bulk-built store: read-your-writes through the
    out-of-core dictionary, nothing in the pager or the snapshot before
    commit, rollback moves no epoch, commit expires the old view and survives
    a reopen;
  - `DiskStore::query` fails on a corrupted POS root and on a corrupted
    id2entity root, and the error stays with its statement.

## GAP_PLAN status line

none (integration of the R2 packages; their own fragments carry the status
lines).

## Docs notes

- docs/STORAGE_BACKEND.md (with R2-DICT's C4 notes): the legacy
  `id_to_string` / `predicate_to_string` answer only for in-memory
  dictionaries; on an out-of-core dictionary they return `None` and fail the
  statement. Use `term()` / `pred()`.
- docs/STORAGE_BACKEND.md (with R1-CORE's C2 notes): `DiskStore::query` runs
  in its own read scope and fails on a read error; `DiskStore`'s own
  `TripleSource` impl still turns read errors into empty answers, so
  evaluate over `DiskSource` or `DiskTriples`. This supersedes the "kept:
  `DiskStore::query` … turn read errors into empty answers" note in
  `phase6-R2-DISKLIB.md`'s Requests.

## Measured numbers

None of the integrator's changes has its own benchmark: the legacy-API change
only removes a table no production path used, and `DiskStore::query` makes
the same `try_*` calls through `DiskSource` as before. The round's
acceptance numbers at univ 8 / 38 are the measure stage's (the package
fragments list the commands).

Verification of the merged tree (4 vCPU VM, other agents' jobs running,
load average about 3.2; `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
CARGO_INCREMENTAL=0`):

- `cargo fmt --all -- --check`, `cargo build --workspace --all-targets`,
  `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace --no-fail-fast`: 2,159 passed, 0 failed, 2 ignored
  (R2-DISKLIB's soak test and gstore-bench's `micro_on_disk`).
- `cargo clippy -p gstore --all-targets --features F -- -D warnings` clean
  and `cargo test -p gstore --features F` for gzip (1,959 passed), tls
  (1,960), zip (1,972), readline (1,956), pfn-wasm (1,977); 0 failed, 1
  ignored each.
- rocksdb: clippy clean; `cargo test -p gstore --features rocksdb` 1,979
  passed, 0 failed, 2 ignored (the soak test and a child-process helper);
  `cargo test -p gstore-bench --features rocksdb` 44 passed, 1 ignored.
- `scripts/conformance.sh`: all six suites pass (parser: only the known
  cases 15 and 17 differ; Python SDK 49/49).
- `flock /tmp/gstore-bench.lock env GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
  cargo run --release -p gstore-bench -- ci` against
  `bench/baselines/smoke.json` (`e859f7e`, univ 1): **PASS, 193 gates
  checked, 0 failed**, all result bodies identical. Ratios to the baseline:
  query geomean 0.50 (snapshot) and 0.56 (disk); build 0.66 (snapshot) and
  **0.02 (disk: 32.3 s → 0.49 s, the bulk build)**; disk build peak RSS 0.51;
  disk directory bytes 0.75; warm open 1.37 (snapshot, 26.8 → 44.3 ms) and
  0.36 (disk); RSS after open 0.96 / 0.88; snapshot bytes identical.
- The snapshot warm-open ratio of that single run was checked with a
  same-session A/B (`gstore-bench run --profile smoke --backends
  snapshot,disk --no-update --no-conc`, base = the `aaea9f8` binaries in
  /tmp/gstore-base-aaea9f8/bin, alternated twice): snapshot warm open 27.2 /
  30.0 ms (base) vs 33.5 / 32.0 ms (merged), first open 27.8 / 38.6 vs 42.0
  / 31.7 ms, RSS after open 22.5 / 22.3 vs 19.7 / 19.4 MiB; disk warm open
  44.1 / 41.6 vs 43.1 / 35.5 ms, disk build 10.6 / 11.6 s vs 0.53 / 0.49 s.
  So at univ 1 the snapshot open is about 1.1–1.2× within noise of a few
  milliseconds (R2-DICT measured 0.80× at univ 8); the measure stage's univ 8
  / 38 reference runs decide it.
- `smoke.json` is **not re-blessed**: plan §5.6(d) needs ≥ 1.5× on every
  smoke metric (R1-BENCH's reading), and snapshot open, RSS, bytes and
  several single queries did not improve that much. (Superseded by the
  measure stage below, which re-blessed it under the corrected rule.)

### Quiet post-merge stage (phase6 `0c9bdb6`: R2 merged, review fixes)

Reference VM (4 vCPU Intel Xeon @ 2.10GHz, 16 GB). Release builds made
before measuring (snapshot and disk from the default build, RocksDB from a
`--features rocksdb` build in `target-rocks`); every heavy command under
`flock /tmp/gstore-bench.lock`; CPU use logged every 10 s from
`/proc/stat`: busy 25–27% (the one measured process) and steal ≤ 1% during
every run below, load average 1.0. `gstore-bench run --profile reference
--scale {8,38} --drop-caches --verify-data`, RocksDB joined with `merge`
(cross-backend self-check passed): `bench/results/phase6-r2-lubm8.json` and
`phase6-r2-lubm38.json`. `compare` against the pre-Phase-6 files
(`phase6-r2-lubm{8,38}-compare.md`) and against Round 1's
(`phase6-r2-lubm{8,38}-compare-r1.md`): **PASS, 281 gates** in all four.
Wall time: univ 8 2.1 min (snapshot + disk) + 1.9 min (RocksDB), univ 38
5.4 min + 8.0 min (Round 1: 3.5 + 1.9 and 15.0 + 8.0 min; the disk build
alone went from 9.6 min to 20 s). Data hashes `ed44da2e7b3bbb98` /
`fcd6493725f3c66a`. Rows and row hashes of all 14 queries equal to the
reference on every layout at both scales, in every process; update round
trips ok.

**univ 8** (pre → R1 → R2; ratios R2/pre and R2/R1):

| backend | build s | gbuild peak MiB | database bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 15.32 → 5.33 → 5.80 (0.38× / 1.09×) | 154 → 138 → 112 | 85,480,180 (all three) | 336 / 292 → 300 / 243 → 213 / 177 (0.61× / 0.73×) | 125.8 → 128.1 → 101.7 (0.81×) | 152.5 → 154.0 → 126.8 | 7.91 → 3.06 → 3.29 (0.42× / 1.08×) |
| disk | 263.74 → 90.14 → 4.04 (0.02× / 0.04×) | 165 → 123 → 97 | 175,005,702 → 175,005,701 → 130,150,406 | 2240 / 1447 → 1124 / 318 → 294 / 259 (0.18× / 0.82×) | 129.7 → 114.5 → 88.2 (0.68×) | 167.5 → 145.2 → 118.3 | 18.99 → 5.63 → 5.87 (0.31× / 1.04×) |
| rocksdb | 53.93 → 31.49 → 35.60 (0.66× / 1.13×) | 286 → 287 → 262 | 41,162,410 → 41,135,741 → 41,140,275 | 9959 / 9989 → 306 / 292 → 202 / 180 (0.02× / 0.62×) | 145.8 → 90.2 → 64.5 (0.44×) | 184.9 → 138.8 → 112.5 | 43.27 → 31.10 → 33.28 (0.77× / 1.07×) |

**univ 38:**

| backend | build s | gbuild peak MiB | database bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 76.82 → 28.92 → 28.32 (0.37× / 0.98×) | 741 → 670 → 550 | 430,848,180 (all three) | 1588 / 1465 → 1591 / 1561 → 1149 / 898 (0.61× / 0.58×) | 596.4 → 608.9 → 488.6 (0.82×) | 710.3 → 708.3 → 590.4 | 46.65 → 16.79 → 18.28 (0.39× / 1.09×) |
| disk | 1627.59 → 575.88 → 19.84 (0.01× / 0.03×) | 738 → 527 → 400 | 889,778,182 → 889,778,181 → 653,959,174 | 12387 / 7673 → 5712 / 2092 → 1654 / 1582 (0.21× / 0.76×) | 544.0 → 470.6 → 350.1 (0.64×) | 740.6 → 580.5 → 460.2 | 146.92 → 38.65 → 37.51 (0.26× / 0.97×) |
| rocksdb | 277.76 → 175.93 → 164.35 (0.59× / 0.93×) | 634 → 628 → 511 | 206,253,032 → 206,222,953 → 206,221,265 | 60811 / 59389 → 1224 / 1187 → 1444 / 937 (0.02× / 0.79×) | 522.0 → 315.1 → 199.8 (0.38×) | 663.3 → 508.1 → 386.4 | 311.50 → 232.27 → 262.79 (0.84× / 1.13×) |

| query (univ 38) | rows | snapshot ms pre → R1 → R2 (R2/R1) | disk ms | rocksdb ms |
|---|---:|---|---|---|
| Q1 | 3 | 1.298 → 0.143 → 0.179 (1.25×) | 39.1 → 4.13 → 4.35 (1.05×) | 60.7 → 50.4 → 62.7 (1.24×) |
| Q2 | 115 | 50.8 → 39.2 → 39.6 (1.01×) | 189.7 → 49.5 → 48.9 (0.99×) | 729.5 → 1030.6 → 1152.1 (1.12×) |
| Q3 | 10 | 2.191 → 0.492 → 0.610 (1.24×) | 39.2 → 4.85 → 4.61 (0.95×) | 195.5 → 174.6 → 185.8 (1.06×) |
| Q4 | 9 | 53.634 → 0.099 → 0.111 (1.13×) | 75.2 → 2.08 → 2.09 (1.01×) | 990.0 → 394.5 → 438.1 (1.11×) |
| Q5 | 576 | 333.2 → 197.8 → 228.9 (1.16×) | 621.3 → 284.0 → 242.1 (0.85×) | 537.7 → 392.0 → 465.9 (1.19×) |
| Q6 | 404,399 | 187.6 → 113.4 → 124.7 (1.10×) | 511.4 → 150.0 → 153.3 (1.02×) | 468.3 → 282.9 → 346.2 (1.22×) |
| Q7 | 23 | 403.0 → 255.6 → 279.2 (1.09×) | 1147.7 → 380.4 → 344.5 (0.91×) | 977.6 → 774.7 → 763.3 (0.99×) |
| Q8 | 10,778 | 363.5 → 225.0 → 234.3 (1.04×) | 871.8 → 317.1 → 299.4 (0.94×) | 904.9 → 668.8 → 733.1 (1.10×) |
| Q9 | 5,507 | 529.3 → 314.5 → 336.8 (1.07×) | 1450.6 → 445.1 → 453.4 (1.02×) | 2125.9 → 1895.5 → 1900.3 (1.00×) |
| Q10 | 3 | 326.6 → 207.9 → 229.3 (1.10×) | 697.1 → 272.0 → 245.8 (0.90×) | 594.9 → 451.4 → 437.0 (0.97×) |
| Q11 | 325 | 1.09 → 1.12 → 1.12 (1.00×) | 1.488 → 0.949 → 0.999 (1.05×) | 16.8 → 14.2 → 17.0 (1.20×) |
| Q12 | 21 | 0.587 → 0.502 → 0.464 (0.92×) | 0.721 → 0.577 → 0.532 (0.92×) | 10.2 → 8.11 → 10.8 (1.34×) |
| Q13 | 432,512 | 223.9 → 134.6 → 142.9 (1.06×) | 906.1 → 221.4 → 215.5 (0.97×) | 511.9 → 305.5 → 347.9 (1.14×) |
| Q14 | 305,503 | 132.1 → 81.3 → 89.6 (1.10×) | 320.3 → 106.8 → 109.4 (1.02×) | 382.7 → 215.5 → 264.9 (1.23×) |

Update and concurrency phases (report-only), R1 → R2: univ 38 disk single
INSERT median 1.19 → 1.53 ms, chunk insert / delete 1057 / 1290 → 1298 /
1177 ms (the bulk-built 90%-fill tree splits more), reopen 3400 → 3087 ms;
snapshot reopen 1198 → 848 ms; Q4 qps at 1 / 8 clients 0.88–1.16× R1 on
every layout (snapshot `/sparql` 4247 / 4839 → 3734 / 4658, `/api` 2009 /
3924 → 1989 / 4556; disk `/sparql` 366 / 284 → 338 / 277). Univ 8 numbers
are in the JSON and the compare files.

**No regression against Round 1 beyond the gates — met.** `compare
phase6-r1-lubm{8,38}.json phase6-r2-lubm{8,38}.json`: PASS, 281 gates each;
calibration-normalized geomean ratios snapshot 1.03 / 1.02, disk 1.01 /
0.92, RocksDB 1.04 / 0.92 (univ 8 / 38); worst per-query ratio far under
3×. The raw geomeans above read +4–13% against the R1 file, but the same
binaries of main-before-the-branch measure 6–8% slower today than in R1's
session, so the round's own effect comes from same-session A/Bs:
- snapshot, base `aaea9f8` vs branch, two alternating rounds each: **1.000×**
  (univ 8) and **1.035×** (univ 38) — details and the profile of Q1/Q3/Q13
  in `phase6-R2-DICT.md`;
- disk: the bulk-built database queries at 0.915× an incrementally built
  one (`phase6-R2-BULK.md`); Materialize mode is otherwise unchanged;
- RocksDB, base `aaea9f8` (built with `--features rocksdb` from the clean
  `aaea9f8` checkout into its own target directory) vs branch, reference
  profile, same session: geomean 31.68 → 33.28 / 33.42 ms (**1.053×**, univ 8) and
  251.07 → 262.79 / 267.50 ms (**1.056×**, univ 38); the small queries move
  most (univ 38 Q1 49.7 → 62.3 ms, Q11 14.5 → 17.2, Q12 8.25 → 10.9), Q5 /
  Q7 / Q9 got faster. Callgrind of all 14 queries twice
  (`gstore-bench __child` query phase, `--toggle-collect` on
  `Database::execute_as`, the same univ 8 RocksDB database): **14.528 G vs
  14.497 G instructions (+0.2%), identical top functions** (RocksDB
  iterators, glibc malloc) — the same work; the +5% is a memory-layout /
  allocator effect of the leaner in-memory dictionary, like the snapshot's
  Q1/Q3, not a code path. RocksDB open and memory improved: warm open 1152
  → 887–937 ms, RSS after open 315.6 → 199.8 MiB, query HWM 507.5 → 386.4
  MiB, build peak 667 → 511–551 MiB (univ 38).

**Snapshot open** (the merge's univ 1 question): univ 8 warm 177 ms
against 236 / 291 ms for the base in the same session, univ 38 898 / 1112
against 1562 / 1487 ms — 0.6–0.75×, so the univ 1 reading of 1.1–1.2× was
noise of a few milliseconds.

**Smoke baseline (§5.6d, the rule given to this stage):** re-bless when
`gstore-bench ci` passes against the current baseline and at least one
headline metric (a backend's query geomean or build time) is ≥ 1.5× better.
`gstore-bench ci` (smoke, univ 1) against `bench/baselines/smoke.json`
(`e859f7e`): **PASS, 193 gates, 0 failed**. Ratios to the baseline
(calibration-normalized gate values): snapshot query geomean **0.50**
(2.0× better), snapshot build **0.49** (2.0×), disk query geomean **0.55**
(1.8×), disk build **0.02** (32.3 s → 0.54 s); warm open 0.90 / 0.37; RSS
after open 0.85 / 0.74; disk directory 0.75× (18,083,846 bytes), snapshot
bytes identical. Four headline metrics clear 1.5×, so `smoke.json` is
**re-blessed**, in a separate commit. The fresh `bless` run made right
after that check measured the disk queries 25% slower than the check
before it and the run after it (geomean 1.10 against 0.86 ms) with a
calibration of 71.3 ms, so it was discarded; the new baseline is `bless
--from` the smoke result of the next `gstore-bench ci` run of the same
commit (calibration 62.3 ms; its query medians match the first check's
within a few percent). `gstore-bench ci` against the re-blessed
`bench/baselines/smoke.json`: **PASS, 193 gates, 0 failed** (calibration
73.6 ms in that run; normalized query geomeans 0.92 snapshot / 0.82 disk,
builds 1.12 / 1.30, warm opens 0.99 / 0.86). The new baseline holds the
round's speed: a return of the queries to the pre-Phase-6 level now reads
about 2.0× (snapshot) / 1.8× (disk) in the smoke geomean gate, which fails
above 1.5, and the disk build gate fails at 2.5 × 0.5 s instead of
2.5 × 32 s.

**Environment note.** All reference, A/B and package runs above ran on the
idle machine. From 02:44 another session started compiling in
`/home/user/gstore-rust` (its heavy commands also under the bench lock, a
few `cargo check` runs without it). Affected and handled: the discarded
fresh `bless` run (above); the first smoke `ci` run, whose first seconds
overlapped such a `cargo check` (it passed; the baseline does not use it);
a `MALLOC_TRIM_THRESHOLD_` diagnostic pair, reported as inconclusive in
`phase6-R2-DICT.md`. The callgrind profiles count instructions and are
unaffected.

## Requests/Not yet

Handled here (Round-2 requests that connect the packages):

- R2-DICT request 1, first part: `lazy_dictionary_resolves_term_and_id_on_demand`
  uses `term()` / `pred()`, and the legacy pin table is gone (C4 exactly).
- R2-DICT request 1, second part, together with R2-DISKLIB's "never over a
  bare `DiskStore`": `DiskStore::query` runs in a `ReadScope` over
  `DiskSource`.
- Tests that need two or three packages: `tests/dt_r2_integration.rs`.

Left for later rounds (owner in bold; the package fragments give the
details):

- **R3-STREAM:** wire `Backend::Disk(DiskTriples)` (R2-DISKLIB's list:
  `insert_triple` / `remove_triple` and `touched()` on `Ok(true)`,
  `take_error`, begin / commit / rollback, `named_op`, `commit_point`,
  `mark_vstree_fresh`); `DiskDict::intern` through the shared pager without the
  gate and bumping the generation (R2-DICT request 2), then `disk_backing()`
  checks `DiskTermSource::generation()` instead of the counts; delegate
  `Database::build_disk*` and `/api build` to `bulk::build` (R2-BULK request
  1; the plan's R2-BULK brief, item 7, and D15 assign the delegation to R3,
  and a server must unload a database before rebuilding it). For Stream mode, open one
  `DiskDict::open_shared(dt.shared_pager())` per database and reuse it across
  statements and snapshot views, as `dt_r2_integration.rs` does:
  `DiskStore::lazy_dictionary()` on a `DiskReadView`'s per-call store would
  start with a cold cache on every view.
- **R3-ROCKS:** `BulkTarget::RocksDb` (R2-BULK request 2).
- **R4-SNAP:** `/begin` on a Stream database (R2-DISKLIB's list).
- **R4-RECLAIM:** `DiskDict::free` and holes (`slot()` bounds), the free
  stack as a lowest-id-first set with the same encoding, `FREE_IDS` in the
  bulk image (R2-DICT requests 2–3, R2-BULK request 5).
- **R4-CLOSURE:** gbuild's `memory:` / `spills:` summary in the bench JSON
  (R2-BULK request 4); the docs notes of all three fragments, including
  R2-BULK's install order, the 2,038-byte key limit and
  `VsTree::write_disk_sorted`.
- **Not in a Phase 6 package:** an N-Triples fast path or chunked parallel
  parse for `.nt` (parser owner, R2-BULK request 3). (`GraphCtx::term_str` /
  `pred_str` returning `TermStr` — R2-DICT request 4 — and the two legacy
  calls in `src/analytics/gfun/mod.rs` — R2-DICT request 5 — were listed
  here as harmless; the gfun calls were not: `DiskStore::query` runs graph
  functions over its backed dictionary. Both are done in
  `phase6-R2-review.md`.)
- **Engine owners:** long posting lists in Stream mode (Q3 2.4×, Q2 1.8× at
  univ 8, R2-DISKLIB) are input to R3's default-mode decision.
- **Measure stage:** all univ 38 acceptance items of the three packages and
  the §5.6(c) univ 8 reference re-run, including the snapshot warm open
  (above: about 1.1–1.2× at univ 1, within a few milliseconds of noise).
- **Observed, not in Round 2's files:** `tests/dt_gapi_db.rs` never removes
  its per-test server roots (`tmp_root`, `/tmp/gstore_gdb_<tag>_<pid>_<n>`);
  `the_upstream_api_test_script_runs_unmodified` leaves about 26 MB (LUBM
  export and backup) per run, and 164 such directories (4.1 GB) had piled up
  in `/tmp` on this machine. Removing the root when `Srv` drops would fix it.
