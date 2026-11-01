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
   backend reports read errors yet, so nothing changes today.
2. **Snapshot queries in a read scope (C2).** `ConcurrentDb` snapshot
   evaluation (`src/concurrent.rs`, R3-SRV / R4-SNAP) should run inside a
   `ReadScope` and neither answer nor cache when an error was recorded.
3. **DiskStore count overrides (C1).** R1-KV adds inherent `count_*`,
   `distinct_p_of_s/o` and `has_*` on `DiskStore`; its `TripleSource` impl
   (R1-KV's `store.rs`) should override the C1 methods with them once both
   packages are merged — each must return exactly what the default returns
   (the ordering contract makes `distinct_p_of_*` a run count).
4. **Disk VS-tree build.** `build_disk_from_sources` (`persist.rs`, R2-DICT
   in Round 2) still signs a `DiskStore` through the generic
   `vstree_entries` (two lookups and copies per entity): R1-KV's
   `for_each_entity_adjacency` can feed it the way
   `TripleStore::for_each_entity_adjacency` now feeds the in-memory build.
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
   (solutions and order are unchanged by construction).

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
