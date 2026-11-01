# R2-DICT — out-of-core dictionary API (TermStr), bounded caches, batched materialization

Package R2-DICT of `docs/PHASE6_PLAN.md` §6 (Round 2), contract C4. Base:
`aaea9f8`. One commit per sub-part (1 TermStr + Dictionary API +
copy-on-write interners + hand-written serde; 2 `dict/cache.rs` + DiskDict;
3 call-site migration; 4 engine chunked materialization + memo + injected
string index; 5 the Database's string-index cache), then the `kv_bench`
modes, three follow-ups found while measuring (prefetch probes not counted
as misses; one batch per chunk for out-of-core materialization; a trimmed,
measurable `StringIndex`), and this fragment.

## Changelog

### Phase 6 R2-DICT: the out-of-core dictionary API, a leaner in-memory dictionary

- **Smaller in-memory dictionary (snapshot RSS after open −20% at univ 8).**
  Each dictionary string is stored once, as an `Arc<str>` owned by the
  id → string vector; the string → id side is an open-addressing table of
  ids (8 bytes a slot) that compares through the vector, instead of a
  `HashMap<String, u32>` holding a second copy of every string. A loaded
  dictionary is also sized exactly (no vector growth slack).
- **Copy-on-write dictionary.** The three interners are
  `Arc<InternerData>`: cloning a `Dictionary` (a `ConcurrentDb` snapshot, an
  HTTP transaction's begin-time copy) is O(1), and the first write to a shared
  clone copies only the interner it changes. Interning an existing term,
  look-ups and freeing an absent key never copy.
- **`dict.bin` keeps its format and becomes reproducible.** The same bincode
  layout (forward map, backward vector with holes, free stack), written by a
  hand-written serializer with the forward map in ascending id order: two
  builds of the same data now write identical `dict.bin` bytes (before, the
  map followed a `HashMap`'s random order). Older binaries read the new files
  and the new code reads old ones into equal dictionaries (holes and the free
  stack included). A file whose forward and backward halves disagree is
  refused with an error instead of loading inconsistently.
- **Dictionary API (contract C4)**, `gstore::dict`:
  - `TermStr<'a>` (`Borrowed(&str)` from an in-memory dictionary, `Shared(Arc<str>)`
    from a backend): `Deref<Target = str>`, `AsRef<str>`, `Borrow<str>`,
    `Display`, `Debug`, `Clone`, `Eq`/`Ord`/`Hash` as its text,
    `PartialEq<str / &str / String>`, `into_string`, `into_shared`,
    `into_owned`.
  - `Space { Entity, Literal, Predicate }` (`of_id`, `of_term`, `first_id`).
  - `Dictionary::term` / `pred` (`Option<TermStr>`, a backend error is noted
    in the statement's read scope), `try_term` / `try_pred` /
    `try_resolve` (`Result`), `try_entity_id` / `try_literal_id` /
    `try_predicate_id` / `try_id`, `id_of`, `resolve`, `prefetch(&ids, &pids)`,
    `resolve_sorted` / `resolve_many` (sorted batches), `for_each_term(space,
    from, f)`, `materialize()`, `id_limit(space)`, `cache_stats()`,
    `backing()`, `generation()`, `entries_in(space)` (borrowed, in memory),
    `heap_bytes()`.
  - `DiskTermSource` is now a full fallible `&self`, `Send + Sync` backend:
    `lookup`, `resolve -> Arc<str>`, `resolve_sorted`, `intern` / `free`
    (refusing by default), `prefetch`, `for_each`, `count`, `id_limit`,
    `generation`, `sync`, `cache_stats`.
  - `try_intern_*` on a backed dictionary goes to the backend (a read-only
    backend refuses a new term); before, it silently interned into the empty
    in-memory side.
  - The legacy `id_to_string` / `predicate_to_string -> Option<&str>` are
    unchanged for in-memory dictionaries. On a backed dictionary they pin the
    strings they hand out in a bounded table owned by that dictionary (4096
    strings, freed with it — no `Box::leak` anywhere) and fail closed beyond
    it (`None` plus a read error).
- **Bounded caches** (`gstore::dict::cache`): `TermCache` `(space, id) →
  Arc<str>` (64 MiB by default) and `KeyCache` `(space, key) → id` (16 MiB),
  16 shards each with CLOCK eviction, charged by bytes (string + 72), emptied
  when the backend's generation changes; `CacheStats`.
- **`DiskDict`** (the kvstore backend) implements the new trait over the
  shared pager: `BTree::get_with` / `IvArray::get_with` (no per-look-up
  copies), both directions cached within budget, batches and prefetches read
  in id order run by run (`IvArray::for_each_range`), `for_each`, cache
  counters. Read-only (`intern` answers existing keys only). New
  `DiskDict::open_shared(pager)` and `with_cache_budgets(term, key)`; the
  constructor `DiskStore` uses is unchanged. `DiskStore::query` /
  `lazy_dictionary` no longer leak every string they resolve.
- **Engine.** SELECT results are materialized in chunks of 4096 rows; over an
  out-of-core dictionary each chunk resolves its distinct ids in one sorted
  batch per space. Id → term and id → value are memoized per query (at most
  65,536 ids each) for FILTER, BIND and ORDER BY. The dictionary substring
  index can be injected: `Evaluator::with_string_index(Arc<StringIndex>)` /
  `with_string_index_provider`; `query::engine::dictionary_string_index`
  builds it. Answers, their order and plans are unchanged.
- **The substring index is cached per dictionary generation.**
  `Database::string_index()` builds the trigram `StringIndex` on the first
  query with a `CONTAINS` / `STRSTARTS` / `STRENDS` / plain `REGEX` filter and
  keeps it until a term is added or freed, so a repeated substring query no
  longer rebuilds it (univ 8: 1.7–1.9 s → 11 ms). Reads and UPDATE `WHERE`
  evaluation use it; `cached_string_index()` peeks. In-memory dictionaries
  only; an out-of-core dictionary runs substring filters without the
  prefilter (still exact). Memory: the cached index stays resident (see
  Measured numbers).
- **Call sites.** Every production id → string use goes through `term()` /
  `pred()`: query engine, `db` (schema lists, DESCRIBE triples, redo log),
  `db::persist` (`DiskOp::resolve`, disk reconciliation, RocksDB term writes
  and migration checks), `db::admin` (type statistics, schema graph),
  `zipio` (export), `reason` (subproperty interning, rule matching) and the
  CSR's predicate list (`for_each_term`).
- `StringIndex::heap_bytes()` (an estimate) and `build` trimming the
  posting lists' growth slack.
- `examples/kv_bench.rs`: `--mode dict-ooc` (Q1–Q14 over the on-disk triples
  with the out-of-core dictionary vs the dictionary in memory, resident bytes
  after Q1–Q14 × 3, `--dict-cache-mb`) and `--mode contains` (a CONTAINS query
  repeated on one open database).
- **Tests.** New `tests/dt_dict_backends.rs` (TermStr; mock-backend routing of
  look-ups, resolves, interns and batches; a backend error failing the
  statement through the read scope; the legacy API failing closed; DiskDict
  over a LUBM-1 `kvstore.kv`: every id round-trips, 1M random look-ups stay
  within a 1 MiB budget, Q1–Q14 identical in order to the in-memory
  dictionary with no cache growth over 3 runs; `dict.bin` read both ways and
  reproducible through save/open; no `Box::leak` in `src/dict` /
  `src/kvstore`); dictionary unit tests (old↔new encoding mirrors over seeded
  intern/free histories with holes and free stacks, reproducible bytes,
  refused inconsistent input, the id table against a `HashMap` model,
  copy-on-write isolation, generations, TermStr); cache and DiskDict unit
  tests; `tests/string_index_planner.rs`: the cached index equals a
  per-query build, is reused, kept by writes that add no term and dropped by
  one that does.
- **Changed assertion:** `dict::tests::persisted_encoding_is_unchanged` keeps
  its old-layout mirror structs and assertions; the three lines that copy the
  interner's fields into the mirror now call accessors (the fields became
  `Arc`-shared and no longer exist as `HashMap<String, u32>` /
  `Vec<Option<String>>`).

## GAP_PLAN status line

§8 row 6 (规模与性能): R2-DICT done — C4 dictionary API (`TermStr`, fallible
out-of-core `DiskTermSource`, bounded CLOCK caches, `DiskDict` without leaks),
copy-on-write interners storing each string once (univ 8 snapshot RSS after
open 0.80×), reproducible `dict.bin` (format unchanged), batched result
materialization, substring index cached per dictionary generation; univ 38
gates by the measure stage.

## Docs notes

- `docs/STORAGE_BACKEND.md` / `docs/DESIGN.md` (dictionary): the C4 API
  (`TermStr`, `term` / `pred` and the `try_*` forms, batches, `for_each_term`,
  `generation`); the out-of-core backend contract (`DiskTermSource`, errors
  through the read scope, `intern` / `free` refused by read-only backends);
  the caches (`TermCache` 64 MiB, `KeyCache` 16 MiB by default, CLOCK,
  generation invalidation; `DiskDict::with_cache_budgets`); the in-memory
  layout (one `Arc<str>` per string, id table, copy-on-write clones).
- The legacy `id_to_string` / `predicate_to_string`: in memory as before; on
  an out-of-core dictionary bounded pinning (4096 strings) then fail-closed —
  a documented deviation from C4's "return `None`", kept because
  `kvstore/store.rs`'s `lazy_dictionary_resolves_term_and_id_on_demand` (not
  this package's file) asserts `Some` (see Requests).
- `docs/STORAGE_BACKEND.md` (snapshot layout): `dict.bin` is byte-identical in
  format; its forward map is now written in id order, so it is reproducible
  for the same input. Two pre-Phase-6 builds of the same data wrote different
  bytes; compatibility is both ways (old binaries read new files).
- `docs/DESIGN.md` / README (query engine): the substring index (`CONTAINS`,
  `STRSTARTS`, `STRENDS`, plain `REGEX` candidate pruning) is built once per
  dictionary generation and kept in memory by the database — about 83 MiB
  at univ 8 once a substring query ran (the build's peak is the same as
  before; the index is now retained); it is not used for out-of-core
  dictionaries.

## Measured numbers

**Implementer run, noisy (other agents compiling; load average 2.5–5.9 on
4 vCPUs).** Univ 8 only (lubm-like/1, 1,029,275 triples, data fnv
`ed44da2e7b3bbb98`, from `/tmp/gstore-bench-cache`). Base = the prebuilt
release binaries of `aaea9f8`; branch = release builds of this branch (the
snapshot and `gbuild` numbers from `fe44055`, whose in-memory path is the
final one; the out-of-core numbers from `478581d` and later). Every heavy
command ran under `flock /tmp/gstore-bench.lock`; base and branch alternated
in one session. univ 38 is left to the measure stage.

### Snapshot (gstore-bench `run --profile reference --scale 8 --backends snapshot --no-conc`)

Five rounds per side, alternating (rounds 1–2 with the update phase, 3–5
`--no-update`); RSS and open per round, queries as the minimum / median of
the five rounds' warm medians. The JSON `commit` field shows the
working-tree commit, not the binary's.

| metric | base `aaea9f8` (5 rounds) | branch (5 rounds) | ratio |
|---|---|---|---:|
| RSS after open MiB | 128.6 / 128.8 / 128.7 / 128.5 / 128.0 | 102.8 / 102.6 / 102.3 / 102.8 / 102.7 | **0.80×** |
| open warm ms | 261 / 346 / 274 / 280 / 281 | 220 / 219 / 219 / 235 / 220 | ≈ 0.80× |
| query HWM MiB | 153–155 | 127–130 | 0.83× |
| gbuild peak MiB | 137.9–138.2 | 112.2–112.4 | 0.81× |
| gbuild wall s | 5.66–7.62 | 5.60–7.08 | noise |
| database bytes | 85,480,180 | 85,480,180 | same |
| Q1–Q14 geomean ms (min / median of rounds) | 3.109 / 3.230 | 3.106 / 3.216 | **0.999× / 0.996×** |

| query | rows | base min / median ms | branch min / median ms | ratio min / median |
|---|---:|---:|---:|---:|
| Q1 | 3 | 0.040 / 0.041 | 0.034 / 0.036 | 0.85 / 0.88 |
| Q2 | 25 | 5.84 / 6.07 | 5.94 / 6.26 | 1.02 / 1.03 |
| Q3 | 10 | 0.093 / 0.094 | 0.071 / 0.076 | 0.77 / 0.81 |
| Q4 | 9 | 0.063 / 0.066 | 0.063 / 0.065 | 1.00 / 0.98 |
| Q5 | 576 | 23.6 / 24.8 | 24.4 / 24.7 | 1.03 / 1.00 |
| Q6 | 80,056 | 20.6 / 21.0 | 21.8 / 22.5 | 1.06 / 1.07 |
| Q7 | 23 | 32.0 / 33.7 | 32.2 / 33.9 | 1.00 / 1.01 |
| Q8 | 10,778 | 35.1 / 35.5 | 35.1 / 36.3 | 1.00 / 1.02 |
| Q9 | 1,081 | 44.9 / 47.6 | 45.8 / 46.9 | 1.02 / 0.99 |
| Q10 | 3 | 23.4 / 24.2 | 23.0 / 24.3 | 0.98 / 1.00 |
| Q11 | 325 | 0.233 / 0.243 | 0.242 / 0.243 | 1.04 / 1.00 |
| Q12 | 21 | 0.124 / 0.140 | 0.137 / 0.144 | 1.10 / 1.03 |
| Q13 | 85,670 | 23.2 / 23.7 | 25.0 / 26.0 | 1.08 / 1.10 |
| Q14 | 60,576 | 15.0 / 15.2 | 16.1 / 16.3 | 1.08 / 1.07 |

Rows and row hashes are identical in every round. The three large-result
queries (Q6, Q13, Q14) read 6–10% slower on the branch in these noisy runs;
a further pair of rounds per side with glibc heap trimming disabled
(`--env MALLOC_TRIM_THRESHOLD_=4294967295`) gave 1.05 / 1.06 / 1.05 (min) with
the geomean at 0.978× — for the quiet stage to confirm or clear. Update phase
(report-only, rounds 1–2): single insert median 0.10 / 0.06 → 0.06 / 0.10 ms,
chunk insert 71 / 43 → 42 / 71 ms, checkpoint 370 / 307 → 323 / 333 ms,
reopen 198 / 285 → 156 / 160 ms; round trips ok.

### `dict.bin` compatibility (gbuild at univ 8, twice per side)

| build | `dict.bin` sha256 (16 hex) | bytes |
|---|---|---:|
| base `gbuild` #1 | `f556c35ee78c11cc` | 36,001,855 |
| base `gbuild` #2 | `5126a090d84bdee2` | 36,001,855 |
| branch `gbuild` #1 | `52504768ccdb14ff` | 36,001,855 |
| branch `gbuild` #2 | `52504768ccdb14ff` | 36,001,855 |

`store.bin`, `vstree.bin` and `named.bin` are identical across all four
builds. Cross-binary: the base `gquery` on the branch-built database and the
branch `gquery` on the base-built one answer Q1–Q14 exactly as each binary
on its own database (sorted result files compared with `cmp`, 14/14).

### CONTAINS twice in one process (univ 8 snapshot, `kv_bench --mode contains`, query cache off)

`SELECT ?x ?n WHERE { ?x ub:name ?n . FILTER(CONTAINS(?n, "Course1")) }`,
3,432 rows:

| | first ms | second ms | second / first |
|---|---:|---:|---:|
| base (same harness built at `aaea9f8`) | 1748.5 | 1790.0 | 1.02 |
| branch, 5 runs | 2202 / 2181 / 1859 / 1752 / 1717 | 15.2 / 11.1 / 12.1 / 11.9 / 11.8 | **0.005–0.007** |

RSS: base 130 → 203 MB (the per-query index is freed, the heap stays);
branch 104 → 209 MB with the index kept, whose own estimate
(`StringIndex::heap_bytes`) is 87,377,701 bytes.

### Out-of-core dictionary (`kv_bench`, univ 8 database built by the base `gbuild --disk`)

`--mode dict-ooc` (Q1–Q14 through the evaluator over the on-disk triples;
out-of-core dictionary with the default 64 MiB budget vs the dictionary in
memory; 4096-page store cache):

| | round 1 | round 2 |
|---|---:|---:|
| resident dictionary bytes after Q1–Q14 × 3 | 12,990,961 (97,504 strings) | 12,990,961 |
| evictions | 0 | 0 |
| Q6 out-of-core / in-memory ms | 37.97 / 31.76 (**1.20×**) | 28.77 / 18.82 (**1.53×**) |
| Q13 | 35.39 / 22.55 (**1.57×**) | 36.91 / 42.47 (**0.87×**) |
| Q14 | 23.09 / 14.85 (**1.55×**) | 21.93 / 23.51 (**0.93×**) |
| Q1–Q14 geomean | 4.58 / 3.84 | 4.33 / 3.95 |
| rows (in order) equal | 14/14 | 14/14 |

With a 1 MiB budget: 1,051,158 bytes resident (budget 1,310,704 incl. the
key cache), 2.2M evictions, Q6 / Q13 / Q14 at 1.57× / 2.94× / 2.89×, answers
equal. Before the batch commit (`478581d`), with two cache probes per cell,
the 64 MiB ratios were 1.98× / 1.66× / 1.81×.

`--mode query` (`DiskStore::query`, its own lazy dictionary), base vs branch
on the same database, two alternating rounds: geomean 5.97 / 5.90 → 4.63 /
4.11 ms; Q6 37.8 / 42.4 → 34.7 / 34.3 ms, Q13 74.9 / 69.7 → 38.0 / 32.7 ms,
Q14 55.7 / 53.5 → 22.8 / 33.5 ms (R1-KV's request: `get_with`, no
per-look-up copies, batched materialization); answers equal to the memory
answer on both.

### Acceptance (R2-DICT)

- Snapshot RSS after open ≤ 0.9 × pre at univ 38: **left to the measure
  stage**; univ 8: 0.80× base (128.6 → 102.7 MiB; pre-Phase-6 univ 8 was
  125.8 MiB).
- Snapshot Q1–Q14 geomean within ±5% of main-before-branch: **met at univ 8**
  (0.999× / 0.996×, noisy; Q6/Q13/Q14 individually +6–10%); univ 38 left to
  the measure stage.
- Out-of-core path at univ 38, resident dictionary bytes ≤ 64 MiB after
  Q1–Q14 × 3 and Q6/Q13/Q14 ≤ 2 × in-memory dictionary: **left to the
  measure stage**; univ 8: 12.4 MiB resident, Q6/Q13/Q14 at 0.87–1.57×.
- Second identical CONTAINS ≤ 0.25 × the first (univ 8 snapshot): **met**,
  0.005–0.007.
- `dict.bin` hash identical to pre-Phase-6 for the same input: read as the
  brief's override — (a) `persisted_encoding_is_unchanged` passes (accessor
  lines adapted, see Changelog), (b) mirror tests both ways **pass**, (c)
  cross-binary compatibility with the base binary **met**; and the branch's
  `dict.bin` is now reproducible (identical hash for two builds), which the
  base's was not.
- Tests and quality bar: **met** — `cargo fmt --all -- --check`, `cargo
  build --workspace --all-targets`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace` (2104 passed, 0 failed, 1
  ignored); `cargo clippy -p gstore --all-targets --features rocksdb -- -D
  warnings` and `cargo test -p gstore --features rocksdb` (1924 passed, 0
  failed, 1 ignored; `db::persist`'s RocksDB term writes changed). No other
  feature-gated code was touched.

## Requests / Not yet

Requests (files outside R2-DICT):

1. **R3-STREAM (`src/kvstore/store.rs`):** move
   `lazy_dictionary_resolves_term_and_id_on_demand` to `term()` / `pred()`
   (it asserts the legacy `id_to_string` returns `Some` on a backed
   dictionary); then `LEGACY_PIN_SLOTS` in `src/dict/mod.rs` can drop to 0,
   making the legacy API strictly fail-closed as C4 says. Also: run
   `DiskStore::query` inside a `ReadScope` and fail on a recorded error (today
   a backend error there is only counted as unscoped); switch
   `disk_backing()`'s count check to `DiskTermSource::generation()` once
   `DiskDict::intern` bumps it (Stream mode).
2. **R3-STREAM / R4-RECLAIM (`DiskDict`):** `intern` (R3) and `free` (R4)
   must bump `DiskDict`'s generation (it empties both caches) or update the
   caches in place; with holes, `DiskDict` must bound ids by the allocated
   limit rather than the live count (`slot()` uses `count` today, which is
   the next id while ids are dense).
3. **R4-RECLAIM (`src/dict/mod.rs`):** the free stack is a `Vec<u32>` (LIFO);
   a `BTreeSet` for lowest-id-first reuse must serialize as the same `seq`.
4. **pfn-wasm owner (`src/pfn/wasm.rs`):** `GraphCtx::term_str` / `pred_str`
   still return `&str` (the host functions convert with `Into`); returning
   `TermStr` would remove the last legacy use on a path that may see an
   out-of-core dictionary.
5. `src/analytics/gfun/mod.rs` (not in R2-DICT's list) still calls
   `id_to_string` / `predicate_to_string` twice (graph-function output
   labels); fine in memory, to migrate in a later round.

Not yet / notes:

- univ 38 numbers (snapshot RSS and geomean, out-of-core resident bytes and
  Q6/Q13/Q14 ratios) are for the measure stage, with these commands
  (from the repository root, release build of this branch):
  - snapshot: `GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache flock
    /tmp/gstore-bench.lock target/release/gstore-bench run --profile
    reference --scale 38 --backends snapshot --drop-caches --verify-data
    --out /tmp/r2dict-u38.json`, then `target/release/gstore-bench compare
    bench/results/pre-phase6-lubm38.json /tmp/r2dict-u38.json --backends
    snapshot --markdown` (RSS after open ≤ 0.9×) and the same against
    `bench/results/phase6-r1-lubm38.json` (main before the branch; geomean
    within ±5%);
  - out-of-core: `flock /tmp/gstore-bench.lock target/release/gbuild --disk
    lubm38 /tmp/gstore-bench-cache/lubm-like1-u38-s0.nt` (or the R2-BULK
    loader), then `flock /tmp/gstore-bench.lock
    target/release/examples/kv_bench lubm38.db --mode dict-ooc --qreps 5`:
    `dictooc_resident_bytes` ≤ 67,108,864 and `dictooc_q6_ratio`,
    `dictooc_q13_ratio`, `dictooc_q14_ratio` ≤ 2.0, `dictooc_all_match=true`;
  - `dict.bin`: build the univ 38 snapshot twice with the branch `gbuild`
    (`sha256sum */dict.bin` equal) and query each side's database with the
    other side's `gquery`.
- At univ 38 the distinct strings Q1–Q14 touch (about 450k) come close to the
  64 MiB term budget (charged at length + 72 bytes); if they exceed it the
  cache evicts within a query and Q6/Q13 slow down (see the 1 MiB run).
- The cached substring index holds about 83 MiB at univ 8 once a substring
  query ran (scaling with the dictionary; `StringIndex::build` now trims the
  posting lists' growth slack); a compact (front-coded strings, delta-coded
  postings) or persistent index is plan item D5.
- A backed dictionary has no substring index (D5); substring filters scan.
