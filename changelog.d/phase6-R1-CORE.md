# Phase 6 R1-CORE — seam contracts, a Sync `&self` read path, per-query read errors, VS-tree fixes

Package R1-CORE of `docs/PHASE6_PLAN.md` §6 (Round 1). Base:
`90b92d6`. One commit per sub-part (A seam + C1 + C2; B Database Sync +
`read_as` / `Statement`; C VS-tree; D engine / optimizer), then two
commits of plan-preserving speed-ups found while measuring the acceptance
items (E1: exact predicate statistics, sorted bulk load, merged VS-tree
signing; E2: candidate lookups instead of predicate scans), a clippy /
formatting commit and this fragment.

## Changelog

### Phase 6 R1-CORE: the storage seam, a shared read path and VS-tree fixes

- **Storage seam (contract C1).** `Backend` and `backend_dispatch!` moved
  unchanged to `src/store/backend.rs` (still `gstore::store::Backend`).
  `TripleSource` gains provided methods `count_po_by_s`, `count_o_by_sp`,
  `count_p_by_so`, `count_ps_by_o`, `count_s_by_po`, `count_so_by_p`
  (`u64`, exactly the length of the list), `distinct_p_of_s`,
  `distinct_p_of_o` (`u64`) and `has_subject` / `has_object` /
  `has_predicate`, with defaults defined through the list methods;
  `TripleStore` overrides them with slice arithmetic, `Backend` dispatches
  them. The ordering contract (pairs sorted, id lists ascending,
  `iter_all` in `(p, s, o)` order) is documented on the trait. An override
  must equal the default exactly; `has_*` answer `true` on a read error.
- **Per-query read errors (contract C2).** New `gstore::store::read_scope`:
  `ReadScope::enter()` (a per-thread, nestable, `!Send` guard),
  `take_error()`, `note_read_error(msg)` and the process-wide
  `unscoped_read_errors()` counter (an error noted outside any scope is
  counted and logged). Every query, UPDATE `WHERE` evaluation and CSR build
  runs in a scope: a read error a backend reports fails that statement —
  never one running on another thread — and its result is never cached.
  No backend reports through it yet (DiskTriples, DiskDict and RocksStore
  do from Round 2/3 on).
- **`Database` is `Send + Sync` (contract C8).** `&self` methods may run
  concurrently, `&mut self` methods need exclusive access. The query cache
  sits behind a mutex and shares its entries (`Arc`), copied out after the
  lock is released; the disk write-through sink and the pending
  persistence error are behind mutexes; the incremental-backup checkpoint
  is atomic. A new `persist_lock` serializes `save`, `checkpoint`, `sync`,
  `backup` and `backup_incremental` (lock order: `persist_lock` → disk →
  `persist_error` → pager). A lock poisoned by a panic is recovered: the
  cache is emptied, the disk sink is marked broken (later commit points
  fail until the database is reopened). A compile-time assertion keeps
  `Database: Send + Sync`.
- **Parsed statements and the shared read path.** New `gstore::Statement`
  (`parse`, `is_update`, `text`, `query`). `Database::read_as(&self,
  &Statement, cancel, pfn)` and `Database::query_read(&self, &str, cancel,
  pfn)` run a SELECT / ASK / CONSTRUCT / DESCRIBE on a shared database —
  with the same cache, VS-tree, named graphs, functions, PFN caller and
  limits, `SERVICE` policy, CSR, integer division, Top-K options and
  graph-function bound as `query` — and refuse an UPDATE before doing
  anything. `Database::execute_as(&mut self, Statement, cancel, pfn)` runs
  either kind. `query`, `query_cancellable` and `query_as` keep their
  signatures and delegate.
- **RocksDB open no longer rebuilds the VS-tree.** It walked every
  entity's edges in RocksDB (most of the open time). A RocksDB database
  now opens with the index stale (`stats().index_valid == false`; `gshow`,
  `gmonitor` and the monitor endpoints report it as stale) and evaluates
  without it until `rebuild_index()`; `GSTORE_VSTREE_ON_OPEN=1` restores
  the rebuild at open. Answers are unchanged as multisets; without ORDER BY
  the row order may differ from a tree-filtered evaluation (VS-tree
  candidates feed the planner).
