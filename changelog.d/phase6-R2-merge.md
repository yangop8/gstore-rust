# Phase 6 R2 merge — integration of R2-DISKLIB, R2-DICT and R2-BULK

Merged in plan order on `phase6` from `aaea9f8`: `p6/r2-disklib` (174898d),
`p6/r2-dict` (33305d8), `p6/r2-bulk` (0969a2a), each with `git merge
--no-ff`. All three merged without conflicts and built at every merge
point; every changed file is inside its package's §6 file list and the §7
ownership table (no ownership exceptions were reported or needed). The
commits after the merges are the integrator's own, listed below.

## Changelog

- **The legacy `&str` dictionary API fails closed on an out-of-core
  dictionary (contract C4).** `Dictionary::id_to_string` /
  `predicate_to_string` on a backed dictionary now answer `None` and record a
  read error for every id (C2), and never read the backend. R2-DICT had kept a
  bounded per-dictionary pin table (up to 4,096 strings) only because a test in
  `src/kvstore/store.rs`, owned by R2-DISKLIB this round, expected `Some` there;
  that test now uses `term()` / `pred()`, and the pin table and
  `CacheStats::legacy_pinned` are gone. In-memory dictionaries are unchanged,
  and no production path reaches the legacy API with a backed dictionary
  (backed dictionaries exist only on `DiskStore`'s out-of-core query path).
  This supersedes the "pin … then fail closed" wording in
  `phase6-R2-DICT.md` (its Changelog bullet and its docs note on the legacy
  API) when the fragments are folded.
- **`DiskStore::query` fails on a read error.** It evaluates over
  `DiskSource` (R2-DISKLIB) with its out-of-core `DiskDict` dictionary
  (R2-DICT) inside a `ReadScope`: a read error of a triple index or of the
  dictionary fails the query with "a read from the store failed while
  evaluating the query: …" instead of answering partially. Healthy answers
  and their order are unchanged. The doc of `impl TripleSource for DiskStore`
  now says that impl does not report errors (use `DiskSource` or
  `DiskTriples`).
- **New test `tests/dt_r2_integration.rs`** (3 tests) runs the three
  packages together on bulk-built stores:
  - the out-of-core dictionary over a bulk-built LUBM `kvstore.kv` resolves
    every id of every space to the memory build's term and back
    (`resolve_many` included), and `materialize()` gives a dictionary whose
    `dict.bin` encoding is byte-identical to the memory build's;
  - Q1–Q14 over `DiskTriples` with the out-of-core dictionary, over a
    `DiskReadView` snapshot (`DiskSource`) and through `DiskStore::query`
    answer the memory build's rows in the same order, with no read error;
  - a transaction on the bulk-built store: read-your-writes through the
    out-of-core dictionary, nothing in the pager or the snapshot before
    commit, rollback moves no epoch, commit expires the old view and survives
    a reopen;
  - `DiskStore::query` fails on a corrupted POS root and on a corrupted
    id2entity root, and the error stays with its statement.

## GAP_PLAN status line

none (integration of the R2 packages; their own fragments carry the status
lines).

## Docs notes

- docs/STORAGE_BACKEND.md (with R2-DICT's C4 notes): the legacy
  `id_to_string` / `predicate_to_string` answer only for in-memory
  dictionaries; on an out-of-core dictionary they return `None` and fail the
  statement. Use `term()` / `pred()`.
- docs/STORAGE_BACKEND.md (with R1-CORE's C2 notes): `DiskStore::query` runs
  in its own read scope and fails on a read error; `DiskStore`'s own
  `TripleSource` impl still turns read errors into empty answers, so
  evaluate over `DiskSource` or `DiskTriples`. This supersedes the "kept:
  `DiskStore::query` … turn read errors into empty answers" note in
  `phase6-R2-DISKLIB.md`'s Requests.

## Measured numbers

None of the integrator's changes has its own benchmark: the legacy-API change
only removes a table no production path used, and `DiskStore::query` makes
the same `try_*` calls through `DiskSource` as before. The round's
acceptance numbers at univ 8 / 38 are the measure stage's (the package
fragments list the commands).

Verification of the merged tree (4 vCPU VM, other agents' jobs running,
load average about 3.2; `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
CARGO_INCREMENTAL=0`):

- `cargo fmt --all -- --check`, `cargo build --workspace --all-targets`,
  `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace --no-fail-fast`: 2,159 passed, 0 failed, 2 ignored
  (R2-DISKLIB's soak test and gstore-bench's `micro_on_disk`).
- `cargo clippy -p gstore --all-targets --features F -- -D warnings` clean
  and `cargo test -p gstore --features F` for gzip (1,959 passed), tls
  (1,960), zip (1,972), readline (1,956), pfn-wasm (1,977); 0 failed, 1
  ignored each.
- rocksdb: clippy clean; `cargo test -p gstore --features rocksdb` 1,979
  passed, 0 failed, 2 ignored (the soak test and a child-process helper);
  `cargo test -p gstore-bench --features rocksdb` 44 passed, 1 ignored.
- `scripts/conformance.sh`: all six suites pass (parser: only the known
  cases 15 and 17 differ; Python SDK 49/49).
- `flock /tmp/gstore-bench.lock env GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
  cargo run --release -p gstore-bench -- ci` against
  `bench/baselines/smoke.json` (`e859f7e`, univ 1): **PASS, 193 gates
  checked, 0 failed**, all result bodies identical. Ratios to the baseline:
  query geomean 0.50 (snapshot) and 0.56 (disk); build 0.66 (snapshot) and
  **0.02 (disk: 32.3 s → 0.49 s, the bulk build)**; disk build peak RSS 0.51;
  disk directory bytes 0.75; warm open 1.37 (snapshot, 26.8 → 44.3 ms) and
  0.36 (disk); RSS after open 0.96 / 0.88; snapshot bytes identical.
- The snapshot warm-open ratio of that single run was checked with a
  same-session A/B (`gstore-bench run --profile smoke --backends
  snapshot,disk --no-update --no-conc`, base = the `aaea9f8` binaries in
  /tmp/gstore-base-aaea9f8/bin, alternated twice): snapshot warm open 27.2 /
  30.0 ms (base) vs 33.5 / 32.0 ms (merged), first open 27.8 / 38.6 vs 42.0
  / 31.7 ms, RSS after open 22.5 / 22.3 vs 19.7 / 19.4 MiB; disk warm open
  44.1 / 41.6 vs 43.1 / 35.5 ms, disk build 10.6 / 11.6 s vs 0.53 / 0.49 s.
  So at univ 1 the snapshot open is about 1.1–1.2× within noise of a few
  milliseconds (R2-DICT measured 0.80× at univ 8); the measure stage's univ 8
  / 38 reference runs decide it.
- `smoke.json` is **not re-blessed**: plan §5.6(d) needs ≥ 1.5× on every
  smoke metric (R1-BENCH's reading), and snapshot open, RSS, bytes and
  several single queries did not improve that much.

## Requests/Not yet

Handled here (Round-2 requests that connect the packages):

- R2-DICT request 1, first part: `lazy_dictionary_resolves_term_and_id_on_demand`
  uses `term()` / `pred()`, and the legacy pin table is gone (C4 exactly).
- R2-DICT request 1, second part, together with R2-DISKLIB's "never over a
  bare `DiskStore`": `DiskStore::query` runs in a `ReadScope` over
  `DiskSource`.
- Tests that need two or three packages: `tests/dt_r2_integration.rs`.

Left for later rounds (owner in bold; the package fragments give the
details):

- **R3-STREAM:** wire `Backend::Disk(DiskTriples)` (R2-DISKLIB's list:
  `insert_triple` / `remove_triple` and `touched()` on `Ok(true)`,
  `take_error`, begin / commit / rollback, `named_op`, `commit_point`,
  `mark_vstree_fresh`); `DiskDict::intern` through the shared pager without the
  gate and bumping the generation (R2-DICT request 2), then `disk_backing()`
  checks `DiskTermSource::generation()` instead of the counts; delegate
  `Database::build_disk*` and `/api build` to `bulk::build` (R2-BULK request
  1; the plan's R2-BULK brief, item 7, and D15 assign the delegation to R3,
  and a server must unload a database before rebuilding it). For Stream mode, open one
  `DiskDict::open_shared(dt.shared_pager())` per database and reuse it across
  statements and snapshot views, as `dt_r2_integration.rs` does:
  `DiskStore::lazy_dictionary()` on a `DiskReadView`'s per-call store would
  start with a cold cache on every view.
- **R3-ROCKS:** `BulkTarget::RocksDb` (R2-BULK request 2).
- **R4-SNAP:** `/begin` on a Stream database (R2-DISKLIB's list).
- **R4-RECLAIM:** `DiskDict::free` and holes (`slot()` bounds), the free
  stack as a lowest-id-first set with the same encoding, `FREE_IDS` in the
  bulk image (R2-DICT requests 2–3, R2-BULK request 5).
- **R4-CLOSURE:** gbuild's `memory:` / `spills:` summary in the bench JSON
  (R2-BULK request 4); the docs notes of all three fragments, including
  R2-BULK's install order, the 2,038-byte key limit and
  `VsTree::write_disk_sorted`.
- **Not in a Phase 6 package:** an N-Triples fast path or chunked parallel
  parse for `.nt` (parser owner, R2-BULK request 3); `GraphCtx::term_str` /
  `pred_str` returning `TermStr` (pfn-wasm owner, R2-DICT request 4); the two
  legacy calls in `src/analytics/gfun/mod.rs` (R2-DICT request 5). Both are
  over in-memory dictionaries today, so they keep working; with a backed
  dictionary they would now fail closed instead of pinning strings.
- **Engine owners:** long posting lists in Stream mode (Q3 2.4×, Q2 1.8× at
  univ 8, R2-DISKLIB) are input to R3's default-mode decision.
- **Measure stage:** all univ 38 acceptance items of the three packages and
  the §5.6(c) univ 8 reference re-run, including the snapshot warm open
  (above: about 1.1–1.2× at univ 1, within a few milliseconds of noise).
- **Observed, not in Round 2's files:** `tests/dt_gapi_db.rs` never removes
  its per-test server roots (`tmp_root`, `/tmp/gstore_gdb_<tag>_<pid>_<n>`);
  `the_upstream_api_test_script_runs_unmodified` leaves about 26 MB (LUBM
  export and backup) per run, and 164 such directories (4.1 GB) had piled up
  in `/tmp` on this machine. Removing the root when `Srv` drops would fix it.
