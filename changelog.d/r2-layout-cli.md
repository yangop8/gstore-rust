# r2-layout-cli — Layout replacement and destructive CLI operations: one stage+swap+fsync primitive for incremental builds, create_rocksdb, backups, drops and renames; crash-atomic legacy shadow; vstree freshness in crash tests

## Changelog

- **A failed `gbuild --incremental --force` keeps the old database**
  (review M12 / durability#5). The incremental disk build used to delete
  the old `kvstore.kv` (and every other layout) before importing, and the
  incremental RocksDB build (`create_rocksdb`, behind every
  `Database::build_rocksdb_*`) discarded the old store before writing the
  new one in its place: a strict parse error, a full disk or a crash left
  an empty or unopenable database (reproduced: COUNT 3 → 0), against
  `gbuild`'s documented "a failed `--force` rebuild leaves the directory as
  it was". The incremental disk build now builds in the bulk build's
  staging area (`<db>.db/.bulk-tmp/`) and installs with the bulk build's
  crash-safe install (WAL replay of the old store, old VS-tree removed
  before the new store lands, the replaced database's reasoning rules and
  the other layouts removed after it); the incremental RocksDB build writes
  `rocksdb.tmp/` and swaps it in as a whole. Both hold the in-process build
  guard and refuse a RocksDB store another process has open, as the bulk
  build does. Behaviour change for library callers: `build_disk_incremental`
  / `build_disk_from_sources_incremental` now also remove the replaced
  database's `reason_rule_files/` (as `gbuild` and the bulk build always
  did).