- **VS-tree.**
  - *Sort fix:* the in-memory and the out-of-core bulk builds cluster
    entries with an unstable sort on the raw signature limbs, then id,
    instead of a stable sort that built a limb vector per comparison (65–68%
    of a memory build before). Entries arrive in ascending id, so the order
    — and `vstree.bin` / `vstree.kv` — is byte-identical (tested against
    the legacy code).
  - *Locator:* the in-memory tree is an arena with parent links and an
    entity id → leaf locator (rebuilt on build and load, never
    serialized): `update` is O(height) instead of a scan of every entry,
    and `probe(id, &sig)` tests one entity's stored signature. The
    serialized form is written and read by hand as the earlier nested tree,
    so `vstree.bin` stays byte-identical.
  - *Probe refinement:* when the tree is in memory and a subject variable
    already has an exact candidate set of at most max(1024, entities/32)
    ids, each candidate is probed instead of walking the tree; the result
    equals walk ∩ candidates exactly. Out-of-core trees keep walking.
  - *Deletes keep the tree:* a delete (or a rollback's removal of the
    transaction's inserts) no longer invalidates the VS-tree — signatures
    only lose bits, so stored entries stay a sound superset; the new
    `Database::vs_shrink_debt()` counts such deletes until the next
    `rebuild_index()`. A rollback that re-adds deleted triples re-signs
    their entities. `CLEAR` and inference still invalidate, and an insert
    still invalidates a read-only out-of-core tree. This ends the "every
    save after a delete rebuilds the VS-tree" loop: a snapshot saved after
    deletes stores the live (superset) tree. After deletes, queries keep
    the tree's pruning (before, they ran without it until the next save or
    rebuild): same answers, but without ORDER BY the row order can differ
    from the base's in that window.
- **Engine and optimizer.**
  - Hash joins key their table with a std-only FxHash-style hasher instead
    of SipHash (output order unchanged: left-major, right-minor; tested
    against randomly keyed SipHash and the nested loop).
  - The cost model and planner count instead of materializing lists
    (`count_*`, `distinct_p_of_*`); every estimate is unchanged, so plans
    and solution order are identical. The `ps_by_o`-based subject NDV keeps
    its value with a FIXME (it counts predicates, not subjects).
  - `TripleStore` keeps each predicate's distinct subject / object counts
    (`pre2sub` / `pre2obj`) exactly under every mutation, so the cost model
    no longer scans — and for objects sorts — every predicate's triples on
    each query (the LUBM Q4 hotspot). They are not serialized (`store.bin`
    is unchanged) and are recomputed on read.
  - A bulk load into an empty `TripleStore` sorts once per index order and
    builds each map from its sorted runs with exactly sized lists (less
    time and memory; identical store), and the VS-tree build signs an
    in-memory store in one merged pass over its subject and object maps.
  - A `?s p ?o` scan whose subject or object variable has a candidate set
    under 1/8 of the predicate's triples looks the candidates up instead of
    scanning the predicate (same triples, same order). LUBM Q4's bushy plan
    scanned three whole predicates as leaves this way; Q4 no longer grows
    with the data. The executor looks a slot's candidate set up once per
    extension, and candidate sets use the fast hasher.
- **Visibility for later rounds.** `db::persist::{remove_other_layouts,
  wal_of}` are `pub(crate)` (R2-BULK).
- **Tests.** New `tests/dt_count_methods.rs`, `tests/dt_read_scope.rs`,
  `src/db/concurrency_tests.rs`; VS-tree mirror, probe, soundness and
  locator tests in `src/signature`; `TripleStore` statistics / bulk-load /
  byte-mirror tests; a reopen-without-rebuild test in
  `tests/dt_rocksdb.rs`; a deletes-keep-the-tree test in `tests/dt_lubm.rs`.
  **Changed assertion:** the db unit test
  `delete_invalidates_then_results_stay_correct` asserted that a delete
  invalidates the index; it is now `delete_keeps_the_index_valid_and_results_correct`
  and asserts the new behaviour.

## GAP_PLAN status line

§8 row 6 (规模与性能): R1-CORE done — C1 count seam, C2 per-query read
errors, `Database: Send + Sync` with a `&self` read path
(`read_as` / `query_read` / `Statement`), RocksDB open without a VS-tree
rebuild, VS-tree sort fix / locator / probes / delete-superset; memory
build and LUBM latency gates measured at univ 8 here, univ 38 by the measure
stage.

## Docs notes

For R4-CLOSURE (README / DESIGN / STORAGE_BACKEND / CHANGELOG):

- `docs/STORAGE_BACKEND.md`: the `TripleSource` ordering contract and the
  C1 count methods (defaults, exactness rule, `has_*` true on read error);
  `Backend` now lives in `src/store/backend.rs`.
- `docs/DESIGN.md` (query path / concurrency): `Database` is `Send + Sync`;
  `read_as` / `query_read` / `execute_as` and `Statement`; the lock order
  `persist_lock` → disk → `persist_error` → pager; `ReadScope` and
  `note_read_error` (C2).
