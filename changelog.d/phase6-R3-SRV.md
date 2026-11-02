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

R3-SRV: RwLock servers (shared reads, exclusive writes, statement parsed once, re-entrancy refused), lock-free monitoring counters, PFN metadata cache + single-flight module loads, ReadScope for snapshot / ConcurrentDb queries, rw stress and SIGKILL crash suites — done; G4 numbers (quiet post-merge stage, univ 8 and 38): Q4 8 / 1 clients 4.9–5.4× (/sparql), 4.1–4.4× (/api), single-client latency within ±3.1% of the base, mixed workload with check_point off met; with check_point on below pre (full snapshot save per update) and `/api monitor` over 100 ms right after a data change or during a save's fsync — recorded for R4.

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

### Quiet post-merge stage (phase6 `77f7ed0`: R3 merged, review fixes)

Reference VM (4 vCPU Intel Xeon @ 2.10GHz, 16 GB), nothing else running:
steal ≤ 1% (logged every 10 s from `/proc/stat`), CPU busy 24–98% while
measuring — the measured process itself: at the 8-client points the 8
clients and the server's workers saturate the 4 vCPUs. Branch = release
build of `77f7ed0` (`cargo build --release -p gstore -p gstore-bench --bins
--example srv_mixed_load`); base = the prebuilt release binaries of
`0fb8842` (`/tmp/gstore-base-0fb8842/bin`, its own `gstore-bench` included:
the concurrency phase runs the servers in-process, so the base harness
measures the base server code) and the same `examples/srv_mixed_load.rs`
built at `0fb8842` in a throwaway worktree. Same session, base and branch
alternating, every heavy command under `flock /tmp/gstore-bench.lock`
(19:16–20:08 UTC). Data from `/tmp/gstore-bench-cache` (univ 8
`ed44da2e7b3bbb98`, 1,029,275 triples; univ 38 `fcd6493725f3c66a`,
5,181,103 triples). Snapshot layout, query cache off. "pre" =
`bench/results/pre-phase6-lubm{8,38}.json`.

#### gstore-bench concurrency phase (snapshot, 10 s per point)

```sh
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
flock /tmp/gstore-bench.lock <bin>/gstore-bench run --profile reference --scale {8,38} \
  --backends snapshot --reps 1 --query-procs 1 --open-reps 1 --no-update \
  --conc-clients 1,8 --conc-secs 10 --conc-queries q1,q4 --conc-endpoints sparql,api \
  --bin-dir <bin> --work-dir /tmp/r3srv-work{8,38} --out <result>.json
```

Univ 8: three runs per side (base a, branch a, base b, branch b, base c,
branch c); univ 38: two per side. The first branch run of each scale is
committed as `bench/results/phase6-r3-conc-lubm{8,38}.json`, with
`gstore-bench compare` against the first base run of the same session in
`phase6-r3-conc-lubm{8,38}-compare.md` (**PASS, 104 gates** at both scales;
the base files stay out of the tree; their `git_commit` names the working
tree, `77f7ed0`, their `config.bin_dir` the base binaries). Every point answered with one distinct
body and no HTTP error, and the bodies (`body_fnv64`) equal the base's and
pre's at both scales. Means of the runs (qps; mean latency in ms):

