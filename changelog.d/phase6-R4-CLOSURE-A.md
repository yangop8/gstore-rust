# R4-CLOSURE-A — gstore-bench: WatDiv-like workload, run labels, mixed workload, gbuild summaries, report tables

Package R4-CLOSURE-A (the code half of plan §6 R4-CLOSURE, item 1, plus
every bench request addressed to R4-CLOSURE) of `docs/PHASE6_PLAN.md`,
Round 4. Base: `d702463` (Phases 0–5 and Phase 6 Rounds 1–3 merged).
Files: `bench/**` and `.github/workflows/bench.yml` only. The final runs,
the re-blessed smoke baseline and the docs are the closure stage's.

## Changelog

### gstore-bench: a second workload, labelled runs, the mixed workload, and documentation tables

- **WatDiv-like workload `watdiv-like/1`** (`bench/src/watdiv.rs`,
  `--workload watdiv`): a deterministic generator in the spirit of the
  Waterloo SPARQL Diversity Test Suite — WatDiv's prefixes (`wsdbm:`,
  `sorg:`, `gr:`, `rev:`, `foaf:`, `og:`, `mo:`, `gn:`, `dc:`) and entity
  classes in a simplified e-commerce / social schema. Scalable classes grow
  with the scale factor (User 1000·s, Product 250·s, Review 1500·s, Offer
  900·s, Purchase 1500·s, Retailer 12·s, Website 50·s), the others keep
  WatDiv's sizes (City 240, Country 25, Topic 250, SubGenre 145, …).
  Social edges (`wsdbm:friendOf`, `wsdbm:follows`) and most references
  pick their targets from a Zipf distribution with integer weights (so the
  output is identical on every platform; `Product0` is the most liked
  product). Scale 1 = 105,620 triples (golden FNV `70c039a88063c158`),
  scale 10 = about 1.06M; 71% are social edges, as in WatDiv. Every line is
  a distinct triple. The templates' fixed constants (`Product0`, `Role2`,
  `Country1`, `Country5`, `Language0`, `ProductCategory2`) always exist,
  and entity 0 of each class carries the triples that make every template
  answerable when its placeholder is bound to it. The update chunk is 100
  users beyond the data set's last one (disjoint from the data).
