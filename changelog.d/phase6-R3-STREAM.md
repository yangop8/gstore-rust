# R3-STREAM — `Backend::Disk` Stream mode in Database: out-of-core triples and dictionary

Package R3-STREAM of `docs/PHASE6_PLAN.md` §6 (Round 3), contract C7. Base:
`0fb8842` (Phase 6 Rounds 1 and 2 merged). Commits per sub-part: (1)
`Backend::Disk`, (2) `DiskDict::intern`, (3) open options and `[storage]`,
(4)+(5) Stream mode in `Database` (open, writes, transactions), (6)/(6b) the
disk build delegation, (7)/(7b) posting lists and point probes, (8) tests,
(9) CI.

## Changelog

### Phase 6 R3-STREAM: on-disk databases can answer queries straight off the B+ trees

- **Stream mode** (`DiskQueryMode::Stream`). An on-disk (`kvstore.kv`,
  `gbuild --disk`) database opened in Stream mode evaluates queries over the
  B+ trees (`Backend::Disk(DiskTriples)`, reads reported per statement, C2)
  with an out-of-core dictionary (`DiskDict` on the same pager, opened once
  per database and shared by every statement). Opening reads nothing
  proportional to the data except the named graphs (kept in memory, D11) and
  the O(P) predicate statistics: at univ 38 (5.18M triples) the open takes
  0.6 ms instead of 1.4 s (cold, page cache dropped: 4–11 ms instead of
  1.5 s), RSS after open is 5.8 MiB instead of 351 MiB, and the query
  high-water mark 211 MiB instead of 461 MiB, with Q1–Q14 at 1.10–1.11× the
  Materialize mode's geomean (quiet measure stage, see Measured numbers).
  `vstree.kv` is used while it is
  fresh and is never rebuilt at open unless `vstree_on_open` is set;
  `rebuild_index()` rewrites it (one streaming pass over SPO/OSP, deferred to
  the end of an open transaction) and marks it fresh in the store's header.
- **Materialize mode stays the default** (`DiskQueryMode::Materialize`, the
  pre-Phase-6 in-memory working set with write-through by dictionary key).
  The default is the single constant `gstore::db::DEFAULT_DISK_QUERY_MODE`.
  The quiet measure stage kept it: every R3-STREAM acceptance item passes at
  univ 38, but gate G1d does not (Stream geomean 2.7× the snapshot layout's,
  Q1/Q3/Q4 12–41×; Materialize mode misses it by as much), and plan §2 keeps
  Materialize the default while G1d fails. Stream mode is opt-in
  (`disk_query_mode=stream`, `GSTORE_DISK_QUERY_MODE=stream`,
  `OpenOptions`); the flip stays a one-line change. Both modes read and write the same
  version-1 `kvstore.kv` (no feature bit), so a directory can be opened in
  either mode, and by older binaries, in turn. In Materialize mode a
  dictionary whose ids have a gap (a duplicate or missing slot) is now read
  with its ids kept, and `vstree.kv` is then rebuilt instead of trusted.
- **Open options (contract C7)**: `OpenOptions { disk_query_mode,
  disk_cache_pages, dict_cache_bytes, vstree_on_open, default_layout }`,
  `Database::open_with(dir, &options)`, `Database::load_disk_with`,
  `Database::disk_query_mode()`, `set_default_open_options` /
  `default_open_options` (all re-exported at the crate root). Precedence:
  `Database::open` uses the installed defaults (or the built-in ones) with the
  environment on top — `GSTORE_DISK_QUERY_MODE=stream|memory`,
  `GSTORE_DISK_CACHE_MB`, and `GSTORE_VSTREE_ON_OPEN` (Round 1's RocksDB
  switch, now an override of `vstree_on_open`); `open_with` takes its
  options as given. A RocksDB open builds the VS-tree only with
  `vstree_on_open` (as before, off by default).
- **`conf.ini` `[storage]` section** (Rust extension): `disk_query_mode`
  (`memory`/`stream`), `disk_cache_mb` (16), `dict_cache_mb` (80),
  `vstree_on_open` (off), `default_layout` (unset; `snapshot`/`disk`/
  `rocksdb`, for the build paths of R4-SNAP). `GStoreConfig::open_options()`;
  `gserver` installs the section as the process defaults at startup and logs
  the effective mode.
- **Writes in Stream mode** go by id: a new term is interned through the
  shared pager first (`DiskDict::intern`: `id2*`, then `*2id`, then the count
  root, one pager operation under the pager write lock, no gate — an orphan
  term if its triple never comes), then the triple through `DiskTriples`
  (`touched()` exactly when it returns `Ok(true)`; a write error breaks the
  store and fails the statement). Named-graph quads go through
  `DiskTriples::named_op`. Transactions are the store's overlay: no triple
  reaches `kvstore.kv` before `commit`, `rollback` never touches it, and the
  default graph needs no undo log (the named graphs keep theirs). A broken
  store refuses every statement (reads included) until the database is
  reopened. `CLEAR DEFAULT` reads the default graph in a read scope and
  removes it under one drain of the writer gate. `into_memory()` reads the
  store and every term (ids kept). The in-memory substring index is never
  built over a backed dictionary (substring filters scan, D5).