| univ | endpoint | query | clients | pre qps | base qps | branch qps | base ms | branch ms |
|---|---|---|---:|---:|---:|---:|---:|---:|
| 8 | /sparql | Q4 | 1 | 81.0 | 4,972 | 4,886 | 0.198 | 0.202 |
| 8 | /sparql | Q4 | 8 | 76.3 | 6,500 | **24,399** | 1.228 | **0.322** |
| 8 | /sparql | Q1 | 1 | 2,001 | 9,131 | 9,009 | 0.108 | 0.110 |
| 8 | /sparql | Q1 | 8 | 2,099 | 13,361 | **47,457** | 0.598 | **0.163** |
| 8 | /api | Q4 | 1 | 85.6 | 2,324 | 2,370 | 0.386 | 0.375 |
| 8 | /api | Q4 | 8 | 76.5 | 5,518 | **10,134** | 1.385 | **0.737** |
| 8 | /api | Q1 | 1 | 1,617 | 4,311 | 4,398 | 0.222 | 0.216 |
| 8 | /api | Q1 | 8 | 2,293 | 10,724 | **13,790** | 0.731 | **0.565** |
| 38 | /sparql | Q4 | 1 | 17.1 | 3,830 | 3,837 | 0.258 | 0.258 |
| 38 | /sparql | Q4 | 8 | 13.4 | 4,511 | **20,234** | 1.773 | **0.389** |
| 38 | /sparql | Q1 | 1 | 574 | 4,017 | 3,912 | 0.248 | 0.254 |
| 38 | /sparql | Q1 | 8 | 585 | 3,990 | **19,739** | 2.003 | **0.400** |
| 38 | /api | Q4 | 1 | 17.8 | 2,032 | 2,051 | 0.445 | 0.440 |
| 38 | /api | Q4 | 8 | 15.4 | 4,582 | **8,545** | 1.690 | **0.878** |
| 38 | /api | Q1 | 1 | 597 | 2,474 | 2,404 | 0.391 | 0.402 |
| 38 | /api | Q1 | 8 | 618 | 4,211 | **9,694** | 1.884 | **0.809** |

Scaling 8 clients / 1 client, per run (branch; base for contrast):

- /sparql Q4: univ 8 **4.91×, 5.09×, 4.98×** (base 1.32×, 1.34×, 1.27×);
  univ 38 **5.15×, 5.40×** (base 1.27×, 1.09×).
- /sparql Q1: univ 8 **5.39×, 5.27×, 5.14×** (base 1.46×, 1.40×, 1.53×);
  univ 38 **5.16×, 4.94×** (base 1.02×, 0.97×).
- /api Q4: univ 8 **4.06×, 4.37×, 4.39×** (base 2.40×, 2.24×, 2.50×);
  univ 38 **4.15×, 4.19×** (base 2.37×, 2.14×).
- /api Q1: univ 8 **2.95×, 3.24×, 3.23×** (base 2.53×, 2.45×, 2.49×);
  univ 38 **3.98×, 4.08×** (base 1.68×, 1.72×).

Single-client mean latency, branch vs base (mean of the runs; median of the
runs in brackets): univ 8 /sparql Q1 +1.5% (+0.3%), Q4 +1.9% (+1.0%), /api
Q1 −2.5% (−0.2%), Q4 −3.1% (+0.9%); univ 38 /sparql Q1 +2.7%, Q4 −0.3%,
/api Q1 +2.8%, Q4 −1.2%. The run-to-run spread of one side is up to 8%
(/sparql) and 20% (/api, base run c); the fastest branch run against the
fastest base run is within +2% at univ 8 and +4.5% / +5.5% for Q1 at univ
38 (/sparql / /api: 0.252 vs 0.241 ms, 0.400 vs 0.379 ms), inside that spread.

The query phase of these runs (one process, one warm repetition, kept as
short as the CLI allows) is noise-level only: per query the branch / base
ratio of the run means is 0.72–1.16 at univ 8 and 0.77–1.29 at univ 38, in
both directions; its gates pass. The R3 query latencies are the reference
runs' business, not this stage's.

#### Mixed workload: `examples/srv_mixed_load.rs` (snapshot, Q4, 20 s per mode)

```sh
flock /tmp/gstore-bench.lock <srv_mixed_load> --data /tmp/gstore-bench-cache/lubm-like1-u{8,38}-s0.nt \
  --secs 20 --readers 8 --write-ms 100 --query q4 --scratch /tmp/r3srv-mixed{8,38}
```

8 keep-alive readers; one writer sends a single-triple `INSERT DATA` every
100 ms. Reader qps, runs a / b (order: branch a, base a, branch b, base b
per scale); 0 read errors and 0 write errors in every run:

