# Phase 6 R4 merge — integration of R4-RECLAIM, R4-SNAP, R4-VS and R4-CLOSURE-A

The Round-4 integrator's own changes on `phase6` (base `d702463`): the four
packages merged in order (`p6/r4-reclaim` partial, `p6/r4-snap`,
`p6/r4-vs`, `p6/r4-closure-a`), the wiring between them, the Round-4
requests that were small and safe, the test temp-directory cleanup, and the
restore lock of the R3 review's second pass.

Ownership: every package stayed inside its files, with two exceptions,
both accepted: R4-RECLAIM changed a test of `src/kvstore/pager.rs` (its
"unsupported feature" assertions move from `FEATURE_FREE_IDS`, now
supported, to `FEATURE_LONG_KEYS`; test-only) and one test of its own
module's `src/db/stream_review_tests.rs` (that test relies on deleted terms
keeping their ids, so it turns reclamation off); R4-CLOSURE-A named its
fragment `phase6-R4-CLOSURE-A.md` (the brief's name) instead of the plan's
`phase6-R4-CLOSURE.md`. R4-RECLAIM reported `partial` (two acceptance
bounds missed, see its fragment); it builds, its suite passes and its
answers are correct, so it merged (plan §5.7).

## Changelog

- **Batched writes on in-memory stores (behaviour unchanged, faster).**
  `INSERT DATA`, `DELETE DATA`, `DELETE/INSERT … WHERE`, `LOAD` (and the
  undo of a failed one), `ADD`/`COPY`/`MOVE`, `gadd`/`gsub` and `/api
  batchInsert`/`batchRemove`, and transaction rollback now hand the
  default-graph triples of a statement (or import batch) to the store at
  once (`TripleStore::insert_many` / `remove_many`, one merge or
  compaction pass per touched adjacency list) instead of one sorted insert
  per triple. With lowest-first id reuse a new term gets a low id, and the
  per-triple path moved most of each long list once per triple
  (memory-layout inserts were 4.7× slower with reclamation on in
  R4-RECLAIM's churn). Results, counts, error semantics (the statements
  before a failing one stay applied), undo/redo logs, write-through and
  reclamation are those of the one-by-one path; Stream-mode and RocksDB
  stores still write one triple at a time. The VS-tree entry of an entity
  with no edge before the batch (a new term or a reused id) is replaced by
  the union of its batch edges — its exact signature — without reading the
  store.
- **A disk `/begin` snapshot no longer defers the commit's id frees.** The
  read snapshot of a Stream-mode transaction reads its dictionary only
  inside its `DiskReadView` (drained by the writer gate before any free),
  so it takes an unpinned dictionary clone; the pinned clone made the
  commit defer every free into memory, where an unload or restart lost
  them (orphans until `gcompact --reclaim`). `ConcurrentDb` snapshots of
  Stream databases stay pinned (they resolve a materialized store outside
  any read view); in-memory snapshots own copy-on-write versions of their
  ids.
- **`[system] id_reclaim`, `[storage] rocksdb_block_cache_mb` and
  `rocksdb_bloom` work from `conf.ini`.** gserver installs them at startup
  and `/api refreshconf` re-installs them (`GStoreConfig::
  install_storage_defaults` next to the open options); before, only
  `GSTORE_ID_RECLAIM` / `GSTORE_ROCKS_BLOCK_CACHE_MB` had an effect. The
  startup and `refreshconf` log lines name the reclamation policy.
- **`[storage] default_layout` is no longer "reserved":** REST `/build`,
  `/api build` and `gbuild` use it (R4-SNAP); gserver's "has no effect yet"
  warning and `GStoreConfig::default_layout_warning` are gone.
- **A restore that creates its database holds a lock beside it.**
  `grestore`, REST `/restore` and `/api restore` into a database directory
  that does not exist yet hold `<dir>.restore-lock` (new
  `dirlock::SideLock`; removed by its last holder) from before staging
  until the swap, and the restore repair that `Database::open` runs takes
  it too: another process opening the new database meanwhile is refused
  instead of deleting the staged copy (R3 review, second pass).
- **VS-tree policy:** the library's own unit tests run under the default
  `Auto` policy like everything else (the two `db::tests` that check the
  out-of-core tree's page reads scope `VsPolicy::Tree`).
- `Dockerfile`: the image ships `gcompact`.
- Removed `examples/srv_mixed_load.rs`: gstore-bench's mixed phase covers
  it on every layout.
- Tests: every test helper that makes a temp directory (or store file)
  registers it for removal when the test thread ends — also when the test
  fails — so a full `cargo test --workspace` leaves nothing in the temp
  directory. New `tests/dt_r4_merge.rs` (batched memory writes against the
  one-by-one Stream path, `ConcurrentDb` snapshots across id reuse, a
  server-built disk database's commit frees despite its `/begin`
  snapshot); `dt_gserver_lifecycle`'s `[system] id_reclaim` /
  `default_layout` test of the gserver binary; `db::dirlock` and
  `db::stream_review_tests` tests of the restore side lock.

## GAP_PLAN status line

Phase 6 Round 4 merged (R4-RECLAIM, R4-SNAP, R4-VS, R4-CLOSURE-A) with the
wiring between them: batched in-memory writes under id reuse, unpinned disk
transaction snapshots, conf.ini storage defaults in gserver; the quiet
measure stage and the docs closure (R4-CLOSURE) remain.

## Docs notes

- docs/STORAGE_BACKEND.md (id reclamation): in-memory stores apply each
  statement's default-graph writes as one batch; which snapshots pin a
  dictionary (a `ConcurrentDb` snapshot of a Stream database) and which do
  not (in-memory clones; a disk `/begin` snapshot, which reads inside its
  view).
- docs/CONFIG / conf.ini docs: `[system] id_reclaim`, `[storage]
  rocksdb_block_cache_mb` / `rocksdb_bloom` are read by gserver at startup
  and by `refreshconf`; `[storage] default_layout` is effective.
- docs/DEPLOY.md: `gcompact` in the image and the packages' binary lists;
  a restore that creates a database leaves `<name>.db.restore-lock` beside
  it only while it runs.
- README / docs/BENCHMARKS.md: `examples/srv_mixed_load.rs` is gone; use
  gstore-bench's mixed phase.

## Measured numbers

R4-RECLAIM request 6 (memory-layout inserts with id reuse), on this
integration branch: `examples/reclaim_churn.rs --layout memory --base 8
--rounds 10`, data the first 18 universities of
`/tmp/gstore-bench-cache/lubm-like1-u38-s0.nt`, release build, one process
per policy, under `flock /tmp/gstore-bench.lock`, shared 4-core machine
(load average about 2): **noisy, for the quiet measure stage to confirm**.
"before" is the merged tree before the batching commit, "after" the tree
with it; totals over the ten rounds (1,315,190 triples deleted and as many
inserted):

| build | policy | insert total | delete total | insert, first university |
|---|---|---:|---:|---:|
| before | on (Immediate) | 52.1 s | 56.3 s | 8.81 s |
| before | off | 13.9 s | 90.0 s | 1.35 s |
| after | on (Immediate) | **12.5 s** | **12.0 s** | 1.26 s |
| after | off | 11.6 s | 9.7 s | 1.22 s |

Insert on/off went from 3.76× to **1.08×**; delete on/off is 1.23× (it was
0.62× in R4-RECLAIM's run and 0.63× in "before": the batched removal made
deletes 4.7× / 9.3× faster under on / off, and what remains of the
difference is that reused low ids sit near the front of the per-predicate
lists, so each round's batch compacts more of each list than with ids
appended at the end). Live counts, limits and RSS are unchanged by the
batching (live = referenced in all rounds, limit / peak live 1.000, RSS
1.54× of round 0, 1.13× of round 1).

Commands:

```sh
cargo build --release -p gstore --example reclaim_churn
for p in on off; do
  flock /tmp/gstore-bench.lock target/release/examples/reclaim_churn \
      --data /tmp/gstore-bench-cache/lubm-like1-u38-s0.nt --layout memory \
      --policy $p --base 8 --rounds 10 --scratch <dir>
done
```

## Verification

On the merged and wired tree (`e941a2b`; the commit after it adds only this
fragment), toolchain 1.99.1, `CARGO_PROFILE_DEV_DEBUG=0
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`, every job of ci.yml and
bench.yml with its commands:

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `GSTORE_DISK_QUERY_MODE=memory cargo test --workspace`: 2,351 passed,
  0 failed, 6 ignored (the micro-benchmarks and latency drivers, all
  pre-existing or the packages'); `GSTORE_DISK_QUERY_MODE=stream`: the
  same. Neither run left a new entry in /tmp (checked against a marker
  file).
- Feature matrix (`cargo build --locked --profile test -p gstore
  --all-targets --features F`, `cargo clippy -p gstore --all-targets
  --features F -- -D warnings`, `cargo test -p gstore --features F`):
  gzip 2,129 passed, tls 2,130, zip 2,142, readline 2,126, pfn-wasm 2,148,
  `zip pfn-wasm readline` 2,164; 0 failed, 5 ignored each; build and
  clippy clean each.
- `scripts/conformance.sh`: basic, update, transaction, api, parser
  (failing cases 15 and 17, the known divergences) and python (49 SDK
  checks) all PASS.
- RocksDB (`CARGO_TARGET_DIR=target-rocks`): `cargo clippy -p gstore
  --all-targets --features rocksdb -- -D warnings` clean; `cargo test -p
  gstore --features rocksdb` 2,178 passed, 0 failed, 6 ignored.
- bench.yml rocksdb-correctness: `cargo build --profile test --locked -p
  gstore --bin gbuild --features rocksdb`, `cargo test -p gstore-bench
  --features rocksdb`: 66 passed, 1 ignored.
- bench.yml smoke: `cargo build --release --locked -p gstore -p
  gstore-bench --bin gbuild --bin gstore-bench`; `gstore-bench report
  --check README.md CHANGELOG.md` exit 0; `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache gstore-bench ci --baseline
  bench/baselines/smoke.json`: PASS, 307 gates checked, 0 failed, the
  WatDiv-like workload reported as new; the canary (`check
  --self-test-slowdown 3` of the result against itself) exits 1.
- Fixed on the way: `dt_bulk_crash::a_rocksdb_store_in_use_is_not_replaced`
  failed once in a full run (a flake R4-VS saw too): a child process
  another test spawned held the released lock between its fork and exec;
  the test now retries the rebuild briefly after the release.

## Requests/Not yet

Left recorded, with their owners:

- R4-CLOSURE (docs): R4-RECLAIM's, R4-SNAP's, R4-VS's and this fragment's
  Docs notes; `docs/CPP_PARITY.md` line 47; `GSTORE_VS_POLICY`; the
  `gcompact` rows; fold the fragments; the final reference runs, the smoke
  re-bless with the gstore-bench/2 tool and the report markers
  (R4-CLOSURE-A requests 2 and 3).
- The quiet measure stage: every package's measure instructions (churn on
  memory, Stream and a release RocksDB build; gstore-bench reference univ 8
  and 38 for snapshot, Stream, Materialize and RocksDB; `/begin`,
  `ConcurrentDb` update latency, Stream Q4 1→8 clients, `/api monitor` at
  univ 38), the G1d verdict and, if every gate passes, the
  `DEFAULT_DISK_QUERY_MODE` flip (R4-VS request 3).
- R4-RECLAIM's two missed bounds (memory RSS 1.55× a bulk-built round 0,
  1.14× of round 1; Stream `kvstore.kv` 1.47× a bulk-built round 0, flat
  from round 8) are accepted as measured against a round 0 that is a fresh
  bulk image; the plan's bounds assumed a steady-state round 0. The delete
  on/off ratio of the memory layout (1.23× above, gate ≤ 1.10) is for the
  quiet stage to confirm or record.
- `db/persist.rs` owner (G4): a save from a captured copy-on-write image so
  check_point saves can release the exclusive guard (R4-SNAP request 2).
- `src/db/fsutil.rs`: `read_bincode` from an in-memory slice (R4-SNAP
  request 3) — not done: reading a whole snapshot file first would double
  the open's peak memory for large `store.bin` files; needs a measured
  trade-off.
- `/api stat` query mode and dictionary cache counters (R4-SNAP request 4,
  optional).
- `src/kvstore/disk_triples.rs`: a per-epoch cache of long range counts
  (R4-VS request 1); `src/kvstore/store.rs`: `SpoWalk::collect` through
  `LeafCursor::seek_in_leaf` (R4-VS request 2, R3-STREAM request 6) —
  performance work for a measured follow-up.
- A `Database`-side "tree is exact" signal so `Auto` can store-check
  in-memory trees (R4-VS request 4, optional).
- A `report --check README.md CHANGELOG.md` step in ci.yml (R4-CLOSURE-A
  request 4, optional; the files have no markers yet).
- `ConcurrentDb` snapshots of Stream and RocksDB databases still
  materialize the store per publish, and a `ConcurrentDb` over a Stream
  database defers its frees while its base snapshot lives (versioned reads,
  D2).
- No automatic VS-tree rebuild threshold for freed entities (phantoms until
  `rebuild_index`); Stream frees deferred by a pin are in memory only
  (R4-RECLAIM's notes).
- In-memory dictionaries are copy-on-write per interner (R2-DICT): while a
  snapshot shares one (a `ConcurrentDb` publish, an open `/begin`), the
  first write that changes it copies that interner — before Round 4 only an
  insert of a new term did, now a delete that frees a term does too (once
  per published version, not per triple). A spare copy kept like the
  store's (`TripleStore::keep_spare_copies`) would make it O(changes);
  measure with the `ConcurrentDb` update-latency driver first.
