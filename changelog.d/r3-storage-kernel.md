# r3-storage-kernel — Storage kernel robustness: atomic cache invalidation, quad-aware id reuse, one WAL path helper + advisory lock, explicit free-id format choice, release-mode checks, crash-test assertions

## Changelog

- **Behaviour change: a pager file is locked against other processes**
  (review disk#9). `Pager::open` (so `DiskStore::open`, `DiskTriples::open`,
  every `kvstore.kv` / `vstree.kv` open) takes an advisory exclusive lock
  on the file (`flock` / `LockFileEx`, std `File::try_lock`) *before* it
  looks at the write-ahead log, and the process keeps it while any of its
  pagers on the file lives. An open from another process fails with
  "'…/kvstore.kv' is in use: another process has it open" and touches
  nothing — two processes used to be able to replay and delete each
  other's log and overwrite each other's pages. Within one process the lock
  is shared (as the directory lock is): a second pager of the same process
  is not refused, and keeping to one pager per file in a process stays the
  caller's job (documented in the pager's module docs). The last pager of
  the process releases the lock explicitly, so a child process being
  spawned at that moment cannot keep it alive until its `exec`. A file
  system without advisory locks opens unlocked, as before.
- **One write-ahead-log path rule** (review disk#9). `Pager::wal_path(path)`
  is the only derivation of `<path>.wal`; `db::persist::wal_of`, the bulk
  install's `wal_of` and `DiskStore::open_with` call it. New
  `Pager::remove_with_wal(path)` removes a paged file with its log (and a
  staged `.wal.tmp`) — the log first, so a log never outlives its file —
  and fsyncs the directory; only "not found" is ignored.
  `Database::use_streaming_vstree` uses it, so a stale log that cannot be
  removed now fails the call before anything is built (it used to ignore
  every removal error).
- **A term a named-graph quad uses is never freed** (review disk#3).
  `DiskStore::free_key` (the Materialize-mode write-through of a reclaimed
  term) checked `GSPO` only with a graph-id prefix, so a term used only as
  a quad's subject, predicate or object was freed and the next new term
  took its id — the quad silently changed meaning. The store now keeps
  reference counts of the quads' terms, gathered from `GSPO` by the first
  `free_key` that needs them (once, `O(quads)`) and kept current by every
  quad write (`O(1)`); a failed write operation or a refresh drops them.
- **Out-of-core dictionary caches: a value read before a free is never
  cached after it** (reviews disk#6, conc#12, db#20). `ClockCache` keeps the
  generation per shard and brings a shard to the cache's generation under
  its lock; new `get_at` / `contains_at` / `insert_at` (also on `TermCache`
  and `KeyCache`) take the generation the caller read at and drop an insert
  of an older one under the shard lock. `DiskDict` takes the cache epoch
  before it reads the pages, caches at that epoch, and a free moves the
  epoch while it still holds the pager's write lock. Before, a resolve or
  lookup racing a free could cache the old string of a freed id (the new
  term reusing the id then resolved to the old string) or a stale
  `key → id` (making `intern` answer a freed id without writing the term:
  two terms, one id). The legacy `check_generation` / `get` / `insert` keep
  their behaviour; `advance` and `generation` are new.
- **A panic under the pager's write lock no longer makes every read panic**
  (review conc#8). New `Pager::lock_read` / `Pager::lock_write` recover a
  poisoned lock; a poisoned lock marks the pager failed, so reads go on and
  every later write and commit is refused with `StorageFailed` (the reopen
  replays a complete log) instead of panicking. `DiskStore` (every read
  path, `commit`, `flush`, `mark_vstree_fresh`, `enable_reclaim`, write
  operations) and `DiskDict` use them; the pager's own cache mutex is
  poison-tolerant too.
- **Commits let readers run, and checksum faster** (review disk#10).
  `DiskStore::commit` (every commit point) now writes, applies and fsyncs
  the batch under the pager's *shared* guard (new `Pager::commit_shared`);
  only taking the batch and marking it clean (`Pager::finish_commit`) hold
  the exclusive guard — readers sharing the pager (the out-of-core
  dictionary, read-only views, ConcurrentDb snapshot readers) no longer
  wait for the log write and two fsyncs. A page a writer changes between
  the two steps stays dirty for the next commit. The log's CRC-32 is
  table-driven (slicing by 8; the bit loop took ~90 ms per 16 MiB batch)
  and the log is streamed through a 1 MiB buffer instead of being copied
  whole first. `Pager::flush` runs the same two steps under its `&mut`.
  The log format is unchanged.
- **The free-id format of a file is an explicit choice** (reviews disk#8,
  design#6). An existing `kvstore.kv` keeps the format its header records
  — version 1, or version 2 with the free-id list — whatever the process
  policy (`GSTORE_ID_RECLAIM`, `[system] id_reclaim`); only `gcompact
  --enable-reclaim` upgrades. The policy chooses the format of *new* files
  only and reaches them through explicit options: new
  `BulkOptions::free_ids` (default: the process policy when the options
  are made; the bulk image's header is written from it, no longer from
  the process policy) and new `OpenOptions::free_ids_if_new` /
  `with_free_ids_if_new` (default `true`; `default_open_options()` takes it
  from the policy when one is set) for an open that creates the store file
  (`Database::open_with` uses its options' value; `DiskStore::open` uses
  the process's default open options). `DiskStore::open` keeps its
  signature.
- **RocksDB: a dictionary id beyond its space fails the open instead of
  aborting the process** (review db#18). Loading the `dict` column family
  checked the id-space bounds only after sizing the slot vectors by the
  stored ids, so one corrupt entry with an id near `u32::MAX` allocated
  ~100 GB and aborted. Each id is now checked against its space's bound
  first and the slots are reserved fallibly: "corrupt RocksDB dictionary:
  entity id N is beyond its id space".
- `remove_sorted_from` (the in-memory store's batched list removal) skips a
  value the list does not hold in release builds too; it removed the next
  larger element there (review db#19).
- Tests: new `tests/storage_kernel_regressions.rs` (quad-aware frees, a
  poisoned pager lock, the cross-process pager lock both ways, a WAL that
  cannot be removed, existing files keeping their format under either
  policy, new files taking the explicit format, RocksDB dictionary ids
  beyond their space); unit tests for the cache races (a test-only hook
  parks a read between its page read and its cache insert), the
  `ClockCache` generation rules, a commit parked inside its log write while
  a reader reads, the table CRC against the bit loop, the lock registry and
  `remove_with_wal`. Corrected tests: `tests/dt_reclaim.rs` (review
  tests-b#4: the "lowest-first reuse" check was an accounting identity and
  `peak_live` was never read — now each space's id limit is bounded by its
  peak of live terms after a final reopen, and new deterministic
  lowest-first reuse tests run on Materialize, Stream and RocksDB through
  reopens), `tests/dt_reclaim_crash.rs` (review tests-b#8: the kill waits
  for the child's first acknowledgement and every iteration asserts one;
  a child that acknowledged nothing fails with its stderr),
  `tests/dt_bulk_crash.rs` (review tests-a#5: a log whose replay writes a
  marked page past the old store's end, observed through a hard link to
  the replaced file, proves replay on any file system; the failed-replay
  branch runs in an `RLIMIT_FSIZE` child so it fails on every file system).

## GAP_PLAN status line

none

## Docs notes

- docs/STORAGE_BACKEND.md (pager / format section): the pager file lock —
  advisory exclusive lock taken in `Pager::open` before WAL recovery, held
  per process (shared by the process's pagers, released explicitly by the
  last), other processes refused; the in-process rule (one pager per file,
  share it through `Arc<RwLock<Pager>>`); `Pager::wal_path` is the only
  `<path>.wal` derivation and `Pager::remove_with_wal` the checked removal.
  No format change: the WAL framing and every header are as before. The
  format registry gained no feature bit (see the backlog entry below).
- docs/STORAGE_BACKEND.md (id reclamation): `GSTORE_ID_RECLAIM` /
  `[system] id_reclaim` choose the format of new store files only, passed
  explicitly as `BulkOptions::free_ids` and `OpenOptions::free_ids_if_new`;
  existing files keep their header's format.
- docs/STORAGE_BACKEND.md (commit): a commit holds the pager exclusively
  only to take its batch and to mark it clean; the log write, apply and
  fsyncs run under a shared guard.
- docs/REFACTOR_BACKLOG.md — design#6 / disk#8 (D3): the layering moves are
  not done — `kvstore/store.rs` still imports `parser::{import, sparql}`,
  `parser::nquads::Quad` and `query::{Evaluator, QueryResult}` for
  `DiskStore::query` / `build_files` / `build_str` / `import_sources`;
  `dict/cache.rs` uses `query::engine::FxBuildHasher`; `http_users.rs`
  takes SHA-256/PBKDF2 from `query::hash`; `sysdb.rs` and `http_users.rs`
  call `http_api::valid_db_name`; `db::fsutil` and `sysdb::clock` serve
  unrelated modules; `bulk/commit.rs` depends on `db::{dirlock, fsutil,
  persist::remove_other_layouts}`; `DiskStore::open` reads the process's
  default open options. A `util` module at the bottom of the graph and
  moving the query/build helpers out of `kvstore` would touch nearly every
  module in parallel with every other package (reason: D3, crate-wide
  move). Only the explicit format choice of design#6 was done here.
- docs/REFACTOR_BACKLOG.md — disk#9 (D7): the write-ahead log still
  records no identity of its file. Binding them needs a new WAL framing
  behind a format-registry feature bit, which would make every new file
  unreadable for older binaries (and an older binary would discard a log
  in the new framing as torn). Not required: the cross-process lock, the
  single path rule and the checked, log-first removal close the failure
  paths the review names (a foreign log beside a fresh file, two openers).
- docs/REFACTOR_BACKLOG.md — disk#3 (Stream mode): `DiskTriples::free_ids`
  still leaves the named-graph check to its caller (`db/reclaim.rs` checks
  `NamedGraphs::node_refs` / `pred_refs` first); the store-level quad
  reference counts of `DiskStore::free_key` could serve it as a second
  safety net (disk_triples.rs is outside this package).
- docs/REFACTOR_BACKLOG.md — conc#8 follow-up: `DiskTriples`' own test code
  still unwraps the shared pager's lock (`disk_triples.rs` tests only; no
  production path left).
- docs/REFACTOR_BACKLOG.md — db#18 remainder: an id *inside* its space
  (below 2,000,000,000) still sizes that space's slot vector (24 B per id)
  when the RocksDB dictionary loads; `try_reserve` turns a request the
  allocator refuses into "corrupt RocksDB dictionary", but under memory
  overcommit a corrupt id near the bound still costs that allocation. No
  ratio bound against the entry count is applied: reclamation can leave
  legitimate gaps of any size, and the `dict` CF records no id-space limit
  to check against (persisting one, a `meta` key, would close it).

## Measured numbers

- **Bench smoke gate** (D9; release `gbuild` + `gstore-bench` of the base
  4f0e075 and of this branch, `GSTORE_BENCH_CACHE` warm, `flock
  /tmp/gstore-bench.lock gstore-bench ci --baseline
  bench/baselines/smoke.json`, two rounds in alternating order, load
  average 6.6–10 on 4 vCPUs with other worktrees' builds running): **PASS,
  456 gates checked, 0 failed, for both binaries in both rounds.** Time
  geomean vs the committed baseline (LUBM-like snapshot / disk, WatDiv-like
  snapshot / disk; bound 1.50):

  | run | snapshot | disk | watdiv snapshot | watdiv disk |
  |-----|---------:|-----:|----------------:|------------:|
  | round 1 base   | 1.18 | 1.18 | 1.27 | 1.30 |
  | round 1 branch | 0.89 | 0.92 | 1.01 | 1.13 |
  | round 2 branch | 1.05 | 1.10 | 1.25 | 1.28 |
  | round 2 base   | 1.13 | 1.24 | 1.28 | 1.35 |

  The snapshot layout's code is untouched, so its spread (0.89–1.18) is
  the machine's noise; the disk layout moves within it. Disk mixed
  workload (LUBM-like, 5,716-triple chunk), base vs branch: chunk insert
  323.5 / 319.6 ms vs 323.5 / 315.5 ms, chunk delete 551.9 / 564.0 ms vs
  564.4 / 528.3 ms, single insert median 20.0 / 18.6 ms vs 20.0 / 19.9 ms
  (fsync-bound), reopen 130.4 / 117.5 ms vs 87.3 / 73.8 ms.
- **WAL checksum** (disk#10): CRC-32 of a full default-cache batch
  (4,097 pages, 16,797,704 bytes), best of 5, `rustc +1.99.1 -O` on the
  functions copied from `pager.rs`: bit loop 68.0 ms, table-driven
  (slicing by 8) 11.3 ms — 6.0x (load average ~9). The log is streamed
  through a 1 MiB buffer instead of a 16 MiB copy of the batch.
- Commit under a shared guard: the unit test
  `readers_run_while_a_commit_writes_its_log` holds a commit inside its log
  write and takes the pager's read lock meanwhile (it failed with the former
  exclusive commit: "a reader of the shared pager was locked out while the
  commit wrote its log").
- Test suites (debug, 4 vCPUs shared with other builds):
  `cargo test --workspace` 2,778 passed / 0 failed with
  `GSTORE_DISK_QUERY_MODE=memory` and with `=stream`; `cargo test -p gstore
  --features rocksdb` 2,638 passed / 0 failed; `tests/dt_reclaim.rs` 12–15 s
  (17 tests), `tests/dt_reclaim_crash.rs` 6.8 s, `tests/dt_bulk_crash.rs`
  9.3 s, `tests/storage_kernel_regressions.rs` 0.5 s.
- Independent review (same machine, other worktrees building): every fix
  reverted at once (cache generation check, quad check, poison-tolerant
  read lock, shared commit, `remove_sorted_from` presence check, checked
  WAL removal, pager `flock`, the bulk install's WAL replay, the
  dictionary's free-list reuse, a crash child that dies at start) made the
  new tests fail — the two cache-race tests, the `ClockCache` generation
  test, the shared-commit test, `an_absent_value_removes_nothing`, five of
  `storage_kernel_regressions.rs`, both WAL-replay tests of
  `dt_bulk_crash.rs` on ext4 and on tmpfs (`TMPDIR=/dev/shm`), the Memory
  and Materialize workloads and the Materialize reuse test of
  `dt_reclaim.rs`, both crash tests of `dt_reclaim_crash.rs` ("the child
  acknowledged nothing"); restored, all pass. Then: `cargo test
  --workspace` 2,778 passed / 0 failed with `GSTORE_DISK_QUERY_MODE=memory`
  and with `=stream`; `cargo test -p gstore --features rocksdb` 2,638
  passed / 0 failed; workspace and rocksdb clippy `-D warnings`, `cargo
  doc -p gstore` with `RUSTDOCFLAGS=-D warnings` and `cargo fmt --check`
  clean; `scripts/conformance.sh --no-build` (dev binaries): all six
  suites pass.

## Requests/Not yet

- Edits outside the package's owned files (smallest change each):
  - `src/config.rs` (r3-parser is this round's config.rs editor):
    `GStoreConfig::bulk_options()` gained `..crate::bulk::BulkOptions::default()`,
    `GStoreConfig::open_options()` gained `..OpenOptions::default()`, and
    the `OpenOptions { .. }` literal in the test
    `every_key_is_read_from_its_section` gained the same rest expression —
    needed because `BulkOptions` and `OpenOptions` each got a field the
    plan asked for. Expect trivial textual conflicts at most.
  - `src/db/persist.rs` `Database::load_disk_with`: one line,
    `DiskStore::open_with(&kv, …, options.free_ids_if_new)` instead of
    `DiskStore::open` (outside the granted `wal_of` / `dictionary_from_terms`).
  - `src/db/mod.rs` `Database::use_streaming_vstree`: one line,
    `remove_file_and_wal(path)?` (the helper now returns `Result`).
  - `src/bulk/pipeline.rs`: the `Ctx` struct gained `free_ids` and the
    `header_page` call passes it; `src/bulk/commit.rs` `build_disk`'s
    `Ctx` literal passes `bulk.free_ids` (the "lines that pass the explicit
    BulkOptions choice" of the plan).
- Partial ids: design#6 — only the explicit free-id format choice was
  done; the layering moves are in the backlog note above (D3). disk#9 —
  the WAL-to-file identity binding is in the backlog note above (D7); the
  lock, the path helper and the checked removal are done.
- Not failing before (guards, D6 noted): `existing_files_keep_their_format_under_either_policy`
  passes on the base too — the review found no path that flips an existing
  file's format, and none was found here; it pins that. The explicit-format
  test needs the new options, so it cannot run on the base.
- For the directory-lock owner (R4 r4-cli-config): `DirLock` has the fork
  race the pager lock avoids — the last holder drops the locked descriptor
  without `unlock()`, so a child process spawned by another thread at that
  moment keeps the lock until its `exec` and an immediate re-acquire fails
  spuriously; unlocking explicitly before dropping `DirHeld.file` closes it.
- Test-only `.wal` derivations remain in modules outside this package
  (`bptree.rs`, `isarray.rs`, `ivarray.rs`, `overflow.rs`,
  `disk_triples.rs`, `pfn/graph.rs`, `query/engine.rs`,
  `signature/vstree.rs`, `signature/disk_vstree.rs`, `bulk/kvimage.rs`
  tests): cleanup helpers could call `Pager::wal_path` (r5-test-hygiene).