| univ | endpoint | mode | target | base | branch |
|---|---|---|---:|---:|---:|
| 8 | /sparql | read-only | — | 6,949 / 6,042 | 25,642 / 23,857 |
| 8 | /sparql | check_point off + writer | ≥ 152.7 | 6,360 / 6,024 | **26,783 / 23,116** |
| 8 | /sparql | check_point on + writer | ≥ 76.3 | 26.5 / 27.0 | **47.5 / 41.1** |
| 8 | /api | read-only | — | 5,754 / 5,987 | 10,816 / 10,723 |
| 8 | /api | check_point off + writer | ≥ 153.0 | 5,399 / 6,107 | **10,776 / 9,689** |
| 8 | /api | check_point on + writer | ≥ 76.5 | 25.7 / 24.8 | **35.4 / 35.7** |
| 38 | /sparql | read-only | — | 4,281 / 4,377 | 19,048 / 18,525 |
| 38 | /sparql | check_point off + writer | ≥ 26.8 | 4,513 / 4,422 | **20,298 / 20,029** |
| 38 | /sparql | check_point on + writer | ≥ 13.4 | 5.3 / 4.5 | **7.7 / 10.3** |
| 38 | /api | read-only | — | 4,260 / 4,383 | 10,935 / 9,403 |
| 38 | /api | check_point off + writer | ≥ 30.8 | 4,160 / 4,245 | **10,121 / 9,396** |
| 38 | /api | check_point on + writer | ≥ 15.4 | 6.3 / 8.9 | **8.4 / 8.2** |

(targets: 2 × and 1 × pre's read-only 8-client Q4 qps — univ 8 76.3
/sparql, 76.5 /api; univ 38 13.4 / 15.4.) With check_point off the writer
got 200–201 writes acknowledged in each 20 s window, mean 0.75–1.44 ms
(base 1.55–2.24 ms). With check_point on each update saves the whole
snapshot before it is acknowledged: 317–322 ms (/sparql) and 310–311 ms
(/api) at univ 8 (base 328–329 / 301–308 ms), 1.30–1.32 s and 1.24–1.25 s
at univ 38 (base 1.30–1.32 / 1.23–1.27 s), so the writer, due every 100 ms,
runs back to back (61–67 updates in 20 s at univ 8, 16–17 at univ 38) and
holds the database almost all the time; readers get in between two saves
only — 10–15 reads per update on the branch, 6–11 on the base (about one
per reader).

**Profile of the check_point-on exclusive section** (brief, callgrind,
univ 1, `--endpoints sparql --modes checkpoint-on --readers 1 --secs 4`,
collection toggled on `ServerInner::execute`, 21 updates): 92.9% of the
instructions are `Database::save` — bincode serialization of the
`TripleStore` (45.3%), the VS-tree (26.1%) and the dictionary (21.3%); the
update itself (`Database::execute_as`) 2.9%, the reads 3.3%. Callgrind does
not see the fsync wait, which adds to the save's wall time. With check_point
off the same updates take 0.75–1.44 ms, so the save is about 99.5% of an
update's time at univ 8 and more at univ 38. Same save time on the base: no
regression from this round; the shortfall is the full-snapshot save under
the exclusive guard that the brief's "save before release" requires (see
Requests).

#### Monitoring while a long query runs and while a write holds the database

Real `gserver` processes (branch `77f7ed0`, then base `0fb8842`) on a univ
38 snapshot database built by their own `gbuild` (411 MB), check_point on;
a Python client (fresh connection per request). A "long query" is
`SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o . FILTER(STRLEN(STR(?o)) > 100k) }`
(k varied, so no cache answers it; 1.97–2.60 s); a "write" is a one-triple
`INSERT DATA` (1.16–1.63 s: the check_point save; an ASK sent 150 ms into
it took 1.0–1.5 s, so the database was held). From 150 ms into the hold
until it ends, each monitoring endpoint is requested in turn (every ~20 ms
single-db, every ~220 ms with `/api stat` in the loop); 4 long queries, 4
writes, and 2 long queries with a write queued behind them (sent 150 ms
in, answered after 3.2–3.8 s) per server. Max latency of the requests sent
and answered inside the hold:

| server | endpoint | long query | write holds | long query + queued write | base `/status` (any hold) |
|---|---|---:|---:|---:|---:|
| single-db | `GET /status` | 4.7 ms (one 47 ms outlier) | 4.2 ms | 3.0 ms | 1,042–3,691 ms |
| `--api` | `GET /status` | 3.1 ms | 1.0 ms | 1.9 ms | 1,180–2,366 ms |
| `--api` | REST `GET /monitor` | 2.1 ms | 1.9 ms | 0.7 ms | |
| `--api` | REST `GET /show` | 1.2 ms | 0.9 ms | 2.8 ms | |
| `--api` | `/api show` | 0.9 ms | 2.1 ms | 1.4 ms | |
| `--api` | `/api monitor` | 5.5 ms | **75–150 ms** | **120–159 ms** | |
| `--api` | `/api stat` | 203 ms | 206 ms | 203 ms | |

