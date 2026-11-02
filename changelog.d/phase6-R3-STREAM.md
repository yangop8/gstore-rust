# R3-STREAM — `Backend::Disk` Stream mode in Database: out-of-core triples and dictionary

Package R3-STREAM of `docs/PHASE6_PLAN.md` §6 (Round 3), contract C7. Base:
`0fb8842` (Phase 6 Rounds 1 and 2 merged). Commits per sub-part: (1)
`Backend::Disk`, (2) `DiskDict::intern`, (3) open options and `[storage]`,
(4)+(5) Stream mode in `Database` (open, writes, transactions), (6)/(6b) the
disk build delegation, (7)/(7b) posting lists and point probes, (8) tests,
(9) CI.

## Changelog

### Phase 6 R3-STREAM: on-disk databases can answer queries straight off the B+ trees

- **Stream mode** (`DiskQueryMode::Stream`). An on-disk (`kvstore.kv`,
  `gbuild --disk`) database opened in Stream mode evaluates queries over the
  B+ trees (`Backend::Disk(DiskTriples)`, reads reported per statement, C2)
  with an out-of-core dictionary (`DiskDict` on the same pager, opened once
  per database and shared by every statement). Opening reads nothing
  proportional to the data except the named graphs (kept in memory, D11) and
  the O(P) predicate statistics: at univ 8 (1.03M triples) the open takes
  0.6–0.8 ms instead of about 300 ms, RSS after open is 7.1 MiB instead of
  90 MiB, and the query high-water mark 70 MiB instead of 121 MiB
  (implementer run, see Measured numbers). `vstree.kv` is used while it is
  fresh and is never rebuilt at open unless `vstree_on_open` is set;
  `rebuild_index()` rewrites it (one streaming pass over SPO/OSP, deferred to
  the end of an open transaction) and marks it fresh in the store's header.
- **Materialize mode stays the default** (`DiskQueryMode::Materialize`, the
  pre-Phase-6 in-memory working set with write-through by dictionary key).
  The default is the single constant `gstore::db::DEFAULT_DISK_QUERY_MODE`;
  the measure stage flips it to Stream with a one-line commit only if every
  R3-STREAM gate passes at univ 38. Both modes read and write the same
  version-1 `kvstore.kv` (no feature bit), so a directory can be opened in
  either mode, and by older binaries, in turn. In Materialize mode a
  dictionary whose ids have a gap (a duplicate or missing slot) is now read
  with its ids kept, and `vstree.kv` is then rebuilt instead of trusted.
- **Open options (contract C7)**: `OpenOptions { disk_query_mode,
  disk_cache_pages, dict_cache_bytes, vstree_on_open, default_layout }`,
  `Database::open_with(dir, &options)`, `Database::load_disk_with`,
  `Database::disk_query_mode()`, `set_default_open_options` /
  `default_open_options` (all re-exported at the crate root). Precedence:
  `Database::open` uses the installed defaults (or the built-in ones) with the
  environment on top — `GSTORE_DISK_QUERY_MODE=stream|memory`,
  `GSTORE_DISK_CACHE_MB`, and `GSTORE_VSTREE_ON_OPEN` (Round 1's RocksDB
  switch, now an override of `vstree_on_open`); `open_with` takes its
  options as given. A RocksDB open builds the VS-tree only with
  `vstree_on_open` (as before, off by default).
- **`conf.ini` `[storage]` section** (Rust extension): `disk_query_mode`
  (`memory`/`stream`), `disk_cache_mb` (16), `dict_cache_mb` (80),
  `vstree_on_open` (off), `default_layout` (unset; `snapshot`/`disk`/
  `rocksdb`, for the build paths of R4-SNAP). `GStoreConfig::open_options()`;
  `gserver` installs the section as the process defaults at startup and logs
  the effective mode.
