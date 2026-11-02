# R3-SRV — parallel readers: RwLock servers, lock-free monitoring, PFN metadata cache

Package R3-SRV of `docs/PHASE6_PLAN.md` §6 (Round 3). Base: `0fb8842`
(Phase 6 Rounds 1 and 2 merged). Files: `src/server.rs`, `src/http_api.rs`,
`src/gapi/{ops_db,ops_query,ops_reason}.rs`, `src/pfn/registry.rs`,
`src/concurrent.rs`, `tests/dt_rw_server_stress.rs` (new),
`tests/dt_rw_crash.rs` (new), `tests/dt_gapi_db.rs`,
`tests/dt_http_durability.rs`, and — an ownership exception the brief asked
for — `examples/srv_mixed_load.rs` (new).

## Changelog

### Phase 6 R3-SRV: reads of one database run in parallel on both HTTP servers

- **The single-database server (`gstore::server::Server`, `gserver <dir>`)
  shares its database between readers.** `ServerInner.db` is a
  `RwLock<Database>` (poison-tolerant `read_locked` / `write_locked`). A
  request is parsed into a `Statement` before any lock is taken (a syntax
  error answers the same `400` without touching the database); a SELECT /
  ASK / CONSTRUCT / DESCRIBE then runs on a shared guard through
  `Database::read_as`, concurrently with other reads; an UPDATE takes the
  exclusive guard and runs through `Database::execute_as` with the code
  path unchanged (dirty flag, the check_point / closing save before the
  guard is released, the partial-update note).
- **The multi-database server (`ApiServer`, `gserver --api`, REST and the
  upstream `/api` protocol) does the same per database.** Each loaded
  database sits in a `DbCell` (a `RwLock<DbSlot>` with `read()` /
  `write()` / `try_write()`); statements are parsed once (the parse that
  classifies them for the privilege check is the one that runs, replacing
  the second parse of `is_update_sparql`) and routed:
  - shared: statement reads (also a transaction owner's read in
    `/tquery`), export, backup, `loadcsr`, `monitor` / `schema` while
    their statistics are not cached, reasoning Check;
  - exclusive: updates, begin / commit / rollback, checkpoint / save,
    upload, batch, unload / drop / restore / retire, the shutdown pass,
    reasoning Execute / Disable;
  - installing new `[query]` settings uses `try_write`; a database busy
    meanwhile takes them with its next request (`read_synced`: drop the
    shared guard, sync once under the exclusive one, read again).
  The transaction contract is unchanged (single writer, `409` for
  non-transactional writes during a transaction, begin-time snapshot reads,
  per-database tids, retired slots).
- **Re-entrancy is an error, not a deadlock, in every build.** A
  thread-local registry of held cells makes a second `read()` / `write()`
  of the same database on one thread return an error ("refused instead of
  deadlocking") — no thread holds two guards of a database or upgrades a
  shared guard while holding it.
- **Monitoring never waits for a database.** Each database publishes its
  figures (triples — the committed ones during a transaction — entities,
  literals, predicates, its mutation count, last save time, index /
  transaction / dirty flags) through a sequence lock at the end of every
  exclusive section and after it is loaded (`DbCounters`). `/status` (both
  servers), REST `/monitor` and `/show`, and the `/api` `show` / `stat`
  read them without the database lock; `/api monitor` / `schema` do too
  once their class statistics are cached for the published data (the cache
  now also keeps the subject count).
- **The RocksDB layout is locked like the others.** The servers' directory
  lock (`hold_dir_lock` in `DbSlot::handle` and `save_db`) now covers a
  RocksDB database's directory as well (`<dir>/LOCK` beside `rocksdb/`), so
  another process's lifecycle operation or bulk build sees it in use (the
  R2 review's request for the servers' calls).
- **Read errors (contract C2) in the remaining read paths.** A
  transaction's read snapshot (REST / `/api`), `ConcurrentDb` snapshot
  queries and `VersionView` queries evaluate inside a `ReadScope`: a read
  error a backend reports fails the query. Copying a store with no in-RAM
  copy (RocksDB) for a snapshot runs in a scope too: a `/begin` whose copy
  recorded an error fails, and a `ConcurrentDb` snapshot published from such
  a copy fails every query instead of answering from a partial copy.