(At idle: `/status` 0.4–0.7 ms; `/api stat` 201–204 ms on both binaries:
it samples CPU usage over 200 ms by design, upstream's semantics, and never
takes a database lock; `/api monitor` 1 ms with its class statistics
cached, 0.89–0.94 s for the first call.) On the base the first request of
each round, `/status`, waited for the hold to end (1.0–3.7 s), so no other
base endpoint was sampled inside one.

`/api monitor`, traced per request (same setup, own server, monitor in a
tight loop):

- With its class statistics current it answers in ≤ 10 ms in every hold.
- The **first `/api monitor` after the data changed** recomputes the
  subject count, class statistics and schema graph: 0.69–0.82 s at univ 38
  with the database free, 0.83–0.92 s when the first request comes while a
  long query runs (it computes on a shared guard beside the query: it
  does not wait for the lock, but it is not cheap either) and up to 0.88 s
  when it takes the shared guard just before a write, which then waits
  behind it.
- In the last ~300 ms of a check_point save, `/api monitor` took 58–217
  ms: its `diskUsed` walks the database directory and stats the files the
  save is fsyncing; a plain `stat` of those files from another process
  stalls 240–301 ms at the same moment, `/api monitor` with `disk=0` stays
  ≤ 18 ms, `/api show` ≤ 6 ms. File-system, not lock, latency.

So monitoring no longer waits for a statement on either server (base: 1.0–3.7
s), and every figure except `/api monitor`'s statistics is served in ≤ 6 ms
(≤ 47 ms worst sample) at univ 38. `/api monitor` misses 100 ms in two
cases: right after a write (statistics recomputed, ~0.7–0.9 s at univ 38;
the base did the same under its lock) and during the fsync tail of a
check_point save (its directory walk); see Requests.

#### Acceptance (plan §6 R3-SRV and the G4 items that exist now), quiet

| item | target | measured | verdict |
|---|---|---|---|
| Q4 qps 8 clients / 1 client, /sparql | ≥ 2.5× | univ 8 4.91×, 5.09×, 4.98×; univ 38 5.15×, 5.40× (base 1.09–1.34×; pre 0.94× / 0.78×) | **met** |
| Q4 qps at 8 clients | ≥ 215 qps | /sparql 23,235–25,241 (univ 8), 20,056–20,411 (univ 38); /api 9,289–10,583, 8,402–8,689 | **met** |
| Q1 8 clients / 1 client, /sparql | ≥ 2.0× | univ 8 5.39×, 5.27×, 5.14×; univ 38 5.16×, 4.94× | **met** |
| Mean Q4 latency at 8 clients | ≤ 45 ms | /sparql 0.311–0.337 ms (univ 8), 0.386–0.392 (univ 38); /api 0.704–0.802, 0.864–0.891 | **met** |
| Same ratios through /api | Q4 ≥ 2.5×, ≥ 215 qps; Q1 ≥ 2.0× | Q4 4.06×, 4.37×, 4.39× / 4.15×, 4.19×; Q1 2.95×, 3.24×, 3.23× / 3.98×, 4.08× | **met** |
| Single-client latency vs base | ≤ +5% | mean of runs −3.1% … +2.8% over the 8 points (Q1 /sparql +1.5% / +2.7%, /api −2.5% / +2.8%; Q4 +1.9% / −0.3%, −3.1% / −1.2%; univ 8 / 38) | **met** |
| Mixed, check_point off: reader qps | ≥ 2 × pre read-only 8-client | univ 8 /sparql 26,783 / 23,116 (≥ 152.7), /api 10,776 / 9,689 (≥ 153.0); univ 38 /sparql 20,298 / 20,029 (≥ 26.8), /api 10,121 / 9,396 (≥ 30.8) | **met** |
| Mixed, check_point on: reader qps | ≥ pre | univ 8 /sparql 47.5 / 41.1 (< 76.3), /api 35.4 / 35.7 (< 76.5); univ 38 /sparql 7.7 / 10.3 (< 13.4), /api 8.4 / 8.2 (< 15.4); base 24.8–27.0 and 4.5–8.9 | **not met** (≥ base; the full snapshot save under the exclusive guard; §5.7, Requests) |
| Monitoring while a long query runs (G4) | ≤ 100 ms | `/status` (both servers), REST `/monitor` and `/show`, `/api show`: ≤ 5 ms (one 47 ms single-db sample); `/api monitor` ≤ 6 ms with current statistics, 0.83–0.92 s for the first call after a write; `/api stat` 203 ms (its 200 ms CPU sample, idle the same) | **met** except `/api monitor` right after a data change (**not met**, Requests) |
| Monitoring while a write holds the database | ≤ 100 ms | the same endpoints ≤ 4.2 ms (also with a write queued behind a long query: ≤ 3.0 ms); `/api monitor` 58–217 ms in the save's fsync tail | **met** except `/api monitor` (**not met**, Requests) |
| Latch, monitoring (≤ 1 s), settings, re-entrancy tests; `dt_http_durability`, `upstream_transaction`; `dt_rw_server_stress`; `dt_rw_crash` | pass | see the verification line below | **met** |
| PFN: concurrent first resolves load the module once | pass | `pfn::registry::tests::concurrent_first_resolves_load_the_module_once` (`pfn-wasm`): in the R3 merge stage's feature matrix (`phase6-R3-merge.md`, pfn-wasm 2,036 passed); not re-run here | met (merge stage) |
| G4 on Stream-disk databases | — | measured in part 3 of this stage (R3-STREAM) | n/a here |

