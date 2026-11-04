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

### Quiet post-merge stage (phase6 `8264302`; part 2 of 3: R4-SNAP, gate G4)

Reference VM (4 vCPU Intel Xeon @ 2.10GHz, 16 GB), nothing else running, one
session (2026-11-04, 03:31–05:30 UTC). CPU busy and load average logged
every 10 s from `/proc/stat`: mean 33% busy (0–99%), 1-minute load average
0.2–7.4 — the measured processes themselves (the 8-client concurrency points
saturate the 4 vCPUs; the harness records the 1-minute load average left by
its previous point). Toolchain 1.99.1, `CARGO_PROFILE_DEV_DEBUG=0
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`; every heavy command under
`flock /tmp/gstore-bench.lock`, base and head alternating. **Head** =
release builds of `8264302` (`cargo build --release -p gstore -p
gstore-bench --bins`, `--examples`, `cargo test --release -p gstore --test
dt_cow_store --no-run`). **Base** = the tree before this round, `d702463`:
the prebuilt release binaries `/tmp/gstore-base-d702463/bin` (its own
`gstore-bench`: the harness runs the servers, the opens and the queries in
its own children, so the base harness measures base code), plus scratch
drivers built at `d702463` in a throwaway worktree with its own target
directory: the latency drivers of `dt_cow_store` in a scratch test file, and
R3-SRV's `examples/srv_mixed_load.rs` (removed from the head by the Round-4
merge) with a scratch `--prebuilt DIR` option. Data from
`/tmp/gstore-bench-cache` (univ 8 `ed44da2e7b3bbb98`, 1,029,275 triples;
univ 38 `fcd6493725f3c66a`, 5,181,103 triples). "pre" =
`bench/results/pre-phase6-lubm{8,38}.json`. This stage changed no code (no
regression of this round needed a fix; see the acceptance table); the two
scratch drivers and every database it built were deleted.

#### `ConcurrentDb` one-triple update, terms existing (`dt_cow_store`'s `concurrent_update_latency`)

```sh
GSTORE_COW_BENCH_NT=/tmp/gstore-bench-cache/lubm-like1-u{8,38}-s0.nt flock /tmp/gstore-bench.lock \
  <test binary> --ignored --nocapture --exact concurrent_update_latency
```

Base: the same function without its `hold` loop, in a scratch test file of a
`d702463` worktree (release, own target directory); head:
`target/release/deps/dt_cow_store-*`. 80 updates (an `INSERT DATA` and its
`DELETE DATA`, 40 rounds) after 4 warm-up ones; order base, head, base, head
per scale; the two runs per side:

| univ | side | median ms | mean ms | p90 ms |
|---|---|---:|---:|---:|
| 8 | base | 48.29 / 48.85 | 51.59 / 52.50 | 63.68 / 67.37 |
| 8 | head | **0.084 / 0.109** | 0.091 / 0.122 | 0.119 / 0.169 |
| 8 | head, a reader holds the last two snapshots (the copy fallback) | 14.65 / 14.46 | 15.44 / 15.28 | 17.73 / 17.83 |
| 38 | base | 209.4 / 221.2 | 216.3 / 228.3 | 249.5 / 262.9 |
| 38 | head | **0.510 / 0.541** | 0.519 / 0.545 | 0.570 / 0.599 |
| 38 | head, copy fallback | 70.8 / 68.2 | 73.9 / 71.5 | 87.0 / 85.1 |

Lower latency than the base: **443–581× at univ 8, 387–434× at univ 38**
(the copy fallback 3.3–3.4× and 3.0–3.2×); acceptance ≥ 1.7×: **met**. The
head update at univ 38 is 0.5 ms because the triple (`University0 ub:name
…`) has the lowest subject id, so each sorted insert moves the tail of the
`ub:name` lists (O(list), not O(store)).

#### Batched `TripleStore` writes (`dt_cow_store`'s `batch_of_low_ids_latency`)

```sh
GSTORE_COW_BENCH_NT=/tmp/gstore-bench-cache/lubm-like1-u{8,38}-s0.nt GSTORE_COW_BENCH_BATCH={5330,130000} \
  flock /tmp/gstore-bench.lock target/release/deps/dt_cow_store-* --ignored --nocapture --exact batch_of_low_ids_latency
```

One binary; median of 5 of each column (one `insert` / `remove` per triple,
the per-triple path, against `insert_many` / `remove_many`); the batch is
the triples of the lowest subject ids:

| store | batch | insert one by one | `insert_many` | remove one by one | `remove_many` |
|---|---:|---:|---:|---:|---:|
| univ 8 (1,029,275 triples) | 5,330 | 268.7 ms | **2.84 ms** (95×) | 268.4 ms | **3.23 ms** (83×) |
| univ 8 | 130,000 | 5,551 ms | **22.3 ms** (249×) | 5,970 ms | **45.3 ms** (132×) |
| univ 38 (5,181,103 triples) | 5,330 | 1,634 ms | **21.9 ms** (75×) | 1,518 ms | **7.29 ms** (208×) |
| univ 38 | 130,000 | 35,611 ms | **138 ms** (258×) | 35,545 ms | **58.7 ms** (605×) |

