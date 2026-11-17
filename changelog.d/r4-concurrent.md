# r4-concurrent — ConcurrentDb defects fixed (D2: kept): incremental version-chain GC, consistent partial-failure updates, poison tolerance; transaction/MVCC claims in CPP_PARITY and README

## Changelog

Behaviour changes first (all in the library type `concurrent::ConcurrentDb`,
which no server or command-line tool uses):

- **An UPDATE that fails part-way is published** (review conc#4, http#9,
  robust#13). `ConcurrentDb::update` returned early on an error, although the
  operations before the failing one stay applied in the writer database (as
  `Database::query` keeps them): readers kept the old snapshot, no OCC
  version was recorded, a transaction overlapping the update committed
  without the documented conflict, and the change surfaced later inside an
  unrelated version. Now a request that changed the database (its
  `mutation_count` moved) is published and recorded as a non-transactional
  write even when it fails, so an overlapping `RepeatableRead` /
  `Serializable` transaction conflicts; the error is returned as
  `Database::query` returned it. A request that changed nothing before
  failing publishes nothing (no spurious version or conflict).
- **No lock stays poisoned; no double panic** (review conc#3, http#17).
  Every lock was taken with `unwrap()`: one panicking `write()` closure made
  every later `update` / `sync` / `Txn::commit` panic with `PoisonError`, and
  a panic inside a commit while the OCC lock was held made `Txn::drop` panic
  again during the unwind — "panic in a destructor during cleanup", process
  abort. Now: a `write()` closure that panics has the database transaction
  it opened rolled back, what it applied otherwise published (as any
  `write()`, a non-transactional write), the locks released, and the panic
  then continues into the caller; a commit that panics while applying its
  writes rolls them back; a writer lock poisoned by any other panic is
  recovered by the next writer, which first publishes the state it was left
  in (overlapping transactions conflict); a poisoned OCC lock is recovered
  conservatively (every transaction live at the panic conflicts); `Txn` and
  `VersionView` drops never panic and only deregister while a panic unwinds.
  A transaction a `write()` closure leaves open when it *returns* is left
  alone, as before.
- **Version-chain GC is incremental** (review conc#2, http#8). The GC walked
  every chain on every commit, conflict, `Txn` drop and `VersionView` drop,
  and a chain whose state differs from the construction-time base (every
  transactionally inserted triple) is kept for the wrapper's life: after
  20,000 transactional inserts each commit and each empty `begin()`+drop
  examined 20,000 chains (8.3 ms per commit in the review's measurement,
  quadratic in total). Each commit now queues the keys it wrote under its
  version, and a GC examines only the keys of the versions the oldest live
  pin has passed since the previous GC — each record once; a drop with
  nothing to collect does no chain work. The OCC history is collected from
  its oldest end. The memory of chains that differ from the base (one record
  per such key, for the wrapper's life) is unchanged and now documented.
- New diagnostics: `ConcurrentDb::version_chain_stats()` returning
  `concurrent::VersionChainStats { chains, records, gc_visits }` (the chains
  kept, their records, and a GC work counter).
- Documentation: `src/concurrent.rs`'s module docs state that `ConcurrentDb`
  is a library-only second concurrency model (the servers are single-writer
  with their own `/begin` transactions; review design#14) and describe the
  failure and panic semantics above and the GC's cost and memory;
  `Dictionary`'s `Clone` documents its side effect — a clone of an
  out-of-core dictionary pins every id for its lifetime, so a long-lived
  clone (a published snapshot, `ConcurrentDb`'s base) defers id reuse
  (design#14); `src/db/query_cache.rs` no longer calls `query_cache_size` a
  proposed `[system]` key (it is an implemented `[query]` key;
  gap-doc-claims#4).
- `docs/CPP_PARITY.md`: the three status tables are rewritten from the code
  (gap-doc-claims#3, #4, #5): how to read them; server vs library-only
  (「库」) vs experimental (「实验」) items; servers single-writer with three
  `isolevel` values that all behave serializably; OCC multi-writer, four
  isolation levels and per-key version chains only in `ConcurrentDb`; the
  cluster a frozen experimental feature, not production replication;
  library-only or unwired: RDFS materialization, update/redo logs and
  incremental backup, top-k subgraph matching, the prefix-compression
  codec, the `sp2o_vlist` index, `IsArray`. Stale rows updated (degree API,
  numeric tower with exact decimal division, 32 graph functions, `GRAPH`
  update templates, HTTPS via `tls`); wrong statements in the dated update
  paragraphs carry inline 〔更正〕 corrections (÷ and SUM are not f64,
  incremental backup copies the redo log, upstream has no JSON-LD parser,
  32 graph functions, "gap basically zero"); a dated paragraph records the
  rewrite.
- Tests: new `tests/concurrent_db.rs` (8 tests, public API) and 4 tests in
  `src/concurrent.rs` driven by test-only failpoints (`failpoint(step)`,
  armed per thread; no crate-wide failpoint module). The redo-log unit test
  of `src/concurrent.rs` uses a per-process temp directory removed also on
  failure (it shared a fixed `/tmp` path between parallel runs). conc#15
  coverage — every behaviour the finding listed has a named test:

  | behaviour (conc#15) | test(s) |
  |---|---|
  | `ConcurrentDb::write()` closure panic: no poisoning | `tests/concurrent_db.rs`: `a_panicking_write_closure_does_not_poison_the_writer`, `a_transaction_left_open_by_a_panicking_closure_is_rolled_back`, `a_transaction_a_closure_leaves_open_on_return_stays_open` |
  | panics inside commit / update; drops while unwinding | `src/concurrent.rs` tests: `a_panic_inside_commit_neither_aborts_nor_poisons`, `a_panic_while_applying_a_commit_rolls_its_writes_back`, `a_panic_inside_update_is_published_by_the_next_writer`, `drops_while_unwinding_with_a_poisoned_occ_lock_do_not_abort` |
  | `ConcurrentDb::update()` partial failure | `tests/concurrent_db.rs`: `a_partially_applied_update_is_published_and_conflicts_overlapping_txns`, `a_failed_update_that_changed_nothing_is_not_a_write` |
  | version-chain GC cost (conc#2, http#8) | `tests/concurrent_db.rs`: `version_chain_gc_work_stays_flat_after_20k_commits`, `churned_keys_leave_no_chains`, `a_pinned_view_keeps_its_records_until_dropped` |
  | concurrent UUID uniqueness (round 3, exists) | `tests/sparql_builtins.rs`: `uuids_and_rand_are_unique_across_threads` (r3-sparql-functions, conc#1 / query#16) |
  | cancelling a live CSR build (round 2, exists) | `tests/cancellation.rs`: `a_cancelled_read_stops_its_csr_build_and_caches_nothing`, `a_cancelled_update_stops_its_csr_build_and_caches_nothing`, `cancel_stops_the_csr_build_of_a_stream_database`, `cancel_stops_the_csr_build_of_a_snapshot_read` (r2-index-cancel, conc#5) |
  | concurrent log-line integrity (round 1, exists) | `tests/crash_regressions.rs`: `concurrent_log_lines_never_interleave`, `concurrent_rest_requests_log_whole_lines` (conc#6) |

## GAP_PLAN status line

§1 row 1.11 (文档自评失真): `docs/CPP_PARITY.md` 的三张状态表已按代码重写(2026-11,review gap-doc-claims#3/#4/#5):区分服务器、「库」与「实验」,旧"更新"段落中与代码不符的表述就地加〔更正〕。

## Docs notes

- **README.md** (gap-doc-claims#5) — replace the "Transactions" feature
  bullet (currently "commit/rollback, per-key MVCC + snapshot isolation, four
  named isolation levels, and full + **incremental** backup/restore. …")
  with:

  > * **Transactions** — `begin` / `commit` / `rollback` on both HTTP
  >   protocols (REST `/begin`, `/tquery`, `/commit`, `/rollback` and the
  >   `/api` operations), one transaction per database at a time: a database
  >   is single-writer (while one is open, a second `begin` and every other
  >   write to it are refused — `/api` codes `1008` / `1004`, REST `409`);
  >   upstream's three `isolevel` values are
  >   validated, logged and echoed, and all behave serializably. Reads of one
  >   database run in parallel on both HTTP servers (writes are exclusive),
  >   and an HTTP transaction's begin-time snapshot copies no triple on
  >   in-memory and on-disk databases (copy-on-write stores, disk read views;
  >   RocksDB still copies). Full backup/restore (`gbackup` / `grestore`,
  >   `/api` `backup` / `restore`). The library additionally offers
  >   `concurrent::ConcurrentDb` — snapshot-isolated readers, optimistic
  >   multi-writer transactions with four named isolation levels
  >   (`ReadUncommitted` behaves as `ReadCommitted`) and per-key MVCC version
  >   chains — and redo-log incremental backup / restore
  >   (`Database::backup_incremental`); no server or command-line tool uses
  >   either.

  In the same list, replace "Also an **RPC cluster** (`rpc` / `cluster`),
  and **concurrency/MVCC** (`concurrent`)." with "Also an experimental **RPC
  cluster** (`rpc` / `cluster`, see [Experimental: cluster](#experimental-cluster))
  and the library-only multi-writer **MVCC** wrapper
  (`concurrent::ConcurrentDb`)."; after "C++ parity is documented in
  [`docs/CPP_PARITY.md`](docs/CPP_PARITY.md)" add "(per component, with
  library-only and experimental items marked; the upstream gaps still open
  are listed in [`docs/GAP_PLAN.md`](docs/GAP_PLAN.md) §9.3)"; and in the
  Layout table change the `rpc` / `cluster` / `concurrent` row's role to
  "experimental RPC cluster; `ConcurrentDb`: library-only snapshot isolation
  + OCC multi-writer transactions".
- **docs/REFACTOR_BACKLOG.md** — add:
  - (design#14, D2/D3) `concurrent::ConcurrentDb` is a second, library-only
    concurrency model next to the servers' single-writer `DbCell` +
    `/begin` transactions (its own OCC, isolation levels, version chains and
    publication path). Kept and fixed (D2); merging the two models — or
    building the servers' multi-writer transactions (PHASE6_PLAN §8 D14) on
    it — is a design decision left open.
  - (conc#2, http#8 — memory half, D3) The version chains keep one record
    for every key whose committed state differs from the construction-time
    base snapshot, for the wrapper's life (`version_chain_stats().chains`).
    Folding them needs re-basing: replace the base with a newer published
    snapshot once no pin is below it, and drop the chains folded into it.
    That changes what `VersionView::query` reads (it materializes from
    `base`, and its code belongs to r4-sparql-guard in round 4), and would
    make version views see legacy `update()`/`write()` writes, which they do
    not today. Not done: the GC cost was the defect; the memory is
    documented.
  - (conc#2, observation) `VersionView::query` materializes the base
    snapshot plus every chain on each query (O(base + chains)); point reads
    (`VersionView::contains`) clone nothing. Unchanged, documented on the
    method.
  - (conc#2 follow-up, dictionary) Publishing shares the dictionary's
    copy-on-write interners with the snapshot, and an interner has no
    spare copy (unlike `TripleStore::keep_spare_copies`): the first
    brand-new term interned after a publish copies that term space's
    interner, O(terms) — about 0.4 ms per commit or `write()` at 20,000
    entities in a debug build (Measured numbers of r4-concurrent). A spare
    interner replayed with the terms interned since (O(new terms)) would
    make such commits flat too; it changes `src/dict/mod.rs` (an interning
    hot path, bench-gated), outside this package.
  - (conc#15, observation) `ConcurrentDb`'s snapshot and version-view
    queries take no cancel flag, so their CSR builds cannot be cancelled
    (the servers' read paths can, since round 2): an API addition, not a
    defect of the existing contract.
- **docs/DEPLOY.md**: none (no configuration key, no compat switch).
- **docs/STORAGE_BACKEND.md**: none (no on-disk format change).
- **docs/CPP_PARITY.md**: edited directly by this package (convention 2).

## Measured numbers

- GC work counter (`VersionChainStats::gc_visits`, deterministic), debug
  test profile:
  - before the fix (the counter added to the unfixed GC; run of
    `tests/concurrent_db.rs`): the first 100 one-triple commits on a fresh
    wrapper examined 5,050 chains (1 + 2 + … + 100); a churn of 2,000
    transactional inserts then 2,000 deletes examined 4,002,000 chains;
  - after: 100 one-triple commits examine 100 chains both on a fresh wrapper
    and after 20,000 commits (20,000 chains kept); 100 × (empty
    `begin()`+drop, an aborted transaction, a `version_view()`+drop) examine
    0; the churn examines 4,000 (one visit per record); a conflicting commit
    examines at most 2 (a scratch counter test; the assertions of
    `tests/concurrent_db.rs` bound the same quantities).
- Timings: a scratch test (not committed) on the public API, one-triple
  transactional commits of a new subject each, debug test profile
  (`cargo +1.99.1 test`, `CARGO_PROFILE_TEST_DEBUG=0`), run back to back
  on the base (`b147336`'s `src/concurrent.rs`) and on this branch, on a
  4-vCPU machine **with other build jobs running** (load average 11–15), so
  only the ratios are meaningful; means over 500 operations:

  | | base, 3k commits | branch, 3k | base, 20k | branch, 20k |
  |---|---|---|---|---|
  | commit, first 500 on a fresh wrapper | 112.8 µs | 10.3 µs | 145.6 µs | 10.4 µs |
  | commit of a triple of known terms, after N commits | 1.172 ms | 8.0 µs | 11.534 ms | 11.0 µs |
  | commit interning a new term, after N commits | 987.5 µs | 66.4 µs | 10.405 ms | 447.6 µs |
  | `write()` interning a new term (no GC, control) | 44.1 µs | 70.0 µs | 357.9 µs | 371.9 µs |
  | empty `begin()` + drop | 1.263 ms | 139 ns | 9.971 ms | 136 ns |
  | `version_view()` + drop | 1.537 ms | 106 ns | 11.005 ms | 106 ns |

  The base's 20k run took 134.6 s for its 20,500 commits (quadratic). On
  the branch a commit that only references known terms is flat (10.3 →
  11.0 µs at 20k). A commit that interns a brand-new term still grows with
  the dictionary (447.6 µs at 20k) — as `write()` does without any version
  chain (371.9 µs): the dictionary's interners are copy-on-write without a
  spare copy, so the first new term after a publish copies that term
  space's interner (O(terms)). That is the dictionary's documented cost,
  not the GC's; recorded for the backlog above and now stated in
  `src/concurrent.rs`'s cost model.
- Verification on this branch (same machine state): `cargo +1.99.1 clippy
  --workspace --all-targets -- -D warnings` clean; `RUSTDOCFLAGS="-D
  warnings" cargo +1.99.1 doc -p gstore --no-deps --locked` clean;
  `cargo +1.99.1 test --workspace --no-fail-fast` with
  `GSTORE_DISK_QUERY_MODE=memory`: 2,925 passed, 0 failed, 8 ignored; with
  `GSTORE_DISK_QUERY_MODE=stream`: 2,924 passed, 1 failed (the load-sensitive
  JSON-LD timing test below, which passed 3 of 3 times re-run alone in
  stream mode), 8 ignored.
- Bench smoke gate: not run — `gstore-bench` does not use `ConcurrentDb`
  (no hot path of convention 8 is touched: only `src/concurrent.rs`, docs and
  comments changed), so the gate's workloads run identical code.

## Requests/Not yet

- Edits outside the owned files: `src/db/query_cache.rs` module docs (two
  lines: `query_cache_size` is the implemented `[query]` key;
  gap-doc-claims#4). `src/dict/mod.rs`: documentation on `impl Clone for
  Dictionary` only (owned for that purpose).
- For the round-4 integrator / r4-sparql-guard: `VersionView::query` and
  `Snapshot::query` (r4-sparql-guard's) were not edited. `VersionView::query`
  keeps calling `self.db.versioned.read().unwrap()`; `versioned` is now a
  `ChainLock` whose `read()` returns `Result<_, Infallible>`, so that line
  compiles unchanged and can no longer panic on a poisoned lock. If
  r4-sparql-guard rewrites the function, keep calling `.read()` on the
  field (any `unwrap` / `expect` / `let Ok(..)` works). Textual conflicts may
  appear next to `VersionView::contains` (edited here) and in the module
  docs; keep both sides.
- Observed, for another package (r3-parser's test; r5-test-hygiene):
  `tests/rdf_parser_conformance.rs::jsonld_node_contexts_and_context_arrays_add_layers_not_copies`
  failed once in each full workspace run of this branch under load
  (average 11–15 from parallel builds): "4x the context array members took
  23.7x / 20.5x as long (1.42 ms -> 33.6 ms / 1.74 ms -> 35.6 ms)", over its
  30 ms floor; it passed 3 of 3 times run alone in stream mode, and this
  branch changes no parser code. A 4x input taking ~20x under load may be
  noise or a superlinear part; worth a work-counter check.
- Not yet: the memory half of conc#2/http#8 (re-basing the version-chain
  base; reason above, backlog), and the dictionary's interner copy on the
  first new term after a publish, which keeps commits that intern new terms
  O(terms) in wall-clock time although their GC work is now flat (backlog
  entry above; `src/dict/mod.rs` is not this package's). All ids of the
  package are otherwise fixed.
