# R2-REVIEW — Phase 6 Round 2 review fixes

## Changelog

- **Graph functions work over an out-of-core dictionary again.**
  `DiskStore::query` evaluates over its lazy (backed) dictionary, and the
  graph functions (`shortestPath`, `kHopReachable`, `PR`, `WCC`, …) printed
  their vertex and predicate labels through the legacy `&str` dictionary
  API, which fails closed on a backed dictionary (C4). Every such query
  failed with "the legacy &str dictionary API cannot serve an out-of-core
  dictionary"; before Round 2 it answered. `GFunCtx::entity_str` /
  `node_str` / `gnode_str` / `pred_str` now return `TermStr` read through
  `Dictionary::term` / `pred`, and so do PFN's `GraphCtx::term_str` /
  `pred_str` (the trait signature changed; `TermStr` converts into a
  `Cow<str>` for the wasm host). No production code calls the legacy API
  any more. In-memory answers are unchanged (borrowed strings, no copy).
- **A batched dictionary read decodes only the ids it was asked for.**
  `DiskDict::resolve_sorted` (one batch per 4,096-row materialization chunk)
  and `prefetch` walked every slot between requested ids, reading the
  overflow chain of every unrequested long literal (> 123 bytes) in a run
  and failing the whole batch — and the query — on a damaged slot nobody
  asked for. The new `IvArray::for_each_key` still reads the page-table and
  directory pages once per run, in file order, but decodes only the
  requested slots.
- **The per-query id → term / id → value memo is bounded by bytes and used
  for out-of-core dictionaries only.** It was bounded by 65,536 entries per
  memo and filled for every evaluator: a FILTER, BIND or ORDER BY over many
  distinct large literals held two copies of each (a `Term` and a `Value`)
  until the query ended — about 520 MiB per query with 4 KiB literals, also
  on the in-memory path, where it doubled the CPU time too. Now one
  `IdMemo` per query (shared with `GRAPH` sub-evaluators) holds at most
  8 MiB, charged per entry by key length plus overhead, skips keys over
  16 KiB, and is not used at all over an in-memory dictionary (whose
  strings are borrowed; values are parsed and dropped as before Round 2).
- **`DiskTriples`** (library only; R3 wires it into `Database`):
  - `disk_store()` and `shared_pager()` return `Result` and refuse once the
    store is broken (C5): after a commit that failed part-way, the raw store
    held neither the old nor the new committed state and answered reads
    from it without any error.
  - A `commit()` from inside one of the store's own snapshot reads (it would
    wait for itself) is refused **before** the transaction is taken: the
    transaction stays open and the store healthy. It used to be dropped
    with an error that looked recoverable, losing every buffered write.
  - `vstree_stale()` is also `true` while the open transaction adds triples:
    `vstree.kv` was built from the committed state and would prune the
    transaction's own new edges (C9). Removals keep the tree a sound
    superset and do not count.
  - The overlay's memoized statistics deltas (`distinct_subjects`,
    `distinct_objects`, `num_predicates`, `pred_distinct_*`) are not
    memoized when computing them hit a base read error: each later statement
    probes again and reports the error, instead of silently using a wrong
    value (C1, C2).
- **Bulk build (`gbuild --disk`, `bulk::build`):**
  - A spilled sorted run no longer keeps a file descriptor open until the
    final merge: its file is closed once written and reopened by path when
    merged, so a sorter holds at most fan-in + 1 descriptors. With
    `--memory 1` a 4M-triple input failed with "Too many open files" under
    the default `ulimit -n 1024`.
  - The I/O buffers of spilling, merging and reading back runs and spools
    are charged to the budget while they exist (budget.rs said merge
    read-ahead was covered; it was not), so `memory: … peak` is no longer
    under-reported. Each is now `min(256 KiB, share / (fan-in + 1))`
    (`Budget::io_bytes`), so a merge's readers and writer fit one more share
    and a tiny `--memory` stays near its budget; every share of 16 MiB or
    more — all of them at the default budget — still gets 256 KiB.
  - Scratch directories are created exclusively under a fresh name
    (`gstore-bulk-scratch-<pid>-<seq>-<nonce>`) and never by removing an
    existing one: builds in different PID namespaces sharing a `--tmp-dir`
    could get the same `<pid>-<seq>`, delete each other's live runs and
    write wrong indexes with exit status 0. Each scratch directory holds a
    `LOCK` file under an advisory lock for its lifetime, and a build sweeps
    the scratch directories in its `--tmp-dir` / `build_tmp_dir` whose lock
    nobody holds — what crashed or killed builds left behind (they were
    never removed before).
  - With `--threads` > 1 the parser threads keep the sequential import's
    batches (`batch_size` / `batch_bytes`) and hand each over in parts of
    8,192 quads / 8 MiB of terms as they are parsed, followed by an
    end-of-batch event; an encoder error (an over-long key, an id-space
    bound) is held until the end of its batch, where the sequential import
    raises it. They used to batch at 8,192, so such an error surfaced at a
    different statement than with one thread: a different first error in
    strict mode and missing `parse_error.log` lines in tolerant mode. This
    needed two provided methods on `parser::import::QuadSink`, `handoff()`
    (default `None`: whole batches, as before) and `batch_end()` (default:
    nothing); every other import is unchanged.
  - A RocksDB store that is open is never replaced: the build refuses when
    `rocksdb/LOCK` (or a staged `rocksdb.new/LOCK`) is locked by any process
    (Linux: read from `/proc/locks`, whatever the holder's pid namespace).
    `Database::open` takes no directory lock for that layout, so a
    `gbuild --disk --force` over a served RocksDB database used to delete
    its store under it and lose every later acknowledged write.
  - A stale `kvstore.kv.wal` of the database being replaced whose replay
    fails part-way (ENOSPC, EIO) stops the install with the log in place —
    the old database's next open replays it. The error used to be ignored
    and the log deleted, which left a torn old store if the rename that
    follows did not happen.
  - The install's commit point is the rename of the new `kvstore.kv`. An
    error after it no longer fails the build (the new database is live):
    the remaining steps still run and the errors are returned in the new
    `BulkReport::warnings`; gbuild prints "built (the new database is in
    place), but its install did not finish: …" and exits with status 1.
    The replaced database's reasoning rules (`reason_rule_files/`) are
    removed by the install itself, right after the new store is durable
    (crash table step 6), instead of by gbuild after `bulk::build`
    returned — which an error or a kill in between skipped.