End to end the integrator wired the batches into the database's write paths
(`phase6-R4-merge.md`); part 1 of this stage measured the effect in
R4-RECLAIM's churn on the memory layout: insert time with reclamation on /
off 0.99 and 1.04 (4.7× with the per-triple path, in R4-RECLAIM's
implementer run). Met.

#### Memory layout (snapshot): `gstore-bench run --profile reference --backends snapshot --no-conc`

```sh
flock /tmp/gstore-bench.lock /tmp/gstore-base-d702463/bin/gstore-bench run --profile reference --scale {8,38} \
  --backends snapshot --no-conc --bin-dir /tmp/gstore-base-d702463/bin --work-dir /tmp/r4snap-work --out base-u{s}-{i}.json
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale {8,38} --extra none \
  --backends snapshot --no-conc --bin-dir target/release --work-dir /tmp/r4snap-work --out head-u{s}-{i}.json
```

Each side runs its own harness (the children do the opens and queries).
Three pairs per scale, order base, head, head, base, base, head; means of
the runs [range]; query = the minimum over the 5 query processes of the warm
median, as the harness records it:

| univ | | build s | open warm ms | RSS after open MiB | Q1–Q14 geomean ms | chunk insert (5,330) ms | chunk delete ms | single insert median ms | checkpoint ms |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 8 | base | 4.92 [4.74–5.06] | 174 [173–175] | 102.0 | 3.13 [3.10–3.17] | 41.6 [41.0–42.5] | 24.4 | 0.050 | 251 [244–257] |
| 8 | head | 5.14 [4.91–5.36] | 182 [175–188] | 102.4 | **2.17** [2.13–2.19] | **34.8** [34.0–36.1] | 23.3 | 0.051 | 267 [249–280] |
| 8 | head / base | **1.043** | 1.045 | 1.004 | **0.693** (pairs 0.682, 0.690, 0.707) | **0.837** | 0.953 | 1.03 | 1.065 |
| 38 | base | 27.53 [27.06–27.90] | 1,355 [1,113–1,492] | 489.1 | 17.49 [16.98–18.03] | 54.6 [53.5–55.7] | 35.7 | 0.016 | 2,878 [2,769–2,993] |
| 38 | head | 27.10 [26.47–27.60] | 1,395 [1,126–1,703] | 489.6 | **9.96** [9.83–10.12] | **44.3** [36.6–50.7] | 32.8 | 0.015 | 3,307 [2,973–3,654] |
| 38 | head / base | **0.984** | 1.030 | 1.001 | **0.569** (pairs 0.545, 0.579, 0.585) | **0.812** | 0.917 | 0.99 | 1.149 |

Per query, head / base of the means — univ 8: q1 0.43, q2 0.96, q3 0.21, q4
0.89, q5 0.73, q6 0.66, q7 0.81, q8 0.84, q9 0.84, q10 0.68, **q11 1.021**,
q12 0.997, q13 0.61, q14 0.66; univ 38: q1 0.14, q2 0.94, q3 0.03, q4 0.57,
q5 0.84, q6 0.69, q7 0.79, q8 0.93, q9 0.87, q10 0.85, q11 0.98, q12 0.95,
q13 0.67, q14 0.72. Rows and row hashes are identical in all 12 runs; the
update round trip is `ok` in every run.

- **Q1–Q14 within ±5%: met** — no query is slower than +2.1% (q11 at univ 8,
  0.223 → 0.228 ms); the geomean is 0.69× / 0.57× (R4-VS's VS-tree policy
  and the rest of the round). The implementer's +4.7% (three sub-0.2 ms
  queries) and part 1's q11 / q12 +20% / +15% at univ 38 do not reproduce on
  the quiet VM (q11 1.303 → 1.278 ms, q12 0.477 → 0.455 ms).
- **Build within ±5%: met** — +4.3% at univ 8 (pairs +3.6%, +7.9%, +1.4%;
  ranges overlap), −1.6% at univ 38 (part 1: +2.2% / −0.3%).
- **Insert throughput within ±5%: met** — chunk insert 0.84× / 0.81× the
  base's time, single inserts 1.03× / 0.99×.
- Not gated here, seen on the way: the update phase's snapshot **checkpoint
  is +6.5% / +15%** (part 1: +12% at univ 38) — see the save profile below;
  single deletes 0.0036 → 0.0046 ms / 0.0043 → 0.0058 ms (R4-RECLAIM's
  candidate check, as part 1 found).

#### The snapshot save (the update phase's checkpoint +6.5% / +15%, and check_point on)

The update phase's checkpoint (a full snapshot save at the database's home)
was +6.5% at univ 8 and +15% at univ 38 above (base 2,769–2,993 ms, head
2,973–3,654 ms; part 1: +12%), so the save path was profiled, outside the
harness, on databases opened fresh (a scratch test driver, the same function
at `d702463` and at the head: `Database::open`, then 3 timed `save(dir)`
into a new directory, wall and process CPU time):

