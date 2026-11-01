# Phase 6 R2-BULK — bulk index build for kvstore.kv (sorted, bottom-up, bounded memory, crash-safe install)

Package R2-BULK of `docs/PHASE6_PLAN.md` §6 (Round 2). Base: `aaea9f8`
(Phase 6 Round 1 merged). New module `src/bulk/` (std only, contract C6),
`gbuild --disk` switched to it, the `[system] build_*` configuration keys,
and three test files.

## Changelog

### Phase 6 R2-BULK: `gbuild --disk` builds the indexes in bulk

- **`gbuild --disk` is a bulk build** (`gstore::bulk::build`, contract C6).
  The input is parsed (per source, on up to `--threads` parser threads,
  replayed in source order), encoded into compact term maps (first-seen ids
  in exactly the in-memory build's order — subject, predicate, object,
  graph — so the ids equal the memory build's), sorted externally under a
  memory budget, and written bottom-up as page images: the SPO / POS / OSP
  (12-byte keys) and GSPO (16-byte keys) B+ trees at a 90% fill with the
  last two nodes of each level rebalanced, the `entity2id` / `literal2id` /
  `predicate2id` trees, the dense `id2*` IvArrays with their overflow
  chains, the per-predicate statistics array and the header. The result is
  a **version-1 `kvstore.kv`** in the unchanged format: it opens with
  `DiskStore::open` (and with older binaries), and the incremental writers
  keep inserting into and deleting from it. `vstree.kv` is written
  streaming from the sorted signatures and is byte-identical to the one the
  incremental build writes; the store's VS-tree flag is "fresh".
  - Univ 8 (1.03M triples, implementer run): 4–5.6 s instead of ~100 s for
    the incremental build, `kvstore.kv` 0.71× its size (see Measured
    numbers).
  - New flags: `--memory MB` (sort budget, default 1024), `--threads N`
    (`0` = every core, the default), `--tmp-dir DIR` (where sorted runs
    spill; default inside the database directory), `--fill PCT` (50–100,
    default 90) and `--incremental` (the previous one-insert-at-a-time
    path, for comparisons). They are refused (exit 2) without `--disk`.
  - Instead of reopening the database, `gbuild --disk` prints the counts
    and a summary of the phases, the memory (budget, peak, term map) and
    the spills.
  - `Database::build_disk*` and `/api build` stay incremental this round.
- **Crash-safe install.** The build holds the directory's `LOCK` (another
  process holding it, or a database of this process having it open, makes
  the build fail) and an in-process claim (`db::dirlock::BuildGuard`: a
  second bulk build of the same directory fails), and writes everything
  into `<db>/.bulk-tmp/`. The install then: marks the staging area
  (`COMMIT`), replays and removes a stale `kvstore.kv.wal` of the old store
  and removes the old `vstree.kv`, renames the new `kvstore.kv` over the
  old one, renames `vstree.kv` into place, removes the other layouts
  (snapshot files; `rocksdb/` renamed aside durably, then deleted) and the
  staging area, syncing the directory after each step. A crash at any step
  leaves the old or the new database, which an unmodified binary opens. A
  failed build (a strict import's first bad statement, a missing source, a
  term too long for the disk layout) leaves an existing database untouched.
  A stale `.bulk-tmp/` is removed by the next build.
- **Same import semantics as the sequential import**: the import report,
  the `parse_error.log` lines and a strict import's first error are those
  of `import_sources`, at every thread count. A term the disk layout cannot
  store fails with exactly the incremental build's error (the 2038-byte
  B+ tree key limit, the 33,554,432-entry IvArray capacity, the id-space
  bounds of `checked_*_id`).
- **Memory.** Sort buffers, the signature spool and merge buffers stay
  within the budget (a quarter of it split between the buffers live in a
  phase; beyond that sorted runs spill to scratch files, merged with a
  fan-in of `min(64, share / 256 KiB)`); with everything fitting, nothing
  is written to scratch at all. The term maps (about `L + 26` bytes per
  term) are outside the budget and reported.