- VS-tree: deletes keep the tree as a sound superset (`vs_shrink_debt`),
  `rebuild_index()` tightens it; probe refinement for small candidate
  sets; `vstree.bin` / `vstree.kv` bytes unchanged.
- RocksDB: open no longer rebuilds the VS-tree; `GSTORE_VSTREE_ON_OPEN=1`
  (environment) restores it until R3-STREAM's `OpenOptions::vstree_on_open`
  replaces it; `stats().index_valid` is `false` right after such an open.

## Measured numbers

### Quiet post-merge stage (phase6 `05be7e3`: R1 merged, review fixes, rustfmt)

Reference VM (4 vCPU Intel Xeon @ 2.10GHz, 16 GB), idle apart from this
stage (load average 1.0 while measuring; four idle debug `gserver`
processes of another workflow at 0% CPU). Release builds made before
measuring (snapshot and disk from the default build, RocksDB from a
`--features rocksdb` build in `target-rocks`); every heavy command under
`flock /tmp/gstore-bench.lock`. `gstore-bench run --profile reference
--scale {8,38} --drop-caches --verify-data` (5 warm reps in each of 5 fresh
query processes; a query's figure is the minimum of the 5 per-process
medians, as in the reference), joined with `merge` into
`bench/results/phase6-r1-lubm8.json` and `phase6-r1-lubm38.json`, compared
with `bench/results/pre-phase6-lubm{8,38}.json` (`gstore-bench compare
… --markdown`: **PASS, 281 gates** at both scales; the Markdown is
`bench/results/phase6-r1-lubm{8,38}-compare.md`). Wall time: univ 8
3.5 min (snapshot + disk) + 1.9 min (RocksDB), univ 38 15.0 min + 8.0 min.
Data hashes `ed44da2e7b3bbb98` (univ 8) and `fcd6493725f3c66a` (univ 38),
identical to the reference. Ratios are branch / pre. (The univ 8 file
records load average 2.82 at its start: the decay of the release builds
that had just finished; a second univ 8 snapshot run at load 1.00 matched
it within 0.3% in geomean.)

**univ 8** (`phase6-r1-lubm8.json`):

| backend | build s | gbuild peak MiB | database bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 15.32 → 5.33 (0.35×) | 154 → 138 | 85,480,180 → 85,480,180 | 336 / 292 → 300 / 243 (0.83×) | 125.8 → 128.1 | 152.5 → 154.0 | 7.91 → 3.06 (0.39×) |
| disk | 263.74 → 90.14 (0.34×) | 165 → 123 | 175,005,702 → 175,005,701 | 2240 / 1447 → 1124 / 318 (0.22×) | 129.7 → 114.5 | 167.5 → 145.2 | 18.99 → 5.63 (0.30×) |
| rocksdb | 53.93 → 31.49 (0.58×) | 286 → 287 | 41,162,410 → 41,135,741 | 9959 / 9989 → 306 / 292 (0.03×) | 145.8 → 90.2 | 184.9 → 138.8 | 43.27 → 31.10 (0.72×) |

| query | rows | snapshot ms | disk ms | rocksdb ms |
|---|---:|---:|---:|---:|
| Q1 | 3 | 0.307 → 0.041 (0.14×) | 0.420 → 0.361 (0.86×) | 9.47 → 8.61 (0.91×) |
| Q2 | 25 | 8.04 → 5.89 (0.73×) | 38.4 → 8.60 (0.22×) | 87.1 → 33.5 (0.38×) |
| Q3 | 10 | 0.435 → 0.095 (0.22×) | 0.479 → 0.426 (0.89×) | 29.3 → 28.1 (0.96×) |
| Q4 | 9 | 9.50 → 0.061 (0.01×) | 9.73 → 0.175 (0.02×) | 123.3 → 57.5 (0.47×) |
| Q5 | 576 | 37.3 → 23.1 (0.62×) | 115.5 → 31.6 (0.27×) | 69.9 → 49.6 (0.71×) |
| Q6 | 80,056 | 31.0 → 19.9 (0.64×) | 97.3 → 28.4 (0.29×) | 68.2 → 45.9 (0.67×) |
| Q7 | 23 | 54.0 → 30.7 (0.57×) | 216.5 → 49.7 (0.23×) | 131.8 → 103.4 (0.78×) |
| Q8 | 10,778 | 51.0 → 32.0 (0.63×) | 164.9 → 47.4 (0.29×) | 162.1 → 126.0 (0.78×) |
| Q9 | 1,081 | 70.4 → 42.7 (0.61×) | 266.3 → 65.4 (0.25×) | 241.4 → 212.5 (0.88×) |
| Q10 | 3 | 36.1 → 22.1 (0.61×) | 114.0 → 33.6 (0.29×) | 69.6 → 50.9 (0.73×) |
| Q11 | 325 | 0.390 → 0.240 (0.62×) | 0.542 → 0.328 (0.60×) | 2.71 → 2.30 (0.85×) |
| Q12 | 21 | 0.156 → 0.137 (0.88×) | 0.155 → 0.126 (0.81×) | 1.52 → 1.22 (0.80×) |
| Q13 | 85,670 | 35.2 → 22.2 (0.63×) | 172.1 → 37.9 (0.22×) | 76.8 → 50.3 (0.66×) |
| Q14 | 60,576 | 21.1 → 14.3 (0.68×) | 60.2 → 19.1 (0.32×) | 50.0 → 37.8 (0.76×) |

