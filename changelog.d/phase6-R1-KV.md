# Phase 6 R1-KV — kvstore read path, per-operation atomicity, format registry

## Changelog

- **kvstore format registry** (`src/kvstore/format.rs`, new): the single
  owner of the `kvstore.kv` / `vstree.kv` header — `MAGIC_V1` (`GSTOREKV`) and
  `MAGIC_V2` (`GSTOREK2`), `NROOTS` 24 → 32, every root slot (0–20 unchanged;
  21 `FEATURES`; 22 `FREE_IDS` and 23–25 `FREE_COUNT_E/L/P` reserved for
  R4-RECLAIM; 26–31 reserved), the feature bits (0 `FREE_IDS`, 1
  `IVARRAY_WIDE` and 2 `LONG_KEYS` reserved) and their rules. Every open
  checks the header: `FEATURES == 0` ⇔ V1, unknown (unimplemented) bits are
  refused, V2 without bits is refused. `Pager::features()`,
  `Pager::enable_feature(bit)` (no bit is implemented yet, so it refuses every
  bit until R4) and `Pager::require_supported()`. **No format change**: a file
  written by this build is V1 and its header bytes past `16 + 24 * 8` are zero;
  a univ 8 `gbuild --disk` produces a `kvstore.kv` and `vstree.kv` byte-identical
  to the pre-Phase-6 build.
- **Pager**: cached pages are shared `Arc<[u8; 4096]>` buffers —
  `read_page_shared` returns a handle without copying, `write_page` copies on
  write (a reader keeps the bytes it was given), `read_page` still returns a
  copy. CLOCK replaces the `O(capacity)` LRU scan in both eviction paths; a
  dirty page leaves the cache only through a committed flush. Misses use
  `pread` on Unix. The page map uses a multiplicative hash. New `dirty_pages()`,
  `capacity()`, `cached_pages()`.
- **Operation brackets** (`Pager::begin_op` / `end_op` / `abort_op`, RAII
  `PagerOp`): while an operation is open the cache never commits to make room
  (it grows past capacity instead) and `flush` is refused; `end_op` at depth 0
  restores the capacity, committing first if the dirty pages alone exceed it.
  Brackets nest. A bracket dropped without `commit` (an error or a panic) rolls
  the operation back: its pages return to their previous contents and the
  header (page count, free list, roots) to its state at `begin_op`.
- **B+ tree reads** go through a zero-copy, bounds-checked `NodeView` (a
  malformed page is an `Err`, never a panic; cyclic child pointers and leaf
  chains are detected). Nodes whose entries all have one size are
  binary-searched; trees declared with `BTree::with_fixed_shape` (SPO, POS,
  OSP: 12-byte keys; GSPO: 16-byte keys; empty values) classify each node in
  `O(1)` and re-check every probed or visited entry. A prefix run is bounded by
  two binary searches per leaf, so `scan_prefix_with`, `count_prefix` and
  `prefix_exists` compare no key inside the run. New `get_with`, `contains`,
  `cursor_ge` + `LeafCursor`, `scan_prefix_with(.., FnMut -> ControlFlow)`,
  `first_with_prefix`. `insert` enforces a declared shape. Writes keep `Node`.
- **IvArray**: `get_with` (no allocation for an inline value) and a sequential
  `for_each_range` (one directory page per 32 ids, page-table pages read once
  per span, unallocated spans skipped, overflow chains followed); the layout
  constants and `CAPACITY` are `pub(crate)`; a corrupt inline length is an
  error instead of a panic.
