# Phase 6 执行计划:规模与性能(gstore-rust)

> Plan version 1 (2026-11-01). The design record for Phase 6 of docs/GAP_PLAN.md; R4-CLOSURE reports the measured outcome of each gate in docs/GAP_PLAN.md §9 and docs/BENCHMARKS.md.
>
> - **Semantic reference:** `eaf53202cb9cf2e81493ed8a49480bf7fbf81d83` ("eaf5320"). Line numbers below refer to eaf5320.
> - **Implementation base:** `main` once phases 0–5 and phase 4 (reasoning/PFN) are merged. Code moves during those merges, so locate it **by symbol**, not by line. The phase5 branch provides the streaming import (`src/parser/import.rs`, `src/parser/stream.rs`, `Database::build_from_sources` / `insert_sources` / `remove_sources`, `DiskStore::import_sources`) and is the base for every bulk-loading package.
> - **Inputs:** the five subsystem maps, the measured baseline, nine design proposals (two angles each for disk streaming and concurrency) and the critique. This plan resolves the critique; §1 records each decision.
> - **Shape:** 4 rounds of at most 3 packages each (12 packages). Each package runs in its own git worktree. Within a round no two packages own the same file. Everything that did not fit is in §8 "Deferred", with a reason.

---

## 0. 基线(eaf5320,4 vCPU / 16 GB,Python 生成器 gen_lubm.py)

| Metric | memory (snapshot) | disk (`--disk`, B+tree) | RocksDB |
|---|---|---|---|
| Build 1.05M / 5.19M | 12.5 s / 67.8 s (VS-tree sort 65–68%) | 253 s / **1522 s** | 37.2 s / 198.6 s |
| Build peak RSS 5.19M | 6770 MiB (parser) | 6769 MiB | 6775 MiB |
| On-disk size 5.19M | 432 MB | 891 MB (kv 779 + vstree 112) | 206 MB |
| Open, cold / warm, 5.19M | 1.53 / 1.36 s | 11.7 / 7.7 s (`to_memory`) | **57.1 / 53.2 s** (VS-tree rebuilt every open) |
| RSS after open, 5.19M | 596 MiB | 544 MiB (HWM 740) | 522 MiB |
| Q1/Q4/Q9/Q14, 5.19M, warm ms | 2.8 / 80.6 / 495 / 125 | 49 / 71 / 1544 / 382 | 61 / 1031 / 2153 / 356 |
| Q4 at 5.19M ÷ Q4 at 1.05M | 7.5× (VS-tree walk, hotspot 4a) | — | — |
| Read scaling, 8 clients vs 1 | Q4 74.7 vs 86.7 qps (**0.86×**); /api 0.77× | — | — |

Answer counts per query, 1M / 5M: Q1 8/8, Q2 28/132, Q3 9/9, Q4 10/10, Q5 481/481, Q6 82111/405346, Q7 34/34, Q8 11137/11137, Q9 1050/5413, Q10 8/8, Q11 333/333, Q12 21/21, Q13 87795/433468, Q14 62096/307070.

