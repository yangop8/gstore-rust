# r2-dirlock — Directory locks and locked saves/loads: inode-keyed DirLock registry, directory lock for saves to a new directory and for public loads, parent fsync on directory creation; layout-dispatch guard (layout matrix)

## Changelog

- **Behaviour change: a save into another directory takes that
  directory's lock, whatever the directory holds** (review db#11).
  `Database::save` and `Database::backup` into a directory that is not
  the database's home used to take the directory's lock only when it
  already held a database, so two savers creating the same new database
  (two `gbuild` runs of one name, a build beside a gserver `/build`)
  raced: the second save's snapshot recovery deleted the first one's
  staged members, and the first one then committed its remaining members
  over the second one's set. Both reported success and the directory was
  unopenable (`corrupt database: meta says N entities but dict has M`).
  Now the target is created first and locked for every save that is not
  at home — a new or empty directory too — so a save into a directory
  another process holds fails at once with the usual "is locked by
  another process" error and writes nothing; the lock is released when
  the save returns. Within one process the lock is shared, so it gained
  a writer exclusion (`DirLock::writing`): every save into a foreign
  directory holds it, and a snapshot save at home holds its own lock's —
  two saves of one process into the same directory are serialized (the
  second waits) and never install a mix. A new or empty save target now
  holds a `LOCK` file afterwards, as every directory a database was
  opened from already does.
- **Behaviour change: `Database::load_disk` / `load_disk_with` take the
  directory lock** (review db#17). These public functions open — and
  write: WAL replay, legacy-shadow reconcile, VS-tree rebuild,
  write-through — an on-disk database, but took no lock, so an embedder
  using them beside a gserver had two writers on `kvstore.kv` and its
  WAL. They now take the directory's lock as `Database::open` does and
  hold it for the database's life (shared with `open`'s own; signatures
  unchanged): refused while another process holds the directory.
- **The directory lock registry is keyed by the locked file, not by the
  path** (review db#10). A directory moved away with its lock (a restore
  swap, an `/api` rename) lent its in-process registry entry to a new
  directory at the old path: `Database::restore_into` into a directory
  the process still had open, then `Database::open`, shared the stale
  entry and never locked the new `<dir>/LOCK`, leaving the restored
  database open to another process. The registry is now keyed by the
  (device, inode) of the locked `LOCK` file (the canonical path off
  Unix); `DirLock::acquire` shares an entry only when this process holds
  the very file `<dir>/LOCK` names and otherwise takes a fresh OS lock
  (retrying when the file is moved between open and lock). New
  `DirLock::holds(dir)`; `Database::hold_dir_lock` uses it instead of
  comparing the path the lock was taken under. Lock-failure messages keep
  their text.
