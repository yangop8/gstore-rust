# R4-VS — the VS-tree policy: disk databases stop paying for `vstree.kv`

Round 4 package R4-VS (not in the plan's package list: it implements
R3-STREAM request 8, recorded for Round 4). Base: `d702463` (Phase 6
Rounds 1–3 merged). Files: `src/query/{engine,candidates,mod}.rs`,
`src/query/vs_policy.rs`, `src/query/count_memo.rs`,
`src/query/vs_policy_tests.rs` (new), `src/signature/{mod,disk_vstree}.rs`,
`src/kvstore/bptree.rs`, `tests/dt_vs_policy.rs` (new), this fragment.

## Changelog

### Phase 6 R4-VS: the VS-tree policy; disk queries no longer read `vstree.kv`

- **VS-tree policy** (`gstore::query::VsPolicy`). `Auto` (the default)
  computes exactly the candidate sets the VS-tree used to give, more
  cheaply where that is provably exact; `Tree` is the previous behaviour
  (the tree for every subject variable); `Off` never consults the tree.
  Switches, in order of precedence: `Evaluator::with_vs_policy`,
  `query::scoped_vs_policy(policy)` (per thread, a guard),
  `query::set_default_vs_policy`, and the environment variable
  `GSTORE_VS_POLICY=auto|tree|off` (read once). The library's own unit
  tests (`cargo test --lib`) default to `Tree`: two of `db::tests`
  (`disk_database_filters_via_on_disk_vstree`,
  `streaming_vstree_matches_in_memory_path`, in `src/db/mod.rs`, not this
  package's file) assert that a query paged `vstree.kv` nodes, which
  `Auto` no longer does (see Requests); integration tests and binaries get
  `Auto`. Per-thread decision counts:
  `query::vs_stats()` / `reset_vs_stats()` (`skipped`, `store_checked`,
  `listed`, `probed`, `walked`, `seeded`, `off`).
- **What `Auto` does.** A query signature's bits are those of the edges
  with a known neighbour (the candidate generator cut the variable's set
  from exactly those edges, and the tree keeps a superset of every
  entity's signature, so every candidate's entry has them) plus predicate
  bits of edges whose neighbour is a variable. So:
  - a set that already satisfies every bit keeps its entities without
    touching the tree (*skipped*; all layouts);
  - with an out-of-core tree (`vstree.kv`, Stream and Materialize mode)
    the remaining predicate bits are tested against the store
    (*store-checked*), and a variable without a set (its signature is then
    predicate bits only) gets the entities with those bits from the
    store's posting lists (*listed*): an entity's signature has the bit of
    predicate `p` in direction `d` iff it has an edge in `d` whose predicate
    encodes to that bit (several predicates can share a bit; all of them
    count). Per bit the engine looks candidates up or merges posting lists,
    whichever the exact counts (C1) make cheaper. `vstree.kv`'s pages are
    never read under `Auto`;
  - an in-memory tree's probes and walks are unchanged.

  On a tree consistent with the store the sets equal the tree's, so the
  plans, the answers and their order are unchanged (row hashes equal on
  every layout and mode, and order-identical to `Tree`, tested). After
  deletes in the same session (the tree keeps stale bits until
  `rebuild_index()`; a reopen drops a stale `vstree.kv`), an in-memory
  tree still gives `Tree`'s sets; an out-of-core one gives the current
  store's exact sets (a subset of the stale walk's): the same answers, the
  order of solutions without `ORDER BY` as a freshly rebuilt tree gives it.
- **Candidate generation** (all layouts, same sets): a variable's
  constant-edge lists are intersected shortest first; a list much longer
  than the set so far is not read — the set's members are tested against
  the store instead; lists are counted exactly only when the store's
  per-predicate average says they are short (on disk a long range count
  reads one B+ tree leaf per ~250 entries), longer ones are ordered by that
  estimate; lists already in contract order are not re-sorted; a much
  shorter set is binary-searched into a long one.
- **Count memo** (`query::count_memo`, internal): the exact counts the
  candidate generator and the optimizer ask for while one BGP compiles come
  from one memo, seeded with the length of every list read whole.
- **Constant stars**: a BGP of at most three patterns that all bind one
  variable with the other two positions constant (`?x p o`, `s p ?x`) needs
  no plan search: its solutions are the variable's candidates in ascending
  order whatever the join order (the plain optimizer's rows and order,
  tested).
- **Candidate lookups in scans**: a pattern with a constant end and a
  variable end holding at most 64 candidates, few against the predicate's
  average list, tests the candidates (`exists`, ascending) instead of
  reading the list — the same triples in the same order.
- **`vstree.kv` must have `FEATURES == 0`** (plan §3.1): the out-of-core
  tree refuses, on open and on a build over the file, a pager header with
  feature bits — also once R4-RECLAIM makes `FEATURE_FREE_IDS` a bit the
  pager itself accepts.
- **B+ tree**: `BTree::count_prefix` (and every run walk) decides a leaf
  lying wholly inside the run with one comparison of its last key instead
  of a binary search; new `LeafCursor::seek_in_leaf(key)` (binary search
  in the rest of a uniform leaf; for the disk store's SPO cursor, R3-STREAM
  request 6 — not used yet, see Requests).
- **Signatures**: `Signature::predicate_bit`, `predicates_with_bit`,
  `predicate_bits(dir)`, `only_predicate_bits`, `and_not`, `set_bits`.
- **Tests**: `tests/dt_vs_policy.rs` — LUBM Q1–Q14 and 120 seeded random
  BGPs over a graph whose predicates share signature bits, on the
  in-memory build and an on-disk database in Stream and Materialize mode:
  `Auto` answers what `Tree` answers in order, `Off` the same answers
  (in order under `ORDER BY`), decisions deterministic, `Auto` never reads
  `vstree.kv`, the disk modes equal in order and the memory build as a
  multiset; after deletes the answers stay equal under every policy.
  Unit tests (`query::engine::vs_policy_tests`): on random graphs and BGPs
  `Auto`'s candidate sets equal `Tree`'s on in-memory and out-of-core
  trees with either way of testing a bit forced, never a page of the disk
  tree read; stale trees give equal (memory) or sub- (disk) sets; the
  constant-star plan and the candidate lookups give the plain optimizer's
  rows in order; `retain_sorted`. `query::candidates`: generation equals
  reading every list (member tests forced either way). `count_memo`,
  `signature` (bit encodings and their inverse), `disk_vstree` (a feature
  bit is refused), `bptree` (`seek_in_leaf` against a fresh cursor).