- **PFN metadata cache and single-flight loads.** `PfnRegistry` keeps each
  user's parsed `data.json` as a shared `Arc<[PfnInfo]>` (filled under the
  registry's `data_lock` shared, replaced or dropped under it exclusively by
  every write), so concurrent `PFN.name(…)` queries and listings share one
  parse. Loading a module is single-flight per function: concurrent first
  calls wait for one load and share its module.
- **Tests.**
  - `tests/dt_rw_server_stress.rs` (new): eight readers, a writer of
    two-operation UPDATEs and (on `ApiServer`) a begin / write / own-read /
    commit-or-rollback cycle, REST and `/api` reads alternating. Every pair
    is seen whole, pending and rolled-back data never, and after a graceful
    shutdown the reopened database equals the acknowledged-write ledger.
    120 s watchdog; `GSTORE_RW_STRESS_SECS` (default 2) per test, about 3 s
    in all.
  - `tests/dt_rw_crash.rs` (new, unix): a real `gserver` in single-db and
    `--api` mode on the snapshot and disk layouts (and RocksDB with the
    `rocksdb` feature) is `SIGKILL`ed under three writers and two readers;
    every acknowledged write is in the reopened database and nothing unsent
    is. 10 seeds per case (`GSTORE_RW_CRASH_SEEDS`), about 10 s in a debug
    build.
  - Unit tests: two reads of one database meet in a two-party latch through
    single-db `/sparql`, REST `/sparql` and `/api` `query` (a serialized
    server times out); `/status`, `/monitor`, `/show` and `/api`
    `monitor` / `show` / `stat` answer within 1 s while a reader holds the
    database, while a writer does, and while a reader holds it with a
    writer queued; settings installed while two reads run are not waited
    for and apply with the next request; the re-entrancy guard; the counter
    sequence lock; snapshot read errors; the PFN entry cache; concurrent
    first PFN calls load the module once (`pfn-wasm`).
  - Existing tests changed only mechanically (the per-database lock API:
    `locked(h)` → `h.read()` / `h.write()` in `http_api`'s unit tests,
    `ReadSnapshot::capture(..)` → `.unwrap()`); no assertion changed.
  - Temp directories: `tests/dt_gapi_db.rs` removes each server root when
    its `Srv` drops (it joins the server first; also on a failed test),
    `tests/dt_http_durability.rs` (`gstore_durability_*`) and the
    `http_api` unit tests (`gstore_api_unit_*`) remove theirs when the test
    thread ends.
- **`examples/srv_mixed_load.rs` (new):** N keep-alive readers running one
  LUBM query plus one writer (a one-triple `INSERT DATA` every W ms) against
  a snapshot database, through `/sparql` and `/api`, read-only and with
  check_point off and on; prints reader qps / latency and writer latency.

## GAP_PLAN status line

R3-SRV: RwLock servers (shared reads, exclusive writes, statement parsed once, re-entrancy refused), lock-free monitoring counters, PFN metadata cache + single-flight module loads, ReadScope for snapshot / ConcurrentDb queries, rw stress and SIGKILL crash suites — done; G4 numbers: see Measured numbers (univ 38 left to the measure stage).

## Docs notes

- docs/ARCHITECTURE.md (or the server section of README.md): the locking
  model of both servers — shared reads, exclusive writes, the routing list
  above, lock-free monitoring, the lock order (server `RwLock` / `DbCell` →
  rule-directory lock → the database's internals: `persist_lock` → disk sink
  → `persist_error` → pager → `DiskTriples` gate → pager; never the
  reverse), and that a re-entrant guard is an error.
- docs/API.md / the REST section: `/status`, `/monitor`, `/show` and `/api`
  `show` / `stat` report the figures as of the last completed write (they
  do not wait for a running statement); `/api monitor` / `schema` likewise
  once computed for the current data.