**univ 38** (`phase6-r1-lubm38.json`):

| backend | build s | gbuild peak MiB | database bytes | open first (cold) / warm ms | RSS after open MiB | query HWM MiB | Q1–Q14 geomean ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| snapshot | 76.82 → 28.92 (0.38×) | 741 → 670 | 430,848,180 → 430,848,180 | 1588 / 1465 → 1591 / 1561 (1.07×) | 596.4 → 608.9 | 710.3 → 708.3 | 46.65 → 16.79 (0.36×) |
| disk | 1627.59 → 575.88 (0.35×) | 738 → 527 | 889,778,182 → 889,778,181 | 12387 / 7673 → 5712 / 2092 (0.27×) | 544.0 → 470.6 | 740.6 → 580.5 | 146.92 → 38.65 (0.26×) |
| rocksdb | 277.76 → 175.93 (0.63×) | 634 → 628 | 206,253,032 → 206,222,953 | 60811 / 59389 → 1224 / 1187 (0.02×) | 522.0 → 315.1 | 663.3 → 508.1 | 311.50 → 232.27 (0.75×) |

| query | rows | snapshot ms | disk ms | rocksdb ms |
|---|---:|---:|---:|---:|
| Q1 | 3 | 1.30 → 0.143 (0.11×) | 39.1 → 4.13 (0.11×) | 60.7 → 50.4 (0.83×) |
| Q2 | 115 | 50.8 → 39.2 (0.77×) | 189.7 → 49.5 (0.26×) | 729.5 → 1030.6 (**1.41×**) |
| Q3 | 10 | 2.19 → 0.492 (0.22×) | 39.2 → 4.85 (0.12×) | 195.5 → 174.6 (0.89×) |
| Q4 | 9 | 53.6 → 0.099 (0.002×) | 75.2 → 2.08 (0.03×) | 990.0 → 394.5 (0.40×) |
| Q5 | 576 | 333.2 → 197.8 (0.59×) | 621.3 → 284.0 (0.46×) | 537.7 → 392.0 (0.73×) |
| Q6 | 404,399 | 187.6 → 113.4 (0.60×) | 511.4 → 150.0 (0.29×) | 468.3 → 282.9 (0.60×) |
| Q7 | 23 | 403.0 → 255.6 (0.63×) | 1147.7 → 380.4 (0.33×) | 977.6 → 774.7 (0.79×) |
| Q8 | 10,778 | 363.5 → 225.0 (0.62×) | 871.8 → 317.1 (0.36×) | 904.9 → 668.8 (0.74×) |
| Q9 | 5,507 | 529.3 → 314.5 (0.59×) | 1450.6 → 445.1 (0.31×) | 2125.9 → 1895.5 (0.89×) |
| Q10 | 3 | 326.6 → 207.9 (0.64×) | 697.1 → 272.0 (0.39×) | 594.9 → 451.4 (0.76×) |
| Q11 | 325 | 1.09 → 1.12 (1.04×) | 1.49 → 0.949 (0.64×) | 16.8 → 14.2 (0.84×) |
| Q12 | 21 | 0.587 → 0.502 (0.86×) | 0.721 → 0.577 (0.80×) | 10.2 → 8.11 (0.80×) |
| Q13 | 432,512 | 223.9 → 134.6 (0.60×) | 906.1 → 221.4 (0.24×) | 511.9 → 305.5 (0.60×) |
| Q14 | 305,503 | 132.1 → 81.3 (0.61×) | 320.3 → 106.8 (0.33×) | 382.7 → 215.5 (0.56×) |

Update and concurrency phases (report-only), pre → branch:

