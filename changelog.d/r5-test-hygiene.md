# r5-test-hygiene — Test-suite hygiene: shared helpers with timeouts and drop-guards, unique temp paths, per-test counters, hang-proof harnesses

## Changelog

Behaviour changes first. No user-visible behaviour changes; one
doc-hidden library addition and test-suite changes.

- **`ApiServer::expired_snapshot_reads()`** (`#[doc(hidden)]`, review
  tests-b#10): how many reads of a transaction's on-disk snapshot *this
  server* saw expire and re-ran on the live database. The process-wide
  `gstore::http_api::expired_snapshot_reads()` stays; a test reading it also
  counted the expiries of every other server its binary runs in parallel.
- **`tests/common/`** (review tests-a#16, tests-a#8, tests-b#11, tests-b#16):
  one shared copy of the helpers some 45 test files had copied and let
  drift apart, included with `mod common;`:
  - `tmp` — unique temporary paths `<temp_dir>/gstore_<tag>_<pid>_<n>`, a
    `TempDir` removed on drop, and the thread-scoped registry the `TmpDirs`
    copies implemented (both remove their paths also when a test panics);
  - `http` — an HTTP/1.1 client with **one overall deadline per request**
    (connect, write and read together; default 120 s): a hung server fails
    the test with the request line and the bytes that arrived, instead of
    blocking in `read_to_end` until the CI job's 75-minute timeout (a
    per-read timeout alone does not bound a peer that trickles bytes);
  - `server` — `Running<S>`: an in-process `ApiServer` or `Server` on a
    thread of its own, **shut down and joined when the guard drops**, within
    60 s; a server whose shutdown hangs fails the test (or, while a test is
    already failing, is reported and left behind) instead of keeping the
    binary alive;
  - `globals` — `LoggerReset` and `DefaultOpenOptions`, guards that put the
    process-wide diagnostic logger and default open options back when
    dropped, also on failure;
  - `alloc_counter.rs` (included by path; a binary has one global
    allocator) — per-thread bytes held, peak and allocated, for tests that
    bound memory or work by bytes rather than wall-clock time.
- **`tests/test_helpers_fail_instead_of_hanging.rs`**: the helpers' own
  tests — a request to a silent peer and to a trickling peer fails within
  its deadline naming the request; a deliberately stuck server makes its
  guard's drop panic after the stop wait; a real `ApiServer`'s guard closes
  its listener; temporary paths are unique and removed, also after a panic.
- **No fixed temporary paths** (review db#22, disk#13, tests-a#6, tests-b#9,
  conc#16): `src/db/mod.rs`'s tests (`gstore_ut_<tag>`, two fixed `.nt`
  files), `src/signature/disk_vstree.rs`'s and `vstree.rs`'s
  (`gstore_diskvs_<tag>.kv`, `gstore_vstree_disk_<tag>.kv`), `src/kvstore/
  store.rs`'s `gstore_disk_formats_ut`, and `tests/dt_cli.rs`,
  `dt_persist_backends.rs`, `dt_graph.rs`, `dt_lubm.rs`, `dt_parity.rs`,
  `dt_persist.rs`, `dt_small.rs`, `dt_update.rs` now use pid- (and
  counter-) unique names, removed also when a test fails. Two test runs
  sharing `/tmp` (two checkouts, two agents) no longer remove, lock and
  rebuild each other's databases (see Measured numbers).
- **Servers stop and clients time out** in `dt_gapi_core`, `dt_gapi_users`,
  `dt_reclaim_concurrency`, `dt_rw_server_stress`, `dt_settings_phase5`,
  `dt_http_parity`, `dt_gapi_async_wiring` (servers under `Running`) and in
  `dt_cross_phase5_api`, `dt_logging`, `dt_phase0_review`,
  `dt_phase0_wiring` (requests through `tests/common/http`) (review
  tests-a#8, tests-b#11); the seven remaining clients that read with no
  timeout at all (`dt_http_durability`, `layout_replacement`,
  `login_lockout`, `password_changes_do_not_block_other_requests`,
  `query_limits`, `rocksdb_checkpoint_resources`: 120 s;
  `dt_gserver_lifecycle`'s keep-alive check: 30 s, as its other client)
  got a read timeout, so no HTTP read in the suite blocks without a bound.
- **Corrected assertions** (each checked against deliberately broken code,
  see Measured numbers):
  - `dt_bulk_import` blank-node scoping compares the rows with the memory
    build's and pins them (`COUNT(DISTINCT ?s)` = 2 in the default graph,
    `"one"`, `"two"`, the GRAPH query `"three"`), not row counts (review
    tests-a#4; the review expected 3 subjects, counting g.nq's `_:x`, which
    is in graph `_:g`, not in the default graph);
  - `dt_rw_server_stress`'s `/api` test requires at least one acknowledged
    two-operation write (it runs until there is one, at most a minute)
    (review tests-b#19);
  - `dt_snapshot_disk`'s commit test waits for *its server's* expired-read
    count (review tests-b#10);
  - `dt_disk_stream`'s corpus differential test reads its own corpus files
    (`tests/dt_disk_stream_corpus/{sparql11,semantics,syntax}.txt`, taken
    from what its lexer found in `dt_sparql11.rs`, `dt_sparql_semantics.rs`
    and `dt_sparql_syntax.rs`; the lexer is gone), no query may fail in
    every mode, and each corpus needs a floor of queries with a non-empty
    answer (22 of 23, 14 of 17, 9 of 13) (review tests-a#17);
  - `dt_phase4_review_fixes` bounds work by bytes instead of wall-clock
    time: the PFN combination count holds < 256 KiB, the wide-argument
    expansion allocates < 2.5× for 2× the keys, the `emit_json_str` bound
    holds < 192 MiB (pfn-wasm) (review conc#16);
  - `dt_gapi_users`'s log tests run once more when the local date changed
    while they ran (`on_one_day`, checked on a fake clock), with every
    assertion as strict as before (review tests-a#15);
  - `dt_bulk_rocks`'s FIFO feeder cannot hang the test (the test opens the
    FIFO's read end itself after the build and awaits the feeder with a
    hang guard), and the rebuild after releasing `rocksdb/LOCK` retries a
    refusal from a concurrently forked child, as `dt_bulk_crash` does
    (review tests-a#12);
  - `dt_settings_phase5`, `dt_logging`, `dt_merge4_wiring` reset the
    process-wide logger, and `dt_rw_server_stress` the default open
    options, with a guard, also when they fail (review tests-b#16);
  - `dt_cross_phase_api` and `dt_cross_phase5_api` remove their scratch
    data directories (under `target/tmp`, one pair per run before), and so
    does `dt_phase4_wiring` (four per run) (review tests-a#10).

## GAP_PLAN status line

none

## Docs notes

- **README.md** (the testing paragraph after "Conformance is tested
  against upstream's own material"): new integration tests use
  `tests/common` (`mod common;`) for temporary paths (`tmp::TempDir`,
  `tmp::thread_dir`), HTTP requests (`http::request`, an overall deadline
  per request) and in-process servers (`server::Running`, stopped and
  joined on drop); no fixed temporary names, no client read without a
  deadline, no server left running; bounds on memory or work by bytes
  (`tests/common/alloc_counter.rs`) rather than wall-clock time.
- **docs/REFACTOR_BACKLOG.md** — test-suite hygiene left after this
  package (review tests-a#16, tests-a#8, tests-b#11; D3: no migration for
  its own sake, the files below are not named by the review ids):
  - 35 copies of the `TmpDirs` guard remain (`dt_bulk_disk`, `dt_cli_compat`,
    `dt_csr`, `dt_dict_backends`, `dt_disk`, `dt_disk_stream_crash`,
    `dt_disk_stream_txn`, `dt_gapi_async_wiring`, `dt_gapi_tasks`,
    `dt_graph`, `dt_gserver_lifecycle`, `dt_hardening`,
    `dt_http_durability`, `dt_http_parity`, `dt_load_import`, `dt_logging`,
    `dt_lubm`, `dt_merge4_fixes`, `dt_overlay`, `dt_p1_review`,
    `dt_parity`, `dt_persist`, `dt_pfn`, `dt_phase0_review`,
    `dt_phase0_wiring`, `dt_phase4_review_fixes`, `dt_r2_integration`,
    `dt_rdf_import`, `dt_reason_manage`, `dt_rocksdb`,
    `dt_security_review`, `dt_small`, `dt_sparql_semantics`, `dt_update`,
    `dt_zip_io`); they are pid-unique and remove their paths, so only the
    duplication is left.
  - About 55 test files hand-roll an HTTP client with a per-read timeout
    only (the seven that read without any timeout — `dt_http_durability`,
    `layout_replacement`, `login_lockout`,
    `password_changes_do_not_block_other_requests`, `query_limits`,
    `rocksdb_checkpoint_resources`, one site of `dt_gserver_lifecycle` — got
    a read timeout in this package's review); about 60 files spawn
    `serve_forever` without a guard that joins it. Move them to `tests/common/http` and
    `tests/common/server`.
  - `tests/upstream_scripts/harness.rs` (15 binaries): `Server`'s drop asks
    the server to stop but does not join it, `http_with` has a 600 s
    per-read timeout, `scratch` directories are removed only where a test
    registers them, and `scratch_fixed` uses fixed names under
    `CARGO_TARGET_TMPDIR` (shared by two runs of the same checkout).
  - Eight test files carry their own counting global allocator
    (`csr_build_memory`, `dt_gfun_output_memory`, `gapi_download_policy`,
    `gfun_limits`, `optional_partial_indexes_are_bounded`,
    `rdf_parser_conformance`, `rdfxml_literal_canonical_form_is_bounded`,
    `sparql_path_multiplicity_is_bounded`); `tests/common/alloc_counter.rs`
    can replace them.
  - `dt_phase4_review_fixes::runtime::host_calls_on_large_strings_are_charged_and_cancellable`
    (pfn-wasm) keeps two 5 s wall-clock bounds inside its 20 s channel
    guard (review conc#16 named the 1 s and 2 s bounds, now byte counts).
- **docs/STORAGE_BACKEND.md**, **docs/DEPLOY.md**: nothing (no format
  change, no compat switch).

## Measured numbers

Machine: 4 vCPUs, 15 GB, other agents' builds running at the same time.

**Concurrent runs sharing one `TMPDIR`** (the collisions of review db#22,
disk#13, tests-a#6, tests-b#9; `concurrent.sh` = N copies of a test binary
at once, R rounds, one shared `TMPDIR`), base = the binaries of
1f0ed8b, branch = this package:

| binary (tests) | rounds × copies | base failed | branch failed | left in TMPDIR (base / branch) |
|---|---|---|---|---|
| dt_cli (gbuild_then_gquery_roundtrip, gquery_update_persists_to_disk) | 5 × 4 | 20 of 20 | 0 of 20 | 0 / 0 |
| dt_persist_backends (all 19) | 5 × 3 | 15 of 15 | 0 of 15 | 2 / 0 |
| gstore lib (db::tests, disk_vstree, vstree, store dispatch) | 8 × 3 | 23 of 24 | 0 of 24 | 2 / 0 |

**Corrected assertions against deliberately broken code** (env-gated
breaks in a local build, never committed; "old" = the test as of
1f0ed8b, "new" = this package's):

| id | break | old test | new test |
|---|---|---|---|
| tests-a#4 | every document's blank nodes in one shared scope (both builds) | passes | fails (COUNT 1, not 2) |
| tests-b#19 | non-transactional `/update` on `ApiServer` always 409 | passes | fails (no acknowledged pair) |
| tests-b#10 | the long read of db `l` cancelled instead of expired at commit | passed 2 of 3 runs (masked by the parallel tests' expiries) | failed 3 of 3 |
| tests-a#17 | `Database::query` fails for every statement | passes | fails (query failed in every mode) |
| conc#16 | the first 2000 combinations built before the count | passes | fails (bytes held) |
| conc#16 | a quadratic allocation (n/8 vectors of growing size) in `evaluate` | passes | fails (ratio > 2.5) |
| conc#16 | `emit_json_str` builds the escaped copy first (pfn-wasm) | passes | fails (bytes held) |
| tests-a#12 | `bulk::build` refuses its FIFO input at the start (rocksdb) | hangs (killed by a 90 s timeout) | fails in 1 s |

**Byte bounds of conc#16** (measured): 4,457 bytes held while 4^10
combinations are counted and refused (bound 256 KiB); 31,229,899 bytes
allocated for 65,000 keys and 62,713,883 for 130,000 (ratio 2.01, bound
2.5); 67,135,507 bytes held by the over-long `emit_json_str` (bound 192
MiB; an escaped copy would add 384 MiB).

**tests-a#10**: after one run of `dt_cross_phase_api`, `target/tmp` held
`upstream_xphase_cache_data_<pid>_1` and `upstream_xphase_priv_data_<pid>_0`
at base, nothing now.

**Bench smoke gate**: not run. No hot path changed: the one non-test
change is a relaxed counter increment next to the existing one, executed
only when a transaction's snapshot read expires (a commit under a running
non-transactional read), which the smoke workload does not do.

**Full runs** (this branch, `cargo +1.99.1`, other agents' builds running):

| command | result | left in the private `TMPDIR` |
|---|---|---|
| `GSTORE_DISK_QUERY_MODE=memory cargo test --workspace` | 3,099 passed, 0 failed, 10 ignored | nothing |
| `GSTORE_DISK_QUERY_MODE=stream cargo test --workspace` | 3,099 passed, 0 failed, 10 ignored | nothing |
| `cargo test -p gstore --features rocksdb` (shared rocksdb target dir) | 2,965 passed, 0 failed, 10 ignored | nothing |
| `cargo test -p gstore --features pfn-wasm --test dt_phase4_review_fixes` | 17 passed | — |
| `cargo clippy --workspace --all-targets -- -D warnings`; the same for `-p gstore --features rocksdb` and `--features pfn-wasm`; `cargo doc -p gstore --no-deps` with `-D warnings`; `cargo fmt --all -- --check` | clean | — |

After a full workspace run, `target/tmp` holds only
`harness::scratch_fixed`'s `upstream_bfs_*` directories (fixed names by
design, emptied by the next run); the first run also left
`dt_phase4_wiring`'s four `upstream_p4w_*` scratch directories, which that
test now removes (the same defect as review tests-a#10).

## Requests/Not yet

- **Edits outside the owned files** (the smallest change each; no other
  round-5 package edits these):
  - `src/http_api.rs`: one field `expired_snapshot_reads` at the end of
    `struct ApiInner` and its initialiser in `ApiServer::bind`, one
    increment line in `ApiInner::read_committed` (next to the global
    counter's), and the new method `ApiServer::expired_snapshot_reads`
    after `task_manager()` (review tests-b#10).
  - `src/signature/vstree.rs` (`tests::diskvs_tmp`) and
    `src/kvstore/store.rs` (`tests::build_files_dispatches_on_extension`):
    pid-/counter-unique names (review disk#13).
  - Tests named by the review ids but not in this package's list:
    `dt_graph`, `dt_lubm`, `dt_parity`, `dt_persist`, `dt_small`,
    `dt_update` (temp names); `dt_http_parity`, `dt_gapi_async_wiring`
    (server guard; `dt_http_parity` also the client); `dt_cross_phase5_api`
    (client, scratch removal); `dt_logging`, `dt_phase0_review`,
    `dt_phase0_wiring` (client); `dt_logging`, `dt_merge4_wiring` (logger
    guard); `dt_phase4_wiring` (scratch removal); `dt_http_durability`,
    `layout_replacement`, `login_lockout`,
    `password_changes_do_not_block_other_requests`, `query_limits`,
    `rocksdb_checkpoint_resources`, `dt_gserver_lifecycle` (one
    read-timeout line after each connect that had none, review tests-b#11;
    added in the package's review).
  - Merge note: `fix/r5-test-audit` also edits `src/http_api.rs`
    (`DbSlot` save, `ApiInner::dispatch`/`handle_update` parse hooks,
    backup directory creation); the regions are disjoint from the lines
    above.
- **Partial / not yet**: tests-a#16 is fixed for the files the ids name
  and this package's own files (22 test files use `tests/common`); the
  remaining copies, clients and harness are listed for
  docs/REFACTOR_BACKLOG.md above (D3). The tests-a#12 lock-release race
  (a fork inside a microsecond window) was not reproduced; the retry
  mirrors `dt_bulk_crash`'s. db#22's `src/db/fsutil.rs` half was already
  pid- and counter-unique (b8eff5e, round 2).
