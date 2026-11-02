# R3-REVIEW — Phase 6 Round 3 review fixes

Fixes for the confirmed findings of the Round 3 review (lenses rocks,
srv-locks, srv-semantics, stream-core, stream-persist, cross-lock-order,
cross-cutting), on `phase6` from `4b8c3e0`. Each fix has a regression test
that fails on `4b8c3e0` (see Measured numbers) unless noted there. A second
pass (lenses fix-correctness, fix-regressions) over those fixes, from
`b502624`, found six more (two of them one finding); their fixes are the
"Second pass" entries below, each with a regression test that fails with the
fix reverted.

## Changelog

- **RocksDB reads (`backend::rocks`):**
  - `so_by_p` on its seek path (a predicate with at least 32 triples per
    subject) checks the SPO iterator's status after every subject's run.
    RocksDB's `Seek` resets the status, so a read error (an EIO, a block
    checksum mismatch) that ended one subject's run was cleared by the seek
    to the next subject: the statement answered a silently shortened list —
    cacheable, as no error was recorded (C2). The error is now recorded and
    fails the statement.
  - `iter_all` sizes its predicate-ordered result from the `c|p` counters
    only when they add up, without overflow, to the global triple counter
    and the space can be reserved; otherwise it reads the SPO keys and
    sorts them (the same result). A damaged but well-formed counter used to
    abort the process on a huge allocation, or (wrapping) panic on an
    out-of-range slice. The other counter-sized capacity hints (`so_by_p`,
    `subs_by_p`, `objs_by_p`) only reserve what can be had.
  - A `GSTORE_ROCKS_BLOCK_CACHE_MB` value that is not a number of MiB
    (`64M`, `1g`, `64 MiB`) is logged once per process (`gs_warn!`) and the
    256 MiB default is used, like the other storage overrides; it used to be
    replaced by the default silently.
- **Bulk installs check the RocksDB store's lock again right before they
  replace anything** (`bulk::commit::refuse_open_store`, the disk and the
  RocksDB install). The check ran only when the build started: an older
  binary (which takes `rocksdb/LOCK` but not the directory lock) that
  opened the store while the input was parsed and sorted had its store
  swapped out under it, losing every write it acknowledged. The build is
  now refused before the commit point and the old database kept; only the
  few milliseconds of the install itself remain uncovered.
- **The directory lock comes before every repair of an interrupted swap.**
  `Database::open` finished an interrupted restore (`<dir>.restore-*`) and
  an interrupted store install (`rocksdb.new` / `.old` / `.tmp`) before it
  took `<dir>/LOCK`, so a second process opening the directory while
  another installed a bulk-built store or restored a backup deleted the
  staged copy (and then the old database) and only afterwards failed on the
  lock — both copies lost while `gbuild` reported success. Now
  `Database::recover_restore` repairs only under the lock of the directory
  (of `<dir>.restore-old` while the directory is moved aside, its lock file
  with it), `open` locks before repairing a store swap (also when only swap
  leftovers are there), and either fails, touching nothing, while another
  process holds the lock. The REST `/load` repair goes through the same
  function.
- **Every path that writes over a database directory takes its lock**
  (cross-process; a database of this process shares it):
  `Database::save` / `checkpoint` into a directory holding a database it is
  not served from (the snapshot `gbuild --force`), the incremental disk
  build (`gbuild --disk --incremental`, which also refuses a directory a
  database of this process has open, as the bulk build does),
  `Database::restore_into` (`grestore`) and `gdrop`. They used to replace or
  delete a served database's files (`kvstore.kv` unlinked under a running
  server, which went on acknowledging writes that a restart lost).