| univ | side | save, 9 per side (3 processes alternating) | median | CPU per save |
|---|---|---|---:|---:|
| 8 | base | 190–251 ms | 222 ms | 82–115 ms |
| 8 | head | 180–228 ms | 212 ms | 82–128 ms |
| 38 | base | 779–958 ms | 838 ms | 401–522 ms (mean 438) |
| 38 | head | 806–1,176 ms | 863 ms | 415–538 ms (mean 453) |

callgrind of one save at univ 8 (`--toggle-collect='*save_locked*'`):
**181,069,336 instructions on the base, 181,070,044 on the head**
(+0.0004%). The save does the same work; the snapshot files `gbuild` writes
are byte for byte the same (`cmp` of `dict.bin`, `store.bin`, `named.bin`,
`vstree.bin`, `meta.bin` from the base's and the head's `gbuild` at univ 8;
the database sizes are equal in all 12 memory-layout runs above), and
`dt_cow_store`'s golden and legacy-mirror tests pass (9 passed, 3 timing
drivers ignored). The checkpoint difference in the harness is the time of
writing and fsyncing 85 / 431 MB (its spread on one side is 5–23%), not the
copy-on-write store: not a regression of this round.

**check_point on, snapshot layout (R3-SRV request 2): unchanged** — each
acknowledged update still saves the whole snapshot under the exclusive guard
(1.2–1.4 s, median, at univ 38 through `gserver`), as on the base; the mixed
workload with `check_point` on stays below pre on this layout (table above),
and the head equals the base there. Not implemented in this round (the
implementer's investigation stands: `Database::save` writes from a live
`&Database` and the VS-tree is not copy-on-write, both in `db/persist.rs`);
the follow-up is R4-SNAP request 2. A disk-layout database, whose commit
point is incremental, meets every mixed target with `check_point` on
(20,644–21,801 `/sparql`, 9,407–10,178 `/api` reader qps, writes
acknowledged in 4–8 ms) and is buildable from the servers (`layout=disk`).

#### `/begin` through `gserver` at univ 38

Real `gserver --api <home> --db-home <home> --thread-num 8` processes (base
`/tmp/gstore-base-d702463/bin`, head `target/release`), one fresh process
per side and layout, two sessions per side in the order base, head, head,
base. Memory = a snapshot-layout database (`GSTORE_DISK_QUERY_MODE=memory`;
one database built by the base's `gbuild`, served to both sides: the
snapshot files are byte-identical); Stream = `GSTORE_DISK_QUERY_MODE=stream`
on each side's own `gbuild --disk` database (base V1, head V2). `POST
/load`, one warm-up `COUNT(*)` read, then six rounds of `POST /begin` (`curl
-w %{time_total}`; VmRSS of the gserver pid from `/proc/<pid>/status` right
before and after), one `/tquery` `INSERT DATA`, one non-transactional
`/sparql` `COUNT(*)`, `/rollback`:

| database | side | `/begin` (12 rounds) | RSS growth at `/begin`, first round of a fresh process | all rounds | first tquery write | snapshot `COUNT(*)` read | rollback |
|---|---|---:|---:|---:|---:|---:|---:|
| memory (snapshot layout) | base | 91–927 ms (first two rounds 207–927 ms) | +222.4 MiB (227,656–227,760 KiB) | 0–222.4 MiB | 0.7–137 ms | 0.52–2.23 s | 117–155 ms |
| memory | head | **0.66–1.08 ms** (median 0.71) | **+20 KiB** | 0–20 KiB | 98–977 ms (copies the store once; +243 MiB in the first rounds) | 0.51–1.48 s | 127–198 ms |
| disk, Stream | base | 1,118–1,355 ms (median 1,224) | +200.7 MiB (205,500–205,540 KiB) | 0–200.7 MiB | 0.7–1.1 ms | 0.52–0.81 s | 48–77 ms |
| disk, Stream | head | **0.52–0.94 ms** (median 0.72) | **+20 KiB** | 0–20 KiB | 0.7–1.1 ms | 0.62–0.83 s (through the view) | 0.8–61 ms |

Acceptance (`/begin` ≤ 5 ms with RSS growth ≤ 4 MiB on memory, ≤ 50 ms with
≤ 16 MiB on a Stream disk database; G4 the same): **met at univ 38** on both
(the base copied the store: 0.1–0.9 s and 1.1–1.4 s, 200–222 MiB). The
harness's in-process `/api begin` point (20 samples per layout, head only:
the base harness has none) agrees: median 0.28–0.47 ms, max 0.53–2.33 ms,
RSS growth ≤ 0.11 MiB, on snapshot, Stream and Materialize at univ 8 and 38.

#### Concurrency and mixed workload through the bundled servers (G4)

```sh
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale {8,38} --extra none \
  --backends snapshot | --backends disk --env GSTORE_DISK_QUERY_MODE=stream --label stream \
                      | --backends disk --env GSTORE_DISK_QUERY_MODE=memory --label materialize \
  --reps 1 --query-procs 1 --open-reps 1 --no-update --conc-clients 1,8 --conc-secs 5 \
  --conc-queries q1,q4 --conc-endpoints sparql,api --mixed-secs 10 --bin-dir target/release