The Rust LUBM-like generator (R1-BENCH) will not reproduce the Python data byte for byte. So **every later acceptance ratio is measured against `bench/results/pre-phase6-lubm{8,38}.json`**, which R1-BENCH records on the base commit with the new tool. The absolute numbers above show the expected magnitude only. `univ 8` ≈ 1.05M triples, `univ 38` ≈ 5.2M, `univ 82` ≈ 10.8M (comparable to upstream's lubm10M).

Upstream reference: gStore 0.5.0 at lubm10M (formal_experiment.tex) took 248.5 s to build and produced a 2.86 GB index. Comparable queries: q8≈Q1 24 ms, q1≈Q2 144 ms, q10≈Q3 29 ms, q4≈Q4 37 ms, q13≈Q14 223 ms. Its harness has a floor of about 23 ms per query, and the hardware differs from ours.

---

## 1. 设计决策(逐项,含对 critique 的处理)

| Item (GAP_PLAN §8 stage 6) | Decision | Main reasons and how the critique is handled |
|---|---|---|
| **Backend::Disk, streaming queries straight through Database** | **Conservative angle**, keeping these parts of the hybrid: `Backend::Disk(DiskTriples)` over the existing kvstore.kv SPO/POS/OSP trees; no format change; Materialize (today's `to_memory` working set) kept as fallback and test oracle. **The dictionary is also out of core**: the kvstore.kv in-file dictionary, read through a bounded cache (see the dictionary row). | The ambitious seg/ layout is deferred (§8). Fixes the critique asked for: (a) read errors are captured **per query**, through a thread-local `ReadScope`, not in a shared sticky slot (C2); (b) the Materialize fallback keeps exact ids, and treats vstree.kv as stale when alignment cannot be proven; (c) every logical write runs inside a pager **op bracket** (no-steal), so an eviction flush can no longer split a term or triple write; (d) the snapshot gate is **drain + cancel**: a commit cancels in-flight snapshot readers instead of waiting behind them; (e) Stream mode accepts dictionary holes (R4 frees ids by id, inside DiskTriples). |
| **Dictionary and VS-tree stay on disk** | Dictionary: keep kvstore.kv's `entity2id/literal2id/predicate2id` B+trees and `id2*` IvArrays, accessed out of core through `DiskDict` with a bounded `TermCache`. The API returns `TermStr` (borrowed in memory, `Arc<str>` on disk), which ends the `Box::leak`. Results are materialized in sorted batches (the equivalent of upstream `trySequenceAccess`). VS-tree: `vstree.kv` stays on disk and is used while fresh; it is never rebuilt at open; rebuilding is a streaming pass. The in-memory VS-tree gains a locator and probe refinement, and deletes no longer invalidate it (its entries form a superset). | The disk-dictionary design's `terms.kv`/`vsindex.kv`/GSTOREK2 format is **rejected for Phase 6**. It collides with id-reclaim's use of GSTOREK2, needs cross-file commit hooks, and forces a one-way upgrade. Keeping terms in the same pager/WAL as the triples makes the term-before-triple rule fall out of one op bracket. Kept from that design: the `TermStr` API, the bounded caches, batched prefetch, the sort fix, probe refinement and the superset (grow-only) soundness argument. The IvArray 33.5M cap and the 4 KB key limit are deferred (feature bits reserved; LUBM-1000 is about 22M entities, below the cap). |
| **Persistent StringIndex** | **Upstream meaning** (persistent id→string with batched, offset-sorted access) = the kvstore `id2*` IvArray plus `Dictionary::prefetch` / `resolve_many` (R2-DICT). The Rust-only trigram substring index is **cached per mutation generation** for in-memory dictionaries (it is no longer rebuilt per query). | A persistent trigram `lexindex.kv` is deferred (§8). It only prunes candidates; disk-backed dictionaries run substring FILTERs correctly without it; no LUBM/WatDiv acceptance query uses substring filters. |
| **Bulk index build** | **Balanced bulk design, reduced**: external sort of id tuples, bottom-up B+tree and IvArray page images at 90% fill, streaming vstree.kv, staged crash-safe install that durably renames the old `rocksdb/` aside, `gbuild --disk` (R2). RocksDB uses an SST-ingest sink built on the same pipeline (R3). Output is kvstore.kv **v1** (byte-compatible). | Ids are first-seen in (s,p,o,g) order, identical to the memory build. Deferred: the parallel deterministic encoder and dictionary overflow segments. The term map is reported but sits outside the budget (about L+26 B per term). One sort fix ordered by (limbs, id) is shared by CORE and BULK. |
| **RwLock or cheap snapshots instead of `Mutex<Database>`** | **Conservative angle**: `Database: Send+Sync` with a `&self` read path (R1), `RwLock` servers (R3), copy-on-write `Arc` for the Interner (R2) and the TripleStore (R4), a `DiskReadView` gate for disk snapshots (R2/R4). Taken from the ambitious angle: the PFN metadata cache with single-flight loads, `Arc<QueryResult>` cache entries, and lock-free monitoring counters. | Persistent data structures and pager MVCC are deferred (§8). Fixes the critique asked for: (a) the snapshot-layout save loop is gone because deletes no longer invalidate the VS-tree (R1); (b) re-entrancy is detected in release builds as well, and returns an error instead of deadlocking (R3); (c) ConcurrentDb publish costs an O(N) store copy at the first write after each publish; this is documented and benchmarked, not claimed to be O(1). |
| **Reclaim ids immediately on delete** | **Balanced id-reclaim design** (R4), plus a native Backend::Disk path that frees by id under the DiskTriples writer gate. Reuse is **lowest-id first** on every layout. The free list lives in kvstore.kv slot 22 as a B+tree under feature bit `FREE_IDS` (format registry C3). Existing v1 files are **never upgraded implicitly**: reclamation on them is off until `gcompact --enable-reclaim`. New databases are created with the feature enabled. | Fixes from the critique: the GSPO graph-first limitation is documented (the in-memory `NamedGraphs` reference counts are authoritative); the VS-tree is **not** invalidated on retire (superset rule); CLEAR, DROP, MOVE and COPY get redo-log records; the set of existing test files R4-RECLAIM may edit is fixed. |
| **Benchmark, CI, docs** | **Balanced bench design**: a `gstore-bench` crate lands in **R1**, so every later round measures against checked-in pre-Phase-6 JSON. It is the only harness: the other designs' `lubm_bench.rs`, `scripts/bench/*` and `gen_lubm.py` copies are dropped. Repo-wide rustfmt is applied as one integrator commit right after the R1 merge. Docs and CHANGELOG use `changelog.d/` fragments, folded in R4. | WatDiv-like workload in R4-CLOSURE. CI uses a shared rust-cache key for RocksDB; the bench crate's default tests stay at or under 60 s. |

Engine CPU hotspots: two cheap, plan-preserving fixes are in R1-CORE (a fast deterministic hasher in `join_sets_hash`; VS probe refinement). The pipelined/morsel executor and UNION de-materialization are deferred, because the memory backend already sits within the same order of magnitude as upstream on comparable queries.

---

## 2. Phase 6 目标与验收(退出闸门,R4-CLOSURE 用 gstore-bench 测量)

All gates apply on the reference box (4 vCPU / 16 GB, no other heavy jobs running) with the LUBM-like generator. "pre" means `bench/results/pre-phase6-lubm{8,38}.json`. Unless stated, gates are measured at univ 38.

| # | Gate | Target | pre / old baseline |
|---|---|---|---|
| G1a | Disk build (`gbuild --disk`, bulk) | ≤ 0.04 × pre (≈ ≤ 60 s); peak RSS ≤ 1.25 GiB with `--memory 1024`; kvstore.kv ≤ 0.75 × pre | 1522 s; 6.77 GiB; 779 MB |
| G1b | Disk open (Stream) | warm ≤ 0.5 s, cold ≤ 2.0 s; open(univ 38) / open(univ 8) ≤ 1.5 | 7.7 / 11.7 s |
| G1c | Disk memory (Stream) | RSS after open ≤ 120 MiB; VmHWM through Q1–Q14 ≤ 400 MiB; RSS-after-open ratio univ 38 / univ 8 ≤ 1.5 | 544 / 740 MiB |
| G1d | Disk latency (Stream) | Q1–Q14 warm geomean ≤ 0.6 × pre disk, and ≤ 1.5 × the memory layout of the same commit; no query > 3 × memory | Q1 49 ms, Q9 1544 ms |
| G2 | Memory/snapshot | build ≤ 0.5 × pre; RSS after open ≤ 0.9 × pre; Q1–Q14 geomean ≤ 0.9 × pre; Q4 ≤ 25 ms; Q1/Q3/Q4/Q11/Q12 latency ratio univ 38 / univ 8 ≤ 2.0 | 67.8 s; 596 MiB; Q4 80.6 ms (ratio 7.5×) |
| G3 | RocksDB | build ≤ 0.25 × pre (≈ ≤ 50 s); open ≤ 10 s; Q1–Q14 geomean ≤ 0.8 × pre | 198.6 s; 57.1 s |
| G4 | Concurrency | Q4 qps at 8 clients ≥ 2.5 × 1 client via /sparql and /api, on memory and Stream-disk databases; `/begin` ≤ 5 ms (memory) and ≤ 50 ms (disk); RSS growth at `/begin` ≤ 4 MiB (memory) and ≤ 16 MiB (disk); monitoring answers in ≤ 100 ms while a long query runs | 0.86× / 0.77×; `/begin` = full clone |
| G5 | Id reclamation | 10 LUBM churn rounds: live counts equal the referenced terms exactly, within ±5% of round 0, high-water ≤ 1.05 × peak live; delete overhead ≤ 10% on memory and disk, ≤ 20% on RocksDB | dictionary grows about 2.2× under churn |
| G6 | Correctness | identical answer counts and row hashes on every layout and mode; full test matrix (default + gzip/tls/zip/readline/pfn-wasm/rocksdb) green; conformance.sh green; crash suites green | — |
| G7 | "Same order as upstream" | at univ 82 against upstream lubm10M: build µs/triple, bytes/triple and each comparable query ≤ 10× upstream (≤ 3× counts as **met**); report per metric | upstream 23 µs/triple, 264 B/triple |

If G1d fails, Stream mode ships opt-in and Materialize stays the default disk query mode (R3-STREAM decides; R4-SNAP and R4-CLOSURE report). G1a–c, G2–G7 stand on their own.

---

## 3. 磁盘格式与版本策略

Policy:
1. **Existing databases are never upgraded implicitly.** A new binary opens every pre-Phase-6 directory read-write unchanged.
2. **New databases may be created in the newest format** that the creating binary supports, through a feature bit, and old binaries must **refuse** them loudly rather than misread them.
3. One registry owns all versioning: `src/kvstore/format.rs` (R1-KV). No package invents its own magic, slot or layout version.

### 3.1 kvstore.kv (and every Pager file)
- `MAGIC_V1 = "GSTOREKV"`, `MAGIC_V2 = "GSTOREK2"`. `NROOTS` grows from 24 to 32. Older files read the extra slots as 0. The header of a V1 file written by new code is byte-identical to today's, with zero padding.
- Root-slot map:

| Slots | Use | Status |
|---|---|---|
| 0–20 | Unchanged: SPO/POS/OSP, the six dictionary structures, counters, SP2O_VLIST, COMPACTED, PRED_STATS, distinct counts, GSPO, quad count, VSTREE_STALE | existing |
| **21** | `FEATURES` u64 bitmask | new |
| **22** | `FREE_IDS` B+tree root | R4 |
| **23–25** | `FREE_COUNT_E/L/P` | R4 |
| 26–31 | — | reserved |

- Feature bits:

| Bit | Feature | Owner |
|---|---|---|
| 0 | `FREE_IDS` | R4-RECLAIM |
| 1 | `IVARRAY_WIDE` | reserved, deferred (lifts the 33.5M cap) |
| 2 | `LONG_KEYS` | reserved, deferred (dictionary keys > 4 KB) |
| 3–63 | — | reserved |

- Rules:
  - `FEATURES == 0` ⇔ `MAGIC_V1`.
  - The first use of a feature sets its bit **and** `MAGIC_V2` in the same pager flush as the first structure that needs it.
  - On open, refuse unknown bits, and refuse `MAGIC_V2` with `FEATURES == 0`.
  - vstree.kv must have `FEATURES == 0`.
- Node, IvArray, overflow and WAL formats are unchanged throughout Phase 6.
- Stream mode, the bulk loader (R2) and Materialize mode all write **V1 only**. Before R4, any directory can therefore switch freely between Stream mode, Materialize mode and older binaries.

### 3.2 Snapshot layout
`dict.bin`, `store.bin`, `named.bin`, `vstree.bin` and `meta.bin` stay **byte-identical**. Golden tests enforce this:
- `persisted_encoding_is_unchanged`;
- new mirror tests for TripleStore and VsTree.

Hand-written serde for the `Arc` copy-on-write and `Arc<str>` structures must reproduce today's bincode exactly. A `free` stored as a BTreeSet serializes as the same seq.

### 3.3 RocksDB
`LAYOUT_VERSION` stays `"1"`; the CF set and key encodings are unchanged. Gaps in the dict CF already mean free ids. The block cache, bloom filters and SST ingest are options or build-path changes only, not format changes.

### 3.4 Compatibility matrix (at the end of Phase 6)

| Directory | New binary | Old binary |
|---|---|---|
| Snapshot (any age) | rw, unchanged | rw (bytes identical) |
| kvstore.kv V1, pre-existing | rw in Stream or Materialize; reclaim off until `gcompact --enable-reclaim` | rw |
| kvstore.kv V1 built by R2+ bulk | rw | rw |
| kvstore.kv V2 (`FREE_IDS`; new databases from R4 on, or explicitly upgraded ones) | rw | **refuses** ("not a gStore KV file") |
| RocksDB layout 1 | rw (no VS-tree rebuild at open) | rw |

Downgrading from V2 means `gexport` with the new binary, then `gbuild` with the old one.

---

## 4. 共享契约(各包必须遵守;owner 在括号里)

**C1 TripleSource seam (R1-CORE).**
- `Backend` and `backend_dispatch!` move to `src/store/backend.rs` (re-exported as `crate::store::Backend`). `Backend` stays static dispatch.
- Provided methods, with defaults that are correct for every implementor:
  - `count_po_by_s`, `count_o_by_sp`, `count_p_by_so`, `count_ps_by_o`, `count_s_by_po`, `count_so_by_p` (u64; exactly the `len` of the range);
  - `distinct_p_of_s`, `distinct_p_of_o`;
  - `has_subject`, `has_object`, `has_predicate`. On a read error these **must return true** (never free an id when in doubt).
- Ordering contract, documented on the trait:
  - `po_by_s` returns (p,o); `ps_by_o` returns (p,s); `so_by_p` returns (s,o);
  - `o_by_sp`, `s_by_po`, `p_by_so`, `subs_by_p`, `objs_by_p` and the key lists are ascending;
  - `iter_all` returns (p,s,o).
- Overrides must equal the defaults **exactly**, so plans and solution order never change.

**C2 Per-query read errors (R1-CORE), in `src/store/read_scope.rs`.**
- `ReadScope::enter()` returns a guard: `!Send`, nestable, per thread.
- `scope.take_error()`.
- `note_read_error(msg)` records into the innermost scope on the current thread. With no scope active it bumps a process-wide `unscoped_read_errors()` counter and logs.
- Every query, UPDATE WHERE evaluation, CSR build, export and snapshot query runs inside a scope. A recorded error makes the statement fail, and the result is never cached.
- Backends (DiskTriples, DiskDict, RocksStore) report through `note_read_error`. They **never** clear anything shared.
- The evaluator is single-threaded. Any future parallel evaluation must hand the scope across threads explicitly.

**C3 Format registry (R1-KV).** As in §3.1. `Pager::features()`, `enable_feature(bit)`, `require_supported()`.

**C4 Dictionary (R2-DICT).**
- `TermStr<'a> { Borrowed(&'a str), Shared(Arc<str>) }`.
- Dictionary methods:
  - `term(id)`, `pred(id)`, `try_term`, `try_pred`, `try_*_id`;
  - `prefetch`, `resolve_many`, `for_each_term`, `materialize`, `id_limit`, `backing`.
- `DiskTermSource` becomes a full, fallible, `&self`, Send+Sync backend:
  - `lookup`, `resolve -> Arc<str>`;
  - `intern` (default `Err`), `free` (default `Err`);
  - `prefetch`, `for_each`, `count`, `id_limit`, `generation`, `sync`.
- The legacy `id_to_string` / `predicate_to_string -> Option<&str>` work for in-memory dictionaries only. On a backed dictionary they return `None` and note a read error (fail closed; no leak).
- The Interner is `Arc<InternerData>` (copy-on-write), with each string stored once.

**C5 DiskTriples (R2-DISKLIB).**
- `DiskTriples`: a `TripleSource + MutableStore` over `DiskStore`, with a transaction overlay. No triple reaches the pager before commit; dictionary terms may, because orphan terms are safe.
- Writer gate: a writer sets `draining`, cancels in-flight snapshot readers, waits for them to leave, applies its change, then bumps `epoch`.
- `DiskReadView::with_base(|&DiskStore, &Arc<AtomicBool>| ..) -> Result<R, ViewError{Expired, Closed, Io}>`.
- Broken state: any write or flush error refuses later writes **and** reads until the database is reopened.
- `commit_point(&self)`.

**C6 Bulk build (R2-BULK).** `gstore::bulk::build(dir, name, BulkInput, &ImportOptions, &mut dyn ErrorLog, BulkTarget{Disk, RocksDb}, &BulkOptions) -> Result<BulkReport>`, with staged, crash-safe install. `BulkOptions { memory_budget, threads, temp_dir, fill_percent }`.

**C7 Open options (R3-STREAM).**
- `OpenOptions { disk_query_mode: Stream|Materialize, disk_cache_pages, dict_cache_bytes, vstree_on_open, default_layout: Option<Layout> }`.
- `set_default_open_options` and `default_open_options()`. Environment overrides: `GSTORE_DISK_QUERY_MODE=stream|memory`, `GSTORE_DISK_CACHE_MB`.
- `Database::open_with`.
- Config: the `[storage]` section.

**C8 Concurrency (R1-CORE, R3-SRV).**
- `&self` methods on `Database` may run concurrently; `&mut` methods need exclusivity.
- Lock order:
  - ApiState → (never wait for a db lock while holding it) → db slot → rule-dir lock;
  - inside Database: `persist_lock` → disk → `persist_error` → pager;
  - DiskTriples: gate → pager.
- No thread ever holds two db guards, re-acquires a guard it already holds, or upgrades read to write without dropping the read guard first.
- Every logical kvstore write holds the pager write lock for the whole operation.

**C9 Invariants that every package keeps.**
- Every applied data change goes through `touched()` and advances `mutation_count` if and only if data changed.
- Acknowledge only after the write is durable (Phase 0 commit points; DiskSink and broken-backend semantics).
- The VS-tree only ever produces a sound superset, and is used only while `index_valid`.
- Dictionary id-space bounds are enforced through `checked_*_id`.
- Layout detection precedence: `rocksdb/` > `kvstore.kv` > `meta.bin`.
- The HTTP transaction contract: single writer, 409 for non-transactional writes during a transaction, begin-time snapshot reads, tids, `retired` slots.
- RocksDB writes a term before any triple that uses it, and journal replay is unchanged.

**C10 Change fragments (all).**
- Each package writes `changelog.d/phase6-<ID>.md` with sections: Changelog, GAP_PLAN status line, Docs notes, Measured numbers, Requests/Not yet.
- No package except R4-CLOSURE edits README.md, CHANGELOG.md or docs/*.md.

---

## 5. 执行规则(所有包)

1. **Worktree:** `git -C /home/user/gstore-rust worktree add <path> -b phase6/<ID> main`, where `main` is the state after the previous round was merged and verified. Locate code by symbol.
2. **Ownership:** edit only the files listed for your package (new files under listed paths count as listed). If you need a change elsewhere, work around it inside your own files, or record it under "Requests" in your fragment for a later round. The integrator rejects diffs outside your ownership.
3. **Dependencies:** no new third-party crates (std, serde, serde_json, bincode, regex, plus the existing feature-gated crates). RocksDB code stays behind `rocksdb`.
4. **Verification before handoff:**
   - `cargo fmt --all -- --check` (from R2 on);
   - `cargo clippy --workspace --all-targets -- -D warnings`;
   - `cargo test --workspace`;
   - `cargo test -p gstore --features rocksdb` if RocksDB code can reach your change;
   - the feature matrix (gzip, tls, zip, readline, pfn-wasm) if you touched feature-gated code.
5. **Benchmarks:**
   - Run the `gstore-bench` reference profile at univ 8 and/or 38 (R1 packages use the prototype harness, see their briefs).
   - Measure the base and your branch **in the same session**.
   - Serialize heavy runs across all agents with `flock /tmp/gstore-bench.lock <cmd>`.
   - Share generated `.nt` files through `/tmp/gstore-bench-cache/`.
   - Delete databases right after measuring, and keep at least 7 GiB of disk free.
   - Record the numbers in your fragment.
6. **Merge protocol per round (integrator):**
   - (a) Merge the packages in the listed order.
   - (b) Run the full verification from rule 4, plus `scripts/conformance.sh` and `gstore-bench ci`.
   - (c) Re-run `gstore-bench run --profile reference --scale 8` and compare with pre-Phase-6; confirm each package's acceptance.
   - (d) Re-bless `bench/baselines/smoke.json` if things improved by ≥ 1.5×.
   - (e) After round 1 only: one pure-format commit `cargo fmt --all`, plus `.git-blame-ignore-revs` holding its SHA, before round 2's worktrees are cut.
7. **Fallback:** a package that cannot meet a performance gate still merges, as long as correctness gates and the full suite pass and its feature stays behind the documented switch. It records the shortfall in its fragment, and R4-CLOSURE reports it.

---

## 6. 实施轮次(Rounds)

Round order follows the dependency chain:
- **R1:** harness + seam + kvstore primitives;
- **R2:** dictionary API, DiskTriples library, bulk loader;
- **R3:** Database wiring for Stream mode, RwLock servers, RocksDB;
- **R4:** id reclamation, cheap snapshots and disk serving, closure.

### Round 1 — foundations (merge order: R1-KV, R1-CORE, R1-BENCH; then the rustfmt commit)

#### R1-BENCH — gstore-bench harness, pre-Phase-6 reference, CI bench + rustfmt config
**Brief.**
1. **Crate.** Add workspace member `bench/` (package `gstore-bench`, `publish=false`; deps `gstore` (path), `serde`, `serde_json`, which Cargo.lock already has via gnlqa; `[features] rocksdb=["gstore/rocksdb"]`). Add `members += "bench"` in the root Cargo.toml and `COPY bench ./bench` in the Dockerfile (cargo loads every member manifest). Check this by running `cargo metadata --locked` in a temp dir that holds only the Dockerfile's COPY set.
2. **Library.**
   - `rng`: SplitMix64 with Lemire `below`, `sample`, `shuffle`, and golden-sequence tests.
   - `hash`: FNV-1a 64 and `rows_fnv64`, a multiset hash in which unbound ≠ "".
   - `workload` trait and a built-in `tiny` workload.
   - `lubm`: a Rust port of `the design-stage prototype `gen_lubm.py` (kept outside the repo; R1-BENCH replaces it)`:
     - UBA cardinalities, the univ-bench IRIs, and the duplicate `rdf:type ub:University` lines;
     - the forced non-emptiness for Q1, Q2, Q7, Q9 and Q10;
     - one SplitMix64 stream per department, streamed output through a 1 MiB BufWriter;
     - version `lubm-like/1`, with a golden FNV for univ=1, seed=0;
     - `update_chunk`: a disjoint Department0 of University{univ};
     - queries via `include_str!` of `testdata/lubm/lubm_q1..14.rq`, with their text hashes pinned.
3. **Binary `gstore-bench`.**
   - Subcommands: `gen`, `run`, `check`, `ci`, `compare`, `report --markdown [--compare]`, `bless`, `calibrate`.
   - Every phase runs in a fresh child (`__child`): JSON request on stdin, exactly one `GSTORE_BENCH_RESULT {json}` line on stdout, per-child timeout. A crash or timeout exits with code 2, never a pass.
   - **The build phase invokes the real CLI** from a single module, `bench/src/runner/backends.rs`: `gbuild`, `gbuild --disk`, `gbuild --rocksdb`, located through `--bin-dir` (default `target/release`).
     - Records wall time, child peak RSS (`getrusage(RUSAGE_CHILDREN)` via `extern "C"`, Linux) and directory bytes.
     - `--gbuild-arg X` (repeatable) is passed through.
   - Open, query, update and conc phases call the public API in-process:
     - `Database::open`, the query-cache setter set to 0, plus a per-run `# gstore-bench run k` comment prefix;
     - first run plus median of `reps`;
     - rows and `rows_fnv64` must be stable across runs and equal across backends.
   - `--env K=V` (repeatable) is passed to every child. Later rounds use it for `GSTORE_DISK_QUERY_MODE` and similar.
   - Update phase:
     - N single INSERT DATA statements (median, p95);
     - chunk insert, checking that `triple_num` grows by exactly `chunk.unique_triples`;
     - chunk delete; the answers must be unchanged;
     - reopen; the triple count must match.
     - Entity/literal growth is report-only.
   - Conc phase: an in-process `Server` (single db, GET /sparql, TSV) **and** an `ApiServer` (`/api` operation=query), with closed-loop keep-alive clients. Every body hash must be identical.
   - Calibration `calib/1` (sort, hash map and format micro-benchmarks, min of 5; plus fsync_ms).
   - RSS from `/proc/self/status`; null on non-Linux.
4. **Schemas and gates.**
   - Result/baseline schema `gstore-bench/1`.
   - `bench/thresholds.json` (`gstore-bench-gates/1`):
     - exact gates: generator version and data hash, query text hashes, `triple_num == unique_triples`, rows and hashes equal to the baseline and across backends, update round trip, conc body equality;
     - one-sided, calibration-normalized timing gates: per query `(m+2ms)/(b+2ms) > 3.0` fails; geomean > 1.5 fails; build > 2.0 fails (2.5 for disk); open > 2.5 fails;
     - RSS: `(m+32MiB)/(b+32MiB) > 1.5` fails;
     - disk bytes: > 1.25 fails;
     - 2 targeted retries using min-of-medians.
   - Exit codes 0/1/2. `--self-test-slowdown X` serves as a canary.
   - Profiles: `micro` (tests), `smoke` (CI: univ=1, snapshot+disk, conc report-only), `reference` (univ 8 and 38; snapshot, disk, rocksdb if compiled; reps 5; conc [1,8]).
5. **CI.**
   - New `.github/workflows/bench.yml`:
     - job `smoke`: release build, `gstore-bench ci` with `$GITHUB_STEP_SUMMARY`, canary `check --self-test-slowdown 3` must exit 1, artifact upload;
     - job `rocksdb-correctness`: `cargo test -p gstore-bench --features rocksdb`, with a rust-cache **shared-key** shared with ci.yml's rocksdb job;
     - job `reference`: dispatch/weekly, report-only;
     - `paths-ignore` for docs; cancel-in-progress.
   - `ci.yml` gains a `fmt` job (`cargo fmt --all -- --check`).
   - Add `rustfmt.toml` with `style_edition = "2021"`, and `#[rustfmt::skip]` on `const SHA512_K` in `src/query/hash.rs`.
   - **Do not reformat any other file.** The integrator runs `cargo fmt --all` after the R1 merge.
6. **Reference data.** On this worktree's base (which **is** pre-Phase-6), record `bench/results/pre-phase6-lubm8.json` (snapshot, disk, rocksdb) and `bench/results/pre-phase6-lubm38.json` (same backends).
   - The disk build at univ 38 takes about 25 min: run it in the background under `flock /tmp/gstore-bench.lock`, with the RocksDB build in a separate `CARGO_TARGET_DIR`.
   - Bless `bench/baselines/smoke.json`.
   - Write `bench/README.md`, and `changelog.d/README.md` describing the fragment convention (C10).

**Tests:** RNG and hash golden values; LUBM determinism plus the golden hash; `unique_triples == triple_num`; all 14 queries non-empty at univ 1, plus pinned micro row counts; chunk disjointness round trip; gate unit tests on synthetic JSON; CLI end to end with `micro` (bless→check passes, canary exits 1, injected child crash and timeout exit 2, cache sidecar regeneration); a conc micro run with equal bodies. Default `cargo test -p gstore-bench` ≤ 60 s in debug (disk micro runs are `#[ignore]` or release-only).

**Acceptance:**
- univ 1/8/38 give 143K/1.05M/5.19M unique triples, each ±5%. All 14 queries are non-empty at every scale. Generation runs at ≥ 1M triples/s in release.
- Fidelity against the ad-hoc baseline:
  - snapshot univ 8: warm medians of Q2, Q4–Q10, Q13 and Q14 within ±30% of 9.6/10.7/45.5/31.5/55.7/54.3/77.0/43.2/35.6/26.6 ms;
  - RocksDB open at univ 8: 9.7 s ± 30%;
  - disk Q5/Q9/Q13: within ±30% of 139/337/207 ms.
- Repeatability: two univ 8 snapshot runs have a geomean ratio in [0.93, 1.07]; the snapshot and disk byte counts are identical.
- Gates:
  - a uniform 1.3× slowdown passes; 1.6× fails;
  - a ≥ 20 ms query slowed 4× fails;
  - a row-hash change fails with no retry;
  - the canary exits 1.
- CI smoke job takes ≤ 15 min with a warm cache.
- `pre-phase6-lubm8.json` and `pre-phase6-lubm38.json` are committed, and the default `cargo test -p gstore-bench` takes ≤ 60 s.

**Files:** `bench/**` (new), `Cargo.toml`, `Cargo.lock`, `Dockerfile`, `.github/workflows/ci.yml`, `.github/workflows/bench.yml` (new), `rustfmt.toml` (new), `src/query/hash.rs`, `changelog.d/README.md` (new), `changelog.d/phase6-R1-BENCH.md`.

#### R1-CORE — seam contracts, Sync `&self` read path, per-query read errors, VS-tree fixes
**Brief.**
- **(A) Seam, C1 and C2.**
  - Move `Backend`, `backend_dispatch!` and their impls verbatim to new `src/store/backend.rs`, re-exported.
  - Add the C1 provided methods with exact defaults. `TripleStore` overrides all of them with slice arithmetic (no `to_vec`), and `Backend` dispatches them.
  - Document the ordering contract.
  - Add `src/store/read_scope.rs` (C2).
- **(B) Database, C8.**
  - Make `Database: Send + Sync`:
    - query cache `RefCell` → `Mutex` holding `Arc<QueryResult>` entries, cloned after unlock and cleared on poison;
    - `backup_offset` → `AtomicU64`;
    - `disk: RefCell<Option<DiskSink>>` → `Mutex`;
    - `persist_error` → `Mutex`;
    - new `persist_lock: Mutex<()>` taken at the entry of `save`, `checkpoint`, `sync`, `backup` and `backup_incremental`, which delegate to `*_locked` internals without re-entrant locking (never call `storage()` while holding the disk guard);
    - a static `assert_send_sync::<Database>()`.
  - New `src/db/statement.rs`: `Statement { text: Arc<str>, query }` with `parse`, `is_update`, `text`, `query`.
  - `read_as(&self, &Statement, cancel, pfn) -> Result<QueryResult>`: the read branch of `query_inner`, moved verbatim (cache, VS-tree only while `index_valid`, named graphs, functions, PFN caller and limits, SERVICE policy, CSR provider, integer division, top-k, gfun bound, cache put), run inside a `ReadScope`. A recorded error returns `Err` and is not cached.
  - `query_read(&self, &str, ..)` and `execute_as(&mut self, Statement, ..)`. `query`, `query_cancellable` and `query_as` keep their signatures and delegate.
  - UPDATE WHERE evaluation and the `csr()` build closure also run inside a scope.
  - Make `persist::remove_other_layouts` and `wal_of` `pub(crate)` (for R2-BULK).
  - `open_rocksdb` stops calling `rebuild_index()` (`index_valid=false`). `GSTORE_VSTREE_ON_OPEN=1` restores the old behaviour. Tests that need a tree call `rebuild_index()` explicitly.
- **(C) VS-tree.**
  - **Sort fix:** `VsTree::build` and `DiskTree::build` use `sort_unstable_by` on precomputed limbs, then id. Entries arrive in ascending id, so this equals today's stable `sort_by_key(sig_key)`; prove `vstree.bin`/`vstree.kv` byte identity in a test.
  - **Locator:** add an in-memory id → leaf locator (serde-skipped, rebuilt on load and build; `vstree.bin` bytes unchanged), making `update` O(height) and `probe(id,&sig)` O(1).
  - **Probe refinement:** `refine_candidates_with_vstree` probes each existing candidate when an in-memory tree is present and the variable already has an exact candidate set of size ≤ max(1024, entities/32). The result must equal walk ∩ existing exactly. Disk trees keep walking.
  - **Superset semantics:** deletes and rollback-of-inserts no longer invalidate the in-memory tree. Signatures only lose bits, so stale entries stay a sound superset. Keep a `vs_shrink_debt` counter. Rollback-of-deletes upserts the affected entities. Inference and CLEAR still invalidate, and inserts still invalidate a read-only disk tree. This ends the "every save after a delete rebuilds the VS-tree" loop.
- **(D) Engine and optimizer.**
  - `join_sets_hash` uses a std-only FxHash-style `BuildHasher` instead of SipHash. Output order must not depend on the hasher; test it.
  - In `optimizer.rs`, `planner.rs` and `candidates.rs`, replace every count-only `range.len()` and `distinct_first(range)` with C1 methods. Plans must stay identical. Keep the value of the existing `ps_by_o`-based NDV with a FIXME: it counts predicates, not subjects.

**Invariants:** plans and ordered results are identical on all corpora; `touched()` and `mutation_count` are unchanged; acknowledge only after durable; dict/store/vstree bytes are unchanged; `Database::query(&mut self)` callers are unaffected.

**Tests:**
- `tests/dt_count_methods.rs`: counts equal `len` for TripleStore and Backend; plans are identical through a default-method wrapper.
- `src/db/concurrency_tests.rs`:
  - `read_as` equals `query` on LUBM Q1–Q14 with the VS-tree valid and invalid, named graphs, a user function and a CSR function;
  - `query_read` refuses an UPDATE with no side effects;
  - 8 threads run Q1–Q14 on a shared `&Database` and match the sequential results;
  - concurrent save/backup/incremental backup produce valid output and a contiguous chain;
  - a generation oracle runs under `RwLock<Database>` with the cache on.
- `tests/dt_read_scope.rs`: two threads, one with an injected error; only that query fails; unscoped errors are counted.
- VS-tree:
  - sort-fix byte identity;
  - probe == walk ∩ existing on random sets;
  - soundness: after random deletes, candidates ⊇ brute-force matches;
  - update through the locator equals a rebuild.
- `tests/dt_rocksdb.rs`: after reopen without a rebuild, answers are equal as multisets.

**Benchmark:** gstore-bench lands in this same round, so copy the prototype `lubm_bench.rs` (design-stage prototype, kept outside the repo) into your worktree's `examples/` **locally, uncommitted** (adapt the cache setter name). Use `gen_lubm.py --univ 8/38`. Measure base and branch in the same session under the flock. The integrator re-checks with gstore-bench after the merge.

**Acceptance:**
- Memory build ≤ 0.5 × base at univ 8 and univ 38.
- RocksDB open ≤ 2 s at univ 8 and ≤ 10 s at univ 38 (baseline 9.7 and 57 s).
- Memory Q4 ≤ 25 ms at univ 38, and Q4(univ 38)/Q4(univ 8) ≤ 2.0.
- Q1–Q14 warm geomean ≤ 0.95 × base at univ 38, with no query > 1.05 × base.
- Answer counts are identical.
- Single-client latency is within ±5%: Sync costs nothing.
- The full suite and the rocksdb suite pass.

**Files:** `src/store/mod.rs`, `src/store/backend.rs` (new), `src/store/read_scope.rs` (new), `src/db/mod.rs`, `src/db/persist.rs`, `src/db/query_cache.rs`, `src/db/statement.rs` (new), `src/db/concurrency_tests.rs` (new), `src/query/engine.rs`, `src/query/optimizer.rs`, `src/query/planner.rs`, `src/query/candidates.rs`, `src/signature/vstree.rs`, `src/signature/disk_vstree.rs`, `src/signature/mod.rs`, `src/lib.rs`, `tests/dt_count_methods.rs` (new), `tests/dt_read_scope.rs` (new), `tests/dt_rocksdb.rs`, `tests/dt_persist_backends.rs`, `tests/dt_phase0_review.rs`, `tests/dt_optimizer.rs`, `tests/dt_lubm.rs`, `changelog.d/phase6-R1-CORE.md`.

#### R1-KV — kvstore read path, per-operation atomicity, format registry (no format change)
**Brief.**
1. **`src/kvstore/format.rs` (new): the registry of §3.1, in code and module docs.** Move the root-slot constants here as `pub(crate)`, set `NROOTS=32`, add MAGIC V1/V2, `FEATURES` in slot 21, the feature bits and their rules, and `Pager::features()`, `enable_feature(bit)` (unused until R4) and `require_supported()`.
2. **`pager.rs`.**
   - Cache pages as `Arc<[u8; PAGE_SIZE]>`, add `read_page_shared` (no 4 KiB copy), and copy on write in `write_page` (`Arc::make_mut`). The existing `read_page` API keeps working.
   - Replace the O(capacity) `min_by_key` with CLOCK victim selection in both evict paths. Dirty pages leave the cache only through a committed flush.
   - On unix, use pread (`FileExt::read_exact_at`) for misses.
   - Add an **op bracket**, `begin_op()`/`end_op()` with a nesting counter. While an op is open, eviction never flushes and the cache may overshoot; `end_op` at depth 0 restores capacity.
   - Add `dirty_pages()` and `capacity()`.
3. **`bptree.rs`.** Add a zero-copy, bounds-checked `NodeView` (a malformed page returns `Err`, never panics). Use it for `get`, `find_leaf_ge`, `prefix_exists` and `count_prefix`, and for the new `scan_prefix_with(pager, prefix, FnMut(&[u8],&[u8]) -> ControlFlow<()>)` and `first_with_prefix`. Writes keep `Node`.
4. **`ivarray.rs`.** Add `get_with(id, FnOnce(&[u8]))` (zero allocation) and a sequential `for_each_range(from, to, f)` (one directory page per 32 ids; follows overflow chains). Make the layout constants (`SLOT_SIZE`, `SLOTS_PER_DIR`, `PT_FANOUT`, `INLINE_CAP`, `CAPACITY`) `pub(crate)`.
5. **`store.rs` (DiskStore).**
   - **Fallible reads:** inherent `try_*` methods mirroring every `TripleSource` method, returning `io::Result`. Decode straight into the output Vecs **in contract order** (C1):
     - `so_by_p` as (s,o) and `ps_by_o` as (p,s), both sorted;
     - `subs_by_p` sorted and deduplicated; `objs_by_p` deduplicated;
     - `subject_keys`/`object_keys` as a streaming dedup over SPO/OSP, with no `iter_all` and sort;
     - `iter_all` in (p,s,o) order.
     The legacy infallible `TripleSource` impl delegates to them; it keeps swallowing errors only on that test-only path.
   - **Exact counts:**
     - inherent `count_*` via allocation-free `count_prefix`, with `count_so_by_p` taken from the stats;
     - `distinct_p_of_s/o`;
     - `has_subject/object/predicate`, which return true on `Err`;
     - a pred-stats mirror (`Vec<(num,dsub,dobj)>`) plus a non-empty predicate count, loaded at open in O(P) and maintained by `insert_ids`/`delete_ids`;
     - an exact `distinct_subjects` (drop `.max(1)`), and `num_predicates` counting non-empty predicates only.
   - **Write paths:** make `insert_ids`, `delete_ids`, `insert_quad_ids`, `delete_quad_ids` (by id, refactored out of the key versions) and `intern_*` `pub(crate)`. **Every logical write runs in one op bracket while holding the pager write lock for its whole duration.** This fixes the torn insert and duplicate-id hazard noted at store.rs:474-479.
   - **Other additions:**
     - `shared_pager()`;
     - `from_shared_pager(Arc<RwLock<Pager>>)`, a read-only view with counts read from the roots and the stats mirror;
     - `commit(&self)`;
     - `for_each_entity_adjacency(f)`, a merged streaming SPO+OSP pass for VS-tree rebuilds;
     - `pub(crate)` `key3`/`key4` and `PRED_STAT_LEN` for R2-BULK.
6. **`disk_dict.rs`.** Compile fixes only.
7. **`examples/kv_bench.rs`.** Micro-benchmarks on a disk database built from `gen_lubm.py --univ 8`: hot random `po_by_s`/`o_by_sp`, full SPO scan, `to_memory` open, and `DiskStore::query` Q1–Q14.

**Invariants:** no format change. A file written with `FEATURES=0` keeps MAGIC V1, and its header bytes past `16+24*8` are zero. The WAL protocol and recovery are unchanged. All existing kvstore tests pass.

**Tests:**
- pager and bptree:
  - NodeView vs `Node::parse` property test on seeded random pages; `scan_prefix_with == scan_prefix`;
  - CLOCK never evicts a dirty page;
  - Arc copy-on-write: a holder of an old page keeps the old bytes;
  - pread == seek+read;
  - op bracket: with an 8-page cache a large op never flushes mid-op (cfg(test) counter) and does flush at `end_op`; `mem::forget` mid-op, then reopen, shows the op entirely absent;
- `for_each_range == get` per id, including overflow chains;
- DiskStore:
  - `try_*` is in **strictly ordered** equality with TripleStore on seeded data, including an empty store and data after deletes;
  - counts == len;
  - `distinct_subjects` and `num_predicates` are exact;
- format registry: a V1 file stays V1; an unknown bit is refused; V2 without features is refused;
- fix any order assumptions in `tests/dt_disk.rs`.

**Benchmark:** `examples/kv_bench.rs`, base vs branch, same session, under the flock.

**Acceptance:**
- Hot random `po_by_s`/`o_by_sp` ≥ 3× faster; full SPO scan ≥ 3× faster.
- Warm open of a univ 8 disk database (`to_memory`) ≤ 0.7 × base.
- `DiskStore::query` Q1–Q14 answers identical to memory, with a warm geomean ≥ 2× faster than base.
- An incremental `gbuild --disk` at univ 8 is no slower than base: op brackets add no cost.
- The full suite passes.

**Files:** `src/kvstore/format.rs` (new), `src/kvstore/pager.rs`, `src/kvstore/bptree.rs`, `src/kvstore/ivarray.rs`, `src/kvstore/store.rs`, `src/kvstore/disk_dict.rs`, `src/kvstore/mod.rs`, `src/kvstore/overflow.rs`, `src/kvstore/vlist.rs`, `src/kvstore/isarray.rs`, `examples/kv_bench.rs` (new), `tests/dt_disk.rs`, `changelog.d/phase6-R1-KV.md`.

### Round 2 — dictionary API, DiskTriples, bulk loader (merge order: R2-DISKLIB, R2-DICT, R2-BULK)

#### R2-DICT — out-of-core dictionary API (TermStr), bounded caches, batched materialization
**Brief.**
1. **`dict/mod.rs`, contract C4.**
   - Add `TermStr<'a>` (Deref<str>, AsRef, Display, Clone, `PartialEq<&str>`, `into_string`) and `Space` (reuse the existing `TermSpace` if it is equivalent).
   - Dictionary gains:
     - `term`, `pred`, `try_term`, `try_pred`, `try_entity_id`, `try_literal_id`, `try_predicate_id`;
     - `prefetch(&[ids], &[pids])`;
     - `resolve_many` (a sorted batch);
     - `for_each_term(space, from, f)`, `materialize()`, `id_limit(space)`, `cache_stats()`, `backing()`.
   - Redesign `DiskTermSource` as a full backend: `&self`, fallible, Send+Sync, with `lookup`, `resolve -> Arc<str>`, `intern` (default `Err`), `free` (default `Err`), `prefetch`, `for_each`, `count`, `id_limit`, `generation`, `sync`.
     - Option-returning lookups on a backed dictionary report errors through `note_read_error` (C2).
     - `try_intern_*` on a backed dictionary routes to `backing.intern`. This fixes today's silent intern into the empty in-memory interners.
     - `id_to_string`/`predicate_to_string -> Option<&str>` stay for in-memory dictionaries only. On a backed dictionary they return `None` plus a read error.
   - **Migrate every production call site to `term()`/`pred()`:** `query/engine.rs` (`resolve_string` and the others), `db/mod.rs`, `db/persist.rs` (`DiskOp::resolve`, redo/update logs, RocksDB term writes), `db/admin.rs`, `zipio.rs`, `reason/mod.rs`.
   - **Interner:** store each string once, as an `Arc<str>` shared by the forward map and the backward Vec. Hand-written serde keeps `dict.bin` **byte-identical**: keep `persisted_encoding_is_unchanged`, and add a mirror test in both directions covering holes and the free stack.
   - **Copy-on-write:** `Interner { data: Arc<InternerData> }`, mutated through `Arc::make_mut`. Interning an existing term, and other no-ops, never copy.
2. **`dict/cache.rs` (new).**
   - `TermCache`: 16 shards with CLOCK eviction, `(space,id) → Arc<str>`, byte budget defaulting to 64 MiB.
   - `KeyCache`: `(space,key) → id`, 16 MiB.
   - Both are invalidated when `backing.generation()` changes. This replaces the count-only check.
3. **`kvstore/disk_dict.rs`.** `DiskDict` implements the new trait over the shared pager, with the caches.
   - **No `Box::leak` anywhere.** Add a test that greps `src/dict` and `src/kvstore` for it.
   - `resolve` uses `IvArray::get_with`.
   - `prefetch` sorts the ids and walks directory pages sequentially.
   - It stays read-only: intern arrives in R3.
   - **Keep the `DiskDict` constructor signature used by `kvstore/store.rs` unchanged** (R2-DISKLIB owns that file).
4. **`query/engine.rs`.**
   - Materialize final rows in chunks of 4096 rows, with one sorted `prefetch` per chunk.
   - Memoize id→Value per query (bounded) for FILTER and ORDER BY.
   - Add `with_string_index(Arc<StringIndex>)`, injected instead of a per-query rebuild.
5. **`db/mod.rs`.** Cache the trigram `StringIndex` per `mutation_count` (`Mutex<Option<(gen, Arc<StringIndex>)>>`, cleared in `touched()`), for in-memory dictionaries only. Backed dictionaries run substring FILTERs without the prefilter, which is still correct.

**Invariants:** identical results and order on all corpora; `dict.bin` bytes unchanged; Dictionary stays Send+Sync and cheap to clone; checked bounds unchanged.

**Tests:**
- `TermStr` semantics.
- Mock-backend routing:
  - lookup, resolve and intern;
  - an injected error makes the query fail through the scope;
  - the legacy `&str` API on a backed dictionary returns `None` and records an error.
- `DiskDict` over a LUBM-1 kvstore.kv:
  - all ids round-trip;
  - resident bytes stay ≤ budget under 1M random lookups with a 1 MiB budget;
  - no growth over repeated runs.
- `dict.bin` compatibility in both directions.
- Copy-on-write clone isolation, including that interning an existing term keeps sharing.
- The `StringIndex` cache equals a per-query build and is invalidated by writes. Update `tests/string_index_planner.rs` if needed.
- The out-of-core `DiskStore::lazy_dictionary` query path matches memory answers on Q1–Q14.

**Benchmark:** gstore-bench reference at univ 8 and univ 38 (snapshot), plus `examples/kv_bench.rs` extended with a `dict-ooc` mode: `DiskStore::query` with the lazy dictionary.

**Acceptance:**
- Snapshot RSS after open ≤ 0.9 × pre at univ 38.
- Snapshot Q1–Q14 geomean within ±5% of main-before-branch.
- Out-of-core path at univ 38: resident dictionary bytes ≤ 64 MiB after Q1–Q14 × 3; Q6/Q13/Q14 ≤ 2 × the same queries with the in-memory dictionary.
- A second identical CONTAINS query (univ 8 snapshot) ≤ 0.25 × the first.
- The `dict.bin` hash is identical to pre-Phase-6 for the same input.

**Files:** `src/dict/mod.rs`, `src/dict/cache.rs` (new), `src/dict/prefix.rs`, `src/kvstore/disk_dict.rs`, `src/kvstore/string_index.rs`, `src/query/engine.rs`, `src/query/results.rs`, `src/query/functions.rs`, `src/db/mod.rs`, `src/db/persist.rs`, `src/db/admin.rs`, `src/zipio.rs`, `src/reason/mod.rs`, `src/analytics/csr.rs`, `src/pfn/graph.rs`, `src/bin/gexport.rs`, `examples/kv_bench.rs`, `tests/dt_dict_backends.rs` (new), `tests/string_index_planner.rs`, `tests/dt_csr.rs`, `tests/dt_gfun_e2e.rs`, `tests/dt_gfun_metric.rs`, `changelog.d/phase6-R2-DICT.md`.

#### R2-DISKLIB — exact transaction overlay + DiskTriples (the `Backend::Disk` payload), library level
**Brief.**
1. **`src/store/overlay.rs` (new).** `Overlay { added: TripleStore, removed: TripleStore }`.
   - Invariants: added ∩ base = ∅, removed ⊆ base, added ∩ removed = ∅.
   - `insert`/`remove(base, t)` return exact changed flags; `drain()` returns the changes sorted.
   - `OverlayView<'a, B: TripleSource>` implements **all** of `TripleSource`, including the C1 methods:
     - sorted two-way merges in contract order;
     - exact count arithmetic;
     - exact `distinct_*` and `num_predicates` via probes over the overlay's keys;
     - `iter_all` in (p,s,o) order;
     - an untouched-key fast path.
   - `pub mod overlay;` in `store/mod.rs`.
2. **`src/kvstore/disk_triples.rs` (new), contract C5.** `DiskTriples { store: DiskStore, shared: Arc<DiskShared{gate, epoch, closed}>, txn: Option<DiskTxn{overlay, named_ops}>, broken, dirty }`.
   - **Reads:** go through `DiskStore::try_*`. An `Err` calls `note_read_error` and returns an empty range, so the statement fails. Count, distinct and has overrides call the exact inherent methods. With a transaction open, reads use `OverlayView` (read-your-writes).
   - **Writes, `MutableStore`:**
     - auto-commit: take the writer gate, call `insert_ids`/`delete_ids` (op-bracketed), then `epoch += 1` and `dirty = true`;
     - inside a transaction: overlay only. **No triple reaches the pager before commit.**
   - **Transactions and lifecycle:**
     - `begin()`;
     - `commit()`: under the gate, apply named ops, then removals, then additions, then bump the epoch;
     - `rollback()`: never touches the pager;
     - `named_op(insert, g, t)`;
     - `commit_point(&self)`;
     - `Drop`: drain, set `closed`, best-effort flush.
   - **Broken state:** after a write or flush error, later writes are refused and reads note an error. `#[doc(hidden)] debug_fail_next_write()`.
   - **Gate:** a `Mutex<{readers, draining}>` plus a `Condvar` plus per-reader cancel flags. A writer sets `draining` (new readers get `Expired`), raises the readers' cancel flags (the evaluator stops at its next cancellation checkpoint), waits for zero readers, applies, bumps the epoch, and clears `draining`.
   - **`DiskReadView { pager, shared, begin_epoch }`:**
     - `with_base(|ds, cancel| ..)` registers as a reader and builds a `DiskStore::from_shared_pager` view;
     - it returns `Expired` if the gate is draining or the epoch moved (checked before and after), `Closed` after drop, and never re-enters.
   - `vstree_stale`/`mark_vstree_fresh` passthrough. Everything is Send+Sync. **Not wired into Database** (that is R3).
3. **`kvstore/store.rs`.** Only additive helpers that turn out to be missing.
4. **`examples/disk_stream_probe.rs`.** Open a kvstore.kv, wrap it in `DiskTriples` with an in-memory `DiskStore::dictionary()`, and run Q1–Q14 through the Evaluator over `DiskTriples` and over the materialized TripleStore. Report latencies and check equality.

**Tests:**
- Seeded overlay model test: random insert/remove against base+overlay vs a materialized TripleStore. Every method (counts, distinct, has) must match **in order**, and the flags must be equal.
- DiskTriples:
  - transaction writes never dirty the pager (page count and dirty set unchanged until commit);
  - commit applies, rollback leaves the pager untouched;
  - broken state;
  - `Drop` flushes;
  - a view returns `Expired` after any write and `Closed` after drop;
  - an injected read error reaches the scope.
- Threaded stress: 8 snapshot readers running 10 s queries plus a committer. The commit completes within 200 ms of cancellation; readers retry and see either the begin-time or the committed state, never a torn one.
- Read-error isolation across threads.

**Acceptance:**
- All tests pass.
- `disk_stream_probe` at univ 8 and univ 38: answers identical to memory; warm Q1–Q14 geomean ≤ 1.5 × the materialized path. If it is above that, list the hot spots in the fragment, since R3's default-mode decision depends on it.
- Overlay reads on untouched keys ≤ 1.1 × base cost.

**Files:** `src/store/overlay.rs` (new), `src/store/mod.rs` (mod and re-export lines only), `src/kvstore/disk_triples.rs` (new), `src/kvstore/mod.rs`, `src/kvstore/store.rs`, `examples/disk_stream_probe.rs` (new), `tests/dt_overlay.rs` (new), `changelog.d/phase6-R2-DISKLIB.md`.

#### R2-BULK — bulk index build for kvstore.kv (sorted, bottom-up, bounded memory, crash-safe install)
**Brief.** New module `src/bulk/`, std only, contract C6.
1. **`budget.rs`, `scratch.rs`, `extsort.rs`.**
   - A Budget with per-phase shares and peak accounting.
   - A lazy scratch directory, removed on drop.
   - External sort for fixed-size records (`[u32;3]` permutations, `[u32;4]` quads, `SigRec(limbs,id)`):
     - de-duplicated runs spilled under the budget;
     - k-way merge with fan-in ≤ min(64, share / 256 KiB);
     - an in-memory fast path that creates no files.
2. **`termmap.rs`, `encode.rs`.** A compact sharded term map (arena + offsets + ids + open addressing, about L+26 B per term).
   - **Sequential** first-seen id assignment in exactly `BuildAccumulator::add`'s order (s, p, o, then g), so ids equal the memory build's.
   - Enforce `checked_*_id`, the IvArray cap (33,554,432 per space) and the 4,081-byte key limit, with the existing error texts.
   - The term map is reported but sits outside the budget.
3. **`source.rs`, `pipeline.rs`, `stats.rs`.**
   - Parse with phase5 `parser::import` per source on up to `threads` threads, with ordered replay. ImportReport, the `parse_error.log` lines and the first strict-mode error must be byte-identical to sequential `import_sources`.
   - Stats taps: `triple_count`, distinct subjects/objects, per-predicate (num, dsub, dobj), quad count.
4. **`kvimage.rs`.** Page-image writers producing **V1 kvstore.kv** using the R1 registry constants:
   - fixed-key trees (SPO/POS/OSP 12 B, GSPO 16 B) built bottom-up at 90% fill, with the last leaves rebalanced, leaves linked, and internal levels;
   - variable-key `*2id` trees;
   - dense `id2*` IvArrays with overflow chains;
   - pred_stats, header roots and counters;
   - slack pages chained into the free list.
   Written without the cache or WAL into an unpublished staging file, with one fsync.
5. **`sigs.rs` + `signature/disk_vstree.rs` `DiskTree::build_sorted(path, sorted iter)`.** Signatures come from the merged SPO (out) and OSP (in) streams, OR-joined by id and sorted externally by (limbs, id). The output must be **byte-identical** to `DiskTree::build` for the same entries. Mark it fresh.
6. **`commit.rs`, `bulk::build`.** Target Disk; `RocksDb` returns "unsupported" until R3.
   - Hold the DirLock (refuse if another process holds it) and an in-process BUILDING set.
   - Stage into `<dir>/.bulk-tmp/`.
   - Commit sequence:
     1. COMMIT marker;
     2. durably move an existing `rocksdb/` and the snapshot files aside (`fsutil::discard_dir`);
     3. remove the stale WAL;
     4. rename kvstore.kv and vstree.kv into place;
     5. `sync_dir`;
     6. remove other layouts;
     7. delete `.bulk-tmp`.
   - Every crash point leaves either the old or the new database, openable by an unmodified binary. A strict parse failure leaves the old database untouched. Clean up a stale `.bulk-tmp` at the next build.
7. **CLI and config.**
   - `gbuild --disk` uses `bulk::build`. New flags: `--memory MB` (1024), `--threads N`, `--tmp-dir`, `--fill PCT`, and `--incremental` (the old path, for A/B).
   - After a build, gbuild prints counts and a phase/spill summary instead of reopening.
   - `config.rs` and `conf.ini.example`: `[system] build_memory_mb`, `build_threads`, `build_tmp_dir`, `build_fill_factor`; `build_multi_thread=off` means 1 thread.
   - `lib.rs` exposes `pub mod bulk`.
   - `Database::build_disk*` stays incremental until R3 delegates.

**Invariants:** output is readable by today's `DiskStore::open`; ids equal the memory build's; tolerant import semantics are unchanged; a partial build is never detectable as the current layout.

**Tests:**
- extsort vs std sort across budgets, threads and fan-in 2.
- Encoder ids == the sequential Dictionary's.
- Page writers read back through BTree, IvArray and `DiskStore::open`, and survive 10k random mutations.
- A bulk-built kvstore.kv is logically equal to `DiskStore::build_files` output (all trees, counters, IvArrays, pred_stats) on `testdata/lubm` plus an N-Quads mix.
- vstree.kv byte identity.
- Q1–Q14 rows **and order** identical.
- Bytes identical across thread counts.
- Crash failpoint matrix (8 points × prior layout none/snapshot/disk/rocksdb), a subprocess `kill -9` loop, and lock refusal.

**Benchmark:** gstore-bench reference with `--backends disk` at univ 8 and univ 38 (gbuild now uses the bulk path). Compare against `--gbuild-arg --incremental` and run a `--gbuild-arg --memory --gbuild-arg 128` variant.

**Acceptance:**
- Build ≤ 15 s at univ 8 and ≤ 60 s at univ 38.
- Peak RSS ≤ 1.25 GiB at univ 38 with the default budget; with `--memory 128` at univ 8, peak ≤ 400 MiB and at least one spill.
- kvstore.kv ≤ 0.75 × pre.
- Open and Q1–Q14 of a bulk-built database within ±10% of an incrementally built one.
- Identical answers.

**Files:** `src/bulk/**` (new), `src/signature/disk_vstree.rs`, `src/bin/gbuild.rs`, `src/config.rs`, `conf/conf.ini.example`, `src/lib.rs`, `src/db/dirlock.rs`, `tests/dt_bulk_disk.rs`, `tests/dt_bulk_import.rs`, `tests/dt_bulk_crash.rs` (new), `changelog.d/phase6-R2-BULK.md`.

### Round 3 — wiring and servers (merge order: R3-ROCKS, R3-SRV, R3-STREAM)

#### R3-STREAM — `Backend::Disk` Stream mode in Database: out-of-core triples and dictionary
**Brief.**
1. **`store/backend.rs`.** Add `Backend::Disk(kvstore::DiskTriples)`, `as_disk`/`as_disk_mut`, full dispatch including the C1 methods, and Disk handling in `take_write_error`. Code outside db/store uses `as_memory()` with an `iter_all` fallback, so it still compiles.
2. **`kvstore/disk_dict.rs`.** `intern(space, key)` for Stream mode: the next id (R4 adds free-list reuse), writing id2*, then *2id, then the count root, in **one op bracket** under the pager write lock shared with DiskTriples. Bump the generation and update the caches. Term-before-triple holds trivially, and a crash can only leave an orphan term.
3. **`db/open_options.rs` (new), contract C7.** `Database::open_with` and `disk_query_mode()`, re-exported from lib.rs.
4. **`db/persist.rs`.**
   - **`load_disk`, Stream mode:**
     1. `DiskStore::open` plus the legacy shadow reconcile;
     2. `Dictionary::from_backing(DiskDict)` on the shared pager;
     3. `named_to_memory`;
     4. vstree.kv only if it is fresh; never rebuilt at open unless `vstree_on_open`;
     5. `Backend::Disk`, with `db.disk = None`.
     Nothing proportional to the data is read except named graphs and the O(P) stats.
   - **`load_disk`, Materialize mode:** today's path, except that if `DiskStore::dictionary()` skipped an id, vstree.kv is treated as stale.
   - `build_disk*` delegates to `bulk::build`; the old bodies are kept as `#[doc(hidden)] *_incremental*`.
   - Disk branches in intern paths, infer, `persist_mutation` (named ops through `DiskTriples::named_op`), `commit_point`, `storage()`, `into_memory`, `save`/`sync`/`backup`/`copy_disk_files_to`.
   - `rebuild_index` for Disk: a streaming `for_each_entity_adjacency` pass, then `write_vstree_kv`, then mark it fresh. Inside a transaction it is deferred to commit.
5. **`db/mod.rs`.**
   - Writes on Disk go by id and call `touched()` exactly when something changed.
   - begin/commit/rollback use the DiskTriples transaction API: skip the default-graph `UndoOp` replay, keep the named-graph undo.
   - `clear_default` on Disk.
   - `read_as`, `run_update`, `csr` and export check the read scope and the broken state, and never cache an errored result.
   - `store()` is documented to panic for Disk.
6. **Config and CI.**
   - `config.rs`: a Rust-only `[storage]` section with `disk_query_mode`, `disk_cache_mb`, `dict_cache_mb`, `vstree_on_open` and `default_layout` (an Option, consumed by R4-SNAP); `GStoreConfig::open_options()`.
   - `conf.ini.example`.
   - `bin/gserver.rs` applies the options at startup and on refreshconf.
   - `.github/workflows/ci.yml`: run the gstore test job twice, with `GSTORE_DISK_QUERY_MODE=stream` and `=memory`.

**Invariants:** single id space; no triple reaches the pager before commit; `mutation_count` advances if and only if data changed; acknowledge only after durable; `FEATURES` stays 0, so Stream mode, Materialize mode and older binaries can take turns on the same directory; the VS-tree is used only while valid; errors fail the query and never yield a partial answer.

**Tests:**
- **`tests/dt_disk_stream.rs`:**
  - Build `testdata/lubm` plus an N-Quads file and open it in both modes with the same VS state. Compare results in order for:
    - LUBM Q1–Q14;
    - the dt_sparql11, dt_sparql_semantics and dt_sparql_syntax corpora;
    - LIMIT, OFFSET and top-k;
    - paths with both ends unbound, `?s ?p ?o LIMIT`;
    - `GRAPH ?g` and `GRAPH <g>`;
    - CONTAINS and REGEX;
    - DESCRIBE, CONSTRUCT and aggregates;
    - graph functions through the CSR.
  - Multiset equality against an in-memory build.
  - A seeded update workload against an in-memory oracle: INSERT, DELETE, DELETE WHERE, CLEAR, DROP, quads, LOAD and `reason_execute`. `mutation_count` must advance if and only if data changed. Reopen alternately in Stream, Materialize and Stream mode.
- **`dt_disk_stream_txn.rs`:** read-your-writes; rollback; commit followed by reopen; large transactions; named graphs; an injected commit failure leaves the database broken until reopen.
- **`dt_disk_stream_crash.rs`** (`mem::forget`):
  - forget after the i-th acknowledged UPDATE: the state equals the oracle at i;
  - an uncommitted transaction is absent;
  - a crash between a term and its triple leaves only an orphan term.
- dt_phase0_review, dt_persist_backends and dt_disk pass in both modes. Set the options explicitly rather than weakening assertions.

**Benchmark:** gstore-bench reference, `--backends disk`, with `--env GSTORE_DISK_QUERY_MODE=stream` vs `=memory`, on the same bulk-built directories at univ 8 and univ 38.

**Acceptance (univ 38):**
- RSS after open ≤ 120 MiB, and the RSS ratio univ 38 / univ 8 ≤ 1.5.
- Open warm ≤ 0.5 s, cold ≤ 2.0 s.
- VmHWM through Q1–Q14 ≤ 400 MiB.
- Each Q1–Q14 ≤ 2.0 × Materialize on the same directory; geomean ≤ 1.4×; Q1, Q3, Q4, Q11 and Q12 ≤ Materialize + 5 ms.
- Auto-commit single-triple INSERT DATA median ≤ Materialize.
- Identical answers.
- **Flip the default to Stream only if every gate passes.** Otherwise keep Materialize as the default and record the numbers.

**Files:** `src/store/backend.rs`, `src/kvstore/disk_dict.rs`, `src/kvstore/disk_triples.rs`, `src/kvstore/store.rs`, `src/kvstore/mod.rs`, `src/db/mod.rs`, `src/db/persist.rs`, `src/db/open_options.rs` (new), `src/db/admin.rs`, `src/lib.rs`, `src/config.rs`, `conf/conf.ini.example`, `src/bin/gserver.rs`, `.github/workflows/ci.yml`, `tests/dt_disk_stream.rs`, `tests/dt_disk_stream_txn.rs`, `tests/dt_disk_stream_crash.rs` (new), `tests/dt_disk.rs`, `tests/dt_persist_backends.rs`, `tests/dt_phase0_review.rs`, `tests/dt_phase0_wiring.rs`, `tests/dt_persist.rs`, `changelog.d/phase6-R3-STREAM.md`.

#### R3-SRV — parallel readers: RwLock servers, PFN metadata cache, lock-free monitoring
**Brief.**
1. **`server.rs`.** `ServerInner.db: RwLock<Database>` with poison-tolerant `read_locked`/`write_locked`.
   - `execute()` parses the `Statement` outside the lock. A parse error returns the same 400 and takes no lock.
   - Reads take a read guard and call `read_as`.
   - Updates take a write guard and run the code path unchanged (`mutation_count` before/after, dirty flag, check_point/closing save before release, partial-update note) through `execute_as`.
   - `status` takes a read guard. `with_load_policy`, `with_query_settings`, `finish_shutdown` and the test panic take a write guard.
2. **`http_api.rs` and gapi.**
   - **Lock wrapper:** `DbCell { slot: RwLock<DbSlot>, csr }` with `read()`, `write()` and `try_write()` returning `SlotRead`/`SlotWrite`. They are poison-tolerant, and a thread-local registry **in all builds** turns re-acquiring the same cell into an error instead of a deadlock.
   - **Settings sync:** `ApiInner::read_synced` upgrades once to sync stale query settings, then retries with a read guard.
   - **Statement plumbing:** `QueryJob` carries `Option<Statement>`, a single parse that replaces the lock-choice reparse in `is_update_sparql`. `DbSlot::read(&self)`; `lock_for_op_read` and `lock_for_op_write`.
   - **Site classification:**
     - shared: statement reads, transaction-owner reads, export, backup, `load_csr`, inspect/stat/monitor/status/show, reason Check;
     - exclusive: updates, begin/commit/rollback, checkpoint/save/`wrote`, upload, batch, unload/drop/restore/retire, shutdown, reason Execute/Disable, `apply_query_settings` via `try_write`.
   - The transaction contract is unchanged. Snapshot evaluation runs inside a `ReadScope`. Write the lock rules (C8) into the module docs.
3. **Lock-free monitoring.** Each `DbCell` publishes atomic counters (triples, entities, literals, predicates, generation, last save) at the end of every write section and after load. `/status`, `/monitor`, REST `/show` and `/api` monitor/stat read them without taking the db lock.
4. **`pfn/registry.rs`.** A per-user `Arc` cache of parsed `data.json`, invalidated under `data_lock` by every registry write, plus single-flight module loading.
5. **`concurrent.rs`.** Snapshot and VersionView queries run inside a `ReadScope`; update the docs.

**Tests:**
- Latch proof: a test function blocks on a 2-party barrier with a timeout, and two concurrent reads both succeed. Run it on single-db `/sparql`, REST `/sparql` and `/api` query.
- Monitoring answers within 1 s while a read holds the lock and while a write holds it.
- A settings change applies while readers are running.
- The re-entrancy guard returns an error.
- `tests/dt_http_durability.rs` and `tests/upstream_transaction.rs` pass unchanged.
- New `tests/dt_rw_server_stress.rs`:
  - 8 readers, 1 writer and transaction cycles;
  - a multi-op write is seen as 0 or 2 changes, never 1;
  - uncommitted or rolled-back data is never visible;
  - the acknowledged-write ledger equals the reopened state;
  - 120 s watchdog.
- New `tests/dt_rw_crash.rs` (unix): a real gserver in single-db and `--api` modes on snapshot, disk and rocksdb; SIGKILL under load; every acknowledged write is durable; 10 seeds.
- PFN: concurrent first resolves load the module once.

**Benchmark:** gstore-bench conc phase on a memory snapshot at univ 8, cache off, through `/sparql` and `/api`.

**Acceptance:**
- Q4 qps at 8 clients ≥ 2.5 × 1 client and ≥ 215 qps; Q1 ≥ 2.0×.
- Mean Q4 latency at 8 clients ≤ 45 ms. Same ratios through `/api`.
- Single-client latency within +5%.
- Mixed workload (8 readers plus 1 writer every 100 ms, check_point off): reader throughput ≥ 2.0 × the pre-Phase-6 read-only 8-client throughput. With check_point on, reader throughput ≥ pre.

**Files:** `src/server.rs`, `src/http_api.rs`, `src/gapi/mod.rs`, `src/gapi/ops_db.rs`, `src/gapi/ops_query.rs`, `src/gapi/ops_reason.rs`, `src/gapi/ops_lifecycle.rs`, `src/gapi/ops_sys.rs`, `src/gapi/tasks.rs`, `src/pfn/registry.rs`, `src/concurrent.rs`, `tests/dt_rw_server_stress.rs`, `tests/dt_rw_crash.rs` (new), `tests/dt_http_durability.rs`, `tests/upstream_transaction.rs`, `tests/dt_gapi_db.rs`, `tests/dt_gserver_lifecycle.rs`, `changelog.d/phase6-R3-SRV.md`.

#### R3-ROCKS — RocksDB: SST-ingest bulk build, bounded streaming reads, exact stats
**Brief.**
1. **`backend/rocks.rs`.**
   - Scan through raw iterators with `iterate_upper_bound` set to the prefix successor, decoding in place (no `Box<[u8]>` per key). Iterator status errors call `note_read_error` instead of a silent break.
   - Take `count_*`, `has_*` and `distinct_*` from the exact stats counters (`s|`, `o|`, `x|`, `y|`, `c|`); they must equal `len` exactly.
   - Keep an in-memory mirror of `c|`/`u|`/`v|`: loaded after journal replay, updated only after a successful WriteBatch, reloaded after `rollback_journal`.
   - Contract order without whole-range sorts: `so_by_p` from the `x|p` subject keys plus SPO seeks; `iter_all` in (p,s,o) order.
   - `RocksTuning`: shared LRU block cache plus whole-key bloom filters, on by default, with env `GSTORE_ROCKS_BLOCK_CACHE_MB`. These are options only (config keys arrive in R4). **Keep the RocksStore constructors used by `db/persist.rs` source-compatible.**
2. **`src/bulk/rocks_sink.rs`, plus `bulk/mod.rs` and `bulk/types.rs` wiring.** `BulkTarget::RocksDb` reuses the R2 pipeline.
   - `SstFileWriter` per CF, with each CF's own options, split at 256 MiB:
     - `spo`, `pos`, `osp`, `gspo`;
     - `dict` (`tag‖be32(id) → key`);
     - every stats tag, exact: O/P/S/T, `c/u/v|p`, `s|id`, `o|id`, `x|p|s` (through a PsCount re-sort), `y|p|o`; zero counters are absent.
   - Create a fresh store in `.bulk-tmp/rocksdb`, call `ingest_external_file_cf` with `move_files`, write meta `name` and then `layout="1"` last, flush, `sync_wal`, close, reopen and check the counters.
   - Install with `fsutil::install_staged_dir`, then remove the other layouts.
3. **`gbuild --rocksdb`.** Uses `bulk::build`; `--incremental` keeps the old path. `Database::build_rocksdb_*` library helpers stay on the per-triple path, documented as small-data helpers.

**Invariants:** layout `"1"`, the CF set and key encodings are unchanged; term-before-triple; journal semantics; stats exactness.

**Tests:**
- New `tests/dt_bulk_rocks.rs`:
  - every CF's key/value set is byte-equal to the incremental build;
  - the dt_rocksdb fingerprint holds on a bulk-built store;
  - updates after the build keep the stats exact;
  - a failed build leaves the old database intact.
- `tests/dt_rocksdb.rs`:
  - an unsorted fingerprint in order against TripleStore;
  - counts == len;
  - the mirror equals point gets across inserts, deletes, journal rollback and reopen;
  - an injected iterator error makes the query fail.

**Benchmark:** gstore-bench with `--features rocksdb`, `--backends rocksdb`, at univ 8 and univ 38.

**Acceptance:**
- Build ≤ 10 s at univ 8 and ≤ 45 s at univ 38.
- Peak RSS ≤ 1.25 GiB at univ 38.
- On-disk size ≤ 1.1 × pre.
- Q1–Q14 geomean ≤ 0.8 × the pre-Phase-6 RocksDB numbers.
- Open ≤ 10 s at univ 38.
- Identical answers.

**Files:** `src/backend/rocks.rs`, `src/bulk/mod.rs`, `src/bulk/types.rs`, `src/bulk/rocks_sink.rs` (new), `src/bin/gbuild.rs`, `tests/dt_rocksdb.rs`, `tests/dt_bulk_rocks.rs` (new), `changelog.d/phase6-R3-ROCKS.md`.

### Round 4 — reclamation, cheap snapshots, closure (merge order: R4-RECLAIM, R4-SNAP, then R4-CLOSURE rebased and measured)

#### R4-RECLAIM — immediate id reclamation on delete (all layouts) + CLEAR redo coverage
**Brief.**
1. **`dict/mod.rs`.** Inside `Arc<InternerData>`:
   - store `free` as a `BTreeSet<u32>` serialized as the same seq, so `dict.bin` bytes are unchanged;
   - **reuse the lowest free id first**, with a trailing-hole trim cascade;
   - a `retired` set (serde-skip);
   - `serde(from)` normalization: holes become free, trailing holes are trimmed, duplicates are rejected;
   - `TermId`, `retire`/`release`/`free_id`;
   - `from_slots(ent, lit, pred: Vec<Option<String>>)`, which keeps exact ids including holes;
   - `id_limits`, `take_allocations`.
2. **`db/named.rs` (new).** `NamedGraphs`: derefs to the map, serializes exactly as the map, keeps `node_refs`/`pred_refs` (+1 per graph key), prunes emptied graphs, `verify_refs()`.
3. **`db/reclaim.rs` (new).** Policy `IdReclaim { Immediate (default), Off }`.
   - **When:** after an applied delete (DELETE DATA/WHERE, quad deletes, CLEAR/DROP/MOVE/COPY, `remove_sources`, rollback of inserts).
   - **Which:** candidates {s, p, o, g} are probed with C1 `has_*` and the NamedGraphs refs.
   - **Sequence:** persist the free, then `dict.retire`, then `touched()`, then `release` once no pin older than the retire generation remains.
   - **Transactions:** frees are deferred to commit (one batch); terms minted by a transaction are released at rollback.
   - **Order:** frees always run **after** `record_redo` and `persist_mutation`.
4. **`db/mod.rs` and `db/persist.rs`.**
   - Wire every delete site.
   - `reclaim_unused` → `Result<usize>`: `Err` inside a transaction, bumps `mutation_count`, persists on every layout.
   - `open_rocksdb` and `DiskStore::dictionary()` use `from_slots`.
   - **CLEAR, DROP, MOVE and COPY write redo-log records**, which are missing today.
5. **Layout persistence.**
   - Snapshot: written at the next save.
   - Materialize disk: `DiskOp::Free{space, key}`.
   - **Stream disk:** `DiskTriples::free_ids` by id, under the writer gate (which drains snapshot readers), one op bracket per term:
     - delete the `*2id` entry and the IvArray slot;
     - insert into the `FREE_IDS` tree (slot 22), or trim the high-water mark;
     - update `FREE_COUNT`;
     - `enable_feature(FREE_IDS)` in the same flush;
     - bump the generation.
     - Defensive re-check (SPO/OSP prefixes plus pred_stats; NamedGraphs is authoritative for quads because GSPO is keyed graph-first): if the term is still referenced, refuse and mark the backend broken instead of corrupting.
   - Interning pops the lowest free id.
   - **Existing V1 files: reclamation stays Off** until `gcompact --enable-reclaim`. New databases (DiskStore create and bulk `kvimage`) start with `FREE_IDS` set.
   - RocksDB:
     - auto-commit: a `delete_terms` batch after the triple batch (a crash leaves only an orphan);
     - transaction: `commit_journal_with(frees)`, atomic with the journal clear;
     - frees for `take_replayed()` ids after open.
6. **VS-tree.** Freed entities stay in the tree (a superset). A reused id goes through the insert path, which upserts its exact signature through the R1 locator.
7. **Config and tools.**
   - `config.rs`: `[system] id_reclaim=on|off`, plus the R3 `RocksTuning` keys (`rocksdb_block_cache_mb`, `rocksdb_bloom`); `conf.ini.example`.
   - New `bin/gcompact.rs`: `--reclaim` runs `reclaim_unused` and saves; `--enable-reclaim` upgrades V1 to V2. Add its `[[bin]]` entry to `Cargo.toml`.
   - `gmonitor` shows live counts, id limits and free counts.
8. **`examples/reclaim_churn.rs`.** Data comes from `gstore-bench gen --workload lubm --scale 18`, split by University. Ten rounds of "delete university r, insert university 8+r", Immediate vs Off.

**Tests:**
- Dictionary unit tests: lowest-first reuse, trim, retire/release, `from_slots`, normalization, `dict.bin` bytes.
- NamedGraphs refs under random operations equal a recompute.
- Database tests:
  - only unreferenced terms are freed;
  - named-graph and graph-IRI references pin a term;
  - transaction deferral;
  - rollback frees terms the transaction minted;
  - Off reproduces today's behaviour;
  - a pin keeps an id from being reused.
- `tests/dt_reclaim.rs`: a seeded workload of ≥ 10K operations on memory, Stream disk, Materialize disk and RocksDB, against an Off oracle. After every operation: export and Q1–Q14 are equal, live counts equal the distinct referenced terms, `verify_integrity` passes. Reopen at random points.
- `tests/dt_reclaim_crash.rs`: the test re-executes itself as a child and is killed with SIGKILL, 20 iterations per layout. Afterwards integrity holds, acknowledged operations are present, and there are ≤ 4 orphans, which `reclaim_unused` removes.
- `tests/dt_reclaim_concurrency.rs`: ConcurrentDb readers hold the invariant `o = 'v-' + local(s)` under churn; an HTTP transaction with reusing inserts runs while 8 readers read.
- Fix order- and count-dependent assertions in other tests (see Files).

**Acceptance:**
- In every one of 10 churn rounds at univ 8, live counts equal the referenced terms exactly and stay within ±5% of round 0. The Off control is expected to grow to about 2.2× entities.
- High-water mark ≤ 1.05 × peak live.
- RSS ≤ 1.15 × round 0 (memory); kvstore.kv ≤ 1.2 × round 0 (Stream).
- Delete throughput, Immediate/Off: ≤ 1.10 on memory and disk, ≤ 1.20 on RocksDB.
- Fresh builds: build, open and Q1–Q14 within ±5%.
- Identical answers.

**Files:** `src/dict/mod.rs`, `src/db/mod.rs`, `src/db/persist.rs`, `src/db/reclaim.rs` (new), `src/db/named.rs` (new), `src/kvstore/store.rs`, `src/kvstore/disk_triples.rs`, `src/kvstore/disk_dict.rs`, `src/kvstore/format.rs`, `src/backend/rocks.rs`, `src/bulk/kvimage.rs`, `src/config.rs`, `conf/conf.ini.example`, `src/bin/gcompact.rs` (new), `src/bin/gmonitor.rs`, `Cargo.toml`, `src/lib.rs`, `examples/reclaim_churn.rs` (new), `tests/dt_reclaim.rs`, `tests/dt_reclaim_crash.rs`, `tests/dt_reclaim_concurrency.rs` (new), **all other pre-existing `tests/*.rs` except `tests/dt_http_durability.rs` and `tests/upstream_transaction.rs`**, `changelog.d/phase6-R4-RECLAIM.md`.

#### R4-SNAP — O(1) snapshots and serving disk databases
**Brief.**
1. **`store/mod.rs`.** `TripleStore { data: Arc<StoreData> }`, copy-on-write.
   - `StoreData` keeps today's fields in the same order with the same derives.
   - Serde is hand-written and delegates, so `store.bin`/`named.bin` bytes are identical; `Debug` is unchanged.
   - Each `&mut` method checks for a no-op on shared data first and copies nothing in that case; otherwise it calls `make_mut`.
   - Shrink a Vec when len < cap/4.
   - `#[doc(hidden)] shares_storage_with`.
2. **`http_api.rs`.**
   - **Snapshot store:** `ReadSnapshot.store = SnapStore { Mem(TripleStore), Disk(DiskReadView) }`. The Mem clone is O(1).
   - **`/begin` on a Stream database:** capture a `DiskReadView`, a dictionary handle sharing the backing, and a named-graph clone. No triples are copied.
   - **Snapshot queries:**
     - run inside `with_base`, with the gate's cancel flag wired into the evaluator and a `ReadScope`;
     - `Expired` re-runs the read on the live database under the slot read guard, re-checking `retired` and the transaction;
     - `Closed` maps to NotLoaded;
     - the snapshot CSR is keyed by epoch.
   - RocksDB `/begin` is unchanged (it materializes).
   - **Build layouts:** REST `/build` and `/api build` accept `layout=snapshot|disk|rocksdb` (disk and rocksdb go through `bulk::build`). The default is `[storage] default_layout` if set; otherwise `disk` if R3 shipped Stream as default, else `snapshot`.
   - `gbuild --layout` uses the same default; plain `gbuild` keeps working.
3. **`gapi/ops_db.rs`.** The build layout parameter; `show`, `stat` and `inspect` for Disk (out-of-core counts, cache stats).
4. **`concurrent.rs`.** `Snapshot::from_db` is O(1) for memory databases; Disk and RocksDB still materialize (documented). Update the cost-model docs.

**Tests:**
- New `tests/dt_cow_store.rs`: TripleStore copy-on-write isolation, no-ops never copy, golden bincode against a legacy mirror, a randomized differential with periodic clones, and a cross-thread read while the original mutates.
- `/begin` shares storage until the first tquery write.
- New `tests/dt_snapshot_disk.rs`:
  - a transaction owner inserts pairs through tquery while 8 readers query non-transactionally: readers always see the even begin-time count, never an uncommitted pair;
  - after commit the readers converge, exercising `Expired`;
  - a rollback is never visible;
  - `/commit` completes in ≤ 1 s while a 30 s snapshot query runs;
  - unloading during reads gives a clean error;
  - `/api build layout=disk`, then querying, gives the same answers as the snapshot layout.
- Run the contract tests in `dt_http_durability.rs` and `upstream_transaction.rs` over both the snapshot and the disk layout.

**Benchmark:** gstore-bench plus `/begin` timing at univ 38.

**Acceptance:**
- `/begin` ≤ 5 ms with RSS growth ≤ 4 MiB on memory, and ≤ 50 ms with ≤ 16 MiB on a Stream disk database.
- A ConcurrentDb 1-triple update using existing terms: ≥ 1.7× lower latency than main-before-branch.
- A disk database through gserver: Q4 qps at 8 clients ≥ 2.5 × 1 client.
- Memory Q1–Q14, build and insert throughput within ±5%.

**Files:** `src/store/mod.rs`, `src/http_api.rs`, `src/gapi/ops_db.rs`, `src/gapi/mod.rs`, `src/concurrent.rs`, `src/server.rs`, `src/bin/gbuild.rs`, `tests/dt_http_durability.rs`, `tests/upstream_transaction.rs`, `tests/dt_snapshot_disk.rs` (new), `tests/dt_cow_store.rs` (new), `changelog.d/phase6-R4-SNAP.md`.

#### R4-CLOSURE — WatDiv-like workload, final measurements, docs closure
**Brief.**
1. **WatDiv-like workload (`bench/`).**
   - Generator version `watdiv-like/1`: WatDiv prefixes; a simplified e-commerce/social schema; per-scale entity counts (User 1000s, Product 250s, ...); Zipf-distributed social edges; the fixed constants Product0, Role2, Country1, Country5, Language0 and ProductCategory2 are guaranteed to exist.
   - 20 templates, `bench/queries/watdiv/{L1-5,S1-7,F1-5,C1-3}.rq`, each with a `#mapping` header.
   - Deterministic instantiation by an ASK-walk of at most 32 tries.
   - Added to the smoke profile (scale 1) and the reference profile (scale 10).
   - Tests: determinism and a golden hash; composition within ±10%; every template parses and is non-empty; snapshot and disk row hashes are equal.
2. **Final runs. This package merges last:** rebase onto main after R4-RECLAIM and R4-SNAP have merged.
   - Run the reference profile on the final main:
     - LUBM univ 8 and univ 38, on snapshot, disk-Stream, disk-Materialize and rocksdb;
     - univ 82 (≈ lubm10M) on disk and snapshot, if RAM allows;
     - WatDiv-like scale 10.
   - Commit `bench/results/phase6-final-*.json`. Re-bless `smoke.json`, and tighten `thresholds.json` where the numbers allow.
3. **Docs.**
   - `docs/BENCHMARKS.md`: method, generators vs UBA/WatDiv, protocol, gates, bless, generated tables, comparison with upstream and its caveats.
   - README: a Performance section with a generated table between markers, plus the reproduce command.
   - CHANGELOG: a Phase 6 section folded from `changelog.d/*.md`. The integrator removes the fragments in the round-4 merge commit.
   - `docs/GAP_PLAN.md`: a §9 status table for phases 0–6 (rows 6a–6g), plus a Phase 6 paragraph judging G1–G7 by the numbers.
   - `docs/CPP_PARITY.md` fixes: line 47 (upstream frees ids at degree 0), VS-tree parity, load no longer materializes the store.
   - `docs/STORAGE_BACKEND.md`: Stream and Materialize modes, what stays in memory, the ordering contract, overlay and gate semantics, the format registry and compatibility matrix, the bulk build, id reclamation, RocksDB.
   - `docs/DESIGN.md` and `docs/REFACTOR_BACKLOG.md`: concurrency notes.

**Acceptance:**
- Every exit gate in §2 is reported with its measured value and PASS/FAIL. Any FAIL has a recorded reason and follow-up.
- `gstore-bench report` regenerates the README and CHANGELOG tables byte for byte from the committed JSON.
- The bench smoke CI job is green with the re-blessed baseline.
- The WatDiv-like tests pass.

**Files:** `bench/**`, `.github/workflows/bench.yml`, `docs/**`, `README.md`, `CHANGELOG.md`, `changelog.d/README.md`, `changelog.d/phase6-R4-CLOSURE.md`.

---

## 7. 共享文件所有权总表(按轮)

| File | R1 | R2 | R3 | R4 |
|---|---|---|---|---|
| src/store/mod.rs | CORE | DISKLIB (mod lines) | — | SNAP |
| src/store/backend.rs | CORE (new) | — | STREAM | — |
| src/store/read_scope.rs, overlay.rs | CORE / — | — / DISKLIB | — | — |
| src/dict/mod.rs | — | DICT | — | RECLAIM |
| src/db/mod.rs, persist.rs | CORE | DICT | STREAM | RECLAIM |
| src/query/engine.rs | CORE | DICT | — | — |
| src/query/optimizer/planner/candidates | CORE | — | — | — |
| src/signature/vstree.rs | CORE | — | — | — |
| src/signature/disk_vstree.rs | CORE | BULK | — | — |
| src/kvstore/pager, bptree, ivarray | KV | — | — | — |
| src/kvstore/format.rs | KV (new) | — | — | RECLAIM |
| src/kvstore/store.rs | KV | DISKLIB | STREAM | RECLAIM |
| src/kvstore/disk_dict.rs | KV (compile) | DICT | STREAM | RECLAIM |
| src/kvstore/disk_triples.rs | — | DISKLIB (new) | STREAM | RECLAIM |
| src/kvstore/mod.rs | KV | DISKLIB | STREAM | — |
| src/backend/rocks.rs | — | — | ROCKS | RECLAIM |
| src/bulk/** | — | BULK (new) | ROCKS (mod, types, rocks_sink) | RECLAIM (kvimage.rs) |
| src/http_api.rs, src/gapi/* | — | — | SRV | SNAP (http_api, ops_db, mod) |
| src/server.rs, src/concurrent.rs | — | — | SRV | SNAP |
| src/pfn/registry.rs | — | — | SRV | — |
| src/config.rs, conf/conf.ini.example | — | BULK | STREAM | RECLAIM |
| src/bin/gbuild.rs | — | BULK | ROCKS | SNAP |
| src/bin/gserver.rs | — | — | STREAM | — |
| src/lib.rs | CORE | BULK | STREAM | RECLAIM |
| Cargo.toml / Cargo.lock | BENCH | — | — | RECLAIM (Cargo.toml) |
| .github/workflows/ci.yml | BENCH | — | STREAM | — |
| .github/workflows/bench.yml, bench/** | BENCH | — | — | CLOSURE |
| docs/**, README.md, CHANGELOG.md | — | — | — | CLOSURE |

---

## 8. Deferred(不在 Phase 6 的 12 个包里)

| # | Item | Reason |
|---|---|---|
| D1 | Segment layout / disk format v2 (mmap'd immutable generations, toggle-delta log, compaction; the ambitious disk angle) | One-way migration on open, unsafe mmap with SIGBUS exposure, a chain of about 10 work packages, and headline numbers with no prototype behind them. Revisit as "disk format v2", gated on a B1+C1 prototype benchmark at LUBM-38. |
| D2 | Persistent copy-on-write structures, versioned read views, pager MVCC (the ambitious concurrency angle) | About 2k lines of custom data structures with unmeasured single-thread cost, an unsafe `'static` RocksDB snapshot, and no Disk MVCC design. Phase 6 reaches G4 with RwLock + Arc copy-on-write + the DiskReadView gate. |
| D3 | Parallel deterministic dictionary encoder and dictionary overflow segments in the bulk loader | Parsing is the bottleneck at the target scales. At about L+26 B per term the map fits in RAM up to LUBM-1000 (about 2.7 GB). It is reported in BulkReport instead of being budgeted. |
| D4 | RocksDB out-of-core dictionary (`term2id` CF, layout 2) and native RocksDB snapshots for `/begin` | Needs a layout bump plus migration, and an owned snapshot over `Arc<DB>` (unsafe lifetime extension). RocksDB open still loads the dictionary, which is why G3 targets ≤ 10 s. |
| D5 | Persistent trigram substring index (`lexindex.kv`) | Prefilter only; correctness does not depend on it. In-memory dictionaries get a per-generation cache in R2. No LUBM/WatDiv acceptance query uses substring filters. |
| D6 | Incrementally maintained paged VS-index (`vsindex.kv` with leaf pointers); VS-tree for RocksDB | vstree.kv is used while fresh and rebuilt by streaming on request. Upstream 1.4 has no VS-tree. Benchmark VS-tree on vs off first (R4-CLOSURE reports both). |
| D7 | IvArray capacity above 33.5M keys per space, and dictionary keys > 4 KB in `*2id` | Feature bits 1 and 2 are reserved. LUBM-1000 (about 22M entities, 11M literals) fits. Both fail loudly today. |
| D8 | Pipelined/morsel executor, zero-copy chunk visitor API, UNION de-materialization, adaptive candidate intersection | An engine-wide rewrite. The memory backend is already within the same order of magnitude as upstream on comparable queries. R1 takes the cheap plan-preserving wins (hasher, VS probe). This becomes the next engine item. |
| D9 | Space reclamation: pager vacuum and defragmentation, RocksDB compact-on-deletion, `delete_range` CLEAR, `compact_all` | Files only grow, but correctness and the id gates do not depend on reclaiming space. Range-delete CLEAR needs a journal/redo design. |
| D10 | Writer admission control, timed write locks, fail-fast try-locks like upstream | Changes client-visible behaviour and needs a config contract. R3 makes monitoring lock-free and reads parallel, which removes most of the starvation. |
| D11 | Named graphs on disk (a GSPO-backed `NamedSource`; `Evaluator.named` made generic) | Memory grows with the quad count, but LUBM and WatDiv have no named graphs. It needs an engine API change. |
| D12 | Crash atomicity of disk transactions larger than the page cache | Durable but not atomic, as today. It needs an undo journal or shadow paging in the pager. |
| D13 | Sharded or lock-free page cache | Measure contention after R3/R4 first; Arc pages and CLOCK already shorten the critical section. |
| D14 | Multi-writer MVCC transactions and meaningful isolation levels | The HTTP contract and tests assume a single writer (409). |
| D15 | Library `Database::build_rocksdb_*` delegating to SST ingest | gbuild and `/api build` use the bulk path. The library helpers stay per-triple small-data helpers, so test fixtures keep their behaviour. |

---

## 9. 主要风险与回退

| Risk | Mitigation and fallback |
|---|---|
| Stream-mode latency (G1d) | Materialize stays as a one-line switch (`[storage] disk_query_mode=memory`). R2-DISKLIB's probe example gives an early signal. Default flips only if every gate passes. |
| R2-DICT is cross-cutting (`TermStr`) | The legacy `&str` API remains for in-memory dictionaries, so tests and fixtures keep compiling. A backed dictionary fails closed. |
| Cross-file format collisions | One registry (§3.1). No other package adds magics or slots. |
| Id reuse vs snapshots | Memory snapshots own copy-on-write dictionary versions (R2). Disk snapshots are drained by the gate before any free. RocksDB `/begin` materializes. Pins exist for future shared backings. |
| Merge conflicts | Strict per-round file ownership (§7). Visibility exports needed by later rounds are pre-assigned to the earlier owner (`remove_other_layouts`, kvstore constants). |
| Benchmark noise and disk space | Heavy runs serialized with `flock /tmp/gstore-bench.lock`; base and branch measured in the same session; shared `.nt` cache; databases deleted after each run; ≥ 7 GiB free. |
| Large packages (R1-CORE, R2-BULK, R3-STREAM) | Each has a fallback: ship the correctness parts and record performance shortfalls (§5.7). R2-BULK may fall back to `DiskTree::build` over collected entries if streaming VS signatures run late, as long as it reports peak memory. |
