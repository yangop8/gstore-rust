# R2-DISKLIB — exact transaction overlay + DiskTriples (the `Backend::Disk` payload), library level

Package R2-DISKLIB of `docs/PHASE6_PLAN.md` §6 (Round 2), contract C5.
Base: `aaea9f8`. Library only: nothing is wired into `Database` (R3-STREAM
does that), so no user-visible behaviour changes in this package.

## Changelog

### Phase 6 R2-DISKLIB: transaction overlay, `DiskTriples`, snapshot views

- **Exact transaction overlay** (`gstore::store::overlay`, re-exported as
  `gstore::store::{Overlay, OverlayView, OverlayChanges}`). `Overlay` holds a
  transaction's added and removed triples apart from its base, keeping
  `added ∩ base = ∅`, `removed ⊆ base` and `added ∩ removed = ∅`, so
  `insert(base, t)` / `remove(base, t)` (and the fallible `try_insert` /
  `try_remove`, which probe the base only when the overlay cannot decide)
  return exactly the "changed" flag a materialized store would. `drain()`
  returns the net changes in `(s, p, o)` order. `OverlayView` implements the
  **whole** `TripleSource` trait, C1 methods included, over
  `(base − removed) ∪ added`: lists are sorted merges in the contract order,
  counts are exact arithmetic, `distinct_*` / `pred_distinct_*` /
  `num_predicates` / `distinct_p_of_*` probe only the keys the overlay
  touches (the global and per-predicate ones memoized until the next change),
  `iter_all` merges in `(p, s, o)` order, and `has_*` answer `true` on a read
  error. A one-hash Bloom filter (8 KiB per key kind) of the subjects,
  objects and predicates the overlay's changes concerned makes a read of an
  untouched key one bit test on top of the base read.
- **`DiskTriples`** (`gstore::kvstore::DiskTriples`, new
  `src/kvstore/disk_triples.rs`, contract C5): a `TripleSource +
  MutableStore` over a writable `DiskStore`, `Send + Sync`.
  - *Reads* use the fallible `DiskStore::try_*` methods; an error is
    reported with `note_read_error` (C2), so the statement running in the
    thread's `ReadScope` fails, and the read answers empty (`has_*`: `true`).
    The C1 counts, `distinct_p_of_*` and `has_*` use the store's exact
    prefix counts. The same reporting reader is public as
    `gstore::kvstore::DiskSource` (wrap any `&DiskStore` in it).
  - *Transactions*: `begin()`, then every triple write lands in an `Overlay`
    and **no triple reaches the pager**; reads see the transaction's writes
    (read-your-writes through `OverlayView`); `rollback()` never touches the
    pager; `commit()` applies the queued named-graph operations (in order),
    then the removals, then the additions, each one atomic pager operation;
    `named_op(insert, g, t)` queues or applies a quad write. Outside a
    transaction a write is applied at once (auto-commit). `insert_triple` /
    `remove_triple` / `insert_many` return `Result`; the `MutableStore`
    methods answer `false` on an error and keep it for `take_error()`.
  - *Writer gate*: before changing the pager a writer marks the gate
    draining, raises the cancel flag of every snapshot reader inside it,
    waits until none is left, applies its change and bumps the **epoch**
    (only when data changed: a no-op write cancels nobody and moves
    nothing). A write from inside one of the same store's snapshot reads is
    refused with an error instead of waiting for itself. A writer that waits
    more than 10 s for readers that ignore their cancel flag logs a warning.
  - *Snapshot views*: `read_view()` returns a `DiskReadView` (`Clone`,
    `Send + Sync`, holds the shared pager and gate, not the `DiskTriples`).
    `with_base(|&DiskStore, &Arc<AtomicBool>| ..) -> Result<R, ViewError>`
    registers as a reader, builds a fresh read-only
    `DiskStore::from_shared_pager` view and hands it the reader's cancel flag
    (give it to `Evaluator::with_cancel`); it answers `ViewError::Expired` if
    the gate is draining or the epoch moved (checked before and after the
    closure; a cancelled reader always gets `Expired`), `Closed` after the
    `DiskTriples` was dropped or closed, and `Io` if the store is broken. A
    view never sees an open transaction's writes; it does not retry.
  - *Durability and the broken state*: `commit_point(&self)` commits applied
    writes durably (one WAL-protected flush) when there are any. Any write or
    flush error breaks the store: later writes are refused, reads report a
    read error and views answer `Io`, until it is reopened; dropping a
    broken store flushes nothing. `Drop` drains the gate, marks the views
    closed and flushes applied writes (best effort); `close()` does the same
    and reports a flush error. An open transaction is discarded by both.
    `#[doc(hidden)] debug_fail_next_write()` / `debug_fail_write_after(n)`
    inject a write failure for tests.
  - Passthroughs: `vstree_stale()`, `mark_vstree_fresh()`, `disk_store()`
    (the committed state), `shared_pager()`, `epoch()`, `is_dirty()`,
    `broken()`, `in_transaction()`, `transaction()`.