- **Writes in Stream mode** go by id: a new term is interned through the
  shared pager first (`DiskDict::intern`: `id2*`, then `*2id`, then the count
  root, one pager operation under the pager write lock, no gate — an orphan
  term if its triple never comes), then the triple through `DiskTriples`
  (`touched()` exactly when it returns `Ok(true)`; a write error breaks the
  store and fails the statement). Named-graph quads go through
  `DiskTriples::named_op`. Transactions are the store's overlay: no triple
  reaches `kvstore.kv` before `commit`, `rollback` never touches it, and the
  default graph needs no undo log (the named graphs keep theirs). A broken
  store refuses every statement (reads included) until the database is
  reopened. `CLEAR DEFAULT` reads the default graph in a read scope and
  removes it under one drain of the writer gate. `into_memory()` reads the
  store and every term (ids kept). The in-memory substring index is never
  built over a backed dictionary (substring filters scan, D5).
- **`DiskDict` can intern** (`DiskDict::writable()`; read-only stays the
  default for `DiskStore::lazy_dictionary` and views). A new term bumps the
  generation; since an intern only adds, the caches are updated in place
  rather than emptied. `DiskStore` now takes its term counts from the header
  wherever a shared dictionary may have interned (write operations, `flush`,
  `entity_num`, `dictionary()`, the lazy dictionary's freshness check), so no
  id is ever handed out twice.
- **`Database::build_disk` is the bulk build** (`bulk::build(..,
  BulkTarget::Disk, BulkOptions::default())`, also as
  `Database::build_disk_bulk_from_sources`): ids equal the memory build's,
  crash-safe install, and — like `gbuild --disk` — it refuses a directory a
  database of this process has open. The old bodies are kept as
  `#[doc(hidden)] build_disk_incremental` /
  `build_disk_from_sources_incremental`; `build_disk_from_sources` still
  forwards to the incremental build because its remaining callers use it as
  the incremental reference (see Requests).
- **Restore validation** opens the staged copy in Materialize mode, which
  reads the whole store, so a truncated on-disk backup is refused whatever
  the default mode is.
- **Faster Stream-mode reads** (results and order unchanged): `so_by_p`,
  `ps_by_o` and `subs_by_p` re-sort their index range with a radix sort on
  packed keys; `DiskTriples` keeps the long posting lists (2048+ entries) of
  the committed store in a 32 MiB cache keyed by the epoch (a write empties
  it); point reads of SPO (`exists`, `o_by_sp`, `po_by_s`) step a
  per-thread forward cursor instead of descending the tree for every probe
  of the engine's sorted candidates (a probe behind it or more than 48
  entries ahead descends as before; a locality score turns stepping off for
  random probe patterns). LUBM Q2 at univ 8: 1.39–1.44× the Materialize
  latency, from 2.0×; Q3 1.22×, from 2.16×.
- **CI** runs the test job twice, with `GSTORE_DISK_QUERY_MODE=memory` and
  `=stream` (same build). Tests that need one mode open with explicit
  `OpenOptions`.
- **Tests**: `tests/dt_disk_stream.rs` (Stream vs Materialize in order and vs
  the in-memory build as a multiset: LUBM Q1–Q14, named graphs, LIMIT /
  OFFSET / Top-K, paths with both ends unbound, CONTAINS / REGEX, DESCRIBE,
  CONSTRUCT, aggregates, graph functions; the read queries and data of the
  dt_sparql11 / dt_sparql_semantics / dt_sparql_syntax corpora; a seeded
  update workload with LOAD, CLEAR / DROP, ADD / COPY / MOVE, quads and
  `reason_execute` against an in-memory oracle, `mutation_count` advancing
  iff data changed, reopened alternately in Stream, Materialize and Stream
  mode; VS-tree freshness; terms interned on disk),
  `tests/dt_disk_stream_txn.rs` (read-your-writes, rollback and a commit
  point inside a transaction leave `kvstore.kv` byte-identical, commit +
  reopen in both modes, a 10,000-triple transaction, named graphs, an
  injected commit failure breaks the database until reopen, RDFS inference
  rolls back), `tests/dt_disk_stream_crash.rs` (`mem::forget` after the
  i-th acknowledged update, an uncommitted transaction across a `sync`, a
  crash between a term and its triple leaves only an orphan term), unit
  tests in `disk_dict.rs` (intern, rollback of a failed intern),
  `disk_triples.rs` (list cache), `store.rs` (radix sort, the SPO cursor
  against the tree), `open_options.rs` and `config.rs`.
  `db::tests::disk_database_mutation_invalidates_then_stays_correct` now
  covers both modes.