- **`DiskDict` can intern** (`DiskDict::writable()`; read-only stays the
  default for `DiskStore::lazy_dictionary` and views). A new term bumps the
  generation; since an intern only adds, the caches are updated in place
  rather than emptied. `DiskStore` now takes its term counts from the header
  wherever a shared dictionary may have interned (write operations, `flush`,
  `entity_num`, `dictionary()`, the lazy dictionary's freshness check), so no
  id is ever handed out twice.
- **`Database::build_disk` is the bulk build** (`bulk::build(..,
  BulkTarget::Disk, BulkOptions::default())`, also as
  `Database::build_disk_bulk_from_sources`): ids equal the memory build's,
  crash-safe install, and — like `gbuild --disk` — it refuses a directory a
  database of this process has open. The old bodies are kept as
  `#[doc(hidden)] build_disk_incremental` /
  `build_disk_from_sources_incremental`; `build_disk_from_sources` still
  forwards to the incremental build because its remaining callers use it as
  the incremental reference (see Requests).
- **Restore validation** opens the staged copy in Materialize mode, which
  reads the whole store, so a truncated on-disk backup is refused whatever
  the default mode is.
- **Faster Stream-mode reads** (results and order unchanged): `so_by_p`,
  `ps_by_o` and `subs_by_p` re-sort their index range with a radix sort on
  packed keys; `DiskTriples` keeps the long posting lists (2048+ entries) of
  the committed store in a 32 MiB cache keyed by the epoch (a write empties
  it); point reads of SPO (`exists`, `o_by_sp`, `po_by_s`) step a
  per-thread forward cursor instead of descending the tree for every probe
  of the engine's sorted candidates (a probe behind it or more than 48
  entries ahead descends as before; a locality score turns stepping off for
  random probe patterns). LUBM Q2 at univ 8: 1.39–1.44× the Materialize
  latency, from 2.0×; Q3 1.22×, from 2.16×.
- **CI** runs the test job twice, with `GSTORE_DISK_QUERY_MODE=memory` and
  `=stream` (same build). Tests that need one mode open with explicit
  `OpenOptions`.
- **Tests**: `tests/dt_disk_stream.rs` (Stream vs Materialize in order and vs
  the in-memory build as a multiset: LUBM Q1–Q14, named graphs, LIMIT /
  OFFSET / Top-K, paths with both ends unbound, CONTAINS / REGEX, DESCRIBE,
  CONSTRUCT, aggregates, graph functions; the read queries and data of the
  dt_sparql11 / dt_sparql_semantics / dt_sparql_syntax corpora; a seeded
  update workload with LOAD, CLEAR / DROP, ADD / COPY / MOVE, quads and
  `reason_execute` against an in-memory oracle, `mutation_count` advancing
  iff data changed, reopened alternately in Stream, Materialize and Stream
  mode; VS-tree freshness; terms interned on disk),
  `tests/dt_disk_stream_txn.rs` (read-your-writes, rollback and a commit
  point inside a transaction leave `kvstore.kv` byte-identical, commit +
  reopen in both modes, a 10,000-triple transaction, named graphs, an
  injected commit failure breaks the database until reopen, RDFS inference
  rolls back), `tests/dt_disk_stream_crash.rs` (`mem::forget` after the
  i-th acknowledged update, an uncommitted transaction across a `sync`, a
  crash between a term and its triple leaves only an orphan term), unit
  tests in `disk_dict.rs` (intern, rollback of a failed intern),
  `disk_triples.rs` (list cache), `store.rs` (radix sort, the SPO cursor
  against the tree), `open_options.rs` and `config.rs`.
  `db::tests::disk_database_mutation_invalidates_then_stays_correct` now
  covers both modes.
- **Changed test** (outside behaviour change): `dt_persist_backends`'
  `saving_another_layout_replaces_the_old_one` drops its open handle before
  rebuilding the directory with `build_disk` and checks that the bulk build
  refuses it while it is open.

## GAP_PLAN status line

§8 row 6 (规模与性能): R3-STREAM done — `Backend::Disk` Stream mode wired into
`Database` (out-of-core triples and dictionary, writes by id, store-overlay
transactions, broken-state refusal), open options C7 with `[storage]` and
environment overrides, `build_disk` on the bulk build. Quiet univ 38: open
0.6 ms vs 1.4 s, RSS after open 5.8 MiB vs 351 MiB, query HWM 211 vs 461 MiB,
Q1–Q14 geomean 1.10–1.11× the Materialize mode (0.30× pre disk); every
R3-STREAM acceptance item met, G1b/G1c met, G1d not met (2.7× the snapshot
layout; Materialize misses it too, the on-disk VS-tree probes), so
Materialize stays the default and Stream ships opt-in (plan §2).

## Docs notes

- docs/STORAGE_BACKEND.md (on-disk layout): the two disk query modes, what
  Stream mode keeps in memory (named graphs, the O(P) statistics, the page
  cache, the dictionary caches of `dict_cache_mb`, the 32 MiB long-list
  cache, one SPO cursor per reading thread) and what it reads on demand;
  `vstree.kv` used only while fresh, never rebuilt at open unless
  `vstree_on_open`, `rebuild_index()` rewrites it; writes by id, terms
  before triples (orphan terms harmless), transactions as the store overlay
  (no triple before commit; D12 still applies to very large commits); the
  broken state ("reopen the database"); `CLEAR DEFAULT` reads the default
  graph's ids into memory to remove them (no range delete, D9); a Stream
  database never builds the substring index (substring filters scan, D5);
  `Database::store()` panics for a Stream database (use `backend()`).