- With check_point on, every UPDATE saves before it is acknowledged under
  the exclusive guard, so readers wait for the save: on a large snapshot
  database a frequent writer with check_point on lowers read throughput a
  lot (see Measured numbers); the disk and RocksDB layouts commit
  incrementally and do not have this cost.
- docs/BENCHMARKS.md: `examples/srv_mixed_load.rs` (usage in its header).

## Measured numbers

**Implementer run, noisy (other agents compiling and benchmarking on the same
4 vCPUs).** Release builds of base `0fb8842` (the prebuilt
`/tmp/gstore-base-0fb8842/bin`, and the same `examples/srv_mixed_load.rs`
built in a throwaway base worktree) and of this branch, measured in one
session, alternating base / branch twice, every run under
`flock /tmp/gstore-bench.lock`. Univ 8 = 1,029,275 triples, snapshot layout,
query cache off. The univ 38 numbers are left to the quiet measure stage.

### gstore-bench concurrency phase (univ 8, snapshot, 10 s per point)

```sh
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
flock /tmp/gstore-bench.lock <bin>/gstore-bench run --profile reference --scale 8 \
  --backends snapshot --reps 1 --query-procs 1 --open-reps 1 --no-update \
  --conc-clients 1,8 --conc-secs 10 --conc-queries q1,q4 --conc-endpoints sparql,api \
  --bin-dir <bin> --work-dir /tmp/r3srv-bench-work --out <result>.json
```

Mean of the two runs per side (qps; mean latency in ms):

| endpoint | query | clients | base qps | branch qps | base ms | branch ms |
|---|---|---|---|---|---|---|
| /sparql | Q4 | 1 | 4,741 | 4,743 | 0.208 | 0.208 |
| /sparql | Q4 | 8 | 6,146 | 19,780 | 1.302 | 0.445 |
| /sparql | Q1 | 1 | 8,761 | 8,246 | 0.113 | 0.120 |
| /sparql | Q1 | 8 | 12,366 | 41,541 | 0.646 | 0.190 |
| /api | Q4 | 1 | 2,179 | 2,357 | 0.411 | 0.378 |
| /api | Q4 | 8 | 4,568 | 7,840 | 1.696 | 1.008 |
| /api | Q1 | 1 | 3,726 | 3,580 | 0.256 | 0.270 |
| /api | Q1 | 8 | 9,803 | 11,255 | 0.799 | 0.710 |

Scaling 8 clients / 1 client, per run (branch run a, run b; base run a, run b):

- /sparql Q4: branch 5.48×, 2.82× (mean of runs 4.17×); base 1.28×, 1.31×.
- /sparql Q1: branch 5.64×, 4.45× (5.04×); base 1.37×, 1.45×.
- /api Q4: branch 4.18×, 2.55× (3.33×); base 2.33×, 1.87×.
- /api Q1: branch 3.29×, 2.97× (3.14×); base 2.55×, 2.71×.

Every point answered with one distinct body and no HTTP error.

(Since Round 1 Q4 takes about 0.2 ms at univ 8, so these numbers mostly
measure the HTTP stack; the pre-Phase-6 baseline had Q4 at 12 ms.)

### Mixed workload: `examples/srv_mixed_load.rs` (univ 8, snapshot, Q4)

```sh
flock /tmp/gstore-bench.lock <srv_mixed_load> \
  --data /tmp/gstore-bench-cache/lubm-like1-u8-s0.nt --secs 10 --readers 8 \
  --write-ms 100 --query q4 --scratch <tmp dir>
```

8 keep-alive readers; the writer sends one single-triple `INSERT DATA`
every 100 ms. Reader qps, runs a / b:

| endpoint | mode | base | branch |
|---|---|---|---|
| /sparql | read-only | 5,125 / 5,998 | 15,047 / 16,628 |
| /sparql | check_point off + writer | 4,989 / 5,518 | 15,350 / 16,261 |
| /sparql | check_point on + writer | 64 / 51 | 102 / 104 |
| /api | read-only | 4,908 / 4,868 | 8,323 / 10,580 |
| /api | check_point off + writer | 4,553 / 5,272 | 8,478 / 10,820 |
| /api | check_point on + writer | 38 / 46 | 38 / 34 |

