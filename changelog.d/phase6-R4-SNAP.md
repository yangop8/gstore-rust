# R4-SNAP — O(1) snapshots and serving disk databases

Package R4-SNAP of `docs/PHASE6_PLAN.md` §6 (Round 4). Base: `d702463`
(Phase 6 Rounds 1–3 merged). Files: `src/store/mod.rs`, `src/http_api.rs`,
`src/gapi/ops_db.rs`, `src/concurrent.rs`, `src/bin/gbuild.rs`,
`tests/dt_http_durability.rs`, `tests/upstream_transaction.rs`,
`tests/dt_snapshot_disk.rs` (new), `tests/dt_cow_store.rs` (new),
`changelog.d/phase6-R4-SNAP.md`. `src/gapi/mod.rs` and `src/server.rs`
(also in the list) needed no change. No ownership exception.

## Changelog

### Phase 6 R4-SNAP: copy-on-write triple store, cheap transaction snapshots, disk databases from the servers

- **The in-memory `TripleStore` is copy-on-write.** Its indexes live in one
  `Arc`'d `StoreData` (the fields of before, in the same order, with the
  same derives): `clone()` is O(1) and shares them; the first mutation of
  a shared store copies them once; later mutations are in place. A
  mutation that changes nothing (a present triple inserted, an absent one
  removed, an empty or all-present bulk load) never copies. Serialization
  delegates to `StoreData`, so `store.bin` / `named.bin` are byte for byte
  what they were, and `Debug` prints the same text (both checked against a
  legacy mirror of the old definition). A sorted list that drops below a
  quarter of its allocation gives the rest back. `#[doc(hidden)]
  shares_storage_with` tells whether two stores share their indexes.
- **Spare copies for writers that publish after every change**
  (`TripleStore::keep_spare_copies`, `#[doc(hidden)]`, off by default).
  When such a store must copy shared indexes, it keeps the indexes it
  copied as a spare, with a journal of its later inserts and removes; the
  next time it must copy, a spare no clone holds any more is brought up to
  date by replaying the journal (O(changes)) instead of copying every list
  (O(triples)). A bulk load drops the spare; so does a journal longer than
  4096 + triples/8 changes. The price is a second copy of the indexes in
  memory, so only `ConcurrentDb` enables it.
- **Batched sorted-merge writes: `TripleStore::insert_many` /
  `remove_many`** (also `MutableStore::insert_many` / `remove_many`, whose
  default does one triple at a time). A batch (any order, repeats allowed)
  is sorted once per index order and merged into each touched list in one
  pass: only the tail after the list's first new entry moves, once — where
  one `insert` per triple moves that tail once per triple. Removal likewise
  compacts each touched list once. Element `i` of the result is what
  `insert` / `remove` of the `i`-th triple returns after the earlier ones,
  and the store ends up exactly as after them (triples, statistics,
  pruning, serialized bytes); a batch that changes nothing never copies
  shared indexes. It answers R4-RECLAIM's request: with lowest-first id
  reuse a reused low id lands at the front of long per-predicate
  (`p2so`) and per-class (`o2ps`) lists, and per-triple inserts then move
  most of each list per triple (4.7× slower inserts in its churn). A
  spare copy's journal now replays each run of inserts (or removes) as one
  batch. The database's write paths still insert one triple at a time:
  they belong to R4-RECLAIM's `db/mod.rs`, and the integrator wires them
  (Requests).
- **`ConcurrentDb` publishes in O(1)** for an in-memory database: a
  snapshot shares the dictionary (copy-on-write since Round 2), the default
  graph's store and every named graph's store; the authoritative database's
  store keeps a spare copy, so a one-triple update after a publish costs
  about two triple inserts, not a store copy (the copy is the fallback
  while a reader still holds the snapshot two versions back). The replaced
  snapshot is let go after the `current` lock is released. A Stream-mode
  on-disk or RocksDB database is still materialized per publish (a
  `ConcurrentDb` snapshot must stay readable after later commits; a
  `DiskReadView` expires at the next write). The module's cost model says
  so.