## GAP_PLAN status line

§8 row 6 (规模与性能): R4-VS done — VS-tree policy (`Auto` computes the
tree's candidate sets from the store on disk databases, never reading
`vstree.kv`; skips refinements the candidate sets already satisfy),
shortest-first candidate generation with member tests, per-BGP count memo,
constant-star plans; quiet measure stage at univ 38: Stream Q1–Q14
geomean 11.7 ms = 1.11× the snapshot layout of the same commit (worst
query Q6 2.39×) and 0.08× pre disk — **G1d met** (Materialize 0.72×);
against the base: snapshot 0.58×, Stream 0.23×, Materialize 0.19×,
RocksDB 0.39×; the default disk query mode stays Materialize (Stream
1.54× Materialize, Q6/Q13/Q14 > 2×: R3-STREAM's flip conditions).

## Docs notes

- docs/DESIGN.md (query engine): the VS-tree policy (`Auto`/`Tree`/`Off`,
  what `Auto` skips or computes from the store and why the sets are the
  tree's), the decision counters, `GSTORE_VS_POLICY`; candidate generation
  shortest first with member tests; the per-BGP count memo; constant-star
  plans; candidate lookups in scans.
- docs/STORAGE_BACKEND.md (disk layout): under the default policy
  `vstree.kv` is not read by queries — its freshness still decides whether
  VS-tree semantics apply (a stale one is dropped at open as before), and
  `GSTORE_VS_POLICY=tree` restores the page walks; after deletes in a
  session a disk database's candidate sets follow the current store
  (answers unchanged); `vstree.kv` with pager feature bits is refused.
- docs/CPP_PARITY.md (VS-tree parity): upstream walks its VS-tree for every
  query; gstore-rust computes the same candidate sets from the store for an
  on-disk tree and skips refinements that cannot remove a candidate.
- README / CHANGELOG (performance): the numbers below once the quiet stage
  has them.

## Measured numbers

### Quiet post-merge stage (phase6 `20cdc06`; part 3 of 3: R4-VS, gate G1d, the default disk query mode)

Reference VM (4 vCPU, 16 GB), nothing else running, 11-04 05:37–06:23 UTC
for the timing runs (CPU busy 25.0 % on average, at most 35 %, sampled every
10 s from `/proc/stat`; steal ≤ 1 %; 1-minute load average ≤ 1.30; ≥ 7.8
GiB disk free), the callgrind profiles after them (06:24–07:11), and two
base / new single-insert pairs at 07:12–07:15 (busy 24.6 %, load ≤ 0.94). Toolchain 1.99.1,
`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`;
every heavy command under `flock /tmp/gstore-bench.lock`; data from
`/tmp/gstore-bench-cache` (univ 8 `ed44da2e7b3bbb98`, univ 38
`fcd6493725f3c66a`). **New** = release builds of `20cdc06` (code of
`b9e8dc0`; `cargo build --release -p gstore --bins -p gstore-bench`, and
`CARGO_TARGET_DIR=target-rocks … --features gstore/rocksdb,gstore-bench/rocksdb`);
**base** = the release binaries of `d702463` (the tree before this round:
`/tmp/gstore-base-d702463/bin`, `bin-rocks` for RocksDB), each side with its
own `gstore-bench` and `gbuild`, measured in this session. Per scale, in
this order (base and new interleaved, each run bulk-builds its own database
and deletes it):

```sh
<bin>/gstore-bench run --profile reference --scale {8,38} --no-conc --drop-caches \
    --backends snapshot | disk --env GSTORE_DISK_QUERY_MODE={stream,memory} | rocksdb \
    [--extra none: new side only, the base harness has no WatDiv-like workload] \
    --bin-dir <bin> --out /tmp/r4vs-u<s>-<base|new>-<snapshot|stream|memory|rocksdb>.json
```

(base snapshot, new snapshot, new Stream, base Stream, base Materialize,
new Materialize, new RocksDB, base RocksDB), then extra samples: RocksDB at
univ 8 three more pairs and at univ 38 two more (`--no-update`, order
alternated), three alternating Materialize/Stream pairs of 500 single
inserts at univ 38 (`--singles 500 --query-procs 1 --reps 1 --open-reps 1`),
the policy control (`--env GSTORE_VS_POLICY=tree`, Stream, univ 38,
`--no-update`) and a second new Stream / Materialize / snapshot sample at
univ 38 (`--no-update`, "run b"). Warm ms = the minimum over 5 processes of
the median of 5 runs. **Rows and row hashes of Q1–Q14 are identical in all
36 result files of this stage and equal to pre-Phase-6's** (every layout
and mode, base and new, stable in every process); every update round trip
`ok` (22 runs with the update phase). `gstore-bench
compare` base → new: PASS (96 gates) for snapshot, Stream, Materialize and
RocksDB at both scales; pre-Phase-6 disk → new Stream / Materialize: every
gate passes but `exact.phase disk/conc` (this stage ran `--no-conc`).

#### Univ 38 (5,181,103 triples), warm ms

| query | rows | snapshot base | snapshot new | Stream base | Stream new | Materialize base | Materialize new | RocksDB base | RocksDB new | Stream new, `GSTORE_VS_POLICY=tree` | Stream / snapshot (new) | Stream / Materialize (new) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Q1 | 3 | 0.094 | 0.013 | 5.443 | 0.017 | 4.130 | 0.014 | 7.602 | 0.038 | 4.622 | 1.39 | 1.28 |
| Q2 | 115 | 37.48 | 34.26 | 76.99 | 55.86 | 46.05 | 27.13 | 508.1 | 498.0 | 65.20 | 1.63 | 2.06 |
| Q3 | 10 | 0.383 | 0.015 | 6.365 | 0.016 | 5.467 | 0.013 | 24.36 | 0.066 | 3.927 | 1.05 | 1.25 |
| Q4 | 9 | 0.084 | 0.051 | 2.179 | 0.071 | 2.164 | 0.050 | 1.269 | 0.255 | 2.041 | 1.41 | 1.43 |
| Q5 | 576 | 274.0 | 258.7 | 321.2 | 296.2 | 271.3 | 224.2 | 366.0 | 376.5 | 353.4 | 1.15 | 1.32 |
| Q6 | 404,399 | 123.2 | 79.82 | 238.1 | 191.2 | 152.9 | 92.74 | 234.5 | 223.1 | 247.6 | 2.39 | **2.06** |
| Q7 | 23 | 339.1 | 302.6 | 438.7 | 298.7 | 382.4 | 250.4 | 413.7 | 440.0 | 384.9 | 0.99 | 1.19 |
| Q8 | 10,778 | 317.3 | 243.8 | 377.3 | 272.0 | 341.2 | 241.9 | 444.3 | 392.3 | 383.4 | 1.12 | 1.12 |
| Q9 | 5,507 | 390.9 | 344.6 | 643.0 | 456.9 | 474.0 | 338.8 | 716.0 | 691.9 | 657.1 | 1.33 | 1.35 |
| Q10 | 3 | 269.2 | 252.5 | 345.2 | 256.5 | 277.6 | 206.7 | 400.8 | 423.6 | 312.7 | 1.02 | 1.24 |
| Q11 | 325 | 1.407 | 1.409 | 1.271 | 0.282 | 0.937 | 0.171 | 1.867 | 1.966 | 1.145 | 0.20 | 1.64 |
| Q12 | 21 | 0.495 | 0.519 | 0.683 | 0.158 | 0.513 | 0.079 | 0.275 | 0.227 | 0.700 | 0.31 | 2.02 |
| Q13 | 432,512 | 151.5 | 96.47 | 320.3 | 223.7 | 219.2 | 104.9 | 249.7 | 248.8 | 340.6 | 2.32 | **2.13** |
| Q14 | 305,503 | 85.57 | 63.84 | 176.0 | 147.4 | 106.1 | 67.82 | 184.1 | 178.8 | 180.2 | 2.31 | **2.17** |
| geomean | | 18.06 | 10.48 | 50.97 | 11.66 | 38.86 | 7.55 | 62.13 | 24.44 | 47.70 | **1.11** | **1.54** |

New / base geomean: snapshot 0.58, Stream 0.23, Materialize 0.19, RocksDB
0.39. Pre-Phase-6 disk geomean 146.92 ms: Stream 0.079×, Materialize
0.051×. Run b (queries only): Stream 11.03, Materialize 7.69, snapshot 9.84
ms — Stream / snapshot **1.12** (worst Q14 2.47, Q6 2.40, Q13 2.24),
Materialize / snapshot 0.78, Stream / Materialize **1.43** (Q6 2.24, Q13
2.16, Q14 2.02, Q2 1.78, Q12 1.80). The control — the new binary with
`GSTORE_VS_POLICY=tree` — gives the old disk numbers back (geomean 47.70
against base Stream 50.97 ms; Q1 4.62, Q3 3.93, Q4 2.04 ms): the VS-tree
policy is what removed the `vstree.kv` walks.

#### Univ 8 (1,029,275 triples), warm ms

| query | rows | snapshot base | snapshot new | Stream base | Stream new | Materialize base | Materialize new | RocksDB base | RocksDB new | Stream / snapshot (new) | Stream / Materialize (new) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Q1 | 3 | 0.029 | 0.013 | 0.404 | 0.019 | 0.361 | 0.013 | 1.609 | 0.034 | 1.44 | 1.48 |
| Q2 | 25 | 6.441 | 6.183 | 11.39 | 6.960 | 8.509 | 4.409 | 7.440 | 5.169 | 1.13 | 1.58 |
| Q3 | 10 | 0.063 | 0.014 | 0.485 | 0.018 | 0.400 | 0.013 | 4.826 | 0.051 | 1.31 | 1.34 |
| Q4 | 9 | 0.056 | 0.050 | 0.215 | 0.075 | 0.165 | 0.051 | 0.439 | 0.162 | 1.50 | 1.48 |
| Q5 | 576 | 30.32 | 22.24 | 41.25 | 23.77 | 38.85 | 23.99 | 44.29 | 46.22 | 1.07 | 0.99 |
| Q6 | 80,056 | 20.65 | 14.51 | 44.05 | 32.37 | 29.11 | 16.58 | 35.41 | 34.38 | 2.23 | 1.95 |
| Q7 | 23 | 37.95 | 31.09 | 59.00 | 30.54 | 59.57 | 28.28 | 49.78 | 53.26 | 0.98 | 1.08 |
| Q8 | 10,778 | 39.82 | 33.41 | 69.63 | 44.91 | 52.16 | 30.82 | 89.26 | 100.3 | 1.34 | 1.46 |
| Q9 | 1,081 | 53.67 | 42.85 | 100.4 | 64.82 | 71.37 | 45.29 | 103.0 | 108.6 | 1.51 | 1.43 |
| Q10 | 3 | 30.98 | 21.24 | 43.42 | 27.56 | 38.91 | 21.95 | 43.90 | 43.73 | 1.30 | 1.26 |
| Q11 | 325 | 0.234 | 0.235 | 0.395 | 0.187 | 0.310 | 0.128 | 0.948 | 0.999 | 0.79 | 1.47 |
| Q12 | 21 | 0.099 | 0.101 | 0.141 | 0.090 | 0.102 | 0.047 | 0.162 | 0.165 | 0.89 | 1.92 |
| Q13 | 85,670 | 25.25 | 17.48 | 53.95 | 34.21 | 41.23 | 18.35 | 41.99 | 40.38 | 1.96 | 1.86 |
| Q14 | 60,576 | 15.63 | 10.51 | 30.00 | 22.95 | 20.30 | 12.00 | 28.93 | 28.42 | 2.18 | 1.91 |
| geomean | | 3.15 | 2.23 | 7.37 | 2.98 | 5.83 | 2.01 | 10.29 | 5.22 | **1.34** | 1.49 |

New / base geomean: snapshot 0.71, Stream 0.40, Materialize 0.34, RocksDB
0.51; Stream / pre-Phase-6 disk (18.99 ms) 0.157×.

#### Open, memory and updates (Stream / Materialize, new and base)

| metric | univ 38 Stream new | univ 38 Materialize new | univ 38 Stream base | univ 8 Stream new | univ 8 Materialize new | univ 8 Stream base |
|---|---:|---:|---:|---:|---:|---:|
| open first, page cache dropped (cold) / warm ms | **5.2 / 1.11** (warm samples 0.90, 1.14, 1.11; run b 0.85) | 1392.6 / 1365.8 | 6.3 / 0.83 | 15.2 / 1.07 | 331.1 / 249.6 | 5.5 / 0.86 |
| RSS after open MiB | **6.1** | 351.1 | 5.7 | 6.0 | 89.1 | 5.7 |
| query high-water mark MiB (5 processes) | **207.4** (run b 207.6) | 469.0 | 210.6 | 65.7 | 115.7 | 69.1 |
| single INSERT DATA median / p95 ms (100) | 2.155 / 3.547 | 1.778 / 2.987 | 1.452 / 2.438 | 1.628 / 2.315 | 1.680 / 2.261 | 1.826 / 3.645 |
| single DELETE DATA median / p95 ms | 1.842 / 2.874 | 2.107 / 2.940 | 1.268 / 1.727 | 2.001 / 3.156 | 2.090 / 2.958 | 1.405 / 2.666 |
| chunk insert / delete ms | 1070 / 1105 | 930 / 987 | 879 / 930 | 541 / 690 | 702 / 695 | 695 / 706 |
| `gbuild --disk` s | 20.06 | 18.80 | 18.98 | 4.29 | 4.10 | 3.84 |

Base Materialize: univ 38 open 1603.1 / 1503.5 ms, RSS 350.6 MiB, single
insert 1.654 ms; univ 8 open 365.1 / 296.5 ms. After the update round trip
the new databases are back at their fresh term counts (univ 38 842,657
entities / 433,389 literals; the base's grew to 843,783 / 434,033):
R4-RECLAIM, part 1 of this stage.

**Single-triple INSERT DATA, more samples** (univ 38, new, three
alternating pairs of 500, Materialize first): Materialize 1.866 / 1.384 /
1.835 ms, Stream 1.717 / 1.783 / 1.761 ms — mean of the medians **1.754
against 1.695 ms (+3.5 %)**, pair by pair −8.0 %, +28.8 %, −4.0 %; with the
100-insert reference pair (+21 %) Stream is at or below Materialize in two
of four pairs. Fsync-bound (calibration fsync 1.0 ms) and inside the
Materialize spread (1.38–1.87 ms); R3's quiet stage had −3.8 %. Not a
regression of Stream inserts: two more alternating base / new Stream pairs
of 500 (07:12–07:15 UTC; base, new, new, base) give base 1.821 / 1.969 ms
against new 1.641 / 1.753 ms (−10 %); single deletes 2.07 / 2.18 against
2.68 / 2.35 ms (R4-RECLAIM's immediate frees, part 1 of this stage).

#### RocksDB against the base, repeated

Univ 8, four pairs (min over the four runs, new / base): every query ≤
1.036 (Q7; Q12 1.030 on 0.16 ms, Q8 1.001, the others ≤ 1.004 or faster;
medians of the runs ≤ 1.043, Q5) — the first pair's Q8 +12 % (100.3 ms
against 88.0–93.2 in the other new runs) was one slow process set.
Univ 38, three pairs: min-of-runs Q11 1.097 (1.95–2.31 against 1.78–1.97
ms), Q10 1.062, Q5 1.029, Q7 1.025, the others ≤ 0.992; medians Q7 1.064,
Q11 1.053, Q2 1.025, the others ≤ 1.009. The spread between runs of one
binary is as large (base Q10 353.7 / 400.8 / 458.4 ms, Q8 400.1–479.4;
new Q2 498.0–592.5), so the deterministic counts decide (below): the
query work is unchanged, these are not a regression.

#### Profiles (callgrind, deterministic)

- **Why Stream misses R3-STREAM's ratios to Materialize.** Univ 8 database
  built by the new `gbuild --disk`, the new `gconsole --local` running the
  query once and three times (differing comment lines, so no query-cache
  hits), counted inside `Evaluator::evaluate`. Warm run (the three-run
  count minus the one-run count, halved), Stream / Materialize: Q14 141.9M
  / 121.0M instructions (1.17×), Q6 186.8M / 160.3M (1.17×), Q13 205.3M /
  172.4M (1.19×) — against timings of 1.91×, 1.95×, 1.86× at univ 8. With
  `--cache-sim=yes`, Q14's warm run has 903K against 509K L1 data read
  misses (1.77×): Stream resolves its 60,576 result ids through the
  dictionary's `TermCache` (a hash lookup per id, one `Arc<str>` each),
  where Materialize indexes the in-memory interner in id order. A first run
  also reads the terms from `id2*` (`DiskDict::resolve_sorted` →
  `IvArray::for_each_key`: 74.0M of Q14's 210.3M, about 1,220 instructions
  per term) and the posting list from the B+ tree (9.1M). Q2's Stream cost
  is the `o_by_sp` probes (39.8M of 102.5M; Materialize's whole evaluate is
  42.4M) — Request 2. None of that code changed in this round: Stream got
  faster on every query (0.23× geomean); the ratio grew because the
  removed `vstree.kv` walk was a cost both modes paid alike (Q14 −28.6 ms
  Stream, −38.3 ms Materialize at univ 38; `tree` puts it back, 180.2 ms),
  so what remains of Stream is its out-of-core dictionary and posting
  lists. A target out of reach for this package, not a regression.
- **RocksDB Q7 / Q10 / Q11 at univ 38** (database built by the base
  `gbuild --rocksdb`, both `gquery`/`gconsole` binaries on it,
  `--cache-sim=yes --toggle-collect=…Evaluator…::evaluate`). First run:
  Q7 1,858.0M → 1,951.7M (+5.0 %), Q10 1,519.2M → 1,608.8M (+5.9 %), Q11
  19.30M → 19.19M; all of the Q7 / Q10 difference is glibc's
  `malloc_consolidate` + `unlink_chunk` inside a `free` (Q7 19.9M → 113.8M,
  Q10 0.17M → 94.0M; the L1 read misses +3.1M / +3.0M are the same
  functions'): the heap the open leaves (R4-RECLAIM's `from_slots`
  dictionary load replaced per-term interning) puts the fastbin
  consolidation into the first query instead of a later one. **Warm runs**
  (three runs minus one, halved): Q10 1,591.1M → 1,588.6M (−0.16 %; L1
  read misses 7.44M → 7.37M; both binaries consolidate ~94M per warm run),
  Q11 18.15M → 18.06M (−0.5 %; L1 read misses 12.4K → 12.9K). Univ 8,
  first run: Q5 323.72M → 323.79M, Q7 389.49M → 389.17M, Q10 319.99M →
  320.09M, Q11 8.74M → 8.63M; L1 read misses within ±0.3 % (Q11 9,659 →
  9,278). The query work is
  unchanged; the timing deltas above are run-to-run noise.

#### Acceptance and gates, quiet (univ 38 unless stated)

| item | target | measured | verdict |
|---|---|---|---|
| G1d Stream geomean / snapshot of the same commit | ≤ 1.5× | 11.66 / 10.48 = **1.11×** (run b 1.12×; univ 8 1.34×; base 2.82×) | **met** |
| G1d no query > 3× memory (Stream) | ≤ 3× | worst Q6 2.39×, Q13 2.32×, Q14 2.31× (run b Q14 2.47×; base Q1 58×, Q4 26×, Q3 17×) | **met** |
| G1d Stream geomean / pre disk | ≤ 0.6× | 11.66 / 146.92 = **0.079×** (univ 8 0.157×) | **met** |
| G1d on Materialize | ≤ 1.5×, ≤ 3×, ≤ 0.6× pre | 0.72× (run b 0.78×), worst Q6 1.16×; 0.051× pre | **met** |
| Q2 at or below the base's disk numbers | ≤ base | Stream 76.99 → 55.86 ms, Materialize 46.05 → 27.13 ms (univ 8 11.39 → 6.96, 8.51 → 4.41) | **met** |
| G1b open warm / cold (Stream) | ≤ 0.5 s / ≤ 2.0 s | 1.11 ms / 5.2 ms (`--drop-caches`) | **met** |
| G1b open univ 38 / univ 8 | ≤ 1.5 | warm 1.04, cold 0.34 (15.2 ms at univ 8) | **met** |
| G1c RSS after open; ratio univ 38 / univ 8 | ≤ 120 MiB; ≤ 1.5 | 6.1 MiB; 1.02 | **met** |
| G1c VmHWM through Q1–Q14 | ≤ 400 MiB | 207.4 MiB (Materialize 469.0) | **met** |
| R3-STREAM: each Q ≤ 2.0× Materialize, same directory | ≤ 2.0× | **Q14 2.17×, Q13 2.13×, Q6 2.06×, Q2 2.06×, Q12 2.02×** (run b Q6 2.24×, Q13 2.16×, Q14 2.02×) | **not met** |
| R3-STREAM: geomean vs Materialize | ≤ 1.4× | **1.54×** (run b 1.43×; univ 8 1.49×; base 1.31×) | **not met** |
| R3-STREAM: Q1, Q3, Q4, Q11, Q12 | ≤ Materialize + 5 ms | +0.004, +0.003, +0.022, +0.110, +0.080 ms | **met** |
| R3-STREAM: single INSERT DATA median | ≤ Materialize | 1.754 vs 1.695 ms (3 × 500, +3.5 %); 2.155 vs 1.778 (100) | **not met** (inside the spread; 2 of 4 pairs at or below) |
| R3-STREAM / G6: identical answers | identical | rows and row hashes equal in all 36 files and to pre | **met** |
| Snapshot Q1–Q14 within ±5 % of the base | ±5 % | geomean 0.58× (univ 8 0.71×); worst Q12 +4.8 % (0.495 → 0.519 ms; univ 8 +1.8 %), Q11 +0.1 %, all others faster | **met** |
| RocksDB Q1–Q14 within ±5 % of the base | ±5 % | geomean 0.39× (univ 8 0.51×); univ 8 every query ≤ +3.6 % (min of 4 runs); univ 38 Q11 +9.7 %, Q10 +6.2 % (min of 3), Q7 +6.4 % (median), the others ≤ +2.9 %; warm-run instructions and cache misses equal (Q10 −0.16 %, Q11 −0.5 %) | **met** by the counts; the univ 38 timing deltas are within the run-to-run spread |
| G2 Q1 / Q3 / Q4 / Q11 / Q12 univ 38 / univ 8 (snapshot) | ≤ 2.0 | 0.96 / 1.08 / 1.01 / **5.99 / 5.14** (base 3.20 / 6.11 / 1.51 / 6.02 / 4.99) | Q1/Q3/Q4 met; Q11/Q12 **not met** (unchanged; Request 4) |
| G2 ratios, other layouts (new) | (report) | Stream 0.93 / 0.87 / 0.95 / 1.51 / 1.76; Materialize 1.08 / 0.93 / 0.98 / 1.34 / 1.67; RocksDB 1.12 / 1.29 / 1.58 / 1.97 / 1.37 | report |
| RocksDB Q2 scaling univ 38 / univ 8 | (report) | 498.0 / 5.17 = 96× (base 508.1 / 7.44 = 68×) | not improved (Request 6) |

#### Default disk query mode: Materialize stays the default

G1b, G1c and G1d pass at univ 38, on Stream and on Materialize, but three
R3-STREAM acceptance items do not: each query ≤ 2.0× Materialize (Q14
2.17× / 2.02×, Q13 2.13× / 2.16×, Q6 2.06× / 2.24× in the two samples, also
Q2 2.06× and Q12 2.02× in the first), the geomean ≤ 1.4× (1.54× / 1.43×),
and the single-insert
median ≤ Materialize (+3.5 %, inside the spread). The flip needs every one
of them (plan §6 R3-STREAM, Request 3), so `DEFAULT_DISK_QUERY_MODE` stays
`Materialize` and nothing in the code changed at this stage (plan §5.7:
recorded here, R4-CLOSURE reports it). What blocks it is the gap between
Stream's out-of-core dictionary and Materialize's in-memory interner on
results of 300–430K rows (above), which this round made the larger share
by removing the VS-tree walk from both. Follow-ups: cheaper Stream term
resolution for large results (a dense id-indexed decoded-term cache, or
batched `id2*` reads without per-term allocation, in
`src/kvstore/disk_dict.rs` / `src/dict`), Request 2 (`seek_in_leaf` in
`SpoWalk`) for Q2, Request 1 (range-count cache) for the optimizer's
counts; then re-measure the flip (Stream is already faster than the
snapshot layout on Q7, Q11 and Q12, within 1.5× of it on ten of the 14
queries, and opens in about 1 ms with 6 MiB).

### Implementer run (noisy)

**Implementer run, noisy**: 4 vCPU, another implementer compiling and
benchmarking at the same time (load average 1.0–8.5 during the runs);
every heavy command under `flock /tmp/gstore-bench.lock`. Release binaries
of this branch (`cargo build --release -p gstore --bins -p gstore-bench`,
code of `aca4eef`; RocksDB: `CARGO_TARGET_DIR=/tmp/p6r4-target-rocks`,
`--features gstore/rocksdb,gstore-bench/rocksdb`, code of `5a627d9`)
against the base binaries `/tmp/gstore-base-d702463/bin{,-rocks}`, same
session, base and branch alternating:
`<bin>/gstore-bench run --profile reference --scale {8,38} --backends
{snapshot|disk|rocksdb} --no-conc --no-update --bin-dir <bin>
[--env GSTORE_DISK_QUERY_MODE=stream|memory] --out …` with
`GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache` (each run bulk-builds its
own database with that side's `gbuild`, deleted afterwards). Warm ms =
the minimum over 5 processes of the median of 5 runs. **Rows and row
hashes of Q1–Q14 are identical across the 8 result files of each scale**
(snapshot, Stream, Materialize and RocksDB, base and branch).

Univ 38 (5,181,103 triples):

| query | rows | snapshot base | snapshot branch | Stream base | Stream branch | Materialize base | Materialize branch | Stream / snapshot (branch) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Q1 | 3 | 0.151 | 0.018 | 9.456 | 0.025 | 9.175 | 0.019 | 1.37 |
| Q2 | 115 | 49.9 | 48.6 | 100.3 | 75.9 | 69.3 | 42.3 | 1.56 |
| Q3 | 10 | 0.450 | 0.019 | 11.3 | 0.025 | 9.485 | 0.019 | 1.32 |
| Q4 | 9 | 0.116 | 0.074 | 4.261 | 0.102 | 3.894 | 0.071 | 1.39 |
| Q5 | 576 | 490.8 | 448.1 | 590.8 | 495.8 | 597.8 | 507.4 | 1.11 |
| Q6 | 404,399 | 141.4 | 104.8 | 328.4 | 238.7 | 210.6 | 113.3 | 2.28 |
| Q7 | 23 | 564.2 | 540.2 | 748.3 | 557.6 | 776.2 | 574.0 | 1.03 |
| Q8 | 10,778 | 503.3 | 487.4 | 636.4 | 500.5 | 627.1 | 499.7 | 1.03 |
| Q9 | 5,507 | 634.4 | 618.0 | 1049.3 | 803.6 | 836.1 | 684.5 | 1.30 |
| Q10 | 3 | 487.6 | 443.8 | 597.9 | 504.7 | 614.8 | 516.7 | 1.14 |
| Q11 | 325 | 1.366 | 1.270 | 1.544 | 0.461 | 1.210 | 0.275 | 0.36 |
| Q12 | 21 | 0.559 | 0.528 | 0.868 | 0.213 | 0.690 | 0.131 | 0.40 |
| Q13 | 432,512 | 173.3 | 131.8 | 443.0 | 282.0 | 293.4 | 142.2 | 2.14 |
| Q14 | 305,503 | 105.3 | 78.8 | 237.2 | 182.3 | 141.9 | 83.4 | 2.31 |
| geomean | | 24.81 | 15.01 | 79.04 | 17.83 | 65.32 | 12.67 | **1.19** |

Univ 8 (1,029,275 triples), geomeans: snapshot 4.857 → 3.675 ms, Stream
11.584 → 4.863, Materialize 9.208 → 3.069; Stream / snapshot (branch)
1.32, worst query Q14 2.00 (Q1 1.35, Q3 1.34, Q4 1.50, Q6 1.96, Q13 1.86).
Per query, snapshot base → branch: Q1 0.040 → 0.018, Q2 8.84 → 8.95,
Q3 0.087 → 0.019, Q4 0.079 → 0.071, Q5 61.9 → 52.9, Q6 28.8 → 20.9,
Q7 74.1 → 58.5, Q8 75.9 → 66.4, Q9 86.6 → 78.4, Q10 59.9 → 49.2,
Q11 0.346 → 0.364, Q12 0.146 → 0.160, Q13 32.4 → 25.5, Q14 19.6 → 15.0.

RocksDB (opens without a VS-tree, so only the engine changes apply),
geomean univ 8 17.02 → 8.39 ms, univ 38 95.5 → 37.2 ms; univ 38 per query
base → branch: Q1 12.8 → 0.049, Q2 726.7 → 776.2, Q3 39.4 → 0.097,
Q4 1.95 → 0.33, Q5 641 → 666, Q6 297 → 297, Q7 728 → 782, Q8 753 → 740,
Q9 1157 → 1181, Q10 648 → 662, Q11 2.65 → 2.99, Q12 0.39 → 0.34,
Q13 357 → 344, Q14 256 → 261.

**Instruction counts** (deterministic; callgrind of one `gquery` run,
counted inside `Evaluator::evaluate` with `--toggle-collect`, univ 8
databases built by the base `gbuild`; branch = `ee3abdc`), base → branch:

| query | snapshot | Stream | RocksDB |
|---|---:|---:|---:|
| Q1 | 327,208 → 39,168 | 10,510,573 → 238,311 | 21,900,359 → 268,831 |
| Q2 | 47.53M → 45.41M | 101.56M → 102.42M (first run, lists read cold) | 56.63M → 56.44M |
| Q3 | 974,865 → 66,867 | 17.00M → 0.29M | 69.89M → 0.53M |
| Q4 | 493,637 → 392,226 | 4.08M → 0.94M | 4.30M → 1.47M |
| Q5 | 180.3M → 145.7M | 243.1M → 158.9M | 323.7M → 323.4M |
| Q7 | 241.3M → 196.3M | 357.4M → 238.8M | 389.5M → 389.0M |
| Q9 | 292.8M → 256.5M | 639.2M → 542.6M | — |
| Q10 | 178.0M → 143.5M | 241.5M → 155.7M | 320.0M → 319.7M |
| Q11 | 1,921,871 → 1,916,600 | 5.66M → 3.70M | 8.73M → 8.64M |
| Q12 | 829,852 → 836,483 (+0.8%) | 2.80M → 2.64M | 1.97M → 1.97M |
| Q14 | 148.9M → 123.7M | 259.7M → 210.8M | 257.5M → 257.1M |

(Q6, Q8, Q13 likewise lower on every layout.) The RocksDB timings above
that are slower than the base (Q2 +6.8%, Q7 +7.5%, Q11 +13% at univ 38)
are not borne out by the instruction counts of the final code (all at or
below the base); one cause was real and is fixed in `ee3abdc`: the
candidate lookups replaced a 156-entry `rdf:type ub:Department` list by
156 RocksDB point reads in Q2 (+2.3% instructions before the cap of 64
lookups). The RocksDB timing runs predate that fix; the quiet stage should
re-measure.

**Why the base fails G1d (profiling, univ 38, Stream).** Every LUBM query
with a subject variable walked `vstree.kv` (it cannot probe): Q1 read
~3,800 pages through `DiskTree`'s 1,024-page cache. A scratch driver
(opens the database once, runs each query 1 + 5 times per policy through
`scoped_vs_policy`, compares rows in order) on univ 38 with the code of
`75217ca`, Stream: `Tree` / `Auto` Q1 9.36 / 0.026 ms, Q3 8.61 / 0.039,
Q4 4.12 / 0.109, Q2 106.6 / 79.5, geomean 78.2 / 19.2 ms; snapshot
geomean `Tree` 18.7, `Auto` 16.1 — identical rows in order under both
policies everywhere. `Off` (no tree)
changes LUBM Q2's plan at univ 38 (rows equal, order not) and runs it 4×
slower (375 vs 90 ms): the tree's candidate sets do matter for plans, so
the policy keeps them and only changes how they are computed. Choosing
lists vs lookups at 16 / 64 / 256 / 100,000 list entries per lookup
(univ 38, Stream and Materialize): 64 is best on both (geomean of the
affected queries 16.6 / 0.71 ms against 21.2 / 1.16 at 16 and 31.1 / 2.20
when always reading lists). A long range count on disk costs ~0.5 ns per
entry (`count_s_by_po(rdf:type, ub:Publication)` = 312,726 entries in
0.156 ms, against 0.3 µs in memory), which is why candidate generation
only counts lists estimated short and constant stars skip the plan search.

**Gates and acceptance (this run, noisy):**

| item | target | measured | verdict |
|---|---|---|---|
| G1d Stream geomean / snapshot, univ 38 | ≤ 1.5× | 17.83 / 15.01 = **1.19×** (base: 79.04 / 24.81 = 3.19×) | met here; **left to the quiet stage** |
| G1d no query > 3× memory, univ 38 | ≤ 3× | worst Q14 2.31×, Q6 2.28×, Q13 2.14× (base: Q3 25×, Q1 63×) | met here; quiet stage |
| G1d Stream geomean / pre disk, univ 38 | ≤ 0.6× | 17.83 / 146.92 (pre, R3 fragment) = 0.12× | met |
| G1d on Materialize mode | (both modes) | 12.67 / 15.01 = 0.84×; worst Q5 1.13× | met here |
| Q2 at or below today's disk numbers | ≤ base | Stream 100.3 → 75.9 ms, Materialize 69.3 → 42.3 ms (univ 38) | met |
| Snapshot Q1–Q14 within ±5% of base | ±5% | geomean 0.60×; every query faster or within noise; instructions lower on every query but Q12 (+0.8%) | met (improved) |
| RocksDB Q1–Q14 within ±5% of base | ±5% | geomean 0.39×; instructions at or below the base on every query; three timings +7–13% before `ee3abdc` (see above) | met by instruction counts; timings **left to the quiet stage** |
| Identical answers and order | identical | row hashes equal in all 16 files; order vs `Tree` tested | met |
| G2 Q1/Q3/Q4/Q11/Q12 univ 38 / univ 8 (snapshot) | ≤ 2.0 | Q1 1.00, Q3 1.00, Q4 1.04 (base 3.78 / 5.17 / 1.47); Q11 3.49, Q12 3.30 (base 3.95 / 3.83) | Q1/Q3/Q4 now met; Q11/Q12 not |
| RocksDB Q2 scaling univ 38 / univ 8 | (report) | 776.2 / 8.19 = 95× (base 726.7 / 9.25 = 79×) | not improved |

**For the quiet measure stage** (from the repository root of the merged
tree; base binaries as in this run):

```sh
cargo build --release -p gstore --bins -p gstore-bench
CARGO_TARGET_DIR=target-rocks cargo build --release -p gstore -p gstore-bench --bins \
    --features gstore/rocksdb,gstore-bench/rocksdb
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache
for s in 8 38; do
  for side in base:/tmp/gstore-base-d702463/bin new:target/release; do
    tag=${side%%:*}; bin=${side#*:}
    flock /tmp/gstore-bench.lock $bin/gstore-bench run --profile reference --scale $s \
        --backends snapshot --no-conc --bin-dir $bin --out /tmp/r4vs-u$s-$tag-snapshot.json
    for mode in stream memory; do
      flock /tmp/gstore-bench.lock $bin/gstore-bench run --profile reference --scale $s \
          --backends disk --no-conc --env GSTORE_DISK_QUERY_MODE=$mode --bin-dir $bin \
          --out /tmp/r4vs-u$s-$tag-$mode.json
    done
  done
  for side in base:/tmp/gstore-base-d702463/bin-rocks new:target-rocks/release; do
    tag=${side%%:*}; bin=${side#*:}
    flock /tmp/gstore-bench.lock $bin/gstore-bench run --profile reference --scale $s \
        --backends rocksdb --no-conc --bin-dir $bin --out /tmp/r4vs-u$s-$tag-rocksdb.json
  done
done
for f in snapshot stream memory rocksdb; do for s in 8 38; do
  target/release/gstore-bench compare /tmp/r4vs-u$s-base-$f.json /tmp/r4vs-u$s-new-$f.json --markdown
done; done
```

G1d: Stream / snapshot geomean and per query from
`/tmp/r4vs-u38-new-stream.json` against `/tmp/r4vs-u38-new-snapshot.json`
(and Materialize from `…-new-memory.json`); vs pre disk against
`bench/results/pre-phase6-lubm38.json`. With the policy forced back
(`--env GSTORE_VS_POLICY=tree`) the same binary gives the old disk numbers,
an in-run control.

### Verification

At `f684af7` (the code of the final commit; later commits touch this
fragment only): `cargo fmt --all -- --check` clean; `cargo build
--workspace --all-targets` and `cargo clippy --workspace --all-targets --
-D warnings` clean; `GSTORE_DISK_QUERY_MODE=memory cargo test --workspace`:
2,255 passed, 0 failed, 2 ignored; `GSTORE_DISK_QUERY_MODE=stream cargo
test --workspace --no-fail-fast`: 2,255 passed, 0 failed, 2 ignored (a
first stream pass stopped at `dt_bulk_crash::a_rocksdb_store_in_use_is_not_replaced`,
"rocksdb/LOCK is held by process <the test's own pid>" after the holder
was dropped; it passed in the memory pass, in three reruns of that file in
Stream mode and in the full rerun — a flake of that test, which no code of
this package reaches); with `CARGO_TARGET_DIR=/tmp/p6r4-target-rocks`:
`cargo clippy -p gstore --all-targets --features rocksdb -- -D warnings`
clean, `cargo test -p gstore --features rocksdb`: 2,102 passed, 0 failed,
2 ignored. No feature-gated code and no bench code changed. Two existing
lib tests (`db::tests::disk_database_filters_via_on_disk_vstree`,
`streaming_vstree_matches_in_memory_path`) assert that a query paged
`vstree.kv`: they pass because the library's unit tests default to `Tree`
(see Changelog and Requests 5); no existing test was changed.

A trial merge of `p6/r4-reclaim` (`f83ece6`) and then `p6/r4-snap`
(`a011b07`) onto `f684af7` (a throwaway worktree) merges without conflicts;
on the merged tree `cargo test -p gstore --lib -- signature:: query::
kvstore::bptree db::tests` (255 tests, among them
`a_vstree_file_with_feature_bits_is_refused` with R4-RECLAIM's
`SUPPORTED_FEATURES = FEATURE_FREE_IDS`, where the pager itself accepts
the bit) and `--test dt_vs_policy` pass.

## Requests/Not yet

1. **`DiskTriples` owner (`src/kvstore/disk_triples.rs`; R4-RECLAIM this
   round)**: answer `count_s_by_po` / `count_o_by_sp` / `count_ps_by_o` /
   `count_po_by_s` of long ranges from a per-epoch count cache (like the
   long-list cache; only without an overlay). A range count on disk reads
   one leaf per ~250 entries (`?x rdf:type ub:Publication`, 312,726
   entries: 0.156 ms at univ 38, 0.3 µs in memory); the engine now avoids
   it for constant stars and short-estimated lists, but the optimizer
   still counts every pattern of other BGPs once per query.
2. **`src/kvstore/store.rs` owner** (R3-STREAM request 6): `SpoWalk::collect`
   can call the new `LeafCursor::seek_in_leaf(key)` (binary search in the
   rest of the current leaf) before stepping up to 48 entries one by one;
   the remaining Stream overhead of Q2 (1.56× the snapshot layout at univ
   38) is mostly those `o_by_sp` probes.
3. **R4-CLOSURE**: report G1d from the quiet numbers; if G1b/G1c/G1d and
   every R3-STREAM item pass, the measure stage flips
   `DEFAULT_DISK_QUERY_MODE` (`src/db/open_options.rs`, not this
   package's file). Document `GSTORE_VS_POLICY` (environment only: there is
   no `conf.ini` key, `src/config.rs` is not this package's file — a
   `[query] vs_policy` key would be a later addition).
4. **In-memory trees (snapshot layout)**: `Auto` still probes / walks them
   like `Tree`. Q11 and Q12 are faster on Stream (store checks: 0.46 /
   0.21 ms) than on the snapshot layout (probes: 1.27 / 0.53 ms at univ
   38) and keep G2's Q11/Q12 ratios above 2.0. Store checks there would
   change the candidate sets of a snapshot database after deletes (its
   saved tree keeps stale bits), hence possibly the order of solutions
   without `ORDER BY` relative to the base — not done; a `Database`-side
   signal that the tree is exact (`vs_shrink_debt() == 0`, `src/db`) would
   make it safe.
5. **`src/db/mod.rs` owner (R4-RECLAIM this round)**: the tests
   `db::tests::disk_database_filters_via_on_disk_vstree` and
   `streaming_vstree_matches_in_memory_path` assert
   `db.vstree.disk_pages_read() > 0` after their queries — true only under
   `VsPolicy::Tree`. Run those queries inside
   `let _g = crate::query::scoped_vs_policy(VsPolicy::Tree);` (they test
   the out-of-core tree's walk) and assert the same answers once more under
   `Auto` with `disk_pages_read()` unchanged; then `BUILT_IN_DEFAULT` in
   `src/query/vs_policy.rs` can drop its `cfg(test)` case (`Tree` for the
   library's unit tests).
6. Not yet: RocksDB Q2's scaling (×95 from univ 8 to 38) is unchanged — it
   opens without a VS-tree and its Q2 join is dominated by `s_by_po` reads.
