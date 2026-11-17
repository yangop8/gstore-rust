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

Round-3 review fixes (the land review of the merged round; each with a test
that fails on the merged tree, except the test-robustness items):

- **Behaviour change: property paths hand their repeated solutions out
  lazily, under a per-query budget** (round-3 regression of query#9 /
  sparqlconf#4). A `/` or `|` path reaching a pair of nodes by `k` routes is
  `k` solutions (§18.4). The walk kept `k` as a count, but every pair was
  then pushed `k` times into a vector before anything consumed it, in both
  executors, so `ASK`, `EXISTS` and `LIMIT 1` could not stop early: on one
  self-loop triple a 24-step `(:p|:p)/…` chain took 1.2 GB and a 30-step one
  (a 761-byte query) aborted the process. The pairs now keep their counts
  through the evaluation; the streaming executor hands the copies out one
  at a time (so `ASK`, `EXISTS`, `LIMIT` take only what they need) and the
  materializing one pushes them. Every copy past a pair's first is charged
  to a per-query budget, `Evaluator::with_path_max_duplicates` (default
  `PATH_MAX_DUPLICATES` = 4,194,304, shared with `GRAPH` blocks); a query
  that would hand out more fails with a query error ("a property path of
  this query has more than … repeated solutions …") instead of exhausting
  the memory. Where only existence or distinctness matters — `ASK`,
  `EXISTS`, and a `SELECT DISTINCT` without aggregates, graph functions or
  per-solution functions (`BNODE`, `RAND`, `UUID`, `STRUUID`, user and PFN
  functions) — each pair is handed out once and nothing is charged; a
  sub-SELECT decides for itself. Test:
  `tests/sparql_path_multiplicity_is_bounded.rs`.
- **An OPTIONAL property path is walked from the end point the left side
  binds** (sparqlconf#9's bind join covered inner-join conjuncts only).
  `?s :id "2990" OPTIONAL { ?s :next* ?x }` ran the path from every node of
  the graph (501,500 path steps on a 1,000-node chain, 4.5 million on
  3,000, for 10 rows). The right side of an OPTIONAL with a path whose end
  point the left side binds is now a `PathJoin` seeded by the left side's
  variables; both executors join each left solution with it depth first,
  left-major, and keep the left solution when nothing passes the
  OPTIONAL's filters. Same answers and order. Test: the same file
  (`path_steps` independent of the chain's length).
- **OPTIONAL / MINUS / join partial indexes are bounded** (round-3
  regression of sparqlconf#2 / query#10). A left solution binding only part
  of a group's shared variables is looked up in an index on that part,
  built on first use and kept — with no limit on how many parts, so 1,000
  `VALUES` rows each binding a different subset built 1,000 indexes of the
  right side (1.6 GB on 10,000 triples for a one-row answer; the nested
  loop before took 18 MB). The partial indexes of one `RowIndex` now hold
  at most 4x its rows together (`PARTIAL_INDEX_FACTOR`); past that a part
  without an index is answered by a scan of its group (same candidates,
  same order). Tests: `tests/optional_partial_indexes_are_bounded.rs`,
  `probe::tests::partial_indexes_hold_at_most_a_few_times_the_rows`.
- **`BNODE(str)` gives each of two equal solutions its own node** (query#5;
  r3-sparql-functions). Solutions were told apart by their bindings only
  (without the nodes `BNODE` minted), so a repeated `VALUES` row, a path
  reached by two routes, or `SELECT (BNODE() AS ?a) (BNODE("x") AS ?c)`
  over equal rows shared one node (§17.4.2.9: distinct from the nodes of
  other query solutions). Every pass that binds an expression's value —
  `BIND`, the SELECT expressions, grouped solutions, in both executors —
  now gives each solution an identity while its expressions run; a minted
  node records it, so a later `BIND` or SELECT expression of the same
  solution keeps its node, and a solution changed since (joined, which may
  repeat it) is a new one. Tests:
  `tests/bnode_of_a_string_per_repeated_solution.rs`, W3C cases
  `functions/bnode-of-a-string-is-fresh-per-repeated-solution` and
  `functions/bnode-of-a-string-is-fresh-per-path-route`.
- **`REPLACE` parses a long `$` reference in linear time** (query#11;
  r3-sparql-functions). The digits after a `$` were given back one at a
  time with an insert at the front (XPath `fn:replace` §5.6.3), O(n²) per
  evaluated solution with no cancellation check: 800,000 digits took 6.5 s,
  a replacement at the 16 MiB `sparql_max_size` about 45 minutes per row.
  The reference is now chosen in one forward pass. Tests:
  `tests/replace_group_reference_scales_linearly.rs`,
  `xpath_regex::tests::a_group_reference_takes_the_digits_giving_back_took`.
- **RDF/XML: an XML literal's canonical form is bounded while it is
  written** (parser#9; r3-parser). Exclusive canonicalization repeats each
  namespace a top-level element of a `parseType="Literal"` value uses, so
  a 1 MiB namespace used by 4,000 siblings (a 1 MB upload) asked for 4 GiB
  and aborted the process; the document's expansion budget was checked
  only after the literal existed. The canonical form is now written
  against the budget left and fails with the usual "expands to more than …
  bytes" error as soon as it passes it. Test:
  `tests/rdfxml_literal_canonical_form_is_bounded.rs`.
- **update.log: a damaged last record is a torn tail unless a whole record
  follows it** (durability#6; r3-db-redo). A last record whose checksum or
  terminator was wrong (a zero-filled or changed last block) counted as
  corruption when any text after its start looked like a record header —
  an update-log statement may hold a line starting with `REC ` — and the
  appender's scan resumed at that text, so the next append cut the file
  inside the damaged record and wrote behind its leftover bytes: the log
  could never be replayed again. Every defect now follows the whole-record
  rule; the appender cuts at the record's start. Tests: `logs` unit tests,
  `tests/update_log_torn_statement_tail.rs`.
- **`backup_incremental` refuses an increment that could not be restored**
  (robust#10; r3-db-redo). The appender's scan steps over a corrupt record
  of the live log, so a damaged commit was copied into the increment, the
  backup answered `Ok` and `restore_incremental` failed ("checksum
  mismatch") for it and every later increment. The copied range is now
  walked as the restore walks it; a damaged one fails the backup with the
  offset, and a full `backup()` starts a new chain. Test:
  `tests/backup_incremental_refuses_an_unrestorable_range.rs`.
- **`restore_incremental` copies a disk / RocksDB base into a private
  directory beside it** (db#13; r3-db-redo). The scratch copy was
  `gstore-restore-<pid>-<n>` in the system temporary directory (often a
  small tmpfs), removed (ignoring the error) and re-created with
  `create_dir_all`, so a directory or symlink another user had put there
  was written into. It is now created exclusively (`mkdir`, mode 0700, a
  fresh name with a nonce: `<base>.restore-tmp-<pid>-<n>-<nonce>`) beside
  the base, in the temporary directory only when the base's parent cannot
  be written, and only a directory the call created is removed. Tests:
  `tests/restore_incremental_copies_the_base_privately.rs`,
  `db::tests::a_private_dir_is_created_exclusively`.
- **The redo and update logs follow what a reopen keeps after a failed
  commit point** (durability#6, db#8; r3-db-redo). On the disk layout every
  failed commit point dropped the commit's records, also when the failed
  flush had already given its write-ahead log a durable name — the next
  open replays it, so the reopened database held a commit neither log had;
  on RocksDB an auto-commit UPDATE whose journal could not be deleted (or
  whose log sync failed) was logged while a transaction in the same state
  was not. The pager now counts the flushes whose log name was made durable
  (`Pager` / `DiskStore` / `DiskTriples::durable_wal_batches`), and a failed
  commit point is classified once for UPDATE requests and transactions:
  nothing reached the store → records dropped (a transaction rolled back);
  the commit point's own flush failed after its log's name was durable,
  the store healthy before → the next open replays it, records appended;
  anything else (RocksDB journal or sync failures, a store broken
  part-way) → records dropped and the logs marked in doubt:
  `backup_incremental` refuses until a full `backup()` starts a new chain.
  Tests: `tests/redo_log_durability.rs`'s
  `the_logs_agree_with_the_reopened_database_after_a_failed_commit_point`
  (it replaces `an_update_whose_commit_point_failed_is_in_neither_log`,
  which never reopened the database and pinned the divergence; D6
  correction) and `db::commit_fate_tests` (rocksdb feature).
- **REST `/backup` marks a backup in progress** (design#4; r3-rest-parity).
  The owner record (`BACKUP_OF`) was written only after a successful backup
  and the cleanup ran only on an error return, so a crash or a panic left a
  directory with data and no record, and every later default-path backup
  of the database was refused (`409`) until someone removed it by hand. A
  backup now writes `BACKUP_OF.partial` (the database's name) before it
  copies anything and replaces it with `BACKUP_OF` once complete; a
  directory with a partial marker of the same database may be backed up
  into again, and REST `/restore` and `/api` `restore` refuse it (`409` /
  "incomplete"), also when a stopped backup was replacing an earlier
  complete one (whose rules may already be the new backup's). `/api`
  `backup` writes the marker too and its listing skips such directories.
  A marker naming the database being restored is reported before the
  ownership check, so the database's own restorers get the `409`, not a
  `403` "not a backup of" it. Tests:
  `http_api::tests::a_rest_backup_stopped_part_way_can_be_retried_and_is_never_restored`;
  `tests/rest_api_parity.rs::a_failed_backup_over_an_earlier_one_keeps_it_and_can_be_retried`
  is corrected (D6, with a review note): it asserted that the earlier
  backup stays restorable after a failed replacement, but the rules and
  ledgers are copied before the store, so the directory then pairs new
  rules with the old store; it now checks the record kept, the marker
  set, the restore refused for the restorer and root, and the retry
  completing the backup.
- **Load-robust scaling tests** (tests-compat). The seven parser scaling
  assertions of `tests/rdf_parser_conformance.rs` and the OPTIONAL/MINUS
  scaling test of `tests/sparql_join_semantics.rs` compared wall times of a
  small run inside one scheduler slice and a large one that a busy machine
  preempts, measured one after the other (7 of 8 and 3 of 11 binary runs
  failed with four busy loops on four CPUs). New work counter
  `EvalStats::join_probe_rows` (the rows the hashed operands of OPTIONAL,
  MINUS and joins looked at); the join test asserts it first,
  deterministically, in both executors (at least the matches, at most 4
  per data row, 9x for 8x the data), and keeps its timing check as the CPU
  time of the evaluating thread, sizes alternating. The parser assertions
  compare the parsing thread's CPU time, small and large alternating, and
  measure a miss again (up to three times); bounds unchanged. Corrected
  tests carry a note (D6: a flaky test, not wrong behaviour).

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

Round-3 review fixes:

- README.md (SPARQL notes) / CHANGELOG.md: a query whose `/` or `|` paths
  would hand out more than 4,194,304 repeated solutions (copies of a pair
  of nodes reached by several routes) fails with a query error; `DISTINCT`,
  `ASK`, `EXISTS`, a `LIMIT` or a closure (`*`, `+`) avoid it. The budget is
  an `Evaluator` builder setting (`with_path_max_duplicates`); the servers
  use the default (no configuration key).
- docs/DEPLOY.md (backups, operator notes; not a compat switch): a REST or
  `/api` backup directory holding `BACKUP_OF.partial` is a backup that did
  not finish (crash, panic, failure part-way); `/restore` refuses it (409)
  and the next backup of the same database into it is allowed and
  completes it.
- README.md / library docs (`restore_incremental`): a disk or RocksDB base
  is copied into `<base>.restore-tmp-<pid>-<n>-<nonce>` beside it (the
  system temporary directory only when the base's parent is not writable),
  so about the base's size of free space is needed there, besides the
  memory the restored database takes. `backup_incremental` refuses (a) a
  range of the redo log that holds a corrupt record and (b) any increment
  after a commit point that failed with an unknown outcome (the logs are
  "in doubt"), until a full `backup()` starts a new chain.
- README.md / docs/BENCHMARKS.md (EvalStats): `join_probe_rows` joins the
  work counters (`path_steps`, `bgp_bindings`, …) where they are listed.
- docs/REFACTOR_BACKLOG.md:
  - (query#9 / sparqlconf#4, D3) The path budget is not a configuration
    key (the servers run with the default); a lone path's `(start, end)`
    pairs are still walked in full, as counts, before its first copy
    streams (memory: one entry per distinct pair, not per route); `SELECT
    REDUCED`, CONSTRUCT without template blank nodes and the right side of
    a MINUS could take each pair once too, and still hand out (and charge)
    every copy.
  - (sparqlconf#9, D3) Paths inside a MINUS or EXISTS operand whose end
    point the other side binds are still evaluated with both ends free
    (OPTIONAL is bind-joined now).
  - (query#5) `BNODE(str)`'s solution identity is taken by the passes that
    bind a value (BIND, SELECT expressions, grouped solutions). A FILTER,
    ORDER BY or aggregate argument has none, so there two equal solutions
    share a node (unobservable except through e.g. `COUNT(DISTINCT
    BNODE("x"))`); a solution extended by a non-`BNODE` value or joined
    since a node was minted for it is a new solution (as before); equal
    solutions that a projection makes from one solution holding a node
    minted for it keep sharing that node.
  - (sparqlconf#2 / query#10) Building one partial index of a `RowIndex`
    polls no cancellation (each build is bounded: all partial indexes
    together hold at most 4x the operand's rows).
  - (durability#6) The "logs in doubt" mark is session state, like the
    backup checkpoint (`backup_offset`), not persisted: after a reopen the
    chain needs a full `backup()` again anyway. A RocksDB transaction whose journal was deleted but
    whose WAL sync failed is in doubt rather than classified (RocksDB
    does not say whether the batch is durable).

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

Round-3 review fixes, verified at their head (1865d57, branch `fix-r3`; the
same 4-vCPU machine, at most two heavy jobs at once through the slot
limiter, other worktrees building and testing throughout — load average
8-15; toolchain 1.99.1, `CARGO_PROFILE_{DEV,TEST}_DEBUG=0`):

- Failing before: this round's integration tests run against the merged
  tree's `src/` (b147336) with the new test files in place:
  `update_log_torn_statement_tail` 0 of 2 passed ("update.log is corrupt
  at byte 81: record terminator missing" / "checksum mismatch");
  `backup_incremental_refuses_an_unrestorable_range` 1 of 2 (the damaged
  increment was written); `restore_incremental_copies_the_base_privately`
  0 of 1 (the planted `/tmp/gstore-restore-<pid>-0` was removed);
  `rdfxml_literal_canonical_form_is_bounded` 1 of 2 (peak 541,107,873
  bytes for a 1,051,244-byte document, budget 67,279,616);
  `optional_partial_indexes_are_bounded` 0 of 1 (peak 611 MB on 4,000
  triples, 306 MB on 2,000); `replace_group_reference_scales_linearly` 1
  of 2 (4x the digits took 16.8x the CPU time, 0.36 s -> 6.08 s, on every
  attempt); `redo_log_durability` 19 of 20
  (`the_logs_agree_with_the_reopened_database_after_a_failed_commit_point`:
  "the redo log disagrees with the reopened database");
  `sparql_path_multiplicity_is_bounded` (a copy without the budget
  builder, whose two budget tests were left out) 1 of 3 (ASK over 22
  doubling steps peaked at 134,221,706 bytes; the OPTIONAL path took
  501,500 path steps); `bnode_of_a_string_per_repeated_solution` 0 of 4
  (equal solutions got one node, in both executors). The unit tests inside
  the fixed modules (`logs`, `probe`, `xpath_regex`, `http_api`,
  `db::tests::a_private_dir_is_created_exclusively`,
  `db::commit_fate_tests`) were checked against the unfixed code when they
  were written (commit messages); not re-run against b147336 here.
- Load robustness of the corrected timing tests: with four busy loops
  added to the machine's load (load average 14), the
  `rdf_parser_conformance`, `sparql_join_semantics` and
  `replace_group_reference_scales_linearly` binaries passed 8 of 8 runs
  each, none needing a retry (the review saw 7 of 8 and 3 of 11 failing).
  `EvalStats::join_probe_rows` in `optional_minus_and_unbound_joins_scale_linearly`:
  OPTIONAL and MINUS 1,250 rows looked at for 2,500 data rows and 10,000
  for 20,000; join after OPTIONAL 7,497 and 59,997; the same in both
  executors.
- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean (it
  caught a `needless_range_loop` in a new `probe` unit test, fixed in
  b701ff0).
- `cargo test --workspace --no-fail-fast`: `GSTORE_DISK_QUERY_MODE=memory`
  184 test binaries, 2938 passed, 0 failed, 8 ignored;
  `GSTORE_DISK_QUERY_MODE=stream` 184 binaries, 2938 passed, 0 failed, 8
  ignored. The memory-mode run before 1865d57 failed exactly
  `rest_api_parity::a_failed_backup_over_an_earlier_one_keeps_it_and_can_be_retried`
  (403 for a restore the test expected to succeed), the test corrected
  above.
- RocksDB (shared target directory, gstore compiled from this tree, under
  the rocksdb lock): `cargo clippy -p gstore --all-targets --features
  rocksdb -- -D warnings` clean; `cargo test -p gstore --features rocksdb
  --no-fail-fast` 173 test binaries, 2801 passed, 0 failed, 8 ignored
  (`db::commit_fate_tests`' two tests among them).
- Bench smoke gate (the fixes touch the engine, `topk` and `probe` hot
  paths): `cargo build --release --locked -p gstore -p gstore-bench --bin
  gbuild --bin gstore-bench` clean; `gstore-bench report --check README.md
  CHANGELOG.md` clean; `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=… target/release/gstore-bench ci --baseline
  bench/baselines/smoke.json`: PASS, 456 gates checked, 0 failed; time
  geomean vs the committed baseline: LUBM-like snapshot 0.87, disk 0.94,
  WatDiv-like snapshot 0.89, disk 0.94 (bound 1.50; the merge head
  measured 0.91 / 0.94 / 0.97 / 0.98); calibration score 67.57 ms. Machine
  state: the RocksDB test run and other worktrees' jobs ran meanwhile
  (load average 5-8).
- Not re-run for the review fixes: the other feature entries (gzip, tls,
  zip, readline, pfn-wasm; the fixes touch no feature-gated code) and
  `scripts/conformance.sh` (no compat switch, no wire-format change).
- `/tmp` leak check: listed before the workspace runs and after both: 50
  and 51 entries; the one added, `gstore_cancellation_snapshot_<pid>_2`,
  is no name a test of this tree uses (another worktree's run, pid not
  ours).

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
- Round-3 review fixes: all seventeen findings are fixed (none deferred).
  For later rounds: r4-sparql-guard (the Evaluator owner in round 4, with
  `EvalSettings`) may give the per-query path budget
  (`Evaluator::with_path_max_duplicates`) a configuration key next to
  `gfun_max_output_bytes` and its `query_settings()` line, and extend the
  OPTIONAL bind join to MINUS / EXISTS operands (docs notes above);
  r5-test-hygiene may move the other timing-based scaling tests of the
  suite to thread CPU time or work counters as done here
  (`tests/rdf_parser_conformance.rs`'s `assert_scales`,
  `EvalStats::join_probe_rows`).
