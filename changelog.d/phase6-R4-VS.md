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
constant-star plans; implementer run (noisy) at univ 38: Stream Q1–Q14
geomean 17.8 ms = 1.19× the snapshot layout of the same commit (worst
query Q14 2.31×) and 0.12× pre disk — G1d met in that run, left to the
quiet measure stage; snapshot geomean 0.60× the base, Materialize 0.19×,
RocksDB 0.39×.

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