# base: /tmp/gstore-base-d702463/bin/gstore-bench with the same options but the
# --extra / --label / --mixed-* ones (its harness has no mixed phase);
# base mixed workload: R3-SRV's examples/srv_mixed_load.rs built at d702463
# (--secs 10 --readers 8 --write-ms 100 --query q4), on a snapshot it builds and,
# with a scratch --prebuilt option, on a `gbuild --disk` database served in Stream mode.
```

Two pairs per scale (order base, head, head, base; each base run followed by
its two mixed-load runs). The servers run in the harness's process (`Server`
for `/sparql`, `ApiServer` for `/api`), closed-loop keep-alive clients,
query cache off, 5 s per point; every point answered with one distinct body,
the same on both sides and in every layout, with no HTTP error. The first
head run of each scale is committed as
`bench/results/phase6-r4-conc-lubm{8,38}.json`, with
`phase6-r4-conc-lubm{8,38}-compare.md` (the report against the first base
run: **gates PASS, 281 checked, 0 failed**, at both scales; and the
concurrency table against pre).

**Q4, 1 → 8 clients** (qps, means of the two runs; the ratio of each run):

| univ | layout | endpoint | base | base 8 / 1 | head | **head 8 / 1** | head 1-client mean ms (base) |
|---|---|---|---|---|---|---|---|
| 8 | snapshot | /sparql | 5,774 → 32,907 | 5.54×, 5.86× | 6,283 → 29,664 | **4.39×, 5.08×** | 0.157 (0.170) |
| 8 | snapshot | /api | 2,667 → 11,147 | 4.23×, 4.13× | 2,620 → 11,273 | **4.06×, 4.58×** | 0.343 (0.336) |
| 8 | disk, Stream | /sparql | 2,040 → 6,322 | 3.22×, 3.00× | 4,676 → 22,002 | **4.57×, 4.85×** | 0.211 (0.488) |
| 8 | disk, Stream | /api | 1,426 → 4,339 | 3.31×, 2.80× | 2,399 → 10,574 | **4.23×, 4.59×** | 0.376 (0.657) |
| 8 | disk, Materialize | /sparql | 2,196 → 6,220 | 2.80×, 2.87× | 5,626 → 32,383 | **5.49×, 6.02×** | 0.175 (0.452) |
| 8 | disk, Materialize | /api | 1,469 → 4,443 | 2.92×, 3.13× | 2,506 → 11,048 | **4.12×, 4.71×** | 0.357 (0.635) |
| 38 | snapshot | /sparql | 3,788 → 23,345 | 6.16×, 6.17× | 6,320 → 33,223 | **5.00×, 5.53×** | 0.156 (0.261) |
| 38 | snapshot | /api | 2,007 → 8,448 | 4.13×, 4.30× | 2,345 → 9,896 | **3.90×, 4.60×** | 0.386 (0.453) |
| 38 | disk, Stream | /sparql | 363 → 624 | 1.70×, 1.74× | 4,793 → 20,269 | **4.35×, 4.11×** | 0.205 (2.757) |
| 38 | disk, Stream | /api | 328 → 598 | 1.86×, 1.79× | 2,301 → 9,609 | **3.92×, 4.50×** | 0.396 (3.004) |
| 38 | disk, Materialize | /sparql | 360 → 624 | 1.72×, 1.75× | 5,859 → 33,445 | **5.75×, 5.66×** | 0.168 (2.775) |
| 38 | disk, Materialize | /api | 341 → 613 | 1.87×, 1.72× | 2,817 → 10,873 | **3.81×, 3.92×** | 0.316 (2.878) |

Acceptance "a disk database through gserver: Q4 qps at 8 clients ≥ 2.5 × 1
client" (R4-SNAP) and G4's "≥ 2.5× via /sparql and /api, on memory and
Stream-disk databases": **met** on every layout, endpoint and run at both
scales (lowest 3.81×, Materialize `/api` at univ 38). The base's disk
layouts scaled 1.7–3.3× (their single-client Q4 took 0.45–3.0 ms; the head's
0.17–0.40 ms — R4-VS's VS-tree policy).

**Mixed workload**: 8 readers (Q4) and one writer (a single-triple `INSERT
DATA` every 100 ms), 10 s per point; reader qps per run (head: the harness's
mixed phase; base: `srv_mixed_load`). Targets as in R3: with `check_point`
off ≥ 2 × pre's read-only 8-client Q4 qps, with it on ≥ pre's (pre snapshot:
univ 8 76.3 `/sparql`, 76.5 `/api`; univ 38 13.4 / 15.4; pre disk: univ 8
79.3 / 76.4; univ 38 11.7 / 11.5):

| univ | layout | endpoint | check_point | target | base | head | head writes acknowledged, mean ms |
|---|---|---|---|---:|---:|---:|---|
| 8 | snapshot | /sparql | off | ≥ 152.6 | 32,378.2 / 31,781.5 | **32,953.2 / 32,645.1** | 101 / 0.7, 101 / 0.7 |
| 8 | snapshot | /sparql | on | ≥ 76.3 | 60.7 / 43.6 | 67.7 / 45.8 | 37 / 277.8, 37 / 280.8 |
| 8 | snapshot | /api | off | ≥ 153 | 13,235.6 / 12,003.9 | **12,038.1 / 11,067.2** | 101 / 1.1, 101 / 1.2 |
| 8 | snapshot | /api | on | ≥ 76.5 | 34.5 / 43.9 | 34.5 / 39.4 | 39 / 277.6, 38 / 271.8 |
| 8 | disk, Stream | /sparql | off | ≥ 158.6 | 20,469.1 / 19,496.8 | **22,766.7 / 23,689.6** | 101 / 3.8, 101 / 3.4 |
| 8 | disk, Stream | /sparql | on | ≥ 79.3 | 20,718.0 / 19,552.3 | **21,801.4 / 21,231.5** | 101 / 7.5, 101 / 6.2 |
| 8 | disk, Stream | /api | off | ≥ 152.8 | 11,089.6 / 10,433.8 | **10,171.7 / 10,131.9** | 101 / 4.3, 101 / 4.1 |
| 8 | disk, Stream | /api | on | ≥ 76.4 | 11,016.4 / 10,342.3 | **10,177.6 / 9,924.1** | 101 / 4.0, 101 / 4.2 |
| 38 | snapshot | /sparql | off | ≥ 26.8 | 23,334.2 / 23,649.5 | **29,837.0 / 31,566.7** | 101 / 0.9, 101 / 1.0 |
| 38 | snapshot | /sparql | on | ≥ 13.4 | 10.2 / 12.9 | 19.5 / 10.1 | 9 / 1313.6, 9 / 1259.2 |
| 38 | snapshot | /api | off | ≥ 30.8 | 10,129.5 / 10,377.7 | **9,341.5 / 9,908.7** | 101 / 2.2, 101 / 2.3 |
| 38 | snapshot | /api | on | ≥ 15.4 | 5.8 / 7.5 | 7.7 / 6.0 | 9 / 1334.5, 7 / 1663.9 |
| 38 | disk, Stream | /sparql | off | ≥ 23.4 | 17,621.5 / 17,822.6 | **20,838.3 / 22,284.6** | 101 / 3.9, 101 / 3.7 |
| 38 | disk, Stream | /sparql | on | ≥ 11.7 | 16,887.9 / 18,178.6 | **21,122.9 / 20,644.1** | 101 / 6.2, 101 / 5.9 |
| 38 | disk, Stream | /api | off | ≥ 23 | 10,122.0 / 10,468.6 | **9,676.5 / 9,856.8** | 101 / 5.8, 101 / 4.2 |
| 38 | disk, Stream | /api | on | ≥ 11.5 | 9,586.4 / 10,534.8 | **9,407.4 / 9,418.1** | 101 / 4.1, 101 / 4.3 |

With `check_point` off every point is far above its target on both layouts
(**met**); head / base: snapshot `/sparql` +2% / +31%, `/api` −8% / −6%
(univ 8 / 38), Stream `/sparql` +16% / +22%, `/api` −6% / −5% (the base side
is a different client program, `srv_mixed_load`; the harness's own read-only
8-client snapshot `/api` points above have the head 1% and 17% ahead). With
`check_point` on, the **Stream** database meets its targets by two to three
orders of magnitude (**met**): a write is a commit point (4–8 ms) and
readers keep reading. On the **snapshot layout** each acknowledged write
saves the whole snapshot under the exclusive guard (278–281 ms at univ 8,
1.26–1.66 s at univ 38), the writer runs back to back and readers get in
between saves only: below pre at univ 8 and at univ 38 except one `/sparql`
run (19.5 ≥ 13.4), the same as the base (60.7 / 43.6, 34.5 / 43.9; 10.2 /
12.9, 5.8 / 7.5) — **not met**, unchanged from R3 (R4-SNAP request 2,
below). The base's Stream reader rate without a writer (`srv_mixed_load`'s
read-only mode: 6,200–7,190 `/sparql` qps at univ 8, 570–607 at univ 38) is
lower than with one: before R4-VS its out-of-core VS-tree probe dominated
Q4, and an insert invalidates that tree (its `db/mod.rs`), so the reads
after the first write skip the probe.

#### Monitoring endpoints at univ 38 (G4; R3-SRV request 6)

Real `gserver --api` processes as for `/begin` (`check_point` on, the
default), a Python client (`http.client`, a fresh connection per request,
Basic credentials for the REST routes). Per session: `/api load`; every
endpoint once (the first `/api monitor` of a loaded database computes its
statistics); 5 idle rounds; **after a change**: for each endpoint three
times, one `/api` `query` with a single-triple `INSERT DATA` (acknowledged
after its check_point save), then the endpoint at once; **during a save**:
an `INSERT DATA` in a thread and the endpoint sent 0.15, 0.6, 0.9, 1.1 and
1.3 s into it (only the samples sent while the update was still running
count), plus three updates per endpoint during which the endpoint is
requested back to back (every 5 ms) until the update is acknowledged — the
save's fsync tail included; **during a long query**: one more change, then
the never-ending `ASK { ?a ub:advisor ?x . ?b ub:advisor ?y
FILTER(STRLEN(STR(?x)) + STRLEN(STR(?y)) < 0) }` through `/api`, then `/api
monitor` (the first call after that change) and 5 rounds of every endpoint;
the server is killed afterwards (the query never ends). Snapshot layout:
four sessions (base, head, head, base) on the shared snapshot database (each
session inserts triples of its own; a check_point update took 1.2–1.4 s,
median, up to 2.9 s); Stream: one session per side on its own `gbuild
--disk` database (a commit point, 5 ms per update). Maximum latency per
case, ms (single samples: 6–10 per case, endpoint and side; back to back:
150–1,430 samples per endpoint and side, `/api stat` 35–36):

| layout | endpoint | idle | right after a change | sent during a save | back to back through a save | long query: first call after a change | long query: later calls |
|---|---|---:|---:|---:|---:|---:|---:|
| snapshot | `GET /status` | 1.5 / 2.0 | 1.1 / 1.8 | 1.6 / 5.4 | 10.2 / 9.3 | | 1.5 / 1.6 |
| snapshot | REST `GET /monitor` | 1.1 / 0.9 | 1.4 / 2.7 | 1.7 / 1.2 | 47.6 / 3.1 | | 1.6 / 3.0 |
| snapshot | REST `GET /show` | 1.0 / 0.9 | 1.1 / 1.5 | 1.8 / 3.2 | 8.8 / 8.0 | | 1.6 / 1.7 |
| snapshot | `/api show` | 1.0 / 0.9 | 1.3 / 1.3 | 2.0 / 2.5 | 3.8 / 7.8 | | 1.6 / 2.0 |
| snapshot | `/api monitor` | 1.8 / 1.1 | **667.7 / 44.1** | 8.1 / 43.5 | **193.1 / 48.4** | **703.4 / 42.8** | 1.7 / 43.6 |
| snapshot | `/api stat` | 202.4 / 202.6 | 202.1 / 202.2 | 206.0 / 202.2 | 203.6 / 219.8 | | 202.1 / 205.2 |
| Stream | `GET /status`, REST `/monitor`, `/show`, `/api show` | ≤ 1.1 / ≤ 1.6 | ≤ 1.6 / ≤ 1.3 | (a commit takes 5 ms) | | | ≤ 3.4 / ≤ 1.5 |
| Stream | `/api monitor` | 1.6 / 1.2 | **2,986.6 / 42.3** | | | **2,687.2 / 43.0** | 2.4 / 43.4 |
| Stream | `/api stat` | 202.1 / 201.9 | 204.6 / 202.1 | | | | 202.2 / 202.2 |

(base / head.) No request failed. The head's `/api monitor` waits at most 40
ms for the background statistics refresh, then answers with the statistics
of the data before the change and the current counts (median 42 ms whenever
a refresh is pending — during a save it cannot run, the save holds the
database); the base recomputes them on the request path after every change:
0.59–0.70 s on the snapshot layout, 2.3–3.0 s out of core on Stream, also
while the long query runs, and up to 193 ms when it lands in a save. The
first `/api monitor` of a freshly loaded database computes the statistics on
both sides (snapshot 0.66–0.73 s; Stream 2.40 s) — not one of G4's three
cases. A first pass of this probe (same procedure, offsets up to 2.9 s, no
back-to-back loop) agrees: head ≤ 43.9 ms in every case, base 0.60–0.76 s
after a change and 199 ms 1.1 s into a save; its second session per side
re-inserted the first session's triples (no change, no save) and was
replaced by the pass above.

- `/status`, REST `/monitor` and `/show`, `/api show`, `/api monitor` ≤ 100
  ms while a long query runs, right after a change and during a check_point
  save: **met** (head max 48.4 ms; base `/api monitor` 0.19–3.0 s, and once
  REST `/monitor` 47.6 ms in a save).
- `/api stat`: **not met as written** — 201.6–219.8 ms on both sides in
  every case, idle included (the first call after the load 202–246 ms): it
  samples the CPU usage over 200 ms by design (upstream's semantics; R3
  measured the same); what it adds beyond that sample is ≤ 18 ms in every
  case.

#### Acceptance (R4-SNAP and G4), quiet

| item | target | measured (head; base) | verdict |
|---|---|---|---|
| `/begin` on a memory database (univ 38) | ≤ 5 ms, RSS growth ≤ 4 MiB | 0.66–1.08 ms, +20 KiB (base 91–927 ms, +222 MiB) | **met** |
| `/begin` on a Stream disk database (univ 38) | ≤ 50 ms, ≤ 16 MiB | 0.52–0.94 ms, +20 KiB (base 1.12–1.36 s, +201 MiB) | **met** |
| `ConcurrentDb` 1-triple update, terms existing | ≥ 1.7× lower latency than the base | univ 8 0.084 / 0.109 ms vs 48.3 / 48.8 ms (443–581×); univ 38 0.510 / 0.541 ms vs 209 / 221 ms (387–434×); copy fallback 3.0–3.4× | **met** |
| A disk database through the servers: Q4 qps 8 / 1 clients (Stream and Materialize, `/sparql` and `/api`) | ≥ 2.5× | Stream 4.57 / 4.85, 4.23 / 4.59 (univ 8), 4.35 / 4.11, 3.92 / 4.50 (univ 38); Materialize 5.49 / 6.02, 4.12 / 4.71, 5.75 / 5.66, 3.81 / 3.92 (base 1.70–3.31×) | **met** |
| G4: the same on a memory database | ≥ 2.5× | 4.39 / 5.08, 4.06 / 4.58 (univ 8); 5.00 / 5.53, 3.90 / 4.60 (univ 38) | **met** |
| Memory layout Q1–Q14 | within ±5% of the base | geomean 0.693× / 0.569×; slowest query q11 +2.1% (univ 8), at univ 38 q11 0.98×, q12 0.95× | **met** |
| Memory layout build | within ±5% | +4.3% (univ 8), −1.6% (univ 38) | **met** |
| Memory layout insert throughput | within ±5% | chunk insert 0.837× / 0.812× the base's time; single inserts 1.03× / 0.99× | **met** |
| Batched `TripleStore` inserts / removes (R4-RECLAIM request) | — | 5,330 / 130,000 low-id triples: insert 95× / 249× (univ 8), 75× / 258× (univ 38) faster than one by one, remove 83× / 132×, 208× / 605×; wired: churn insert on / off 0.99× / 1.04× (part 1) | **met** |
| `store.bin` / `named.bin` bytes, `Debug` | unchanged | `gbuild` output byte-identical to the base's; golden / legacy-mirror tests pass | **met** |
| G4 monitoring: `/status`, REST `/monitor` and `/show`, `/api show`, `/api monitor` while a long query runs, right after a change, during a check_point save (univ 38) | ≤ 100 ms | ≤ 48.4 ms on the snapshot layout, ≤ 43.4 ms on Stream (base `/api monitor` 0.6–0.7 s after a change and during the long query, 193 ms in a save; 2.3–3.0 s on Stream) | **met** |
| G4 monitoring: `/api stat` | ≤ 100 ms | 201.6–219.8 ms, idle included, both sides (a 200 ms CPU sample by design; ≤ 18 ms beyond it) | **not met** as written (by design; follow-up 2) |
| G4 mixed, check_point off: reader qps (snapshot, Stream) | ≥ 2 × pre's read-only 8-client Q4 | snapshot 32,953 / 32,645, 12,038 / 11,067 (univ 8), 29,837 / 31,567, 9,342 / 9,909 (univ 38); Stream 22,767 / 23,690, 10,172 / 10,132, 20,838 / 22,285, 9,677 / 9,857 (targets 23–159) | **met** |
| G4 mixed, check_point on, Stream | ≥ pre | 21,801 / 21,231, 10,178 / 9,924 (univ 8); 21,123 / 20,644, 9,407 / 9,418 (univ 38) (targets 11.5–79.3) | **met** |
| G4 mixed, check_point on, snapshot layout | ≥ pre | univ 8 67.7 / 45.8 (`/sparql`, ≥ 76.3), 34.5 / 39.4 (`/api`, ≥ 76.5); univ 38 19.5 / 10.1 (≥ 13.4), 7.7 / 6.0 (≥ 15.4); base 60.7 / 43.6, 34.5 / 43.9, 10.2 / 12.9, 5.8 / 7.5 | **not met** (= base; full snapshot save per update; follow-up 1) |
| check_point on with the snapshot layout (R3-SRV request 2) | saves off the exclusive guard | not implemented (investigated; request 2) | **not met** (follow-up 1) |
| Identical answers | — | every concurrency and mixed point one distinct body, equal to the base's and across layouts; Q1–Q14 rows and hashes equal in every run | **met** |

**G4 (concurrency): partly met** — Q4 scaling ≥ 2.5× via `/sparql` and
`/api` on memory, Stream and Materialize databases at univ 8 and 38;
`/begin` O(1) in time and memory; monitoring within 100 ms in every case
except `/api stat`'s fixed 200 ms sample; mixed workload met everywhere but
on the snapshot layout with `check_point` on (unchanged from R3 and equal to
the base).

Follow-ups (plan §5.7; recorded for R4-CLOSURE):

1. **check_point on, snapshot layout** (R4-SNAP request 2; owner of
   `db/persist.rs`): save from a captured copy-on-write image
   (`Database::save_image`: dictionary, store and named graphs are O(1)
   clones now; the VS-tree needs a copy-on-write form or is rebuilt from the
   captured store), so the HTTP layer releases the exclusive guard after
   capturing it and serves non-transactional reads the pre-update snapshot
   until the save is durable. Until then the advice for write-heavy serving
   with check_point on is the disk layout (`layout=disk`), which meets every
   mixed target.
2. **`/api stat`** (`gapi/ops_sys.rs`): its CPU figure is a blocking 200 ms
   sample per request; a background sampler (the last 200 ms window,
   refreshed by a small thread or on demand at most every 200 ms) would
   answer in ~1 ms with the same semantics.
3. **The first `/api monitor` of a freshly loaded database** computes the
   statistics on the request path (0.66–0.73 s on the snapshot layout, 2.4 s
   on Stream at univ 38, both sides): starting the background refresh at
   load would make every call ≤ 40 ms. (Not one of G4's cases.)

Commands (this stage; base = `/tmp/gstore-base-d702463/bin`, `S` = the
scratch directory):

```sh
cargo build --release -p gstore -p gstore-bench --bins && cargo build --release -p gstore --examples
cargo test --release -p gstore --test dt_cow_store --no-run
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
# 1. ConcurrentDb update latency / batched writes (dt_cow_store drivers; base: scratch test at d702463)
GSTORE_COW_BENCH_NT=$GSTORE_BENCH_CACHE/lubm-like1-u38-s0.nt flock /tmp/gstore-bench.lock \
    target/release/deps/dt_cow_store-* --ignored --nocapture --exact concurrent_update_latency