| scale / backend | single INSERT median ms | chunk insert / delete ms | checkpoint ms | reopen ms | Q4 qps /sparql 1 client | Q4 qps /api 1 client |
|---|---:|---:|---:|---:|---:|---:|
| 8 / snapshot | 0.86 → 0.06 | 29 / 26 → 45 / 25 | 8436 → 304 | 187 → 239 | 81.0 → 5309.5 | 85.6 → 2176.9 |
| 8 / disk | 1.74 → 1.10 | 1679 / 1419 → 823 / 812 | 0 → 0 | 10006 → 679 | 84.9 → 2470.8 | 85.9 → 1472.9 |
| 8 / rocksdb | 1.54 → 0.22 | 172 / 107 → 126 / 101 | 22 → 27 | 10121 → 166 | 7.2 → 14.9 | 7.4 → 15.2 |
| 38 / snapshot | 5.04 → 0.02 | 49 / 45 → 61 / 35 | 46004 → 1309 | 1122 → 1198 | 17.1 → 4247.3 | 17.8 → 2008.8 |
| 38 / disk | 2.33 → 1.19 | 4830 / 5024 → 1057 / 1290 | 0 → 0 | 51873 → 3400 | 12.6 → 366.5 | 11.6 → 324.0 |
| 38 / rocksdb | 4.35 → 0.26 | 186 / 149 → 188 / 124 | 39 → 36 | 55702 → 903 | 1.2 → 2.6 | 1.2 → 2.1 |

**Same-session memory build A/B** (base `90b92d6` `gbuild` vs branch
`gbuild`, snapshot layout, Rust data, alternated twice): univ 8 14.51 /
13.93 s → 5.15 / 6.05 s (**0.39×**), univ 38 71.47 / 72.12 s → 28.46 /
28.33 s (**0.40×**). `store.bin`, `vstree.bin` and `named.bin` are
byte-identical base vs branch at both scales, and `meta.bin` too when both
builds use the same database path (it records the path); `dict.bin` has the
same size and different bytes (its `HashMap` order, as noted below).