Verification at `77f7ed0` (this stage, release profile):
`cargo test --release --locked -p gstore --lib -- server::tests:: http_api::tests::`:
80 passed, 0 failed (among them the latch tests `two_sparql_reads_run_at_once` /
`two_reads_of_one_database_run_at_once`, `monitoring_answers_while_the_database_is_held`,
`monitoring_after_a_write_answers_while_the_database_is_held`,
`status_and_syntax_errors_do_not_wait_for_the_database`,
`query_settings_installed_while_reads_run_apply_next`,
`a_reentrant_database_guard_is_refused`); `cargo test --release --locked -p gstore
--test dt_rw_server_stress --test dt_rw_crash --test dt_http_durability --test
upstream_transaction`: 4 + 4 + 21 + 2 passed, 0 failed (2.3 s and 5.9 s for the two
new suites in release).
No code changed at this stage; every database it built was deleted.

### Implementer run

**Implementer run, noisy (other agents compiling and benchmarking on the same
4 vCPUs).** Release builds of base `0fb8842` (the prebuilt
`/tmp/gstore-base-0fb8842/bin`, and the same `examples/srv_mixed_load.rs`
built in a throwaway base worktree) and of this branch, measured in one
session, alternating base / branch twice, every run under
`flock /tmp/gstore-bench.lock`. Univ 8 = 1,029,275 triples, snapshot layout,
query cache off. The univ 38 numbers are left to the quiet measure stage.

#### gstore-bench concurrency phase (univ 8, snapshot, 10 s per point)

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

#### Mixed workload: `examples/srv_mixed_load.rs` (univ 8, snapshot, Q4)

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

#### Acceptance (plan §6 R3-SRV)

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

#### For the quiet measure stage (univ 38)

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

#### Test times (debug build, implementer machine)

`dt_rw_server_stress` 2.5 s (2 tests, 2 s of load each: about 900 /sparql
and 780 /api reader rounds, 20 two-operation writes, 19 transactions);
`dt_rw_crash` 9.3 s (4 cases × 10 seeds: 128 / 605 / 84 / 446 acknowledged
writes checked after `SIGKILL` for single-db snapshot / single-db disk /
api snapshot / api disk).

#### Verification at the final commit

- `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --
  -D warnings` (Rust 1.99.1): clean.
- `cargo test --workspace --no-fail-fast`: 2,190 passed, 0 failed, 2
  ignored.
