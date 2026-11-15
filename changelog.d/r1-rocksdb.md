# r1-rocksdb — RocksDB: crash-atomic auto-commit statements, commit point without memtable flush, bounded fds, robust key decoding

## Changelog

- **RocksDB auto-commit statements are crash-atomic** (review H-11,
  gap-rocksdb-exec#1). Every auto-commit statement on a RocksDB database is
  now journaled in the store's existing undo journal, as transactions
  always were: a SPARQL UPDATE request (`INSERT DATA`, `DELETE`/`INSERT …
  WHERE`, `LOAD`, `CLEAR`, `DROP`, `ADD`/`COPY`/`MOVE`) and an
  `insert_sources` / `remove_sources` import (`gadd`, `gsub`, `/api
  batchInsert` / `batchRemove`). A crash part-way (SIGKILL, OOM kill, power
  loss) is undone as a whole at the next open, so the database reopens at
  a statement boundary — never half of a DELETE/INSERT rename, half a LOAD
  or half a CLEAR — like the disk layout, whose commit point writes a
  statement as one. A statement that fails part-way keeps what it applied
  (the documented semantics): its journal is committed too. The ids a
  journaled statement frees are freed at its end, in the batch that deletes
  the journal (until then they are neither deleted nor reused). A failed
  `LOAD` stays "no change at all" (mutation count and CSR put back).
  Direct-API writes (`insert_triple`, `materialize_rdfs`, …) and REST
  `/batchInsert` / `/batchRemove` (one `try_insert_triple` per triple in
  `http_api.rs`) are still atomic per triple only.
- **No orphan dictionary terms after a crash on RocksDB**
  (gap-rocksdb-exec#4). Terms written new inside a transaction or a
  journaled statement are recorded in the journal in the same batch, so a
  crash between a term and its triple lets the next open free the term. A
  rollback replays the journal but keeps it until the terms it leaves
  unreferenced are deleted in the same batch as the journal; the open's
  replay of a journal a crash left does the same. The journal record format
  stays readable by older binaries (a term record is a `-` record that an
  older binary replays as a no-op removal plus the same reclamation).
- **`check_point` no longer flushes RocksDB's memtables on every write**
  (review H-12, gap-rocksdb-exec#2). A save at a RocksDB database's home —
  the servers' `check_point` save after every acknowledged write — is now a
  commit point only (an fsync of RocksDB's WAL, which already made every
  write durable). Each save used to flush all seven column families,
  leaving about three table files (and three open descriptors) per write,
  making writes several times slower (5–6x in the review's measurement,
  about 13x here) and ending in `Too many open files` (EMFILE) after a few
  hundred writes under `ulimit -n 1024`. Memtables are now
  flushed by an explicit `Database::checkpoint` (`gcompact`), a backup and
  when the database is closed.
- RocksDB now opens with a bounded number of open files (new key
  `[storage] rocksdb_max_open_files`, default 4096, `0` = unlimited —
  RocksDB's own default), keeps four old info logs (`LOG.old.*`; each open
  added one and backups copied them all) and bounds the write-ahead logs'
  total to 256 MiB (the column families still holding the oldest log are
  then flushed). Options only: the files are unchanged and open with any
  value. `RocksTuning` has a new field `max_open_files`.
- **A damaged RocksDB store is reported, never a panic** (disk#11,
  gap-rocksdb-exec#6). Every decoded key's length is checked; a key of the
  wrong length (a partial copy, a foreign tool, bit rot) fails the
  statement with a read error naming the column family, instead of a
  slice-index panic in a query, `gcompact --check`, `gexport` or an open
  with `vstree_on_open`.
- **`gcompact --check` checks RocksDB's statistics** (gap-rocksdb-exec#7).
  `Database::verify_integrity` now runs the new
  `RocksStore::verify_stats`: every counter of the `stats` column family
  equals a recount from the SPO keys, POS and OSP hold exactly the SPO
  triples, the global, cached and mirrored counters agree — streamed
  (constant memory, each ordering and counter prefix read once). A wrong,
  missing or stray counter used to pass `--check` (and `--reclaim` then
  freed terms the data still used).
- New test-only hook `RocksStore::debug_set_write_hook` (doc-hidden): crash
  tests stop a child at an exact write batch and SIGKILL it there.
- Tests: `tests/rocksdb_statement_atomicity.rs` (SIGKILL battery at exact
  write batches over a DELETE/INSERT rename, `INSERT DATA`, `LOAD`, `CLEAR
  GRAPH`, `insert_sources`, `remove_sources` and a rollback; every reopened
  state is a statement boundary with live term counts equal to the
  referenced terms), `tests/rocksdb_checkpoint_resources.rs` (120
  checkpointed single-triple `/update`s through `gserver`'s single-database
  server add no table file per write; options file records the bounds; the
  config key), `tests/rocksdb_damaged_store.rs` (malformed keys in SPO,
  POS, OSP and the statistics; wrong, missing and stray counters through
  `verify_integrity` and `gcompact --check`; the streamed check agrees with
  `check_stats`). `tests/dt_reclaim_crash.rs` now kills multi-operation
  auto-commit requests and rolled-back transactions too and allows no
  orphan term on RocksDB (gap-rocksdb-exec#8); `tests/dt_rocksdb.rs`'s temp
  paths carry the pid (gap-rocksdb-exec#8); the RocksDB crash test of
  `tests/dt_phase0_review.rs` checks that the child applied its writes and
  died of SIGABRT (tests-b#7); `tests/dt_load_import.rs` runs a failed
  64 Ki-batch `LOAD` on the disk layout (both modes) and RocksDB, with
  reopens (tests-a#1).

## GAP_PLAN status line

none

## Docs notes

- README.md (configuration table, `[storage]`): add `rocksdb_max_open_files`
  | `4096` | the most files a RocksDB database keeps open; `0` = unlimited
  (RocksDB's default). In the storage-layouts section: RocksDB auto-commit
  statements (UPDATE requests, `gadd`/`gsub`, `/api batchInsert`/
  `batchRemove`) are crash-atomic; a `check_point` save is a WAL fsync.
- docs/DEPLOY.md (storage keys paragraph, after `rocksdb_bloom`):
  `rocksdb_max_open_files` (4096). Next to `LimitNOFILE`: the bound is per
  loaded RocksDB database, so size `LimitNOFILE` above
  (RocksDB databases loaded at once × `rocksdb_max_open_files`) plus the
  connection limit. Not a D1 compat switch (a tuning key; no
  `upstream-compat` warning).
- docs/STORAGE_BACKEND.md §RocksDB: (1) the undo journal covers every
  auto-commit statement as well as transactions; frees wait for the
  statement's end and leave the `dict` CF in the journal-deleting batch;
  (2) the journal's term records (kind byte 2, `-` op, `(id, id, id)`),
  readable by every earlier binary — format registry: no new feature bit,
  `LAYOUT_VERSION` stays `"1"`, the column families and keys are unchanged;
  (3) commit point = WAL fsync, memtables flushed by `checkpoint`, backup,
  close; bounded `max_open_files` / `keep_log_file_num` /
  `max_total_wal_size`; (4) line 265 ("RocksDB: 自动提交时在三元组批之后删除
  `dict` CF 条目…"): auto-commit frees now go into the journal-deleting
  batch too, and a rollback's/open's replay deletes its unreferenced terms
  in that batch (no crash window leaves orphans); (5) `gcompact --check`
  runs `RocksStore::verify_stats`; (6) cost: a journaled statement writes
  one journal record per triple or quad it changes (plus one per term it
  writes new) into the `meta` CF, in the same batches, and deletes them at
  its end (point deletes up to 1,024 records, else one range delete), so a
  very large `gadd` / `gsub` / `LOAD` on RocksDB temporarily stores a
  journal as large as its change set; its freed ids are held until its
  end (`remove_sources` keeps only the dead ones between batches).
- docs/REFACTOR_BACKLOG.md: (a) REST `/batchInsert` / `/batchRemove`
  (`http_api.rs` handle_batch) apply one `try_insert_triple` per triple,
  each its own RocksDB write: not crash-atomic as a whole on RocksDB until
  the handler runs as one journaled statement (gap-rocksdb-exec#1
  follow-up; `http_api.rs` is r3-rest-parity's in R3). (b) Direct-API
  multi-triple calls (`insert_data`, `materialize_rdfs`, …) stay atomic
  per triple, and can leave an orphan term if a crash falls between a new
  term and its triple (gap-rocksdb-exec#4 residue; `gcompact --reclaim`
  frees it). (c) REST `/checkpoint` and `/api checkpoint` call
  `DbSlot::save` (a commit point); `Database::checkpoint` also flushes the
  memtables — route explicit checkpoints there if a flush is wanted
  (gap-rocksdb-exec#2 follow-up). (d) `gcompact --reclaim --check` runs the
  reclaim before the check; a corrupted counter is only reported after
  `--reclaim` trusted it (gap-rocksdb-exec#7 follow-up for
  `src/bin/gcompact.rs`).

## Measured numbers

Machine: the campaign's shared 4-vCPU / 15 GB box, up to three other
agents building at the same time; test profile (`opt-level = 1`) with
the `rocksdb` feature (no release build of librocksdb was available), so
absolute times are high and noisy; base and branch were measured in the
same session, base = `d4acadb` (7bbf4ae plus the doc-hidden write hook,
no behaviour change).

- **check_point writes (H-12).** LUBM fixture (100,543 triples) built with
  `gbuild --rocksdb`; the single-database `Server` in-process, 200
  single-triple `INSERT DATA` `/update` requests (scratch test, not
  committed). Session 1 (lighter load, two runs each) and session 2 (a
  private build directory, the box busy with other builds):

  | | ms/update, check_point on | table files after 200 writes | memtable flushes (LOG) | ms/update, check_point off |
  |---|---|---|---|---|
  | base, session 1 | 9.01 / 9.83 | 11 -> 633 / 11 -> 622 | 1000 / 1000 | 0.44 / 0.61 |
  | branch, session 1 | 0.73 / 0.60 | 11 -> 11 / 11 -> 11 | 0 / 0 | 0.48 / 0.53 |
  | base, session 2 | 72.27 | 11 -> 612 | 1000 | 2.38 |
  | branch, session 2 | 5.60 | 11 -> 11 | 0 | 1.28 |

  Base flushed the five non-empty column families on every write (about
  three new table files per write); the branch's checkpointed write costs
  a WAL fsync plus the journal's deletion batch and adds no table file.
  `tests/rocksdb_checkpoint_resources.rs` on base: 120 writes grew the
  store from 6 to 368 table files.
- **Failing before the fix** (run on `d4acadb` with the new tests):
  `rocksdb_statement_atomicity`: 7 of 7 scenarios failed (e.g. "rename
  killed after write 1 of 242: the reopened store is no statement
  boundary (299 rows; before 300, after 300)"; "rollback killed after
  write 61 of 62 (state before): orphan terms, 434 live vs 311
  referenced"; `insert_sources` stopped between a term and its triple:
  orphan terms); `rocksdb_checkpoint_resources`: 3 of 3 failed
  ("120 checkpointed writes grew the store from 6 to 368 table files";
  "the store's options lack max_open_files=4096"); `rocksdb_damaged_store`:
  4 of 4 failed (panics at `rocks.rs` "range end index 4 out of range for
  slice of length 3" in a full scan and "… 8 … length 6" in an open with
  `vstree_on_open`; "missing counter: verify_integrity passed"). The
  agreement test and the config-key test use new API and were not run on
  base.
- **tests-a#1 cost.** The first version of the persistent failed-LOAD
  test (65,536 + 1,000 statements, each minting two terms) took 93 s for
  the three layouts; the committed one (50 minting statements + 257 x 257
  statements over existing terms, the same full first batch) takes 32.6 s.
- **Large statements on RocksDB** (the journal's cost), session 2, same
  database: `LOAD` of 20,000 fresh triples and `DELETE WHERE` of them, three
  rounds each — base 24.0 / 37.0 / 32.7 µs per loaded triple and 26.8 /
  36.1 / 32.3 µs per deleted one; branch 36.6 / 28.1 / 30.1 and 30.1 /
  27.8 / 31.6 µs. The extra journal put per write (and two term records
  per fresh triple) is within the run-to-run noise here.
- **Bench smoke gate** (`gstore-bench ci`, default features — no RocksDB
  in the smoke profile; the non-RocksDB code paths changed only by a
  `bool` per UPDATE request and a check in `flush_reclaim`):
  `target/release/gstore-bench ci --baseline bench/baselines/smoke.json
  --data-dir <cache>` — branch `93d640c`: exit 0, 148 rows pass, 0 fail
  (calibration 51.78 ms, load 8.3); base `7bbf4ae` built from `git
  archive` in the same session: exit 0, 148 pass, 0 fail (calibration
  70.75 ms, load 5.7). Query geomean ms branch / base: LUBM-like snapshot
  0.47 / 0.54, disk 0.46 / 0.46; WatDiv-like snapshot 0.13 / 0.14, disk
  0.13 / 0.13. Disk single-insert medians (1.17 vs 8.07 ms, LUBM-like)
  differ by the box's fsync load between the runs, not by the change
  (the disk write path is untouched).

## Requests/Not yet

- Edits outside the owned files (all inside the sanctioned regions unless
  noted): `src/db/mod.rs` `run_update`, `failed_update`, `insert_sources`,
  `remove_sources`, the RocksDB journal / term block of `rollback()`, and
  two new helpers next to them (`end_update_statement`,
  `end_sources_statement`); `src/db/persist.rs` `save_locked`, the
  `save` doc, `open_rocksdb_with`, `rocks_rollback_journal`,
  `intern_persisting_terms`, the `rocks_commit_journal_with` doc, two new
  fns next to the `rocks_*` ones (`rocks_begin_statement`,
  `rocks_end_statement`) and — **outside the listed region** — four lines
  in `Database::checkpoint()` (flush RocksDB's memtables on an explicit
  checkpoint); `src/db/reclaim.rs` (owned for the verify_integrity
  wiring) also `flush_reclaim` (one condition: defer while a RocksDB
  journal records) and a new `keep_dead_candidates`; `src/config.rs` the
  `rocksdb_max_open_files` key at its anchor (KNOWN_KEYS, struct, Default,
  set, value, rocks_tuning, the install_storage_defaults doc) and the test
  `every_key_is_read_from_its_section` (one key in its text, one
  assertion: it requires every key); `conf/conf.ini.example` `[storage]`.
- For r2-txn (edits `rollback()` next round): the block after
  `self.touched()` is the RocksDB journal/term ordering (replayed journal
  deleted with the dead terms in one batch); keep it. A rolled-back
  transaction's journal is `Stale { replay: false }` between the replay
  and that batch. gap-rocksdb-exec#5 (failed journal deletion at commit)
  is unchanged and now also applies to an auto-commit statement's end
  (the statement then reports the persistence error, the journal stays
  stale and writes are refused until it is deleted).
- For r3-rest-parity (`http_api.rs`): REST `/batchInsert` /
  `/batchRemove` (`handle_batch`) still apply one `try_insert_triple` per
  triple, so on RocksDB a crash keeps a prefix of the batch; run the loop
  as one statement (or through `insert_sources`) to make it crash-atomic
  like `/api batchInsert`. REST `/checkpoint` and `/api checkpoint` call
  `DbSlot::save` (now a commit point only for RocksDB); call
  `Database::checkpoint` there if an explicit checkpoint should also
  flush memtables. `DbSlot::save`'s doc ("a cheap commit point") is now
  accurate for RocksDB too; no edit needed.
- For r4-cli-config (`src/bin/gcompact.rs`): `--reclaim --check` runs the
  reclaim first, so a corrupted counter is trusted by the reclaim before
  the check reports it; run `--check` first (or refuse `--reclaim` when
  `verify_stats` fails).
- `src/bin/gserver.rs` `install_open_options`' doc lists the RocksDB keys
  without `rocksdb_max_open_files` (comment only; gserver.rs belongs to
  r1-http-surface / r1-download this round).
- Not done (D3/D8, see Docs notes → REFACTOR_BACKLOG): direct-API
  multi-triple writes are not journaled as statements; no streaming
  replacement for `check_stats` (kept as the in-memory reference check,
  tests use it).