- **Drops never leave a half-deleted database** (durability#10). `gdrop`,
  the REST `/drop` and the `/api` `drop` with `is_backup=false` ran
  `remove_dir_all` on the live `<name>.db`; a crash or an error part-way
  left a partial directory that the registry re-registered at the next
  start (a snapshot without `dict.bin` that no longer opens, a RocksDB store
  that reopens empty, with its old grants). Now the directory is renamed to
  `<name>.db.drop-old`, the parent directory is fsynced — from then on the
  database is gone — and only then deleted. Such a leftover is not a
  database to any scan (its name does not end in `.db`) and the next drop
  of the name removes it (`gdrop <name> --yes`, which still reports that
  the database does not exist; REST `/drop` and `/api drop`, which answer
  404 / 1005 as before). The `/api` logical deletion (`is_backup=true`,
  `<db>.db.bak`) is made durable too. New public API:
  `Database::drop_dir(dir)` (takes the directory lock, then the same
  rename-aside drop).
- **`gbackup` refuses a destination inside the database** (bulkcli#4).
  `gbackup x x.db` (also `./x.db/`, a directory inside it, a symbolic link
  to it) printed "Backed up", made no backup and deleted the database's
  pending `kvstore.kv.wal`. It is now refused before anything is touched
  ("backup of 'x.db' refused: the destination … is the database directory
  or lies inside it", exit 1), for zipped backups too; the library's
  `Database::backup_dir` refuses the same, and takes the lock of a
  destination that holds a database (one another process has open is no
  longer rewritten under it).
- **`/api rename` is durable and reports registry and grant failures**
  (durability#14). The directory rename is fsynced before anything records
  the new name. A failure saving the registry (`system.json`) or the grants
  (`users.db`) — it used to be ignored, silently losing the renamed
  database's grants — is answered as `1005 "Rename fail: …; nothing was
  renamed"` and undone (registry entry, then the directory, renamed back
  durably). Only a failure to forget the old name's grants after
  everything else moved is answered as a failure that says the rename
  happened (the start-up reconcile forgets those grants).
- **A backup never pairs a new `kvstore.kv` with an old `vstree.kv`**
  (durability#4). A backup refreshed over an older one (`gbackup` of an
  on-disk database, `Database::backup` / a save of an open one into
  another directory) replaced `kvstore.kv` before `vstree.kv`; a copy that
  failed or crashed in between left the new store, whose header says its
  VS-tree is fresh, beside the previous backup's tree, and
  `GSTORE_VS_POLICY=tree` then pruned every entity added since. The
  destination's VS-tree and write-ahead logs are now removed durably before
  the new store lands; `backup_dir` also removes a stale snapshot commit
  record, `meta.bin` and every database file the source lacks first, and
  copies `meta.bin` (the snapshot layout's marker) last, so a partial copy
  is never a mixed snapshot.
- **The legacy-shadow reconcile survives a crash part-way**
  (durability#9). A snapshot saved over an on-disk database whose save
  crashed before `kvstore.kv` was removed is reconciled into `kvstore.kv`
  at the next open, in as many page commits as the difference needs; those
  commits made `kvstore.kv` newer than the shadow, so an open after a
  second crash part-way skipped the shadow by mtime and served a mix of
  both states. The decision to apply a shadow is now recorded first in a
  durable marker, `SHADOW.apply`, bound to the store file's device and
  inode; an open that finds it applies the shadow again (idempotently)
  before retiring the shadow files (`store.bin` last) and the marker.
  Switching a directory to another layout removes the snapshot's commit
  record and `store.bin` first, and the disk layout's files before their
  write-ahead logs.
- Developers: `fsutil` has one stage + swap + fsync primitive for
  replacing and removing directories (review durability#15): `StagedDir`
  (staged beside its target, the whole tree fsynced, swapped in with the
  parent fsynced; `InstallError::committed` tells a failure after the
  commit point apart), `discard_db_dir` / `remove_drop_leftover` (drops),
  `rename_dir_durably`, `is_within`; `replace_dir_with_copy` (the backup
  copy of a `rocksdb/` store) goes through `StagedDir`.
  `bulk::commit::install_staged_disk` installs a disk database another
  builder staged. Crash-test hooks (environment, read once per operation,
  never on a hot path): `GSTORE_DROP_PARK` parks a drop right after its
  durable rename, `GSTORE_SHADOW_PARK=<n>` parks a reconcile after `n`
  committed changes.
- Tests: `tests/layout_replacement.rs` (new) — failed incremental disk and
  RocksDB rebuilds (strict parse error; an `RLIMIT_FSIZE` child for
  RocksDB) keep the old database; `gdrop`, a server's REST `/drop` and its
  `/api drop` killed with SIGKILL right after the rename leave no
  half-deleted database and the next drop removes the leftover; `gbackup`
  into the database is refused and the WAL survives; a failed backup holds
  no stale VS-tree (both copy paths); `/api rename` with a blocked
  `users.db` / `system.json` save is refused and undone; a reconcile killed
  after 3 of 10 changes is finished by the next open. `tests/dt_bulk_crash.rs`
  checks after every crash that the VS-tree the directory opens with agrees
  with its store under `VsPolicy::Tree` (durability#17; `NEW` gains a
  subject the old tree lacks, and a self-test plants an old tree beside a
  new store). `tests/dt_persist_backends.rs`' older-shadow test sets the
  shadow's mtime explicitly instead of sleeping 20 ms (tests-b#15).

## GAP_PLAN status line

none

## Docs notes

- docs/STORAGE_BACKEND.md (directory replacement / crash safety): builds
  never write over the database they replace — the bulk builds and now the
  incremental disk build stage `kvstore.kv` + `vstree.kv` in `.bulk-tmp/`
  and run the bulk install; the incremental RocksDB build stages
  `rocksdb.tmp/` and swaps it in (`rocksdb.tmp` → `rocksdb.new` →
  (`rocksdb` → `rocksdb.old`) → `rocksdb`, recovered by every open as
  before). A dropped database directory goes through `<name>.db.drop-old`
  (renamed, parent fsynced, deleted). A backup copy removes the
  destination's `vstree.kv`, WALs, snapshot commit record, `meta.bin` and
  every file the source lacks durably before it copies, and copies
  `meta.bin` last.
- docs/STORAGE_BACKEND.md (legacy shadow snapshots + format registry):
  `SHADOW.apply` is a transient recovery marker (content
  `kvstore.kv dev=<d> ino=<i>`), not a format change: no feature bit, no
  version change; an older binary ignores it and decides by mtime as it
  always did. `remove_other_layouts` removes it, and a stale one (another
  store file) decides nothing. No on-disk format changed in this package
  (D7).
- docs/DEPLOY.md (operations): an interrupted drop (crash, power loss, an
  I/O error while deleting) leaves `<name>.db.drop-old` beside the other
  databases; it is not listed, not registered and holds no data anyone
  reads — delete it by hand or drop the name again. `gbackup` refuses a
  destination inside the database directory. No configuration key and no
  compat switch (D1 table unaffected).
- README.md (`gdrop`, `gbackup` lines of the tools table, if it describes
  them): `gdrop` renames the directory aside durably before deleting it;
  `gbackup` refuses a destination inside the database.
- docs/REFACTOR_BACKLOG.md — durability#15 (design, D3), what remains after
  the stage+swap primitive: (1) the replacement paths still differ in
  shape: the bulk disk install (two files in place, 8 steps,
  `bulk::commit::install`, now shared with the incremental disk build),
  the bulk RocksDB install (`bulk::rocks_sink::install`, which duplicates
  `StagedDir::install`'s "where did the swap stop" recovery), the RocksDB
  save into another directory (`save_locked`, r2-dirlock's function this
  round, calls `install_staged_dir` directly), the restore
  (`install_staged_dir_as` with `RESTORE_SWAP`) and the snapshot set
  (`SnapshotWriter`); a single layout-replacement API owning the lock, the
  staging, the install steps and the recovery per layout would subsume
  them. (2) `backup_dir` / `copy_disk_files_to` stay file-by-file copies
  made safe by order, not a staged whole-directory swap: `gbackup`'s
  positional form writes into an existing directory that may hold files
  that are not the database's, which a swap would delete — a staged backup
  needs that contract changed first. (3) The flush-failure policy
  divergence the review lists under durability#15 (`DiskSink::flush`
  retries vs `DiskTriples::commit_point` poisons) is durability#16's, not
  touched here. (4) The first decision on a legacy shadow is still by
  mtime (`>=`), the only information a legacy build left; and
  `copy_disk_files_to` over a directory holding a snapshot written in the
  same second (on a file system with whole-second timestamps) would treat
  that snapshot as a newer shadow — the bulk install's
  `newer_than_snapshots` step is not applied there (low; needs two
  unlikely conditions). (5) Two discard variants remain: `discard_dir`
  (`STORE_SWAP` names, for `rocksdb/`, also handles a staged `.new`) and
  `discard_db_dir` (`DROP_SWAP`, whole database directories). (6) Leftover
  `<name>.db.drop-old` directories are removed by the next drop of the
  name, not swept at server start (the review's optional suggestion); the
  `/api drop` of a missing database removes a leftover without the name's
  busy claim (it only ever touches the `.drop-old` name).

## Measured numbers

- No hot path changed (D9): the query, commit and open paths are untouched
  except one extra `stat` per on-disk open (`SHADOW.apply` is looked for
  when there is no shadow), so the bench smoke gate was not run for this
  package. The paths changed are the incremental builds (one extra rename
  of two files and the install's fsyncs per build), drops (one rename and
  one directory fsync more), `/api rename` (one directory fsync) and backups
  (one directory fsync more).
- Test times on the shared 4-vCPU machine (other agents building):
  `cargo test --test layout_replacement` 9 tests in about 1–2 s (default
  features), `--test dt_bulk_crash` 9 tests in 8–16 s in both
  `GSTORE_DISK_QUERY_MODE` modes (the kill test is bounded at 25 s as
  before).
- Whole suites on this branch: `cargo test --workspace` 2549 passed, 0
  failed with `GSTORE_DISK_QUERY_MODE=memory` and with `stream`;
  `cargo test -p gstore --features rocksdb` 2401 passed, 0 failed;
  `cargo test -p gstore --features zip` 2337 passed, 0 failed.

## Requests/Not yet

- Edits outside the owned files (all small, no signature changed):
  `src/bulk/commit.rs` — new `pub(crate) fn install_staged_disk` (after
  `build_disk`), no existing function changed; `src/db/persist.rs` — inside
  the granted functions plus the new `Database::drop_dir` (public, after
  `backup_dir`), the private helper `store_identity` and the constants
  `SHADOW_APPLY_FILE` / `SHADOW_PARK_ENV`, and the module docs;
  `src/http_api.rs` `handle_drop` (granted); `src/gapi/ops_db.rs` `rename`
  and `drop_db` (granted). `save_snapshot`, `copy_disk_files_to` and
  `reconcile_legacy_shadow` keep their signatures.
- `tests/dt_bulk_crash.rs`: the shared `NEW` data now has 4 triples (was
  3) — the next owner (R3 r3-storage-kernel) should keep the extra subject
  triple, which is what lets the VS-tree check see an old tree.
- Not done (recorded above for the backlog): the full layout-replacement
  redesign (durability#15 items 1–6). Every id of the package is otherwise
  fixed.
