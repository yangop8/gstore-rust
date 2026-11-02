# R4-RECLAIM — immediate id reclamation on delete (all layouts) + CLEAR redo coverage

## Changelog

### Phase 6 R4-RECLAIM: the ids of deleted terms are reused

- **Immediate id reclamation (behaviour change).** After every applied delete
  — `DELETE DATA` / `DELETE … WHERE`, quad deletes, `CLEAR` / `DROP` /
  `MOVE` / `COPY`, `remove_sources` (`gsub`, `/api batchRemove`), the undo of
  a failed `LOAD`, the rollback of a transaction's inserts — the subject,
  predicate, object and graph IRI of each removed triple are checked, and
  every one that no triple or quad references any more is freed: its key
  and id leave the dictionary and the id is reused by the next new term,
  **lowest free id first**. `entity_num` / `literal_num` / `predicate_num`
  now count exactly the terms the data references. A transaction frees at
  `commit` (one batch) and frees the terms it minted at `rollback`. The
  default policy is `on` (`IdReclaim::Immediate`); `[system] id_reclaim=off`
  or `GSTORE_ID_RECLAIM=off` keeps the former behaviour (ids only freed by an
  explicit `gcompact --reclaim` / `Database::reclaim_unused`).
- **Every layout persists its frees:** snapshot databases at the next save
  (`dict.bin` keeps its byte layout: the free list is the same `seq` of
  `u32`, now ascending); Materialize-mode disk databases write a key-level
  free through to `kvstore.kv`; Stream-mode disk databases free by id inside
  the store under its writer gate (`DiskTriples::free_ids`, which re-checks
  the triple indexes first and refuses — breaking the store — rather than
  corrupt it); RocksDB deletes the freed `dict` CF entries after the triple
  batch (auto-commit) or in the commit's journal batch (transactions), and
  frees the terms a journal replay at open left unreferenced.
- **kvstore.kv format (new databases only).** A new on-disk database — created
  by `DiskStore::open` on a new file or by the bulk build (`gbuild --disk`) —
  keeps a free-id list: B+ tree root in header slot 22 (`FREE_IDS`, keys
  `space tag ‖ be32(index)`), counts in slots 23–25, feature bit 0
  (`FEATURE_FREE_IDS`, now in `SUPPORTED_FEATURES`) and magic `GSTOREK2`.
  Binaries older than Phase 6 refuse such a file ("this gStore KV file uses
  feature bits 0x1 that this binary does not support"). **Existing version-1
  files are never upgraded implicitly**: reclamation stays off on them until
  `gcompact --enable-reclaim`. With `id_reclaim=off` new files are created
  version 1, readable by older binaries. Freeing the highest id lowers the
  space's high-water mark (and with it every free id that becomes the
  highest: the trim cascade), so the count roots can now go down.
- **Pins.** A removed term's id is *retired* first and released for reuse
  once no `DictPin` older than the removal is alive. A clone of an
  out-of-core (Stream-mode) dictionary — a snapshot sharing the on-disk
  dictionary — pins it for its lifetime: a Stream-mode database defers its
  frees (retried by the next statement) while such a clone exists, so an id a
  snapshot read never resolves to a different term. In-memory clones are
  copy-on-write and need no pin. `Dictionary::pin()` takes one explicitly;
  `clone_unpinned()` is for readers inside a `DiskReadView`.
- **CLEAR / DROP / MOVE / COPY write redo-log records** (`REDO C …` for one
  graph, `REDO N` for all named graphs); `replay_redo_log` and
  `restore_incremental` reproduce them (they were silently missing).
- **VS-tree:** freed entities stay in the tree (a sound superset); an entity
  receiving its first edge — a new term, or a reused id — gets its exact
  signature instead of a union onto the stale entry.
- **Dictionary API:** `TermId`, `retire` / `release` / `free_id`, `pin` /
  `DictPin` / `is_pinned` / `clone_unpinned`, `from_slots` (exact ids with
  holes; RocksDB's and `DiskStore::dictionary`'s loader), `id_limits`,
  `free_counts` / `retired_counts`, `track_allocations` /
  `take_allocations`, `try_intern_new`, `unreferenced`. Loading normalizes a
  dictionary (every hole free, trailing holes trimmed; a free list naming an
  index twice or a term stored under two ids is refused). The in-memory
  `resolve` / `term` / `pred` path is inlined (R2-DICT's measured +2.5% on
  large results).
- **`Database` API:** `set_id_reclaim` / `id_reclaim` / `reclaims_ids`,
  `reclaim_unused() -> Result<usize>` (was `usize`; refused inside a
  transaction and on a version-1 `kvstore.kv`; advances `mutation_count`
  when it frees), `verify_integrity`, `id_space_stats` (`IdSpaceStats`),
  `enable_id_reclaim`; `db::{IdReclaim, set_default_id_reclaim,
  default_id_reclaim, ENV_ID_RECLAIM}`. Named graphs are kept in
  `db::named::NamedGraphs`, which counts every term's uses (graph IRIs
  included) and serializes exactly as the former map (`named.bin`
  unchanged); `named_graphs()` still returns `&BTreeMap<u32, TripleStore>`.
