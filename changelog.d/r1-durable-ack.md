# r1-durable-ack — Never acknowledge a non-durable write; never treat a read error as 'absent'

## Changelog
- **Behaviour change: a write that was applied in memory but not saved is never acknowledged** (durability#2). A write whose check_point save failed (or, with check_point off, whose commit point failed) used to answer success with a `"warning"` field: HTTP 200 / `StatusCode` 0. It now answers a failure whose message is `"applied in memory but not durable: <cause>"`. The operation's fields and the `"warning"` field are kept as detail.
  - REST `/update`, `/sparql` UPDATE, `/upload`, `/batchInsert`, `/batchRemove` and `/commit` answer HTTP 500: `{"StatusCode":500,"StatusMsg":"applied in memory but not durable: …",<fields>,"warning":…}`.
  - The single-database `gserver` (`/update`, `/sparql` UPDATE) answers HTTP 500: `{"error":"applied in memory but not durable: …","changed":N,"warning":…}`.
  - The upstream `/api` protocol keeps HTTP 200, as for every `/api` answer. `query` updates, `commit` and `reasonManage` execute answer `StatusCode` 1005 (disable already saved its retraction itself and failed when that save failed), the code `batchInsert` already answered for a failed save.
  - An UPDATE that fails part-way and whose applied part cannot be saved is a persistence failure. It now answers REST 500 (was 400 with `{"error":…}`) and `/api` 1005 with the "not durable" message (was `"Query fail: …"`). This uses the new `OpErrorKind::Persist`.
  - The write stays applied in memory and is saved by the next successful save, `/checkpoint`, `/unload` or graceful shutdown, but it is lost if the process dies first.
  - The REST query log records such a write with `statusCode` 500 (it recorded 0, a success); the `/api` query log already recorded the answer's 1005.
- **Behaviour change: an unreadable `named.bin` fails the open** (db#1, robust#4, durability#3).
  - Before, every read error of a snapshot database's `named.bin` (truncation, decode error, EIO, EACCES, EMFILE, a dangling symlink) read as "no named graphs". The next check_point save or `gcompact --reclaim` then overwrote the graphs with an empty map and freed their terms.
  - Now `Database::open`/`load`/`read_copy` (and so `gserver` loads, every CLI tool and `gcompact --reclaim`) fail with an error naming the file.
  - A missing `named.bin` (a database written before named graphs existed) still opens without named graphs.
  - The legacy RocksDB migration refuses an unreadable `named.bin` before it retires any file. No format change.
- **Behaviour change: user-store mutations whose `users.db` save fails change nothing** (sec#15).
  - `UserStore::add`, `drop_user`, `set_password`, `forget_database` and `copy_database_privileges` roll the in-memory table back when the save fails. Before, a "failed" password change left the new password live until a restart brought the old one back.
  - A user's sessions are revoked only after a successful save.
- **Login sessions are bounded** (sec#12, http#12).
  - A login sweeps expired sessions once the table has grown. The next sweep waits until the table has doubled, so a sweep costs amortised O(1) per login.
  - One user holds at most `MAX_SESSIONS_PER_USER` (1024) sessions. A further login ends that user's oldest session, never another user's.
  - A session is expired once its age reaches the TTL.
  - New `UserStore::session_count()` and `UserStore::set_session_ttl()`.
- **Startup reconcile forgets only databases confirmed gone** (gapi#9). If the server root cannot be listed, the registry is not reconciled and a warning is logged. A `<db>.db` that exists but cannot be examined (a dangling symlink to a volume not mounted yet, a failing stat) keeps its registration, creator and grants, with a warning. Only a `NotFound` directory unregisters the database and forgets its grants.
- **`ipmanage` saves only entries its loader reads back** (gapi#7).
  - The list loader keeps a line shorter than 7 characters when it is a valid entry, such as `::1`, `::` or `1::`. Other short lines stay comments, as upstream.
  - `IpFilter::save` refuses a list the loader would not read back whole. An allow list of `::1` no longer comes back empty and locks everyone out.
  - Divergence from upstream: upstream skips every line shorter than 7 characters.
- Tests:
  - New test files:
    - `tests/snapshot_read_errors.rs`: truncated, undecodable, EISDIR, EACCES (skipped as root) and dangling-symlink `named.bin`; `gcompact --reclaim` refuses and leaves the file untouched; a missing `named.bin` still opens; the VS-tree is valid after reopen, also after `vstree.bin` damage; with the `rocksdb` feature, the legacy migration refuses. This is the named.bin part of db#24.
    - `tests/save_failure_ack.rs`: `gserver` children with `RLIMIT_FSIZE` below the snapshot size (`SIGXFSZ` ignored), so every save fails with EFBIG. Every write path above answers non-success, the query logs record those writes as failures, and after SIGKILL and a restart exactly the acknowledged writes are present.
    - `tests/user_store_consistency.rs`: injected `users.db` save failures, a flooding user, and expired sessions swept on login.
    - `tests/sysdb_reconcile_read_errors.rs`: a dangling-symlink database across restarts.
    - `tests/ipmanage_round_trip.rs`: short IPv6 entries through save, reload and restart.
  - New unit test with a test failpoint in `src/sysdb.rs`: an unlistable root keeps registrations and grants.
  - Corrected tests (D6, each with a `review <id>` note):
    - `tests/dt_rw_crash.rs`: the acknowledgement rule no longer counts a reply that reports a persistence failure. A unit test covers the rule (durability#18).
    - `tests/dt_http_durability.rs`: `gserver` children are kill-on-drop guards started in a per-test temp directory (tests-a#9). `contains("1")` and `contains("true")`/`contains("false")` are replaced by the parsed `StatusCode`/message and the parsed SPARQL boolean (tests-a#11).
    - `tests/dt_cli_compat.rs`: the gmonitor check reads the `triple_num` line (tests-a#11).
    - `tests/dt_cli.rs`: gbuild's exit status is asserted. The closed-pipe gadd adds two new triples and checks the database holds them (tests-a#11).
    - The `src/gapi/ipfilter.rs` unit test `upstream_list_files_parse` pinned the skipping of `::1` and is corrected (gapi#7).

## GAP_PLAN status line
none

## Docs notes
- README.md (the durability paragraph, "`gserver` saves each write before acknowledging it"): add that a write whose save fails is answered as a failure. That is HTTP 500 from REST and the single-database server, or `StatusCode` 1005 from `/api`, with `"applied in memory but not durable: <cause>"`. The write stays applied in memory and is saved by the next successful save, `/checkpoint`, `/unload` or shutdown, but it is lost if the process dies first. Clients must treat such a reply as "not committed".
- README.md (`/api` notes): `ipmanage` accepts and keeps short IPv6 entries such as `::1` in the list files. Upstream skips lines shorter than 7 characters.
- docs/DEPLOY.md: this package adds no secure default and no compat switch. Operator note for the monitoring section: HTTP 500 or `StatusCode` 1005 with "applied in memory but not durable" means saves are failing (disk full, quota, EIO). At startup, the warnings "cannot list the database directories" and "stays registered" mean a database directory or volume was unreachable; its grants were kept.
- docs/STORAGE_BACKEND.md (format registry): no on-disk format change. `named.bin` read errors now fail the open; only a missing `named.bin` means "no named graphs". The snapshot commit record (`SNAPSHOT.commit`) carries member names only, with no per-member length or checksum.
- docs/REFACTOR_BACKLOG.md:
  - db#1 / robust#4 / durability#3 follow-up: record the quad count, or per-member checksums, in `meta.bin` behind a format feature bit (format registry, old binaries refuse cleanly). A `named.bin` that still decodes, for example an older copy restored beside a newer `meta.bin`, would then be detected too. This needs a format change (D7), so it was not done here.
  - db#24, the parts outside this package: malformed or huge-length redo.log/update.log input, and redo completeness after inference or a failed flush, go to r3-db-redo (db#9, db#3, db#2). VS-tree validity after CLEAR/DROP/inference goes to r4-db-graph (db#4). `mutation_count` after an empty rollback goes to r2-txn (db#7).
  - durability#18, the parts outside this package: an fsync-tracking (power-loss) fault layer goes to r5-test-audit (fsync audit). Torn redo/update-log tests go to r3-db-redo (log framing). This package covers the save-failure acknowledgement with `RLIMIT_FSIZE` children.
  - gapi#7, the suggested extra guard: refusing an allow-list save that would lock out the calling address is not implemented. Upstream `ipmanage` writes any valid list, so the guard is a new refusal that needs a policy decision (how a proxied or trusted-local caller counts, whether root may knowingly replace the list); the plan scoped this package to the save/load round trip of list entries. The silent-emptying cause is fixed, and an empty `ips` was already refused (1003).

## Measured numbers
- No hot path changed (pager, RocksDB, engine, topk, string index, optimizer, gfun, HTTP read path), so the bench smoke gate was not run (convention 8). Only these changed: the write replies, the open path's `named.bin` read, startup reconcile, `UserStore` mutations and login, and `ipmanage`.
- Session sweep scaling: 10 000 logins whose sessions are all expired (TTL 0) leave at most 1024 entries in the table, against 10 000 before (`tests/user_store_consistency.rs::expired_sessions_are_swept_on_login`). A user flooding 5 000 logins keeps at most 1024 sessions.
- `scripts/conformance.sh --no-build` (dev binaries of this branch): all six suites passed (basic, update, transaction, api, parser with the two known failing cases 15 and 17, python with 49 SDK checks). No compat switch was needed.
- `tests/save_failure_ack.rs` runs in about 0.5 s (two `gserver` children per test, a 3001-triple base, `RLIMIT_FSIZE` 64 KiB).

## Requests/Not yet
- Edits outside the owned files (for the integrator):
  - `src/gapi/ops_reason.rs` `reasonManage`: the success block that set a `"warning"` now sets `StatusCode` 1005 and `StatusMsg`. No R1 owner.
  - `src/gapi/ops_query.rs` `end()` (the `/api` commit/rollback answer): the `Ok(warning)` arm now answers 1005. No R1 owner.
  - `src/http_api.rs`:
    - `execute_query_job` (no R1 owner; r2-limits-auth's in R2): the query-log outcome of a non-durable update is the `Persist` failure.
    - Two new items: `NOT_DURABLE` (a const before `warn_field`) and `Resp::write_outcome`, inside `impl Resp` right after `ok_warn`.
    - Doc comments of `end_txn` and `OpErrorKind`, and the module docs' Durability paragraph.
    - The tails of `handle_upload` and `handle_batch` (sanctioned).
  - `src/server.rs` `handle_sparql`: the two sanctioned arms (rev3), plus the new helpers `update_reply` and `failure_status` after `changed_json`.
  - `src/db/persist.rs`: the new `Database::read_named_graphs`, right after `read_snapshot`.
  - `src/http_users.rs`:
    - Rollback also in `forget_database` and `copy_database_privileges`.
    - New session helpers, `session_count`, `set_session_ttl` and two constants.
  - `src/sysdb.rs` tests module: a thread-local failpoint and one unit test.
  - `tests/dt_cli.rs` and `tests/dt_cli_compat.rs`: the tests-a#11 sub-claims. No R1 owner; `dt_cli.rs` is r5-test-hygiene's in R5.
- For r2-txn (M2's "applied but not durable" commit status): reuse `NOT_DURABLE` and `Resp::write_outcome`, so every not-durable answer has one shape.
- For r3-rest-parity (R3 sole `http_api.rs` editor): `run_query_job` still ends the task of a non-durable update as succeeded (`TaskGuard::set_ok`). This is not visible on the wire today (`operationTaskList` reports both as finished, status 1); align it when the task state becomes visible.
- Environment note for every agent using `CARGO_TARGET_DIR=/home/user/fix-campaign/target-rocks`: worktrees with the same layout get the same artifact hash for the `gstore` crate. A feature build can therefore silently link another worktree's `gstore` library when that one was built last. Touch one of your own source files before a feature build; one run here linked the unfixed library until `src/db/persist.rs` was touched.
- Nothing of this package's ids is left open.
