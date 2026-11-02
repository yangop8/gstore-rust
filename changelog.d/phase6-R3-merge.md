# Phase 6 R3 merge — integration of R3-ROCKS, R3-SRV and R3-STREAM

Merged in plan order on `phase6` from `0fb8842`, each with `git merge
--no-ff`: `p6/r3-rocks` (0a7e44a) as 3e111c8, `p6/r3-srv` (fedb7c6) as
34ad735, `p6/r3-stream` (7f39195) as 71673c8. All three merged without
conflicts and built at every merge point (`cargo build --workspace
--all-targets`; the RocksDB build too after the last one).

Ownership (§6 file lists, §7 table): every changed file is in its package's
list except the two reported exceptions, both accepted:

- R3-ROCKS, `tests/dt_bulk_disk.rs` (R2-BULK's test, no Round-3 owner): the
  one assertion that `BulkTarget::RocksDb` fails now runs only without the
  `rocksdb` feature — it encoded the behaviour R3-ROCKS replaces; the
  RocksDB target is tested in `dt_bulk_rocks.rs`.
- R3-SRV, `examples/srv_mixed_load.rs` (new): the mixed-workload driver its
  brief asked for (builds under `--all-targets`, clippy-clean).

R3-SRV reported "partial": one performance item is not met (mixed workload
with check_point on through `/api`, see its fragment). Its correctness gates
and the full suite pass, so it merges under §5.7 (fallback); the shortfall
stays in its fragment for R4-CLOSURE.

The commits after the merges are the integrator's own, listed below.

## Changelog

- **The RocksDB layout takes the directory lock too** (2772d63, cea4960;
  the Round-2 review's request, R3-ROCKS request 1, R3-STREAM request 7).
  `Database::open` / `open_rocksdb_with` and the per-triple
  `create_rocksdb` (`build_rocksdb_*`) take `<dir>/LOCK` for the RocksDB
  layout as for the snapshot and disk layouts, and the database keeps it
  for as long as it lives. A second process opening the directory gets the
  directory lock's error naming the holder. `create_rocksdb` also refuses a
  directory a database of this process serves ("open in this process")
  instead of discarding the live store's files. The servers already held
  the lock for every layout (R3-SRV's `DbSlot::handle` / `save_db`).
  - A bulk build over an open RocksDB database of this process is now
    refused by `DirLock::held_here` ("is open in this process") before its
    `rocksdb/LOCK` check. That check stays, as the defence against an older
    binary that holds only RocksDB's own lock; the two `/proc/locks`
    readers are one function (`bulk::commit::rocksdb_lock_holder`).
  - `dt_bulk_crash::an_open_rocksdb_database_is_not_replaced` and
    `dt_bulk_rocks::a_failed_build_leaves_the_old_database` assert the
    in-process refusal; new `dt_bulk_rocks::an_open_rocksdb_database_holds_the_directory_lock`
    covers the lock's lifetime (open, `build_rocksdb_from_str`, drop), the
    per-triple helper's refusal, and the older-binary case for the RocksDB
    target (only `rocksdb/LOCK` held: "in use", then replaced once free).
  - `src/db/dirlock.rs`'s module docs say so.
- **One definition of the RocksDB meta keys** (a92f4c4; R3-ROCKS request
  2). `persist.rs`'s `rocks_layout` re-exports `backend::rocks::{META_NAME_KEY,
  META_LAYOUT_KEY, LAYOUT_VERSION}` instead of keeping byte-identical
  copies, so the per-triple build and the SST writer cannot drift apart.
  The `build_rocksdb_*` helpers are documented as the per-triple
  small-data builds (plan D15), pointing at `bulk::build` with
  `BulkTarget::RocksDb` for large inputs.
- **`Database::build_disk_from_sources` is the bulk build** (bdd559d;
  R3-STREAM request 1, completing R2-BULK request 1). `gbuild --disk
  --incremental` and the bulk build's comparison tests (`dt_bulk_disk`,
  `dt_bulk_import`) name `build_disk_from_sources_incremental` as the
  incremental reference; `build_disk_from_sources` now forwards to
  `build_disk_bulk_from_sources`, like `build_disk`. No other caller used
  it. (Supersedes R3-STREAM's "build_disk_from_sources still forwards to
  the incremental build".)
- **CI's Stream pass runs the whole suite** (d37ca7b; R3-STREAM request 3).
  `dt_bulk_import::tolerant_report_and_log_equal_the_sequential_import`
  opens its bulk-built database with `OpenOptions::materialize()` (it
  compares through `Database::store()` and the legacy `id_to_string`, which
  only a materialized database has), so `ci.yml`'s
  `GSTORE_DISK_QUERY_MODE=stream` step drops its `--skip` and runs
  `cargo test --workspace` like the Materialize step.
- **`refreshconf` re-installs `[storage]`** (b9911a2, 7f8e2dc; R3-STREAM
  request 2). `ApiServer::with_storage_refresh` makes `/api refreshconf`
  install the re-read section as the process's default open options
  (`set_default_open_options`); `gserver` opts in next to
  `with_log_refresh`, having installed the same section at startup. An
  embedded server leaves the process's defaults alone. Databases loaded
  afterwards use the new options (the environment still overrides them);
  loaded ones keep their mode. `conf.ini.example` says so.
- **Servers and bulk builds of a served directory** (f1499ae, 40a1bfe). No server
  path calls `Database::build_disk*` or `bulk::build`: REST `/build`
  retires the loaded copy and saves a snapshot, `/api build` refuses an
  existing database (1004) and builds into a staging directory. New
  `tests/dt_r3_integration.rs` pins the contract on the disk layout and,
  with the feature, the RocksDB layout: a served database holds its
  directory; `/api build` over it answers 1004 and leaves it serving; a
  bulk build of its directory is refused ("open in this process") until
  `unload` releases the handle, then replaces it, and `load` serves the
  new database; REST `/build` replaces a loaded on-disk database in place.
  The same file tests the `refreshconf` item above.
- **Server stress over Stream-mode disk databases** (07d73ce; R3-SRV's
  read / write site classification against `Backend::Disk`).
  `dt_rw_server_stress` runs both servers over a `kvstore.kv` database as
  well: the single-database server over one opened in Stream mode, the
  `ApiServer` over one it loads in the process's mode (the binary installs
  Stream; CI runs it in both modes), with transactions in the store's
  overlay. Every pair is seen whole, uncommitted and rolled-back data
  never, and the reopened database equals the ledger.
  - Checked by reading the code as well. Every path R3-SRV runs under a
    shared guard calls only `&self` methods of `Database`, so the
    interning `Dictionary` methods (`&mut self`) cannot run there.
    `DiskDict::intern` runs only under the exclusive guard.
  - Shared-guard reads of a Stream database go through `DiskTriples`' plain
    `TripleSource` reads (pager read lock per operation), not through the
    writer gate, which only `DiskReadView` readers enter (none on the server
    paths before R4-SNAP).
  - `backup` / `save` under a shared guard take `persist_lock`, then the
    pager write lock for the commit point. A broken store fails the reads
    (C2) and the updates.
  - The order stays C8's end to end: server `RwLock` / `DbCell` →
    `Database` (`persist_lock` → disk sink → `persist_error` → pager), and
    `DiskTriples` gate → pager. No path waits for a database guard while it
    holds a lower lock.
- **R2-merge wording** (5360282; Round 2's last review, nit): the "Rust
  1.99.1 lints" paragraph of `phase6-R2-merge.md` names gstore-bench
  all-targets with rocksdb as checked in addition to CI's clippy scopes (no
  CI job lints it).
- **Housekeeping:** `/tmp/p6r3-target-rocks` (the implementers' shared
  RocksDB target, 6.3 GB) deleted once no process used it.

## GAP_PLAN status line

none (integration of the R3 packages; their own fragments carry the status
lines).

## Docs notes

- docs/STORAGE_BACKEND.md (or wherever R4-CLOSURE documents the directory
  lock): every layout's directory is locked against other processes while a
  database of it is open, RocksDB included (`<dir>/LOCK` beside
  `rocksdb/`, whose own `LOCK` RocksDB still takes); the per-triple
  `build_rocksdb_*` helpers return the database holding it and refuse a
  directory this process serves.
- Configuration docs for `[storage]` (with R3-STREAM's notes): `gserver`
  applies the section at startup and on `/api refreshconf`; loaded
  databases keep their mode.
- `Database::build_disk_from_sources` is the bulk build (as `build_disk`);
  the incremental build is `build_disk_from_sources_incremental` /
  `build_disk_incremental` (hidden, for comparisons and `gbuild --disk
  --incremental`).

## Measured numbers

No performance measurement at the merge stage (§5.6 (c), the quiet univ 8 /
38 runs, is the measure stage's, with each package's measure instructions).
The smoke gate's result is under Verification below.

### Quiet post-merge stage (phase6 `4ef1141`; part 3: reference files, default mode, smoke baseline)

Parts 1 and 2 of the stage are in `phase6-R3-ROCKS.md` and
`phase6-R3-SRV.md`; R3-STREAM's acceptance, the G1b–G1d evaluation and the
default-mode decision are in `phase6-R3-STREAM.md`. Reference VM (4 vCPU
Intel Xeon @ 2.10GHz, 16 GB), nothing else running (20:17–21:07 UTC; CPU
busy 23–29% with one measured process, up to 99% at the 8-client points,
steal ≤ 2%). Release build of `4ef1141`; every heavy command under `flock
/tmp/gstore-bench.lock`; data from `/tmp/gstore-bench-cache` (univ 8
`ed44da2e7b3bbb98`, univ 38 `fcd6493725f3c66a`). "Base" = the release
binaries of `0fb8842` (main before this round,
`/tmp/gstore-base-0fb8842/bin`), measured in this session.

**Reference files.**
- `bench/results/phase6-r3-lubm{8,38}.json`: `gstore-bench merge` of the
  snapshot layout (`run --profile reference --backends snapshot
  --drop-caches`), the disk layout in the default mode, Materialize
  (`--backends disk --drop-caches --env GSTORE_DISK_QUERY_MODE=memory`),
  both full reference runs of `4ef1141`, and part 1's RocksDB runs
  (`--features rocksdb` build of `e1c7131`, `--no-conc`).
- `bench/results/phase6-r3-disk-stream-lubm{8,38}.json`: the disk layout in
  the non-default Stream mode (full reference run, concurrency and update
  phases included).
- Compare reports next to them: `phase6-r3-lubm{8,38}-compare.md` (against
  pre-Phase-6), `phase6-r3-lubm{8,38}-compare-r2.md` (against Round 2),
  `phase6-r3-disk-stream-lubm{8,38}-compare.md` (Stream against Materialize
  and against the pre-Phase-6 disk layout: PASS, 105 gates each).

`compare` against pre-Phase-6 and against Round 2: **272 of 273 gates pass**
at both scales. The one failure is `exact.phase rocksdb/conc`: part 1 ran
RocksDB with `--no-conc`, as its brief said. Every other exact gate passes:
rows and row hashes of Q1–Q14 on every layout, the update round trips, and
the concurrent bodies of the snapshot and disk layouts.

To cover the missing RocksDB concurrency phase, a separate run was made
(`target-rocks` binaries, `--backends rocksdb --query-procs 1 --reps 1
--open-reps 1 --no-update`; kept out of the reference files, because
`merge` refuses a backend twice):
- Bodies are identical per endpoint and query, and the run passes against
  pre-Phase-6 (`--backends rocksdb`). The only failure is the update phase
  it skipped.
- Q4 at univ 8: /sparql 1,370 → 6,504 qps from 1 to 8 clients (4.75×),
  /api 1,140 → 4,575 (4.01×).
- Q4 at univ 38: /sparql 536 → 2,334 (4.35×), /api 488 → 1,951 (4.00×).
  Pre-Phase-6 managed 1.20 → 1.16 qps at univ 38.

| univ 8 | build s | gbuild peak MiB | database bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| snapshot, pre → R2 → R3 | 15.32 → 5.80 → 5.31 | 154 → 112 → 113 | 85,480,180 (all) | 336 / 292 → 213 / 177 → 209 / 187 | 125.8 → 101.7 → 102.0 | 152.5 → 126.8 → 128.8 | 7.91 → 3.29 → **3.11** |
| disk (Materialize) | 263.74 → 4.04 → 4.49 | 165 → 97 → 99 | 175,005,702 → 130,150,406 → 130,150,405 | 2240 / 1447 → 294 / 259 → 289 / 259 | 129.7 → 88.2 → 88.6 | 167.5 → 118.3 → 119.2 | 18.99 → 5.87 → **5.49** |
| disk, Stream (R3) | 3.73 | 109 | 130,150,405 | **4.4 / 0.6** | **5.9** | **69.2** | 6.67 |
| rocksdb | 53.93 → 35.60 → 4.21 | 286 → 262 → 93 | 41,162,410 → 41,140,275 → 31,266,467 | 9959 / 9989 → 202 / 180 → 255 / 184 | 145.8 → 64.5 → 68.3 | 184.9 → 112.5 → 102.6 | 43.27 → 33.28 → **10.56** |

| univ 38 | build s | gbuild peak MiB | database bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| snapshot, pre → R2 → R3 | 76.82 → 28.32 → 28.84 | 741 → 550 → 550 | 430,848,180 (all) | 1588 / 1465 → 1149 / 898 → 1043 / 1015 | 596.4 → 488.6 → 489.0 | 710.3 → 590.4 → 588.5 | 46.65 → 18.28 → **16.14** |
| disk (Materialize) | 1627.59 → 19.84 → 21.01 | 738 → 400 → 434 | 889,778,182 → 653,959,174 → 653,959,173 | 12387 / 7673 → 1654 / 1582 → 1491 / 1407 | 544.0 → 350.1 → 350.7 | 740.6 → 460.2 → 460.7 | 146.92 → 37.51 → **39.77** |
| disk, Stream (R3) | 20.06 | 429 | 653,959,173 | **10.5 / 0.6** | **5.8** | **210.8** | 44.13 |
| rocksdb | 277.76 → 164.35 → 22.25 | 634 → 511 → 402 | 206,253,032 → 206,221,265 → 155,868,540 | 60811 / 59389 → 1444 / 937 → 1006 / 901 | 522.0 → 199.8 → 276.6 | 663.3 → 386.4 → 440.2 | 311.50 → 262.79 → **66.91** |

**No regression against Round 2 beyond the gates: met.**

Gate ratios of `compare-r2` (calibration-normalized):

| backend | geomean univ 8 / 38 | build univ 8 / 38 | warm open univ 8 / 38 |
|---|---|---|---|
| snapshot | 1.11 / 0.95 | 1.03 / 1.03 | 1.17 / 1.14 |
| disk | 1.02 / 1.07 | 1.22 / 1.06 | 1.10 / 0.89 |
| RocksDB | 0.32 / 0.29 | 0.12 / 0.16 | 1.03 / 1.11 |

All of these are far under the limits (1.5, 2.0/2.5, 2.5). Raw, across
sessions: snapshot 3.29 → 3.11 ms and 18.28 → 16.14 ms; disk 5.87 → 5.49 ms
and 37.51 → 39.77 ms.

Same-session A/B against the base binaries (alternating, `--no-conc
--no-update`):
- **Snapshot.** Geomean this tree / base is **0.964×** at univ 8 (base
  3.313 / 3.206 ms, this tree 3.197 / 3.124 / 3.109) and **0.971×** at
  univ 38 (base 17.19 / 16.70, this tree 16.72 / 16.51 / 16.14). No query
  is slower than 1.03×. RSS after open, query HWM and the build are
  unchanged.
- **Disk, Materialize mode.** Geomean 1.03× at univ 38 (base 38.84 /
  37.91 ms, this tree 39.80 / 39.17 / 39.77) and 1.01× at univ 8 (base
  5.563, this tree 5.716 / 5.494). Q5, Q9 and Q13 are +5–9% at univ 38.
  - Callgrind shows the same code path. Setup: `gquery`,
    `--toggle-collect` on `Database::read_as`, Materialize mode, the base
    and this tree's `gquery` on the same univ 8 directory, three runs each.
  - Q9: base 425.3 / 429.9 / 430.4 M instructions, this tree 432.2 /
    430.2 / 429.4 M. Q5: 229.9 vs 230.0 M.
  - Every engine function's count is identical. The only variation is the
    number of `vstree.kv` page reads: 28,017–28,869 on the base, 28,740–
    29,200 here. That number depends on the search order (hash seeds)
    against `DiskTree`'s 1,024-page cache.
  - So the +3% is run-to-run spread: one query's per-process medians vary
    by ±10–30% in a run. Not caused by this round; no fix.
- **Disk build peak RSS.** 400–434 MiB on the base and on this tree alike
  (402.7 / 434.2 MiB base, 400.6–432.1 here). It depends on `gbuild`'s
  parser threads; Round 2's 400.4 MiB was one sample.

**G2 (snapshot layout) at univ 38**, for R4-CLOSURE:
- Build 28.84 s = 0.38× pre (≤ 0.5): **met**.
- RSS after open 489.0 MiB = 0.82× pre (≤ 0.9): **met**.
- Q1–Q14 geomean 16.14 ms = 0.35× pre (≤ 0.9): **met**.
- Q4 0.096 ms (≤ 25 ms): **met**.
- Latency ratio univ 38 / univ 8 for Q1, Q3, Q4, Q11, Q12 (≤ 2.0): Q4 1.33
  **met**; Q1 3.05, Q3 5.42, Q11 4.45, Q12 3.63 **not met**. These are
  sub-millisecond queries whose candidate lists grow with the data. The
  ratios were the same in Round 1 (3.44 / 5.19 / 4.68 / 3.66) and Round 2
  (3.80 / 5.25 / 4.48 / 3.49): not a regression.

**Phase 6 gates now, univ 38 (R4-CLOSURE measures them):**
- G1a: disk build 21.0 s, 0.013× pre; peak 400–434 MiB.
- G1b and G1c (Stream): met.
- G1d: not met, on both disk modes (the on-disk VS-tree, R3-STREAM
  request 8).
- G2: met except the scaling ratios above.
- G3: met (part 1).
- G4: met on the memory layout (part 2) and on a Stream-disk database at
  univ 8: Q4 3.35× on /sparql, 2.79× on /api. At univ 38 it is 2.51× /
  2.63× (Materialize 2.41× / 2.55×). Monitoring has part 2's two `/api
  monitor` exceptions.

**Default disk query mode: Materialize, unchanged.** Every R3-STREAM
acceptance item passes at univ 38, but G1d fails, and plan §2 keeps
Materialize the default while G1d fails. The numbers are in
`phase6-R3-STREAM.md`.

**Smoke baseline (§5.6d; the rule used since Round 2):** re-bless only when
`gstore-bench ci` passes against the current baseline and at least one
headline metric (a backend's query geomean or build time) is ≥ 1.5× better.
- `gstore-bench ci` against `bench/baselines/smoke.json` (`0c9bdb6`, R2's
  quiet stage): **PASS, 193 gates, 0 failed**. Calibration 55.7 ms against
  the baseline's 62.3.
- The canary (`check --self-test-slowdown 3` of the result against itself)
  exits 1, as required.
- Ratios to the baseline (calibration-normalized gate values):

  | backend | query geomean | build | warm open | RSS after open / query HWM / build peak | bytes |
  |---|---:|---:|---:|---:|---:|
  | snapshot | 1.06 (raw 0.94×) | 1.16 | 1.05 | 1.00 / 1.00 / 1.00 | identical |
  | disk | 1.06 (raw 0.96×) | 1.19 | 0.88 | 1.01 / 1.01 / 0.93 | identical |

- No headline metric is ≥ 1.5× better (this round's speed-ups are on
  RocksDB, which the smoke profile does not run, and on Stream mode, which
  is not the default). So `smoke.json` is **not re-blessed**.

Every database built at this stage was deleted, together with the result
files that are not committed (their numbers are above and in
`phase6-R3-STREAM.md`). The RocksDB target directory (`target-rocks`, used
by parts 1–3) was removed at the end. No code changed at this stage.

## Verification

Every job of `.github/workflows/ci.yml` and `bench.yml` (smoke and
rocksdb-correctness) was run with the commands as written there, on the
pinned toolchain (`rustup default` 1.99.1), with
`CARGO_PROFILE_{DEV,TEST}_DEBUG=0` instead of CI's `line-tables-only` (disk
space; no effect on what is checked). RocksDB builds used their own target
directory.

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- test job: `GSTORE_DISK_QUERY_MODE=memory cargo test --workspace` gave
  2,217 passed, 0 failed, 2 ignored. The same with
  `GSTORE_DISK_QUERY_MODE=stream`, now without the `--skip`, gave 2,217
  passed, 0 failed, 2 ignored.
- features job, for each of gzip, tls, zip, readline, pfn-wasm and
  "zip pfn-wasm readline": `cargo build --locked --profile test -p gstore
  --all-targets --features …`, the clippy step with `-D warnings`, and
  `cargo test -p gstore --features …`. All clean. Test counts:

  | features | passed | failed | ignored |
  |---|---|---|---|
  | gzip | 2,017 | 0 | 1 |
  | tls | 2,018 | 0 | 1 |
  | zip | 2,030 | 0 | 1 |
  | readline | 2,014 | 0 | 1 |
  | pfn-wasm | 2,036 | 0 | 1 |
  | zip pfn-wasm readline | 2,052 | 0 | 1 |
- conformance: `scripts/conformance.sh` passed every suite (basic,
  update, transaction, api, parser, python). The only parser cases that
  fail are 15 and 17, the known divergences. The Python SDK check gave 49
  passed, 0 failed.
- rocksdb job: `cargo clippy -p gstore --all-targets --features rocksdb --
  -D warnings` is clean. `cargo test -p gstore --features rocksdb` gave
  2,058 passed, 0 failed, 2 ignored.
  - This job's clippy caught `clippy::err_expect` in the new
    `dt_bulk_rocks` test, fixed in cea4960.
  - Both steps were run again on the final code.
- bench.yml rocksdb-correctness: `cargo build --profile test --locked -p
  gstore --bin gbuild --features rocksdb`, then `cargo test -p gstore-bench
  --features rocksdb`: 44 passed, 0 failed, 1 ignored.
- bench.yml smoke: `cargo build --release --locked -p gstore -p
  gstore-bench --bin gbuild --bin gstore-bench`, then, under `flock
  /tmp/gstore-bench.lock` with `GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache`,
  `gstore-bench ci --baseline bench/baselines/smoke.json`.
  - PASS: all 58 gates pass (the geomean ratio is 1.00 on the snapshot
    layout and 1.02 on the disk layout).
  - The canary (`check --self-test-slowdown 3` of the result against
    itself) exits 1, as required.
  - No re-bless of `bench/baselines/smoke.json`: nothing moved by ≥ 1.5×
    (§5.6 (d)).
- The test job and the feature matrix ran before the last three commits
  (7f8e2dc, cea4960, 40a1bfe). Those commits change only a comment in
  `conf.ini.example`, `dt_bulk_rocks` (which compiles to nothing without
  `rocksdb`) and `dt_r3_integration`'s cleanup guards.
  - After them, fmt and workspace clippy were run again, and
    `dt_r3_integration` was run in both modes.
  - The conformance suite, the smoke gate and both RocksDB jobs ran on the
    final code.

## Requests/Not yet

Left for Round 4, with their owners:

- R4-SNAP: server builds through `bulk::build` with a layout parameter
  (`layout=snapshot|disk|rocksdb`; R3-ROCKS request 3, R3-SRV request 1).
  REST `/build` must retire the loaded handle before calling it and not
  hold its `DirLock::acquire_existing` probe across it (the bulk build
  takes the lock itself and refuses a directory `DirLock::held_here`
  reports); `tests/dt_r3_integration.rs` shows the unload / build / load
  sequence that works today.
- R4-SNAP: `/begin` and `ConcurrentDb` snapshots of a Stream database still
  copy the default graph through `iter_all` (R3-STREAM request 4).
- R4-SNAP / R4-CLOSURE: check_point on with the snapshot layout (R3-SRV
  request 2) — the mixed `/api` workload's reader throughput is below its
  target while each update rewrites the snapshot under the exclusive guard.
- R4-RECLAIM: `DiskDict::free` must move the generation and `cache_epoch`
  (R3-STREAM request 5).
- R4 config: a `[storage]` key for the RocksDB block cache feeding
  `RocksTuning` (R3-ROCKS request 4; today `GSTORE_ROCKS_BLOCK_CACHE_MB`).
- R4-CLOSURE (`bench/**`): gbuild's summary in the result JSON (R3-ROCKS
  request 5), a mixed-workload point replacing `examples/srv_mixed_load.rs`
  (R3-SRV request 5), a run label so one backend's two disk query modes
  can share a result file (R3-STREAM's note).
- Temp-directory leaks of the test files R3-SRV listed (R3-SRV request 3).
- Not Round-4-owned: a seek within the current leaf in
  `src/kvstore/bptree.rs` for the per-thread SPO cursor (R3-STREAM request
  6, R1-KV's file).
- The measure stage: every univ 38 acceptance item of the three packages
  (their fragments' measure instructions), and the Stream-mode default flip
  only if every R3-STREAM gate passes. Done (quiet post-merge stage, under
  Measured numbers and in the package fragments): Materialize stays the
  default because G1d fails on both disk modes (R3-STREAM request 8, for
  R4-SNAP / R4-CLOSURE).
- Not in CI: `cargo doc -p gstore` warns about broken or private
  intra-doc links in about 30 files, most of them from before Round 3 and
  a few from it (e.g. `backend/rocks.rs`'s `SEEK_RUN`, `db/persist.rs`'s
  `super::default_open_options`); none comes from the integrator's commits.
  A docs pass (R4-CLOSURE) could fix them and add `cargo doc` with
  `-D warnings` to CI.
