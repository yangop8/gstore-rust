# r4-storage-format — Secondary on-disk structures and statistics: validated VS-tree/VList/prefix decoding, saturating counters, accelerator perturbation tests, cross-version fixtures from the real pre-Phase-6 binary

## Changelog

- **Behaviour change (review disk#15): `VsTree::open_disk` on a missing path
  is an error** (`GStoreError::Io`, `NotFound`, naming the file) and creates
  nothing; an empty file is refused as corrupt. It used to create the file
  through the pager and return an empty tree (no filtering). A database
  whose `vstree.kv` is empty now fails to open with "VS-tree file … is empty
  (corrupt; remove it to rebuild the index)"; a missing `vstree.kv` is
  rebuilt or skipped as before (the open paths check for it first).
- **Behaviour change (review disk#4): the out-of-core VS-tree validates what
  it reads.** A scan checks every node it visits — kind byte 0/1, entry
  count ≤ 32 (`DISK_FANOUT`), child ids inside the file, never the header
  page and below their parent's (both writers write children first, so a
  valid tree is acyclic), every leaf at one depth, no path longer than 8
  nodes — and `open_disk` checks the header (root id inside the file, an
  entity count the file can hold). A violation is a corruption error
  (`GStoreError::Serialize`, `ErrorKind::Corrupt`: "VS-tree file '…' is
  corrupt: … (remove it to rebuild the index)"). As a candidate filter the
  tree then answers "no filtering", which is sound. Before, an entry count
  above 32 panicked with an index out of range, child id 0 read the header
  as a node, and a cycle overflowed the stack and aborted the process.
- **disk#5: `VsTree::build_disk` replaces an existing file.** It removes the
  file and its write-ahead log (`Pager::remove_with_wal`) before building,
  as its docs always said; before, the pager opened the old file and
  appended the new tree after the old one (three builds of 5,000 entities
  grew the file to 671,744, 1,339,392 and 2,007,040 bytes) and replayed a
  stale `<path>.wal` into it. Production paths already built under a fresh
  temporary name; library callers are affected.
- **Behaviour change (review robust#11): a corrupt VList is a read error.**
  `DiskStore::try_o_by_sp` reports a compacted `sp2o_vlist` record that does
  not decode, or one with an unknown tag, as `io::ErrorKind::InvalidData`
  ("corrupt kvstore: …"), so the statement fails (contract C2) instead of
  `<s> <p> ?o` silently having no values. `kvstore::vlist::decode_u32s`
  refuses a count its remaining bytes cannot hold before allocating (a
  garbage count of `u32::MAX` used to request 16 GiB and abort the process).
- **db#21:** `dict::prefix::decode_block` bounds its allocation the same way
  (two bytes per entry at least; `u32::MAX` entries asked for 96 GiB); its
  module docs say the front-coding codec is library-only (no on-disk
  structure uses it; kept public, D2).
- **gap-accel-soundness#4, bulkcli#14: per-predicate triple counts saturate
  instead of wrapping.** The disk layout's `pred_stats` record stays a
  `u32` (no format change, D7): the incremental writer now stops at
  `u32::MAX` ("at least this many", kept by deletes until
  `rebuild_stats` recounts), the bulk build counts in `u64` — the RocksDB
  `c|p` counter gets the exact count, the disk record `u32::MAX` when it
  does not fit — and the distinct counters add with saturation. A count
  that wrapped to 0 made `VsPolicy::Auto` drop the predicate on an
  out-of-core VS-tree and answer no rows (and a debug build panicked with
  an overflow); `predicate_bit_classes` now treats a saturated count as an
  unknown one (ordered last), and only a count of 0 as absent.
- **Tests.** `tests/storage_corruption.rs` (damaged `sp2o_vlist` records and
  a damaged `vstree.kv` node through the public API; queries stay correct
  under every VS policy); new unit tests in `signature::disk_vstree`
  (`validation_tests`: count, child 0, child past the file, cycles, depth,
  uneven leaves, kind byte, header, rebuild sizes, stale WAL, missing and
  empty files), `kvstore::vlist`, `dict::prefix` and `bulk::stats`.
  `tests/dt_vs_policy.rs` gains the perturbation battery of review
  gap-accel-soundness#5 on all three layouts (in memory, disk Stream, disk
  Materialize) under every VS policy: insert churn, id reuse, rolled-back
  transactions and restarts against an oracle database built afresh;
  substring pushdown over literals of every datatype family against the
  `|| false` form; `ORDER BY … LIMIT/OFFSET` over extreme numeric keys
  under every top-k setting, repeat determinism and page tiling; and a
  `pred_stats` count forced past `u32::MAX`.
- **Cross-version compatibility (review gap-doc-claims#8).** New
  `scripts/compat-fixtures.sh` builds the pre-Phase-6 commit eaf5320 in a
  scratch worktree and checks the compatibility matrix with real binaries
  of both versions: eaf5320's `gbuild`/`gquery` write a snapshot and a
  version-1 disk database that the current code opens and answers
  (`tests/format_compat.rs`); eaf5320's `gquery` reads a snapshot and a
  `GSTORE_ID_RECLAIM=off` disk database written by the current `gbuild`,
  and refuses the default version-2 one ("not a gStore KV file");
  eaf5320's `restore_incremental` fails on the `CLEAR` record of an
  increment written by the current code ("unknown redo.log op 'C'").
  `--generate` rewrites the checked-in fixtures `testdata/compat/eaf5320/`
  (`testdata/compat/README.md`: commit, commands, sizes, hashes).
  `tests/format_compat.rs` runs in every `cargo test`; the new workflow
  `.github/workflows/compat.yml` runs the script weekly and on demand.

## GAP_PLAN status line

none

## Docs notes

- **README.md** (review gap-doc-claims#6), the storage-layouts bullet
  (README.md:161-162): replace "(**Stream**: it opens in milliseconds with a
  few MiB of memory whatever the data's size)" with "(**Stream**: the
  default graph opens in milliseconds with a few MiB of memory whatever its
  size; named graphs are loaded into memory when the database opens, so
  open time and memory grow with their quads — PHASE6_PLAN §8 D11)". In the
  "Disk query mode" bullet (README.md:296-298), "Stream opens in
  milliseconds and keeps only the named graphs, …" becomes "Stream opens
  the default graph in milliseconds (the named graphs are read whole at
  open, linear in their quads: D11) and keeps only the named graphs, …".
  The code is `load_disk_stream` → `DiskStore::named_to_memory`
  (src/db/persist.rs, src/kvstore/store.rs); round 1 measured 0.01 s /
  13 MiB with the default graph only against 0.94 s / 76 MiB with 300k
  named-graph quads.
- **docs/STORAGE_BACKEND.md**, "兼容矩阵": the matrix is now checked with
  the eaf5320 binaries by `scripts/compat-fixtures.sh` (weekly,
  `.github/workflows/compat.yml`), against fixtures in
  `testdata/compat/eaf5320/` that `tests/format_compat.rs` opens in every
  test run. The redo-log sentence is confirmed by the real old binary:
  eaf5320's `restore_incremental` fails with "unknown redo.log op 'C'" on
  an increment holding a `CLEAR DEFAULT` written by this version.
- **docs/STORAGE_BACKEND.md**, format registry: `vstree.kv` is validated
  while read (kind byte, entry count ≤ 32, child ids in `1..page` below
  their parent, uniform leaf depth, depth ≤ 8, header root and count); a
  damaged file filters nothing and `open_disk` refuses a missing, empty or
  header-damaged one ("remove it to rebuild the index"). The per-predicate
  `pred_stats` count is a `u32` that saturates at `u32::MAX` (an unknown
  count, at least that large); RocksDB's `c|p` is exact (`u64`), bulk or
  incremental. No format change.
- **docs/REFACTOR_BACKLOG.md** (review disk#15, D8): opening any pager file
  still needs write access and replays/creates as a writer would — a
  read-only `OpenMode` for `Pager` (no create, no WAL replay, refused
  writes) is an API addition left for later; `open_disk` now refuses a
  missing or empty file, the half that changed behaviour silently.
- **docs/REFACTOR_BACKLOG.md** (review gap-accel-soundness#4): a saturated
  disk-layout `pred_stats` count stays at `u32::MAX` across deletes until
  `DiskStore::rebuild_stats` recounts (no API calls it on demand besides
  the open of a pre-statistics file); widening the record to `u64` needs a
  feature bit (D7) and was not worth one at this scale (2^32 triples of one
  predicate).
- **docs/REFACTOR_BACKLOG.md** (review robust#11): `kvstore::overflow::read_chain`
  still follows a chain until it has `len` bytes; a damaged chain that
  cycles reads up to the record's length (≤ 4 GiB) before the VList decode
  refuses it — a visited-page bound would close that.

## Measured numbers

- **Bench smoke gate** (D9; release `gbuild` + `gstore-bench` of the base
  b147336 and of this branch, built with `--offline --release` in one scratch
  target, `GSTORE_BENCH_CACHE` warm, `flock /tmp/gstore-bench.lock
  gstore-bench ci --baseline bench/baselines/smoke.json --bin-dir …`, three
  rounds — base/branch, branch/base, base/branch — with load averages of
  8–16 on 4 vCPUs from other worktrees' builds and tests): **PASS, 456 gates
  checked, 0 failed, for both binaries in every round.** Time geomean vs the
  committed baseline (bound 1.50):

  | run | snapshot | disk | watdiv snapshot | watdiv disk |
  |-----|---------:|-----:|----------------:|------------:|
  | round 1 base   | 0.67 | 0.70 | 0.75 | 0.67 |
  | round 1 branch | 0.83 | 0.86 | 0.86 | 0.97 |
  | round 2 branch | 0.98 | 0.91 | 0.97 | 1.02 |
  | round 2 base   | 0.83 | 0.93 | 0.76 | 0.93 |
  | round 3 base   | 0.79 | 0.83 | 0.83 | 0.91 |
  | round 3 branch | 0.85 | 0.96 | 0.90 | 1.00 |

  Per query, the median of the three rounds, branch / base, geomean:
  LUBM-like snapshot 1.033, disk 1.019; WatDiv-like snapshot 0.985, disk
  1.038. The snapshot layout runs none of the changed code (its VS-tree is
  in memory; the counters and VList changes are disk-only), so its ±3.5 %
  is this machine's noise, and the disk layout stays inside it (round 1's
  base was measured in a quieter minute: its disk queries were ~15 % faster
  than the same binary's rounds 2 and 3). The default `Auto` policy never
  reads `vstree.kv` pages, so the per-node checks of disk#4 run only under
  `VsPolicy::Tree` (a few comparisons per 4 KiB node read).
- **Test suites** (debug, `CARGO_PROFILE_*_DEBUG=0`, 4 vCPUs shared with
  other builds): `cargo +1.99.1 test --workspace --no-fail-fast` 2,939
  passed / 0 failed with `GSTORE_DISK_QUERY_MODE=memory` and with `=stream`
  (178 test binaries). The first `memory` run (without `--no-fail-fast`)
  stopped at `db::concurrency_tests::generation_oracle_under_a_rwlock`
  (`checked > 0`: under a load average of 12 the four readers did not get
  the lock once before the writer's 60 inserts finished); it passed three
  times alone and in the rerun — not this package's code (see
  Requests/Not yet). `cargo clippy --workspace --all-targets -D warnings`
  clean. With `--features rocksdb` (shared rocks target, under its lock):
  `cargo clippy -p gstore --all-targets --features rocksdb -D warnings`
  clean (it caught a now-useless `u64::from` in
  `rocks_sink::write_pred_counters`, fixed), and `cargo test -p gstore
  --features rocksdb --no-fail-fast` 2,800 passed / 0 failed / 9 ignored
  (166 test binaries plus the doc tests). `scripts/conformance.sh
  --no-build` (dev binaries): all six suites pass (parser: only the known
  cases 15 and 17 fail, as on the base; Python SDK 49/0).
- **Compatibility script** (`CARGO="cargo +1.99.1"
  scripts/compat-fixtures.sh --generate --offline --keep`): eaf5320's
  `gbuild` + `gquery` debug build 27.6 s cold; fixtures 7 files, 100,488
  bytes (`kvstore.kv` 86,016, `vstree.kv` 8,192, snapshot 6,280);
  `tests/format_compat.rs` 0.14–0.26 s; old `gquery` on the current
  version-2 database: "cannot load database 'nv2.db': database error: not a
  gStore KV file"; old `restore_incremental`: "database error: unknown
  redo.log op 'C'".
- **Reproductions before the fixes** (each new test run against the
  unfixed code): an entry count of 40 — "range end index 4211 out of range
  for slice of length 4096" (disk_vstree.rs:226); a cycle — "has overflowed
  its stack … fatal runtime error: stack overflow, aborting"; three builds —
  the size assertion failed; `open_disk` of a missing path — `Ok`;
  `decode_u32s([FF FF FF FF 0F])` — "memory allocation of 17179869180 bytes
  failed" (SIGABRT); `decode_block` with a `u32::MAX` count — "memory
  allocation of 103079215080 bytes failed"; a damaged VList — `Ok([])`;
  `bulk::Stats` past `u32::MAX` and the forced `pred_stats` count — "attempt
  to add with overflow".

## Requests/Not yet

- **Edits outside the package's files** (all small, listed for the
  integrator): `src/bulk/rocks_sink.rs` `check_counters` — the `live`
  vector's element type `(u32, u32, u32)` → `(u64, u32, u32)` — and
  `write_pred_counters` — `u64::from(n)` → `n` for the `c|p` counter
  (clippy's useless conversion under `--features rocksdb`; one line each;
  r4-cli-config edits other functions of that file); `src/kvstore/store.rs`
  — `try_o_by_sp` (a VList that does not decode is an error) and
  `materialize_vlist` (an unknown tag is an error), both robust#11, besides
  the sanctioned counter sites (`insert_ids_in`, `delete_ids_in`,
  `rebuild_stats`, and the new `count_up` / `count_down` helpers after
  `type PredStat`); `src/query/engine.rs` `predicate_bit_classes`
  (sanctioned). New test files `tests/storage_corruption.rs` (robust#11,
  disk#4 end to end) besides the owned `tests/format_compat.rs`.
  `src/store/mod.rs` (in-memory `TripleStore`) needed no change: its
  per-predicate counts are `usize`.
- **Test corrected (D6):** `signature::disk_vstree::tests::a_vstree_file_with_feature_bits_is_refused`
  expected `DiskTree::build` over a file with feature bits to fail, because
  the build used to open the old file and keep its header; with disk#5 the
  build replaces the file, so the test now expects a clean rebuilt file
  (comment "review disk#5" in the test). `bulk::stats::tests::taps_equal_set_counts`
  compares the triple count as `u64` (the field's new type); nothing it
  checks changed.
- **disk#15** is fixed in its trivial half only (see Docs notes: the
  read-only pager mode is a D8 backlog entry).
- **gap-doc-claims#8 / disk#5 for an old file:** the eaf5320 `vstree.kv`
  in the fixture passes the new validation; a `vstree.kv` that an old
  binary rebuilt in place (disk#5's append) also does (the newest tree's
  children still precede their parents), but no fixture holds one.
- **Flaky test, not this package's:**
  `db::concurrency_tests::generation_oracle_under_a_rwlock`
  (src/db/concurrency_tests.rs:240) asserts that its four reader threads
  checked at least once while the writer thread made 60 inserts; under a
  load average of 12 the readers sometimes never get the lock (std's
  `RwLock` lets the writer's next `write()` win), so the assertion fails
  without a bug. A latch (the writer waits for a first reader check) would
  make it deterministic — for the test-hygiene package (R5).
- **CI:** `.github/workflows/compat.yml` is new (weekly, `workflow_dispatch`);
  the closing step confirms it on GitHub Actions (convention 12c). The
  script needs the full history (`fetch-depth: 0`) to check out eaf5320.
