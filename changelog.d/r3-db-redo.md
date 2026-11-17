# r3-db-redo — Library-only redo/update log and incremental backup (D2: kept, defects fixed): crash-consistent logs, committed-only records, backup isolation

## Changelog
- **Behaviour change (review durability#6, db#8): the redo log and the update
  log are crash-consistent.** Both are library API only
  (`Database::enable_redo_log*`, `enable_update_log*`, `replay_*`,
  `backup_incremental`, `restore_incremental`); no server or CLI turns them
  on. The records of one commit — an auto-commit UPDATE request, a
  transaction, a direct-API mutation, a `materialize_rdfs` / `run_rules`
  run — are now appended with one `write`, and an auto-commit UPDATE's or a
  transaction's are appended *after* its commit point and `fdatasync`ed. A
  direct-API mutation (`insert_triple`, …), which has no commit point of its
  own, is appended when it is applied and `fdatasync`ed by the next commit
  point; `Database::sync()` is now also the logs' commit point (it appends
  what they are owed and `fdatasync`s them). Before, every record took four
  unbuffered writes and an open/close per triple (a 50-triple `INSERT DATA`
  opened `redo.log` 50 times), nothing was ever synced, and an UPDATE was
  logged before its commit point (in the update log: even when its commit
  point then failed).
- **db#8: a direct-API call is one commit of the redo log.** `insert_data`,
  `try_insert_data`, `delete_data`, `insert_sources` and `remove_sources`
  append their records with one `write` when they return (also when they
  fail or panic part-way: what they applied stays), instead of one
  open/`write` per triple; inside a transaction or an UPDATE request they
  go with its commit as before.
- **A request that panics part-way logs what it applied.** Like a request
  that fails, an UPDATE (or reasoning run) that panics keeps the
  operations it applied before the panic; their redo records are now
  appended on the way out, and the statement's redo hold ends with it. A
  hold left set kept the records of later direct-API writes back, and a
  later `rollback()` dropped them all.
