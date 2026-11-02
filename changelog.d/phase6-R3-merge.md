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
  only if every R3-STREAM gate passes.
- Not in CI: `cargo doc -p gstore` warns about broken or private
  intra-doc links in about 30 files, most of them from before Round 3 and
  a few from it (e.g. `backend/rocks.rs`'s `SEEK_RUN`, `db/persist.rs`'s
  `super::default_open_options`); none comes from the integrator's commits.
  A docs pass (R4-CLOSURE) could fix them and add `cargo doc` with
  `-D warnings` to CI.
