# r5-test-audit — Test-suite audits: an fsync audit model for every commit point, adversarial-coverage audit, wall-clock thresholds replaced by latches

## Changelog

Behaviour changes first (durability fixes the new fsync audit found; no
format change, no API removed):

- **New directories are created durably at every acknowledged commit
  point** (review tests-b#3). A database directory created by the disk
  bulk build (`Database::build_disk`, `gstore::bulk::build`, `gbuild`), the
  RocksDB bulk build, `Database::build_rocksdb_from_str` /
  `build_rocksdb_from_files`, `Database::backup_dir`, the servers' backups
  (REST `/backup`, `/api` `backup`: the backups directory and the backup's
  own directory) and a user's PFN source / module directories were made
  with `create_dir_all`: their files were synced, but the new directory's
  entry in its parent was not, so a power cut after the success answer
  could lose the whole database, backup or function. All of them now use
  `fsutil::create_dir_durable` (the parent of every created directory is
  fsynced).
- **Reasoning-rule directories are created durably** (tests-b#3).
  `reason_rule_files/` (created by the first `reasonManage add`) and its
  `inferred/` ledger directory are fsynced into their parents when they are
  created, so a rule's ledger is durable before its rule file and before
  the insert it records (a rule file without its ledger would otherwise be
  taken for an upstream rule after a power cut).
- New (doc-hidden, test-only in effect) `gstore::sync_probe`: the fsyncs
  commit points rely on go through `sync_file` / `sync_file_data` /
  `sync_dir` (fsutil, the pager's write-ahead log, the bulk install, the
  redo and update logs, rule files) and RocksDB's WAL sync reports itself;
  each sync is recorded (file identity, length, mtime, a directory's
  entries, call site) for every live `Probe`. With no probe alive the
  record costs one atomic load per fsync. Not a supported API.
- New `tests/fsync_audit.rs` (20 tests, 3 more with `--features rocksdb`):
  around one commit point, every changed file must have been synced after
  its last write, every changed directory after its last entry change and a
  RocksDB store's log after its last write; and every intermediate state a
  protocol documents (pager WAL; snapshot commit record; the bulk install's
  `cleared` / `kv-synced` / rules removed / `vstree-placed` /
  `layouts-removed` / `cleaned`; directory swaps; copies of a save
  elsewhere and of a backup; a rule's ledger before its rule file and
  before the insert) must have been recorded by a directory sync, in
  order. Covered: auto-commit updates (snapshot, disk Stream and
  Materialize, RocksDB), `commit()` and `/commit`, both servers' `/update`,
  snapshot saves and checkpoints (also over a disk database), disk and
  RocksDB saves elsewhere, backups (library, REST, `/api`), the restore
  swap, bulk installs (disk and RocksDB, into new / disk / ruled / snapshot
  directories), `build_rocksdb_from_str`, WAL create/rename/delete and its
  replay at open, a VS-tree rebuilt at open, redo/update-log appends, rule
  directory and PFN writes (review tests-b#3, tests-a#13).
- New `tests/adversarial_coverage.rs`: the map from every DoS /
  hostile-input category of review sec#20, http#20, robust#17 and gapi#21
  to its regression test (table below), plus the two that were missing:
  more than 256 header fields and a header line or request target over
  64 KiB are refused with `400` before authentication on the
  single-database and `/api` servers, and the worker serves on.
- Tests corrected (D6, with `review <id>` notes): `gapi::tasks`'s
  `callbacks_report_delivery_failures` only ever saw the host-local
  refusal; it now pins the refusal (nothing connects), a transport error
  with `callback_allow_local`, and a 503 that a retry recovers from
  (gapi#21).
- Wall-clock thresholds replaced by latches, counters or endless work plus
  a hang guard (http#20, gapi#21): `dt_gapi_tasks` (the 2500 ms async
  answer, two shutdown bounds), `server.rs` tests (idle connections, the
  dripping client, worker hand-over, counters), `http_client` tests
  (trickling endpoint and TLS handshake, closed port), `http_api` tests
  (`bad_credentials_never_hash_under_the_state_lock` now parks the hash
  and checks the state lock; the monitoring and query-settings tests' one
  second bounds). New doc-hidden test hook
  `http_users::test_hooks::park_verifications_of`.
- Test temp directories in the `server.rs`, `http_client.rs` and
  `gapi/tasks.rs` test modules are unique and removed on drop.

## GAP_PLAN status line

none

## Docs notes

- docs/STORAGE_BACKEND.md (durability / commit points): the fsync claims
  are now enforced by `tests/fsync_audit.rs` (see the Changelog list of
  covered commit points); every directory a commit point creates is made
  durable in its parent. Format registry: no on-disk format change.
- README.md (testing section, if it lists the suites): `tests/fsync_audit.rs`
  checks the fsyncs of every commit point through `gstore::sync_probe`;
  `tests/adversarial_coverage.rs` maps the adversarial categories to their
  regression tests.
- docs/REFACTOR_BACKLOG.md:
  - tests-b#3 (optional part): a nightly power-loss job on a filesystem
    that drops unsynced data (dm-flakey, or an `LD_PRELOAD` shim that
    discards writes not followed by `fsync`) was not added: it needs root
    and a block device (dm-flakey) or a libc shim maintained beside the
    code; the in-process audit checks the same syncs deterministically.
    tests-a#13's variant (rebuild every power-loss state from recorded
    writes and run the old-or-new checks) is likewise not implemented: the
    audit checks the documented intermediate states through the entries
    each directory sync recorded instead.
  - tests-b#3: three syncs no durable state depends on, so the audit cannot
    miss their removal — `persist::write_vstree_kv`'s directory sync (the
    pager's write-ahead-log protocol syncs the same directory before the
    header that calls the tree fresh), and in the RocksDB bulk build
    `rocks_sink`'s `sync_wal` right after a memtable flush (the log is
    empty) and its `sync_dir(staging)` (the staged store is renamed away and
    any later build discards the staging area). Kept as belt and braces.
  - http#20: `http_api::tests::statements_and_batch_payloads_are_parsed_off_the_state_lock`
    still samples the state lock (`busy * 8 < samples`); a deterministic
    version needs park points in the statement classifier and the
    batch-payload parser (production code in `http_api.rs`). The verifier
    measured it 10/10 green pinned to one CPU.
  - http#20: `monitoring_answers_while_the_database_is_held` still orders
    its "writer queued behind a reader" case with a 100 ms sleep (the
    RwLock's waiter is not observable); a slow start only weakens that
    case, it cannot fail it.

## Measured numbers

- Mutation check of the fsync audit (D6 for an audit): every sync call
  site a covered commit point reaches was disabled one at a time (an
  environment-gated skip in the probe, used locally and not committed;
  sites listed by the probe's `file:line`). Without the rocksdb feature 37
  sites: all but `write_vstree_kv`'s directory sync fail at least one audit
  test. With `--features rocksdb`, the 13 sites the RocksDB tests reach:
  all fail `rocks::rocksdb_builds_sync_their_install`,
  `a_rocksdb_save_elsewhere_syncs_the_swap` or
  `rocksdb_commit_points_sync_the_write_ahead_log`, except `rocks_sink`'s
  post-flush `sync_wal` and `sync_dir(staging)`, and the swap's second
  parent sync and `remove_other_layouts`' sync, which the non-RocksDB tests
  kill (restore swap, snapshot save over a disk database). The three
  survivors are explained in the Docs notes.
- Each durability fix's test failed before the fix: the directory-creation
  fixes in `backup_dir` (`backups_sync_the_copy_and_its_directory`),
  `create_rocksdb` (`rocksdb_builds_sync_their_install`: "directory …: its
  entries changed but it was never synced"), REST `/backup` and `/api`
  `backup` (`the_api_server_syncs_a_backup_before_it_acknowledges`, each
  path on its own); the rule-directory, PFN and bulk-build syncs fail their
  tests when disabled (mutation check above).
- The header-limit tests fail against relaxed limits (512 fields, 128 KiB
  lines: the server then answers with its head-deadline message, which the
  tests tell apart from the refusal); 25 consecutive runs green.
- Wall-clock thresholds in the audited tests (`grep -c 'elapsed() <\|elapsed() >\|busy \* '`,
  base 9f00e07 → branch; the remaining hits are hang guards or production
  code):

  | file | before | after | what is left |
  |---|---|---|---|
  | `tests/dt_gapi_tasks.rs` | 3 | 0 | three `Instant::now() < until` hang guards in poll loops |
  | `src/server.rs` (tests) | 5 (+1 150 ms work loop) | 0 | the 2 hits are production log rate limits |
  | `src/http_client.rs` (tests) | 2 | 0 | — |
  | `src/gapi/tasks.rs` (tests) | 0 (1 unguarded busy wait) | 0 | — |
  | `src/http_api.rs` (the http#20 tests) | 6 | 1 | `statements_and_batch_payloads_are_parsed_off_the_state_lock` (backlog) |
  | `tests/dt_rw_crash.rs`, `tests/fsync_audit.rs`, `tests/adversarial_coverage.rs` | 0 | 0 | — |

- Adversarial-coverage table (sec#20, http#20, robust#17, gapi#21; also in
  `tests/adversarial_coverage.rs`'s module docs):

  | category | regression test |
  |---|---|
  | request head dripped | `server::tests::a_dripping_client_cannot_hold_a_worker` |
  | body dripped after a complete head, one worker (plain, chunked, `/api`) | `http_framing_deadlines::a_dripped_body_is_cut_off_and_the_worker_serves_the_next_request`, `a_dripped_body_gains_nothing_by_declaring_a_large_length`, `a_dripped_chunked_body_is_cut_off_too`, `a_dripped_api_body_cannot_hold_the_api_server` |
  | TLS handshake / head / body dripped (`tls`) | `http_framing_deadlines::tls::a_dripped_tls_handshake_cannot_hold_the_worker` and its two neighbours |
  | connections per address | `http_framing_deadlines::the_per_address_cap_refuses_only_the_address_over_it` |
  | pre-authentication body cap and body budget | `http_preauth_limits::an_unauthenticated_body_over_the_preauth_cap_is_refused_before_it_is_read`, `a_chunked_unauthenticated_body_is_cut_off_at_the_preauth_cap`, `the_body_budget_bounds_all_bodies_in_memory` |
  | pre-authentication parse amplification | `http_preauth_limits::a_wide_json_body_is_refused_before_its_tree_is_built`; `gapi::params::tests::form_bodies_and_query_strings_have_a_bounded_number_of_pairs`, `multipart_parts_are_bounded`; `json::tests::limits_bound_values_and_depth` |
  | multipart worst case | `http_preauth_limits::a_multipart_boundary_longer_than_rfc_2046_allows_is_refused`, `a_64_character_boundary_over_megabytes_parses_in_linear_time` |
  | framing violations | `http_framing_deadlines::ambiguous_framing_is_refused_and_the_connection_closed`; `chunked_framing_counts_against_the_body_cap` |
  | control characters in header lines | `http_framing_deadlines::control_characters_in_header_lines_are_refused` |
  | header count / header line length | **new**: `adversarial_coverage::more_than_256_header_fields_are_refused_before_authentication`, `a_header_line_over_64_kib_is_refused_before_authentication` |
  | raw bodies holding `&query=` / `&update=` | `http_request_semantics::a_raw_sparql_update_body_is_never_parsed_as_a_form`, `a_raw_sparql_query_body_is_never_parsed_as_a_form`, `a_raw_rest_body_never_switches_the_database_or_the_statement` |
  | GET update, CORS, DNS rebinding | `http_browser_surface::get_never_runs_an_update_unless_the_compat_switch_is_on`, `cors_headers_go_only_to_listed_origins`, `a_loopback_request_must_name_a_local_host` |
  | lockout on every authentication path | `login_lockout::api_login_and_test_connect_are_refused_for_a_locked_account_without_hashing`, `rest_login_and_basic_credentials_are_refused_for_a_locked_account`, `the_pre_body_check_refuses_a_locked_account_before_hashing` |
  | hashing off the state lock | `http_api::tests::bad_credentials_never_hash_under_the_state_lock` (now deterministic) |
  | session growth / expiry | `user_store_consistency::a_user_flooding_logins_ends_only_their_own_oldest_sessions`, `expired_sessions_are_swept_on_login` |
  | deeply nested `SERVICE` reply | `service_hardening::a_deeply_nested_reply_is_a_parse_error`, `a_service_endpoint_replying_100k_nested_brackets_does_not_abort` |
  | surrogate pairs | `service_hardening::surrogate_pair_escapes_in_service_results_decode`, `a_surrogate_pair_from_a_service_endpoint_joins_with_local_data` |
  | `SERVICE` to loopback / host-local | `service_hardening::service_refuses_host_local_endpoints_under_the_allow_service_policy`, `the_rest_server_refuses_host_local_service_endpoints` |
  | control characters in endpoint IRIs | `service_hardening::crlf_in_a_service_endpoint_iri_is_refused_before_connecting`, `sparql_post_refuses_whitespace_and_control_characters` |
  | callback transport error / retry / `callback_allow_local` | `gapi::tasks::tests::callbacks_report_delivery_failures` (corrected), `dt_gapi_tasks::callback_receives_the_result_and_failures_are_tolerated` |
  | failed query in the query log | `query_limits::failed_api_queries_are_in_the_query_log_with_their_status` |
  | short `ipmanage` entries | `ipmanage_round_trip::short_ipv6_entries_round_trip_through_save_and_restart` |
  | reconcile against an unlistable root | `sysdb::tests::an_unlistable_root_keeps_registrations_and_grants`; `sysdb_reconcile_read_errors` |
  | corrupt / truncated `named.bin` | `snapshot_read_errors::a_truncated_named_bin_fails_the_open_and_gcompact_reclaim_refuses` and neighbours |
  | failed redo-log append | `redo_log_durability::a_failed_append_is_reported_cut_off_and_retried_in_order` |
  | query time / answer size budgets | `query_limits::a_runaway_rest_query_times_out_and_a_queued_writer_then_completes`, `a_rest_answer_over_the_byte_budget_is_refused`; `api_result_budget_counts_escapes` |
  | anonymous export download | `gapi_download_policy::anonymous_downloads_are_refused_by_default` |
  | `ConcurrentDb` chain growth | `concurrent_db::version_chain_gc_work_stays_flat_after_20k_commits`, `churned_keys_leave_no_chains` |

  Tests that codified the risky behaviour were corrected where each fix
  landed (rounds 1-2: the `/api` login lockout exemption is now only the
  `upstream_login_lockout` switch's in `dt_gapi_users`; loopback `SERVICE`
  is refused in `dt_security_review`; anonymous export downloads need
  `anonymous_download` in `dt_gapi_db`) and here
  (`callbacks_report_delivery_failures`).
- Runtimes: `fsync_audit` 0.5-0.8 s, `adversarial_coverage` 0.2-0.4 s;
  `dt_gapi_tasks` 3.4 s.
- Bench smoke gate (D9; the only change on a measured path is one relaxed
  atomic load next to each `fsync` / `fdatasync` / `flush_wal`):
  `cargo build --release --locked -p gstore -p gstore-bench --bin gbuild
  --bin gstore-bench`, then `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=/home/user/fix-campaign/bench-cache gstore-bench ci
  --baseline bench/baselines/smoke.json --bin-dir <release dir>` on the
  branch head: `Gates: PASS (456 checked, 0 failed)`, calibration score
  69.61 ms. Shared 4-vCPU machine with other packages' builds and this
  package's own feature runs going on; branch only (no base-vs-branch
  pair), as no measured code path changed beyond that load.
- Final verification on the branch head (77ecf46 plus this fragment):
  `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -D
  warnings` clean; `RUSTDOCFLAGS=-D warnings cargo doc -p gstore
  --no-deps` clean (after the 77ecf46 link fix); `cargo test --workspace`
  3099 passed, 0 failed, 10 ignored in each of
  `GSTORE_DISK_QUERY_MODE=memory` and `stream`; with `--test-threads=1`:
  `fsync_audit` 20, `adversarial_coverage` 2, `dt_gapi_tasks` 16,
  `dt_rw_crash` 5 and the touched lib modules (`server::`,
  `http_client::`, `gapi::tasks::`, the five `http_api` tests) 75, all
  passed; `--features tls`: clippy clean, lib `server::` / `http_client::`
  63 passed, `http_framing_deadlines` 13 and `adversarial_coverage` 2
  passed; `--features rocksdb` (shared `target-rocks`, under its lock):
  clippy clean, `cargo test -p gstore --features rocksdb` 2968 passed, 0
  failed, 10 ignored (the three `fsync_audit::rocks::` tests included).

## Requests/Not yet

- Edits outside this package's owned files (all small; no other R5 package
  owns them):
  - `src/db/persist.rs` (R5: nobody): `Database::backup_dir` and
    `Database::create_rocksdb` create their directory with
    `create_dir_durable`.
  - `src/http_api.rs` (R5: nobody): `handle_backup` and `backup_target`
    use `create_dir_durable`; test module: the five http#20 tests above.
  - `src/gapi/ops_db.rs` (R5: nobody): the `backup` operation's target and
    both destination paths use `create_dir_durable`.
  - `src/http_users.rs`: `test_hooks::note_verification` also serves the
    new `park_verifications_of` (doc-hidden test hook).
  - `src/db/logs.rs` `LogWriter::write_out`, `src/reason/store.rs`
    `RuleStore::create` / `set_ledger` / `ensure_dir`, `src/pfn/registry.rs`
    `write_all` / `create` / `compile`, `src/bulk/rocks_sink.rs`
    `sync_tree` / `build_rocksdb`: syncs routed through the probe and the
    durability fixes above.
  - Sanctioned regions: `src/db/fsutil.rs` (`sync_dir`, `write_tmp_mode`,
    `sync_tree`), `src/kvstore/pager.rs` (`apply_committed`,
    `sync_parent_dir`, `write_wal`, `recover_wal`, new `file_of_wal`),
    `src/bulk/commit.rs` (`sync_file`, `build_disk`,
    `newer_than_snapshots`, `install`), `src/backend/rocks.rs`
    (`RocksStore::sync_wal`); `src/lib.rs` one `pub mod sync_probe` line.
- Not done (backlog, reasons in Docs notes): the nightly power-loss job;
  the remaining sampling test in `http_api.rs`.
- `--test-threads=1`: run for the touched test targets and lib modules
  (not the whole workspace, which the default-parallelism runs cover in
  both disk query modes).