- **`/begin` copies no triple.** A transaction's `ReadSnapshot` holds a
  `SnapStore`: a copy-on-write clone of an in-memory store (also a
  Materialize-mode disk database's working set), or the
  `DiskTriples::read_view()` of a Stream-mode database; a RocksDB database
  is still materialized. The dictionary and the named graphs are shared
  (copy-on-write), and the committed triple count is recorded at `/begin`.
  The transaction's first write to an in-memory store copies it once.
- **Reads of an on-disk snapshot run under the store's writer gate**
  (`DiskReadView::with_base`, over `DiskSource`, in a `ReadScope`), with
  the gate's cancel flag in the evaluator; the request's own cancel flag
  (`/cancel`, a shutdown past its grace period) is forwarded to it by a
  small shared thread (`cancel_bridge`, polling every 5 ms while a
  forwarding is registered). The snapshot's CSR is keyed by the view's
  epoch. The commit's drain cancels the reads in flight and expires the
  view; a read that expired is re-run (`ApiInner::read_committed`) on the
  database as it is then — re-checking under the guard that it is still
  served and whether a transaction is open — so it answers the committed
  state; the last of three attempts reads holding the guard. A closed view
  (the database was unloaded) answers "not loaded". So `/commit` no longer
  waits for long snapshot reads (on any layout: snapshot reads already ran
  without the database's guard). `gstore::http_api::expired_snapshot_reads()`
  (`#[doc(hidden)]`) counts the re-runs.
- **REST `/build` and `/api build` take `layout=snapshot|disk|rocksdb`**
  (also `memory`, `kvstore`, `rocks`). The disk and RocksDB layouts are
  built in bulk (`bulk::build`, the server's `[system] build_*` options);
  RocksDB only by a server built with the `rocksdb` feature, a clear
  `400` / `1003` otherwise; an unknown layout is refused the same way. The
  default is `[storage] default_layout` (the server's configuration, else
  the process's open options), else the snapshot layout — Round 3 kept
  Materialize as the default disk query mode, so the plan's rule gives
  `snapshot`. REST `/build` probes the directory lock and releases the
  probe, retires the loaded copy, then bulk-builds (the build takes the
  lock itself), opens and serves the result; a failed build serves the
  previous database again. `/api build` bulk-builds into its staging
  directory and renames it into place, as before.
- **`gbuild --layout snapshot|disk|rocksdb`.** Without it (and without
  `--disk` / `--rocksdb`, which stay) the layout is `./conf/conf.ini`'s
  `[storage] default_layout`, else `snapshot`; plain `gbuild` and every
  existing flag work as before. A `--layout` that contradicts `--disk` /
  `--rocksdb` is a usage error.
- **`ApiServer::with_open_options`**: the databases a server loads (`/load`,
  `/api load`, restores, `monitor` of an unloaded database, the disk /
  RocksDB builds) open with the given options instead of the process
  defaults and the environment (for embedded servers and tests).
- **`/api monitor` and `schema` never compute statistics on the request
  path after a change** (R3-SRV request 6). The class statistics, subject
  count and schema graph of a loaded database are recomputed by a
  background refresh (one per database at a time, on a shared guard); the
  request waits for it at most 40 ms — a small database is done by then —
  else answers with the statistics computed for the earlier data and the
  published counts, and settles once the refresh ran. Only a database
  whose statistics were never computed computes them on the request path.
  `diskUsed` is the directory size measured when the database is loaded
  and at the end of every exclusive section (after its save), never walked
  on the request path (where it stalled behind a save's fsync). The old
  `DbCell::try_read` is gone (unused).
- **`show` and `monitor` report the layout** (`"layout":
  "snapshot"|"disk"|"rocksdb"`) and a loaded on-disk database's query mode
  (`"queryMode": "stream"|"materialize"`); Rust extensions, as `csr` and
  `txn`. (`/api stat` is in `gapi/ops_sys.rs`, not this package's.)
- **Tests.** New `tests/dt_cow_store.rs`: clone isolation both ways, no-op
  writes never copy, golden bincode and `Debug` against the legacy
  definition (random stores and a fixed byte string), a randomized
  differential (3,000 operations × 5 seeds, with and without spares,
  published / held / dropped clones checked byte for byte), a clone read on
  another thread while the original takes 20,000 writes, `ConcurrentDb`
  snapshots over reused spares with concurrent readers, and the latency
  driver (ignored); the batched operations against repeated single ones
  (random batches with repeats, present and absent triples, held and
  published clones, with and without spares), batches before, inside and
  after long lists and emptying a predicate, no-op batches never copying,
  and the `batch_of_low_ids_latency` micro-benchmark (ignored); a unit test
  of the merge and the one-pass removal over every split of a small
  universe. New `tests/dt_snapshot_disk.rs` (servers forced to
  Stream mode): eight readers see only the begin-time count while a
  transaction inserts pairs and converge after the commit, a rollback is
  never visible; `/commit` takes under a second while a long snapshot query
  (about 60 s uninterrupted) runs, which expires and re-runs; unloading
  during reads answers 200 or 404 only; `/api build layout=disk` answers
  like `layout=snapshot` (live and from a transaction's snapshot), unknown
  and unavailable layouts are refused. `dt_http_durability`'s 13
  `ApiServer` tests and `upstream_transaction`'s two scripts each run over
  the snapshot and the disk layout (`_disk` variants). New unit tests:
  `/begin` shares the store until the first tquery write;
  `/api monitor` right after a change answers at once and settles.
- **Changed assertions** (behaviour this brief changes): in
  `dt_http_durability`, the two check_point-off tests expect an on-disk
  database's acknowledged writes on disk before the save (a commit point
  per write; the snapshot layout keeps the old expectation); in
  `http_api`'s unit tests, two `/api monitor` checks after a change poll
  until the statistics settle instead of expecting them in the first
  answer. (`dt_snapshot_disk`'s reader test, new in this package, was
  itself fixed during the package: it expected the committed count from the
  moment `/commit` was sent, so a read that finished before the commit
  applied — correctly answering the begin-time count — failed it about one
  run in ten; the readers now accept the count committed when the read
  starts or the one being committed when it ends.)

## GAP_PLAN status line

R4-SNAP: copy-on-write TripleStore (O(1) clones, bytes and Debug unchanged), O(1) `/begin` on memory and Stream-disk databases (disk snapshot reads under the writer gate, expired reads re-run), `ConcurrentDb` O(1) publishes with spare copies, `layout=` for REST `/build`, `/api build` and `gbuild`, `/api monitor` statistics and `diskUsed` off the request path (≤ 47 ms in every measured case at univ 8 and 38, base up to 1.08 s), batched `TripleStore::insert_many` / `remove_many` for R4-RECLAIM's id reuse (70× faster for a 5,330-triple low-id batch at univ 8; wiring by the integrator) — done; check_point saves of the snapshot layout still run under the exclusive guard (follow-up below).

## Docs notes

- docs/STORAGE_BACKEND.md / DESIGN (concurrency): the copy-on-write
  `TripleStore` (clone O(1), first write copies once, no-op writes never
  copy); spare copies (`ConcurrentDb` only: a second copy of the indexes in
  memory, updates O(changes)); `ConcurrentDb`'s cost model (O(1) publish
  for memory databases; Stream / RocksDB still materialize per publish, and
  why).
- HTTP docs (transactions): `/begin` is O(1) on memory and Stream-disk
  databases (RocksDB still copies); the first write of a transaction on a
  memory database copies the store once; on a Stream database
  non-transactional reads during a transaction run on a read view, the
  commit cancels them and they re-run on the committed state (a `/commit`
  no longer waits for long reads); a Stream database's dictionary is
  shared with the snapshot (terms a transaction interns are visible to it,
  unused by its triples).
- HTTP / `/api` docs: `layout=snapshot|disk|rocksdb` on REST `/build` and
  `/api build` (default `[storage] default_layout`, else `snapshot`;
  RocksDB needs the feature); `gbuild --layout`; `show` / `monitor` report
  `layout` and `queryMode`; `monitor` / `schema` may report the statistics
  of the data before a change for as long as the background refresh runs
  (counts are always current); `diskUsed` as of the end of the last write.
- Config docs: `[storage] default_layout` now picks the layout of REST
  `/build`, `/api build` and `gbuild` (see Requests: the config docs and
  `gserver`'s warning still call it reserved).
- `ApiServer::with_open_options` (embedded servers).
- docs/STORAGE_BACKEND.md (in-memory store): `insert_many` / `remove_many`
  merge a batch into each list in one pass (the write paths that use them,
  once wired); why it matters with lowest-first id reuse.

## Measured numbers

All numbers: **implementer run, noisy** (4 vCPU VM shared with another
implementer; load average 3–4 during the runs), univ 8 (`lubm-like1-u8-s0`,
1,029,275 triples), every heavy command under `flock
/tmp/gstore-bench.lock`, base = `d702463` (`/tmp/gstore-base-d702463/bin`,
or a driver built at `d702463` in a throwaway worktree), branch = this
branch's release build, measured alternately in the same session. Quiet
numbers (and univ 38) are the measure stage's (commands below).

**`/begin` through `gserver`** (REST, 5 rounds each, two sessions per side;
`RSS` = VmRSS growth of `gserver` across the request):

| database | side | `/begin` | RSS growth at `/begin` | first tquery write | snapshot `COUNT(*)` read | rollback |
|---|---|---:|---:|---:|---:|---:|
| memory (snapshot layout) | base | 19.5–61.3 ms (45–61 ms while memory is fresh) | +45.4 MiB (fresh), 0–0.9 MiB (allocator reuse) | 0.4–8.3 ms | 98–136 ms | 20–30 ms |
| memory | branch | **0.46–0.89 ms** | **+4–100 KiB** | 17–57 ms (copies the store once) | 97–138 ms | 20–26 ms |
| disk, Stream | base | 198–324 ms | +66.2 MiB (fresh) | 0.5–0.9 ms | 97–162 ms | 11–19 ms |
| disk, Stream | branch | **0.41–0.66 ms** | **+0–24 KiB** | 0.5–0.9 ms | 123–173 ms (read through the view) | 0.5–0.8 ms |

Acceptance `/begin` ≤ 5 ms with ≤ 4 MiB (memory) and ≤ 50 ms with ≤ 16
MiB (Stream disk): **met** at univ 8 (univ 38 for the measure stage; both
are O(1) in the data by construction).

**`ConcurrentDb` one-triple update, terms existing** (`dt_cow_store`'s
`concurrent_update_latency`, 80 updates after 4 warm-up ones, medians of
two sessions per side):

| | median | mean | p90 |
|---|---:|---:|---:|
| base | 38.6–41.0 ms | 40.6–42.3 ms | 45.8–51.6 ms |
| branch | **0.076–0.079 ms** | 0.079–0.11 ms | 0.090–0.099 ms |
| branch, a reader holds the last two snapshots (no spare reusable: the copy fallback) | 13.4–14.0 ms | 13.5–14.3 ms | 14.1–15.8 ms |

Acceptance ≥ 1.7× lower latency: **met** (≈ 500×; the fallback 2.9×).

**Stream-disk database through the servers, Q4, 1 → 8 clients**
(`gstore-bench run --profile reference --scale 8 --backends disk --env
GSTORE_DISK_QUERY_MODE=stream --no-update --conc-queries q4 --conc-clients
1,8`, 5 s per point; bodies identical):

| | /sparql 1 → 8 clients | ratio | /api 1 → 8 clients | ratio |
|---|---|---:|---|---:|
| base | 2,310 → 9,816 qps | 4.25× | 1,642 → 6,000 qps | 3.65× |
| branch | 2,328 → 8,473 qps | **3.64×** | 1,472 → 5,894 qps | **4.00×** |

Acceptance ≥ 2.5×: **met** at univ 8 on both endpoints (unchanged code
path for non-transactional reads; the spread is noise). Univ 38: measure
stage.

**Memory layout (snapshot), gstore-bench reference profile**
(`--backends snapshot --no-conc`, four alternating base/branch pairs):

| | build s | build peak MiB | open warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms | chunk insert (5,330) ms | chunk delete ms | single insert median ms | checkpoint ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| base | 5.22–5.45 (5.28) | 112.8 | 197–246 (215) | 103.1–103.3 | 128.5–129.7 | 2.94–3.09 (2.99) | 40.8–67.2 (52.7) | 22.3–26.9 (23.9) | 0.06–0.10 | 280–316 |
| branch | 5.13–5.41 (5.22) | 112.8 | 216–248 (229) | 103.6–103.9 | 130.0–130.6 | 3.08–3.22 (3.13) | 43.5–48.4 (45.5) | 24.5–28.5 (25.7) | 0.059–0.060 | 273–330 |

- Build −1.2%, insert throughput (chunk insert) −14% time, single inserts
  as fast or faster: **met**.
- Q1–Q14 geomean **+4.7%** (every pair +3.7–5.8%): at the edge of ±5%.
  Two more pairs with glibc's heap trimming disabled: +2.5%. It comes from
  three sub-0.2 ms queries only: Q1 0.034 → 0.041 ms (+21%), Q3 0.073 →
  0.095 ms (+30%), Q12 +7.5%; every other query is within ±3%.
  - Callgrind of 65 warm Q3 runs in one process (in-process driver
    `memory_query_latency`, base built at `d702463`): `Database::read_as`
    is **63.68 M instructions on both sides**, with the same simulated
    cache misses — the read path does the same work.
  - With glibc's mmap threshold pinned (`MALLOC_MMAP_THRESHOLD_=33554432`,
    both sides, three pairs) the order flips: Q1 base 45–66 µs vs branch
    38 µs, Q3 base 160–166 µs vs branch 138 µs. So the small-query gap is
    the allocator's state after the open (its dynamic mmap threshold and
    heap layout), not the copy-on-write read path. The measure stage
    should judge Q1–Q14 on the quiet machine (and may add the pinned
    threshold as a control).
- Warm open +6.6% (noisy, overlapping ranges). Callgrind of the open: the
  bincode deserialization is 20–35% more instructions in this build for
  *every* snapshot file, `dict.bin` and `vstree.bin` included, which this
  package does not touch (`BufReader::read_exact` is no longer inlined
  into the element loop): a codegen difference of the whole crate, not of
  the store's format (`store.bin` bytes are identical). Request below.
- Chunk delete +7.5% (noise-level; a list that drops below a quarter of
  its allocation now gives the rest back).

**`/commit` while a long snapshot query runs** (`dt_snapshot_disk`,
debug-profile test build, small data): the query runs about 60 s
uninterrupted; `/commit` answered in well under 1 s (the test asserts < 1
s), the read expired and re-ran.

**check_point on, snapshot layout (R3-SRV request 2): not changed, numbers
as in R3.** Each acknowledged update still rewrites the whole snapshot under
the exclusive guard. Investigated whether the copy-on-write store lets the
save run from an O(1) clone without readers ever observing a write before
it is durable: it does only if (1) the save can run from a captured image
with no database guard held — `Database::save` (`db/persist.rs`, not this
package's) writes from a live `&Database`, and the VS-tree is not
copy-on-write — and (2) non-transactional readers are served the
pre-update state until the save completes (a "save pending" snapshot, as
a transaction's). Saving under a shared guard instead does not help: the
lock prefers writers, so the next queued writer blocks every new reader
for the rest of the save. With the redo log best-effort, there is no
cheaper durability point. Not implemented; follow-up under Requests, to
be reported under G4 at closure. The disk layout, whose commit point is
incremental, is now buildable from the servers (`layout=disk`), which is
option (c) of that request.

**`/api monitor` latency (R3-SRV request 6)** — `gserver --api`,
check_point on, snapshot layout, `POST /api {"operation":"monitor"}` timed
with curl (`time_total`); two alternating rounds per side at univ 8, one
at univ 38; the long query is a never-matching streamed cross product of
`ub:advisor` pairs (it runs for minutes):

| case | univ 8 base | univ 8 branch | univ 38 base | univ 38 branch |
|---|---:|---:|---:|---:|
| first call after the load (computes the statistics; both sides) | 169–173 ms | 196–235 ms | 948 ms | 877 ms |
| warm (median of 5) | 0.91–0.96 ms | 0.94–0.96 ms | 1.73 ms | 0.98 ms |
| right after an acknowledged update | **172–220 ms** | **41–47 ms** | **0.99–1.08 s** | **41–43 ms** |
| the call after that, at once | 0.8–1.1 ms | 41–43 ms (the refresh still runs) | 0.8–1.1 ms | 41–42 ms |
| during an update's check_point save (0.15 s in; univ 38 also 0.6–1.3 s in) | 0.8–1.2 ms | 0.8–42 ms | 0.8–0.9 ms, **177 ms** at 1.3 s | 0.8–43 ms |
| during a long query (first call after a change) | **184–230 ms** | 41 ms | **1.02 s** | 43 ms |
| during a long query (later calls) | 0.9–1.5 ms | 0.8–20 ms | 0.9–1.3 ms | 1.0–42 ms |

Every branch answer is ≤ 47 ms (the request waits at most 40 ms for the
background refresh, then answers with the statistics of the data before
the change and the current counts); the base computes the statistics on
the request path after every change (0.2 s at univ 8, 1 s at univ 38) and
once stalled 177 ms in a save's fsync tail. G4's monitoring item (≤ 100
ms while a long query runs, right after a change, during a save):
**met** in this noisy run at both scales; the quiet stage re-checks it at
univ 38 (command 5).

**Batched inserts / removes of low ids** (`dt_cow_store`'s
`batch_of_low_ids_latency`, release build, same binary: one `insert` /
`remove` per triple — what the database does today — vs `insert_many` /
`remove_many`; median of 5). The batch is the triples of the lowest
subject ids — what lowest-first reuse produces — removed first, then
inserted back:

| store | batch | insert one by one | `insert_many` | remove one by one | `remove_many` |
|---|---:|---:|---:|---:|---:|
| LUBM univ 8 (1,029,275 triples) | 5,330 (a chunk insert) | 334 ms | **4.7 ms** (71×) | 336 ms | **4.0 ms** (85×) |
| LUBM univ 8 | 130,000 (≈ one university) | 6.61 s | **34 ms** (194×) | 7.06 s | **60 ms** (117×) |
| synthetic, 1,000,000 triples, 20 classes | 5,330 | 491 ms | **7.4 ms** | 484 ms | **5.4 ms** |
| synthetic | 50,000 | 4.22 s | **28 ms** | 4.22 s | **33 ms** |

The one-by-one figure for a university (6.6 s) is R4-RECLAIM's measured
6.7 s churn insert; batching brings it below its no-reuse 0.87 s once the
write paths use it (Requests). Batches of high ids (appended at the end of
the lists) were already cheap one by one.

**`ConcurrentDb` update latency, re-run in this session** after the
batched-operation change (the spare's journal replay): base 67.8 ms median
(p90 73.6 ms; the machine was busier than in the first session), branch
0.089–0.096 ms (p90 0.11–0.16 ms), the copy fallback 19.3–20.9 ms.
Acceptance (≥ 1.7× lower): **met**.

### Measure-stage commands

Base binaries: `/tmp/gstore-base-d702463/bin`; branch: `cargo build
--release -p gstore -p gstore-bench --bin gbuild --bin gserver --bin
gstore-bench`; data `GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache`; every
command under `flock /tmp/gstore-bench.lock`, base and branch alternating.

1. `ConcurrentDb` update latency (univ 8 and 38): `cargo test --release -p
   gstore --test dt_cow_store --no-run`, then
   `GSTORE_COW_BENCH_NT=/tmp/gstore-bench-cache/lubm-like1-u38-s0.nt
   <dt_cow_store binary> --ignored --nocapture concurrent_update_latency`.
   The base side: the test function `concurrent_update_latency` without
   its `hold` loop (it uses only `ConcurrentDb::{new, update, snapshot}`
   and `Database::build_from_files`, all at `d702463`) copied into a
   scratch test file of a `d702463` worktree.
2. `/begin` (univ 38, memory and Stream disk): build with `gbuild` (and
   `gbuild --disk`), serve with `gserver --api <home> --db-home <home>`
   (`GSTORE_DISK_QUERY_MODE=memory|stream`), `/load`, then per round
   `POST /begin?db_name=b` (time with `curl -w %{time_total}`; VmRSS of
   the `gserver` pid from `/proc/<pid>/status` before and after), one
   `/tquery` INSERT DATA, one non-transactional `/sparql` read, and
   `/rollback`. Take the first rounds of a fresh process for the RSS
   growth (later rounds reuse freed memory on the base).
3. Memory Q1–Q14, build, insert: `gstore-bench run --profile reference
   --scale 8 --backends snapshot --no-conc --bin-dir <side> --out
   <side>-mem-<i>.json` (≥ 3 alternating pairs; also `--scale 38`).
   Optionally the in-process driver: `GSTORE_COW_BENCH_DB=<snapshot db
   dir> <dt_cow_store binary> --ignored --nocapture
   memory_query_latency` (base: the same function in a scratch test file
   at `d702463`).
4. Stream-disk concurrency: `gstore-bench run --profile reference --scale
   38 --backends disk --env GSTORE_DISK_QUERY_MODE=stream --no-update
   --conc-queries q4 --conc-clients 1,8 --bin-dir <side>`.
5. `/api monitor` latency (univ 38, memory): `POST /api
   {"operation":"monitor",…}` once warm, right after each of three
   `/api` `query` operations with an INSERT DATA (check_point on), during
   a long query (a streamed cross product that never matches, e.g. `ASK {
   ?a ub:advisor ?x . ?b ub:advisor ?y FILTER(STRLEN(STR(?x)) +
   STRLEN(STR(?y)) < 0) }`), and 150 ms into a
   check_point update (whose save runs about 1.2 s at univ 38; also
   0.6, 0.9, 1.1 and 1.3 s in, to reach the save's fsync tail). The
   long query never ends: stop the server after it (an update would wait
   for it on the snapshot layout).
6. Batched writes: `cargo test --release -p gstore --test dt_cow_store
   --no-run`, then `GSTORE_COW_BENCH_NT=/tmp/gstore-bench-cache/lubm-like1-u8-s0.nt
   GSTORE_COW_BENCH_BATCH=5330 <dt_cow_store binary> --ignored --nocapture
   batch_of_low_ids_latency` (also `GSTORE_COW_BENCH_BATCH=130000`, and
   univ 38). Base and branch are the two columns of the same run. Once the
   integrator wired the write paths, R4-RECLAIM's
   `examples/reclaim_churn.rs --layout snapshot --policy immediate` is the
   end-to-end check (its insert time should drop from ~6.7 s per
   university to below the Off policy's ~0.9 s).

## Requests/Not yet

Requests (outside this package's files):

1. **`src/bin/gserver.rs` (no Round-4 owner) and `src/config.rs` /
   `conf/conf.ini.example` (R4-RECLAIM):** `[storage] default_layout` now
   has an effect — it picks the layout of REST `/build`, `/api build` and
   `gbuild` (snapshot when unset). `gserver` still logs
   `GStoreConfig::default_layout_warning()` ("has no effect yet") at
   startup, and the `default_layout` field docs and `conf.ini.example`
   still call it reserved: drop the warning (the function can go) and
   update both texts. (`ApiServer`'s `refreshconf` no longer logs it.)
2. **check_point saves of the snapshot layout (G4 mixed workload; owner of
   `db/persist.rs`):** a save from a captured copy-on-write image
   (`Database::save_image` or similar: dictionary, store and named graphs
   are O(1) clones now; the VS-tree would need a copy-on-write form or
   to be saved from the store), so the HTTP layer can release the
   exclusive guard after capturing it while non-transactional readers are
   served the pre-update snapshot until the save is durable (the
   transaction snapshot machinery in `http_api.rs` is ready for that).
   Until then the mixed workload with check_point on stays as measured in
   R3; the measure stage should also run it on a disk-layout database
   (`examples/srv_mixed_load.rs` builds a snapshot database today — an
   option for its owner or R4-CLOSURE's `bench/**`).
3. **`src/db/fsutil.rs` (`read_bincode`):** deserialize from the file read
   into memory (`bincode::deserialize` of a `Vec<u8>`/mmap-free slice)
   instead of through a `BufReader`: in this build `BufReader::read_exact`
   is not inlined into bincode's element loops, which costs 20–35% more
   instructions per snapshot file at open (every file, not only
   `store.bin`); a slice reader is faster and does not depend on the
   inliner's choices.
4. **`gapi/ops_sys.rs` (`/api stat`):** report a loaded on-disk database's
   query mode and dictionary cache counters (`Dictionary::cache_stats`) if
   wanted; `show` and `monitor` report `layout` / `queryMode` now.
5. **Integrator, after R4-RECLAIM's merge — batched writes
   (`src/db/mod.rs`, `src/store/backend.rs`; R4-RECLAIM request 6).**
   (a) `src/store/backend.rs`, `impl MutableStore for Backend`: dispatch
   the two new methods, so a memory database reaches the batched merge
   (UFCS, because `DiskTriples` has inherent `insert_many` / `remove_many`
   of another signature; Disk and RocksDB keep the trait's one-by-one
   default):
   `fn insert_many(&mut self, t: &[IdTriple]) -> Vec<bool> {
   backend_dispatch!(self, s, MutableStore::insert_many(s, t)) }` and the
   same for `remove_many`. (b) `src/db/mod.rs`: in each batch write path,
   intern the batch's default-graph triples first (terms before triples, as
   now), call `self.store.insert_many(&ids)` (or `remove_many`) once, then
   run, for each triple whose result is `true` and in input order, exactly
   the bookkeeping `try_insert_triple_id` / `remove_default_id` run after
   `changed` today (VS-tree upsert or shrink debt, `touched()`, undo log,
   `record_redo`, `persist_mutation`, the reclaim candidates). The VS-tree
   upserts after the whole batch stay sound: a new entity is signed from a
   store that already holds all its batch triples, and the later unions add
   bits it already has. Call sites, by symbol: `Database::insert_sources`
   (its batch closure; `/api batchInsert`, `gadd`), `try_insert_data`
   (`INSERT DATA`), `delete_data` (`DELETE DATA`), `remove_sources` (`gsub`,
   `/api batchRemove`), the `UpdateOp::Modify` arm of `run_update` (the
   `dels` / `ins` of `modify_changes`), LOAD's batch closure (the function
   with `"LOAD"` in its `import_sources` call) and `undo_inserts`;
   `rollback`'s undo loop can batch each run of consecutive `UndoOp::Add` /
   `UndoOp::Del` of the default graph (order matters across runs only).
   Named-graph quads keep their own path. Keep the per-triple results:
   `applied` counters and `Ok(n)` counts are the number of `true`s.
6. **Integrator, after R4-RECLAIM's merge — unpinned dictionary clones
   (R4-RECLAIM request 2).** Switch exactly one call site:
   `src/http_api.rs`, `ReadSnapshot::capture`, the `dict: db.dict().clone()`
   field initializer — to `if matches!(store, SnapStore::Disk(_)) {
   db.dict().clone_unpinned() } else { db.dict().clone() }` (the comment
   there says why: an on-disk snapshot reads its dictionary only inside
   `DiskReadView::with_base`, in `ReadSnapshot::read_as`, and a writer
   drains those reads and expires the view before any free). **Do not**
   switch `src/concurrent.rs`, `Snapshot::from_db` (`dict:
   db.dict().clone()`): a `ConcurrentDb` snapshot of a Stream database
   materializes its store and resolves those ids after later writes,
   outside any read view, so an unpinned clone could resolve a freed and
   reused id to the wrong term. Consequence (unchanged): a `ConcurrentDb`
   over a Stream database defers its frees while a snapshot lives, and its
   construction-time base snapshot lives as long as it does. A versioned
   read view (D2) or an in-memory dictionary copy per publish (O(terms))
   would lift that; the servers do not use `ConcurrentDb`.
7. **R4-CLOSURE (`bench/**`):** `bench/src/runner/conc.rs` is unchanged
   and still compiles (`Server::bind`, `shutdown_handle`, `serve_forever`,
   `ApiServer::bind`, `with_query_settings`, `QuerySettings` untouched).
   A `/begin` latency point and a mixed-workload point on a disk database
   would make the G4 rows reproducible from the harness.

Not yet / notes:

- `ConcurrentDb` snapshots of a Stream-mode (or RocksDB) database still
  materialize the store per publish (R3-STREAM request 4's second half):
  a `ConcurrentDb` snapshot must stay readable after later commits
  (`Txn` reads at its begin version, `VersionView`'s base), which a
  `DiskReadView` cannot — it expires at the next write. Needs versioned
  pager reads (D2).
- An open transaction on an in-memory database that writes holds the
  begin-time copy of the store until it ends (as before, but the copy is
  made by the first write, not by `/begin`).
- Spare copies double a `ConcurrentDb` store's memory at steady state
  (database copy + spare + the construction-time base snapshot: three
  copies, as when every publish cloned the store).
- A Stream database's snapshot shares the database's `DiskDict`: terms a
  transaction interns are visible through it (no snapshot triple uses
  them). Id reuse (R4-RECLAIM) frees under the writer gate, which expires
  the view first.
- `/api monitor` / `schema` of a database whose statistics were never
  computed for this instance still compute them on the request path (a
  shared guard; nothing earlier to report). A statistics refresh holds a
  shared guard for one pass over the data (0.7–0.9 s at univ 38): a writer
  queued meanwhile waits for it, as it would for any read.
- Test temp directories: the new and changed test files remove theirs
  (also on failure); the other files R3-SRV listed are not this package's.