- docs/STORAGE_BACKEND.md (C7): `OpenOptions`, `Database::open_with`,
  `set_default_open_options` / `default_open_options`, the precedence
  (built-in < installed (`[storage]`) < environment; `open_with` as given),
  `GSTORE_DISK_QUERY_MODE`, `GSTORE_DISK_CACHE_MB`, `GSTORE_VSTREE_ON_OPEN`.
- docs/DEPLOY.md / docs/CONFIG (wherever `conf.ini` keys are listed): the
  `[storage]` section (`disk_query_mode`, `disk_cache_mb`, `dict_cache_mb`,
  `vstree_on_open`, `default_layout`); `gserver` applies it at startup (a
  loaded database keeps the mode it was opened in); the switch back to
  Materialize is `disk_query_mode=memory`.
- docs/DESIGN.md: `Backend::Disk(DiskTriples)` and the single id space of a
  Stream database (dictionary and store share the pager); lock order inside
  `Database` unchanged (`persist_lock` → disk → `persist_error` → pager), and
  `DiskDict::intern` takes only the pager write lock (gate → pager never
  inverted).
- docs/STORAGE_BACKEND.md (build): `Database::build_disk` uses the bulk
  build and refuses a directory a database of the process has open;
  `build_disk_incremental` is the old path.

## Measured numbers

### Quiet post-merge stage (phase6 `4ef1141`: R3 merged, review fixes)

