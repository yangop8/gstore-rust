# r2-index-cancel — Accelerator and index builds: sound STR() substring pruning, cancellable off-lock CSR/string-index builds, tracked lifecycle threads

## Changelog

Behaviour changes first:

- **Substring filters over typed literals no longer lose rows** (review
  query#1, gap-accel-soundness#2). The dictionary substring index that
  prunes `CONTAINS` / `STRSTARTS` / `STRENDS` / plain `REGEX` candidates
  held each term under its value's canonical form only, while `STR(?v)`
  compares the term's own lexical form: `FILTER(CONTAINS(STR(?c), "001"))`
  missed `"0012"^^xsd:integer`, and `STRSTARTS(STR(?v), …)` missed
  leading-zero decimals (`"0042.50"`), signed and derived integers
  (`"+0012"`, `"005"^^xsd:unsignedByte`), exponent floats (`"1.0E3"`,
  `"1e3"^^xsd:double`) and padded or numeric booleans (`" true"`, `"1"`) on
  every layout with an in-memory dictionary (memory, Materialize, RocksDB;
  Stream mode has no index and was right). A term whose two forms differ is
  now indexed under both, so the candidates of a bare `?v` and of `STR(?v)`
  are each a sound superset.
- **Keep-alive is decided with the response, not broken after it**
  (http#15). While connections wait for a worker (checked once the request
  has been read, before its handler runs), a response now says
  `Connection: close` (no `Keep-Alive` header) and the connection closes
  after it, instead of promising keep-alive and closing the idle connection
  a moment later, when the client may be sending its next request (which
  then got a reset or EOF). A connection that promised keep-alive and then
  finds a queued connection yields its worker only after a last 1 ms look
  finds no request on its way (in the socket, in the TLS layer's buffer,
  or partly received). A next request already received is still served
  first.
- **`/cancel` and a shutdown stop CSR and substring-index builds** (conc#5,
  analytics#6). The CSR build of the advanced graph functions — for a plain
  read, an UPDATE's WHERE clause, a read on an open transaction's in-memory
  snapshot — and the substring-index build of a `CONTAINS`-style filter
  watch the request's cancel flag, checked between predicates (every
  65,536 pairs of an in-memory predicate) and between phases, and every
  1,024 dictionary terms; a cancelled build caches nothing. Before, they
  ran to completion (about 0.3 s per million triples in Stream mode) and
  were cached after the cancel.
- **Index builds no longer block readers** (conc#5, db#5). A CSR build runs
  with the CSR cache's lock released (concurrent callers still share one
  build and a cancellable waiter can stop waiting), so asking whether a CSR
  is cached (`/begin`'s snapshot capture, `show`) never waits for one. A
  reader that finds a substring-index build running goes on without the
  prefilter (exact all the same) instead of waiting.
- **A write that adds or frees terms no longer throws the substring index
  away** (db#5). The next substring query extends the cached index with the
  terms that are new or changed (one identity check per dictionary term,
  trigram work only for what changed) instead of rebuilding it from the
  whole dictionary (2.1–2.9 s at LUBM univ 8 per GAP_PLAN 9.1 6c). A full
  rebuild comes only once more than a quarter of the dictionary (at least
  4,096 terms) was posted that way, or when the dictionary shrank by more
  than a quarter.
- **The CSR build no longer reads every literal triple** (analytics#6). An
  in-memory store is read in place, predicate by predicate, copying only
  entity→entity pairs; an on-disk or RocksDB store is read one predicate at
  a time (`so_by_p`), keeping only its entity pairs — the literal pairs of
  one predicate are held at a time, never all of them.
- **A shutdown saves every free database first** (conc#5). `ApiServer`'s
  shutdown used to lock the databases in name order, so one a long read
  kept busy held up the save of every database after it; the busy ones are
  now waited for after the others are saved (and the read, cancelled, stops
  at its next check).
- **Background statistics refreshes are tracked and cancellable** (conc#9).
  The refresh `/api monitor` starts after a change computes its three passes
  (subject count, class statistics, schema graph) each under a shared guard
  of its own, so a queued writer waits for one pass instead of the whole
  refresh; it checks a cancel flag before each pass, a shutdown stops it and
  joins its thread before saving, and a refresh whose data changed between
  passes is dropped.
- **Server threads are joined after `serve_forever`** (conc#14).
  `Server::serve_forever` and `ApiServer::serve_forever[_tls]` join their
  connection workers (at most 2 s; a worker still serving a connection then
  is left to finish and logged), `ApiServer` joins its statistics refreshes,
  and the shared cancel-bridge thread ends once nothing is registered (the
  next registration starts another), so a program that stops a server and
  starts another in the same process keeps none of the old one's threads.
- **The substring index a database caches keeps candidates only**
  (gap-accel-soundness#3): no second copy of every term string and no
  string-sorted id list (about a third of the resident index: 32.4 % of
  8.2 MB on `testdata/lubm/lubm.nt` per the review). `StringIndex` now
  comes in two kinds: `new` / `build` / `insert` keep their strings
  (`search_contains`, `prefix`, `string_of` stay exact; `prefix` now checks
  the trigram candidates of the prefix instead of binary-searching a
  string-ordered list), and the new `StringIndex::candidates_only` /
  `build_candidates` keep postings and the id list only (`string_of` is
  `None`, `search_contains` and `prefix` return the candidates;
  `keeps_strings` tells which). `query::engine::dictionary_string_index`
  returns the candidates-only kind. The serde form of `StringIndex` changed
  (it is not persisted by any database).
- New library API: `Database::csr_cancellable` and
  `Database::string_index_cancellable` (`csr()` and `string_index()` keep
  their signatures), `CsrGraph::build_from_memory`, `csr::POLL_PAIRS`,
  `CsrCache::get_or_build_cancellable` (`get_or_build` keeps its signature),
  `query::engine::dictionary_string_index_cancellable`. Doc-hidden test
  hooks: `analytics::csr::build_hook`, `kvstore::string_index::build_hook`,
  `gapi::stats_refresh_hook`, and the counters
  `Database::string_index_builds`.
- Module docs: `kvstore::string_index` no longer calls the planner wiring a
  follow-up; `backend::rocks` no longer calls the disk B+ tree store's
  statistics estimates; `kvstore::vlist` describes the overflow page chain
  (disk#14).
- Tests: `tests/accelerator_soundness.rs` (the documented typed-literal
  cases; a battery comparing every pushed-down substring filter, both
  targets, needles from both forms, with its non-mandatory `|| false` twin
  on the memory, Stream, Materialize and — with `--features rocksdb` —
  RocksDB layouts; the database index keeps no string copies; incremental
  extension under inserts, frees and id reuse) and `tests/cancellation.rs`
  (builds parked at their first check by the hooks: `/cancel` of a plain
  read, an UPDATE's WHERE, a transaction snapshot read and a Stream-mode
  read stops the CSR build and caches nothing; a cancelled substring query
  caches no index; a reader does not wait for another's index build; a
  shutdown stops a build and saves every database; a busy database delays
  only its own save; a refresh parked between passes does not block a
  writer and stops at shutdown; no worker, refresh or bridge thread
  outlives `serve_forever`) and `tests/csr_build_memory.rs` (a counting
  allocator: the CSR build of an in-memory database with 200,000 literal
  triples and 2,000 entity edges peaks at about 89 KB above its start;
  before, it copied every triple first, 2.4 MB); unit tests in
  `analytics/csr.rs`, `kvstore/string_index.rs` and `server.rs`
  (`keep_alive_tests`).

## GAP_PLAN status line

none

## Docs notes

- README.md (HTTP server / keep-alive paragraph): under load — when
  connections wait for a worker — responses carry `Connection: close` and
  the connection closes after the response; an idle kept-alive connection
  still yields its worker to a waiting connection, after a 1 ms look for a
  request already on its way.
- README.md (graph functions / cancellation): `/cancel` and the shutdown
  grace period now also bound the CSR build of the advanced graph
  functions and the substring-index build of `CONTAINS`-style filters; a
  cancelled build caches nothing.
- docs/REFACTOR_BACKLOG.md — analytics#6 (remainder): `CsrGraph::build`
  (generic, `iter_all`) still materializes every triple; it is left for
  callers without a predicate list (`src/concurrent.rs`'s ConcurrentDb CSR
  provider, `src/pfn/graph.rs`'s test). An on-disk or RocksDB store's
  per-predicate read (`so_by_p`) still returns the whole predicate's
  `(s, o)` list, literal pairs included, before they are dropped: filtering
  inside the scan needs a streaming `TripleSource` method (e.g. a
  `for_each_so_by_p` callback) implemented by `DiskTriples`, `DiskSource`
  and `RocksStore`, outside this package.
- docs/REFACTOR_BACKLOG.md — db#5 (remainder): the incremental refresh
  walks the whole dictionary once (one identity check per term, no trigram
  work for unchanged terms) and keeps an O(1) copy-on-write clone of the
  in-memory dictionary as its comparison base, so the first write after a
  refresh copies the interner's index arrays (pointers, not strings). A
  dictionary change log (ids interned or freed since a mark) would make the
  refresh O(change) and drop the clone; it needs an addition to
  `src/dict/mod.rs`. The freed terms' postings stay until the next full
  rebuild (harmless: the FILTER checks every candidate).
- docs/REFACTOR_BACKLOG.md — conc#9 (remainder): a refresh is cancelled at
  pass granularity (`subject_num`, `type_statistics`, `schema_graph` have no
  internal checks); "skip starting a refresh while a writer waits" is not
  done (the standard `RwLock` exposes no waiting-writer count). A refresh
  whose data changed between two of its passes is dropped and the next
  `monitor` starts another, so under a steady stream of writes the
  statistics `monitor` / `schema` report may stay those of earlier data
  (with the current published figures) until the writes pause; a refresh
  that falls back to all three passes under one guard after a few dropped
  attempts would bound that.
- docs/REFACTOR_BACKLOG.md — conc#5 / conc#14 (remainder): a statement that
  does not poll its cancel flag still delays its own database's final save
  (the shutdown waits for it rather than skip the save: no data loss), as
  in D10 (timed write locks). Worker threads still serving a connection
  after 2 s are left detached and logged; `ShutdownReport` has no field for
  them.

## Measured numbers

All on the shared 4-vCPU / 15 GB machine while other packages' builds and
tests ran (load average 6-10), so the figures are indicative. Base =
b8b1c8e (sources extracted with `git archive`, built in a separate target
directory), branch = this package's head; release binaries from `cargo
build --release --locked -p gstore -p gstore-bench --bin gbuild --bin
gstore-bench`.

- Bench smoke gate (D9; string index, engine, CSR and the HTTP read path
  changed): `flock /tmp/gstore-bench.lock env GSTORE_BENCH_CACHE=…
  gstore-bench ci --baseline bench/baselines/smoke.json`, base and branch
  back to back, twice in opposite orders.
  - Every run: `Gates: PASS (456 checked, 0 failed)`. Calibration scores
    68.9 / 64.6 ms (run 1, base / branch) and 66.9 / 68.0 ms (run 2).
  - `gstore-bench compare base.json branch.json`, time geomean ratios
    (LUBM-like snapshot, disk, WatDiv-like snapshot, disk): run 1 (base
    first) 1.09, 1.14, 1.08, 1.03; run 2 (branch first) 0.99, 1.05, 0.87,
    0.99 — the order effect is larger than any difference; build and open
    rows, which this package does not touch, moved by up to 1.31 in run 1.
  - HTTP rows (report-only) are noisy both ways, e.g. run 2
    `snapshot/api/q1/c1` 11,515 → 8,764 qps, `disk/api/q1/c2` 8,029 →
    10,523 qps, `snapshot/sparql/q1/c2` 31,256 → 33,544 qps (the
    keep-alive change only differs when connections are queued, where
    the old code closed the connection after the response anyway).
- Substring index after writes (db#5; review repro shape: 60,000
  `"name number i of the review set"` literals, `FILTER(CONTAINS(?n,
  "number 4242 "))`, query cache off, opt-level 1 test profile, one
  scratch test run in each tree): first query (builds the index) base
  264.4 ms, branch 76.3 ms (the candidates-only build posts each form as
  it goes, no string copies); cached 3.1 / 3.1 ms; after an INSERT of a
  new term base 277.8 ms (full rebuild), branch 7.3 ms (extended); after
  a DELETE that frees terms base 264.0 ms, branch 5.2 ms.
- Index size (gap-accel-soundness#3): asserted in
  `tests/accelerator_soundness.rs::the_database_index_keeps_no_copy_of_the_strings`
  (2,000 IRIs + 2,000 literals): the database index is under 3/4 of an
  index keeping the same strings, with the same candidates.
- Cancellation latency (conc#5): deterministic, not timed — a parked
  build observes the raised flag at the check right after the park
  (`tests/cancellation.rs`); checks come every predicate, every 65,536
  pairs of an in-memory predicate and every 1,024 dictionary terms.
- `scripts/conformance.sh --no-build` (dev binaries of this branch, TMPDIR
  in the package's scratch directory): all six suites pass (basic, update,
  transaction, api, parser with the known failing cases 15 and 17, python
  49/49); no compat switch was needed.
- Test suites: `cargo test --workspace` with GSTORE_DISK_QUERY_MODE=memory
  and =stream: 2,557 passed, 0 failed each. `--features rocksdb`: clippy
  clean; lib (1,286), accelerator_soundness, cancellation,
  string_index_planner, dt_csr, dt_rocksdb, dt_gfun_e2e, dt_reclaim,
  dt_persist_backends pass. `--features tls`: clippy clean;
  http_framing_deadlines, http_preauth_limits, dt_gapi_tasks,
  cancellation and the server/http_api/tls lib tests pass.

## Requests/Not yet

- Edits outside the owned files and the granted functions (for the
  integrator):
  - `tests/csr_build_memory.rs` (new, analytics#6): a separate test binary
    because it installs a counting global allocator; nobody else edits it.
  - `src/db/mod.rs`: the `StringIndexCache` struct and its `Debug` impl,
    plus the new `StringIndexSlot`, `CachedStringIndex`,
    `StringIndexCache::lock` and `STRING_INDEX_STALE_MIN` next to it (the
    cache redesign behind the granted `string_index` / `touched` /
    `cached_string_index`); the new `string_index_builds` (doc-hidden)
    after `cached_string_index`.
  - `src/server.rs`: `run_accept_loop` now returns `(bool, WorkerThreads)`
    (its only callers are the four `serve_forever` functions, all this
    package's) and closes the pool instead of dropping it; new
    `WorkerThreads`, `THREAD_JOIN_WAIT` and `join_workers` next to
    `WorkerPool`; new `QUEUED_LOOK` after `IDLE_POLL`; new
    `#[cfg(test)] mod keep_alive_tests` after `wait_for_request`.
  - `src/http_api.rs`: new `ApiServer::join_threads` after
    `serve_forever_tls`. `serve_forever` / `serve_forever_tls` here do not
    yet carry r2-txn's sanctioned reaper-guard line: keep both sides.
    `finish_shutdown` still calls `ApiState::remember_ended` with its
    current signature (moved into a closure with the rest of the per-database
    block).
  - `src/gapi/mod.rs`: two re-export lines after `pub use envelope::…`
    (`stats_refresh_hook`, doc-hidden; `stop_stats_refreshes`, crate).
  - `src/gapi/ops_db.rs`: new `DoneLatch`, `Refresh`, `refresh_stats`,
    `stats_refresh_hook`, `StoppingRefreshes`, `stop_stats_refreshes` next
    to the granted `refreshing` / `refresh_stats_in_background`;
    `compute_stats` and `class_stats` are unchanged.
  - `src/query/engine.rs`: new `dictionary_string_index_cancellable`,
    `refresh_string_index`, `index_substring_forms`, `substring_forms`,
    `STRING_INDEX_POLL` next to the granted `dictionary_string_index`.
  - `src/backend/rocks.rs`, `src/kvstore/vlist.rs`: one module-doc
    sentence each (disk#14).
- Not done: see the REFACTOR_BACKLOG notes above (analytics#6 streaming
  scan, db#5 change log, conc#9 writer-waiting check and pass granularity,
  conc#5/conc#14 non-polling statements and detached workers).
- For a later round: `src/concurrent.rs`'s CSR provider still builds with
  the uncancellable `CsrGraph::build` (r4-concurrent owns the file); it
  could use `CsrGraph::build_from_memory` with its request's flag.