- **`DiskStore`**: additive `try_has_subject`, `try_has_object`,
  `try_has_predicate` (the existence probes with the error returned) and
  `try_quad_exists(g, t)`.
- **`examples/disk_stream_probe.rs`** (new): LUBM Q1–Q14 evaluated over
  `DiskTriples`, over a fresh `DiskReadView` per run (`DiskSource` + the
  view's cancel flag) and over the materialized `TripleStore`, with one
  in-memory dictionary and the same `vstree.kv`; rows compared **in
  order**; warm latencies and their geometric means. An `overlay` mode times
  hot lookups on untouched keys through the store, an empty overlay, an
  overlay that changes other keys, and `DiskTriples` with and without an
  open transaction, and Q1–Q14 with a transaction open. CLI in the file
  header.
- **Tests**: `tests/dt_overlay.rs` (new) — a seeded model test of `Overlay`
  and of `DiskTriples` transactions against a materialized `TripleStore`
  (every `TripleSource` method in order, every count, distinct count and
  `has_*`, every changed flag; pager page count, dirty set and file bytes
  unchanged until commit; commit / rollback / reopen), named-graph
  operations in transactions, view expiry and close, the broken state
  (injected mid-commit and flush failures, reopen), drop flushing, read
  errors from a corrupted POS root reaching the reading statement's scope and
  never another thread's, LUBM Q1–Q14 over `DiskTriples` with and without an
  open transaction (rows and plans equal to the materialized store), and a
  threaded stress test: 8 snapshot readers running "queries" that loop until
  cancelled while a committer commits 12 times — every commit finishes within
  200 ms, readers only ever see whole committed states and converge on the
  last one. A one-minute soak variant with 10-second queries is `#[ignore]`
  (`cargo test -p gstore --test dt_overlay -- --ignored`). Unit tests in
  `src/store/overlay.rs` and `src/kvstore/disk_triples.rs`.

## GAP_PLAN status line

§8 row 6 (规模与性能): R2-DISKLIB done — exact transaction overlay
(`OverlayView`, all of `TripleSource`) and `DiskTriples` with reported reads,
no-pager-before-commit transactions, a drain-and-cancel writer gate with
epochs, `DiskReadView` snapshots and a broken state (C5); library only, wired
into `Database` by R3-STREAM. `disk_stream_probe` at univ 8: Stream answers
identical, warm Q1–Q14 geomean 1.24–1.26× the materialized path.

## Docs notes

- docs/STORAGE_BACKEND.md (Stream mode, for R4-CLOSURE together with
  R3-STREAM's notes):
  - the transaction overlay: invariants, exact changed flags, read-your-writes,
    "no triple reaches the pager before commit; dictionary terms may (orphan
    terms are harmless)", commit order (named ops, removals, additions);
  - the writer gate and epochs: drain + cancel, `Expired` / `Closed` / `Io`,
    "a snapshot reader sees its begin-time state or, after a retry, the
    committed one", the epoch moves only when data changed, a reader that
    never checks its cancel flag delays writers (warning after 10 s);
  - the broken state and its reopen rule; D12 still applies (a crash while a
    very large commit is applied can leave part of it on disk);
  - lock order inside `DiskTriples`: gate → pager (C8), the gate mutex is
    never held across a pager operation.
- docs/DESIGN.md (concurrency): `DiskReadView` is the disk counterpart of the
  copy-on-write memory snapshot; snapshot queries must wrap the store in
  `DiskSource` and pass the cancel flag to the evaluator.

## Measured numbers

**Implementer run, noisy (other agents compiling; load average 3.4–5.5 on 4
vCPUs during the runs).** Univ 8 only; univ 38 is left to the quiet measure
stage (commands below). Every heavy command ran under
`flock /tmp/gstore-bench.lock`. There is no base counterpart (DiskTriples is
new), so the comparison is the plan's: Stream (`DiskTriples`) against the
materialized path, in the same process, on the same database.

Database: `/tmp/gstore-base-aaea9f8/bin/gbuild --disk <dir>/lubm8
/tmp/gstore-bench-cache/lubm-like1-u8-s0.nt` (base binary, 92.6 s; 1,029,275
triples, 168,432 entities, 86,612 literals, 17 predicates; `kvstore.kv`
37,289 pages; `vstree.kv` fresh). Deleted after measuring.

`cargo build --release -p gstore --example disk_stream_probe`, then
`target/release/examples/disk_stream_probe <dir>/lubm8.db` (page cache = the
whole file, `vstree.kv` on both sides, one in-memory dictionary; warm =
median of the timed runs):

| query | rows | materialized ms | DiskTriples ms | ratio | view ms | rows in order |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 3 | 0.349 | 0.540 | 1.55 | 0.509 | equal |
| Q2 | 25 | 9.660 | 17.573 | 1.82 | 17.407 | equal |
| Q3 | 10 | 0.444 | 1.071 | 2.41 | 1.118 | equal |
| Q4 | 9 | 0.190 | 0.245 | 1.29 | 0.234 | equal |
| Q5 | 576 | 33.237 | 35.350 | 1.06 | 35.957 | equal |
| Q6 | 80,056 | 41.246 | 37.223 | 0.90 | 40.754 | equal |
| Q7 | 23 | 53.052 | 56.246 | 1.06 | 56.064 | equal |
| Q8 | 10,778 | 56.878 | 62.874 | 1.11 | 64.173 | equal |
| Q9 | 1,081 | 78.285 | 110.410 | 1.41 | 100.434 | equal |
| Q10 | 3 | 42.325 | 42.437 | 1.00 | 41.196 | equal |
| Q11 | 325 | 0.359 | 0.480 | 1.34 | 0.469 | equal |
| Q12 | 21 | 0.131 | 0.159 | 1.22 | 0.145 | equal |
| Q13 | 85,670 | 53.421 | 54.727 | 1.02 | 54.389 | equal |
| Q14 | 60,576 | 22.984 | 24.507 | 1.07 | 24.051 | equal |
| **geomean** | | **6.469** | **8.137** | **1.258** | **8.013** (1.239) | all equal |

(`--qreps 7 --mode query`. An earlier full run, `--qreps 5`, gave geomeans
7.351 / 9.119 / 9.314 ms: ratio **1.241**, view 1.267, worst Q3 2.70; with
`--cache-pages 4096` — the 16 MiB cache `Database` uses for disk stores
today — 6.851 / 9.047 / 8.964 ms: ratio **1.321**, worst Q3 2.56, Q2 1.97,
Q9 1.74; all rows equal in every run.) The open costs, for scale:
`DiskStore::open` 0.1 ms, `dictionary()` 174–250 ms, materializing the
`TripleStore` 182–221 ms — exactly what Stream mode avoids.

Where Stream loses (for R3's default-mode decision; by inspection, not
profiled): Q3 (`?X type Publication . ?X publicationAuthor <AP0>`), Q2 and
Q1 read long posting lists such as `s_by_po(type, Publication)`. The disk
store decodes them key by key from B+tree leaves (12-byte keys, one page-map
lookup per leaf under the pager read lock) where the memory store copies one
contiguous slice; `try_so_by_p`, `try_ps_by_o` and `try_subs_by_p` also sort,
because the POS / OSP key order is not the contract order. Q9 adds many
`exists` / short-range probes (a B+tree descent, about 0.8–1 µs each, against
tens of ns in memory). The large-result queries (Q5–Q8, Q10, Q13, Q14) are
dominated by the engine and the dictionary and are at parity.

Overlay on untouched keys (`--mode overlay`; ratios against the store read
directly; geometric means over the six methods `po_by_s`, `o_by_sp`,
`ps_by_o`, `s_by_po`, `count_po_by_s`, `exists`; the "other keys" overlay
adds 5,000 triples under fresh subjects and removes about 4,950 triples
whose subjects and objects the sample never reads). The final probe rotates
which source runs first after each method switch (`--reps 9 --lookups
10000`); an earlier run without the rotation (`--reps 15 --lookups 20000`)
always ran the base first, which slightly favours the others:

| source | rotated: ratio of medians (worst method) | rotated: ratio of minima (worst method) | unrotated: medians (worst) | unrotated: minima (worst) |
|---|---:|---:|---:|---:|
| empty `OverlayView` | 1.059 (1.226) | 1.006 (1.037) | 0.938 (1.062) | 0.949 (1.020) |
| `OverlayView`, other keys changed | 1.040 (1.110) | 1.039 (1.095) | 0.978 (1.090) | 0.968 (1.030) |
| `DiskTriples`, no transaction | 1.005 (1.042) | 1.009 (1.031) | 0.932 (1.052) | 0.944 (1.060) |
| `DiskTriples`, transaction open (other keys) | 1.010 (1.103) | 1.012 (1.043) | 0.950 (1.039) | 0.962 (1.047) |

The per-method spread is noise: the empty overlay adds one bounds check on
an empty bit vector per read, yet its worst median ratio is 1.226
(`count_po_by_s`, 1,207 against 985 ns) while its minimum ratio for the same
method is 1.001 (857 against 856 ns). Absolute medians of the direct reads:
`po_by_s` 1.05–1.12 µs, `o_by_sp` 0.91–1.01 µs, `count_po_by_s` 0.99–1.3
µs, `exists` 0.77–0.82 µs, `ps_by_o` 156–169 µs and `s_by_po` 133–142 µs
(the sample is degree-weighted, so the last two include class objects).
Answers equal on every source in every run. Before the key filters existed
(two in-memory map probes per read) the "other keys" overlay measured 1.055
with a worst method of 1.14 (small LUBM, release); the filters removed that
cost. Q1–Q14 with a 9,926-change transaction open whose changes touch the
predicates the queries scan: geomean 8.275 ms against 8.180 ms without
(1.012×).

Stress test timings (`snapshot_readers_never_see_a_torn_state_and_commits_cancel_them`,
test profile, 8 readers, 12 commits): every commit, drain included, took
0.28–0.65 ms with readers that sleep 200 µs between query steps (the final
test), and 0.14–0.21 ms with readers that only yield (an earlier version);
96 reader runs were expired by commits, 16 completed, all on whole committed
states. `cargo test -p gstore --test dt_overlay`: 10 passed, 1 ignored, 13.8 s
(the LUBM build dominates); with `--include-ignored` the one-minute soak (30
commits against readers running 10-second queries) passes too (60.4 s).

### Acceptance (R2-DISKLIB)

- All tests pass — **met**: see the verification below.
- `disk_stream_probe` at univ 8: answers identical to the materialized path
  — **met** (rows in order, all 14 queries, every run, also through
  `DiskReadView`); warm Q1–Q14 geomean ≤ 1.5× the materialized path —
  **met** (1.241 and 1.258; 1.321 with a 16 MiB cache). Hot spots listed
  above anyway, since single queries reach 2.4–2.7× (Q3) and 1.8–1.9× (Q2).
- `disk_stream_probe` at univ 38 — **left to the measure stage**.
- Overlay reads on untouched keys ≤ 1.1× base cost — **met** at univ 8 as
  a geometric mean over the methods (1.006–1.059 in the rotated run,
  0.93–0.98 unrotated; the "other keys" overlay 1.040 / 1.039); single
  methods reach 1.10–1.23 by medians on this noisy machine but ≤ 1.095 by
  minima. A quiet re-run (measure stage, command below) should confirm the
  per-method figures.

### For the measure stage (univ 38, quiet machine)

```sh
D=/tmp/p6-disklib-38            # any scratch directory with ~1.5 GB free
mkdir -p $D
flock /tmp/gstore-bench.lock /tmp/gstore-base-aaea9f8/bin/gbuild --disk $D/lubm38 \
    /tmp/gstore-bench-cache/lubm-like1-u38-s0.nt       # ~10 min (incremental)
cargo build --release -p gstore --example disk_stream_probe
flock /tmp/gstore-bench.lock target/release/examples/disk_stream_probe $D/lubm38.db \
    --mode query --qreps 5                            > probe38-query.txt
flock /tmp/gstore-bench.lock target/release/examples/disk_stream_probe $D/lubm38.db \
    --mode query --qreps 5 --cache-pages 4096         > probe38-query-c4096.txt
flock /tmp/gstore-bench.lock target/release/examples/disk_stream_probe $D/lubm38.db \
    --mode overlay --reps 9 --lookups 5000            > probe38-overlay.txt
rm -rf $D
```

Read `query_disk_over_mem` and `query_all_match` (acceptance ≤ 1.5, true),
and `overlay_other_ratio` / `overlay_other_ratio_min` (≤ 1.1). Any `gbuild
--disk` of the same data works (the format is unchanged; after R2-BULK
merges, the bulk-built database is the realistic one). The whole-file cache
of the first command needs about 0.8 GB of RAM plus about 1 GB for the
materialized side.

## Requests/Not yet

- **R3-STREAM** (wiring `Backend::Disk(DiskTriples)`):
  - use `insert_triple` / `remove_triple` (they return `Result`) and call
    `touched()` exactly when they return `Ok(true)`; `take_write_error` for
    the Disk variant is `DiskTriples::take_error()`;
  - default-graph transactions map to `begin` / `commit` / `rollback`;
    `persist_mutation` sends named-graph writes through `named_op`;
    Database commit points call `commit_point()`; `mark_vstree_fresh()`
    marks the store dirty, so the next commit point persists it;
  - `DiskDict::intern` writes terms through the shared pager **without** the
    gate (terms are orphan-safe and do not move the epoch), holding the pager
    write lock for one operation (C8: gate → pager is never inverted);
  - `DiskStore::query` and the `TripleSource` impl of `DiskStore` itself
    still turn read errors into empty answers (kept: `kv_bench` and R2-DICT's
    out-of-core path use them); Stream mode must evaluate over `DiskTriples`
    or `DiskSource`, never over a bare `DiskStore`.
- **R4-SNAP** (`/begin` on a Stream database): capture `read_view()`; run each
  snapshot query in a `ReadScope` inside `with_base`, over
  `DiskSource::new(ds)`, with `Evaluator::with_cancel(cancel.clone())`; on
  `ViewError::Expired` re-run on the live database, on `Closed` answer
  NotLoaded; key the snapshot CSR by `begin_epoch()`.
- **Engine owners (Stream-mode latency, input for R3's default decision)**:
  long posting lists are the gap (Q3 2.4×, Q2 1.8× at univ 8). Options that
  stay inside the TripleSource seam: answer `s_by_po` / `ps_by_o` from a
  VList-compacted value (as `o_by_sp` already can), or let the executor
  probe `exists` for a small bound side instead of materializing the large
  one.
- Not yet: `DiskReadView` builds a fresh `DiskStore` view per `with_base`
  call (it reloads the O(P) predicate statistics, a few µs at 17
  predicates); a store with very many predicates may want a cached view per
  epoch. The overlay's key filters are fixed at 8 KiB per key kind: a
  transaction touching hundreds of thousands of keys saturates them, which
  only costs the (exact) merge path on every read.