- **Changed test** (outside behaviour change): `dt_persist_backends`'
  `saving_another_layout_replaces_the_old_one` drops its open handle before
  rebuilding the directory with `build_disk` and checks that the bulk build
  refuses it while it is open.

## GAP_PLAN status line

§8 row 6 (规模与性能): R3-STREAM done — `Backend::Disk` Stream mode wired into
`Database` (out-of-core triples and dictionary, writes by id, store-overlay
transactions, broken-state refusal), open options C7 with `[storage]` and
environment overrides, `build_disk` on the bulk build; Materialize stays the
default (flip pending the univ 38 gates). Univ 8 implementer run: open 0.7 ms
vs 300 ms, RSS after open 7 MiB vs 90 MiB, Q1–Q14 geomean 1.24–1.26× the
Materialize mode.

## Docs notes

- docs/STORAGE_BACKEND.md (on-disk layout): the two disk query modes, what
  Stream mode keeps in memory (named graphs, the O(P) statistics, the page
  cache, the dictionary caches of `dict_cache_mb`, the 32 MiB long-list
  cache, one SPO cursor per reading thread) and what it reads on demand;
  `vstree.kv` used only while fresh, never rebuilt at open unless
  `vstree_on_open`, `rebuild_index()` rewrites it; writes by id, terms
  before triples (orphan terms harmless), transactions as the store overlay
  (no triple before commit; D12 still applies to very large commits); the
  broken state ("reopen the database"); `CLEAR DEFAULT` reads the default
  graph's ids into memory to remove them (no range delete, D9); a Stream
  database never builds the substring index (substring filters scan, D5);
  `Database::store()` panics for a Stream database (use `backend()`).
- docs/STORAGE_BACKEND.md (C7): `OpenOptions`, `Database::open_with`,
  `set_default_open_options` / `default_open_options`, the precedence
  (built-in < installed (`[storage]`) < environment; `open_with` as given),
  `GSTORE_DISK_QUERY_MODE`, `GSTORE_DISK_CACHE_MB`, `GSTORE_VSTREE_ON_OPEN`.
- docs/DEPLOY.md / docs/CONFIG (wherever `conf.ini` keys are listed): the
  `[storage]` section (`disk_query_mode`, `disk_cache_mb`, `dict_cache_mb`,
  `vstree_on_open`, `default_layout`); `gserver` applies it at startup (a
  loaded database keeps the mode it was opened in); the switch back to
  Materialize is `disk_query_mode=memory`.
- docs/DESIGN.md: `Backend::Disk(DiskTriples)` and the single id space of a
  Stream database (dictionary and store share the pager); lock order inside
  `Database` unchanged (`persist_lock` → disk → `persist_error` → pager), and
  `DiskDict::intern` takes only the pager write lock (gate → pager never
  inverted).
- docs/STORAGE_BACKEND.md (build): `Database::build_disk` uses the bulk
  build and refuses a directory a database of the process has open;
  `build_disk_incremental` is the old path.

## Measured numbers

**Implementer run, noisy (other agents compiling; load average 1.0–2.7 on 4
vCPUs during the runs).** Univ 8 only; univ 38 is left to the quiet measure
stage (commands below). Every heavy command ran under `flock
/tmp/gstore-bench.lock`. There is no base counterpart for Stream mode (it is
new); the comparison is the plan's: Stream against Materialize, same binary,
same session, on the same bulk-built data. Release binaries of this branch;
`gstore-bench run --profile reference --scale 8 --backends disk --no-conc
--env GSTORE_DISK_QUERY_MODE=stream|memory` (each run bulk-builds its own
database with this branch's `gbuild --disk`, 3.9–5.1 s, `kvstore.kv` +
`vstree.kv` 130.2 MB, deleted after the run), separate `--out` files,
compared with `gstore-bench compare memory.json stream.json --markdown`.

Final code (`894c0e9`), Materialize run between two Stream runs (a, b):

