# r3-integration — Round 3 merge record: cross-package wiring

## Changelog

Behaviour changes first:

- **`BNODE(str)` in SELECT expressions keeps one node per solution**
  (review query#5 meets query#6; r3-sparql-functions' per-solution
  `BNODE(str)` and r3-sparql-joins' left-to-right SELECT expressions).
  r3-sparql-joins extends each solution by the SELECT expressions left to
  right, appending each alias column (unbound) before its expression runs;
  r3-sparql-functions identified a solution for `BNODE(str)` by its whole
  binding row. So `SELECT (BNODE(?v) AS ?a) (BNODE(?v) AS ?b) …` gave two
  different nodes in one solution: the second call saw one more (unbound)
  column. The solution key now ignores trailing unbound slots (an unbound
  column is no part of the solution mapping; columns only grow at the end
  while a solution is evaluated). Tests: `tests/sparql_builtins.rs::
  bnode_of_a_string_is_one_node_per_solution` (r3-sparql-functions' own
  test, failing on the merged tree) and `tests/bnode_in_select_expressions.rs`
  (the materializing path, `ORDER BY`, the `LIMIT` fast path, `ORDER BY` +
  `LIMIT` top-k, `DISTINCT`, an unbound alias in between, a `BIND` next to a
  SELECT expression; fails before).
- **A paged file another process holds is a lock conflict, not an
  unclassified failure** (review disk#9 meets design#3). r3-storage-kernel's
  advisory lock in `Pager::open` refused such a file with the untyped
  `GStoreError::Database`, which r3-rest-parity's classification reads as
  `ErrorKind::Other` (each endpoint's fallback status). It is now
  `GStoreError::Locked` (`ErrorKind::Locked`: `409` on every REST endpoint,
  like the directory locks); the message is unchanged ("'…' is in use:
  another process has it open"). Test:
  `tests/storage_kernel_regressions.rs::a_pager_file_held_by_another_process_is_refused`
  asserts the kind (fails before).

No standalone `upstream-compat` startup line or `scripts/conformance.sh`
compat block came with round 3: no package added a compat switch, so there
was nothing to fold into `GStoreConfig::upstream_compat_warnings` (convention
13c) and the harness still turns on exactly round 1's two switches.

## GAP_PLAN status line

none

## Docs notes

- docs/DEPLOY.md (closing step): round 3 adds no compat switch; the new
  limit keys `[upload] rdf_max_token_size` and `[upload] sparql_max_size`
  (r3-parser) are tuning keys, not compat switches.
- docs/REFACTOR_BACKLOG.md: nothing new from the integration; every
  package's backlog notes stand as written in its fragment.
- CHANGELOG.md (closing step): r3-rest-parity's fragment says the
  single-database server's `/update` stopped by `query_timeout` answers
  `503`, r2-limits-auth's says `400`; `503` is current (r2-integration made
  it so and r3-rest-parity's mapping keeps it).

## Measured numbers

Merge: the seven package branches merged with `git merge --no-ff` in the
given order (`fix/r3-sparql-paths`, `fix/r3-sparql-joins`,
`fix/r3-sparql-functions`, `fix/r3-rest-parity`, `fix/r3-db-redo`,
`fix/r3-parser`, `fix/r3-storage-kernel`) without a textual conflict: each
merge commit's tree is the one `git merge-tree --write-tree` computes for
its two parents. The expected overlaps (r3-sparql-paths' `stream_join` /
`stream_path_join` calling r3-sparql-joins' `Probe`, whose signatures it
kept; r3-sparql-functions' `begin_execution` first lines of `evaluate` and
`eval_update_modify`; r3-storage-kernel's `..Default::default()` lines in
r3-parser's `src/config.rs`) merged cleanly with both sides kept. Four
branches (`fix/r3-sparql-paths`, `-sparql-joins`, `-sparql-functions`,
`-parser`) fork from 0f74615, before the round-2 review fixes
(0f74615..4f0e075, none of which touches `src/query/` or the parser); their
merges keep those fixes (the review fixes' tests are in the runs below).
`tests/w3c_subset.rs` passed after the merge (convention 13e; no `xfail`
case turned into an XPASS); `tests/executor_differential.rs` does not exist
yet (r4-sparql-guard). On the merged tree before the wiring,
`sparql_builtins::bnode_of_a_string_is_one_node_per_solution` failed (the
`BNODE` fix above).

Failing-before checks of the two wiring commits: with `src/query/engine.rs`
and `src/kvstore/pager.rs` reverted to the merge head (41cb660),
`cargo test --test bnode_in_select_expressions --test sparql_builtins
--test storage_kernel_regressions` gave 0 passed / 1 failed, 18 / 1
(`bnode_of_a_string_is_one_node_per_solution`) and 9 / 1
(`a_pager_file_held_by_another_process_is_refused`); restored, 1, 19 and
10 passed, 0 failed.

Full CI matrix at the integration head (4-vCPU machine, at most two heavy
jobs at once; toolchain 1.99.1, `CARGO_PROFILE_{DEV,TEST}_DEBUG=0`):

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `RUSTDOCFLAGS='-D warnings' cargo doc -p gstore --no-deps --locked`:
  clean.
- `cargo test --workspace`: `GSTORE_DISK_QUERY_MODE=memory` 179 test
  binaries, 2913 passed, 0 failed, 8 ignored;
  `GSTORE_DISK_QUERY_MODE=stream` 2913 passed, 0 failed, 8 ignored.
- Feature entries (clippy `-D warnings` and `cargo test -p gstore
  --features …`, each in a fresh target directory, deleted afterwards):
  gzip 2689 passed; tls 2695 passed; zip 2701 passed; readline 2685
  passed; pfn-wasm 2707 passed; `zip pfn-wasm readline` 2723 passed — all
  167 test binaries each, 0 failed, 7 ignored, clippy clean; nothing left
  in the runs' TMPDIR.
- RocksDB (shared target directory, gstore compiled from this tree):
  `cargo clippy -p gstore --all-targets --features rocksdb -- -D warnings`
  clean; `cargo test -p gstore --features rocksdb` 167 test binaries, 2774
  passed, 0 failed, 8 ignored; `cargo test -p gstore-bench --features
  rocksdb` 69 passed, 0 failed, 1 ignored.
- `scripts/conformance.sh` (TMPDIR in the scratch directory): all six
  suites pass (basic, update, transaction, api, parser with the known
  failing cases 15 and 17, python 49/49); `check_compat_switches` saw both
  round-1 switches reported.
- Bench smoke gate: `cargo build --release --locked -p gstore -p
  gstore-bench --bin gbuild --bin gstore-bench` clean; `gstore-bench report
  --check README.md CHANGELOG.md` clean; `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=… target/release/gstore-bench ci --baseline
  bench/baselines/smoke.json`: PASS, 456 gates checked, 0 failed (148 table
  rows); time geomean vs the committed baseline: LUBM-like snapshot 0.91,
  disk 0.94, WatDiv-like snapshot 0.97, disk 0.98 (bound 1.50);
  calibration score 68.36 ms. Machine state: the tls feature test ran in
  the other slot (load average 5.2).
- `/tmp` leak check: `/tmp` listed before and after the
  `GSTORE_DISK_QUERY_MODE=memory` workspace run and after the stream-mode
  run: 39 entries each time, none added, none removed.

## Requests/Not yet

- For the library redo/update-log owner (r4-db-graph or later): the update
  log is replayed through `parse()`, so a process that installed `[upload]
  sparql_max_size` (gserver, gbuild) would refuse to replay a logged
  statement longer than the limit, and a redo record of the old shape is
  read through `ntriples::parse_str`, bounded by `rdf_max_token_size`.
  Neither log is used by a binary today (both are library-only), so nothing
  was changed here.
- Package requests left for later rounds, unchanged: r3-sparql-paths
  (`tests/dt_disk_stream.rs`' `unordered_paths` exemption, r5-test-hygiene),
  r3-sparql-joins (an `expected.error` marker for the W3C harness; an
  OPTIONAL/MINUS bench query), r3-sparql-functions (`SECONDS`, blank nodes
  in `INSERT DATA` / DELETE templates, string argument checks — round 4),
  r3-rest-parity (REST `/upload` of N-Quads/TriG flattening named graphs,
  r4-api-hygiene), r3-parser (gnode's request size limit, r5-cluster; an
  early `sparql_max_size` check before the body is read, r4-api-hygiene;
  `build_from_ntriples_parallel`'s line buffer), r3-storage-kernel
  (`DirLock`'s fork race, r4-cli-config; test-only `.wal` derivations,
  r5-test-hygiene).