GSTORE_COW_BENCH_NT=… GSTORE_COW_BENCH_BATCH=130000 flock /tmp/gstore-bench.lock \
    target/release/deps/dt_cow_store-* --ignored --nocapture --exact batch_of_low_ids_latency
# 2. memory layout, 3 pairs per scale (base: its own harness, without --extra)
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale 38 --extra none \
    --backends snapshot --no-conc --bin-dir target/release --work-dir /tmp/r4snap-work --out head.json
# 3. concurrency + mixed, 2 pairs per scale and layout (base: no --extra/--label/--mixed-*)
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale 38 --extra none \
    --backends disk --env GSTORE_DISK_QUERY_MODE=stream --label stream --reps 1 --query-procs 1 \
    --open-reps 1 --no-update --conc-clients 1,8 --conc-secs 5 --conc-queries q1,q4 \
    --conc-endpoints sparql,api --mixed-secs 10 --bin-dir target/release --out head-stream.json
target/release/gstore-bench merge --out bench/results/phase6-r4-conc-lubm38.json head-snapshot.json head-stream.json head-materialize.json
target/release/gstore-bench report bench/results/phase6-r4-conc-lubm38.json --markdown --compare base-merged.json
# base mixed: srv_mixed_load built at d702463 (+ a scratch --prebuilt DIR option for the gbuild --disk database)
flock /tmp/gstore-bench.lock $S/srv_mixed_load --data $GSTORE_BENCH_CACHE/lubm-like1-u38-s0.nt --secs 10 \
    --readers 8 --write-ms 100 --query q4 --scratch /tmp/r4snap-mixed