- **Record framing (durability#6).** Every record now carries a CRC-32
  header token, `c=<8 hex>`; the last record of a commit carries `e=<8 hex>`
  instead (`REDO I 0 44 e=1a2b3c4d`, `REC <millis> <changed> <len>
  e=…`). Replay applies a commit only once it has read its `e=` record, so
  a transaction whose append a crash cut off replays not at all rather than
  in part. Records written before the token (every log written so far)
  replay as before, each record a commit of its own; readers that predate
  the token ignore it and read the new records unchanged. New module
  `src/db/logs.rs` (framing, the walk, the appender).
- **Torn tails (durability#6, db#8, robust#10).** A crash in the middle of an
  append leaves an incomplete last commit: `replay_redo_log` /
  `replay_update_log` now skip it (with a warning) instead of failing with
  "truncated redo.log body", and the next append (or backup) cuts it off and
  starts on the last whole commit. Before, the next session appended after
  the torn bytes and every later record was unreachable ("RDF parse error …
  unterminated IRI"). A damaged record in the middle of a log (checksum,
  terminator, or a header with a record header after it) is reported as
  corruption (`redo.log is corrupt at byte N: …`), never skipped silently.
  A record that runs past the end of the file is the torn tail unless a
  whole record (complete, its checksum right) follows it — then its length
  field was damaged and it is corruption; text inside its body that merely
  looks like a record header (an update-log statement may hold a line that
  starts with `REC `) does not count.
  An increment's `incr.log` must replay whole: an incomplete one, or one
  whose length disagrees with its `INCREMENTAL` marker, fails
  `restore_incremental`.
- **Behaviour change (robust#10, durability#6): a failed append is reported
  and nothing is lost.** An append that fails (`ENOSPC`, `EFBIG`, a failed
  `fdatasync`) is cut off again at once, so the file still ends on a commit
  boundary, and its records stay owed, in order, to the next append. An
  auto-commit UPDATE whose records could not be appended returns an error
  ("the update is applied and committed, but appending to redo.log '…'
  failed: …"); `commit()` reports `CommitFailure::CommittedWithError`;
  `sync()` reports it; a direct-API write cannot (its `bool` is unchanged),
  and `backup_incremental` refuses while commits are owed ("… an increment
  taken now would miss them"). An unreadable log (an I/O error other than
  "not found") is an error for `backup` / `backup_incremental`; it used to
  pass for an empty log.
- **db#2: committed records survive a rollback and `disable_redo_log`.**
  Records of committed changes whose append failed are owed by the log
  writer, not kept in the transaction's buffer, so a later `rollback()` (or
  a commit that is not durable) drops only its own records.
  `disable_redo_log` / `disable_update_log` write the owed records first; if
  that fails again, they are kept and written first when the same file is
  enabled again (enabling another file abandons them).
- **Behaviour change: the update log takes committed statements only.** The
  statements of a transaction are appended at its commit; a rolled-back
  transaction's never reach `update.log` (they used to, so
  `replay_update_log` re-applied rolled-back statements).
- **db#3: inferred triples are in the redo log.** `materialize_rdfs` and
  `run_rules` now log the triples they add (one commit per run; inside a
  transaction at its commit), so a restore from a base and its increments
  has them.
- **db#9: crafted lengths are errors.** A length field beyond `usize` or a
  sum that overflows fails `replay_redo_log`, `replay_update_log` and
  `restore_incremental` with an error; it used to panic ("attempt to add
  with overflow", in release builds an out-of-range slice).
- **gap-doc-claims#1: unknown ops are refused, and clears are refused by old
  readers.** The replayer refuses an op other than `I`, `D`, `C`, `N` before
  reading its body (`unknown redo.log op 'X' at byte N`), an `I` / `D`
  record without a triple, and a malformed clear record; all of them used to
  replay as nothing. `CLEAR` / `DROP` / `MOVE` / `COPY` records (`C`, `N`)
  now carry the placeholder body `<urn:x-gstore:redo> <urn:x-gstore:redo#op>
  "clear" .`, which readers older than clear records refuse with "unknown
  redo.log op 'C'"; the empty-bodied clears written until now are still
  replayed.
- **db#13: `restore_incremental` never writes the base backup.** A snapshot
  base is read in place without taking its lock (it used to create or
  rewrite `LOCK`); a disk or RocksDB base is copied (`Database::backup_dir`)
  into a scratch directory, opened and detached there, and the copy removed
  (it used to be opened in place, which rewrote `kvstore.kv`'s header and
  wrote `vstree.kv` when the tree was stale). A base on read-only media can
  be restored now. (Round-3 review correction: the scratch directory was
  `gstore-restore-<pid>-<n>` in the system temporary directory, often a
  small tmpfs, and reused whatever another user had put there; it is now
  created exclusively, mode 0700, beside the base — the temporary directory
  only when the base's parent cannot be written — see r3-integration's
  round-3 review fixes.)
- **Behaviour change (db#14): `Database::backup` inside an open transaction
  is refused** ("backup: commit or roll back the open transaction first").
  It used to capture the transaction's uncommitted writes, which no later
  increment undid when it rolled back. The servers already refused it.
- **conc#13: backups follow contract C8's lock order.** `backup` and
  `backup_incremental` copy the reasoning rules (rule-directory lock) before
  they take `persist_lock`; they took them the other way round. `backup`
  takes the target directory's lock before it copies the rules there, so a
  directory another process has open is still refused before anything is
  written into it (review db#11). A backup's checkpoint now moves only once
  its `BACKUP_OFFSET` marker is written.
- **tests-b#18.** `a_read_error_fails_only_its_own_statement` now checks what
  it claimed (no cache entry, re-evaluated, fails again), and
  `a_cacheable_query_that_hit_a_read_error_is_not_cached` pins the rule on a
  query the cache does take (a user function makes a query uncacheable).
- New tests: `tests/redo_log_durability.rs` (torn tails, all-or-nothing
  commits at every cut, a `SIGXFSZ` crash mid-append and an `EFBIG` append
  in `RLIMIT_FSIZE` child processes, a failed commit point logged nowhere
  (round-3 review correction: wrong when the failed flush had already
  given its write-ahead log a durable name, which the next open replays;
  the test now reopens the database and checks that the logs agree with
  it — see r3-integration's round-3 review fixes),
  owed records across rollback / disable, committed-only update log,
  inferred triples, crafted lengths, unknown ops, a line-for-line emulation
  of the pre-clear-record reader, old-format logs, a base backup that stays
  byte- and mtime-identical — snapshot, disk and, with `--features
  rocksdb`, RocksDB bases —, backup inside a transaction, a backup into a
  directory another process holds, one commit per direct-API batch call, a
  request that panics part-way); framing unit tests in `src/db/logs.rs`
  (including statement text that looks like a record header inside a torn
  record, and a damaged length field); the lock-order test
  `src/db/redo_backup_tests.rs`.

## GAP_PLAN status line
none

## Docs notes
- docs/STORAGE_BACKEND.md (format registry and the redo-log downgrade note,
  "redo.log / 增量备份里的 CLEAR … 记录 … 以 'unknown redo.log op 'C'' 失败"):
  the note was wrong for the logs written so far and must say: clear records
  written by Phase 6 up to round 2 of the review-fix campaign have an empty
  body, which pre-Phase-6 binaries (`eaf5320`, `6362b97^`) skip silently —
  restoring such an increment with them brings the cleared data back without
  an error; do not downgrade across one. Clear records written from this
  version on carry the placeholder triple `<urn:x-gstore:redo>
  <urn:x-gstore:redo#op> "clear" .`, which those binaries refuse with
  "unknown redo.log op 'C'" (checked against a line-for-line emulation of
  their reader in `tests/redo_log_durability.rs`). Registry note: the redo
  log, the update log and `incr.log` are not paged files, so the kvstore
  format registry (feature bits, `kvstore/format.rs`) does not cover them.
  The new framing needs no feature bit: the `c=` / `e=` token is a trailing
  header token every older reader ignores, so older binaries read the new
  insert/delete records exactly (one by one, without the all-or-nothing
  commit rule) and refuse the new clear records loudly; this version reads
  every older log (records without the token are commits of their own). The
  `INCREMENTAL` marker stays `GSTORE-INCREMENTAL 1`.
- CHANGELOG.md (the earlier "unknown redo.log op 'C'" downgrade statement of
  Phase 6): correct as above.
- README.md (Library API, redo log / incremental backup): the logs' commit
  rule (records of a commit appended after its commit point, `fdatasync`ed;
  direct-API mutations appended when applied — a batch call such as
  `insert_sources` or `insert_data` as one commit — synced by the next
  commit point; `sync()` syncs the logs), the update log's committed-only rule, the
  torn-tail and corruption behaviour of replay, the reported append failure
  and the `backup_incremental` refusal, `backup()` refused inside a
  transaction, and `restore_incremental` reading the base read-only (a
  scratch copy of a disk / RocksDB base beside it, or in the system
  temporary directory when the base's parent cannot be written: it needs
  about the base's size of free space there, besides the memory the
  restored database takes; round-3 review).
- docs/DESIGN.md §11 (contract C8, owned by r4-ext-docs): add the two log
  appenders (`redo_writer`, `update_writer`) as leaf mutexes under
  `persist_lock` (held for one append or tail scan; nothing is taken under
  them), and that `backup` / `backup_incremental` take the rule-directory
  lock before `persist_lock`.
- docs/REFACTOR_BACKLOG.md:
  - (durability#6, robust#10) Owed records live in memory without a bound
    while a log stays unwritable and direct-API writes go on (the pre-fix
    buffer grew the same way), and they are lost if the `Database` is dropped
    before an append succeeds (no flush on drop: `Database` has no `Drop`;
    a later increment chain then has a gap that nothing reports). A cap with
    a sticky "gap" mark cleared by the next full `backup()` would bound it.
  - (durability#6) The logs are appended after the commit point, so a crash
    in the window between the two (one `write` and one `fdatasync`) leaves
    an on-disk or RocksDB store holding a committed statement its logs do
    not have; the next `backup_incremental` misses it without a word (the
    opposite order would log statements a crash then undoes). A two-phase
    record (prepared before the commit point, confirmed after) or a
    commit sequence number in the store would close the window.
  - (db#8) `replay_*`, `backup_incremental` and the appender's first scan
    read the log into memory (replay and backup did before); a streaming
    reader would bound memory for very large logs. The scan runs once per
    session (afterwards the appender knows the boundary) and
    `backup_incremental` reads only `[checkpoint, end)`.
  - (durability#6) Torn-versus-corrupt is decided by what follows the
    defect: a defective record (running past the end of the file, or with
    a wrong checksum or terminator) is torn unless a whole record (complete,
    checksum right) follows it. (Round-3 review correction: as merged, a
    checksum or terminator defect was corruption when any record *header*
    followed it, so a zero-filled or changed last update-log record whose
    statement held a line starting with `REC ` was reported as corruption,
    and the appender cut the file at that line, inside the record, making
    the log unreplayable for good; both now use the whole-record rule and
    the appender cuts at the record's start.) Two contrived cases remain: a
    torn update-log statement whose own text holds a complete record with a
    valid checksum (a line `REC <n> <n> <len> c=<crc>` with exactly `<len>`
    bytes matching the CRC after it) is reported as corruption instead of
    skipped; and
    a record whose length field was damaged to run past the end, followed
    only by a torn record, is skipped with the torn tail instead of
    reported. A per-append marker (or a log-level sequence number) would
    remove both.
  - (durability#6) An auto-commit UPDATE that fails part-way has its applied
    (committed) part in the redo log but not in the statement-level update
    log, which cannot express a partial statement (as before).
  - (db#13, not in this package's regions) `Database::restore` says a
    snapshot backup "is never written", but `open` takes the directory's
    lock (writing `LOCK`); `restore_incremental` no longer does.
  - (db#14, not in this package's regions) `checkpoint()` / `save()` of an
    in-memory database inside a transaction still save the live state
    (documented on `save`); only `backup()` refuses now.

## Measured numbers
- Not a hot-path package (convention 8 lists pager, RocksDB, engine, topk,
  string index, optimizer, gfun and the HTTP read path); the smoke gate was
  not run. The changes on write paths are `apply_update_ops` (one `bool`
  store per request and, with no log enabled, one early return — no
  allocation, no lock), `run_update`'s `catch_unwind` (no cost unless a
  panic unwinds) and `redo_batch` in the direct-API batch calls (one early
  return when no redo log is enabled, which is every server); no read path
  changed.
- System calls of one 50-triple `INSERT DATA` on an in-memory database with
  the redo log on: base (the review's db#8 evidence, same code as
  `4f0e075`): 50 opens of `redo.log` and 0 fsync; branch: 1 open, 1
  `write` (3530 bytes, the 50 records of the commit), 1 `fdatasync`, plus 1
  `fsync` of the directory when the append created the file (none when it
  existed). Command: `strace -f -e trace=openat,write,fsync,fdatasync
  ./measure <dir>`, where `measure` is a scratch program
  (`Database::build_from_str` + `enable_redo_log` + one `INSERT DATA` of 50
  triples) compiled with `rustc -O --extern gstore=<branch libgstore rlib>`;
  measured while other worktrees were building (counts, not timings).

## Requests/Not yet
- Edits outside the package's listed regions (all small, for the
  integrator):
  - `src/db/mod.rs`: `mod logs;` + `use logs::LogKind;` and
    `#[cfg(test)] mod redo_backup_tests;` in the module list; five fields of
    `Database` (`update_pending`, `update_writer`, `redo_writer`,
    `redo_hold`, with `redo_pending`'s doc) and their initialisers in the
    constructor; the doc lines of `BACKUP_OFFSET_FILE`,
    `INCREMENTAL_MARKER_FILE`, `INCREMENTAL_LOG_FILE` and `backup_offset`
    (they said "update log", the offsets are the redo log's);
    `apply_update_ops` (the redo hold at its start, the update-log append
    moved after the commit point, the log calls on its failure and success
    paths); `failed_commit` and `undo_transaction` (one
    `update_pending.clear()` each, next to the existing
    `redo_pending.clear()`); new private items `CLEAR_BODY`, `UpdateRec`,
    `RedoOp`, `parse_redo_record`, `read_backup_detached`,
    `flush_update_log`, `log_commit`, `end_statement_logs`, `sync_logs`,
    `apply_redo_op` and the writer accessors; the old free functions
    `append_update_log` / `append_redo_log` are gone (replaced by
    `logs::frame_*` and `logs::LogWriter`).
  - `src/db/persist.rs`: `Database::sync` calls `sync_logs()` after its
    commit point (doc updated).
  - Review follow-up (same package, `src/db/mod.rs`): `run_update` (a
    `catch_unwind` around its statement body that logs the applied part
    of a request that panics and ends the redo hold, then resumes the
    panic); `insert_sources`, `remove_sources`, `insert_data`,
    `try_insert_data` and `delete_data` (their bodies wrapped in the new
    private `redo_batch`, placed after `flush_redo`); `backup` takes the
    target's `DirLock` before copying the rules. No other R3 package edits
    these functions.
- New files: `src/db/logs.rs`, `src/db/redo_backup_tests.rs`,
  `tests/redo_log_durability.rs`.
- Not done, with reasons: see the REFACTOR_BACKLOG notes above (bounded
  owed-record memory, streaming replay, `Database::restore`'s lock write,
  `checkpoint()` inside a transaction).