- **Tests:** `tests/dt_dict_backends.rs` (graph functions through
  `DiskStore::query` equal the in-memory dictionary's answers; a batch
  never reads an unrequested damaged neighbour), `tests/dt_overlay.rs`
  (memoized statistics after a failed read, a commit refused inside a view,
  `vstree_stale` during a transaction, the broken store's raw accessors),
  `tests/dt_bulk_crash.rs` (dead vs live scratch directories in a shared
  temp dir, a held `rocksdb/LOCK` and — with `rocksdb` — an open RocksDB
  database, a failed WAL replay, warnings after the commit point), and
  unit tests in `ivarray.rs`, `pfn/graph.rs`, `query/engine.rs`
  (`IdMemo` bound), `bulk/extsort.rs` (descriptors per spilled run, charged
  I/O buffers), `bulk/budget.rs`, `bulk/scratch.rs` and `bulk/source.rs`
  (a sink error with 1, 2 and 4 threads).
- `examples/disk_stream_probe.rs`: with its transaction open the VS-tree is
  stale for `DiskTriples`, so the transaction's Q1–Q14 (and the plain side
  of `query_txn_over_plain`) run without it (`txn_vstree=off`), and
  `q{i}_txn_view_match` compares with the overlay view evaluated without
  any VS-tree (before, both sides were pruned by the same stale tree).
- `changelog.d/phase6-R2-merge.md`: its claim that no production path
  reaches the legacy `&str` API with a backed dictionary is corrected.

## GAP_PLAN status line

none (review fixes to the R2 packages).

## Docs notes

- docs/STORAGE_BACKEND.md (C4 notes): the legacy `id_to_string` /
  `predicate_to_string` are no longer called by any production code; graph
  functions and PFN read labels through `term()` / `pred()`, so they work
  over an out-of-core dictionary (`DiskStore::query`, Stream mode).
- docs/PFN.md: `GraphCtx::term_str` / `pred_str` return
  `Option<TermStr<'_>>` (deref to `str`), not `Option<&str>`.
- docs/STORAGE_BACKEND.md (R2-DICT engine notes): the per-query id → value
  memo serves out-of-core dictionaries only and holds at most 8 MiB per
  query.
- docs/STORAGE_BACKEND.md (C5 notes): `DiskTriples::disk_store()` /
  `shared_pager()` return `Result` and refuse a broken store;
  `vstree_stale()` covers the open transaction's additions; a commit refused
  inside one of the store's own views keeps the transaction.
- docs/STORAGE_BACKEND.md or docs/DEPLOY.md (bulk build, with R2-BULK's
  notes): the memory budget now covers the run and spool I/O buffers
  (`memory: … peak` can exceed a very small `--memory`, e.g. 1 MiB, by
  their size); `--tmp-dir` scratch directories are locked while in use, and
  a build removes the ones dead builds left (older builds' unlocked
  `gstore-bulk-scratch-<pid>-<seq>` directories, with no `LOCK`, are not
  touched); the build refuses a directory whose RocksDB store is open
  (Linux); after the install's commit point, cleanup failures are reported
  as "built, but …" (exit status 1, the new database is live); the install
  removes the replaced database's `reason_rule_files/`.
- The R2-BULK install table (commit.rs module docs) gained the replay
  failure in step 3 and the rules removal in step 6.
- Import API (rustdoc only): `QuadSink::handoff` / `batch_end` let a sink
  take each batch in parts while the batch boundaries stay those of
  `ImportOptions::batch_size` / `batch_bytes`.

## Measured numbers

These fixes are bounded by deterministic tests; each regression test was
run against the unfixed code first and failed there:

- graph functions through `DiskStore::query`: "the legacy &str dictionary
  API cannot serve an out-of-core dictionary" before, equal to the in-memory
  dictionary's rows after (11 queries).