The writer got all 100 writes acknowledged with check_point off (mean
1.2–2.1 ms each); with check_point on every update saves the univ 8
snapshot (≈ 85 MB) before it is acknowledged, 345–410 ms each, so the
writer manages 25–30 updates in 10 s and holds the database exclusively
almost all the time; readers get in only between two saves.

### Acceptance (plan §6 R3-SRV)

| item | result |
|---|---|
| Q4 qps at 8 clients ≥ 2.5 × 1 client, /sparql | **met** at univ 8: 5.48× and 2.82× (19,780 qps mean) |
| … and ≥ 215 qps | **met**: 13,218–26,341 qps |
| Q1 ≥ 2.0×, /sparql | **met**: 5.64×, 4.45× |
| mean Q4 latency at 8 clients ≤ 45 ms | **met**: 0.30–0.59 ms (/sparql), 0.79–1.22 ms (/api) |
| same ratios through /api | **met**: Q4 4.18×, 2.55× (≥ 215 qps: 6,258–9,423); Q1 3.29×, 2.97× |
| single-client latency within +5% | Q4 **met** (/sparql 0.208 → 0.208 ms, /api 0.411 → 0.378 ms); Q1 borderline in this noisy run (/sparql 0.113 → 0.120 ms, +6%; /api 0.256 → 0.270 ms, +5%; the two runs per side differ by up to 18%); left to the quiet stage |
| mixed, check_point off: reader qps ≥ 2.0 × pre read-only 8-client (pre: 76.3 /sparql, 76.5 /api) | **met**: 15,350 / 16,261 (/sparql), 8,478 / 10,820 (/api) |
| mixed, check_point on: reader qps ≥ pre | /sparql **met** (102 / 104 ≥ 76.3); /api **not met** (38 / 34 < 76.5; base 38 / 46). Bound by the full snapshot save each update makes under the exclusive guard (see Requests) |
| univ 38 | left to the measure stage (commands below) |

### For the quiet measure stage (univ 38)

Build `cargo build --release -p gstore -p gstore-bench --bins --example
srv_mixed_load` on the merged tree, then:

```sh
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
# G4 concurrency, base (pre-Phase-6 JSON already holds it) vs merged:
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale 38 \
  --backends snapshot --reps 1 --query-procs 1 --open-reps 1 --no-update \
  --conc-clients 1,8 --conc-secs 10 --conc-queries q1,q4 --conc-endpoints sparql,api \
  --bin-dir target/release --work-dir /tmp/r3srv-work38 --out r3srv-conc-u38.json
# mixed workload (8 readers + 1 writer every 100 ms; check_point off and on):
flock /tmp/gstore-bench.lock target/release/examples/srv_mixed_load \
  --data /tmp/gstore-bench-cache/lubm-like1-u38-s0.nt --secs 20 --readers 8 \
  --write-ms 100 --query q4 --scratch /tmp/r3srv-mixed38
rm -rf /tmp/r3srv-work38 /tmp/r3srv-mixed38
```

(Repeat the univ 8 pair with `--scale 8` / `lubm-like1-u8-s0.nt` for the
quiet univ 8 numbers; the pre-Phase-6 read-only 8-client Q4 throughput to
compare the mixed numbers with is in `bench/results/pre-phase6-lubm{8,38}.json`.)

### Test times (debug build, implementer machine)

`dt_rw_server_stress` 2.5 s (2 tests, 2 s of load each: about 900 /sparql
and 780 /api reader rounds, 20 two-operation writes, 19 transactions);
`dt_rw_crash` 9.3 s (4 cases × 10 seeds: 128 / 605 / 84 / 446 acknowledged
writes checked after `SIGKILL` for single-db snapshot / single-db disk /
api snapshot / api disk).

## Requests/Not yet