- **Stream mode (`Backend::Disk`):**
  - A broken store (a commit that failed part-way) refuses dictionary
    writes too: `DiskDict::with_write_check`, wired by `load_disk_stream`
    to `DiskTriples::write_check`. Direct inserts (`/upload`,
    `batchInsert`, `gadd`, `ConcurrentDb`) interned their terms before the
    store refused the triple; interning ends a pager operation that can
    flush the shared cache — durably writing the applied part of the failed
    transaction (the review reproduced one of its two triples durable after
    400 refused inserts with a 16-page cache).
  - UPDATE operations are applied in a read scope, and so are
    `remove_sources` (`gsub`, `/api batchRemove`) and `ConcurrentDb`'s
    deletes: a dictionary read error while a `DELETE DATA`, the delete half
    of `DELETE … WHERE`, `CLEAR` / `DROP GRAPH` or a removal looks up its
    terms fails the statement (C2). It used to read as "no such term", and
    the statement succeeded with nothing removed. An operation during which
    the store broke (a write error inside a transaction, reported as "not
    applied") fails at once instead of at the next statement.
  - `VersionView::query` materializes the version chains inside its read
    scope, with fallible lookups: a dictionary error fails the query (it
    answered with a deleted triple back, or a committed insert missing).
    The version GC keeps a record when the base cannot be read, instead of
    dropping a tombstone as "agrees with the base". New:
    `Dictionary::try_term_id`.
  - `csr()`, `preload_csr()`, `cached_csr()` and `csr_is_cached()` check
    the broken state (C5, as the plan's R3-STREAM 5 asks), and a failed
    commit clears the query and CSR caches: a CSR cached during the
    transaction (built from its overlay) was served from the broken store,
    and `/api loadcsr` answered "CSR already loaded".
  - `rebuild_index` makes the data durable (a commit point) before it signs
    the entities and renames the new `vstree.kv` in. Direct deletes not yet
    at a commit point were signed from the page cache: a crash (or failed
    flush) after the rename left a tree without the deleted edges beside a
    store that still held them, under a header saying the tree was fresh —
    not a superset (C9), so queries pruned real rows on every later open.
  - `use_streaming_vstree` signs in a read scope and fails on a read error,
    keeping the current tree; it used to build one with entities missing
    edges and mark it valid. `rebuild_index` of a RocksDB database does the
    same (the tree is dropped and the index left invalid).
  - Statistics an open rebuilds for a `kvstore.kv` written before they were
    maintained are written by the first commit point (`sync`, a
    checkpoint, the close of a read-only session): every Stream open of such
    a file used to rescan all triples (`DiskStore::stats_rebuilt`).
  - A read-only view's per-thread SPO cursor (`SpoWalk`) is valid for one
    write generation of the shared pager (`Pager::write_generation`, bumped
    by every page write and operation rollback): stepping a leaf copy older
    than a write by the writer sharing the pager answered without the
    writer's inserts, or with its deletes undone. (No shipped path kept a
    view across writes; the public `DiskStore::from_shared_pager` contract
    did not hold.)
  - The posting-list cache copies a hit after releasing its mutex: the copy
    of a long list (up to 32 MiB) ran under the one cache lock and
    serialized concurrent Stream-mode readers.
  - Restore validation (`stage_restore`) of an on-disk backup opens the
    staged copy in Stream mode and reads every term and every triple once,
    streamed, instead of materializing the whole database: a server that
    serves a database larger than its RAM in Stream mode could not restore
    it (OOM). The integrity coverage is the same (every dictionary array
    entry, every SPO key; the triple index must match the header's count).
- **Server (`http_api`, `/api`):**
  - `/api monitor` and `schema` never wait for a database a writer holds or
    waits for: on a cache miss (the first call after a write) they take the
    shared guard only if it is free (`DbCell::try_read`), else answer with
    the published counters and the class statistics computed for the
    instance's earlier data. Before, the first call after any write waited
    for a long query and the writer queued behind it (G4).
  - `/api monitor` / `schema` compute the class statistics, subject count
    and schema graph in a read scope: a read error answers `1005` and
    nothing is cached (C2). Before, the statistics of part of the data were
    cached and then served lock-free until the next write. An unloaded
    database's statistics are read the same way.
  - A read that finds stale `[query]` settings (after `refreshconf`, or on
    a freshly loaded database of a configured server) syncs them only when
    the database is free (`try_write`); otherwise it runs with the settings
    the database has. Waiting for the exclusive guard made every read of a
    busy database wait for its longest query (writer preference).
  - `/api backup` requests of one database in the same second (one
    `<db>.db_<time>` name) run one after the other again, from removing an
    older backup of the name to packing and rotating (a per-destination
    claim, taken before the database guard). With the shared guard of
    Round 3 they deleted each other's files mid-write: about half of three
    concurrent same-second requests failed with `1005 Backup fail: … No
    such file or directory`, sometimes all of them.
  - A write section that unwinds (a handler panic the server survives)
    marks the database's published figures unpublished; the next monitoring
    read publishes them when the database is free right now
    (`DbCell::figures`, `try_write`), else the next writer does. They used
    to report the data from before the change until some later write.
- **`[storage] default_layout` is documented as reserved** (it is read and
  checked, but no build path uses it until R4-SNAP): `conf.ini.example` and
  the `GStoreConfig` field say so, and `gserver` (at startup and on
  `refreshconf`) logs a warning while it is set.
- **Second pass:**
  - **REST `/batchRemove` fails on a read error.** It removed each triple
    with `Database::remove_triple` outside any read scope: a dictionary read
    error (Stream mode, a damaged dictionary page) reached only the
    process-wide counter and the log, and the triple counted as absent —
    `200 batch removed` with `failed: 1` while the triple stayed stored
    (C2; the first pass fixed `/api batchRemove` and `gsub`, not this
    path). New `Database::try_remove_triple` removes in a read scope and
    fails on a recorded read error or a broken store; the batch stops with
    `500 batchRemove stopped after applying N triple(s): …`, as
    `/batchInsert` does.
  - **A file-level backup repairs its source only under the source's
    lock.** `Database::backup_dir` (`gbackup`, the `/api` backup of an
    unloaded database, a restore's staging copy) finished an interrupted
    snapshot commit or store swap in the directory it copied without that
    directory's lock — and those names are also a save's in progress in
    another process. A `gbackup` during a `gbuild --force` over an on-disk
    database deleted the staged snapshot members; the save then committed
    without them and removed `kvstore.kv`, leaving no database while
    `gbuild` reported success. During a bulk RocksDB install it moved the
    staged `rocksdb.new` into place under the installer, which then deleted
    both stores. It now takes the lock first when there is something to
    repair, and fails, touching nothing, while another process holds it.
  - **`gbackup` holds the database's lock for the whole backup**, as the
    `/api` backup of an unloaded database does: a database another process
    has open (a `gserver` serving it, a `gbuild` writing it) is refused
    with "… is locked by another process (pid N)" — as it already was for
    the snapshot layout and, with the `rocksdb` feature, RocksDB, which
    `gbackup` opens. The on-disk layout (and RocksDB without the feature)
    used to be copied mid-write.
  - **Restores lock their target before anything is staged and hold it
    through the swap** (new `Database::lock_restore_target`: the target's
    lock, then the repair of an interrupted earlier restore, then the lock
    of a target that repair recreated). `restore_into` (`grestore`) locked
    only a target that existed beforehand: after a crash had moved the
    target aside, the directory the repair recreated stayed unlocked
    through the staging and the swap, so a `gserver` could load it
    meanwhile and acknowledge writes that the swap then lost. REST
    `/restore` and `/api restore` staged before they locked (the lock only
    covered the swap): another process opening a database the server had
    not loaded while its backup was staged deleted the staged copy, and the
    restore failed (`500` / `1005 … No such file or directory`). Both now
    lock first (the `/api` restore before unpacking a zipped backup):
    `409` / `1005 … locked by another process` while another process holds
    the directory.
  - **REST `/backup` takes the `/api backup`'s per-destination claim**
    (`BackupClaim`, moved to `http_api` and keyed by the resolved path),
    before it checks the destination and before the database guard.
    Concurrent REST backups of one database into its `<db>.backup` — all
    under the shared guard since Round 3 — checked and wrote the
    destination's owner record at once: all but one answered `409 …
    already holds data that is not a backup` or `500 … No such file or
    directory` (a Round-3 regression; the exclusive guard ordered them
    before). A REST and an `/api` backup into one directory are ordered too.
    (No acknowledged backup was torn: the per-database persist lock already
    serialized the copy itself.)

## GAP_PLAN status line

none

## Docs notes

- docs/CONFIG (or the `[storage]` table in README): `default_layout` is
  reserved — read, checked, without effect until R4-SNAP; gserver warns
  when it is set.
- docs/DEPLOY.md: `GSTORE_ROCKS_BLOCK_CACHE_MB` takes a plain number of MiB
  (`0` = RocksDB's own per-column-family caches); another value is logged
  and ignored. (The R3-ROCKS fragment says DEPLOY.md documents the variable;
  it does not yet.)
- docs/ARCHITECTURE (locking): an open repairs interrupted swaps only under
  the directory lock; saves, the incremental disk build, `restore_into` and
  `gdrop` take the lock of the directory they write; bulk installs check
  `rocksdb/LOCK` at the start and again before the swap.
- docs/ARCHITECTURE (server): `/api monitor` / `schema` report the earlier
  class statistics of a database busy with a writer; same-second `/api
  backup`s of one database are serialized.
- Second pass — docs/ARCHITECTURE (locking): `backup_dir` repairs its
  source only under the source's lock; `gbackup` holds the lock for the
  whole backup, every layout; every restore (`grestore`, REST `/restore`,
  `/api restore`) locks the target before it stages the backup and holds
  it through the swap. docs/ARCHITECTURE (server): backups into one
  directory, REST or `/api`, run one after the other.
- Second pass — docs/DEPLOY.md (Backups): `gbackup` refuses a database
  another process has open; back up a served database through the server
  (`/api backup`, REST `/backup`, `gconsole`'s `backup`). README's lock
  paragraph already lists `gbackup` among the tools refused while
  `gserver` has a database loaded; it is now true of every layout.
- Second pass — REST API docs: `/batchRemove` answers `500` (and stops)
  when a triple's terms cannot be read, like `/batchInsert`.

## Measured numbers

None for performance: these are correctness fixes. Two touch hot paths and
add no work there: the SPO status check runs once per subject on the
`so_by_p` seek path (no extra read), and a posting-list cache hit is the
same copy, made after the lock is released. The restore validation's peak
memory was not re-measured (the verifier measured 788 MiB peak RSS
validating a 3M-triple backup before the fix; the streamed validation holds
the page and dictionary caches and one entity's adjacency at a time).

Regression tests, each run against `4b8c3e0` with the fixes removed (a
`git archive` copy with only the tests grafted on) — all fail there, and
pass here:

| finding | test | on `4b8c3e0` |
|---|---|---|
| so_by_p drops SPO errors | `backend::rocks::tests::a_bad_spo_block_fails_so_by_p_instead_of_shortening_it` (an SST byte-flipped on disk) | no read error recorded, list short |
| iter_all sized by counters | `backend::rocks::tests::damaged_card_counters_do_not_size_iter_all` | abort: `memory allocation of 1688849860263936 bytes failed` |
| block cache env | `backend::rocks::tests::the_block_cache_variable_warns_unless_it_is_a_number_of_mib` | (new helper; old code had no warning) |
| install recheck | `dt_bulk_rocks::a_store_opened_during_the_build_is_not_replaced` (a FIFO input; the store locked mid-build) | built over the open store |
| monitor waits after a write | `http_api::tests::monitoring_after_a_write_answers_while_the_database_is_held` | `/api monitor` waited > 1 s |
| read_synced stalls reads | `http_api::tests::a_read_with_stale_settings_does_not_wait_for_other_reads` | the read waited 5.0 s |
| same-second backups | `http_api::tests::same_second_api_backups_of_one_database_all_succeed` | `1005 Backup fail: … No such file or directory` |
| stats outside a scope | `http_api::tests::monitor_statistics_hit_by_a_read_error_fail_and_are_not_cached` (rocksdb) | status 0, empty `subjectList` cached |
| VersionView materialization | `db::stream_review_tests::a_version_view_fails_on_a_dictionary_read_error` | `Ok([a, c, e])` (deleted `a` back) |
| unwound write section | `http_api::tests::a_write_section_that_unwinds_is_published_later` | figures stayed at 2 triples |
| broken store interns | `db::stream_review_tests::a_broken_stream_store_interns_no_terms` | about 800 terms interned after the failure |
| deletes on dict errors | `db::stream_review_tests::stream_deletes_fail_on_a_dictionary_read_error` | `DELETE DATA` → `Ok(changed: 0)` |
| SpoWalk through a view | `kvstore::store::tests::a_view_steps_no_leaf_copy_older_than_the_writers_writes` | after 16 reads: the writer's insert missing |
| csr on a broken store | `db::stream_review_tests::a_broken_stream_store_serves_no_cached_csr` | `csr()` answered |
| rebuild before durable | `db::stream_review_tests::a_stream_rebuild_never_installs_a_tree_ahead_of_the_data` | Stream query lost `<a>` after the crash |
| streaming vstree read error | `db::stream_review_tests::a_streaming_vstree_build_fails_on_a_read_error`; `a_rocksdb_vstree_rebuild_fails_on_a_read_error` (rocksdb) | tree built and marked valid |
| stats rebuilt every open | `db::stream_review_tests::statistics_rebuilt_at_open_are_saved` | `PRED_STATS` root still 0 |
| open repairs before the lock | `db::stream_review_tests::an_open_repairs_no_swap_another_process_holds` | the staged `rocksdb.new` deleted |
| gbuild / save / restore unlocked | `db::stream_review_tests::saves_builds_and_restores_over_a_locked_directory_are_refused` | snapshot saved over the locked directory |
| default_layout | `config::tests::storage_defaults_and_bad_values` (the warning) | (new method) |

Second pass: each regression test was run against `b502624`'s code paths —
the fixes reverted in place, the tests and the test-only restore probe
(`db::persist::restore_probe`, which records whether another process could
have locked a watched restore target while its backup was staged) kept —
and failed there; all pass here:

| finding | test | with the fix reverted |
|---|---|---|
| REST batchRemove unscoped | `http_api::tests::rest_batch_remove_fails_on_a_dictionary_read_error` (a Stream-mode database with its `ENTITY2ID` root page overwritten) | `200 batch removed`, `failed: 1`, triple kept |
| backup_dir repairs unlocked | `db::stream_review_tests::a_file_backup_repairs_no_save_another_process_holds` (staged `dict.bin.tmp` and `rocksdb.new`, the lock held) | `Ok` (repaired without the lock) |
| gbackup unlocked | `dt_persist_backends::gbackup_refuses_a_database_another_process_holds` (the test process holds the lock; `gbackup` is a child) | exit 0, "Backed up …" |
| restore_into after a crash | `db::stream_review_tests::a_restore_after_an_interrupted_one_holds_the_recreated_directory` | target lock free while staged: `[true]` |
| server restores stage first | `http_api::tests::restores_of_an_unloaded_database_hold_it_while_staging` (REST, then `/api` with REST's fix kept) | `[true]` for each |
| REST backups unclaimed | `http_api::tests::concurrent_rest_backups_of_one_database_all_succeed` (4 concurrent requests, 6 rounds) | 18 of 24 failed: `409 … not a backup of 'rbk'` (round 0), `500 … No such file or directory` |

`db::stream_review_tests::stream_deletes_fail_on_a_dictionary_read_error`
also checks `Database::try_remove_triple` now (new method).

No test for the posting-list lock hold (not observable without
instrumentation; the existing list-cache tests cover the unchanged
results) or for the restore validation's memory (the existing
`dt_phase0_review` corrupt / truncated backup tests cover its coverage).

Verification of this change (same VM, 4 cores; `CARGO_PROFILE_DEV_DEBUG=0
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`, rustc 1.99.1):

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --
  -D warnings`: clean.
- `cargo test --workspace --no-fail-fast` with `GSTORE_DISK_QUERY_MODE=memory`:
  2,231 passed, 0 failed, 2 ignored; with `GSTORE_DISK_QUERY_MODE=stream`:
  2,231 passed, 0 failed, 2 ignored.
- `--features rocksdb` (`CARGO_TARGET_DIR=target-rocks`): `cargo clippy -p
  gstore --all-targets --features rocksdb -- -D warnings` clean; `cargo
  test -p gstore --features rocksdb --no-fail-fast`: 2,078 passed, 0
  failed, 2 ignored (one test renamed its database after a first run, then
  rerun alone).
- No other feature-gated code was touched.

Verification of the second pass (same VM and settings):

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --
  -D warnings`: clean.
- `cargo test --workspace --no-fail-fast` with `GSTORE_DISK_QUERY_MODE=memory`:
  2,237 passed, 0 failed, 2 ignored; with `GSTORE_DISK_QUERY_MODE=stream`:
  2,237 passed, 0 failed, 2 ignored.
- `--features rocksdb` (`CARGO_TARGET_DIR=target-rocks`; the backup,
  restore and server paths changed are compiled there too): `cargo clippy
  -p gstore --all-targets --features rocksdb -- -D warnings` clean; `cargo
  test -p gstore --features rocksdb --no-fail-fast`: 2,084 passed, 0
  failed, 2 ignored.
- `--features zip` (the `/api` restore's zipped-backup path now runs under
  the lock): `cargo clippy -p gstore --all-targets --features zip -- -D
  warnings` clean; `cargo test -p gstore --features zip --no-fail-fast`:
  2,050 passed, 0 failed, 1 ignored. No `cfg(feature)` code was changed.

## Requests/Not yet

- By design, not a gap: `read_synced` leaves a database's new `[query]`
  settings unapplied while it stays busy with reads; the first request that
  finds it free, or its next write section, applies them. (The reviewer's
  alternative, a lock-free per-database settings cell read at statement
  start, would also remove that delay; an R4 refactor.)
- The bulk install's second `rocksdb/LOCK` check leaves the few
  milliseconds of the swap itself uncovered against an older binary (one
  that does not take the directory lock); every Round-3 binary takes it.
- `/api monitor` / `schema` of a database whose statistics were never
  computed for this instance still wait for a writer (there is nothing
  earlier to report); every later call does not.
- Second pass, not closed: a restore that creates its database (no
  `<dir>` and nothing moved aside) has no directory to lock while it
  stages. Another process opening the not-yet-existing `<dir>` meanwhile
  still removes `<dir>.restore-tmp` (its repair has no lock to take), and
  the restore fails; nothing is lost (there was no database, the backup is
  untouched). Closing it needs a lock that lives beside the directory
  rather than in it — an R4 item.
- Second pass, a behaviour change to note in the docs: `gbackup` of an
  on-disk database (or RocksDB without the feature) another process has
  open is now refused, as `gbackup` of the other layouts already was;
  back up a served database through the server.
