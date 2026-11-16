# r2-txn — HTTP and /api transaction lifecycle: binary commit outcome, idle timeout, stale TIDs, RocksDB transaction costs; RocksDB /begin capture outside the exclusive guard

## Changelog
- **Behaviour change: a commit whose commit point fails is rolled back, or answered as not durable** (review M2: gap-txn-lifecycle#3, http#18; gap-rocksdb-exec#5).
  - `Database::commit` used to drop the undo log before the write-through, the commit point and the redo flush ran. A Materialize-mode commit that failed on a full disk was answered "commit failed" while its writes stayed visible to every reader, and the next successful save made them durable (or a restart lost them). A RocksDB commit whose undo journal could not be deleted stayed visible until the next write committed it or a reopen undid it, and its redo records were already logged.
  - The undo log is now kept until the commit point succeeded. When the commit point fails before anything of the transaction can have reached the store, the transaction is rolled back: in memory, or for RocksDB by replaying its journal. On disk this means the failed flush's write-ahead log never got its name, and neither did any flush the page cache made during the commit; on RocksDB it means the journal could not be deleted. None of its writes stays visible, and none becomes durable later. Otherwise (the log was complete, so the next open replays it; RocksDB: the journal was already deleted) the writes stay applied and the outcome says they are not durable.
  - The new library API `Database::commit_checked` returns `Result<(), CommitFailure>`. `CommitFailure` has four variants: `NotCommitted` (still open), `RolledBack`, `NotDurable` and `CommittedWithError` (the redo log could not be appended). `Database::commit` keeps its signature and reports each of them as an error.
  - The ids a transaction's deletes left unreferenced are still freed in the commit's own commit point. The in-memory dictionary retires them only once that commit point succeeded, so the undo of a rolled-back commit finds every term it re-adds. Only a committed transaction reaches the redo log.
  - HTTP: the outcome is recorded only once the commit returned, as what happened: in the registry of ended transactions, in `txn.log` and in the `/api` transaction log. A rolled-back commit answers REST 500 / `/api` 1005, `"commit failed and transaction N was rolled back: <cause>"`; on an on-disk database whose store the failure broke, the message adds that it must be unloaded and loaded before writing to it again. A commit that can no longer be undone answers REST 500 / `/api` 1005 with `"applied in memory but not durable: the commit point of transaction N failed: …; the database needs a reload (unload, then load)"` (`OpErrorKind::Persist`), plus the message as `warning` on `/api`. Both are logged as `ABORTED` in the `/api` transaction log. `txn.log` gets `commit-failed-rolled-back` or `commit-failed-not-durable`. A retried commit is told `"already rolled back after its commit failed"` / `"already left applied but not durable by a failed commit"`, where it used to be told "already committed".
- **Behaviour change: idle transactions are rolled back after `txn_idle_timeout`** (review M3: gap-txn-lifecycle#1).
  - New `[system]` key `txn_idle_timeout`, in seconds: default 600, `0` = never (upstream's behaviour). A transaction with no statement running for that long is rolled back and logged as `ABORTED` (`txn.log`: `aborted`; ended as "rolled back after txn_idle_timeout"). The same happens to a transaction whose owner was deleted ("rolled back because its owner was deleted"), whatever the timeout.
  - Three reaping homes. A new `begin` on the database reaps under the exclusive guard it takes anyway; `begin` is no longer refused by such a transaction. Any request reaps once the earliest idle deadline has passed (one atomic read per request otherwise). A background pass runs every `min(txn_idle_timeout / 4, 30 s)`, so an idle server also frees abandoned transactions, and with them a RocksDB database's copy for one.
  - The request path and the background pass never wait for a database. They take its guard with `try_write`, skip a busy database, and try again later, so `/cancel` and other requests stay immediate. A transaction leaves the registry only while its database's exclusive guard is held, so the registry and the database agree at every step.
  - A statement in flight is activity, and the idle clock restarts when it ends. `/tquery`, `/commit` or `/rollback` of an expired transaction answers "not running (rolled back after txn_idle_timeout)", and the next request then reaps it.
  - Before, one `/begin` by any user with the update privilege (or that user's crashed client) blocked every write and maintenance operation of the database until root found the TID or the server restarted.
- **Behaviour change: who may end a transaction** (gap-txn-lifecycle#5). Any superuser (`root`, or a user with the `admin` privilege) may `/tquery`, `/commit` or `/rollback` any transaction; before, only the literal `root` could. `rollback` no longer needs the update privilege: `sysdb::requirement("rollback")` is now `Anyone`, and the transaction handlers let only the owner or a superuser end it. An owner whose update privilege was revoked can therefore roll back their own transaction. Committing still needs the privilege. This diverges from upstream's `check_privilege`, which requires the update privilege for rollback.
- **Behaviour change: stale transaction ids never end another transaction** (review M4: gap-txn-lifecycle#2).
  - REST and `/api` share one TID parser (`http_api::parse_tid`): a positive integer, or the transaction log's `<begin>_<tid>` form. REST no longer accepts a sign (`+2`).
  - The begin time of a `<begin>_<tid>` id must be the transaction's. Otherwise the id names no open transaction: it gets the remembered end of the earlier transaction it names ("not running (already committed …)"), else "Wrong transaction id" (REST 404, `/api` 1005). Before, the begin time was ignored, so after a reload or a restart the Workbench's "roll back the stale RUNNING row" ended the new transaction with the same number.
  - Ended outcomes keep their database's load generation and begin time and survive reloads; before, a reused id's record was deleted.
  - When a server starts serving, it marks the transactions a previous process left `RUNNING` in the transaction log (`logs/transaction.json`) as `ABORTED`.
  - `begin` still answers the bare id, as upstream.
- **`ConcurrentDb`: a commit the database rolled back publishes nothing** (follows gap-txn-lifecycle#3; found in review). `ConcurrentDb`'s `Txn::commit` treated every commit-point failure as "applied but not durable": it published a new version and recorded the writes in its per-key version chains, so after the binary commit rolled a transaction back, a version-pinned reader (`version_view`) still saw its writes. `apply_txn_ops` now asks `Database::commit_checked`: a rolled-back commit fails the `ConcurrentDb` transaction as a whole (`Err`, nothing published, as an insert that cannot be encoded); a commit whose writes stay applied is published and reported as before (`Err` from `Txn::commit` after publishing).
- **An empty rollback keeps the caches** (db#7). Rolling back a transaction that changed nothing (only reads, or writes that changed no triple) no longer advances `mutation_count` nor drops the cached query results and the CSR.
- **RocksDB `/begin` no longer blocks the database's readers** (gap-rocksdb-exec#3). The whole-store copy for the transaction's read snapshot (REFACTOR_BACKLOG D4) is taken under the shared guard. The exclusive guard is taken only to register the transaction, after checking that the committed state did not change since the copy. If a write slipped in, the copy is taken again; after 3 copies the last one is taken under the exclusive guard, with an info log line.
- **Monitoring shows the open transaction** (gap-txn-lifecycle#1). REST `/show` and `/monitor`, and `/api` `show` and `monitor`, report each database's open transaction as a new `transaction` object: `TID`, `txnlog_TID` (`<begin>_<tid>`), `user`, `isolation`, `begin_time`, `idle_seconds` and `running_statements`.
- New public items:
  - `db::CommitFailure` and `Database::commit_checked`.
  - `RocksStore::rollback_uncleared_journal`.
  - `Pager::wal_batches`, `DiskStore::wal_batches`, `DiskTriples::wal_batches`: flushes that got as far as naming their write-ahead log.
  - `ApiLogs::txn_begin_at` and `ApiLogs::abort_stale_running`.
  - `GStoreConfig::txn_idle_timeout`.
  - Hidden test hooks in `http_api`: `debug_advance_txn_clock`, `debug_wake_txn_reaper`, `debug_wait_txn_reaper_pass`, `debug_park_txn` / `TxnParkPoint`.
- Tests:
  - `tests/txn_lifecycle.rs` (new). It covers:
    - the commit outcome through real `gserver` children under `RLIMIT_FSIZE`, in both disk modes: a log that cannot be staged gives rolled back, invisible, never durable after a reload, a restart or a reopen; a log that is complete but cannot be applied gives not durable, visible, kept by the reopen;
    - each reaping home: begin with a deleted owner, any request, and the periodic pass on an idle server observed in the log file;
    - a running statement keeping its transaction alive;
    - the reapers skipping a database whose guard is held (`/cancel` and an unrelated request answer, and the next request reaps once it is free);
    - owner and superuser rights;
    - monitoring;
    - stale ids after unload/load and after SIGKILL + restart;
    - REST TID parsing;
    - db#7;
    - with `rocksdb`, a read completing while the RocksDB begin copy is parked, and a write that slips in forcing a new copy.
  - `src/txn_commit_tests.rs` (new, crate-internal, `lib.rs` `mod` line): the commit outcome under the pager failpoints (both disk modes) and RocksDB's journal-deletion failpoint, the redo log included; a `ConcurrentDb` commit rolled back at its commit point publishes nothing (fails before the review fix: the version view holds the rolled-back triple). The RocksDB not-durable case ends its transaction with a named-graph quad, so the journal replay deterministically fails at it (the review made it assert `NotDurable`; it accepted a rolled-back outcome too, which was the only one reached).
  - `tests/txn_lifecycle.rs`'s in-process server is joined before its temporary directory is removed (review: the shutdown's `txn.log` append recreated the directory, and earlier runs left `/tmp/gstore_txn_lifecycle_*` behind).
  - Run against the parent commit (b8b1c8e), the tests that need no new hook fail. In `tests/txn_lifecycle.rs` that is all 8: the failed commit is visible, the stale id commits the new transaction, the begin is refused 1008, the read-only rollback moves the mutation count, `+1` is accepted, and admin gets 403. With the park hook ported to the parent, the RocksDB read hangs past its 60 s hang guard. In `src/txn_commit_tests.rs` the three rolled-back cases fail.
  - Corrected tests, each with a `review <id>` note:
    - `tests/upstream_transaction.rs` (gap-txn-lifecycle#6): asserted that a reload "forgets how the old ids ended"; it now checks that the earlier load's id is refused as already committed.
    - `tests/dt_cross_phase_api.rs` (tests-a#2): the vacuous `contains('2')` COUNT check now compares the lexical value.
    - `tests/dt_gapi_core.rs` and `tests/dt_http_durability.rs` (gap-txn-lifecycle#2): a made-up begin time in the `beginTime_tid` form is refused, and the real one, taken from the transaction log or `/show`, is used.
    - `tests/dt_gapi_users.rs`, `src/gapi/auth.rs` unit test (gap-txn-lifecycle#5): `rollback` is open to every user.
    - `src/db/review_tests.rs::stale_journal_after_a_failed_commit_blocks_writes_until_cleared` and `src/db/r4_review_tests.rs::a_clear_rocksdb_refused_logs_only_what_it_removed` (gap-rocksdb-exec#5): they asserted that a commit whose journal could not be deleted stands once the next write cleans the journal up; it is rolled back now.
    - `src/config.rs::tests::every_key_is_read_from_its_section` sets the new key.

## GAP_PLAN status line
none

## Docs notes
- docs/DEPLOY.md, compat-switch table: `txn_idle_timeout` (`[system]`). Secure default `600` (seconds); upstream value `0` (never). Risk with `0`: a client that vanishes after `begin` blocks its database's writes, maintenance and further begins until root rolls the transaction back or the server restarts, and on RocksDB it pins a full copy of the store. `scripts/conformance.sh` does not need it. With `0`, gserver prints `upstream-compat: txn_idle_timeout=0 (…)` at startup (an entry in `GStoreConfig::upstream_compat_warnings`). It is a tuning key, so `conf/conf.ini.example` has it as an active line with its default.
- docs/DEPLOY.md / README.md (transactions):
  - Explain the idle timeout and its three reaping homes: next `begin`, next request, background pass every `min(timeout/4, 30 s)`.
  - A transaction of a deleted user is rolled back by the next `begin` on its database, by the background pass, or by the next request after one of its transaction requests found it orphaned. A plain non-transactional write in between is still refused until one of those runs; detecting the deletion at once would need a hook in user management, which this package does not own.
  - Who may end a transaction (owner, any superuser; rollback needs no update privilege).
  - The `<begin>_<tid>` form is checked.
  - `RUNNING` rows of a killed server become `ABORTED` at the next start.
  - The new `transaction` object in `/show`, `/monitor` and `/api` `show` / `monitor`.
  - The commit failure answers, and that after a "not durable" answer the database must be unloaded and loaded; the reload keeps what the write-ahead log committed.
  - `refreshconf` applies a new `txn_idle_timeout` at once to `begin` and the request path, but the background pass keeps the interval it was started with, and is not started at all when the server started with `0`, until a restart.
- docs/STORAGE_BACKEND.md: no format change.
  - `Pager::wal_batches` is an in-memory counter.
  - A commit's outcome depends on whether a flush of it named its write-ahead log.
  - D12 interplay: a transaction larger than the page cache whose intermediate flush succeeded is answered as not durable when a later flush fails, because its prefix is durable.
  - A committed transaction's id frees stay in its own commit point. Materialize mode retires them in the in-memory dictionary only after that commit point succeeded.
- docs/REFACTOR_BACKLOG.md:
  - D4 (gap-rocksdb-exec#3) stays open: native RocksDB snapshots, so that `/begin` copies nothing. The copy is now taken under the shared guard, and the idle timeout reclaims abandoned copies.
  - New entry (gap-txn-lifecycle#3 / M2, D3): the commit outcome on RocksDB is covered at the library level (`src/txn_commit_tests.rs`, the cfg(test) journal failpoint) but not by an HTTP-level test. No OS-level injection hits the journal deletion deterministically, and the failpoint is not reachable from integration tests.
  - New entry (gap-txn-lifecycle#1, D3): detecting a deleted owner at deletion time needs user-management code to arm the reaper (`ApiInner::txn_reap.arm(0)` after a user is deleted). Until then it is detected lazily, see above.
  - New entry (D3): the shared REST and `/api` transaction lifecycle code is still split across `http_api.rs` (handlers, registry) and `gapi/ops_query.rs` (envelopes). A shared lifecycle module remains a refactor candidate (design#4 family); no behaviour depends on it.

## Measured numbers
- Machine state: other packages' builds and test runs were in the second heavy slot throughout (4 vCPUs shared), so the timings are pessimistic.
- `tests/txn_lifecycle.rs`: 13 tests, 2.4–4.5 s wall time per run (debug build). With `--features rocksdb`: 14 tests, 3.9 s.
- `src/txn_commit_tests.rs`: 3 tests, 0.27 s. With `--features rocksdb`: 5 tests, 0.47 s.
- Bench smoke gate: release build of `gbuild` + `gstore-bench` at f3a89c8 (`cargo build --release --locked -p gstore -p gstore-bench --bin gbuild --bin gstore-bench`), `GSTORE_BENCH_CACHE` warm, `target/release/gstore-bench ci --baseline bench/baselines/smoke.json`: PASS, 456 gates checked, 0 failed (148 table rows, all pass). Time geomean against the committed baseline: LUBM-like snapshot 0.99, disk 1.06, WatDiv-like snapshot 1.15, disk 1.19 (bound 1.50). Report-only `begin_ms` was 0.13–0.14 ms, against 0.39–0.45 ms in the baseline. Machine state: a RocksDB test run and other packages' builds were running (load average 11–13 on 4 CPUs), so the figures are pessimistic. Commit 651de44 changed only the order of a commit's frees, back to the parent's, so it was not measured again. The request path's change is one atomic read per request.
- A finding during verification: the first version wrote a committed transaction's id frees with the next commit point. `tests/dt_reclaim_crash.rs` then saw 9 orphan terms after a kill in the Stream layout, where 4 are tolerated. 651de44 put the frees back into the commit's own commit point, and three further runs passed.
- Conformance: `scripts/conformance.sh --no-build` (dev binaries): all six suites pass. basic, update, transaction, api and python (49/49) pass outright; parser has failing cases 15 and 17, both known. No compat switch was needed.

## Requests/Not yet
- Edits outside the owned files, each the smallest change that fixes an id:
  - `src/kvstore/pager.rs`, `src/kvstore/store.rs`, `src/kvstore/disk_triples.rs` (gap-txn-lifecycle#3): one counter field, its increment after `write_wal`, and the `wal_batches` accessors.
  - `src/backend/rocks.rs` (gap-rocksdb-exec#5): new `rollback_uncleared_journal`. The body of `rollback_keeping_journal` moved, unchanged, into a private `replay_kept_journal` they share.
  - `src/db/persist.rs`: new `rocks_rollback_uncleared_journal` after `rocks_rollback_journal`.
  - `src/db/mod.rs`: `begin` is unchanged; `commit` and `rollback` were rewritten, and `CommitFailure` / `RocksUndo` were added after `enum UndoOp`.
  - `src/lib.rs`: one `#[cfg(test)] mod txn_commit_tests;` line.
  - `src/concurrent.rs` (review fix, follows gap-txn-lifecycle#3): `apply_txn_ops`'s commit arm and its doc comment, the two comments in `Txn::commit` next to its call, and the `CommitFailure` import. No R2 package owns the file; r4-concurrent owns it in R4.
  - `src/config.rs`: one `upstream_compat_warnings()` entry for `txn_idle_timeout=0` (convention 13(c)).
  - `src/http_api.rs`, regions granted by the amendments: the `ApiInner::handle` line, the `serve_forever[_tls]` lines, and the fields and initialisers at their anchors. Also the module docs' Transactions paragraph.
  - `ApiState::remember_ended` and `ApiState::ended_outcome` keep their signatures, so `finish_shutdown` is not edited.
- Expected merge conflicts:
  - `tests/dt_gapi_core.rs`, `tests/dt_http_durability.rs`, `tests/dt_gapi_users.rs`, `src/gapi/auth.rs` (tests module), `src/db/review_tests.rs`, `src/db/r4_review_tests.rs` and `src/config.rs` (tests module): only the assertions or setup that pinned the replaced behaviour, corrected under convention 13(b).
  - r2-index-cancel's `ApiServer::serve_forever` / `serve_forever_tls`: each gets one line (`let _txn_reaper = self.inner.start_txn_reaper();`). This overlap is sanctioned.
- `begin_target` is now a method (`&self`) rather than an associated function. Its two callers are `handle_begin` and gapi `begin()`, both in this package.
- Divergences from upstream (Rust extensions): idle timeout; rollback without the update privilege; superusers end any transaction; `<begin>_<tid>` checked; the `transaction` monitoring object.