- **`DiskStore`:** `open_with(path, cache, free_ids_if_new)`, `frees_ids`,
  `enable_reclaim`, `free_key`, `free_counts`, `term_limits`,
  `verify_free_list`; `entity_num` / `literal_num` / `predicate_num` count
  live terms (allocated minus free); the header's term counts are
  authoritative (a shared dictionary may lower them). `DiskDict::free`
  (moves the generation and the caches' epoch), `header_state`,
  `free_count`; `slot()` bounds ids by the allocated limit.
  `DiskTriples::free_ids` / `frees_ids` / `enable_reclaim`.
- **RocksDB:** `RocksStore::delete_terms`, `commit_journal_with`,
  `take_replayed`; `set_default_rocks_tuning` — `RocksTuning::default()`
  starts from the installed tuning, `GSTORE_ROCKS_BLOCK_CACHE_MB` still
  overrides the block cache.
- **Configuration:** `[system] id_reclaim=on|off`, `[storage]
  rocksdb_block_cache_mb` (default 256) and `rocksdb_bloom` (bits per key,
  default 10, `on`/`off`); `GStoreConfig::install_storage_defaults()`
  installs them process-wide (see Requests: the servers do not call it yet).
- **Tools:** new `gcompact <db> [--reclaim] [--enable-reclaim] [--check]`;
  `gmonitor` shows `id_reclaim` and `live / free / id_limit` per id space.
- **Example:** `examples/reclaim_churn.rs` (gate G5): ten rounds of "delete
  university r, insert university 8 + r" with reclamation on and off.
- **Tests:** `tests/dt_reclaim.rs` (seeded ≥ 10K-operation workloads on
  memory, Stream, Materialize and RocksDB against an `Off` oracle — contents,
  live counts = referenced terms, `verify_integrity`, LUBM Q1–Q14, reopens —
  plus targeted cases: only unreferenced terms are freed, named-graph and
  graph-IRI references pin, transaction deferral, rollback frees minted
  terms, `Off`, pins, CLEAR/DROP/MOVE/COPY redo replay, Stream frees wait for
  a dictionary clone, version-1 stores, `gcompact`/`gmonitor`);
  `tests/dt_reclaim_crash.rs` (SIGKILLed child, 20 iterations per layout);
  `tests/dt_reclaim_concurrency.rs` (`ConcurrentDb` readers and 8 HTTP
  readers keep a pair invariant while ids are reused).

## GAP_PLAN status line

Phase 6 R4-RECLAIM (immediate id reclamation, CLEAR redo coverage): done —
lowest-first id reuse on every layout (snapshot, Stream/Materialize disk,
RocksDB), `FREE_IDS` free list for new `kvstore.kv` files (V1 files upgrade
only through `gcompact --enable-reclaim`), CLEAR/DROP/MOVE/COPY redo records,
`gcompact`.

## Docs notes

- **docs/STORAGE_BACKEND.md** (id reclamation section): the policy
  (`IdReclaim`, `[system] id_reclaim`, `GSTORE_ID_RECLAIM`), when frees run
  (end of each deleting statement; at `commit` in a transaction; minted
  terms at `rollback`), "referenced" (default graph via `has_*`, which are
  `true` on a read error, plus `NamedGraphs` counts — authoritative for
  quads because `GSPO` is keyed graph-first), the per-layout persistence
  (snapshot at save; Materialize `DiskOp::Free`; Stream
  `DiskTriples::free_ids` under the writer gate with its defensive re-check;
  RocksDB `delete_terms` after the triple batch / in the journal batch,
  `take_replayed` at open), lowest-first reuse and the trim cascade, pins
  (`DictPin`; a clone of an out-of-core dictionary pins it; Stream frees
  are deferred while pinned), and that the VS-tree keeps freed entities (a
  superset) while a first edge re-signs exactly.
- **docs/STORAGE_BACKEND.md** (format registry / compatibility matrix,
  plan §3.4): `FEATURE_FREE_IDS` is implemented; slot 22 holds the
  `FREE_IDS` B+ tree (5-byte keys `E|L|P ‖ be32(index)`, empty values),
  slots 23–25 the free counts; the count roots 10–12 are the high-water
  marks (they can go down). New stores and bulk images are V2 unless
  `id_reclaim=off`; V1 files stay V1 (reclamation off) until `gcompact
  --enable-reclaim`; downgrade = `gexport` with the new binary, `gbuild`
  with the old one. `vstree.kv` never uses a feature.
- **README / docs/DEPLOY.md / tools list:** the `gcompact` tool
  (`--reclaim`, `--enable-reclaim`, `--check`); gmonitor's new rows
  (`id_reclaim`, `entity_ids` / `literal_ids` / `predicate_ids`: live, free,
  id_limit). The Dockerfile's binary list does not have `gcompact` yet (see
  Requests).
- **docs/CONFIG (conf.ini section docs):** `[system] id_reclaim`,
  `[storage] rocksdb_block_cache_mb`, `[storage] rocksdb_bloom`; the
  environment overrides (`GSTORE_ID_RECLAIM`, `GSTORE_ROCKS_BLOCK_CACHE_MB`).
- **docs/CPP_PARITY.md:** upstream frees an id when its degree drops to 0
  (`freeEntityID` …); gstore-rust now does the same on every layout, reusing
  the lowest free id (upstream's free list is LIFO), and also frees
  predicates and literals.
- **CHANGELOG (behaviour changes):** ids are reused after deletes (an
  application that cached ids across a delete must not); entity/literal/
  predicate counts drop with deletes; `Database::reclaim_unused` returns
  `Result`; new `kvstore.kv` files are V2; `DiskStore::entity_num` counts
  live terms; CLEAR/DROP/MOVE/COPY appear in `redo.log` (an incremental
  backup holding them cannot be restored by a binary older than this one:
  it fails with "unknown redo.log op 'C'").

## Measured numbers

PLACEHOLDER

## Requests/Not yet

PLACEHOLDER