- **New database directories are durable** (review db#12). A save into
  a new directory and a snapshot write created it with `create_dir_all`
  and only ever fsynced the new directory itself, never the parent whose
  entry names it. One helper, `fsutil::create_dir_durable`, now creates
  the directory and its missing ancestors and fsyncs the parent of every
  directory it created; `save` and `SnapshotWriter::begin` use it.
- **Layout-matrix guard** (reviews db#6, design#5, gap-doc-claims#7; D3).
  `src/db/layout_matrix_tests.rs` runs one script of every public write
  and maintenance operation — `INSERT`/`DELETE DATA` (default and named
  graphs), `DELETE`/`INSERT … WHERE`, `LOAD` (also `INTO GRAPH`),
  `ADD`/`COPY`/`MOVE`, `CLEAR`/`DROP` (`GRAPH`, `DEFAULT`, `NAMED`,
  `ALL`), the direct API, `begin`/`commit`/`rollback`, `save` into
  another directory, `checkpoint`, `sync`, `backup`/`restore`/
  `restore_into`, `reclaim_unused`, `materialize_rdfs`, `run_rules` and
  the upstream `reason_*` rule lifecycle — on every layout: snapshot,
  disk in Materialize and in Stream mode, and RocksDB under the
  `rocksdb` feature. After each step its result, the full content
  (default graph and every named graph), `triple_num` and
  `verify_integrity` must equal the snapshot layout's, and the step must
  survive a checkpoint and a reopen. The layouts are listed through
  exhaustive matches over `StorageKind` and `DiskQueryMode`, so a new
  variant does not compile until it joins the matrix. All four layouts
  agree today.
- Tests: `tests/dirlock_saves.rs` (db#10: a locked directory moved away
  and a new one at its path; `restore_into` into an open directory, then
  a reopen; `hold_dir_lock` after the held directory moved — db#11: a
  snapshot save, a backup and a disk-layout copy into an empty directory
  another process holds are refused and write nothing; a save into a new
  directory releases its lock — db#17: `load_disk` / `load_disk_with` in
  both modes refused while another process holds the directory, the lock
  held while open); unit tests in `src/db/dirlock.rs`:
  `two_saves_into_one_new_directory_never_interleave` (db#11: save A is
  parked after its first staged member through a test-only probe in
  `SnapshotWriter::stage`, save B runs; a test-only probe reports a
  writer that has to wait — before the fix the directory ended as B's
  dictionary with A's store and meta) and
  `creating_a_database_directory_syncs_the_parent_of_each_new_directory`
  (db#12, a thread-local record of the parents `create_dir_durable`
  fsynced); `src/db/layout_matrix_tests.rs`. Every behavioural test was
  run against the unfixed code and failed there (the unit tests with
  only the fix disabled, keeping the probes).

## GAP_PLAN status line

none

## Docs notes

- **docs/STORAGE_BACKEND.md**, section "不变量" (gap-doc-claims#7): the
  first sentence claims that a new backend only implements
  `TripleSource + MutableStore` plus the dictionary seam and that the
  engine is untouched. That holds for default-graph reads only. Restate
  it: default-graph reads (VS-tree, optimizer, graph algorithms, SPARQL
  engine) are generic over `TripleSource`; named graphs always live in
  the in-memory `TripleStore` (`Evaluator.named` is typed
  `Option<&BTreeMap<u32, TripleStore>>`, and SERVICE builds an
  `Evaluator<TripleStore>`; D11), and persistence, journaling,
  transactions and id reclamation are per layout in `Database`, which
  dispatches on the closed `Backend` enum (plus the Materialize sink)
  at about seventy places — a new layout means editing those. Suggested
  wording: "默认图的读路径(VS-tree、优化器、图算法、SPARQL 引擎)对
  `TripleSource` 泛型,新后端在这一面只需实现 `TripleSource +
  MutableStore` + 字典 seam。命名图始终在内存 `TripleStore` 中(D11),
  持久化、日志、事务与 id 回收按布局写在 `Database` 里(封闭的 `Backend`
  枚举加 Materialize sink,约 70 处分派):新增布局要改这些位置;
  `src/db/layout_matrix_tests.rs` 在每种布局上跑同一组写/维护操作,漏掉的
  分支会让它失败。" Also mention, next to the layout table or the commit
  points, that a save into another directory locks it (a new or empty
  directory too) and that `load_disk` / `load_disk_with` lock like `open`.
- **docs/REFACTOR_BACKLOG.md** (D3), new item under I.6 (db#6,
  design#5, gap-doc-claims#7): replace the scattered layout dispatch of
  the `Database` facade (`as_disk()` / `as_disk_mut()` 29x,
  `has_disk_sink` / `disk_guard` / `sink_mut` 17x, `as_rocks*` 15x,
  `as_memory()` 8x in `db/mod.rs`, `persist.rs`, `reclaim.rs`; the
  layout re-derived in `http_api` `ReadSnapshot::capture` and
  `concurrent.rs`; the wildcard in `bulk_target`; three layout-name
  parsers and two printers) by one explicit
  `enum Layout { Snapshot, Materialize { sink }, Stream { triples },
  Rocks { store, unpersisted } }` or a `LayoutDriver` trait owning the
  persistence hooks (on_mutation, on_named_quad, begin/rollback/commit
  journal, commit_point, intern_for_insert, frees, save_to, backup_to,
  snapshot_for_txn), plus one internal "applied change" journal through
  which every mutation path runs touched/undo/redo/write-through/
  note_removed/VS-tree in the documented C9 order; give `StorageKind`
  `FromStr`/`as_str`. Not done in round 2 (D3: no large refactor for its
  own sake); the guard `src/db/layout_matrix_tests.rs` catches a missed
  arm or a new layout meanwhile. The concrete divergences the review
  named are fixed under their own ids by other packages (db#7 r2-txn,
  db#3 r3-db-redo, db#4 r4-db-graph).
- **docs/REFACTOR_BACKLOG.md**, I.6, a follow-up of db#11 (reason:
  outside the finding; the module docs make it the caller's job): within
  one process, opening a snapshot directory (its recovery discards
  staged snapshot members) while another thread saves into that
  directory is not serialized by the writer exclusion — only saves are.
  The servers never do this (per-name reservations); a library user
  must not.
- **src/db/dirlock.rs module docs** (updated in code): the in-process
  rule (shared per locked file), which file is locked (inode key), and
  the writer exclusion of saves.

## Measured numbers

No hot path is touched (D9): the query, pager, RocksDB, string-index
and HTTP read paths are unchanged. A save at home of a disk or RocksDB
database (the servers' `check_point` after every write) gains one
`stat` (the existence check of `create_dir_durable`); a snapshot save at
home gains a `canonicalize` and a `stat` (whether the database's own
lock is on the directory) and an uncontended mutex; a save into another
directory gains one lock-file open and `flock`. The bench smoke gate was
not run for this package for that reason. Test runtimes (debug build,
4 vCPUs shared with other builds): `db::layout_matrix_tests` 6.1 s
without the `rocksdb` feature (3 layouts), 3.0 s with it (4 layouts,
opt-level 1 test profile in the shared rocksdb target dir);
`tests/dirlock_saves.rs` 0.03 s; the two new dirlock unit tests 0.3 s.

## Requests/Not yet

- **Edit outside the owned regions** (one line, conflict-free in round
  2): `src/db/persist.rs` `Database::hold_dir_lock` — its "already
  holds `dir`" test now calls `DirLock::holds(dir)` instead of comparing
  `DirLock::dir()` with the canonical path (db#10: after a move the old
  comparison kept a lock on the moved directory).
- **db#12, remaining directory-creation sites for r3-storage-kernel**
  (which builds on this round's fsutil): `src/bulk/commit.rs`
  (`fs::create_dir_all(dir)` before a bulk build, line ~188),
  `src/bulk/rocks_sink.rs` (~770), `persist.rs`
  `build_disk_from_sources_incremental` and `create_rocksdb`, and
  `stage_restore`'s `create_dir_all(parent)` still create a database
  directory (or its parent) without fsyncing the parent; switch them to
  `fsutil::create_dir_durable`. They are outside this package's regions
  (r2-layout-cli owns the build/replacement functions this round); the
  plan scoped db#12 to `save_locked` and `SnapshotWriter::begin`, both
  fixed.
- **For r3-rest-parity** (types the lock errors in round 3): the save
  paths now return the existing `DirLock::acquire` messages ("database
  directory '…' is locked by another process (pid N): …") for a save or
  backup into a held directory, also when the directory was new or
  empty; texts unchanged.
- **For r4-cli-config** (next editor of `dirlock.rs`, bulk locking):
  `DirLock` now carries a registry key and a writer mutex; a bulk build
  that wants the in-process serialization of saves can hold
  `DirLock::writing()` as well as its `BuildGuard`.
- **For integrators**: a new or empty directory a database is saved or
  backed up into now holds a `LOCK` file afterwards; no test in the
  workspace depended on its absence. The layout-matrix guard runs
  RocksDB only under `--features rocksdb` (CI's rocksdb job).