- **20 query templates** `bench/queries/watdiv/{L1-5,S1-7,F1-5,C1-3}.rq`
  (linear, star, snowflake, complex — WatDiv's basic testing shapes), each
  with a `#mapping vN wsdbm:Class uniform` header (`#mapping none` for the
  constant-free C1–C3). They are **bound by a deterministic ASK walk**: per
  template up to 32 candidate bindings from the template's own SplitMix64
  stream (uniform over the class; the last try is entity 0), and the first
  whose `ASK` is true against the generated data wins. The runner binds
  them in an `instantiate` child (an in-memory database) once per data file
  and caches the result next to it (`<stem>.queries.json`, keyed by the
  generator version, the parameters, the data hash and the templates;
  `--verify-data` re-runs the walk and fails on a difference). Results
  record each query's binding and tries. `gstore-bench queries --workload
  watdiv --scale N` prints the bound queries.
- **Profiles:** `smoke` (CI) measures the WatDiv-like workload at scale 1
  after LUBM univ 1, `reference` at scale 10 after LUBM, each into its own
  section of the result (`extra_workloads`). `--extra none|watdiv:10[,…]`
  sets the further workloads, `--workload watdiv` measures WatDiv alone
  (at the profile's scale for it), `--shrink K` divides the scalable
  classes (tests).
- **Result schema `gstore-bench/2`**, a strict superset of `/1` (every
  committed `/1` result and baseline loads, self-checks and compares
  unchanged; a test reads all of them): `extra_workloads`; a run `label`
  (`--label L` keys every run `backend@L`, so one backend measured twice —
  the disk layout with `--env GSTORE_DISK_QUERY_MODE=stream` and `=memory`
  — shares a file through `merge`, and an unlabelled baseline run is
  compared with each labelled run of its backend); the build's `summary`
  (gbuild's bulk-build report parsed from its output: phase times, memory
  budget and peak, term map, threads, spilled runs — disk and RocksDB
  builds); a query's `bindings`/`tries`; the `mixed` phase.
- **Gates:** each workload of a result is gated on its own against the
  baseline's workload of the same id (subjects `watdiv-like:snapshot/L1`).
  What the baseline lacks — a whole workload, a run, a query — is reported
  as `info.new` and **not gated** (so CI stays green when the smoke profile
  gains the WatDiv-like workload before `smoke.json` is re-blessed); a
  workload, run or phase the baseline has and the result lacks still fails.
  Retries name the workload (`RetryTarget { section, run, phase }`). A run
  key twice in one workload, or a workload twice in one result, fails the
  self-check.
- **Mixed-workload phase** (after the update phase; it writes): per
  endpoint (`/sparql`, `/api`) and `check_point` setting (off, on),
  `readers` closed-loop readers of one query (LUBM `q4`, WatDiv `F5`) while
  one writer sends a single-triple `INSERT DATA` every 100 ms; reader qps
  and latency, acknowledged writes and their latency, and an exact gate
  (every reader got the same answer — compared as a multiset of rows,
  because a write changes the counts the planner costs joins with and so
  the row order — and every write reported one changed triple). Profiles:
  reference 8 readers × 5 s per point, smoke 2 × 1 s, micro 2 × 0.25 s;
  `--no-mixed`, `--mixed-readers`, `--mixed-write-ms`, `--mixed-secs`,
  `--mixed-checkpoint off,on`, `--mixed-endpoints`, `--mixed-query`. It
  runs on every layout, so the disk layout's incremental commit can be
  compared with the snapshot layout's full save. It also takes 20 **`/api`
  `begin` samples** (each rolled back) on the loaded database: latency and
  the server process's RSS growth while the transaction's snapshot is held
  (G4's `/begin` row). This replaces R3-SRV's `examples/srv_mixed_load.rs`.
- **Database bytes** no longer count the disk layout's `LOCK` (it holds
  the builder's pid, whose digit count varied the size between identical
  builds) nor RocksDB's info logs (`rocksdb/LOG*`); both stay listed in
  `files`.
- **Reports:** `report --table overview|queries|build|update|conc|mixed|gates|full
  RESULT... [--compare BASE]...` prints one deterministic table over one or
  more results (a `workload` column; ratios against each result's
  baseline). `report --update DOC...` regenerates every table between
  `<!-- gstore-bench report ARGS -->` and `<!-- /gstore-bench report -->`
  from the committed JSON (ARGS are `report`'s own arguments), byte for
  byte; `report --check DOC...` exits 1 when one is stale. The bench
  workflow's smoke job runs `report --check README.md CHANGELOG.md`.

## GAP_PLAN status line

阶段 6:R4-CLOSURE-A — gstore-bench 增加 WatDiv-like 负载(watdiv-like/1,20 个模板,ASK 遍历确定性实例化;smoke 1 / reference 10)、结果格式 gstore-bench/2(运行标签、多负载分节、gbuild 摘要、混合读写负载与 /begin 点)、新增项只报告不判失败、可按标记逐字节再生成的报告表格 — 完成;最终测量与文档归 closure 阶段。

## Docs notes

- `docs/BENCHMARKS.md` (closure): the WatDiv-like workload — schema,
  scalable and fixed classes, Zipf references, the 20 templates and their
  shapes, the ASK walk (32 tries, entity-0 guarantee, the cache next to the
  data), and that it is **not** WatDiv's generator (no byte or answer
  compatibility with WatDiv data; only the shapes and the vocabulary
  follow it). Sizes: scale 1 105,620 triples, scale 10 ≈ 1.06M (see
  Measured numbers).
- `docs/BENCHMARKS.md`: the result schema `/2` (labels, `extra_workloads`,
  build summary, mixed phase, `/begin` point) and that the gates report new
  workloads / runs / queries instead of failing; the mixed phase's method
  (multiset bodies; it writes, so it runs last and a retried phase rebuilds
  the database).
- README / CHANGELOG generated tables: write the markers
  `<!-- gstore-bench report --table overview bench/results/phase6-final-lubm38.json --compare bench/results/pre-phase6-lubm38.json -->`
  … `<!-- /gstore-bench report -->` and run `gstore-bench report --update
  README.md CHANGELOG.md`; CI then checks them (`report --check`). Paths in
  a marker are relative to the workspace root.
- `bench/README.md` is updated here (commands, options, profiles, phases,
  WatDiv-like workload, schema, gates, reports, recording a reference with
  labels).

## Measured numbers

(to be filled)

## Requests/Not yet

Requests (outside this package's files):

1. **Integrator — remove `examples/srv_mixed_load.rs`** (R3-SRV request 5):
   the mixed phase covers it (same servers, same writer statement, 8
   readers + 1 writer every 100 ms, check_point off and on, `/sparql` and
   `/api`), on every layout and in the result JSON. Its doc comment is the
   only place that names it besides R3-SRV's fragment.
2. **Fragment name.** This package's brief says to write
   `changelog.d/phase6-R4-CLOSURE-A.md` (this file), while the file list of
   plan §6 R4-CLOSURE names `phase6-R4-CLOSURE.md`; the closure stage folds
   this fragment in with the others (and may write `phase6-R4-CLOSURE.md`
   for its own part).
3. **Closure stage — re-bless `bench/baselines/smoke.json`** on the idle
   machine with the new tool (`gstore-bench bless --profile smoke --to
   bench/baselines/smoke.json`): until then the smoke job reports the
   WatDiv-like workload, the mixed phase and the `/begin` point as new
   (`info.new`, not gated) and still gates LUBM univ 1 against the R1
   baseline. After the re-bless the baseline is `gstore-bench/2`, which a
   pre-R4 harness refuses (by design: it cannot read labels or sections).
4. **R4-SNAP / server owners (note, no change needed):** besides the
   constructors R1-BENCH listed (`Server::bind`, `shutdown_handle`,
   `serve_forever`, `ApiServer::bind`, `with_query_settings`,
   `QuerySettings`), the mixed phase uses `Server::with_db_dir`,
   `Server::with_check_point`, `ApiServer::with_check_point` and the `/api`
   `begin` / `rollback` operations (`TID` in the answer). All exist on
   `d702463` and on `p6/r4-snap` as of this package.

Not yet / notes:

- The final reference runs, the WatDiv-like scale 10 reference file, the
  README / CHANGELOG / `docs/BENCHMARKS.md` tables and the G1–G7 verdicts
  are the closure stage's (commands under Measured numbers).
- WatDiv-like templates bind uniformly only (`#mapping … uniform`); WatDiv's
  `zipfian` / `normal` mappings and its stress-testing query generator are
  not ported. A template's placeholder must be an entity of a `wsdbm:`
  class.
- The mixed phase's writer inserts new subjects with a predicate no query
  uses, so the answers do not change while it writes; a workload whose
  concurrency query would see those triples needs another
  `single_insert`.
- The `/begin` point measures the in-process server's RSS (the harness
  process); its client threads are idle while it samples, so the growth is
  the snapshot's.