Reference VM (4 vCPU Intel Xeon @ 2.10GHz, 16 GB), nothing else running
(20:17–21:07 UTC): CPU logged every 10 s from `/proc/stat` — busy 23–29%
while one measured process ran, up to 60% in `gbuild`'s parser threads and
99% at the 8-client concurrency points; steal ≤ 2%. Release build of
`4ef1141` (`cargo build --release -p gstore --bins -p gstore-bench`); every
heavy command under `flock /tmp/gstore-bench.lock`; data from
`/tmp/gstore-bench-cache` (univ 8 `ed44da2e7b3bbb98`, 1,029,275 triples;
univ 38 `fcd6493725f3c66a`, 5,181,103 triples). Per scale, in this order:
`gstore-bench run --profile reference --backends disk --drop-caches --env
GSTORE_DISK_QUERY_MODE=stream` (Stream a, with the concurrency and update
phases), the same with `=memory` (Materialize), Stream again with
`--no-conc` (Stream b), then the snapshot layout (`--backends snapshot`, for
G1d). Each run bulk-builds the same directory with this commit's `gbuild
--disk` (653,959,173 / 130,150,405 bytes every time) and deletes it
afterwards. Stream a is committed as
`bench/results/phase6-r3-disk-stream-lubm{8,38}.json`; Materialize and the
snapshot run are in `phase6-r3-lubm{8,38}.json`.

| metric | univ 38 Materialize | univ 38 Stream a | univ 38 Stream b | univ 8 Materialize | univ 8 Stream a | univ 8 Stream b |
|---|---:|---:|---:|---:|---:|---:|
| open first (cold, page cache dropped) / warm ms | 1491.2 / 1407.2 | **10.5 / 0.6** | 4.2 / 0.6 | 288.9 / 259.1 | 4.4 / 0.6 | 4.6 / 0.5 |
| RSS after open MiB | 350.7 | **5.8** | 5.7 | 88.6 | 5.9 | 5.7 |
| query high-water mark MiB (5 processes) | 460.7 | **210.8** | 210.6 | 119.2 | 69.2 | 69.1 |
| Q1–Q14 geomean ms | 39.77 | 44.13 (**1.109×**) | 43.70 (**1.099×**) | 5.494 | 6.671 (1.214×) | 6.827 (1.242×) |
| single INSERT DATA median / p95 ms (100) | 1.35 / 2.35 | 1.05 / 1.82 | 1.47 / 2.63 | 1.42 / 2.44 | 2.03 / 3.40 | 1.43 / 2.29 |
| single DELETE DATA median / p95 ms | 1.30 / 1.98 | 0.88 / 1.16 | 1.14 / 1.41 | 1.01 / 1.20 | 1.46 / 2.36 | 1.38 / 3.35 |
| chunk insert / delete ms (6,478 / 5,330 triples) | 1100 / 1042 | 893 / 935 | 1060 / 968 | 606 / 759 | 818 / 1063 | 734 / 737 |
| reopen after the updates ms | 2816.7 | 0.7 | 0.6 | 568.6 | 1.0 | 0.7 |
| `gbuild --disk` s | 21.01 | 20.06 | 19.16 | 4.49 | 3.73 | 3.90 |

Per query, univ 38 (warm ms: the minimum over 5 processes of the median of 5
runs; snapshot = the memory layout of the same commit, for G1d):

| query | rows | Materialize | Stream a (× Mat., Δ ms) | Stream b (× Mat., Δ ms) | snapshot | Stream a / snapshot |
|---|---:|---:|---:|---:|---:|---:|
| Q1 | 3 | 4.218 | 4.144 (0.98, −0.07) | 4.284 (1.02, +0.07) | 0.105 | **39.6** |
| Q2 | 115 | 51.03 | 72.75 (**1.43**, +21.7) | 70.62 (1.38, +19.6) | 38.03 | 1.91 |
| Q3 | 10 | 4.628 | 4.892 (1.06, +0.26) | 5.110 (1.10, +0.48) | 0.396 | **12.4** |
| Q4 | 9 | 2.123 | 2.153 (1.01, +0.03) | 2.192 (1.03, +0.07) | 0.096 | **22.4** |
| Q5 | 576 | 264.4 | 242.7 (0.92) | 239.1 (0.90) | 199.0 | 1.22 |
| Q6 | 404,399 | 163.7 | 215.6 (1.32) | 212.8 (1.30) | 122.0 | 1.77 |
| Q7 | 23 | 358.5 | 346.4 (0.97) | 341.0 (0.95) | 263.0 | 1.32 |
| Q8 | 10,778 | 371.8 | 308.3 (0.83) | 314.2 (0.85) | 217.8 | 1.42 |
| Q9 | 5,507 | 490.8 | 591.1 (1.20) | 581.0 (1.18) | 307.4 | 1.92 |
| Q10 | 3 | 278.7 | 263.9 (0.95) | 251.3 (0.90) | 194.1 | 1.36 |
| Q11 | 325 | 0.961 | 1.330 (1.38, +0.37) | 1.255 (1.31, +0.29) | 1.082 | 1.23 |
| Q12 | 21 | 0.546 | 0.662 (1.21, +0.12) | 0.655 (1.20, +0.11) | 0.483 | 1.37 |
| Q13 | 432,512 | 238.1 | 283.4 (1.19) | 274.4 (1.15) | 140.1 | 2.02 |
| Q14 | 305,503 | 118.8 | 153.7 (1.29) | 152.5 (1.28) | 87.9 | 1.75 |
| geomean | | 39.77 | 44.13 (1.109) | 43.70 (1.099) | 16.14 | **2.73** |

Univ 8, Stream / Materialize per query (a; b): Q1 1.18; 1.17, Q2 1.31;
1.34, Q3 1.15; 1.10, Q4 1.20; 1.18, Q5 1.07; 0.98, Q6 1.24; 1.36, Q7 0.99;
1.14, Q8 1.37; 1.34, Q9 1.38; 1.44, Q10 1.02; 1.07, Q11 1.36; 1.31, Q12
1.32; 1.39, Q13 1.25; 1.39, Q14 1.25; 1.29 — Q1/Q3/Q4/Q11/Q12 within
+0.11 ms. Stream / snapshot geomean 2.15–2.20× (Q1 12.2×). Rows and row
hashes of Q1–Q14 are identical in every result file of this stage (both
modes, the snapshot layout, the RocksDB files of part 1, pre-Phase-6 and
Round 2), stable in every process; every update round trip `ok`.
`gstore-bench compare` with Materialize as the base: PASS, 105 gates (Stream
a, both scales; the gate's calibration-normalized geomean ratio, over the
queries of ≥ 1 ms, 1.12 at univ 38 and 1.07 at univ 8); Stream b fails only
`exact.phase disk/conc` (it ran `--no-conc`). Stream a against the
pre-Phase-6 disk layout (`--backends disk`): PASS, 105 gates at both scales
(geomean ratio 0.29 at univ 38, 0.25 at univ 8; RSS-after-open gate 0.07 /
0.23). Both compare reports are in
`bench/results/phase6-r3-disk-stream-lubm{8,38}-compare.md`.

**Single-triple INSERT DATA, more samples.** The 100-insert medians above
do not separate the modes (fsync-bound: Stream 1.05 / 1.47 against 1.35 ms
at univ 38). Three alternating pairs at univ 38 with 500 inserts each
(`--backends disk --singles 500 --query-procs 1 --reps 1 --open-reps 1
--no-conc`, Materialize first in each pair): Materialize 1.268 / 1.289 /
1.287 ms, Stream 1.278 / 1.225 / 1.194 ms — mean of the medians **1.233 vs
1.281 ms (−3.8%)**, pair by pair +0.8%, −5.0%, −7.2%; p95 1.73–1.84 against
1.79–1.99 ms; deletes 1.31–1.43 against 1.33–1.61 ms.

**Opens, more samples.** Three more Stream runs per scale (`--open-reps 3
--query-procs 1 --reps 1 --no-conc --no-update --drop-caches`, scales
alternating) give five cold first opens each: univ 8 4.4 / 4.6 / 6.1 / 5.1 /
5.1 ms (median 5.1), univ 38 10.5 / 4.2 / 6.3 / 5.1 / 5.2 ms (median 5.2);
warm medians of the 15 warm samples 0.62 / 0.62 ms (max 0.83 / 0.71); RSS
after open 5.7–5.9 MiB at both scales. Ratios univ 38 / univ 8: open warm
1.00, open cold 1.02 (medians; the single pair a gave 10.5 / 4.4), RSS after
open 0.98–1.00.

**G4 on Stream-disk databases** (Stream a's concurrency phase: in-process
servers, query cache off, closed-loop keep-alive clients, 5 s per point;
bodies identical per endpoint and query):

| scale | endpoint | Q4 qps 1 → 8 clients | Q4 ratio | Q4 mean ms at 8 | Q1 qps 1 → 8 clients | Q1 ratio |
|---|---|---:|---:|---:|---:|---:|
| univ 8 | /sparql | 2,294 → 7,694 | **3.35×** | 1.03 | 1,793 → 4,625 | 2.58× |
| univ 8 | /api | 1,520 → 4,247 | **2.79×** | 1.81 | 1,425 → 4,212 | 2.96× |
| univ 38 | /sparql | 380 → 954 | 2.51× | 8.38 | 195 → 430 | 2.21× |
| univ 38 | /api | 355 → 934 | 2.63× | 8.50 | 185 → 444 | 2.40× |

(Materialize, same runs: univ 8 Q4 3.12× / 3.66×, univ 38 2.41× / 2.55×;
the snapshot layout 5.41× / 4.02× and 5.30× / 4.43×.) Disk databases scale
less than the snapshot layout in both modes, and their Q1/Q4 cost 2–5 ms
at univ 38 instead of 0.1 ms. The latency comes from the on-disk VS-tree
(below); it very likely limits the scaling too, since every query reads its
pages through one shared 4 MiB cache (not profiled under load).

**Why G1d fails: the on-disk VS-tree (both modes).** Q1, Q3 and Q4 cost
the same in both disk modes (4.2 / 4.6 / 2.1 ms at univ 38) and 12–41× the
snapshot layout's. Callgrind of `gquery` Q1 on the univ 38 Stream database
(first run, 46.8M instructions in all): 62% is `DiskTree::search` over
`vstree.kv` (`VsTree::candidates`), 49% of the total `Pager::read_page_shared`
— about 3,800 page reads (zeroing and copying 4 KiB each, `memset` 33% and
`memcpy` 17%), because the search visits more nodes than `DiskTree`'s
1,024-page (4 MiB) cache holds, so every run reads them again. The engine
work itself is small (`candidates::generate` 34%, mostly the `s_by_po` scan
for the bound object). R2-DISKLIB's probe (`disk_stream_probe --mode query
--cache-pages 4096`, the same univ 38 database) with and without
`vstree.kv`: Q1 5.75 → 0.22 ms, Q3 5.64 → 1.04, Q4 2.40 → 0.11, Q11 1.28 →
0.22, Q12 0.81 → 0.07 ms over `DiskTriples`, geomean 50.99 → 18.05 ms
(rows equal in order) — but Q2 86 → 315 ms, the one query the VS-tree
prunes well. Not a regression of this round (Round 2's quiet Materialize
run had Q1 4.35 ms, pre-Phase-6 39.1 ms) and not specific to Stream mode;
it needs the planner to skip the VS-tree when a bound constant's posting
list is short, or a cheaper disk VS-tree probe (Requests 8).

**Default-mode decision: Materialize stays the default.** Every R3-STREAM
acceptance item passes at univ 38 (table below), and G1b and G1c pass, but
G1d fails (Stream geomean 2.73× the snapshot layout's, limit 1.5×; Q1 40×,
Q3 12×, Q4 22×, limit 3×). Plan §2: "If G1d fails, Stream mode ships
opt-in and Materialize stays the default disk query mode." Materialize
misses G1d by about as much (2.46×; Q1 40×), so the gap is the disk
layout's, not Stream mode's; the flip remains the one-line change of
`DEFAULT_DISK_QUERY_MODE` once G1d passes (R4-SNAP / R4-CLOSURE report it).
No code changed at this stage.

**Acceptance (R3-STREAM, univ 38) and gates G1b–G1d, quiet:**

| item | target | measured | verdict |
|---|---|---|---|
| RSS after open | ≤ 120 MiB | 5.8 / 5.7 MiB (Materialize 350.7) | **met** |
| RSS after open, univ 38 / univ 8 | ≤ 1.5 | 0.98–1.00 (5.7–5.9 MiB at both scales) | **met** |
| Open warm | ≤ 0.5 s | 0.6 ms (median of 15; max 0.71 ms) | **met** |
| Open cold (`--drop-caches` first open) | ≤ 2.0 s | 4.2–10.5 ms (median 5.2 of 5) | **met** |
| G1b open univ 38 / univ 8 | ≤ 1.5 | warm 1.00, cold 1.02 (medians) | **met** |
| VmHWM through Q1–Q14 | ≤ 400 MiB | 210.8 / 210.6 MiB (Materialize 460.7) | **met** |
| Each Q1–Q14 vs Materialize, same directory | ≤ 2.0× | worst Q2 1.43× / 1.38×; all others ≤ 1.38× | **met** |
| Geomean vs Materialize | ≤ 1.4× | 1.109× / 1.099× | **met** |
| Q1, Q3, Q4, Q11, Q12 | ≤ Materialize + 5 ms | +0.48 ms at most (Q3, run b) | **met** |
| Auto-commit single-triple INSERT DATA median | ≤ Materialize | 1.233 vs 1.281 ms (3 alternating pairs × 500; −3.8%); reference runs 1.05 / 1.47 vs 1.35 ms (100 each) | **met** (pair 1: +0.8%, inside its spread) |
| Identical answers | identical | rows and row hashes equal across both modes, snapshot, RocksDB, pre, Round 2; compare exact gates pass | **met** |
| G1c (RSS ≤ 120 MiB, HWM ≤ 400 MiB, ratio ≤ 1.5) | | as above | **met** |
| G1d geomean vs pre disk | ≤ 0.6× | 44.13 vs 146.92 ms = 0.30× | **met** |
| G1d geomean vs the memory (snapshot) layout | ≤ 1.5× | 44.13 vs 16.14 ms = **2.73×** (Materialize 2.46×) | **not met** |
| G1d no query > 3× memory | ≤ 3× | Q1 39.6×, Q3 12.4×, Q4 22.4× (others ≤ 2.02×) | **not met** |
| G4 Q4 8 / 1 clients on a Stream-disk database, univ 8 | ≥ 2.5×, /sparql and /api | 3.35× and 2.79× (≥ 215 qps: 7,694 / 4,247) | **met** (univ 38: 2.51× / 2.63×) |
| Default flip to Stream | only if every gate passes | not flipped (G1d) | Materialize stays the default |

### Implementer run

**Implementer run, noisy (other agents compiling; load average 1.0–2.7 on 4
vCPUs during the runs).** Univ 8 only; univ 38 is left to the quiet measure
stage (commands below). Every heavy command ran under `flock
/tmp/gstore-bench.lock`. There is no base counterpart for Stream mode (it is
new); the comparison is the plan's: Stream against Materialize, same binary,
same session, on the same bulk-built data. Release binaries of this branch;
`gstore-bench run --profile reference --scale 8 --backends disk --no-conc
--env GSTORE_DISK_QUERY_MODE=stream|memory` (each run bulk-builds its own
database with this branch's `gbuild --disk`, 3.9–5.1 s, `kvstore.kv` +
`vstree.kv` 130.2 MB, deleted after the run), separate `--out` files,
compared with `gstore-bench compare memory.json stream.json --markdown`.

Final code (`894c0e9`), Materialize run between two Stream runs (a, b):

| metric | Materialize | Stream a | Stream b |
|---|---:|---:|---:|
| open first / warm ms | 273.0 / 304.2 | 0.7 / 0.7 | 0.8 / 0.6 |
| RSS after open MiB | 90.2 | 7.1 | 7.1 |
| query VmHWM MiB | 120.7 | 70.3 | 70.4 |
| Q1–Q14 geomean ms | 5.68 | 7.02 (1.24×) | 7.13 (1.26×) |
| single INSERT DATA median / p95 ms | 1.48 / 2.09 | 1.60 / 3.03 | 1.58 / 2.50 |
| single DELETE DATA median / p95 ms | 1.27 / 1.80 | 1.14 / 1.41 | 1.21 / 1.49 |
| chunk insert / delete ms (5,330 triples) | 687.5 / 824.2 | 545.8 / 1024.1 | 726.7 / 887.1 |
| reopen ms | 597.4 | 1.9 | 0.7 |

| query | rows | Materialize ms | Stream a ms (ratio) | Stream b ms (ratio) |
|---|---:|---:|---:|---:|
| q1 | 3 | 0.37 | 0.43 (1.16) | 0.41 (1.12) |
| q2 | 25 | 8.34 | 12.01 (1.44) | 11.55 (1.39) |
| q3 | 10 | 0.39 | 0.48 (1.23) | 0.48 (1.22) |
| q4 | 9 | 0.18 | 0.22 (1.25) | 0.23 (1.30) |
| q5 | 576 | 34.60 | 34.64 (1.00) | 34.09 (0.99) |
| q6 | 80,056 | 29.72 | 39.48 (1.33) | 40.07 (1.35) |
| q7 | 23 | 48.97 | 52.25 (1.07) | 55.08 (1.12) |
| q8 | 10,778 | 46.36 | 55.86 (1.20) | 59.03 (1.27) |
| q9 | 1,081 | 65.24 | 91.26 (1.40) | 93.04 (1.43) |
| q10 | 3 | 32.54 | 32.76 (1.01) | 38.19 (1.17) |
| q11 | 325 | 0.33 | 0.48 (1.48) | 0.46 (1.42) |
| q12 | 21 | 0.14 | 0.18 (1.29) | 0.17 (1.23) |
| q13 | 85,670 | 39.07 | 48.38 (1.24) | 49.81 (1.28) |
| q14 | 60,576 | 20.13 | 26.85 (1.33) | 28.03 (1.39) |

Every query's row count and row hash (`rows_fnv64`) are identical in the
three result files, and the update phase's round trip (singles, chunk,
re-check of every query) is `ok` in each. `gstore-bench compare`'s own
calibrated gates (Materialize as the base): PASS (96 checked).

Before the posting-list work (commit (6b) code, same procedure, one pair):
Stream geomean 7.97 ms vs Materialize 5.79 ms (1.38×), Q2 17.51 vs 8.72 ms
(2.01×), Q3 0.95 vs 0.44 ms (2.16×), Q4 1.10×, Q11 1.66×; single insert
median 1.65 vs 1.39 ms. A later pair with an intermediate SPO-walk heuristic
gave single inserts of 1.21 / 1.61 ms (Stream a / b) vs 1.97 ms
(Materialize): at this noise level the single-insert medians (fsync-bound,
20 samples) do not separate the modes.

Instruction counts (callgrind, `valgrind --tool=callgrind` over a scratch
program that opens the univ 8 database and runs one query N times with the
query cache off; deterministic, unlike the timings): LUBM Q2, open plus
three runs, 402M instructions before the posting-list work and 295M after;
one warm Q2 run is 95M instructions in Stream mode against 68M in
Materialize mode (the open subtracted). Q3, open plus three runs: 39M → 26M
(the long `type Publication` list comes from the list cache after the first
run). The remaining Stream overhead of Q2 is
the `o_by_sp` probes (B+ tree leaf stepping instead of a map lookup) and of
the large-result queries the out-of-core dictionary.

### Verification

`cargo fmt --all -- --check` clean; `cargo build --workspace --all-targets`
and `cargo clippy --workspace --all-targets -- -D warnings` clean;
`GSTORE_DISK_QUERY_MODE=memory cargo test --workspace`: 2,197 passed, 0
failed, 2 ignored; `GSTORE_DISK_QUERY_MODE=stream cargo test --workspace --
--skip tolerant_report_and_log_equal_the_sequential_import`: 2,196 passed, 0
failed, 2 ignored (the CI's two passes). RocksDB (`--features rocksdb`,
`-p gstore`): clippy clean; tests 2,018 passed, 0 failed, 2 ignored. After
the RocksDB directory lock was taken out again (Request 7) the RocksDB pass,
fmt, the workspace clippy and the touched test files in both modes were
re-run; the two full workspace passes above ran just before that change,
which touches only the RocksDB layout's open and create.

### Acceptance (R3-STREAM)

- RSS after open ≤ 120 MiB and RSS ratio univ 38 / univ 8 ≤ 1.5: **univ 8
  met** (7.1 MiB); univ 38 and the ratio **left to the measure stage**.
- Open warm ≤ 0.5 s, cold ≤ 2.0 s: **univ 8 met** (0.6–0.8 ms warm; the
  bench's first open 0.7–0.8 ms, page cache not dropped); univ 38 and a cold
  (`--drop-caches`) open **left to the measure stage**.
- VmHWM through Q1–Q14 ≤ 400 MiB: **univ 8 met** (70.4 MiB); univ 38 **left
  to the measure stage**.
- Each Q1–Q14 ≤ 2.0× Materialize, geomean ≤ 1.4×, Q1/Q3/Q4/Q11/Q12 ≤
  Materialize + 5 ms: **met at univ 8** in this noisy run (worst 1.48× Q11,
  geomean 1.24–1.26×; Q1/Q3/Q4/Q11/Q12 within +0.15 ms); the gate is at
  univ 38 — **left to the measure stage**.
- Auto-commit single-triple INSERT DATA median ≤ Materialize: **not
  established** — 1.58–1.60 ms vs 1.48 ms in the final pair, 1.21–1.61 vs
  1.97 ms in an earlier one; fsync dominates and the noise exceeds the
  difference. **Left to the measure stage** (quiet machine).
- Identical answers: **met** (all bench runs; `dt_disk_stream*` tests,
  in order against Materialize and as multisets against the memory build).
- Default flip: **not done** — Materialize stays the default until the
  measure stage has every gate at univ 38.

### For the measure stage (univ 38, quiet machine)

```sh
# release binaries of this branch (from the repository root)
cargo build --release -p gstore --bins -p gstore-bench
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
for run in stream-a:stream memory:memory stream-b:stream; do
  tag=${run%%:*}; mode=${run##*:}
  flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference \
      --scale 38 --backends disk --no-conc --drop-caches \
      --env GSTORE_DISK_QUERY_MODE=$mode --out /tmp/r3stream-u38-$tag.json
done
target/release/gstore-bench compare /tmp/r3stream-u38-memory.json /tmp/r3stream-u38-stream-a.json --markdown
target/release/gstore-bench compare /tmp/r3stream-u38-memory.json /tmp/r3stream-u38-stream-b.json --markdown
# the RSS / open ratios also need a Stream run at univ 8:
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference \
    --scale 8 --backends disk --no-conc --drop-caches \
    --env GSTORE_DISK_QUERY_MODE=stream --out /tmp/r3stream-u8-stream.json
```

Read from the Stream results: `RSS after open` (≤ 120 MiB; univ 38 / univ 8
≤ 1.5), `open warm` (≤ 0.5 s) and the `--drop-caches` first open (≤ 2.0 s),
`query HWM` (≤ 400 MiB); from the comparison: every query ≤ 2.0×, geomean ≤
1.4×, Q1/Q3/Q4/Q11/Q12 ≤ Materialize + 5 ms; single insert median ≤
Materialize. If all pass, flip `DEFAULT_DISK_QUERY_MODE` in
`src/db/open_options.rs` to `DiskQueryMode::Stream` (one line) and re-run
`cargo test --workspace` (the CI runs both modes already).

## Requests/Not yet

Requests (files outside R3-STREAM's list):

1. **Integrator / R3-ROCKS (`src/bin/gbuild.rs`) and R2-BULK's tests
   (`tests/dt_bulk_disk.rs`, `tests/dt_bulk_import.rs`)**: they use
   `Database::build_disk_from_sources` as the *incremental* reference
   (`gbuild --incremental`; `lubm_bulk_equals_incremental_and_memory`
   asserts the bulk file is ≤ 0.75× of it). Switch them to
   `Database::build_disk_from_sources_incremental`; then
   `build_disk_from_sources` can forward to `build_disk_bulk_from_sources`
   (one line in `persist.rs`), completing R2-BULK request 1. `build_disk`
   (used by gstore-bench and the tests) is already the bulk build.
2. **R3-SRV (`src/http_api.rs`, `ApiInner::refresh_config`)**: `refreshconf`
   should re-install `[storage]` for a server that owns the process
   defaults (`gserver`, as `with_log_refresh` does for the log):
   `crate::db::set_default_open_options(fresh.open_options())`. Databases
   loaded afterwards use it; loaded ones keep their mode. `gserver` installs
   it at startup.
3. **`tests/dt_bulk_import.rs` owner**: `tolerant_report_and_log_equal_the_sequential_import`
   reads an opened disk database through `Database::store()` and the legacy
   `id_to_string`, which only a Materialize database has; it should open
   with `Database::open_with(dir, &OpenOptions::materialize())`. Until then
   the CI's Stream pass skips it (`--skip
   tolerant_report_and_log_equal_the_sequential_import`; it still runs in
   the Materialize pass). It is the only test of the workspace that fails in
   Stream mode.
4. **R4-SNAP**: `/begin` (`ReadSnapshot::capture`) and `ConcurrentDb`
   snapshots of a Stream database still materialize the whole default graph
   through `iter_all` (O(triples) per `/begin`); use `DiskTriples::read_view`
   as planned (R2-DISKLIB's list). The snapshot's `Dictionary` clone shares
   the `DiskDict` (fine until ids are freed).
5. **R4-RECLAIM**: `DiskDict::free` must move both the generation and the
   caches' epoch (`cache_epoch`, which empties the caches; an intern does
   not); `Database::reclaim_unused` returns 0 for a backed dictionary until
   then. `DiskStore` takes the larger of its own and the header's term
   counts, which stays right with a free list (counts are the allocated
   limit). `LEGACY_PIN_SLOTS` was already removed at the Round-2 merge.
6. **R1-KV owner (`src/kvstore/bptree.rs`)**: the per-thread SPO cursor steps
   entry by entry (`LeafCursor::next_entry`, ~75 instructions per entry); a
   public "seek within the current leaf" (binary search on a uniform leaf)
   would make the stepping O(log) per probe.
7. **Integrator (`tests/dt_bulk_crash.rs`, `src/bulk/commit.rs`)**: the
   Round-2 review's request (addressed to R3-ROCKS) that `Database::open`,
   `create_rocksdb` and `open_rocksdb` take the directory's `DirLock` for the
   RocksDB layout too is **not** done here although `persist.rs` is this
   package's file: with the lock, a bulk build over a RocksDB database open in
   this process is refused by `bulk::commit`'s `DirLock::held_here` check
   ("is open in this process") before its RocksDB `LOCK` check, and
   `dt_bulk_crash::an_open_rocksdb_database_is_not_replaced` asserts the
   latter's message ("RocksDB store ... in use"). Taking the lock is three
   lines in `persist.rs` (`open_with`'s match, `create_rocksdb`,
   `open_rocksdb_with`) once that test accepts either refusal (or
   `bulk::commit` checks the RocksDB lock first); `http_api`'s
   `hold_dir_lock` skip for RocksDB then goes too.
8. **Quiet measure stage → R4-SNAP / R4-CLOSURE (the query engine's VS-tree
   use, `src/query/`; `src/signature/disk_vstree.rs`)**: G1d fails on both
   disk modes because every LUBM query with a bound constant probes
   `vstree.kv`, and at univ 38 a Q1 probe reads ~3,800 4 KiB pages through
   `DiskTree`'s 1,024-page cache (4–5 ms; the snapshot layout's in-memory
   tree answers Q1 in 0.1 ms). Without `vstree.kv` the Stream-mode geomean
   drops from 51 to 18 ms in R2-DISKLIB's probe (Q1 0.22 ms), but Q2 rises
   from 86 to 315 ms. Options: skip the VS-tree when a bound constant's
   posting list is short (the planner knows the counts, C1), cap a disk
   probe's page reads, or give the disk tree the in-memory tree's probe
   refinement. Then re-measure G1d and flip `DEFAULT_DISK_QUERY_MODE` if it
   passes (every R3-STREAM item already does). The same probes very likely
   limit Q4's 8-client scaling on disk databases at univ 38 (2.4–2.6×
   against 5.3× on the snapshot layout; not profiled under load).

Not yet / notes:

- Stream mode's page cache is the same 16 MiB as before (`disk_cache_mb`);
  larger caches did not change the instruction counts at univ 8, and the
  timings were too noisy to show an effect.
- `rebuild_index()` on a Stream database holds every entity's signature in
  memory while it writes `vstree.kv` (as the Materialize rebuild does); a
  sorted streaming writer (`VsTree::write_disk_sorted`) needs an external
  sort of the signatures.
- `CLEAR DEFAULT` on a Stream database reads the default graph's ids into
  memory (12 bytes per triple) before removing them (no range delete, D9).
- The corpus test extracts 54 distinct read queries (with their data) from
  the three SPARQL test files' string literals; queries assembled with
  `format!` arguments are not extracted.
- `gstore-bench` keys results by backend name, so the two modes need
  separate result files (R1-BENCH's note); a run label would help
  R4-CLOSURE's report.
