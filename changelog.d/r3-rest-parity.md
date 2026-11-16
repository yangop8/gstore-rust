# r3-rest-parity — Typed error classification (design#3) and the concrete REST-vs-/api divergences (design#4): REST /export, /upload registration, /build rules

## Changelog

Behaviour changes first:

- **One HTTP status per kind of failure, the same on every endpoint
  (review design#3).** Errors are classified once
  (`GStoreError::kind()`, mapped by `OpErrorKind::of` in
  `src/http_api.rs`, which the REST endpoints, the upstream `/api` layer
  and the single-database server share):
  - a database directory locked by another process (a gserver, `gquery`,
    `gconsole`, a bulk build) is **`409`** on every REST endpoint — before,
    `/load` and `/upload` answered it `500` while `/drop`, `/build` and
    `/restore` answered `409`;
  - an I/O error taking that lock (the lock file cannot be opened — e.g.
    `EACCES` — or the lock call fails) is **`500`** — before, `/drop`,
    `/build` and `/restore` answered it as a `409` conflict;
  - a store read error while a statement runs (a corrupt page, `EIO` of an
    on-disk or RocksDB store, contract C2) is **`500`** on REST `/sparql`,
    `/update`, `/tquery` and on the single-database server's `/sparql` and
    `/update` — before, `400 Bad Request`, which clients and proxies treat
    as their own error (no retry, no alert); a store broken by an earlier
    write error (`storage failed: …`) likewise;
  - a statement cancelled by `/cancel`, `operationTaskCancel` or a shutdown
    is **`503`** with the `{"error":…}` body (before `400`); a statement
    stopped by `query_timeout` stays `503` — on the single-database server
    also an UPDATE stopped after applying part of its operations (the
    message names the applied part; it was a `400`), as REST answers it;
  - syntax and evaluation errors stay `400`; an error that is not
    classified keeps the status its endpoint gave it before.
  The upstream `/api` keeps HTTP 200, its `StatusCode`s and its messages
  (a cancelled query or one that hit a store read error is still
  `1005 "Query fail: …"`; in a transaction, `tquery` still answers `1005`
  with the statement's message). Library: `GStoreError::kind()`
  and `gstore::error::ErrorKind` (`#[non_exhaustive]`: `Parse`, `Query`,
  `Locked`, `Conflict`, `Corrupt`, `Storage`, `Cancelled`, `Timeout`,
  `Other`); **new variant `GStoreError::Locked`** (no variant removed; it
  displays as `Database` did, so messages read as before — code matching
  `GStoreError` exhaustively needs an arm for it); `DirLock::acquire`,
  `SideLock::acquire` and `BuildGuard::enter` return `Locked` for a lock
  held elsewhere and `Io` (with the path in the message) when the lock
  cannot be taken at all; `gstore::store::read_scope::READ_FAILURE` and
  `read_failure()` name the statement read error every construction site
  builds.
- **REST `/export` writes every quad and runs as a cancellable, streamed
  task (review http#11, M13).** It uses the `/api` `export`'s serializer:
  N-Quads (the default graph, then every named graph) when the database
  has a non-empty named graph, else N-Triples; the new `format=nt|nq`
  parameter picks one (another value is `400`), and the `Content-Type`
  (`application/n-quads` / `application/n-triples`) and the attachment's
  file name (`<db>.nq` / `<db>.nt`) follow. Before, it silently dropped
  every named graph. While a transaction is open it still exports the
  committed state — now with its named graphs. The export is a task:
  listed by `/tasklist` (operation `export`), stopped by `/cancel` at its
  next statement (`503 {"error":"export cancelled"}`). It is written to an
  unnamed scratch file in the export directory under the database's shared
  guard and streamed from there in bounded chunks once the guard is
  released (with a `Content-Length`; never compressed, like a download) —
  before, it held a CONSTRUCT result and its N-Triples text in memory, the
  database's guard included.
- **REST `/upload` that creates a database registers it (review http#16).**
  The new database joins the registry (`system.json`) with the uploading
  user as its creator, and a registered name whose directory was removed
  outside the server (`gdrop`) loses its grants first, as REST `/build`
  and `/restore` do — before, a user granted on the vanished database could
  read the new one, and a brand-new name stayed unregistered (not counted
  by `max_database_num` once unloaded) until the next start.
- **A failed REST `/build` keeps the old database's reasoning rules and
  ledgers (review http#10).** A snapshot-layout rebuild removes the
  replaced database's `reason_rule_files` only once the new database is
  saved; a save that fails (disk full, `EIO`) serves the previous database
  again with its rules and ledgers — before, they were deleted before the
  save. A rebuild whose rule removal fails is registered but not served
  (`500` naming the directory to remove), as a bulk build whose install
  did not finish.
- **REST and `/api` lifecycle operations agree (review design#4):**
  - `/api` `load` finishes a restore that a crash left half-way (the
    database moved aside to `<db>.db.restore-old`, the copy staged as
    `<db>.db.restore-new`) and loads it — it answered `1005 "database[…]
    does not exist."`, where REST `/load` recovered it;
  - REST `/backup` records a directory it creates as a backup of the
    database (`BACKUP_OF`) only once the backup is complete, and removes it
    when the backup fails; an empty directory it wrote into is emptied
    again — before, a failed backup left a directory recorded as a backup
    of the database, which users allowed to restore only from that
    database's backups could restore. An earlier backup of the database
    that a failed backup was replacing keeps its record: each store is
    replaced as a whole, so the directory still holds a complete backup
    (the earlier one, or the new one where its store was replaced before
    the failure), and a retry into it — the default `<db>.backup` is reused
    by every REST backup of the database — is allowed;
  - REST `/restore` registers a database it creates at the backup's time
    (a `<db>.db_<YYYYmmddHHMMSS>` backup), as `/api` `restore` does —
    before, at the time of the restore;
  - REST `/batchInsert` / `/batchRemove` report `success_num` and
    `failed_num` as the `/api` operations do: `failed_num` counts the
    statements skipped as invalid, not duplicates (insert) or absent
    triples (remove) — those were counted as failed. REST's own `failed`
    field is unchanged.
- New tests: `tests/error_status_mapping.rs` (a lock another process holds
  is `409` on `/load`, `/upload`, `/build` in the snapshot and disk
  layouts, `/restore` and `/drop`; an I/O error on the lock file is `500`
  there; a store read error is `500` on REST and on the single-database
  server; a cancelled statement is `503`; syntax errors stay `400`; `/api`
  codes unchanged), `tests/rest_api_parity.rs` (the parity suite: export,
  export inside a transaction — also of a Stream-mode on-disk database,
  whose transaction reads a view of the committed pages — export
  cancelled, upload registration,
  failed rebuild, build/load/unload/drop, load after an interrupted
  restore, failed backups — over an earlier backup (kept and retried), and
  into a fresh and an empty directory under `RLIMIT_FSIZE` — restore
  registration time, batchInsert/batchRemove, users),
  `http_api::tests::errors_map_to_the_status_of_their_kind` (the status
  table) and `error::tests`.

## GAP_PLAN status line

none

## Docs notes

- README.md (REST endpoint table, `/export`): "the whole database:
  N-Triples, or N-Quads when it has a non-empty named graph; `format=nt|nq`
  picks one; a task listed by `/tasklist` and stopped by `/cancel` (503);
  streamed". Its old wording "the whole database as N-Triples" is wrong
  now.
- README.md / docs/DEPLOY.md (HTTP status codes of the REST endpoints and
  the single-database server, if listed): `409` = a lock another process
  holds (every lifecycle endpoint) or a transaction conflict; `500` = a
  storage or persistence failure, including a store read error during a
  query and an I/O error on a database's lock file; `503` = cancelled,
  timed out, result too large, shutting down; `400` = syntax / evaluation
  error. `/api` is unchanged (HTTP 200 + `StatusCode`). No compat switch
  is added (these are status corrections, not secure defaults; the
  upstream `/api` wire format is untouched).
- docs/DEPLOY.md (operations): REST `/export` writes its scratch file in
  the export directory (`[upload]`'s export directory, default
  `<root>/export`), unnamed while it is written: the directory needs room
  for one export per concurrent `/export`.
- docs/STORAGE_BACKEND.md: no format change. `GStoreError::Locked` is the
  error of a `DirLock` / `SideLock` / `BuildGuard` conflict.
- docs/REFACTOR_BACKLOG.md:
  - design#4 (D3): the shared operation layer — one protocol-neutral
    implementation of build/load/unload/backup/restore/drop/export/batch/
    users for REST and `/api` (e.g. `src/service/`), with `ApiInner`,
    `ApiState`, `DbCell`, `DbSlot`, `ReadSnapshot` moved out of
    `src/http_api.rs` — is not done. This package fixed every concrete
    divergence the review and the parity suite found and pinned each in
    `tests/rest_api_parity.rs`, which r4-api-hygiene extends; the two
    copies of restore, backup, drop, load and build remain. Deliberate
    differences documented on the handlers: REST `/build` rebuilds an
    existing database (`/api` refuses), REST `/drop` always deletes,
    REST `/restore` needs a path and leaves the database loaded, REST
    `/export` of a database in a transaction writes its committed state
    (`/api` refuses, as upstream).
  - design#3 (D3): most of the ~220 `GStoreError::Database(String)`
    construction sites stay untyped (`ErrorKind::Other`; each endpoint
    keeps its old status for them). Typed now: the directory locks
    (`Locked` / `Io`), statement read errors (classified by their
    `READ_FAILURE` prefix, which the local `read_failure` helpers of
    `src/db/mod.rs`, `src/concurrent.rs`, `src/zipio.rs` and
    `src/kvstore/store.rs` build — they should call
    `store::read_scope::read_failure`, and it could become a variant of
    its own), and the persistence failures the servers report themselves
    (`OpErrorKind::Persist`). Candidates for typing next: corrupt-file
    errors of `kvstore/format.rs` and `db/persist.rs` (`Corrupt`), "not
    found" errors of opens, capacity errors (id space exhausted).
  - The REST export's serializer for a transaction's snapshot
    (`ReadSnapshot::write_export`) duplicates the statement loop of
    `zipio::write_export`; a `TripleSource`-generic writer in `zipio`
    would serve both.

## Measured numbers

The error mapping and the export change no hot loop: a statement's success
path is untouched (the error closure runs only on failure), and `/export`
— not part of the bench — now writes a file and streams it instead of
building an N-Triples string.

- Bench smoke gate (D9; the HTTP read path's error handling changed):
  `cargo build --release --locked -p gstore -p gstore-bench --bin gbuild
  --bin gstore-bench`, then `flock /tmp/gstore-bench.lock env
  GSTORE_BENCH_CACHE=/home/user/fix-campaign/bench-cache gstore-bench ci
  --baseline bench/baselines/smoke.json` on this branch's head: `Gates:
  PASS (456 checked, 0 failed)`, calibration score 46.22 ms. Shared
  4-vCPU machine with other packages' builds running (load average 9-14);
  branch only — no base-vs-branch run, as no measured path changed.
- `scripts/conformance.sh --no-build`: every suite passes (basic, update,
  transaction, api, parser with its known cases 15 and 17, python 49/49);
  no compat switch added.

## Requests/Not yet

- Edits outside the owned files (each the smallest needed):
  - `src/server.rs` `ServerInner::handle_sparql`, `ServerInner::run_update`,
    `ServerInner::execute` (the sanctioned error→status mapping; `execute`
    now returns `Result<QueryResult, OpError>`), and the removal of the
    free function `failure_status` they were its only callers of.
  - `tests/dt_gapi_tasks.rs` (3 assertions), `tests/dt_http_durability.rs`
    (1) and `tests/gfun_limits.rs` (1): a cancelled statement's status
    `400` → `503`, each commented `review design#3` (convention 13b; the
    tests otherwise unchanged).
- Tests corrected (D6), in this package's own `src/http_api.rs`: the unit
  tests that expected `400` for a cancelled `/sparql` / `/tquery` and for a
  bulk build refused by another process's lock now expect `503` / `409`;
  `a_failed_snapshot_rebuild_keeps_the_acknowledged_writes` injected its
  fault by making the rule directory unremovable, which no longer precedes
  the save (http#10) — it now fails the save itself and still checks that
  the previous database is served again with the acknowledged write, and
  checks the unremovable rule directory separately (built, not served,
  `500`). Each is commented `review <id>`.
- Review correction (D6): the package's first `/backup` fix dropped the
  record of an earlier backup it was replacing before writing. A failure
  then orphaned a directory that still held a complete backup (each store
  is replaced as a whole), and `backup_target` refused every later
  `/backup` into it as "data that is not a backup" (`409`) — for the
  default `<db>.backup` path, every later REST backup of the database —
  and an empty directory written into stayed non-empty and unrecorded,
  with the same effect. The earlier backup now keeps its record and an
  empty directory is emptied again;
  `tests/rest_api_parity.rs::a_failed_backup_over_an_earlier_one_keeps_it_and_can_be_retried`
  (replacing `a_failed_backup_leaves_no_directory_recorded_as_a_backup`,
  whose first half pinned the dropped record) and the empty-directory case
  of `a_backup_that_fails_part_way_leaves_no_directory_on_either_surface`
  fail on the first fix.
- Review correction: once a statement's store read error was classified
  as a storage failure (`OpErrorKind::Failed`), `/api` rendered it through
  the arms of other failures — `query` lost its `"Query fail: "` prefix and
  `tquery` gained `"Transaction query fail: "` — so the `/api` messages of
  existing cases changed. Both render it as before again; pinned by
  `tests/error_status_mapping.rs::a_store_read_error_is_500_not_400`
  (which now also checks REST `/tquery` answers it `500`).
- Seen once, unrelated: `tests/no_background_thread_outlives_serve_forever.rs`
  `the_asynchronous_operation_runner_is_joined` failed in the stream-mode
  workspace run while a `--features zip` run built in the same target
  directory (load average ~13); it passed in the memory-mode run and three
  times in a row alone in stream mode. (The parallel zip run also replaced
  `target/debug/grestore` mid-run, failing three `dt_zip_io` CLI tests;
  rerun serially with `--features zip` they pass.)
- Divergence found for another package (r4-api-hygiene, parity suite): REST
  `/upload` with `format=nquads` or `trig` flattens every named graph into
  the default graph (`parse_rdf` uses `parse_str_triples`), where `/api`
  `batchInsert` / `build` put quads in their graphs. Not fixed here (the
  upload path inserts triple by triple; routing quads needs the
  `insert_sources` path) — the round trip REST `/export` → `/api` `build`
  is what `tests/rest_api_parity.rs` checks.
- The single-database server's partially-applied UPDATE stopped by
  `query_timeout` is now `503` (with the note naming its applied part), as
  REST answers it; r2-limits-auth's fragment says `400` — the closing step
  should keep the newer statement.