# 4. /begin and monitoring: gserver --api H --db-home H --port 9311 --thread-num 8
#    (GSTORE_DISK_QUERY_MODE=memory|stream), curl -w %{time_total} + /proc/<pid>/status VmRSS;
#    monitoring: a Python client (http.client), cases as described above
# 5. save: a scratch test driver (open, save x3) on both sides; callgrind --toggle-collect='*save_locked*'
```

Every database this stage built was deleted (the work directories, the
`/begin` and monitoring homes, the save drivers' copies); the base worktree,
its target directory and the scratch drivers were removed.

### Implementer run (noisy)

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

#### Measure-stage commands

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
- Verified at the final code (`ddad582`, the fragment edits after it touch
  nothing else): `cargo fmt --all -- --check`; `cargo clippy --workspace
  --all-targets -- -D warnings`; `cargo build --workspace --all-targets`;
  `cargo test --workspace --no-fail-fast` with
  `GSTORE_DISK_QUERY_MODE=memory` and `=stream` (107 test binaries each,
  2,268 passed, 0 failed, 6 ignored); `cargo clippy -p gstore --all-targets
  --features rocksdb -- -D warnings` and `cargo test -p gstore --features
  rocksdb` (98 binaries, 2,115 passed, 0 failed). The gzip / tls / zip /
  readline / pfn-wasm matrix was not run: no code of this package is
  behind those features (the RocksDB layout checks are `cfg!`
  expressions, compiled in both configurations, and covered above).