**Q4 scaling, dedicated run.** Q4 is sub-millisecond, so its univ 38 /
univ 8 ratio was also measured with a local helper (`Database::open`, cache
off, 2000 timed runs per query, 5 fresh processes per scale, alternating
univ 8 and univ 38, release build; not committed): median of the 5
per-process medians Q4 0.0589 → 0.0974 ms = **1.65×** (minima 0.0525 →
0.0860 = 1.64×). From the reference runs: minimum of the process medians
0.061 → 0.099 ms = 1.61× (1.49× against a second univ 8 run's 0.066 ms);
median of the per-process medians 0.0705 (10 univ 8 processes) → 0.139 ms
= 1.97×. The other small queries scale with the data (dedicated run,
median ms, univ 8 → univ 38): Q1 0.0424 → 0.1736 (4.1×), Q3 0.120 →
0.602 (5.0×), Q11 0.250 → 1.089 (4.4×), Q12 0.125 → 0.482 (3.9×) — as on
the base (pre: 4.2×, 5.0×, 2.8×, 3.8×); see Requests (G2).

**RocksDB with and without the VS-tree at open** (univ 38, one database,
2 fresh processes each way, 3 warm runs, medians ms; `GSTORE_VSTREE_ON_OPEN=1`
rebuilds the tree at open, which now adds about 12 s at univ 38): Q2
1138 / 1183 without the tree, 771 / 910 with it (pre: 729 minimum, 903
median of the process medians). Most other queries are faster without the
tree (Q4 401 / 414 vs 504 / 434, Q6 311 / 401 vs 373 / 477, Q7 701 / 781 vs
851 / 913, Q13 390 / 371 vs 463 / 482, Q14 249 / 284 vs 312 / 321) and none
is more than 8% slower (Q5 520 / 529 vs 481 / 510). Q2's 1.41× on RocksDB
is therefore the cost of opening without a VS-tree, which this package
does by design (plan §6 R1-CORE B); rebuilding at open would break the
≤ 10 s open target (about 1.2 + 12 s).

**Repeatability on the branch:** a second univ 8 snapshot run
(`--backends snapshot --no-update --no-conc`): Q1–Q14 geomean ratio 1.003
(1.009 over the queries of ≥ 1 ms), database bytes identical.

**Acceptance (R1-CORE), quiet:**

- Memory build ≤ 0.5 × base: **met** — univ 8 0.35× (gstore-bench) /
  0.39× (same session); univ 38 0.38× / 0.40×.
- RocksDB open ≤ 2 s at univ 8: **met** — 306 ms cold, 292 ms warm (pre
  9959 / 9989 ms). ≤ 10 s at univ 38: **met** — 1224 ms cold, 1187 ms warm
  (pre 60811 / 59389 ms).
- Memory Q4 ≤ 25 ms at univ 38: **met** — 0.099 ms (pre 53.6 ms).
  Q4(38)/Q4(8) ≤ 2.0: **met** — 1.61× (reference statistic), 1.65×
  (dedicated, median of 5 × 2000 runs); the median of the reference runs'
  per-process medians gives 1.97×, so the margin is small on that measure.
- Q1–Q14 warm geomean ≤ 0.95 × base at univ 38: **met** — snapshot 0.36×
  (univ 8 0.39×); disk 0.26×, RocksDB 0.75×. No query > 1.05 × base:
  **met on the memory layout** (worst Q11 1.04×; its median of the process
  medians 1.136 vs 1.265 ms = 0.90×) and on disk (worst Q12 0.80×); **on
  RocksDB Q2 is 1.41×** at univ 38 (0.38× at univ 8), the cost of opening
  without the VS-tree, see above and Requests.
- Answer counts identical: **met** — rows and row hashes of all 14 queries
  equal to the reference on all three layouts at both scales, in every
  process and run; update round trips ok.
- Single-client latency within ±5% (Sync costs nothing): **met** — no
  snapshot query is slower than pre by more than 4% (Q11 at univ 38; all
  others ≤ 0.88×); one-client `/sparql` Q1 mean latency 0.50 → 0.11 ms
  (univ 8) and 1.74 → 0.26 ms (univ 38).
- Full suite and RocksDB suite: **met** at `05be7e3` — `cargo test
  --workspace` 2077 passed, 0 failed, 1 ignored (94 test binaries, 93 s
  wall with a warm debug cache); `cargo test -p gstore --features rocksdb`
  (own target directory) 1897 passed, 0 failed, 1 ignored (469 s wall
  including the librocksdb build).

Other observations (no R1 acceptance item): the univ 38 snapshot warm open
is 1.07× pre (1465 → 1561 ms; cold first open 1588 → 1591 ms) and its RSS
after open 596.4 → 608.9 MiB (+12.5 MiB; most likely the VS-tree locator
and parent links, which the implementer measured as +1.7 MiB at univ 8; G2
asks ≤ 0.9× pre); at univ 8 the open is 0.83×. The snapshot
chunk insert is 29 → 45 ms (univ 8) and 49 → 61 ms (univ 38), down from
the 647 ms the R1 review measured on the merged tree but still above pre
(single-shot, report-only).

### Implementer run

**Implementer run, noisy (other agents compiling; load average 4–6 on
4 vCPUs).** univ 8 only (`gen_lubm.py --univ 8`, 1 052 262 triples,
171 836 entities), base `90b92d6` vs this branch, measured in the same
session, interleaved base/branch, every run under `flock
/tmp/gstore-bench.lock`, with the prototype `lubm_bench` example (release,
`--features rocksdb`; not committed). Queries: `testdata/lubm/lubm_q1..14.rq`,
query cache off, warm median of 5 runs, the minimum over 3 interleaved
rounds. univ 38 and the pre-Phase-6 JSON are left to the quiet measure stage
(gstore-bench).

| Metric (univ 8) | base | branch | ratio |
|---|---|---|---|
| Memory build + save (`build_from_files` + `save`), 3 runs | 14.50 / 14.50 / 14.79 s | 7.08 / 6.60 / 6.09 s | 0.42–0.49 (median 0.455) |
| of which parse (branch, `build-mem-split`) | — | 4.21 s of 6.04 s | VS-tree rebuild 0.08 s |
| RSS after build / build peak | 166 / 166 MiB | 128 / 149 MiB | 0.77 |
| Snapshot open (`Database::open`) | 0.24–0.34 s | 0.25–0.32 s | ≈ 1 |
| RSS after snapshot open | 134.5 MiB | 136.2 MiB | +1.7 MiB (locator, parent links) |
| RocksDB open (2 runs) | 11.14 / 10.76 s | 0.28 / 0.35 s | 0.03 |
| RSS after RocksDB open | 148 MiB | 91 MiB | 0.61 |
| Snapshot bytes: `store.bin`, `vstree.bin`, `named.bin`, `meta.bin` | | | identical, base vs branch |
| `dict.bin` | | | same size; bytes differ between any two builds, base vs base too (a `HashMap` serialized in random order) |

Q1–Q14 on the snapshot layout (warm, ms; answer counts identical):

| Query | base | branch | ratio |
|---|---|---|---|
| Q1 | 0.348 | 0.045 | 0.13 |
| Q2 | 8.869 | 7.391 | 0.83 |
| Q3 | 0.502 | 0.096 | 0.19 |
| Q4 | 10.224 | 0.065 | 0.006 |
| Q5 | 40.182 | 27.969 | 0.70 |
| Q6 | 31.446 | 23.640 | 0.75 |
| Q7 | 56.088 | 36.284 | 0.65 |
| Q8 | 54.853 | 38.738 | 0.71 |
| Q9 | 71.855 | 51.872 | 0.72 |
| Q10 | 41.018 | 27.289 | 0.67 |
| Q11 | 0.348 | 0.235 | 0.68 |
| Q12 | 0.153 | 0.137 | 0.90 |
| Q13 | 36.885 | 25.737 | 0.70 |
| Q14 | 22.424 | 17.327 | 0.77 |
| **geomean** | 8.349 | 3.486 | **0.42** |

Answer counts (both): Q1 8, Q2 28, Q3 9, Q4 10, Q5 481, Q6 82111, Q7 34,
Q8 11137, Q9 1050, Q10 8, Q11 333, Q12 21, Q13 87795, Q14 62096.

Q4 at univ 1 (`testdata/lubm/lubm.nt`, 100K triples), branch: 0.096 ms;
univ 8: 0.065 ms — it no longer grows with the data (base: 0.93 ms → 10.2
ms). With the VS-tree probes alone Q4 only went 10.2 → 7.9 ms at univ 8:
its linear cost was (a) `pred_distinct_obj` sorting every object of
`name`, `emailAddress` and `telephone` per query (the base's whole-run
profile spends 14% in that sort) and (b), found with callgrind at univ 1,
the bushy plan scanning those three predicates whole as leaves filtered by
the ten candidate professors. Both are fixed without changing the plan.

RocksDB Q1–Q14 (one warm run each, informational, measured with an
earlier branch build — before the statistics and candidate-lookup commits;
the RocksDB database now opens without a VS-tree, so it evaluates unpruned
until `rebuild_index()`): base geomean 54.9 ms, branch 46.4 ms (0.84); same
answer counts; noisy single runs (Q4 127 → 217 ms, Q8 280 → 161 ms).

Acceptance (plan §6 R1-CORE), as far as univ 8 can tell:

- Memory build ≤ 0.5 × base: **met at univ 8** (0.42–0.49, median 0.455,
  noisy); univ 38 left to the measure stage (the removed VS-tree sort was
  65–68% of the univ 38 build, so the ratio should be lower there).
- RocksDB open ≤ 2 s at univ 8: **met** (0.28–0.35 s vs 10.8–11.1 s);
  ≤ 10 s at univ 38 left to the measure stage.
- Memory Q4 ≤ 25 ms at univ 38 and Q4(38)/Q4(8) ≤ 2.0: left to the measure
  stage; at univ 1 → 8 Q4 went 0.096 → 0.065 ms (flat).
- Q1–Q14 geomean ≤ 0.95 × base, no query > 1.05 × base: **met at univ 8**
  (0.42; worst Q12 0.90); univ 38 left to the measure stage.
- Answer counts identical: **met** (univ 8, snapshot and RocksDB; univ 1 in
  the test suite).
- Single-client latency within ±5% ("Sync costs nothing"): **met** — every
  query is faster, none slower.
- Full suite and RocksDB suite: **met** — `cargo build --workspace
  --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` (1988 passed, 0 failed),
  `cargo clippy -p gstore --all-targets --features rocksdb -- -D warnings`
  and `cargo test -p gstore --features rocksdb` (1852 passed, 0 failed),
  all clean. No feature-gated code other than `rocksdb` was touched.

## Requests / Not yet

Requests (changes outside R1-CORE's files, for the integrator or a later
round):

1. **Export in a read scope (C2).** `Database::export_ntriples` lives in
   `src/db/admin.rs`, which is not in R1-CORE's list: wrap its store walk
   in a `ReadScope` and fail on a recorded error, like `read_as` does. No
   backend reports read errors yet, so nothing changes today. *Done at the
   R1 merge (c76b9cc): `zipio::write_export` runs in a scope, and
   `export_ntriples` delegates to it.*
2. **Snapshot queries in a read scope (C2).** `ConcurrentDb` snapshot
   evaluation (`src/concurrent.rs`, R3-SRV / R4-SNAP) should run inside a
   `ReadScope` and neither answer nor cache when an error was recorded.
3. **DiskStore count overrides (C1).** R1-KV adds inherent `count_*`,
   `distinct_p_of_s/o` and `has_*` on `DiskStore`; its `TripleSource` impl
   (R1-KV's `store.rs`) should override the C1 methods with them once both
   packages are merged — each must return exactly what the default returns
   (the ordering contract makes `distinct_p_of_*` a run count). *Done at
   the R1 merge (f953d5b).*
4. **Disk VS-tree build.** `build_disk_from_sources` (`persist.rs`, R2-DICT
   in Round 2) still signs a `DiskStore` through the generic
   `vstree_entries` (two lookups and copies per entity): R1-KV's
   `for_each_entity_adjacency` can feed it the way
   `TripleStore::for_each_entity_adjacency` now feeds the in-memory build.
   *Done at the R1 merge (12bd4d6).*
5. **RocksDB counts (C1).** `RocksStore` uses the C1 defaults, which
   materialize the list they count (the same cost as before). R3-ROCKS can
   override them with prefix counts. Note for G3: a reopened RocksDB
   database now evaluates without a VS-tree.
6. **Servers on the shared read path (C8).** `Database: Send + Sync` and
   `read_as` / `query_read` / `Statement` are ready for R3-SRV's
   `RwLock<Database>` (parse once, route reads to `read_as` under the read
   lock and updates to `execute_as` under the write lock).
7. **`GSTORE_VSTREE_ON_OPEN`.** An environment switch until R3-STREAM's
   `OpenOptions::vstree_on_open` replaces it (C7).
8. **Streaming executor.** `src/query/topk.rs` (not in R1-CORE's list)
   still enumerates `candidate_triples` and filters each triple with
   `cand_ok`; switching it to `candidate_triples_in` with the slot sets
   looked up once would give the streaming / Top-K paths the same win
   (solutions and order are unchanged by construction). *Done at the R1
   merge (67a67d9).*

Not yet / notes:

- univ 38 numbers, the RocksDB open at univ 38 and the Q4 scaling ratio are
  for the measure stage.
- The memory build is now dominated by parsing (4.2 s of 6.0 s at univ 8,
  `src/parser`, not in R1-CORE's list).
- `dict.bin` bytes are not reproducible between two builds of the same data
  (base included): the dictionary's `HashMap` serializes in random order.
  A golden "bytes unchanged" check for `dict.bin` has to compare decoded
  contents; `store.bin`, `named.bin`, `vstree.bin` and `meta.bin` compare
  byte for byte (they did, base vs branch).
- `vs_shrink_debt` is only counted and reported; nothing rebuilds the tree
  automatically when it grows (heavy-churn databases can call
  `rebuild_index()`; R4-RECLAIM may want a threshold).
- The default `has_*` methods detect a read error only through
  `note_read_error` (they enter a nested scope and forward the error);
  backends that override them must return `true` on error themselves.

From the quiet post-merge stage (`05be7e3`; numbers under Measured numbers):

- **RocksDB Q2 (R3-ROCKS / R3-STREAM, G3):** a RocksDB database opened
  without a VS-tree runs univ 38 Q2 at 1.41× pre (1031 vs 730 ms); with
  `GSTORE_VSTREE_ON_OPEN=1` it is back at pre (771–910 ms), but the rebuild
  adds about 12 s to the open. `OpenOptions::vstree_on_open`, a persisted or
  lazily built tree, or a cheaper candidate source for Q2's three type
  patterns would close it. The RocksDB Q1–Q14 geomean is already 0.75× pre
  (G3 asks ≤ 0.8×), so the margin is small.
- **G2 small-query scaling (later rounds, R4-CLOSURE reports):** Q4 now
  scales 1.6–1.7× from univ 8 to univ 38, but Q1 4.1×, Q3 5.0×, Q11 4.4× and
  Q12 3.9× (G2 asks ≤ 2.0 for Q1/Q3/Q4/Q11/Q12); the base scaled about the
  same (4.2×, 5.0×, 2.8×, 3.8×): these plans touch work that grows with the
  data, which R1 did not change. Q11's ratio rose only because its univ 8
  time fell (0.39 → 0.24 ms) while univ 38 stayed (1.09 → 1.12 ms; 1.04× by
  the reference statistic, 0.90× by the median of the process medians) — the
  one snapshot query that did not get faster at univ 38.
- **G2 open and memory (R4-SNAP / R4-CLOSURE):** the univ 38 snapshot RSS
  after open is 1.02× pre (608.9 vs 596.4 MiB; G2 asks ≤ 0.9×) and the warm
  open 1.07× (1561 vs 1465 ms).