- batched dictionary read with a damaged unrequested overflow slot between
  two requested ones: "dictionary literal batch: … corrupt slot tag 238 for
  key 5" before, both rows after.
- descriptors held by one sorter after 586 spilled runs (share 4 KiB,
  fan-in 4): 586 before, at most the merge's own afterwards (test margin:
  < 200 including other tests' files).
- the sink-error replay with 2 threads: `parse_error.log` was missing the
  line-10002 failure before; identical to 1 thread after (2 and 4 threads,
  strict and tolerant).
- the failed WAL replay (a committed batch with a page at id 2³²−1, past
  ext4's file-size limit, so the replay fails with EFBIG): before, the
  build succeeded and deleted the log; after, it fails with the log kept
  and the old `kvstore.kv` in place.
- `IdMemo` bound: 6,000 distinct 1.5 KB literals through a FILTER and an
  ORDER BY over the lazy dictionary keep the memo within 8 MiB (fewer than
  6,000 values memoized); over the in-memory dictionary nothing is
  memoized. (The reviewer measured +520 MiB peak heap per query with
  100,000 × 4 KiB literals on the in-memory path before.)

`gbuild --disk` A/B at univ 8 (`/tmp/gstore-bench-cache/lubm-like-univ8.nt`,
1,029,275 triples, one file), base = the release `gbuild` of `6bf616f`,
fix = this change, run alternately in one session (4 vCPU VM, load average
about 2 from other agents; peak RSS from `getrusage` of the child, peak
descriptors sampled every 20 ms from `/proc/<pid>/fd`):

| variant | wall s base / fix | peak RSS MiB base / fix | parse+encode ms base / fix | peak fds base / fix |
|---|---|---|---|---|
| default (4 threads), run 1 | 3.72 / 3.92 | 97.6 / 99.2 | 3267 / 3494 | 7 / 7 |
| default (4 threads), run 2 | 4.02 / 3.98 | 97.8 / 98.5 | 3550 / 3541 | 7 / 7 |
| `--memory 1 --threads 1` (1,104 runs, 482 MiB spilled) | 5.42 / 5.20 | 55.9 / 55.8 | 4116 / 3735 | **296 / 11** |
| `--memory 128` (3 runs) | 3.92 / 4.01 | 75.6 / 70.4 | 3410 / 3464 | 9 / 10 |

`kvstore.kv` and `vstree.kv` are byte-identical between base and fix in
every variant (`cmp`). The reported budget peak at `--memory 1` moves from
0.4 to 0.5 MiB (the run I/O buffers are charged now, and are 29 KiB each
instead of 256 KiB); at the default budget it is 104.0 MiB in both. A
first version of the batching fix that let the parser threads hold whole
sequential batches cost +55 MiB peak RSS and +10% parse time at univ 8; the
part-wise hand-over above does not.

`disk_stream_probe --mode overlay --lookups 1 --changes 20000` on a
bulk-built `testdata/lubm`: `txn_vstree=off`, and all 14
`q{i}_txn_view_match=true` against the VS-tree-free reference (the
reviewer's setting where the old probe pruned Q6 and Q13 on both sides and
still reported a match).

Verification of this change (same VM; `CARGO_PROFILE_DEV_DEBUG=0
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`):

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --
  -D warnings`: clean.
- `cargo test --workspace --no-fail-fast`: 2,174 passed, 0 failed, 2
  ignored.
- `--features rocksdb`: clippy clean; `cargo test -p gstore --features
  rocksdb`: 1,995 passed, 0 failed, 2 ignored (it includes
  `an_open_rocksdb_database_is_not_replaced`).
- `--features pfn-wasm` (PFN's `GraphCtx` changed): clippy clean; `cargo
  test -p gstore --features pfn-wasm`: 1,992 passed, 0 failed, 1 ignored
  (run before the last change, to `parser/import.rs` and `bulk/source.rs`,
  which touches no feature-gated code).

## Requests/Not yet

- **R3-ROCKS:** `Database::open` (and `create_rocksdb`, and the server's
  `hold_dir_lock` calls) should take the directory's `DirLock` for the
  RocksDB layout too. The bulk build now checks `rocksdb/LOCK` through
  `/proc/locks`, which covers Linux (every deployment target) but no other
  OS; with the DirLock it would also be portable.
- **R3-STREAM:** `DiskTriples::disk_store()` / `shared_pager()` now return
  `Result` (open the Stream-mode `DiskDict::open_shared` with
  `dt.shared_pager()?`); use the VS-tree with `DiskTriples` only while
  `vstree_stale()` is `false`, which now includes an open transaction's
  additions.
- Not done: a crash in the short window between the new `kvstore.kv`
  becoming durable and the old `reason_rule_files/` being removed (one
  `remove_dir_all` and a directory sync) still leaves the old rules beside
  the new database. Closing it needs the rules tied to the data they were
  made for (a generation id in the ledger), a reasoning-format change
  outside Phase 6.
- Not done: scratch directories left by builds before this change have no
  `LOCK` file and are not swept (they cannot be told from one being set
  up); remove them by hand.