- **DiskStore reads**: inherent `try_*` methods mirroring every
  `TripleSource` method, returning `io::Result`, decoded straight out of the
  cached pages **in the C1 contract order** (`po_by_s` → `(p,o)`, `ps_by_o` →
  `(p,s)`, `so_by_p` → `(s,o)`, all sorted; `subs_by_p` / `objs_by_p` /
  `subject_keys` / `object_keys` ascending and deduplicated, the key lists by
  one streaming pass; `iter_all` in `(p,s,o)` order by a bucketing of the SPO
  scan; `try_iter_spo` for SPO order). The `TripleSource` impl delegates to
  them (a read error is still an empty answer there). Exact allocation-free
  counts `count_po_by_s` / `count_o_by_sp` / `count_p_by_so` / `count_ps_by_o`
  / `count_s_by_po` (plus `try_` forms) and `count_so_by_p` from the statistics;
  `distinct_p_of_s/o`; `has_subject/object/predicate` (true on a read error).
  A per-predicate statistics mirror is loaded at open in `O(P)` and maintained
  by every write, so `pred_card` & co. read no page; `distinct_subjects` is
  exact (no `.max(1)`), `num_predicates` counts non-empty predicates only.
  `to_memory` / `dictionary()` load the dictionary with `for_each_range` and
  the triples with one zero-copy SPO scan.
- **DiskStore writes**: every logical write — interning a term, inserting or
  deleting a triple or a quad, and `insert_keys` / `delete_keys` /
  `insert_quad_keys` / `delete_quad_keys` with their terms — is **one** pager
  operation under the pager write lock: never committed in part (fixes the torn
  insert across SPO/POS/OSP and the duplicate-id hazard of a term committed
  without its count root), and rolled back entirely, counters included, if it
  fails midway. `insert_ids`, `delete_ids`, `insert_quad_ids`,
  `delete_quad_ids` and `intern_*` are `pub(crate)` by id.
- **DiskStore API additions**: `shared_pager()`,
  `from_shared_pager(Arc<RwLock<Pager>>)` (a read-only view; writes and commits
  refused; `refresh()` re-reads counters and statistics), `commit(&self)`,
  `for_each_entity_adjacency(f)` (one merged streaming SPO+OSP pass for VS-tree
  rebuilds, in `vstree_entries` order), `nonempty_predicate_count()`,
  `pub(crate)` `key3` / `key4` / `PRED_STAT_LEN`.
- `examples/kv_bench.rs`: the R1-KV micro-benchmarks (CLI in its header).
- kvstore unit tests use per-process temp files (no clashes between
  concurrent test runs).

## GAP_PLAN status line

Phase 6 R1-KV (kvstore read path, per-operation atomicity, format registry):
done — no on-disk format change; zero-copy, contract-ordered disk reads;
atomic logical writes; the §3.1 registry in `src/kvstore/format.rs`.

## Docs notes

- docs/PHASE6_PLAN.md §3.1 is implemented as written; `SUPPORTED_FEATURES`
  is empty until R4-RECLAIM adds `FEATURE_FREE_IDS` (then `enable_feature`
  accepts it). A V1 header written by this build is byte-identical to a
  pre-Phase-6 one.
- docs/CPP_PARITY.md line "分页文件+LRU页缓存+B+树" and
  docs/REFACTOR_BACKLOG.md item A ("写回式LRU页缓存 … 16个root槽") are out of
  date: the page cache is CLOCK over shared copy-on-write pages, the header has
  32 root slots plus the feature bitmask (`src/kvstore/format.rs`), logical
  writes are atomic pager operations (no-steal: an oversize operation lets the
  cache overshoot its capacity until it ends, then commits), and `DiskStore`
  reads come in the same order as `TripleStore`'s.
- D12 (crash atomicity of disk *transactions* larger than the cache) is
  unchanged: each logical write is atomic, a transaction of many writes is
  durable but not atomic.

## Measured numbers

**Implementer run, noisy (other agents compiling; load average 3.5–4.5 on
4 vCPUs).** Univ 8 only (LUBM-like, `gen_lubm.py --univ 8`: 1,088,422
statements, 1,052,262 unique triples, 171,836 entities, 88,738 literals, 17
predicates). One disk database built with the **base** `gbuild --disk`
(90b92d6) and read by both builds; base and branch `kv_bench` ran back to back
in one `flock /tmp/gstore-bench.lock` session (`kv_bench <db> --queries
testdata/lubm`, defaults: 5 reps, 3 query reps, 20,000 lookups, seed 42; the
base side is the same harness with the base read API in its `api` module).
Times are warm medians.