Requests (outside R3-SRV's files):

1. **R4-SNAP (owner of `http_api.rs` / `gapi/ops_db.rs` in Round 4) — bulk
   builds from the servers.** No server path calls `Database::build_disk*`
   today, so R3-STREAM's delegation of those to `bulk::build` needs no server
   change: REST `/build` builds in memory and saves a snapshot after retiring
   the loaded copy (`retire_loaded`, which drops the database and with it its
   directory handle), and `/api build` refuses an existing database and
   builds into a staging directory. When R4-SNAP adds `layout=disk|rocksdb`
   (through `bulk::build`), REST `/build` must retire the loaded handle
   *before* calling it and must not hold its `DirLock::acquire_existing`
   probe across it (`bulk::build` takes the directory lock itself and refuses
   a directory `DirLock::held_here`), then load the result as it does now.
2. **Check_point on, snapshot layout (G4 mixed item; R4-SNAP / R4-CLOSURE).**
   Every acknowledged UPDATE rewrites the whole snapshot under the
   exclusive guard (the brief's "save before release"); at univ 8 that is
   350–410 ms, so a writer every 100 ms leaves readers only the gaps between
   saves (/api 34–38 qps vs the 76.5 qps target; base the same). Options: (a)
   save on a shared guard after the update (readers keep running, but may
   then see an update before it is durable — a weaker guarantee than today,
   which is why this package did not do it); (b) an incremental /
   journal-based snapshot commit (`db/persist.rs`); (c) serve such databases
   from the disk layout (R4-SNAP), whose commit point is incremental.
3. **Temp-directory leaks in files R3-SRV does not own.** Test roots that
   pile up in `/tmp` across runs (prefix — file): `gstore_gapi_` —
   `tests/dt_gapi_core.rs` (and `gstore_gapi_users_` / `gstore_gapi_async_wiring_`
   — `tests/dt_gapi_users.rs`, `tests/dt_gapi_async_wiring.rs`);
   `gstore_api_` — `tests/dt_http_parity.rs`; `gstore_gtasks_` —
   `tests/dt_gapi_tasks.rs`; `gstore_p1rev_` — `tests/dt_p1_review.rs`;
   `gstore_secrev_` — `tests/dt_security_review.rs`; `gstore_wiring_` —
   `tests/dt_phase0_wiring.rs`; `gstore_rsn_` — `tests/dt_reason_manage.rs`;
   `gstore_p4fix_` — `tests/dt_phase4_review_fixes.rs`; `gstore_dtpfn_` —
   `tests/dt_pfn.rs`; `gstore_review_` — `src/db/review_tests.rs` (thousands
   each on this machine). The pattern used here works for them too: a
   thread-local list of the test's directories removed when the test thread
   ends, or a guard owned by the server handle. (`tests/dt_gserver_lifecycle.rs`,
   `tests/upstream_transaction.rs`, the new tests and the files changed here
   leave nothing.)
4. **R3-STREAM / Database owner (note, no change needed):** the servers rely
   on `Database::stats()`, `mutation_count()`, `read_as`, `query_read`,
   `execute_as`, `preload_csr`, `backup`, `subject_num`, `type_statistics`
   and `schema_graph` being `&self` and internally synchronized (they are);
   `reason_check` takes `&mut self`, so the server's reasoning Check runs the
   rule's check query itself on a shared guard (`manage::check` with
   `DbSlot::read_cancellable`). A Stream-mode `Database` must keep these
   `&self` and cheap (`stats()` is called at the end of every write section
   to publish the monitoring counters).
5. **`bench/src/runner/conc.rs` (R4-CLOSURE):** unchanged and still
   compiling (`Server::bind`, `shutdown_handle`, `serve_forever`,
   `ApiServer::bind`, `with_query_settings`, `QuerySettings` kept). A
   mixed-workload point (readers plus a paced writer) in gstore-bench would
   replace `examples/srv_mixed_load.rs`.

Not yet:

- univ 38 numbers (measure stage, commands above); the quiet Q1
  single-client comparison.
- No disk-layout (Stream mode) concurrency numbers: Stream mode lands with
  R3-STREAM; G4's "Stream-disk databases" figures belong to R4-SNAP /
  R4-CLOSURE.
- The re-entrancy registry refuses a second guard of the *same* database on
  one thread; holding guards of two different databases is not refused (no
  path does it; C8 forbids it).
