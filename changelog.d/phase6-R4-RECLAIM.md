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

### Quiet post-merge stage (phase6 `7538dfa` + this stage's `16a94cf`, `b9e8dc0`; part 1 of 3: R4-RECLAIM, gate G5)

Reference VM (4 vCPU, 16 GB), nothing else running, in two sessions: 11-03
(cut off by a usage limit near its end) and 11-04 (its resumption: the
Materialize pairs on `b9e8dc0`, four more memory pairs and the Materialize
diagnostics below). CPU busy 24–26 % on average while measuring (the one
measured process; at most 36 % while a bulk build's index tasks run in
parallel; 11-04: mean 24.9 %, max 28 %), sampled every 10 s from
`/proc/stat`; 1-minute load average 1.0–1.1 (up to 2.7 in the first minute
after a build; 11-04: 0.7–1.4). **The VM ran about 1.75× faster on 11-04**
(same binaries: memory delete totals 6.8 s against 11.8 s, Materialize
`off` 387–397 s against 796 s), so ratios are only formed between runs of
one session. Toolchain 1.99.1, `CARGO_PROFILE_DEV_DEBUG=0
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`, every heavy command under
`flock /tmp/gstore-bench.lock`. Base = the release binaries of `d702463`
(the tree before this round: `/tmp/gstore-base-d702463/bin`, and `bin-rocks`
for RocksDB), measured in the same session as the branch (11-03, the
fresh builds and the CLI churn). Branch = release builds of
this tree: `target/release` (`-p gstore --bins --example reclaim_churn -p
gstore-bench`) and `target-rocks/release` (`--features
gstore/rocksdb,gstore-bench/rocksdb`, a release build this time — the
implementer's RocksDB churn ran an opt-level 1 build).

This stage changed two things (both committed, see the commits):

- `16a94cf` — `examples/reclaim_churn.rs` gets `--layout materialize`
  (`build_disk`, then `OpenOptions::materialize()`): the plan's G5 churn
  covers Materialize-mode disk too.
- `b9e8dc0` — **a fix of the memory-layout delete overhead** (below):
  `db::reclaim::Candidates` keeps packed `u64` keys (space rank above the
  id, so they sort like `TermId`) and skips a term equal to the one the
  previous removed triple had in the same position. Same candidate set
  (new unit tests compare it with the sorted set of every noted `TermId`
  over random triples, and check the key order); `cargo clippy -p gstore
  --lib --tests --examples -D warnings` clean; `cargo test -p gstore --lib`
  1,215 passed; `dt_reclaim` / `dt_reclaim_crash` /
  `dt_reclaim_concurrency` / `dt_r4_merge` 15 / 4 / 2 / 3 passed, and the
  first three 16 / 5 / 2 with `--features rocksdb` (11-03); on 11-04
  `cargo test -p gstore --lib db::reclaim` 4 passed and the clippy run
  above clean again.

Which build measured what: the memory churn on both (`7538dfa`, and
`b9e8dc0` in both sessions); the Stream, the first Materialize pair and the
RocksDB churn on `7538dfa` (the fix only makes the `on` side's candidate
bookkeeping cheaper, by about 0.2 s over the ten rounds, against 441–878 s
of disk deletes: it can only lower those ratios); two more Materialize
pairs (11-04), the CLI churn, the fresh builds and the compatibility
checks on `b9e8dc0`.

#### G5 churn: `examples/reclaim_churn.rs --base 8 --rounds 10`

Data: the first 18 universities of
`/tmp/gstore-bench-cache/lubm-like1-u38-s0.nt`, split by the subject's
`University<n>`; round r deletes university r − 1 (`remove_sources` +
`sync`) and inserts university 7 + r; one process per layout and policy.
1,315,190 triples are deleted and as many inserted over the ten rounds.

The counts are the same on every layout (memory, Stream, Materialize,
RocksDB) and build:

| policy | live = referenced, all 11 rounds | live: round 0 → max → round 10 | triples: round 0 → round 4 → round 10 | limit / peak live |
|---|---|---|---|---:|
| on (Immediate) | **yes** | 255,061 → 268,332 (round 4) → 254,440 | 1,028,283 → 1,082,102 → 1,025,981 | **1.000** (268,332 / 268,332) |
| off (control) | no | 255,061 → 578,244 (round 10; referenced 254,440: **2.27×**) | same | 1.000 (578,244) |

The largest deviation of the live count from round 0 is **+5.20 %**
(round 4), the data's own: the triple count is +5.23 % in that round and
live / triples is 0.2480 in round 0 and in round 4 (the universities differ
in size). The Off control grows to 2.27× (plan: about 2.2×).

Delete and insert time (sums over the ten rounds, means over the runs; a
pair = one `on` and one `off` process, the order alternated):

| layout (build, session) | pairs | delete on | delete off | **on / off** | insert on | insert off | on / off |
|---|---:|---:|---:|---:|---:|---:|---:|
| memory (`b9e8dc0`, 11-03) | 4 | 11.84 s | 10.46 s | **1.132** (pairs 1.114–1.151) | 12.13 s | 12.24 s | 0.99 |
| memory (`b9e8dc0`, 11-04) | 4 | 6.79 s | 6.09 s | **1.115** (pairs 1.087–1.154) | 7.29 s | 7.02 s | 1.04 |
| memory (the same fix before its commit, 11-03) | 4 | 11.98 s | 10.79 s | 1.110 (pairs 1.044–1.153) | 12.23 s | 12.52 s | 0.98 |
| memory (`7538dfa`, 11-03) | 4 | 12.10 s | 10.22 s | 1.184 | 12.33 s | 11.88 s | 1.04 |
| Stream disk (`7538dfa`, 11-03) | 1 | 441.0 s | 695.9 s | **0.634** | 277.8 s | 476.5 s | 0.58 |
| Materialize disk (`7538dfa`, 11-03) | 1 | 877.7 s | 796.0 s | 1.103 | 561.5 s | 521.7 s | 1.08 |
| Materialize disk (`b9e8dc0`, 11-04) | 2 | 446.2 s | 391.8 s | **1.139** (pairs 1.188, 1.092) | 302.0 s | 286.9 s | 1.05 |
| RocksDB (`7538dfa`, release, 11-03) | 1 | 61.25 s | 62.82 s | **0.975** | 57.98 s | 55.50 s | 1.04 |

Absolute disk times move a lot between sessions (Materialize `off`
796.0 s on 11-03, 386.6 / 396.9 s on 11-04; the implementer's Stream on /
off was 264 / 335 s against 441 / 696 s here), on both policies alike: the
ratios within a session are what G5 gates.

Sizes (round 0 is the fresh bulk build):

| layout | policy | round 0 | round 1 | max (round) | **max / round 0** | max / round 1 |
|---|---|---:|---:|---:|---:|---:|
| memory, RSS MiB | on | 96.8 | 131.0 | 148.3 (10) | **1.53** | 1.13 |
| memory, RSS MiB | off | 96.8 | 136.2 | 214.8 (10) | 2.22 | 1.58 |
| Stream, `kvstore.kv` bytes | on | 107,819,008 | 114,954,240 | 158,949,376 (8–10, flat) | **1.474** | 1.383 |
| Stream, `kvstore.kv` bytes | off | 107,819,008 | 125,808,640 | 227,700,736 (10, growing) | 2.112 | 1.810 |
| Materialize, `kvstore.kv` | on / off | the same bytes as Stream in every round | | | 1.474 / 2.112 | |
| Stream, RSS MiB | on / off | 59.5 / 57.5 | 112.9 / 116.7 | 122.4 / 172.4 | 2.06 / 3.00 | |
| Materialize, RSS MiB | on / off | 129.3 / 126.0 | 181.2 / 166.3 | 195.1 / 227.4 | 1.51 / 1.80 | |
| RocksDB, RSS MiB | on / off | 125.4 / 130.9 | 256.5 / 264.2 | 501.9 / 520.9 | 4.00 / 3.98 (block cache) | |

**What the memory RSS is made of** (one diagnostic run per policy with a
temporary, uncommitted build of the example that prints glibc's
`mallinfo2` and the RSS after `malloc_trim(0)` after every round): with
reclamation on, the heap in use goes 90.8 MiB (round 0) → 104.6 (round 1)
→ 115.5 MiB (round 10, 1.27×) while the triple count is back at round 0's
(−0.2 %), and malloc keeps 5.0 → 31.3 → 37.3 MiB of freed memory; the RSS
after a trim is 92.0 → 124.2 MiB (1.35×). Off: heap in use 90.8 → 188.6
MiB (2.08×, the dictionary). So about half of the growth is freed memory
the allocator keeps after the first round's deletes, the rest the
adjacency vectors growing past the bulk load's exact sizes. Neither is
new in this round (Off is the former behaviour: 2.22×).

**The memory delete overhead, profiled.** callgrind of the churn at univ
8, two rounds, collecting inside `remove_sources`: 2.600 G instructions
with reclamation on against 2.296 G off (+13.2 %); `flush_reclaim` is
275 M of the 304 M difference, and half of that (152 M) was
`Candidates::take` sorting one `TermId` per subject, predicate and object
of every removed triple. `b9e8dc0` removes that (packed keys, repeats
skipped). After it, a temporary build timing `flush_reclaim` puts the whole
reclaim step at 0.40 s of a 12.0 s delete total over the ten rounds
(3.4 %: freeing 323,804 terms 0.32 s, the reference probes 0.04 s, the
candidates 0.03 s). The rest of the gap (~0.9 s) is the same,
policy-independent delete code (`known_ids` lookups, `remove_many`'s list
passes; a delete does not touch the VS-tree) running slower on the state
reuse leaves (freed strings returned to malloc and reused, reused low ids
at the front of the per-predicate and per-object lists, so the deletes of
universities inserted with reused ids pass over longer list tails): in
round 1, whose state is identical for both policies, the median delete
after the fix is 989 ms on against 983 ms off (1.006, four runs each,
11-03), and the per-round ratio then grows to 1.07–1.34 in rounds 3–10.
Measured on / off: 1.18 before the fix; **1.13 / 1.12** after it (11-03 /
11-04, eight pairs on the committed build).

**The Materialize delete overhead, profiled (11-04).** One-round runs
(`--rounds 1`, univ 8, both layouts, two plain timed runs each): delete
on / off 1.14 and 1.05 on Materialize, 1.17 and 1.11 on Stream (the two
Materialize churn pairs' round 1: 1.12 and 1.16). So the frees cost about
the same in both disk modes, +1–3 s on an 18–21 s delete for the ~32 K
terms round 1 frees. Stream's ten-round ratio is below 1 because Off's
growing file (2.1×) slows its later deletes (rounds 6–10: off 74–112 s, on
40–46 s, 11-03), which Materialize's in-memory working set does not feel
(11-04: on / off 1.09–1.22 in rounds 1–9 of both pairs; round 10 1.35 and
0.81). All CPU: user time
equals the wall time, sys < 1 s; `strace -c` of round 1: 33.6 K against
25.4 K page reads and 27 against 23 `fsync`s, ~1 s of syscalls in all.
callgrind (Materialize, universities 0–2 cut to departments 0–1, `--base 2
--rounds 1`, collecting inside `remove_sources`): 13.07 G instructions on
against 11.92 G off (+9.6 %), and the difference is `flush_reclaim`
(1.143 G for 3,673 freed terms, ~311 K instructions each): the `*2id`
B+ tree delete 149 K and the free-list B+ tree insert (`release_index`)
133 K per term; the defensive reference probes are 0.13 % of the phase and
the operation brackets 0.02 %. A deleted triple costs ~820 K (its index
deletes). Every B+ tree write parses the whole node into per-entry
allocations and serializes it back (`Node::parse` 52 %, malloc + free
~60 % of the delete phase): the R1-KV B+ tree, which reclamation's two
writes per freed term use like every other write. So the overhead is the
work of the free itself at the kvstore's per-write cost, not a defect of
this round's code; the follow-up is in the B+ tree (below).

**Memory-layout inserts** (the implementer's 4.7× slowdown with reuse,
fixed by the integration's batched `insert_many`): in the churn on the
committed build, inserts total 12.13 s on against 12.24 s off (0.99×,
11-03) and 7.29 / 7.02 s (1.04×, 11-04); the implementer measured
40.66 / 8.67 s. The first university (round 1, medians) takes 1.10 /
1.35 s (11-03) and 0.76 / 0.75 s (11-04) on / off; the implementer: 6.7 s
against 0.87 s. Against the base binary, through the
command-line tools on the snapshot layout (no in-process churn exists in
`d702463`): `gbuild` of universities 0–7, then ten rounds of `gsub
university r` and `gadd university 8 + r`, each tool a fresh process (open,
apply, save); two runs per side in the order base, on, off, off, on, base;
sums over the ten rounds:

| side | `gbuild` | `gsub` total | `gadd` total | snapshot dir after round 10 |
|---|---:|---:|---:|---:|
| base `d702463` | 8.34 s | 99.07 s | 23.15 s | 158,929,970 B |
| HEAD, `id_reclaim=on` | 8.41 s | **17.17 s** (0.17× base) | **18.28 s** (0.79× base) | 86,928,588 B (entity live 167,544, free 9,226) |
| HEAD, `id_reclaim=off` | 8.76 s | 19.31 s (0.19×) | 21.59 s (0.93×) | 158,929,970 B (entity live 381,804) |

Memory-layout inserts with reuse are faster than the base's inserts, and
than HEAD's without reuse.

#### Fresh builds: `gstore-bench run --profile reference --scale {8,38} --no-conc --drop-caches`

Each side runs **its own harness** with its own `--bin-dir`
(`/tmp/gstore-base-d702463/bin/gstore-bench --bin-dir
/tmp/gstore-base-d702463/bin`, `target/release/gstore-bench --extra none
--bin-dir target/release`; RocksDB from `bin-rocks` / `target-rocks`): the
harness's child processes do the opens and the queries, so the
implementer's `target/release/gstore-bench … --bin-dir <base-bin>` measured
the branch's open and queries with only the base's `gbuild`. Stream =
`--backends snapshot,disk --env GSTORE_DISK_QUERY_MODE=stream`,
Materialize = `--backends disk --env GSTORE_DISK_QUERY_MODE=memory`,
RocksDB = `--backends rocksdb`. Univ 8: three pairs of each; univ 38:
three pairs snapshot + Stream, two of Materialize and of RocksDB; order
base, head, head, base, base, head; means of the raw runs (warm open =
median of the fresh-process re-opens, query = minimum over the 5 query
processes of the warm median, as the harness records them):

| univ | layout | build s | open first (cold) ms | open warm ms | Q1–Q14 geomean ms | slowest query vs base |
|---|---|---|---|---|---|---|
| 8 | snapshot | 8.18 → 8.36 (1.022) | 385 → 317 (0.822) | 277 → 270 (0.975) | 5.15 → 3.80 (0.739) | q12 0.150 → 0.162 (1.075) |
| 8 | Stream | 6.86 → 6.56 (0.957) | 6.5 → 8.7 (1.340) | 0.88 → 1.01 (1.147) | 12.29 → 4.91 (0.400) | q10 0.731 (all faster) |
| 8 | Materialize | 6.54 → 6.43 (0.983) | 420 → 432 (1.030) | 380 → 360 (0.948) | 9.60 → 3.34 (0.348) | q5 0.679 (all faster) |
| 8 | RocksDB | 7.15 → 6.54 (0.915) | 384 → 273 (0.711) | 296 → 225 (0.760) | 17.16 → 8.47 (0.494) | q12 0.270 → 0.275 (1.020) |
| 38 | snapshot | 46.62 → 46.49 (0.997) | 1779 → 1776 (0.998) | 1529 → 1564 (1.023) | 26.14 → 16.05 (0.614) | q11 1.397 → 1.673 (1.198), q12 0.565 → 0.649 (1.148) |
| 38 | Stream | 34.71 → 36.17 (1.042) | 7.4 → 8.0 (1.087) | 0.91 → 0.95 (1.050) | 84.67 → 18.70 (0.221) | q10 0.872 (all faster) |
| 38 | Materialize | 34.70 → 35.26 (1.016) | 2345 → 2199 (0.938) | 2088 → 2076 (0.994) | 68.91 → 12.66 (0.184) | q10 0.813 (all faster) |
| 38 | RocksDB | 36.93 → 35.42 (0.959) | 1385 → 1279 (0.923) | 1146 → 1060 (0.925) | 100.98 → 38.31 (0.379) | q5 1.019 |

Database sizes: snapshot byte-identical (85,480,180 / 430,848,180 B);
disk 130,150,405 → 130,150,400 and 653,959,174 → 653,959,168 B; RocksDB
0.993× / 0.999×. Build peak RSS within ±3.5 %. `gstore-bench compare
<base> <head>` of every pair: **PASS** (175 gates for snapshot + Stream,
96 for Materialize and for RocksDB; 0 failed in 16 pairs).

Most queries are much faster than the base (R4-VS's VS-tree policy and the
other Round-4 work); what is above +5 %:

- **Stream warm / cold open** (+0.13 / +2.2 ms at univ 8, +0.05 / +0.6 ms
  at univ 38). callgrind of a whole `gquery` ASK on a Stream database:
  830,770 instructions (base) against 838,284 (HEAD, a V2 file) and 840,878
  (HEAD, a V1 file built with `id_reclaim=off`): +0.9 %, the same without
  the free list; 20 fresh `gquery` processes take 5.57 / 5.87 / 5.17 ms
  each (base / V2 / V1). Process-level noise at the 1 ms scale, not the
  reclamation.
- **Snapshot q11 / q12 at univ 38** (+0.28 / +0.08 ms; q11 is 1.007× at
  univ 8). callgrind: `Evaluator::evaluate` 5.455 M → 5.390 M (q11) and
  2.872 M → 2.870 M instructions (q12); a whole `gquery q11`, everything
  after the open: 687.1 M → 687.0 M. The same work: the difference is in
  memory access on the 490 MiB univ 38 store (not at univ 8), not in
  reclamation code. Follow-up for the other parts of this measure stage
  (R4-SNAP's copy-on-write store layout is the candidate).
- The snapshot open's dictionary load costs +30 M instructions
  (`Interner::assemble`'s duplicate check, R4-RECLAIM's normalization):
  +2.3 % warm open at univ 38, within the bound.

**Identical answers:** rows and `rows_fnv64` of all 14 queries equal the
base's in every run, on every layout, at both scales; the update round trip
is `ok` on both sides (singles, chunk delta, answers unchanged, reopen
count). After it the base's dictionary has grown (univ 8: entities 168,432
→ 169,456, literals 86,612 → 87,120; univ 38: 842,657 → 843,783, 433,389 →
434,033), HEAD's is back to its fresh counts.

Update phase (report-only, means): single delete median snapshot 0.005 →
0.009 ms (univ 8), Stream 1.84 → 2.64 ms (univ 8) and 1.96 → 2.41 ms
(univ 38), Materialize 2.00 → 2.62 / 2.13 → 2.71 ms, RocksDB 0.34 → 0.37
ms (univ 8): a delete that leaves its terms unreferenced frees them. Chunk
delete Stream univ 38 1,422 → 1,761 ms (head runs 1,402 / 1,979 / 1,903).
Disk checkpoint 0.04–0.07 → 0.5–0.7 ms; snapshot univ 38 checkpoint 1,755 → 1,965
ms and in-process reopen 1,176 → 1,349 ms.

#### Format compatibility, by hand (univ 1, 139,905 triples)

- `d702463`'s `gbuild --disk v1` writes `GSTOREKV`. HEAD's `gsub` (400
  triples) and `gadd` (200 new triples) with `GSTORE_ID_RECLAIM=off` keep
  it `GSTOREKV` (so does a HEAD `gsub` with the default policy: a V1 file
  is never upgraded implicitly). **`d702463`'s `gquery` reads it** and
  answers Q1–Q14 (3 to 11,489 rows) exactly as it answers on a copy the
  base tools updated the same way, and as HEAD's `gquery` answers (sorted
  output md5 per query); triple count 139,705 everywhere.
- `gcompact v1 --enable-reclaim`: exit 0, "kvstore.kv is now version 2",
  magic **`GSTOREK2`**; `gcompact --reclaim --check` frees the 101 terms
  the reclaim-off delete left (entity free 50, literal free 51) and reports
  `integrity ok`; HEAD's answers unchanged.
- **`d702463`'s and `0fb8842`'s `gquery` and `gadd` refuse the upgraded
  file loudly**: exit 1, "this gStore KV file uses feature bits 0x1 that
  this binary does not support (written by a newer gStore); open it with
  that version"; `kvstore.kv` unchanged by the refused opens (md5).
- New HEAD databases are V2: `gbuild --disk` and `gbuild --layout disk` →
  `GSTOREK2` (`GSTORE_ID_RECLAIM=off gbuild --disk` → `GSTOREKV`), refused
  by both old `gquery`s the same way; a HEAD `gsub` of 400 triples on it
  frees 101 ids at once (`gmonitor`: entity live 23,543 free 50, literal
  live 12,421 free 51).

#### Acceptance (R4-RECLAIM and G5), quiet

| item | verdict | measured |
|---|---|---|
| Live counts equal the referenced terms exactly, every round (univ 8) | **met** | all 11 rounds on memory, Stream, Materialize and RocksDB |
| … and within ±5 % of round 0 (Off grows ~2.2×) | **not met** (+5.20 %) | 268,332 / 255,061 at round 4; the data itself is +5.23 % (live / triples 0.2480 in both rounds). Off: 2.27× |
| High-water mark ≤ 1.05 × peak live | **met** | 1.000 on every layout |
| RSS ≤ 1.15 × round 0 (memory) | **not met** (1.53×) | 96.8 → 148.3 MiB; 1.13× round 1; heap in use 1.27×, freed memory malloc keeps 37 MiB; Off 2.22× |
| `kvstore.kv` ≤ 1.2 × round 0 (Stream) | **not met** (1.474×) | 107.8 → 158.9 MB, flat from round 8; Off 2.112× and growing |
| Delete Immediate / Off ≤ 1.10, memory | **not met** (1.13 / 1.12) | 8 pairs on `b9e8dc0`: 11.84 / 10.46 s (1.132, 11-03) and 6.79 / 6.09 s (1.115, 11-04); 1.184 before the fix; the reclaim step itself 3.4 %, round 1 (same state) 1.006 |
| … ≤ 1.10, disk | Stream **met** (0.634); Materialize **not met** (1.139) | Stream 441.0 / 695.9 s (`7538dfa`); Materialize two pairs on `b9e8dc0` 446.2 / 391.8 s (1.188, 1.092), and 877.7 / 796.0 s (1.103) on `7538dfa`: two B+ tree writes per freed term |
| … ≤ 1.20, RocksDB | **met** (0.975) | 61.25 / 62.82 s, release build |
| Fresh builds: build, open, Q1–Q14 within ±5 % (univ 8 and 38) | **not met as written** | builds 0.915–1.042 (none more than +5 %; RocksDB univ 8 8.5 % faster); open and queries mostly far faster (R4-VS); above +5 %: Stream open (+0.13 ms warm at univ 8, cold ms-scale), snapshot q12 (univ 8, +12 µs), q11 / q12 (univ 38, +0.28 / +0.08 ms) — same instruction counts as the base, not reclamation code |
| Identical answers | **met** | rows and row hashes of Q1–Q14 equal the base's on 4 layouts × 2 scales, every run |
| Old binary reads V1 (written with reclaim off) and refuses V2 | **met** | above; also after `gcompact --enable-reclaim` |
| Memory-layout inserts (report) | — | on / off 0.99× / 1.04× in the churn (11-03 / 11-04); CLI `gadd` 0.79× the base |

**G5 (id reclamation): FAIL** — met: live = referenced in every round,
high-water 1.000, delete overhead Stream 0.63×, RocksDB 0.98×; not met:
within ±5 % of round 0 (+5.20 %, the data's own size change), delete
overhead on memory (+12–13 %) and on Materialize disk (+14 %).

Follow-ups (plan §5.7; recorded for R4-CLOSURE):

1. **±5 % of round 0**: unattainable with this churn while live =
   referenced exactly — the referenced terms themselves move +5.2 %. Use
   universities of equal size, or state the bound per triple (live /
   triples: 0.2480 in every round here).
2. **Memory delete overhead** (+12 %; the reclaim step itself 3.4 % after
   `b9e8dc0`): profile the deletes after reuse with a cache simulation
   (`callgrind --cache-sim=yes`, or `perf` on a box that has it); options:
   one retire / release pass per interner per statement instead of per
   term, a bitset instead of the `BTreeSet` free list, freeing the strings
   of a statement in one batch.
3. **RSS ≤ 1.15 × round 0**: round 0 is a bulk load with exactly sized
   lists and an untouched heap; exact-size growth for batched merges
   (`reserve_exact`) and a `malloc_trim` after large deletes would close
   most of it, or bound it against a steady-state round (1.13× of round 1
   here).
4. **`kvstore.kv` ≤ 1.2 × round 0**: the bulk image packs every page; a
   fill factor below 100 % in `bulk::kvimage`, or a bound against round 1
   (1.38× here, flat from round 8).
5. **Materialize delete overhead** (+14 %; Materialize is the default
   disk query mode): a free is two B+ tree writes (`*2id` delete,
   free-list insert) at ~140 K instructions each, because every
   `bptree` write parses the whole node into per-entry allocations and
   serializes it back. Options: a batched, sorted update path in
   `kvstore::bptree` (one parse and one serialize per touched leaf per
   statement) used by `free_term_in` / `release_index` and by
   `delete_keys` — it would cut both sides; or an allocation-free,
   in-place leaf edit. Stream pays the same per free but is net faster
   than Off over the churn (0.63×).
6. Snapshot q11 / q12 at univ 38 (+20 % / +15 %, same instructions): for
   the R4-SNAP part of this measure stage.

Commands (this stage):

```sh
cargo build --release -p gstore --bins --example reclaim_churn -p gstore-bench
CARGO_TARGET_DIR=target-rocks cargo build --release -p gstore -p gstore-bench --bins \
    --example reclaim_churn --features gstore/rocksdb,gstore-bench/rocksdb
D=/tmp/gstore-bench-cache/lubm-like1-u38-s0.nt
for layout in memory stream materialize; do for policy in on off; do
  flock /tmp/gstore-bench.lock target/release/examples/reclaim_churn --data "$D" \
      --layout $layout --policy $policy --base 8 --rounds 10 --scratch /tmp/r4reclaim-churn
done; done   # rocksdb: target-rocks/release/examples/reclaim_churn --layout rocksdb
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
B=/tmp/gstore-base-d702463/bin   # bin-rocks with target-rocks for --backends rocksdb
flock /tmp/gstore-bench.lock $B/gstore-bench run --profile reference --scale 8 \
    --backends snapshot,disk --env GSTORE_DISK_QUERY_MODE=stream --no-conc --drop-caches \
    --bin-dir $B --out base.json
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale 8 \
    --extra none --backends snapshot,disk --env GSTORE_DISK_QUERY_MODE=stream --no-conc \
    --drop-caches --bin-dir target/release --out head.json
target/release/gstore-bench compare base.json head.json
# the Materialize free-path profile (data: universities 0-2 cut to the
# subjects of departments 0-1, 39,468 triples):
valgrind --tool=callgrind --collect-atstart=no --toggle-collect='*remove_sources*' \
    target/release/examples/reclaim_churn --data small.nt --layout materialize \
    --policy on --base 2 --rounds 1 --scratch /tmp/r4prof/scratch-on   # and --policy off
```

Every database this stage built was deleted.

### Implementer run (noisy)

All numbers: **implementer run, noisy** — this machine (4 vCPUs, 16 GiB),
shared with other agents (load average 1–8 during the runs), every heavy
command under `flock /tmp/gstore-bench.lock`. Release build of `31b4b52`
(`cargo build --release -p gstore --bins --example reclaim_churn -p
gstore-bench`); base = `/tmp/gstore-base-d702463/bin`. The quiet measure
stage should re-run the commands at the end of this section.

#### G5 churn: `examples/reclaim_churn.rs`, univ 8 live, 10 rounds

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

#### Fresh builds: `gstore-bench run --profile reference --scale 8`

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

#### Old-binary compatibility (proved by hand, `compat.sh`)

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

#### Tests (this machine, debug/optimized test profile)

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