- **Deterministic output**: `kvstore.kv` and `vstree.kv` are byte-identical
  whatever the thread count or memory budget (page ranges are assigned in a
  fixed order; the parallel POS, OSP + VS-tree and dictionary tasks write
  only their own ranges). (Two builds of data with blank nodes differ, as
  every build path's per-document blank-node scopes are random.)
- **Configuration** (`conf/conf.ini` `[system]`, read by `gbuild` from
  `./conf/conf.ini` when it exists): `build_memory_mb` (1024),
  `build_threads` (0 = every core), `build_tmp_dir` (unset),
  `build_fill_factor` (90); `build_multi_thread=off` means one thread.
  `GStoreConfig::bulk_options()` maps them to `bulk::BulkOptions`.
- **API** (`gstore::bulk`): `build(dir, name, BulkInput, &ImportOptions,
  &mut dyn ErrorLog, BulkTarget, &BulkOptions) -> Result<BulkReport>`;
  `BulkInput::{Sources, Text}`; `BulkTarget::{Disk, RocksDb}` (RocksDB:
  "not supported yet" until R3); `BulkOptions { memory_budget, threads,
  temp_dir, fill_percent }`; `BulkReport` (import report, counts, phase
  times, budget peak, term-map bytes, spills, file sizes; `summary()`);
  `bulk::commit::{STAGING_DIR, CRASH_POINTS}`; test hooks
  `bulk::debug_crash_at(dir, step)` and the `GSTORE_BULK_CRASH=<step>`
  environment variable (abort after an install step).
  `VsTree::write_disk_sorted(path, entries)` writes `vstree.kv` streaming
  from entries in clustering order.
- New files: `src/bulk/{mod,budget,scratch,extsort,termmap,encode,source,
  pipeline,stats,kvimage,sigs,commit}.rs`, `tests/dt_bulk_disk.rs`,
  `tests/dt_bulk_import.rs`, `tests/dt_bulk_crash.rs`.

## GAP_PLAN status line

Phase 6 R2-BULK (bulk index build): done — `gbuild --disk` sorts and writes
`kvstore.kv` (V1) and `vstree.kv` bottom-up under a memory budget and
installs them crash-safely; ids equal the memory build's; univ 8 in ~4–6 s
(was ~100 s).

## Docs notes

- **README.md** (the `gbuild` section): `--disk` builds in bulk; the new
  flags `--memory MB`, `--threads N`, `--tmp-dir DIR`, `--fill PCT`,
  `--incremental` (refused without `--disk`); the printed summary (counts,
  phases, budget / peak, term map, spills, file sizes).
- **docs/STORAGE_BACKEND.md** (disk layout):
  - How a disk database is built: `gbuild --disk` writes the indexes
    bottom-up from sorted data at the fill factor (90%: denser files, and
    later inserts split nodes somewhat more often; `--fill 70` leaves more
    slack); the file is an ordinary version-1 `kvstore.kv`; ids are the
    in-memory build's. `Database::build_disk*` and `/api build` are still
    the incremental path (until R3), which interns a quad's graph before its
    subject, so for N-Quads / TriG input its ids differ from the memory
    build's (the contents are equal).
  - The install: the `<db>/.bulk-tmp/` staging area (never a database; a
    stale one is removed by the next build), the step order and what a
    crash at each step leaves (the table in `src/bulk/commit.rs`), and that
    a bulk build refuses a directory another process holds, one a database
    of the same process has open, and one another bulk build is
    rebuilding.
  - Memory: the budget covers the sort buffers and spools; the term maps
    (about `L + 26` bytes per term of `L` bytes, 24 MiB at univ 8) come on
    top. Spilled runs go to `--tmp-dir` (default: the staging area), so a
    build needs free disk for the new database next to the old one plus,
    when it spills, about one copy of the encoded triples (12 bytes per
    triple per sort) and of the signatures (124 bytes per entity).
  - Over-long terms: a key longer than 2038 bytes fails the build with the
    B+ tree's error, as the incremental build does; the IvArray capacity is
    33,554,432 terms per space.
- **docs/DEPLOY.md** (`conf.ini`): the `[system]` keys `build_memory_mb`,
  `build_threads`, `build_tmp_dir`, `build_fill_factor`; `gbuild` reads
  `./conf/conf.ini` when it exists; `build_multi_thread=off` (upstream's
  shipped value, also in `conf.ini.example`) makes the bulk build use one
  thread.
- **docs/PHASE6_PLAN.md**: (a) the install order differs from the brief:
  the old `vstree.kv` goes first, then the new `kvstore.kv` is renamed in,
  and only then are `rocksdb/` and the snapshot files removed. Moving them
  aside before the rename (the brief's step 2) would leave a directory with
  no database after a crash between the two, and an old `vstree.kv` beside
  a new store marked fresh would be trusted. (b) The key limit the bulk
  encoder enforces is the 2038-byte `bptree::MAX_KEY_LEN` (R1 review), not
  4,081 bytes. (c) `DiskTree::build_sorted(path, iter)` is
  `VsTree::write_disk_sorted` (the `disk_vstree` module is private to
  `signature`, whose `mod.rs` is not R2-BULK's). (d) A bulk image has no
  slack pages, so its free list is empty (`kvimage` keeps a tested
  free-list writer).
- **CHANGELOG.md**: fold the Changelog section above.

## Measured numbers

**Implementer run, noisy (other agents compiling).** 4 vCPU / 16 GB VM;
load average 1.2–2.1 at the start of each run; every heavy command under
`flock /tmp/gstore-bench.lock`; base and branch measured in the same
session. Base = the prebuilt release binaries of `aaea9f8`
(`/tmp/gstore-base-aaea9f8/bin`); branch = `780e9a4` (release; later
commits only add the in-process refusal, clippy fixes and this fragment).
Data: `lubm-like1-u8-s0.nt` from `/tmp/gstore-bench-cache` (1,064,650
lines, 1,029,275 unique triples, 168,432 entities, 86,612 literals, 17
predicates). univ 38 was **not** measured (left to the quiet measure
stage; commands below).

`gstore-bench run --profile reference --scale 8 --backends disk` (5 query
processes × 5 reps, 3 warm opens; `GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache`):

| metric | base `aaea9f8` | branch `--incremental` | branch (bulk) | branch `--memory 128` |
|---|---:|---:|---:|---:|
| build wall s | 96.98 | 89.03 | **4.20** | 4.28 |
| gbuild peak RSS MiB | 123.1 | 123.4 | 96.7 | **74.1** |
| `kvstore.kv` bytes | 152,735,744 | 152,735,744 | **107,880,448** (0.706×) | 107,880,448 |
| `vstree.kv` bytes | 22,269,952 | 22,269,952 | 22,269,952 (byte-identical, tested) | 22,269,952 |
| directory bytes | 175,005,702 | 175,005,702 | 130,150,406 (0.744×) | 130,150,406 |
| open first / warm ms | 497.9 / 367.3 | 335.3 / 342.6 | 390.1 / 350.2 | 317.4 / 364.6 |
| RSS after open MiB | 113.9 | 115.6 | 115.6 | 115.8 |
| Q1–Q14 warm geomean ms | 5.74 | 5.68 | **5.35** | 5.78 |
| rows and row hashes | — | = base | = base | = base |
| update: single insert median ms / 5,330-triple chunk insert ms | 1.26 / 533 | 1.32 / 544 | 1.41 / 638 | — |

Per query (warm ms, base / `--incremental` / bulk): Q1 0.41/0.36/0.37,
Q2 8.58/8.83/8.06, Q3 0.42/0.42/0.40, Q4 0.17/0.17/0.17, Q5
32.57/32.03/31.56, Q6 28.48/28.26/27.24, Q7 48.63/49.50/45.63, Q8
47.57/48.26/43.49, Q9 66.62/67.69/60.42, Q10 32.67/33.42/30.12, Q11
0.32/0.34/0.32, Q12 0.14/0.13/0.12, Q13 38.26/39.30/34.90, Q14
19.95/19.45/18.80 (bulk / incremental 0.90–1.03 per query).
`gstore-bench compare` of the bulk result against the base and against
`--incremental` (`--backends disk`): **PASS, 105 gates** each (build
ratio 0.05). The denser tree (90% fill) makes the update phase's chunk
insert about 17% slower than into the incrementally built tree (more node
splits); single inserts and deletes are unchanged within noise.

`gbuild --disk` directly (wall time and peak RSS from `getrusage` of the
child; the phase times are gbuild's own summary):

| variant | wall s | peak RSS MiB | parse+encode / spo / index / finish ms | budget peak MiB | spills |
|---|---:|---:|---|---:|---|
| default (`--threads` = 4) | 3.58–4.08 | 97 | 3155–3682 / 115–120 / 172–214 / 90 | 88 | none |
| `--threads 1` | 4.01–4.14 | 115 | 3479–3640 / 125–142 / 277–295 / 85 | — | none |
| `--memory 128` | 4.90 | 80.5 | 4360 / 244 / 180 / 105 | 48 | 3 runs, 47.2 MiB |

The term maps took 24.1 MiB. The `--memory 128` build's files are
byte-identical (`cmp`) to the default build's.

Hot spots: the build is dominated by parsing. `parse+encode` is 85–90% of
the wall time, and the Turtle parser (`.nt` files are read as Turtle) on
its single thread is the bottleneck. Encoding (term-map lookups) runs
beside it on the calling thread, so with parser threads the phase takes
about the parse time. The sorts, the three index trees, the dictionary,
`vstree.kv` and the `fsync` take 0.4–0.6 s together at univ 8. (Before the
parser threads dropped their own quads, the pipelined build cost 1.2 s
more CPU than the sequential one: freeing another thread's allocations.)

**Acceptance (R2-BULK):**
- build ≤ 15 s at univ 8 — **met** (4.20 s; 0.043× base, 0.047× the
  same-session incremental build);
- build ≤ 60 s at univ 38 — **left to the measure stage** (not measured;
  everything but parsing costs well under a second per million triples
  here, and parsing scales linearly);
- peak RSS ≤ 1.25 GiB at univ 38 with the default budget — **left to the
  measure stage** (univ 8: 97 MiB);
- `--memory 128` at univ 8: peak ≤ 400 MiB and at least one spill —
  **met** (74–81 MiB, 3 sorted runs spilled);
- `kvstore.kv` ≤ 0.75 × pre — **met at univ 8** (0.706× the base build's
  file; R1's quiet stage showed that file equals the pre-Phase-6 one);
  univ 38 left to the measure stage;
- open and Q1–Q14 within ±10% of an incrementally built database —
  **met** (warm open +2.2%, geomean 0.94×, every query 0.90–1.03×; the
  single first-open sample +16%, while the base's own first open was
  498 ms);
- identical answers — **met** (rows and row hashes of all 14 queries;
  `tests/dt_bulk_disk.rs` compares rows *and order* with the incremental
  and memory builds on `testdata/lubm`).

**Commands for the quiet measure stage (univ 38):**

```sh
cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale 38 \
    --backends disk --drop-caches --out /tmp/r2bulk-u38-bulk.json
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale 38 \
    --backends disk --drop-caches --gbuild-arg --incremental --out /tmp/r2bulk-u38-incr.json
target/release/gstore-bench compare bench/results/pre-phase6-lubm38.json /tmp/r2bulk-u38-bulk.json \
    --backends disk --markdown
target/release/gstore-bench compare /tmp/r2bulk-u38-incr.json /tmp/r2bulk-u38-bulk.json \
    --backends disk --markdown
# gbuild's own summary (phases, budget peak, term map, spills) at univ 38:
rm -rf /tmp/r2b38.db
flock /tmp/gstore-bench.lock target/release/gbuild --disk /tmp/r2b38 \
    /tmp/gstore-bench-cache/lubm-like1-u38-s0.nt
rm -rf /tmp/r2b38.db
# the univ 8 low-memory variant on the quiet machine:
flock /tmp/gstore-bench.lock target/release/gstore-bench run --profile reference --scale 8 \
    --backends disk --no-update --no-conc --gbuild-arg --memory --gbuild-arg 128 \
    --out /tmp/r2bulk-u8-m128.json
```

G1a reads off the first run: build wall (≤ 60 s), gbuild peak RSS
(≤ 1.25 GiB), and `build.files["kvstore.kv"]` against the pre-Phase-6
univ 38 file (≤ 0.75×; `pre-phase6-lubm38.json`'s 889,778,182 directory
bytes include `vstree.kv`, which the bulk build writes byte-identical).

Tests (optimized test profile, this machine): `tests/dt_bulk_disk.rs` 7
tests ~16 s, `tests/dt_bulk_import.rs` 3 tests ~1 s,
`tests/dt_bulk_crash.rs` 5 tests ~8 s (the SIGKILL loop stops after 25 s
at most), `bulk::` unit tests 17 tests ~9 s.

## Requests/Not yet

Requests (outside R2-BULK's files):

1. **R3-STREAM / integrator — delegate `Database::build_disk*` and `/api
   build` to `bulk::build(…, BulkTarget::Disk, …)`.** They are still the
   incremental path. A bulk build refuses a directory that a database of
   the same process has open (`DirLock::held_here`), so the server must
   release its handle (unload) before rebuilding, as it does for a drop.
   After delegation the disk ids equal the memory build's for N-Quads /
   TriG input too (today's incremental path interns a quad's graph first).
2. **R3-ROCKS — `BulkTarget::RocksDb`.** It returns "not supported yet".
   The pieces an SST sink needs are in place: `source` (ordered parse),
   `encode` (first-seen ids, the disk term checks), `extsort` (sorted,
   de-duplicated SPO / POS / OSP / GSPO streams), `stats` (exact counts)
   and `commit` (the staging + install pattern; a RocksDB install would
   swap a staged `rocksdb/` in with `fsutil::install_staged_dir`).
3. **Parser owner (`src/parser`) — single-file parse speed.** At univ 8 the
   build is 85–90% parsing, on one thread per source. `.nt` files are read
   by the Turtle parser; an N-Triples fast path for `.nt`, or a chunked
   parallel parse of line-based files (sharing one blank-node scope and
   keeping file line numbers for the error log), would shorten the build
   further. Not needed for the targets.
4. **R4-CLOSURE (`bench/**`)** — gstore-bench records gbuild's wall time
   and peak RSS but not its summary; parsing `spills:` / `memory:` out of
   `gbuild.log` would put the spill count and the budget peak into the
   result JSON.
5. **R4-RECLAIM (`bulk/kvimage.rs` in R4)** — a bulk image has an empty
   free list and `FEATURES == 0`; a build with `FREE_IDS` would write the
   free-id tree and its counts (slots 22–25) in the same pass and set the
   bit.

Not yet:

- univ 38 numbers (build time, peak RSS, file size) — the measure stage.
- The budget covers the sort buffers and spools, not the term maps (plan
  D3; reported in `BulkReport::term_map_bytes`) nor the parse pipeline
  (bounded: 4 batches of at most 8,192 quads per parser thread).
- Byte-identical output across runs holds for data without blank nodes:
  every build path draws a random scope per document for blank-node labels
  (`BlankNodes::PerDocument`), so two builds of the same file differ there
  (`BlankNodes::Keep` is deterministic).
- The 90% default fill makes later inserts into a freshly built database
  split nodes more often (chunk insert +17% at univ 8); `--fill` /
  `build_fill_factor` trade file size for that.