| Measurement | base 90b92d6 | branch | ratio | acceptance |
|---|---|---|---|---|
| hot random `po_by_s` (ns/lookup) | 23,573 | 1,072 | 22.0× faster | ≥ 3× — met |
| hot random `o_by_sp` (ns/lookup) | 26,188 | 946 | 27.7× faster | ≥ 3× — met |
| full SPO scan to `Vec<IdTriple>` (ms) | 359.0 | 13.7 | 26.2× faster | ≥ 3× — met |
| (branch only) `try_iter_all` in `(p,s,o)` order (ms) | — | 35.0 | | |
| warm `to_memory` open: `DiskStore::open` + `to_memory` + `named_to_memory` (ms) | 1,377.1 | 674.1 | 0.49× | ≤ 0.7× — met |
| warm `Database::load_disk` (ms) | 1,468.7 | 659.7 | 0.45× | |
| `DiskStore::query` Q1–Q14 warm geomean (ms) | 61.48 | 14.61 | 4.2× faster | ≥ 2× — met |
| Q1–Q14 rows and row hash vs the memory answer | all equal | all equal | | identical — met |
| incremental `gbuild --disk` univ 8 (s, wall) | 255.4 | 109.9 | 0.43× | no slower — met |
| `kvstore.kv` / `vstree.kv` bytes | 156,381,184 / 22,716,416 | byte-identical (`cmp`) | | no format change |

Per-query warm ms, base → branch: Q1 5.53 → 0.51, Q2 58.15 → 4.34, Q3 17.23
→ 1.56, Q4 76.66 → 25.01, Q5 48.92 → 34.87, Q6 59.28 → 49.64, Q7 141.08 →
114.98, Q8 612.82 → 163.28, Q9 1607.35 → 140.53, Q10 95.24 → 64.48, Q11 8.99
→ 0.62, Q12 3.56 → 0.16, Q13 164.54 → 121.84, Q14 128.50 → 66.64. An earlier
same-day pair gave the same picture (lookups 20.2 → 1.1 µs, scan 239 → 15 ms,
`to_memory` 1588 → 623 ms, geomean 54.1 → 13.9 ms).

Left to the quiet measure stage: the same `kv_bench` A/B on an idle machine
(rebuild the univ 8 database with the base `gbuild --disk`; the implementer
database was deleted after measuring), and every univ 38 number. The full
suite (`cargo test --workspace`) and `cargo clippy --workspace --all-targets
-- -D warnings` pass. RocksDB was not built: no RocksDB code reaches the
kvstore (`src/backend/` does not use it); no feature-gated code was touched.

## Requests / Not yet

- **R1-CORE / integrator (after the R1 merge):** override the C1 provided
  methods in `impl TripleSource for DiskStore` (`src/kvstore/store.rs`) by
  delegating to the inherent `count_po_by_s` … `count_so_by_p`,
  `distinct_p_of_s/o` and `has_subject/object/predicate` (exact, no
  materialization). Not done here because the trait methods do not exist on
  this branch's base.
- **R2-DISKLIB:** `DiskTriples` should call the `try_*` reads and report errors
  through `note_read_error`; the legacy `TripleSource` impl still swallows them.
  `from_shared_pager` + `refresh()` give a read-only view over the writer's
  pager; `commit(&self)` commits completed operations.
- **R2-BULK / R1-CORE:** `vstree_entries` over a `DiskStore` can use
  `for_each_entity_adjacency` (one streaming pass instead of
  `subject_keys` + `object_keys` + per-entity `po_by_s` / `ps_by_o`).
- **R4-RECLAIM:** add `FEATURE_FREE_IDS` to `format::SUPPORTED_FEATURES`
  together with the structures behind it; `src/signature/disk_vstree.rs`
  should then assert `pager.features() == 0` for `vstree.kv` (today any feature
  is refused by every open, so the rule holds automatically).
- **R2-DICT:** `DiskDict` (`src/kvstore/disk_dict.rs`, compile-only here) can
  use `BTree::get_with` / `IvArray::get_with` to avoid the per-lookup copies;
  the `DiskStore::query` time of the large-result queries (Q6, Q13, Q14) is
  now dominated by id→string materialization there.
- Not yet: the `*2id` dictionary trees still walk their variable-size nodes
  linearly (no offset table); `sp2o_vlist` is not shape-declared (its values
  vary).
