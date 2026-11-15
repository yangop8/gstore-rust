# r1-storage-failstate — Storage write-failure state machine: pager/WAL never torn, broken DiskSink/DiskTriples, recovery by unload/reload

## Changelog

Behaviour changes first:

- **A failed commit of an on-disk database can no longer tear its file**
  (review C-1: durability#1, disk#1). If a commit fails after its
  write-ahead log got its name (applying a page, an fsync, removing the
  log; for example the disk filled up while the batch was applied), the
  pager is now *failed*: every later write, page allocation and commit is
  refused with the new `GStoreError::StorageFailed`, reads keep working,
  and nothing writes the file again until the database is reopened. The
  reopen replays the complete log, so the file holds exactly the last batch
  whose log was complete, never a mix. Before, the next commit point, save,
  shutdown or drop retried the commit, truncated the only complete log,
  failed again while rewriting it, and left a database that no longer
  opened at all (`I/O error: failed to fill whole buffer`), including the
  data committed long before.
- **Materialize mode: a failed commit point now breaks the database**, as a
  failed write-through already did. Every later commit point reports the
  failure (`storage failed: …`) instead of retrying, and dropping the
  database commits nothing. In Stream mode a commit that the out-of-core
  dictionary makes (ending a term write that outgrew the page cache) and
  that fails now breaks the store too. Recover by reopening the database
  (on a server: `unload` + `load`, or just `load`, see below).
- **`unload` of a database whose on-disk store failed now succeeds**
  (review M5: gap-txn-lifecycle#4). `/api unload` and REST `/unload` retire
  it *without* a commit point (none can succeed) and say so in a new
  `warning` field: the failure and how many changes since the last
  successful commit point are discarded (a commit that failed after its log
  was complete is replayed when the database is loaded again). The `/api`
  answer stays HTTP 200, StatusCode 0, `"Database unloaded."`; REST answers
  200. The same text is logged at warn level. Before, unload failed with
  1005 / 500 "it stays loaded" and only a restart of the whole server
  recovered the database.
- **`load` of a loaded database whose on-disk store failed reopens it in
  place** (`/api`: `"Database loaded successfully."` + `warning`; REST:
  `database '…' reopened` + `warning`). A healthy loaded database keeps
  the upstream answer ("The database already load yet." / "already
  loaded") at once: the failure is checked without waiting for the
  database and without reserving it, so such a load never waits behind a
  running statement nor makes the database busy for a concurrent begin or
  unload (guard test `tests/load_of_a_busy_database_answers_at_once.rs`,
  added by the round-1 review: an UPDATE parked on a held `SERVICE`
  endpoint, `/api load` and REST `/load` answer while it holds the
  database, and a `begin` issued meanwhile succeeds; it fails with
  2007ec7's two hunks reverted). A broken database that is busy at that
  very moment is answered "already loaded" too; load it again (or unload
  it). REST `/build` over a
  loaded broken database now replaces it instead of failing on the save
  that could not succeed.
- **The write-ahead log is never truncated or rewritten in place** (review
  durability#1, durability#8, robust#12): a commit stages the log as
  `<file>.wal.tmp`, fsyncs it, renames it to `<file>.wal` and fsyncs the
  directory before the main file is touched, and fsyncs the directory again
  after removing the log. A failure before the rename changes nothing on
  disk and stays retryable at the pager level. A leftover `*.wal.tmp` (a
  crash while staging) is not a commit: the next open removes it.
- **An unreadable write-ahead log fails the open** (review M1: disk#2,
  durability#7, robust#12). Only "not found" means "no log"; any other
  read error (EIO, EACCES, a directory in its place, …), or a log that
  cannot be removed after its replay, fails `Pager::open` and so the
  database open, instead of opening a possibly half-applied file whose log
  the next commit would overwrite. The bulk install's rule "a failed replay
  keeps the old log" now holds for an unreadable log too.
- **A `kvstore.kv` (or `vstree.kv`) shorter than its header's page count
  is refused at open** ("torn or truncated") instead of failing later reads.

New API and tests:

- `GStoreError::StorageFailed(String)`; `Pager::failure`,
  `Pager::failure_flag`, `Pager::mark_failed` (and the `FailureFlag` type);
  `DiskTriples::unsynced_writes`; `Database::storage_failure` (why the
  on-disk store is broken, with the number of changes that are not durable).
  A broken `DiskTriples` *is* its pager's failed state (they share one flag).
- Test-only pager failpoints (`kvstore::pager::failpoints`: fail the apply
  after k pages, fail the WAL write after n bytes, fail the WAL removal,
  unreadable WAL, a directory-fsync counter) and pager / DiskTriples unit
  tests built on them, including a dictionary commit that fails before and
  after its log got its name (both break the triple store sharing the
  pager).
- `tests/storage_fault_injection.rs`: the review's two-ENOSPC scenario with
  `RLIMIT_FSIZE` children (SIGXFSZ ignored): library Materialize and Stream
  with a clean drop and with a crash, small-cache variants, `gserver` +
  kill -9 followed by `gquery` and `gcompact --check`, and an unreadable log
  failing the open (Linux, 64-bit).
- `tests/server_broken_store_recovery.rs`: a broken Stream / Materialize
  database is unloaded, reloaded and reopened in place through `/api` and
  REST, and replaced by REST `/build`, without restarting the server.

No on-disk format change: the WAL framing is unchanged and the staged
`*.wal.tmp` file is transient. Files written by older binaries open
unchanged; older binaries ignore a leftover `*.wal.tmp`.

## GAP_PLAN status line

none

## Docs notes

- docs/DEPLOY.md (operations / troubleshooting): what a `storage failed`
  reply means and how to recover. Suggested text: "A reply or log line
  `storage failed: …` (or `the on-disk store is broken by an earlier write
  error …`) means a commit of that on-disk database failed — typically the
  disk filled up or an I/O error occurred. The database then refuses every
  further write (Stream mode: every statement) until it is reopened; other
  databases are unaffected. The file on disk is consistent: the last commit
  whose write-ahead log was complete is replayed when the database is
  opened again. Free the space (or fix the I/O problem), then `unload` and
  `load` the database (`/api` operations or REST `/unload`, `/load`), or
  just `load` it, which reopens a broken loaded database in place (one
  busy with a request at that moment answers "already loaded": repeat). The
  answer's `warning` and the server log name the failure and how many
  changes since the last successful commit are discarded; clients were told
  of each failed update when it happened. In the single-database mode
  (`gserver DIR`), restart the server. A database whose `kvstore.kv.wal`
  exists but cannot be read is refused at open (`cannot read the write-ahead
  log …`): fix its permissions or the disk error rather than deleting it —
  it may hold the last committed batch."
- docs/DEPLOY.md: no new configuration key and no compat switch (this
  package adds none).
- docs/STORAGE_BACKEND.md (pager / WAL section): the commit protocol is now
  "stage `<file>.wal.tmp`, fsync, rename to `<file>.wal`, fsync the
  directory, apply, fsync, remove the log, fsync the directory"; a failure
  after the rename leaves the pager failed (`StorageFailed`) until a reopen
  replays the log; only `NotFound` means "no log" at open; a file shorter
  than `page_count × 4096` is refused. Format registry: no change (no
  feature bit; the staged file is never replayed, so older binaries stay
  correct; the WAL record framing is unchanged).
- docs/STORAGE_BACKEND.md (broken-until-reopen contract, around the
  existing lines on the broken state): a broken `DiskTriples` is now the
  shared pager's failed state, Materialize breaks on a failed commit point
  as well as on a failed write-through, and a server reopens such a
  database through `unload`/`load` without restarting.
- docs/REFACTOR_BACKLOG.md: (1) the four duplicated WAL path rules
  (disk#9, `persist::wal_of`, `bulk::commit::wal_of`, the pager's own) are
  unchanged; the staged-log name is private to the pager, and
  `remove_other_layouts` does not remove a leftover `kvstore.kv.wal.tmp`
  when it switches a directory to another layout (harmless: never replayed,
  removed by the next open of a disk database there). (2) A failed commit of
  an update larger than the page cache can still leave a prefix of that
  update durable (the existing D12 limit; the small-cache scenarios of
  tests/storage_fault_injection.rs assert exactly "a prefix, then nothing").
  (3) The single-database mode (`gserver DIR`) has no unload: a broken
  database there needs a restart (unchanged). (4) Optional optimization
  (durability#8, D9): the directory fsync after removing the log is what
  the plan asked for, but it is not needed for correctness — the next
  commit's rename-time directory fsync persists the removal, and a log that
  reappears after a crash is always the batch the file already holds
  (replaying it is idempotent). Dropping it would save one directory fsync
  per commit.

## Measured numbers

Bench smoke gate (D9), base 7bbf4ae (exported with `git archive`) and this
branch, release builds of `gbuild` + `gstore-bench`, run back to back in
one session on the shared 4-vCPU / 15 GB machine while other agents were
building and testing (load average 10–14; calibration score 61.8 ms base,
68.5 ms branch), same data cache:

    GSTORE_BENCH_CACHE=<cache> target/release/gstore-bench ci \
        --baseline bench/baselines/smoke.json --bin-dir <release dir> \
        --work-dir <dir> --out <result>.json

- Gate: **PASS** for both (exit 0).
- Query geomean (ms, base / branch): lubm-like disk 0.491 / 0.474,
  snapshot 0.499 / 0.501; watdiv-like disk 0.121 / 0.129, snapshot
  0.139 / 0.141. Warm open (ms): lubm disk 40.2 / 31.3, watdiv disk
  19.1 / 31.1 (noise; the open path only adds one `unlink` of a missing
  `*.wal.tmp` and a length check).
- The commit path (report-only, not gated): disk single-triple update
  median (one commit each) lubm-like 1.285 → 1.878 ms (insert),
  1.290 → 1.908 ms (delete); watdiv-like 1.167 → 1.576 ms (insert),
  1.478 → 1.900 ms (delete). That is the rename plus the two directory
  fsyncs each commit now makes (about +0.4–0.6 ms here). Chunk updates
  (5716 triples, one commit): lubm 423 → 438 ms insert, 710 → 694 ms
  delete; watdiv 567 → 530 ms, 963 → 767 ms. Mixed-phase write means stay
  within the run-to-run noise (2.5–7.8 ms both sides). Snapshot backends
  are unaffected (no pager).
- A quiet-machine re-measurement of the per-commit cost is left to the
  round's land step; if it matters, the directory fsync after removing the
  log could be dropped (see Docs notes, REFACTOR_BACKLOG item 4).

## Requests/Not yet

- Shared edits outside the owned files (all inside the regions the plan
  grants): src/error.rs (new variant `StorageFailed`, its Display and
  `duplicate` arms — sole editor this round); src/db/persist.rs: the
  `DiskSink` struct, `new`/`failure`/`apply`/`flush` and its `Drop` doc,
  and the new `Database::storage_failure` directly after `disk_guard()`;
  src/gapi/ops_db.rs: `unload()` and the reopen-in-place branch of
  `load()`; src/http_api.rs: `handle_load`, `handle_unload`,
  `retire_loaded_saved`. `wal_of()` was not changed.
- For r2-txn (gap-txn-lifecycle#3, M2): a Materialize commit point that
  fails now breaks the database, so a failed commit can no longer become
  durable on a later commit point; but the failed update stays visible in
  the working set until the database is reopened, and a commit that failed
  after its log was complete is replayed by the reopen (the client was told
  "failed"). The binary "applied but not durable" reply belongs there.
- For r3-rest-parity (typed errors, design#3): the single-database server's
  `/update` answers a storage failure as HTTP 400 `query error: storage
  failed: …`; it should be a 5xx. `Database::check_store_readable`
  (src/db/mod.rs) still builds a `GStoreError::Database` for a broken
  Stream store; it could use `StorageFailed`.
- Every id of the package is fixed; nothing is deferred.