| metric | Materialize | Stream a | Stream b |
|---|---:|---:|---:|
| open first / warm ms | 273.0 / 304.2 | 0.7 / 0.7 | 0.8 / 0.6 |
| RSS after open MiB | 90.2 | 7.1 | 7.1 |
| query VmHWM MiB | 120.7 | 70.3 | 70.4 |
| Q1–Q14 geomean ms | 5.68 | 7.02 (1.24×) | 7.13 (1.26×) |
| single INSERT DATA median / p95 ms | 1.48 / 2.09 | 1.60 / 3.03 | 1.58 / 2.50 |
| single DELETE DATA median / p95 ms | 1.27 / 1.80 | 1.14 / 1.41 | 1.21 / 1.49 |
| chunk insert / delete ms (5,330 triples) | 687.5 / 824.2 | 545.8 / 1024.1 | 726.7 / 887.1 |
| reopen ms | 597.4 | 1.9 | 0.7 |

| query | rows | Materialize ms | Stream a ms (ratio) | Stream b ms (ratio) |
|---|---:|---:|---:|---:|
| q1 | 3 | 0.37 | 0.43 (1.16) | 0.41 (1.12) |
| q2 | 25 | 8.34 | 12.01 (1.44) | 11.55 (1.39) |
| q3 | 10 | 0.39 | 0.48 (1.23) | 0.48 (1.22) |
| q4 | 9 | 0.18 | 0.22 (1.25) | 0.23 (1.30) |
| q5 | 576 | 34.60 | 34.64 (1.00) | 34.09 (0.99) |
| q6 | 80,056 | 29.72 | 39.48 (1.33) | 40.07 (1.35) |
| q7 | 23 | 48.97 | 52.25 (1.07) | 55.08 (1.12) |
| q8 | 10,778 | 46.36 | 55.86 (1.20) | 59.03 (1.27) |
| q9 | 1,081 | 65.24 | 91.26 (1.40) | 93.04 (1.43) |
| q10 | 3 | 32.54 | 32.76 (1.01) | 38.19 (1.17) |
| q11 | 325 | 0.33 | 0.48 (1.48) | 0.46 (1.42) |
| q12 | 21 | 0.14 | 0.18 (1.29) | 0.17 (1.23) |
| q13 | 85,670 | 39.07 | 48.38 (1.24) | 49.81 (1.28) |
| q14 | 60,576 | 20.13 | 26.85 (1.33) | 28.03 (1.39) |

Every query's row count and row hash (`rows_fnv64`) are identical in the
three result files, and the update phase's round trip (singles, chunk,
re-check of every query) is `ok` in each. `gstore-bench compare`'s own
calibrated gates (Materialize as the base): PASS (96 checked).

Before the posting-list work (commit (6b) code, same procedure, one pair):
Stream geomean 7.97 ms vs Materialize 5.79 ms (1.38×), Q2 17.51 vs 8.72 ms
(2.01×), Q3 0.95 vs 0.44 ms (2.16×), Q4 1.10×, Q11 1.66×; single insert
median 1.65 vs 1.39 ms. A later pair with an intermediate SPO-walk heuristic
gave single inserts of 1.21 / 1.61 ms (Stream a / b) vs 1.97 ms
(Materialize): at this noise level the single-insert medians (fsync-bound,
20 samples) do not separate the modes.

Instruction counts (callgrind, `valgrind --tool=callgrind` over a scratch
program that opens the univ 8 database and runs one query N times with the
query cache off; deterministic, unlike the timings): LUBM Q2, open plus
three runs, 402M instructions before the posting-list work and 295M after;
one warm Q2 run is 95M instructions in Stream mode against 68M in
Materialize mode (the open subtracted). Q3, open plus three runs: 39M → 26M
(the long `type Publication` list comes from the list cache after the first
run). The remaining Stream overhead of Q2 is
the `o_by_sp` probes (B+ tree leaf stepping instead of a map lookup) and of
the large-result queries the out-of-core dictionary.

### Verification

`cargo fmt --all -- --check` clean; `cargo build --workspace --all-targets`
and `cargo clippy --workspace --all-targets -- -D warnings` clean;
`GSTORE_DISK_QUERY_MODE=memory cargo test --workspace`: 2,197 passed, 0
failed, 2 ignored; `GSTORE_DISK_QUERY_MODE=stream cargo test --workspace --
--skip tolerant_report_and_log_equal_the_sequential_import`: 2,196 passed, 0
failed, 2 ignored (the CI's two passes). RocksDB (`--features rocksdb`,
`-p gstore`): clippy clean; tests 2,018 passed, 0 failed, 2 ignored. After
the RocksDB directory lock was taken out again (Request 7) the RocksDB pass,
fmt, the workspace clippy and the touched test files in both modes were
re-run; the two full workspace passes above ran just before that change,
which touches only the RocksDB layout's open and create.