- `--features pfn-wasm` (the PFN registry's single-flight code is gated on
  it): clippy clean; `cargo test -p gstore --features pfn-wasm`: 2,009
  passed, 0 failed, 1 ignored.
- `--features tls` (`serve_forever_tls` shares the changed request path):
  clippy clean.
- `--features rocksdb` (the servers now lock RocksDB directories; the
  RocksDB crash cases): `CARGO_TARGET_DIR=/tmp/p6r3-target-rocks cargo
  clippy -p gstore --all-targets --features rocksdb -- -D warnings` clean;
  `cargo test -p gstore --features rocksdb`: 2,013 passed, 0 failed, 2
  ignored (including `single_db_rocksdb_…` and `api_rocksdb_…` of
  `dt_rw_crash`).
- No new crate, no `Cargo.lock` change.

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
  single-client comparison (done; see below).
- No disk-layout (Stream mode) concurrency numbers: Stream mode lands with
  R3-STREAM; G4's "Stream-disk databases" figures belong to R4-SNAP /
  R4-CLOSURE.
- The re-entrancy registry refuses a second guard of the *same* database on
  one thread; holding guards of two different databases is not refused (no
  path does it; C8 forbids it).

From the quiet post-merge stage (`77f7ed0`; numbers under Measured numbers):

- Univ 8 and 38 measured quietly: every throughput, scaling and latency item
  is met at both scales (Q4 8 / 1 clients 4.9–5.4× on /sparql, 4.1–4.4× on
  /api; single-client latency −3.1% … +2.8% against the same-session base);
  so is the mixed workload with check_point off (≥ 23,116 / 9,689 reader
  qps on /sparql / /api at univ 8 against targets of 152.7 / 153.0; ≥
  20,029 / 9,396 at univ 38 against 26.8 / 30.8).
- **Mixed workload with check_point on: not met at either scale on either
  endpoint** (univ 8 41–48 qps /sparql, 35–36 /api vs ≥ 76.3 / 76.5; univ
  38 7.7–10.3 / 8.2–8.4 vs ≥ 13.4 / 15.4). The implementer's noisy /sparql
  "met" (102 / 104 qps in 10 s windows) does not hold on the quiet VM.
  Not a regression: the same-session base reads 25–27 and 4.5–8.9 qps, and
  the save time is unchanged (≈ 0.31–0.33 s univ 8, 1.2–1.3 s univ 38).
  Each acknowledged update rewrites the whole snapshot under the exclusive
  guard (request 2; callgrind: 93% of the exclusive section is
  `Database::save`'s serialization), the 100 ms writer runs back to back,
  and readers only get the gaps. Out of reach for R3-SRV without weakening
  "saved before acknowledged / before readers see it"; no Round 4 brief
  covers an incremental snapshot save (R4-SNAP makes the in-memory copy
  for `/begin` O(1), not the save). Recorded per §5.7 for R4-CLOSURE to
  report under G4; request 2's options (b) and (c) stand, and R4-SNAP /
  R4-CLOSURE should measure the mixed workload on a disk (Stream)
  database, whose commit is incremental (`examples/srv_mixed_load.rs`
  builds a snapshot database today), before G4 is judged.
- **`/api monitor` misses the ≤ 100 ms monitoring target in two cases**
  (every other monitoring endpoint answers in ≤ 6 ms — one 47 ms sample —
  while a long query runs, while a write holds the database and with a
  writer queued, `/api stat` apart, whose 200 ms CPU sample is its own;
  the base waited 1.0–3.7 s): (a) the first call after the data changed
  recomputes the subject count, class statistics and schema graph — 0.69–0.92
  s at univ 38, also while a long query runs (on a shared guard: no lock
  wait, but a queued writer then waits behind it); (b) during the fsync
  tail of a check_point save its `diskUsed` directory walk stalls 58–217 ms
  in `stat` (`disk=0` avoids it). Not a regression (the base computed the
  same statistics under its exclusive lock). **Request 6 (owner of
  `gapi/ops_db.rs` in Round 4, R4-SNAP; R4-CLOSURE reports G4):** answer
  `/api monitor` / `schema` from the last computed statistics while a
  refresh runs off the request path (or maintain the class counts
  incrementally), and keep `diskUsed` from a value refreshed after each
  save instead of walking the directory per request.
