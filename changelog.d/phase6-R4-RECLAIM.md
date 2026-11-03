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

All numbers: **implementer run, noisy** — this machine (4 vCPUs, 16 GiB),
shared with other agents (load average 1–8 during the runs), every heavy
command under `flock /tmp/gstore-bench.lock`. Release build of `31b4b52`
(`cargo build --release -p gstore --bins --example reclaim_churn -p
gstore-bench`); base = `/tmp/gstore-base-d702463/bin`. The quiet measure
stage should re-run the commands at the end of this section.

### G5 churn: `examples/reclaim_churn.rs`, univ 8 live, 10 rounds

Data: the first 18 universities of
`/tmp/gstore-bench-cache/lubm-like1-u38-s0.nt` (split by the subject's
`University<n>`), 8 live at a time; round r deletes university r − 1 and
inserts university 7 + r. One process per layout and policy (so RSS is the
policy's own). `live` = entity + literal + predicate counts, `referenced` =
the distinct terms the triples use, `limit` = the sum of the three id
spaces' high-water marks.

| layout | policy | live = referenced, all 11 rounds | live round 0 → max → round 10 | limit / peak live | Off control: live round 10 |
|---|---|---|---|---:|---|
| memory | on | **yes** | 255,061 → 268,332 → 254,440 | **1.000** | — |
| memory | off | no | 255,061 → 578,244 (2.27×) | 1.000 | referenced 254,440 |
| Stream disk | on | **yes** | same as memory | **1.000** | — |
| Stream disk | off | no | 255,061 → 578,244 (2.27×) | 1.000 | |
| RocksDB | on | **yes** | same as memory | **1.000** | — |
| RocksDB | off | no | 255,061 → 578,244 (2.27×) | 1.000 | |

The live count's largest deviation from round 0 is **+5.2 %** (round 4) —
exactly the data's own: the triple count is 1,028,283 in round 0 and
1,082,102 in round 4 (+5.2 %), and live / triples is 0.2480 in both (the
universities differ in size). The Off control grows to 2.27× (plan: about
2.2×), its high-water mark with it.

Times and sizes (sum over the 10 rounds; `delete` = `remove_sources` of one
university + `sync`, 1,315,190 triples deleted in all; `insert` likewise):

| layout | delete on | delete off | **on / off** | insert on | insert off | on / off |
|---|---:|---:|---:|---:|---:|---:|
| memory | 45.16 s | 72.49 s | **0.62** | 40.66 s | 8.67 s | **4.69** |
| Stream disk | 263.50 s | 334.55 s | **0.79** | 166.15 s | 232.31 s | 0.72 |
| RocksDB (opt-level 1 build†) | 48.25 s | 45.43 s | **1.06** | 44.21 s | 41.78 s | 1.06 |

† `cargo build --profile test --features rocksdb --example reclaim_churn`
in the shared RocksDB target dir (no release RocksDB build fitted the disk
budget); both policies ran the same binary.

| layout | policy | round 0 | round 1 | round 10 (max) | max / round 0 |
|---|---|---:|---:|---:|---:|
| memory, RSS MiB | on | 96.8 | 131.4 | 150.1 | **1.55** (1.14 × round 1) |
| memory, RSS MiB | off | 96.8 | 136.8 | 213.4 | 2.20 (1.56 × round 1) |
| Stream, kvstore.kv MB | on | 107.8 | 115.0 | 158.9 (flat from round 8) | **1.47** |
| Stream, kvstore.kv MB | off | 107.8 | 125.8 | 227.7 (still growing) | 2.11 |
| Stream, RSS MiB | on / off | 54.2 / 51.5 | 107.3 / 109.5 | 117.1 / 165.2 | — |
| RocksDB, RSS MiB | on / off | 130.2 / 123.7 | 259.2 / 255.4 | 498.5 / 527.8 | — |

Reading the misses:

- **RSS ≤ 1.15 × round 0 (memory): not met as written (1.55×).** Round 0
  is the fresh bulk load, whose adjacency lists are allocated exactly; the
  first round of single-triple updates grows them (Vec doubling) by +35 MiB
  under either policy (Off: +40 MiB). From round 1 on, reclamation holds
  RSS to 1.14× (Off: 1.56× and rising with the dictionary).
- **kvstore.kv ≤ 1.2 × round 0 (Stream): not met (1.47×).** Round 0 is a
  bulk-built image with full pages; incremental B+ tree updates leave pages
  partly filled. With reclamation the file stops growing (158.9 MB in
  rounds 8–10, free pages reused); without it it grows by ~10 MB a round
  (2.11× and rising).
- **Memory-layout inserts are 4.7× slower with reuse** (6.7 s vs 0.87 s for
  the first university). Not an acceptance item, but a real cost: a poor
  man's profiler (gdb stack samples) puts the time in `memcpy` under
  `TripleStore::insert` — `sorted_insert` into the per-predicate
  `p2so` / per-object `o2ps` vectors. A new term without reuse gets the
  highest id and appends at the end of those sorted lists; a reused low id
  inserts near the front and moves the whole list (an `rdf:type` list holds
  ~200 K pairs). Deletes already pay the same move in the base (≈ 7 s per
  university either way). The fix belongs in `src/store/mod.rs` (see
  Requests); Stream and RocksDB inserts are unaffected (0.72× / 1.06×).

### Fresh builds: `gstore-bench run --profile reference --scale 8`

`--backends snapshot,disk --no-conc`, three A/B pairs in one session
(base→branch, branch→base, base→branch); means of the three raw runs per
side (the calibration score was 59.9 ms base / 63.3 ms branch: the branch
runs met a slightly busier machine). `gstore-bench compare` of the first
pair: **PASS, 175 gates**.

| backend | build | open first | open warm | Q1–Q14 geomean | Q ≥ 5 ms geomean | worst query |
|---|---:|---:|---:|---:|---:|---|
| snapshot | 1.015× | 1.099× | 0.952× | 1.003× | 0.994× | q10 1.034× |
| disk (Stream) | 1.008× | 0.984× | 1.047× | 1.000× | 0.994× | q3 1.034× |

Build, open and every Q1–Q14 median are within ±5 % except the snapshot
**first** open (+9.9 %: runs 276 / 297 / 245 ms vs 237 / 241 / 265 ms; the
warm open of the same runs is −4.8 %). **Identical answers:** rows and row
hashes (`rows_fnv64`) of all 28 query results equal the base's in all six
runs; the update phase's round trip is `ok` on both sides.

Update phase (means of three runs; this is where reclamation works):

| backend | metric | base | branch |
|---|---|---:|---:|
| snapshot | chunk insert 5,330 triples | 44.8 ms | 50.9 ms |
| snapshot | chunk delete 5,330 triples | 25.9 ms | 33.1 ms |
| snapshot | single insert / delete median | 0.059 / 0.005 ms | 0.059 / 0.005 ms |
| disk | chunk insert / delete | 507 / 793 ms | 547 / 862 ms |
| disk | single insert median | 1.31 ms | 1.25 ms |
| disk | single delete median | 1.02 ms | **1.62 ms** |
| disk | entities / literals after the round trip | 169,456 / 87,120 (from 168,432 / 86,612) | **168,432 / 86,612** |

A single Stream-mode delete that leaves its terms unreferenced now also
frees them (the `has_*` probes, the writer-gate re-check and one pager
operation per freed term): +0.6 ms. Chunk deletes pay ~1.3 µs (snapshot) /
~13 µs (disk) per triple for the reference checks.

### Old-binary compatibility (proved by hand, `compat.sh`)

40-triple data set, `del.nt` (4 triples), `add.nt` (2 triples):

- **V1 written with `id_reclaim=off`**: `GSTORE_ID_RECLAIM=off gbuild
  --disk v1` → magic `GSTOREKV`. New `gsub` / `gadd` / `gsub` with the
  default policy (on): still `GSTOREKV` (a V1 file is never upgraded
  implicitly). The **old `gquery` reads it** (count 36, exit 0), the old
  `gadd` writes it (38), and the new `gquery` reads that (38).
- **V2 (default)**: `gbuild --disk v2` → `GSTOREK2`; new `gsub` frees ids
  (`gmonitor`: entity live 36 free 4, literal live 18 free 2). The **old
  `gquery` and `gadd` refuse it**: "this gStore KV file uses feature bits
  0x1 that this binary does not support (written by a newer gStore); open
  it with that version", exit 1; `kvstore.kv` byte-identical (md5) after
  the old `gadd`.
- **Snapshot with freed ids**: the old `gquery` reads it (36), the old
  `gadd` adds to it (reusing the freed ids its own LIFO way), and the new
  `gcompact snap --check` reports `integrity ok`.

### Tests (this machine, debug/optimized test profile)

`tests/dt_reclaim.rs` 15 tests (16 with `--features rocksdb`) 9–12 s;
`tests/dt_reclaim_crash.rs` 4 tests (5 with RocksDB; 20 SIGKILL iterations
per layout) 2–7 s; `tests/dt_reclaim_concurrency.rs` 2 tests ~3 s. The
RocksDB run of every test target (`--features rocksdb`, one target at a
time): 2,115 passed, 0 failed, 2 ignored (pre-existing).

**Commands for the quiet measure stage (univ 8):**

```sh
cargo build --release -p gstore --bins --example reclaim_churn -p gstore-bench
D=/tmp/gstore-bench-cache/lubm-like1-u38-s0.nt   # or: gstore-bench gen --workload lubm --scale 18
mkdir -p /tmp/r4reclaim-churn
for layout in memory stream; do for policy in on off; do
  flock /tmp/gstore-bench.lock target/release/examples/reclaim_churn --data "$D" \
      --layout $layout --policy $policy --base 8 --rounds 10 --scratch /tmp/r4reclaim-churn
done; done
# RocksDB (release, separate target dir):
CARGO_TARGET_DIR=target-rocks cargo build --release -p gstore --features rocksdb --example reclaim_churn
for policy in on off; do
  flock /tmp/gstore-bench.lock target-rocks/release/examples/reclaim_churn --data "$D" \
      --layout rocksdb --policy $policy --base 8 --rounds 10 --scratch /tmp/r4reclaim-churn
done
rm -rf /tmp/r4reclaim-churn
# Fresh builds, branch vs base (alternate the order, three pairs):
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale 8 \
    --backends snapshot,disk --no-conc --bin-dir <base-bin> --out /tmp/r4reclaim-base.json
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale 8 \
    --backends snapshot,disk --no-conc --bin-dir target/release --out /tmp/r4reclaim-branch.json
target/release/gstore-bench compare /tmp/r4reclaim-base.json /tmp/r4reclaim-branch.json --markdown
```

Read off: the `SUMMARY` lines (`live_eq_referenced`, `max_live_dev`,
`limit_over_peak_live`, `rss_ratio`, `kv_ratio`, `delete_ms_total`; the
delete ratio is `delete_ms_total` on / off of the same layout) and the
compare's build / open / per-query ratios and row hashes.

## Requests/Not yet

Ownership exception (made, for the integrator to confirm):

- `src/kvstore/pager.rs` (R1-KV's file, no Round-4 owner), test
  `enabling_a_feature_writes_v2_only_with_a_commit`: its three "this binary
  refuses the feature" assertions used `FEATURE_FREE_IDS`, which R1-KV asked
  this package to make supported. They now use `FEATURE_LONG_KEYS` (still
  unsupported); the test checks the same rules (5-line diff, no code
  change).

Requests:

1. **gserver / http_api (`refreshconf`) owner:** call
   `cfg.install_storage_defaults()` next to
   `set_default_open_options(cfg.open_options())` (gserver's
   `install_open_options`, http_api's storage refresh). Until then
   `[system] id_reclaim` and `[storage] rocksdb_block_cache_mb` /
   `rocksdb_bloom` are parsed, checked and shown but take effect only
   through `GSTORE_ID_RECLAIM` / `GSTORE_ROCKS_BLOCK_CACHE_MB` (or a tool
   that calls the method). `QuerySettings` could not carry the policy: a
   test in `http_api.rs` builds it as an exhaustive struct literal.
2. **R4-SNAP (`concurrent.rs`, `http_api.rs`):** a clone of a Stream-mode
   (out-of-core) dictionary pins it, so a Stream database frees no id while
   a `ConcurrentDb` snapshot or an HTTP `/begin` snapshot holding such a
   clone lives (the frees are deferred and retried by every later
   statement; nothing is lost but the reuse). A snapshot that reads only
   inside a `DiskReadView` (drained by the writer gate before any free)
   should take `db.dict().clone_unpinned()` instead. A `ConcurrentDb` over a
   Stream database always holds a snapshot, so today it never frees ids.
3. **R4-VS (`src/signature/disk_vstree.rs`):** assert `pager.features() ==
   0` when opening `vstree.kv` (R1-KV's request): `FEATURE_FREE_IDS` is now
   a supported bit, so the pager alone no longer refuses a `vstree.kv`
   carrying it (nothing writes one).
4. **Dockerfile owner:** add `gcompact` to the binary list
   (`for b in gserver … gmonitor gnode`); the deb/rpm asset lists in
   `Cargo.toml` have it.
5. **R4-CLOSURE (docs):** the Docs notes above; `docs/CPP_PARITY.md` line 47
   (upstream frees ids at degree 0 — now we do too, lowest id first).
6. **`src/store/mod.rs` owner (R4-SNAP's copy-on-write `TripleStore`):**
   a batched insert that merges a sorted run into each touched adjacency
   list (`insert_many`, O(list + batch) per list instead of one
   `Vec::insert` per triple), used by `insert_sources` / `INSERT DATA`
   batches. With lowest-first reuse a new term gets a low id, and
   `sorted_insert` into the large per-predicate `p2so` (and per-object
   `o2ps`) lists then moves most of the list per triple: memory-layout
   inserts after deletes are 4.7× slower in the churn (Measured numbers).
   The same batching would speed up deletes (`sorted_remove`), which pay
   this cost in the base already.

Not yet / notes:

- **Acceptance misses (see Measured numbers):** memory RSS 1.55× round 0
  (1.14× of round 1; the bound assumed a steady-state round 0) and Stream
  `kvstore.kv` 1.47× round 0 (bulk-built pages vs B+ tree splits; flat
  from round 8); live within ±5 % of round 0 is +5.2 % — the data's own
  size change; the snapshot layout's first open +9.9 % (warm −4.8 %) in
  noisy runs.
- Memory-layout inserts that reuse ids are slower (request 6); a single
  Stream-mode delete that frees terms costs +0.6 ms.

- No automatic VS-tree rebuild threshold (R1-CORE's note on
  `vs_shrink_debt`): freed entities stay in the tree as phantoms (sound
  superset) until `rebuild_index()` / a save that rebuilds it.
- A Materialize-mode database's frees reach `kvstore.kv` by key; the store's
  own id for a term may differ from the working set's after reuse (as
  before, writes are by key, and a reopen re-reads the store's ids).
- Stream frees deferred by a pin are kept in memory only; a restart drops
  them (the terms stay as orphans until `gcompact --reclaim`).
- RocksDB: a failed `delete_terms` keeps the terms in memory and in the
  `dict` CF (orphans, reported at the next commit point).
- An incremental backup (`incr.log`) holding the new `C` / `N` redo records
  cannot be restored by an older binary ("unknown redo.log op").