### Acceptance (R3-STREAM)

- RSS after open ≤ 120 MiB and RSS ratio univ 38 / univ 8 ≤ 1.5: **univ 8
  met** (7.1 MiB); univ 38 and the ratio **left to the measure stage**.
- Open warm ≤ 0.5 s, cold ≤ 2.0 s: **univ 8 met** (0.6–0.8 ms warm; the
  bench's first open 0.7–0.8 ms, page cache not dropped); univ 38 and a cold
  (`--drop-caches`) open **left to the measure stage**.
- VmHWM through Q1–Q14 ≤ 400 MiB: **univ 8 met** (70.4 MiB); univ 38 **left
  to the measure stage**.
- Each Q1–Q14 ≤ 2.0× Materialize, geomean ≤ 1.4×, Q1/Q3/Q4/Q11/Q12 ≤
  Materialize + 5 ms: **met at univ 8** in this noisy run (worst 1.48× Q11,
  geomean 1.24–1.26×; Q1/Q3/Q4/Q11/Q12 within +0.15 ms); the gate is at
  univ 38 — **left to the measure stage**.
- Auto-commit single-triple INSERT DATA median ≤ Materialize: **not
  established** — 1.58–1.60 ms vs 1.48 ms in the final pair, 1.21–1.61 vs
  1.97 ms in an earlier one; fsync dominates and the noise exceeds the
  difference. **Left to the measure stage** (quiet machine).
- Identical answers: **met** (all bench runs; `dt_disk_stream*` tests,
  in order against Materialize and as multisets against the memory build).
- Default flip: **not done** — Materialize stays the default until the
  measure stage has every gate at univ 38.

### For the measure stage (univ 38, quiet machine)

```sh
# release binaries of this branch (from the repository root)
cargo build --release -p gstore --bins -p gstore-bench
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
for run in stream-a:stream memory:memory stream-b:stream; do
  tag=${run%%:*}; mode=${run##*:}
  flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference \
      --scale 38 --backends disk --no-conc --drop-caches \
      --env GSTORE_DISK_QUERY_MODE=$mode --out /tmp/r3stream-u38-$tag.json
done
target/release/gstore-bench compare /tmp/r3stream-u38-memory.json /tmp/r3stream-u38-stream-a.json --markdown
target/release/gstore-bench compare /tmp/r3stream-u38-memory.json /tmp/r3stream-u38-stream-b.json --markdown
# the RSS / open ratios also need a Stream run at univ 8:
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference \
    --scale 8 --backends disk --no-conc --drop-caches \
    --env GSTORE_DISK_QUERY_MODE=stream --out /tmp/r3stream-u8-stream.json
```

Read from the Stream results: `RSS after open` (≤ 120 MiB; univ 38 / univ 8
≤ 1.5), `open warm` (≤ 0.5 s) and the `--drop-caches` first open (≤ 2.0 s),
`query HWM` (≤ 400 MiB); from the comparison: every query ≤ 2.0×, geomean ≤
1.4×, Q1/Q3/Q4/Q11/Q12 ≤ Materialize + 5 ms; single insert median ≤
Materialize. If all pass, flip `DEFAULT_DISK_QUERY_MODE` in
`src/db/open_options.rs` to `DiskQueryMode::Stream` (one line) and re-run
`cargo test --workspace` (the CI runs both modes already).

## Requests/Not yet

Requests (files outside R3-STREAM's list):

1. **Integrator / R3-ROCKS (`src/bin/gbuild.rs`) and R2-BULK's tests
   (`tests/dt_bulk_disk.rs`, `tests/dt_bulk_import.rs`)**: they use
   `Database::build_disk_from_sources` as the *incremental* reference
   (`gbuild --incremental`; `lubm_bulk_equals_incremental_and_memory`
   asserts the bulk file is ≤ 0.75× of it). Switch them to
   `Database::build_disk_from_sources_incremental`; then
   `build_disk_from_sources` can forward to `build_disk_bulk_from_sources`
   (one line in `persist.rs`), completing R2-BULK request 1. `build_disk`
   (used by gstore-bench and the tests) is already the bulk build.
2. **R3-SRV (`src/http_api.rs`, `ApiInner::refresh_config`)**: `refreshconf`
   should re-install `[storage]` for a server that owns the process
   defaults (`gserver`, as `with_log_refresh` does for the log):
   `crate::db::set_default_open_options(fresh.open_options())`. Databases
   loaded afterwards use it; loaded ones keep their mode. `gserver` installs
   it at startup.
3. **`tests/dt_bulk_import.rs` owner**: `tolerant_report_and_log_equal_the_sequential_import`
   reads an opened disk database through `Database::store()` and the legacy
   `id_to_string`, which only a Materialize database has; it should open
   with `Database::open_with(dir, &OpenOptions::materialize())`. Until then
   the CI's Stream pass skips it (`--skip
   tolerant_report_and_log_equal_the_sequential_import`; it still runs in
   the Materialize pass). It is the only test of the workspace that fails in
   Stream mode.
4. **R4-SNAP**: `/begin` (`ReadSnapshot::capture`) and `ConcurrentDb`
   snapshots of a Stream database still materialize the whole default graph
   through `iter_all` (O(triples) per `/begin`); use `DiskTriples::read_view`
   as planned (R2-DISKLIB's list). The snapshot's `Dictionary` clone shares
   the `DiskDict` (fine until ids are freed).
5. **R4-RECLAIM**: `DiskDict::free` must move both the generation and the
   caches' epoch (`cache_epoch`, which empties the caches; an intern does
   not); `Database::reclaim_unused` returns 0 for a backed dictionary until
   then. `DiskStore` takes the larger of its own and the header's term
   counts, which stays right with a free list (counts are the allocated
   limit). `LEGACY_PIN_SLOTS` was already removed at the Round-2 merge.
6. **R1-KV owner (`src/kvstore/bptree.rs`)**: the per-thread SPO cursor steps
   entry by entry (`LeafCursor::next_entry`, ~75 instructions per entry); a
   public "seek within the current leaf" (binary search on a uniform leaf)
   would make the stepping O(log) per probe.
7. **Integrator (`tests/dt_bulk_crash.rs`, `src/bulk/commit.rs`)**: the
   Round-2 review's request (addressed to R3-ROCKS) that `Database::open`,
   `create_rocksdb` and `open_rocksdb` take the directory's `DirLock` for the
   RocksDB layout too is **not** done here although `persist.rs` is this
   package's file: with the lock, a bulk build over a RocksDB database open in
   this process is refused by `bulk::commit`'s `DirLock::held_here` check
   ("is open in this process") before its RocksDB `LOCK` check, and
   `dt_bulk_crash::an_open_rocksdb_database_is_not_replaced` asserts the
   latter's message ("RocksDB store ... in use"). Taking the lock is three
   lines in `persist.rs` (`open_with`'s match, `create_rocksdb`,
   `open_rocksdb_with`) once that test accepts either refusal (or
   `bulk::commit` checks the RocksDB lock first); `http_api`'s
   `hold_dir_lock` skip for RocksDB then goes too.

Not yet / notes:

- Stream mode's page cache is the same 16 MiB as before (`disk_cache_mb`);
  larger caches did not change the instruction counts at univ 8, and the
  timings were too noisy to show an effect.
- `rebuild_index()` on a Stream database holds every entity's signature in
  memory while it writes `vstree.kv` (as the Materialize rebuild does); a
  sorted streaming writer (`VsTree::write_disk_sorted`) needs an external
  sort of the signatures.
- `CLEAR DEFAULT` on a Stream database reads the default graph's ids into
  memory (12 bytes per triple) before removing them (no range delete, D9).
- The corpus test extracts 54 distinct read queries (with their data) from
  the three SPARQL test files' string literals; queries assembled with
  `format!` arguments are not extracted.
- `gstore-bench` keys results by backend name, so the two modes need
  separate result files (R1-BENCH's note); a run label would help
  R4-CLOSURE's report.
