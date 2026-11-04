# 基准测试(gstore-bench)

> Phase 6(`docs/PHASE6_PLAN.md`)的测量记录:方法、数据生成器与 UBA / WatDiv 的区别、测量规程、闸门与阈值、基线的 bless 规则、最终结果、退出闸门 G1–G7 的判定,以及与上游 gStore 的对比和它的限制。命令与选项的完整说明在 [`bench/README.md`](../bench/README.md)。
>
> 数字的出处:标记 `<!-- gstore-bench report ARGS -->` … `<!-- /gstore-bench report -->` 之间的表格由 `gstore-bench report ARGS` 从提交的 `bench/results/*.json` 生成,不要手改(`gstore-bench report --update docs/BENCHMARKS.md` 重新生成,`--check` 校验);其余数字或者出自这些文件,或者出自某个工作包的"安静测量阶段"(quiet stage:空闲的参考机,同一会话里测基线与分支),逐处注明。各包的测量记录已并入 `CHANGELOG.md` 的 Phase 6 一节。

## 1. 方法

`gstore-bench`(工作区成员 `bench/`,R1-BENCH 引入,R4-CLOSURE-A 扩展)是 Phase 6 唯一的测量工具:每轮的验收比值、CI 闸门和本文的表格都出自它。它只经真实的命令行工具和公开 API 测量:

- **每一步一个新的子进程**(`gstore-bench __child`:标准输入一个 JSON 请求,标准输出恰好一行 `GSTORE_BENCH_RESULT {json}`,每阶段有超时;崩溃或超时使整次运行以退出码 2 失败,从不算通过)。
- **build**:真实的 `gbuild`(`gbuild`、`gbuild --disk`、`gbuild --rocksdb`,命令行只在 `bench/src/runner/backends.rs` 一处),记墙钟时间、`gbuild` 的峰值 RSS(`getrusage(RUSAGE_CHILDREN)`)、库目录的字节数(不计解析日志、磁盘布局的 `LOCK` 和 RocksDB 的 `rocksdb/LOG*`,它们在相同的两次构建之间也会不同),以及批量构建的摘要(各阶段耗时、内存预算与峰值、词表、线程、溢出的 run)。
- **open**:`Database::open` 一次(给了 `--drop-caches` 时先清页缓存,即"冷"打开),再在新进程里打开 `--open-reps` 次,取中位数为"热"打开;记打开后的 RSS 与词计数,`triple_num` 必须等于生成器的去重三元组数。
- **query**:在 `--query-procs` 个新进程里各自打开、关掉查询缓存,每条查询先跑一次、再热跑 `reps` 次;每条查询取**各进程热中位数的最小值**。要多个进程,是因为每个进程的哈希种子和堆布局不同:在 Phase 6 之前的引擎上,univ 8 的某些查询在一个进程里能比下一个进程慢约 1.5 倍,单进程中位数让 Q1–Q14 几何均值在两次运行之间漂移 ±10%(R1-BENCH 的记录)。每次运行、每个进程、每个后端的行数和行哈希(`rows_fnv64`,与行序无关的多重集哈希)都必须相同。
- **conc**:进程内的单库 `Server`(`GET /sparql`,TSV)和多库 `ApiServer`(`GET /api?operation=query`,JSON),闭环 keep-alive 客户端,qps、平均与 p95 延迟;同一端点同一查询的所有响应体必须相同。
- **update**(会写,排在后面):单条 `INSERT DATA` / `DELETE DATA`(每条必须恰好改动一个三元组)、一个与数据不相交的更新块的插入与删除(`triple_num` 精确变化)、之后重跑全部查询(答案必须不变)、checkpoint、重新打开。
- **mixed**(也会写):每个端点(`/sparql`、`/api`)和 `check_point` 设置(off、on)下,若干闭环读者跑一条查询(LUBM `q4`,WatDiv-like `F5`),同时一个写者每 100 ms 发一条单三元组 `INSERT DATA`;读者 qps 与延迟、已确认的写及其延迟。每个读者拿到的答案必须相同(按行的多重集比较:写入会改变规划器用来估计连接代价的计数,从而改变行序),每次写都必须报告改动一个三元组。同一阶段还取 20 个 `/api begin` 样本(每个都回滚):延迟,以及持有事务快照时服务器进程的 RSS 增长(G4 的 `/begin` 一行)。
- **校准**(`calib/1`:排序 2 Mi 个 `u64`、1 Mi 键的 `HashMap`、格式化 500 Ki 个 IRI,各取五次的最小值,外加一次 fsync 探测)最先运行,也在子进程里;计时闸门用它归一化。

结果文件是 `gstore-bench/2` 格式的 JSON(`bench/src/schema.rs`;所有 `gstore-bench/1` 文件按 `/2` 读入,缺少较新的字段):环境、校准、负载(生成器版本、数据哈希、查询文本哈希、WatDiv-like 模板的绑定、更新块),以及每次运行的 build、open、query、内存、update、conc、mixed 数据与测量它的二进制和选项。同一后端测两次(例如磁盘布局的两种查询模式)用 `--label`:运行的键变成 `backend@label`(`disk@stream`、`disk@materialize`),再用 `gstore-bench merge` 合并成一个文件。

## 2. 数据生成器(与 UBA / WatDiv 的区别)

**LUBM-like**(`bench/src/lubm.rs`,版本 `lubm-like/1`)是设计阶段 Python 原型 `gen_lubm.py` 的 Rust 移植(原型不在仓库里):UBA 的基数、`testdata/lubm/lubm.nt` 的 univ-bench IRI、UBA 会写出的重复 `rdf:type ub:University` 行,并保证 `testdata/lubm/lubm_q*.rq` 的全部 14 条查询在任何规模下都非空(查询文本原样收录,哈希固定)。每所大学、每个系一个 SplitMix64 流。**它不是 UBA**:数据既不与 UBA 的输出逐字节一致,也不与 Python 原型的输出一致;`docs/PHASE6_PLAN.md` §0 的 eaf5320 基线(用 Python 生成器测得)因此只说明量级,Phase 6 的每个验收比值都对 `bench/results/pre-phase6-lubm{8,38}.json` 计算——那是 R1-BENCH 用本工具在 Phase 6 之前的代码上、在空闲的参考机上记录的。规模(seed 0,出自各结果文件的 `workload` 字段与 `bench/README.md`):

| 规模 | 系 | 行数 | 去重三元组 | 数据 fnv64 |
|---|---:|---:|---:|---|
| univ 1(`smoke`) | 21 | 143,930 | 139,905 | — |
| univ 8 | 156 | 1,064,650 | 1,029,275 | `ed44da2e7b3bbb98` |
| univ 38 | 779 | 5,363,690 | 5,181,103 | `fcd6493725f3c66a` |
| univ 82(约等于上游的 lubm10M) | — | 11,527,626 | 11,135,159 | `bd6f9eb8d771a130` |

更新块是 `University<univ>` 的 Department0(去掉大学的类型行),与数据集不相交。

**WatDiv-like**(`bench/src/watdiv.rs`,版本 `watdiv-like/1`,R4-CLOSURE-A)在词汇、模式形状和查询形状上仿照 Waterloo SPARQL Diversity Test Suite(WatDiv),**但它是自己的生成器**:数据不是 WatDiv 的,和 WatDiv 的数据与答案都没有逐字节或逐条的兼容性。前缀用 WatDiv 的(`wsdbm:`、`sorg:`、`gr:`、`rev:`、`foaf:`、`og:`、`mo:`、`gn:`、`dc:`);可伸缩的类随规模 s 增长(User 1000·s、Product 250·s、Review 1500·s、Offer 900·s、Purchase 1500·s、Retailer 12·s、Website 50·s),其余保持 WatDiv 的大小(City 240、Country 25、Topic 250、SubGenre 145 等)。社交边(`wsdbm:friendOf`、`wsdbm:follows`)和大多数引用按整数权重的 Zipf 分布选目标,输出在任何平台上都一样。模板里的固定常量(`Product0`、`Role2`、`Country1`、`Country5`、`Language0`、`ProductCategory2`)一定存在,每个类的 0 号实体带着让每个模板在占位符绑定到它时可答的三元组。规模 1 为 105,620 个三元组(golden 哈希 `70c039a88063c158`),规模 10 为 995,333 个(`19b2d2742cc622ad`)。

20 个模板 `bench/queries/watdiv/{L1-5,S1-7,F1-5,C1-3}.rq`(线形、星形、雪花形、复杂形,对应 WatDiv 的基本测试集),每个以 `#mapping vN wsdbm:Class uniform` 头说明占位符(C1–C3 是 `#mapping none`)。**ASK 遍历**确定性地绑定模板:每个模板从它自己的 SplitMix64 流取至多 32 个候选绑定(在类中均匀取;第 32 个是 0 号实体),第一个让 `ASK` 形式为真的候选胜出。绑定结果缓存在数据文件旁(`<stem>.queries.json`),也记进结果文件(每条查询的 `bindings` 与 `tries`)。只移植了 `uniform` 映射;WatDiv 的 `zipfian` / `normal` 映射和压力测试查询生成器没有移植。

## 3. 测量规程

- **参考机**:4 vCPU(Intel Xeon @ 2.10GHz)、16 GB 的虚拟机。`docs/PHASE6_PLAN.md` §2 的闸门都在这台机器上、"没有其它重任务运行"时测量。
- **安静的机器**:最终测量(R4-CLOSURE 的安静阶段,phase6 `769c627`,2026-11-04)期间除测量外没有任何任务运行;`vmstat` 每 15 s 采样一次,univ 8 / 38 / WatDiv 运行期间 CPU 平均忙 45%(8 客户端并发点上可达 90%),univ 82 运行期间 31%,steal ≤ 2%,校准分数 67–79 ms。每个结果文件记录自己的校准分数。
- **串行化**:每个重的命令都在 `flock /tmp/gstore-bench.lock` 下运行,多个会话的测量从不重叠。生成的 `.nt` 文件经 `/tmp/gstore-bench-cache/`(`$GSTORE_BENCH_CACHE`)共享,带旁注文件(生成器版本、参数、大小、哈希);`--verify-data` 重新哈希缓存的数据文件、重跑缓存的 WatDiv-like 绑定的 ASK 遍历。
- **冷打开**:`--drop-caches`(需要 root)在第一次打开前清掉页缓存,"open first" 一列因此是冷打开。
- **重复次数**:reference 配置每条查询 5 个进程 × 5 次热运行,取各进程热中位数的最小值;打开 1 次冷 + 若干次热(取中位数);并发每点 5 s;混合负载 8 个读者 × 5 s 每点。
- **A/B**:每个包测自己的验收时,基线二进制与分支二进制**在同一会话里**交替运行;跨会话只比较对 `pre-phase6` 文件的比值。
- **磁盘**:测完立即删库(harness 在每个后端之后删除),至少保留 7 GiB 空闲;univ 82 的数据(2.04 GB 的 N-Triples)在测量后删除。
- **构建**:`cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench`;RocksDB 用 `--features gstore/rocksdb,gstore-bench/rocksdb` 构建在单独的 `CARGO_TARGET_DIR` 里,再用 `merge` 合并结果。复现命令见 [§9](#9-复现)。

## 4. 闸门与阈值(CI)

`gstore-bench check` / `compare` / `ci` 按 `bench/thresholds.json`(`gstore-bench-gates/1`)判定一个结果相对基线是否通过:

- **精确闸门**(从不重试):生成器版本与数据哈希、查询文本哈希(也对固定值)、`triple_num == unique_triples`、每条查询的行数与行哈希等于基线且跨后端相同、跨运行稳定且非空、更新往返、并发响应体相同、混合负载的答案相同与写入确认、基线里的每个后端和阶段都在。
- **计时**(单侧,按校准归一化:测得时间除以校准比,校准比截在 [0.5, 2]):单条查询 `(m + 2 ms) / (b + 2 ms) > 3.0` 失败;每个后端热中位数的几何均值 `> 1.5` 失败(只计基线中位数 ≥ 1 ms 的查询,不足三条时计全部);构建 `> 2.0`(磁盘 `> 2.5`,下限 100 ms);热打开 `> 2.5`(下限 20 ms)。
- **内存**:`(m + 32 MiB) / (b + 32 MiB) > 1.5` 失败(构建峰值、打开后、查询高水位)。
- **大小**:库字节数 `> 1.10 ×` 失败。在 Phase 6 收尾之前是 1.25;字节数是确定的(最终的三次 CI 运行里每个对象都恰为 1.00),所以收紧不需要噪声余量。
- 并发与混合负载的吞吐、`/begin` 延迟和更新时间只报告(`info.*`),不判失败。
- **新的条目只报告**(`info.new`):基线没有的负载、运行或查询不判失败;基线有而结果缺的仍然失败。每个负载对基线中同 id 的负载单独判定;未加标签的基线运行与它的后端的每个带标签运行比较(`disk@stream` 与 `disk@materialize` 都对 pre-Phase-6 的 `disk` 判定)。
- **重试**:计时或内存闸门失败时只重跑失败的阶段,至多 2 次,取中位数的最小值;重跑的 open / query 阶段先重建被 update / mixed 阶段改过的库。
- **金丝雀**:`--self-test-slowdown 3` 把结果自身的每个时间乘 3 再对自己判定,必须失败(退出码 1)。`bench/tests/gates.rs` 固定了闸门的灵敏度:均匀慢 1.3 倍通过、1.6 倍失败,一条 ≥ 20 ms 的查询慢 4 倍失败,内存 1.32 倍通过,行哈希改变不经重试直接失败。

CI(`.github/workflows/bench.yml`):`smoke` 任务(push 与 PR)做 release 构建、文档表格检查 `report --check README.md CHANGELOG.md`、`gstore-bench ci`(LUBM-like univ 1 与 WatDiv-like 规模 1,快照与磁盘布局,对 `bench/baselines/smoke.json`)和金丝雀;`rocksdb-correctness` 任务跑 `cargo test -p gstore-bench --features rocksdb`;`reference` 任务(手动或每周)只报告。`ci.yml` 的 test 任务也跑 `report --check README.md CHANGELOG.md`(bench.yml 忽略只改 Markdown 的提交)。

## 5. 基线与 bless 规则

- `bench/baselines/smoke.json` 是 CI 的基线;`bench/results/pre-phase6-lubm{8,38}.json` 是 Phase 6 每个验收比值的参照,**不再改写**。
- **何时重新 bless `smoke.json`**:一个改动让结果快了 ≥ 1.5 倍,或者有意改变了格式 / 库大小(计划 §5.6(d))。bless 只从一次通过的结果做(`gstore-bench bless --from RESULT`),拒绝没通过自身精确检查的结果;在忙的机器上记录的基线标为 `"provisional": true`。
- **Phase 6 收尾时的 bless**(R4-CLOSURE 安静阶段):对第 2 轮的基线(`0c9bdb6`)跑两次 `ci`,均 PASS(307 个闸门,"1 new workload(s) not in the baseline",即 WatDiv-like 一节);磁盘布局的查询几何均值是基线的 0.59 / 0.60(快 1.5 倍以上),新负载、混合负载和 `/begin` 点都还没被判定,所以用第一次通过的结果(校准 72.1 ms)重新 bless(提交 `5faf215`)。新基线是 `gstore-bench/2`,含 LUBM-like univ 1 与 WatDiv-like 规模 1、快照与磁盘布局、混合负载与 `/api begin` 点;R4 之前的 harness 读不了它(有意为之)。之后对新基线 `ci` 四次 PASS(456 个闸门),金丝雀退出码 1。
- **阈值只在测得的波动允许时收紧**:空闲机上三次运行的最大闸门比值是几何均值 1.42(WatDiv-like 快照的一次运行)、单条查询 1.41、热打开 1.28、构建 1.17、内存 1.07、大小恰为 1.00。所以只把大小闸门从 1.25 收紧到 1.10(提交 `85686e5`),计时和内存闸门不变(它们还被 `bench/tests/gates.rs` 的上述灵敏度固定)。等有 CI runner 上的波动数据再考虑计时闸门。

## 6. 最终结果

R4-CLOSURE 的最终参考运行:reference 配置,`--drop-caches --verify-data`;univ 8 与 38、WatDiv-like 规模 10 在 phase6 `769c627`(第 1–4 轮全部合并、评审修复、R4-RECLAIM / R4-SNAP / R4-VS 的安静阶段之后)上测量,univ 82 用 `85686e5` 重建的 harness 测量(gstore 代码相同)。每个文件里四种运行:快照布局(`snapshot`)、磁盘布局的 Stream 模式(`disk@stream`)与 Materialize 模式(`disk@materialize`)、RocksDB(univ 82 不含 RocksDB,G7 不需要它)。"vs base" 一列是对 `pre-phase6` 同规模文件的比值,磁盘布局的两种模式都对 pre-Phase-6 的 `disk` 运行。

### 6.1 LUBM-like univ 8 与 univ 38:总览

Phase 6 之前(`bench/results/pre-phase6-lubm{8,38}.json`):

<!-- gstore-bench report --table overview bench/results/pre-phase6-lubm8.json bench/results/pre-phase6-lubm38.json -->
| workload | backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| lubm-like univ 8 | snapshot | 15.32 | 154.2 | 85.5 | 335.6 (cold) | 292.1 | 125.8 | 152.5 | 7.91 |
| lubm-like univ 8 | disk | 263.74 | 164.7 | 175.0 | 2240.5 (cold) | 1447.3 | 129.7 | 167.5 | 18.99 |
| lubm-like univ 8 | rocksdb | 53.93 | 285.5 | 41.2 | 9958.8 (cold) | 9988.7 | 145.8 | 184.9 | 43.27 |
| lubm-like univ 38 | snapshot | 76.82 | 741.0 | 430.8 | 1588.4 (cold) | 1464.6 | 596.4 | 710.3 | 46.65 |
| lubm-like univ 38 | disk | 1627.59 | 738.1 | 889.8 | 12387.4 (cold) | 7673.5 | 544.0 | 740.6 | 146.92 |
| lubm-like univ 38 | rocksdb | 277.76 | 634.2 | 206.3 | 60810.7 (cold) | 59389.4 | 522.0 | 663.3 | 311.50 |
<!-- /gstore-bench report -->

Phase 6 结束时(`bench/results/phase6-final-lubm{8,38}.json`):

<!-- gstore-bench report --table overview bench/results/phase6-final-lubm8.json bench/results/phase6-final-lubm38.json --compare bench/results/pre-phase6-lubm8.json --compare bench/results/pre-phase6-lubm38.json -->
| workload | backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms | build vs base | open vs base | geomean vs base |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| lubm-like univ 8 | snapshot | 5.41 | 112.8 | 85.5 | 233.6 (cold) | 202.6 | 102.2 | 128.4 | 2.16 | 0.35× | 0.69× | 0.27× |
| lubm-like univ 8 | disk@materialize | 3.55 | 108.4 | 130.2 | 305.8 (cold) | 267.2 | 89.1 | 115.5 | 2.05 | 0.01× | 0.18× | 0.11× |
| lubm-like univ 8 | disk@stream | 3.65 | 96.9 | 130.2 | 4.1 (cold) | 0.8 | 6.0 | 66.0 | 3.06 | 0.01× | 0.00× | 0.16× |
| lubm-like univ 8 | rocksdb | 4.31 | 92.6 | 31.1 | 164.4 (cold) | 144.1 | 66.4 | 102.0 | 5.16 | 0.08× | 0.01× | 0.12× |
| lubm-like univ 38 | snapshot | 29.60 | 550.1 | 430.8 | 1184.8 (cold) | 1033.0 | 489.1 | 601.0 | 10.15 | 0.39× | 0.71× | 0.22× |
| lubm-like univ 38 | disk@materialize | 21.06 | 400.3 | 654.0 | 1817.3 (cold) | 1720.8 | 351.0 | 457.1 | 7.83 | 0.01× | 0.22× | 0.05× |
| lubm-like univ 38 | disk@stream | 18.38 | 398.2 | 654.0 | 7.5 (cold) | 0.9 | 6.1 | 207.3 | 11.55 | 0.01× | 0.00× | 0.08× |
| lubm-like univ 38 | rocksdb | 19.50 | 401.6 | 155.7 | 738.9 (cold) | 678.0 | 270.8 | 436.2 | 25.00 | 0.07× | 0.01× | 0.08× |
<!-- /gstore-bench report -->

`phase6-final-lubm{8,38}-compare.md` 是 `gstore-bench compare` 对 pre-Phase-6 的完整闸门表(各 373 个闸门,PASS);`-compare-r3.md` 对第 3 轮(各 365 个,PASS);`-compare-r3-stream.md` 是 Stream 运行单独对第 3 轮的 Stream 运行(各 106 个,PASS;由最终文件去掉其它运行后比较,没有改动任何数字)。

### 6.2 univ 38:每条查询

热中位数(ms),"vs base" 是对 pre-Phase-6 同一后端的比值(按校准归一化之前的原始值):

<!-- gstore-bench report --table queries bench/results/phase6-final-lubm38.json --compare bench/results/pre-phase6-lubm38.json -->
| query | rows | snapshot first ms | snapshot median ms | snapshot vs base | disk@materialize first ms | disk@materialize median ms | disk@materialize vs base | disk@stream first ms | disk@stream median ms | disk@stream vs base | rocksdb first ms | rocksdb median ms | rocksdb vs base |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.19 | 0.01 | 0.01× | 0.17 | 0.01 | 0.00× | 0.31 | 0.02 | 0.00× | 0.35 | 0.04 | 0.00× |
| q2 | 115 | 39.03 | 37.56 | 0.74× | 32.84 | 28.64 | 0.15× | 85.32 | 56.64 | 0.30× | 580.79 | 585.74 | 0.80× |
| q3 | 10 | 0.09 | 0.01 | 0.01× | 0.09 | 0.01 | 0.00× | 0.17 | 0.02 | 0.00× | 0.32 | 0.07 | 0.00× |
| q4 | 9 | 0.10 | 0.05 | 0.00× | 0.10 | 0.05 | 0.00× | 0.20 | 0.07 | 0.00× | 0.36 | 0.26 | 0.00× |
| q5 | 576 | 265.79 | 234.72 | 0.70× | 264.95 | 232.52 | 0.37× | 328.85 | 274.10 | 0.44× | 395.86 | 353.87 | 0.66× |
| q6 | 404399 | 82.44 | 80.77 | 0.43× | 92.64 | 91.93 | 0.18× | 265.73 | 194.76 | 0.38× | 224.42 | 232.11 | 0.50× |
| q7 | 23 | 277.73 | 256.74 | 0.64× | 251.28 | 256.17 | 0.22× | 368.96 | 278.73 | 0.24× | 435.28 | 394.93 | 0.40× |
| q8 | 10778 | 238.14 | 244.63 | 0.67× | 215.27 | 274.91 | 0.32× | 253.72 | 256.85 | 0.29× | 384.88 | 421.28 | 0.47× |
| q9 | 5507 | 333.54 | 343.83 | 0.65× | 329.93 | 347.49 | 0.24× | 515.48 | 521.65 | 0.36× | 721.98 | 742.08 | 0.35× |
| q10 | 3 | 220.69 | 231.74 | 0.71× | 224.12 | 232.61 | 0.33× | 236.31 | 219.28 | 0.31× | 369.32 | 400.01 | 0.67× |
| q11 | 325 | 2.70 | 1.31 | 1.20× | 0.50 | 0.17 | 0.12× | 1.20 | 0.26 | 0.17× | 11.57 | 2.01 | 0.12× |
| q12 | 21 | 1.05 | 0.49 | 0.84× | 0.17 | 0.08 | 0.11× | 1.15 | 0.17 | 0.23× | 0.44 | 0.22 | 0.02× |
| q13 | 432512 | 100.11 | 96.02 | 0.43× | 105.42 | 107.74 | 0.12× | 217.72 | 215.91 | 0.24× | 247.34 | 257.25 | 0.50× |
| q14 | 305503 | 62.49 | 62.22 | 0.47× | 71.71 | 72.48 | 0.23× | 151.63 | 155.96 | 0.49× | 187.78 | 208.59 | 0.55× |
<!-- /gstore-bench report -->

### 6.3 univ 38:批量构建、更新

`gbuild --disk` / `--rocksdb` 的批量构建摘要(默认 1,024 MiB 预算,4 线程):

<!-- gstore-bench report --table build bench/results/phase6-final-lubm38.json -->
| workload | backend | gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |
|---|---|---|---:|---:|---:|---:|---:|---:|
| lubm-like univ 38 | disk@materialize | parse+encode 18861, spo 721, index 922, pos 472, osp+vstree 921, dictionary 453, finish 531, install 4 | 1024.0 | 384.2 | 155.0 | 4 | 0 | 82.7 |
| lubm-like univ 38 | disk@stream | parse+encode 16110, spo 736, index 1044, pos 536, osp+vstree 1044, dictionary 378, finish 477, install 3 | 1024.0 | 352.2 | 155.0 | 4 | 0 | 82.7 |
| lubm-like univ 38 | rocksdb | parse+encode 16799, spo 1490, index 1080, pos 700, osp 740, x-counts 608, dictionary 203, ingest 94, install 1 | 1024.0 | 314.7 | 155.0 | 4 | 0 | 0.0 |
<!-- /gstore-bench report -->

<!-- gstore-bench report --table update bench/results/phase6-final-lubm38.json -->
| workload | backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| lubm-like univ 38 | snapshot | 0.01 / 0.17 | 0.01 / 0.01 | 6478 | 41.7 | 31.6 | 1156.6 | 780.5 | 842657 → 842657 | ok |
| lubm-like univ 38 | disk@materialize | 1.70 / 3.16 | 1.85 / 2.62 | 6478 | 949.5 | 954.7 | 0.1 | 2797.7 | 842657 → 842657 | ok |
| lubm-like univ 38 | disk@stream | 1.98 / 2.83 | 2.57 / 3.62 | 6478 | 938.3 | 1015.3 | 0.1 | 1.0 | 842657 → 842657 | ok |
| lubm-like univ 38 | rocksdb | 0.20 / 0.37 | 0.25 / 0.46 | 6478 | 131.9 | 128.8 | 43.3 | 641.5 | 842657 → 842657 | ok |
<!-- /gstore-bench report -->

更新往返之后每个布局都回到新建库时的词计数(842,657 个实体):删除释放的 id 被回收(R4-RECLAIM)。

### 6.4 univ 38:并发、混合负载、`/begin`

闭环客户端,每点 5 s;"qps vs base" 是对 pre-Phase-6 同一点的比值(pre 的快照布局 8 客户端 Q4 只有 13.4 qps):

<!-- gstore-bench report --table conc bench/results/phase6-final-lubm38.json --compare bench/results/pre-phase6-lubm38.json -->
| workload | backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies | qps vs base |
|---|---|---|---|---:|---:|---:|---:|---:|---|---:|
| lubm-like univ 38 | snapshot | sparql | q1 | 1 | 11539.8 | 0.09 | 0.14 | 0 | identical | 20.10× |
| lubm-like univ 38 | snapshot | sparql | q1 | 8 | 74691.0 | 0.10 | 0.28 | 0 | identical | 127.58× |
| lubm-like univ 38 | snapshot | sparql | q4 | 1 | 6223.2 | 0.16 | 0.25 | 0 | identical | 363.36× |
| lubm-like univ 38 | snapshot | sparql | q4 | 8 | 33157.8 | 0.24 | 0.52 | 0 | identical | 2472.61× |
| lubm-like univ 38 | snapshot | api | q1 | 1 | 5391.5 | 0.17 | 0.35 | 0 | identical | 9.04× |
| lubm-like univ 38 | snapshot | api | q1 | 8 | 15417.4 | 0.50 | 1.18 | 0 | identical | 24.95× |
| lubm-like univ 38 | snapshot | api | q4 | 1 | 2504.1 | 0.36 | 0.66 | 0 | identical | 140.65× |
| lubm-like univ 38 | snapshot | api | q4 | 8 | 11600.3 | 0.64 | 1.42 | 0 | identical | 754.93× |
| lubm-like univ 38 | disk@materialize | sparql | q1 | 1 | 12086.2 | 0.08 | 0.12 | 0 | identical | 518.02× |
| lubm-like univ 38 | disk@materialize | sparql | q1 | 8 | 73472.2 | 0.10 | 0.28 | 0 | identical | 3268.44× |
| lubm-like univ 38 | disk@materialize | sparql | q4 | 1 | 5795.0 | 0.17 | 0.30 | 0 | identical | 460.01× |
| lubm-like univ 38 | disk@materialize | sparql | q4 | 8 | 32814.9 | 0.24 | 0.52 | 0 | identical | 2807.77× |
| lubm-like univ 38 | disk@materialize | api | q1 | 1 | 5379.9 | 0.18 | 0.34 | 0 | identical | 230.89× |
| lubm-like univ 38 | disk@materialize | api | q1 | 8 | 15604.2 | 0.50 | 1.16 | 0 | identical | 703.52× |
| lubm-like univ 38 | disk@materialize | api | q4 | 1 | 2596.5 | 0.34 | 0.61 | 0 | identical | 223.01× |
| lubm-like univ 38 | disk@materialize | api | q4 | 8 | 11694.5 | 0.64 | 1.39 | 0 | identical | 1019.30× |
| lubm-like univ 38 | disk@stream | sparql | q1 | 1 | 10898.0 | 0.09 | 0.13 | 0 | identical | 467.09× |
| lubm-like univ 38 | disk@stream | sparql | q1 | 8 | 42034.8 | 0.19 | 0.44 | 0 | identical | 1869.93× |
| lubm-like univ 38 | disk@stream | sparql | q4 | 1 | 4801.1 | 0.21 | 0.37 | 0 | identical | 381.12× |
| lubm-like univ 38 | disk@stream | sparql | q4 | 8 | 19492.9 | 0.40 | 0.86 | 0 | identical | 1667.89× |
| lubm-like univ 38 | disk@stream | api | q1 | 1 | 4698.6 | 0.20 | 0.38 | 0 | identical | 201.65× |
| lubm-like univ 38 | disk@stream | api | q1 | 8 | 13915.2 | 0.56 | 1.27 | 0 | identical | 627.37× |
| lubm-like univ 38 | disk@stream | api | q4 | 1 | 1967.6 | 0.46 | 0.73 | 0 | identical | 168.99× |
| lubm-like univ 38 | disk@stream | api | q4 | 8 | 9309.4 | 0.81 | 1.72 | 0 | identical | 811.42× |
| lubm-like univ 38 | rocksdb | sparql | q1 | 1 | 8236.1 | 0.12 | 0.20 | 0 | identical | 546.66× |
| lubm-like univ 38 | rocksdb | sparql | q1 | 8 | 49920.8 | 0.15 | 0.40 | 0 | identical | 3476.00× |
| lubm-like univ 38 | rocksdb | sparql | q4 | 1 | 2529.2 | 0.39 | 0.64 | 0 | identical | 2114.26× |
| lubm-like univ 38 | rocksdb | sparql | q4 | 8 | 10860.0 | 0.73 | 1.60 | 0 | identical | 9340.00× |
| lubm-like univ 38 | rocksdb | api | q1 | 1 | 3781.1 | 0.25 | 0.49 | 0 | identical | 238.77× |
| lubm-like univ 38 | rocksdb | api | q1 | 8 | 11894.6 | 0.65 | 1.50 | 0 | identical | 787.67× |
| lubm-like univ 38 | rocksdb | api | q4 | 1 | 1629.8 | 0.57 | 0.91 | 0 | identical | 1347.27× |
| lubm-like univ 38 | rocksdb | api | q4 | 8 | 6206.7 | 1.23 | 2.62 | 0 | identical | 5329.54× |
<!-- /gstore-bench report -->

混合负载(8 个 Q4 读者 + 每 100 ms 一次单三元组写)与 `/api begin` 点(pre-Phase-6 的 harness 还没有这两项,所以没有比值):

<!-- gstore-bench report --table mixed bench/results/phase6-final-lubm38.json -->
| workload | backend | endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies |
|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---|
| lubm-like univ 38 | snapshot | sparql | off | q4 | 8 | 31966.8 | 0.24 | 0.54 | 51 / 0 | 0.81 / 1.53 | identical |
| lubm-like univ 38 | snapshot | sparql | on | q4 | 8 | 12.5 | 639.14 | 1124.47 | 6 / 0 | 1089.12 / 1126.97 | identical |
| lubm-like univ 38 | snapshot | api | off | q4 | 8 | 10764.0 | 0.68 | 1.48 | 51 / 0 | 1.44 / 3.14 | identical |
| lubm-like univ 38 | snapshot | api | on | q4 | 8 | 10.1 | 791.24 | 1111.74 | 6 / 0 | 1065.65 / 1111.98 | identical |
| lubm-like univ 38 | disk@materialize | sparql | off | q4 | 8 | 31264.8 | 0.25 | 0.54 | 51 / 0 | 6.00 / 4.97 | identical |
| lubm-like univ 38 | disk@materialize | sparql | on | q4 | 8 | 30304.8 | 0.25 | 0.56 | 51 / 0 | 4.04 / 6.45 | identical |
| lubm-like univ 38 | disk@materialize | api | off | q4 | 8 | 10306.0 | 0.71 | 1.61 | 51 / 0 | 4.40 / 5.52 | identical |
| lubm-like univ 38 | disk@materialize | api | on | q4 | 8 | 9101.0 | 0.80 | 1.82 | 51 / 0 | 4.58 / 6.28 | identical |
| lubm-like univ 38 | disk@stream | sparql | off | q4 | 8 | 20629.9 | 0.38 | 0.82 | 51 / 0 | 3.60 / 4.75 | identical |
| lubm-like univ 38 | disk@stream | sparql | on | q4 | 8 | 19226.9 | 0.41 | 0.85 | 51 / 0 | 8.04 / 5.87 | identical |
| lubm-like univ 38 | disk@stream | api | off | q4 | 8 | 8437.2 | 0.87 | 1.90 | 51 / 0 | 4.58 / 6.13 | identical |
| lubm-like univ 38 | disk@stream | api | on | q4 | 8 | 8675.0 | 0.85 | 1.93 | 51 / 0 | 4.37 / 6.14 | identical |
| lubm-like univ 38 | rocksdb | sparql | off | q4 | 8 | 8552.1 | 0.93 | 1.94 | 51 / 0 | 2.28 / 3.36 | identical |
| lubm-like univ 38 | rocksdb | sparql | on | q4 | 8 | 894.3 | 8.93 | 4.54 | 19 / 0 | 284.88 / 4943.28 | identical |
| lubm-like univ 38 | rocksdb | api | off | q4 | 8 | 5692.0 | 1.32 | 2.74 | 51 / 0 | 3.12 / 4.34 | identical |
| lubm-like univ 38 | rocksdb | api | on | q4 | 8 | 545.1 | 14.58 | 7.51 | 22 / 0 | 262.03 / 1678.29 | identical |

| workload | backend | /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB |
|---|---|---:|---:|---:|---:|---:|
| lubm-like univ 38 | snapshot | 20 | 0.38 | 0.51 | 1.13 | 0.1 / 0.1 |
| lubm-like univ 38 | disk@materialize | 20 | 0.38 | 0.60 | 0.93 | 0.0 / 0.0 |
| lubm-like univ 38 | disk@stream | 20 | 0.44 | 0.59 | 0.62 | 0.1 / 0.1 |
| lubm-like univ 38 | rocksdb | 20 | 2132.55 | 3013.19 | 3191.01 | 269.3 / 269.3 |
<!-- /gstore-bench report -->

快照布局在 `check_point` on 时读者只剩 12.5 / 10.1 qps:每次已确认的写都在独占守卫下重写整个快照(univ 38 每次 1.07–1.09 s),5 s 里只完成 6 次写;磁盘布局按提交点增量写入,没有这个代价。RocksDB 的 `/begin` 仍物化 store(计划 §8 D4)。

### 6.5 univ 82 与 WatDiv-like 规模 10

<!-- gstore-bench report --table overview bench/results/phase6-final-lubm82.json bench/results/phase6-final-watdiv10.json -->
| workload | backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| lubm-like univ 82 | snapshot | 69.79 | 1175.4 | 926.5 | 2715.3 (cold) | 2395.9 | 1046.0 | 1245.0 | 21.94 |
| lubm-like univ 82 | disk@materialize | 49.53 | 645.1 | 1405.5 | 4412.0 (cold) | 3404.1 | 734.4 | 939.5 | 16.09 |
| lubm-like univ 82 | disk@stream | 45.88 | 610.0 | 1405.5 | 4.8 (cold) | 0.9 | 6.4 | 380.8 | 30.18 |
| watdiv-like scale 10 | snapshot | 3.61 | 59.8 | 44.3 | 119.4 (cold) | 100.0 | 60.4 | 79.2 | 0.33 |
| watdiv-like scale 10 | disk@materialize | 2.63 | 54.1 | 83.2 | 290.5 (cold) | 209.7 | 66.0 | 84.5 | 0.26 |
| watdiv-like scale 10 | disk@stream | 2.78 | 53.9 | 83.2 | 7.0 (cold) | 1.0 | 6.3 | 43.3 | 0.41 |
| watdiv-like scale 10 | rocksdb | 3.12 | 67.1 | 27.2 | 92.8 (cold) | 77.0 | 35.2 | 66.6 | 1.19 |
<!-- /gstore-bench report -->

univ 82 的磁盘构建在 1,024 MiB 预算内溢出了 4 个 run(`phase6-final-lubm82.json` 的构建摘要:583.6 MiB,预算峰值 384.7 / 385.0 MiB,词表 310.0 MiB);univ 38 的构建没有溢出的 run(§6.3)。每条查询(univ 82):

<!-- gstore-bench report --table queries bench/results/phase6-final-lubm82.json -->
| query | rows | snapshot first ms | snapshot median ms | disk@materialize first ms | disk@materialize median ms | disk@stream first ms | disk@stream median ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| q1 | 3 | 0.18 | 0.01 | 0.15 | 0.01 | 0.28 | 0.02 |
| q2 | 262 | 88.41 | 85.26 | 77.17 | 71.19 | 202.74 | 145.01 |
| q3 | 10 | 0.10 | 0.01 | 0.10 | 0.02 | 0.22 | 0.02 |
| q4 | 9 | 0.09 | 0.05 | 0.11 | 0.06 | 0.30 | 0.07 |
| q5 | 576 | 696.21 | 645.81 | 771.44 | 707.75 | 819.45 | 777.01 |
| q6 | 870165 | 206.95 | 211.91 | 220.95 | 220.72 | 744.62 | 984.84 |
| q7 | 23 | 858.08 | 800.42 | 728.60 | 847.33 | 1028.46 | 950.42 |
| q8 | 10778 | 664.35 | 707.01 | 677.51 | 744.53 | 916.26 | 813.41 |
| q9 | 11724 | 999.13 | 899.91 | 961.69 | 988.72 | 1581.62 | 1451.28 |
| q10 | 3 | 652.35 | 646.54 | 738.27 | 685.87 | 740.39 | 881.52 |
| q11 | 325 | 6.54 | 3.61 | 0.61 | 0.23 | 11.47 | 0.49 |
| q12 | 21 | 2.50 | 1.21 | 0.26 | 0.13 | 2.92 | 0.31 |
| q13 | 930549 | 253.07 | 255.45 | 270.64 | 282.09 | 1067.70 | 1303.58 |
| q14 | 658632 | 154.71 | 141.28 | 159.76 | 165.29 | 808.84 | 876.09 |
<!-- /gstore-bench report -->

Stream 模式的大结果查询在 univ 82 增长得比数据快:Q6 / Q13 / Q14 分别是快照布局的 4.65 / 5.10 / 6.20 倍(univ 38 是 2.41 / 2.25 / 2.51 倍)。R4-VS 的剖析把 Stream 的大结果开销归于经字典 `TermCache` 解析每个结果 id(未命中时读 `id2*`);这些结果有 0.66–0.93M 行,它们的不同词在 univ 82 已装不进默认 64 MiB 的 `TermCache`(`DEFAULT_TERM_CACHE_BYTES`),这能解释超线性增长,但这一阶段没有专门剖析(没有闸门依赖它)。打开仍是 O(1):热 0.9 ms、冷 4.8 ms、打开后 6.4 MiB。

WatDiv-like 规模 10 的每个模板(20 个绑定后的模板在四种布局上行数与行哈希相同):

<!-- gstore-bench report --table queries bench/results/phase6-final-watdiv10.json -->
| query | rows | snapshot first ms | snapshot median ms | disk@materialize first ms | disk@materialize median ms | disk@stream first ms | disk@stream median ms | rocksdb first ms | rocksdb median ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| L1 | 2 | 1.18 | 0.75 | 1.00 | 0.68 | 1.65 | 0.69 | 3.88 | 3.03 |
| L2 | 23 | 0.98 | 0.49 | 0.46 | 0.17 | 2.23 | 0.58 | 2.03 | 1.30 |
| L3 | 22 | 0.06 | 0.01 | 0.05 | 0.02 | 0.20 | 0.05 | 0.22 | 0.06 |
| L4 | 8 | 0.04 | 0.01 | 0.04 | 0.01 | 0.14 | 0.03 | 0.21 | 0.07 |
| L5 | 2 | 0.29 | 0.17 | 0.14 | 0.05 | 0.48 | 0.14 | 0.45 | 0.31 |
| S1 | 9 | 1.44 | 1.46 | 1.35 | 1.42 | 1.98 | 1.53 | 3.80 | 3.32 |
| S2 | 8 | 0.14 | 0.05 | 0.14 | 0.05 | 0.32 | 0.11 | 0.83 | 0.49 |
| S3 | 55 | 0.19 | 0.08 | 0.16 | 0.09 | 0.64 | 0.17 | 1.80 | 0.68 |
| S4 | 2 | 0.31 | 0.22 | 0.06 | 0.03 | 0.14 | 0.06 | 0.32 | 0.15 |
| S5 | 7 | 0.08 | 0.03 | 0.07 | 0.04 | 0.20 | 0.08 | 0.42 | 0.24 |
| S6 | 4 | 0.04 | 0.02 | 0.03 | 0.02 | 0.09 | 0.03 | 0.38 | 0.11 |
| S7 | 1 | 0.02 | 0.01 | 0.02 | 0.01 | 0.04 | 0.02 | 0.07 | 0.03 |
| F1 | 1 | 0.20 | 0.11 | 0.16 | 0.11 | 0.34 | 0.20 | 2.39 | 1.50 |
| F2 | 1 | 0.89 | 0.88 | 0.74 | 0.72 | 1.00 | 0.84 | 2.15 | 1.68 |
| F3 | 51 | 8.80 | 5.94 | 7.66 | 5.12 | 10.81 | 6.33 | 13.30 | 9.54 |
| F4 | 8 | 3.14 | 2.91 | 2.22 | 2.15 | 3.13 | 2.13 | 3.49 | 3.09 |
| F5 | 37 | 1.20 | 1.09 | 1.12 | 0.96 | 2.10 | 1.01 | 11.95 | 9.06 |
| C1 | 95 | 4.60 | 4.25 | 3.89 | 3.67 | 7.16 | 3.90 | 28.14 | 28.12 |
| C2 | 2855 | 19.84 | 20.34 | 18.84 | 18.59 | 26.98 | 21.21 | 44.58 | 38.22 |
| C3 | 85286 | 27.23 | 25.59 | 27.47 | 19.41 | 36.82 | 23.04 | 84.39 | 69.46 |
<!-- /gstore-bench report -->

WatDiv-like 的混合负载(8 个 F5 读者 + 每 100 ms 一次写)暴露了 Stream 模式在持续写入下的弱点:`/sparql`、check_point off 时 Stream 读者 191.6 qps,Materialize 2,550.3 qps(`phase6-final-watdiv10.json`)。每次已确认的写都推进 `DiskTriples` 的 epoch,长 posting list 缓存(`ListCache`)在新 epoch 的第一次读取时整体清空,F5 的长列表因此每 100 ms 重新从 B+ 树读一遍。LUBM Q4 不受影响。

### 6.6 VS-tree 开 / 关

计划 §8 D6 要求先对比 VS-tree 开与关。R4-VS 的安静阶段(代码 `b9e8dc0`,univ 38,Stream 模式):强制旧行为 `GSTORE_VS_POLICY=tree`(每个主语变量都遍历树,磁盘库遍历 `vstree.kv` 的页)时 Q1–Q14 几何均值 47.70 ms,默认的 `VsPolicy::Auto`(候选集与树给出的完全相同,但对 out-of-core 树改从 store 计算,从不读 `vstree.kv` 的页)时 11.66 ms;两者的行与行序相同。`VsPolicy::Off`(完全不用树)会改变 LUBM Q2 在 univ 38 的计划(行相同、顺序不同)并且更慢:树给出的候选集影响计划,所以 `Auto` 保留候选集、只改变计算方式。

## 7. 退出闸门 G1–G7

目标见 `docs/PHASE6_PLAN.md` §2;除非注明,在 univ 38 测量。实测值出自上面的 `bench/results/phase6-final-*.json`,两处例外注明了出处:G4 的监控端点和 G5 的 churn 不在 reference 配置里,取自同一份 gstore 代码(`b9e8dc0`)的 R4-SNAP 与 R4-RECLAIM 安静阶段。

| 闸门 | 目标 | 实测 | 结论 | 原因 / 后续 |
|---|---|---|---|---|
| **G1a** 磁盘构建(`gbuild --disk`,批量) | ≤ 0.04 × pre(1,627.6 s → ≤ 65.1 s);`--memory 1024` 时峰值 RSS ≤ 1.25 GiB;`kvstore.kv` ≤ 0.75 × pre | Stream / Materialize 两次运行的构建 18.38 s / 21.06 s = **0.011× / 0.013×**;默认 1,024 MiB 预算下峰值 398 / 400 MiB(预算峰值 352 / 384 MiB,没有溢出的 run);`kvstore.kv` 542,609,408 字节 = **0.697×** | **通过** | |
| **G1b** 磁盘打开(Stream) | 热 ≤ 0.5 s,冷 ≤ 2.0 s;open(univ 38) / open(univ 8) ≤ 1.5 | 热 **0.92 ms**,冷(清页缓存后)**7.5 ms**;热的比值 **1.13**(0.92 / 0.81 ms);冷的比值在参考那一对里是 1.82(7.5 / 4.1 ms),另测三对(univ 8:5.3 / 5.1 / 8.1 ms;univ 38:7.8 / 7.3 / 4.3 ms)后四对的中位数是 7.4 / 5.2 = **1.43**,热中位数 0.91 / 0.85 = 1.07 | **通过** | 冷打开的数字是每次运行一个毫秒级的设备延迟样本(univ 8 的 4.1–8.1 ms 与 univ 38 的 4.3–7.8 ms 范围重叠);打开读的是固定的一组页,univ 82 也是冷 4.8 ms、热 0.9 ms |
| **G1c** 磁盘内存(Stream) | 打开后 RSS ≤ 120 MiB;Q1–Q14 期间 VmHWM ≤ 400 MiB;打开后 RSS 的 univ 38 / univ 8 比值 ≤ 1.5 | **6.1 MiB**;**207.3 MiB**;**1.01**(6.13 / 6.05)。univ 82:6.4 MiB、380.8 MiB | **通过** | |
| **G1d** 磁盘延迟(Stream) | Q1–Q14 几何均值 ≤ 0.6 × pre 磁盘,且 ≤ 1.5 × 同一提交的快照布局;没有查询 > 3 × 快照 | 11.55 ms = **0.079×** pre(146.92)、**1.14×** 快照(10.15);最差 Q14 **2.51×**、Q6 2.41×、Q13 2.25×(Materialize:0.053×、0.77×,最差 Q14 1.16×) | **通过** | 默认磁盘查询模式仍是 Materialize:R3-STREAM 自己的翻转条件(每条查询 ≤ 2.0 × Materialize:这里 Q14 2.15×、Q6 2.12×、Q12 2.07×、Q13 2.00×;几何均值 ≤ 1.4 ×:这里 1.47×)仍不满足,Stream 的大结果在 univ 82 扩展得更差(§6.5) |
| **G2** 内存 / 快照布局 | 构建 ≤ 0.5 × pre;打开后 RSS ≤ 0.9 × pre;几何均值 ≤ 0.9 × pre;Q4 ≤ 25 ms;Q1/Q3/Q4/Q11/Q12 的 univ 38 / univ 8 延迟比 ≤ 2.0 | 构建 29.60 s = **0.385×**;RSS 489.1 MiB = **0.820×**;几何均值 10.15 ms = **0.218×**;Q4 **0.049 ms**;比值 Q1 **1.03**、Q3 **1.04**、Q4 **0.97**、Q11 **5.58**(1.305 / 0.234 ms)、Q12 **4.91**(0.494 / 0.101 ms) | **未通过**(Q11、Q12 的扩展性) | 亚毫秒查询,内存 VS-tree 的 probe 随数据增长(R4-VS:快照布局 probe 花 1.27 / 0.53 ms,磁盘布局对 store 检查花 0.46 / 0.21 ms)。不是第 4 轮的退化:对 `d702463` 的同会话 A/B 时间相同(R4-VS 1.409 对 1.407 ms;R4-SNAP 0.98× / 0.95×),基线的比值是 6.02 / 4.99;磁盘布局对 store 检查,比值是 1.23 / 1.85(Stream)、1.30 / 1.65(Materialize)。后续(R4-VS 请求 4,`src/db`、`src/query`):`Database` 一侧证明树是精确的(`vs_shrink_debt() == 0`)之后,快照布局也改为对 store 检查;计划 §8 D6 |
| **G3** RocksDB | 构建 ≤ 0.25 × pre;打开 ≤ 10 s;Q1–Q14 几何均值 ≤ 0.8 × pre | 构建 19.50 s = **0.070×**(277.76 s);打开 **0.74 s** 冷 / 0.68 s 热(pre 60.8 / 59.4 s);几何均值 25.00 ms = **0.080×**(311.50) | **通过** | Q2 仍扩展得最差(univ 38 585.7 ms,univ 8 5.49 ms;R4-VS 请求 6) |
| **G4** 并发 | 经 `/sparql` 与 `/api`、在内存与 Stream 磁盘库上,8 客户端的 Q4 qps ≥ 2.5 × 1 客户端;`/begin` ≤ 5 ms(内存)、≤ 50 ms(磁盘);`/begin` 时 RSS 增长 ≤ 4 MiB(内存)、≤ 16 MiB(磁盘);长查询运行时监控 ≤ 100 ms | Q4 8 / 1:快照 **5.33 / 4.63**,Stream **4.06 / 4.73**(univ 8:5.24 / 3.89、4.35 / 4.38;Materialize 5.66 / 4.50;pre 0.78 / 0.86);`/api begin` 中位数内存 **0.38 ms**(最大 1.13),Stream **0.44 ms**(最大 0.62);RSS 增长 **+0.07 / +0.06 MiB**;监控(R4-SNAP 安静阶段,同一代码,univ 38):`/status`、REST `/monitor` 与 `/show`、`/api show`、`/api monitor` 在长查询期间、变更之后、保存期间都 ≤ **48.4 ms**(快照)/ ≤ **43.4 ms**(Stream);**`/api stat` 201.6–219.8 ms** | **未通过**(按字面:`/api stat`) | `/api stat` 按设计每个请求阻塞采样 200 ms CPU(上游语义),除此之外 ≤ 18 ms。后续:`src/gapi/ops_sys.rs` 改为后台 CPU 采样器。不在 §2 的界内、但一起测量的:快照布局 `check_point` on 时的混合负载仍低于 pre(univ 38 12.5 / 10.1 qps,pre 的只读 8 客户端 13.4 / 15.4;univ 8 42.2 / 38.1,pre 76.3 / 76.5),与基线相同:每次写都在独占守卫下保存整个快照(univ 38 每次 1.07–1.09 s,univ 82 2.4–2.9 s)。后续:从捕获的写时复制镜像保存(`db/persist.rs`,与 §8 D2 相关);磁盘布局满足每个混合负载目标(Stream 在 check_point on 时 19,227 / 8,675 qps) |
| **G5** id 回收 | 10 轮 churn:live 计数恰等于被引用的词、在第 0 轮的 ±5% 内、高水位 ≤ 1.05 × live 峰值;删除开销内存与磁盘 ≤ 10%、RocksDB ≤ 20% | (R4-RECLAIM 安静阶段,`b9e8dc0`)内存、Stream、Materialize、RocksDB 的全部 11 轮 live = 被引用;高水位 **1.000**;第 4 轮 **+5.20%**(268,332 / 255,061;数据本身 +5.23%;Off 2.27 倍);删除 Immediate / Off:内存 **1.13 / 1.12**(两天各一次)、Stream **0.63**、**Materialize 1.14**、RocksDB **0.98** | **未通过**(±5%;内存与 Materialize 的删除开销) | ±5%:被引用的词自己就变了 +5.2%(各大学大小不同;每轮 live / triples 都是 0.2480)——后续:等大的大学,或按每三元组定界。内存(+12–13%;回收步骤本身 3.4%,第 1 轮 1.006):id 复用在后续轮次留下的状态——后续:每条语句一次 retire / release、位图空闲列表。Materialize(+14%,默认磁盘模式):每次释放是两次 B+ 树写,按现有的每写代价——后续:`kvstore::bptree` 的批量叶子更新路径。R4-RECLAIM 自己的 RSS 与 `kvstore.kv` 界(不属 §2)也没满足(1.53 倍、1.47 倍;文件只增不减,§8 D9) |
| **G6** 正确性 | 每种布局与模式的答案数与行哈希相同;完整测试矩阵、`conformance.sh`、崩溃测试全绿 | univ 8 与 38 的四种布局、univ 82 的三种,Q1–Q14 行数与行哈希都相同,等于 pre-Phase-6 的,每个进程里稳定;WatDiv-like 在四种布局上答案相同;每个更新往返、并发响应体和混合负载点一致;测试(R4-CLOSURE 安静阶段,`85686e5`,gstore 代码同 `769c627`):工作区两种磁盘模式各 2,375 个通过,RocksDB 2,202,特性组合 2,192,gstore-bench 69(+ RocksDB 69),conformance 6 / 6 组,0 失败 | **通过** | 逐特性矩阵最后一次在 `e941a2b` 上运行(之后只有 `rocksdb` 门控的代码变过,它在这里跑了) |
| **G7** "与上游同量级"(univ 82 对 lubm10M) | 构建 µs/三元组、字节/三元组和每条可比查询 ≤ 10 × 上游;≤ 3 × 计为**达成** | 快照 / 磁盘(默认 Materialize):构建 **6.27 / 4.45 µs**(上游 23 的 0.27× / 0.19×);**83.2 / 126.2 字节/三元组**(264 的 0.32× / 0.48×);Q1 0.013 / 0.014 ms(24),Q2 **85.3 / 71.2 ms**(144 的 0.59× / 0.49×),Q3 0.014 / 0.016(29),Q4 0.052 / 0.055(37),Q14 **141.3 / 165.3 ms**(223 的 0.63× / 0.74×) | **通过**(每项 ≤ 1×) | Stream(可选)在 10 倍以内,但 Q14 是上游的 3.93 倍(876 ms)、Q2 1.01 倍。限制见 §8 |

**小结**:G1a、G1b、G1c、G1d、G3、G6、G7 通过;G2、G4、G5 在上表所列的项上未通过,每项都记了实测值、原因与后续(计划 §5.7)。没有一项是第 4 轮的退化:G2 的 Q11 / Q12 和 G4 的 `/api stat`、快照布局 check_point 保存与基线相同(同会话 A/B),G5 的未达项是回收设计的代价,R4-RECLAIM 的安静阶段剖析(callgrind)并记录了它们。

## 8. 与上游 gStore 的对比(G7)及其限制

上游参照是 gStore 0.5.0 在 lubm10M 上的正式实验(`formal_experiment.tex`,见 `docs/PHASE6_PLAN.md` §0):构建 248.5 s、索引 2.86 GB,即每三元组 23 µs、264 字节;可比的查询 q8 ≈ Q1 24 ms、q1 ≈ Q2 144 ms、q10 ≈ Q3 29 ms、q4 ≈ Q4 37 ms、q13 ≈ Q14 223 ms。比值 = 本仓 / 上游:

| 指标 | 上游 | 快照布局 | 磁盘布局(Materialize,默认) | 磁盘布局(Stream,可选) |
|---|---:|---:|---:|---:|
| 构建 µs/三元组 | 23 | 6.27(**0.27×**) | 4.45(**0.19×**) | 4.12(0.18×) |
| 字节/三元组 | 264 | 83.2(**0.32×**) | 126.2(**0.48×**) | 126.2(0.48×) |
| Q1 ms | 24 | 0.013(0.0005×) | 0.014(0.0006×) | 0.019(0.0008×) |
| Q2 ms | 144 | 85.26(**0.59×**) | 71.20(**0.49×**) | 145.0(1.01×) |
| Q3 ms | 29 | 0.014(0.0005×) | 0.016(0.0006×) | 0.018(0.0006×) |
| Q4 ms | 37 | 0.052(0.0014×) | 0.055(0.0015×) | 0.075(0.002×) |
| Q14 ms | 223 | 141.3(**0.63×**) | 165.3(**0.74×**) | 876.1(3.93×) |

本仓的数字出自 `bench/results/phase6-final-lubm82.json`(构建时间与库字节数除以 11,135,159 个三元组;查询是热中位数)。这张表应当这样读:

- **数据不同**:LUBM-like 生成器不是 UBA(§2);univ 82 是 11.1M 个三元组,lubm10M 约 10.8M。
- **硬件不同**:上游的实验机器与本仓的 4 vCPU 参考机不同,没有在同一台机器上运行上游。
- **harness 不同**:上游的 harness 每条查询约有 23 ms 的下限;本仓的数字是进程内的热中位数。Q1、Q3、Q4 在本仓是亚毫秒级,这几项比值主要量的是上游的下限,不说明引擎本身差几个数量级。
- **索引不同**:上游的数字是它自己的(磁盘)索引;本仓快照布局是内存快照,只在磁盘布局一列上可以和"磁盘索引"直接对照。字节/三元组计的是整个库目录。
- **不是外推**:G7 在 univ 82 上实测,没有从 univ 38 外推。

结论:在快照布局和默认模式的磁盘布局上,每个指标都不超过上游(G7 达成);Stream 模式在 10 倍以内,但 Q14 超过 3 倍。

## 9. 复现

最终参考文件的命令(R4-CLOSURE 安静阶段;从工作区根目录运行,需要 root 才能 `--drop-caches`):

```sh
cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench
CARGO_TARGET_DIR=target-rocks cargo build --release -p gstore -p gstore-bench --bin gbuild --bin gstore-bench \
    --features gstore/rocksdb,gstore-bench/rocksdb
export GSTORE_BENCH_CACHE=/tmp/gstore-bench-cache B=target/release/gstore-bench R=target-rocks/release/gstore-bench
for s in 8 38; do
  flock /tmp/gstore-bench.lock $B run --profile reference --scale $s --extra none --backends snapshot \
      --drop-caches --verify-data --out f$s-s.json
  flock /tmp/gstore-bench.lock $B run --profile reference --scale $s --extra none --backends disk \
      --env GSTORE_DISK_QUERY_MODE=stream --label stream --drop-caches --verify-data --out f$s-ds.json
  flock /tmp/gstore-bench.lock $B run --profile reference --scale $s --extra none --backends disk \
      --env GSTORE_DISK_QUERY_MODE=memory --label materialize --drop-caches --verify-data --out f$s-dm.json
  flock /tmp/gstore-bench.lock $R run --profile reference --scale $s --extra none --backends rocksdb \
      --bin-dir target-rocks/release --drop-caches --verify-data --out f$s-r.json
  $B merge --out bench/results/phase6-final-lubm$s.json f$s-s.json f$s-ds.json f$s-dm.json f$s-r.json
  $B compare bench/results/pre-phase6-lubm$s.json bench/results/phase6-final-lubm$s.json --markdown \
      > bench/results/phase6-final-lubm$s-compare.md
done
# WatDiv-like 规模 10:同样四次运行,用 --workload watdiv(不加 --scale / --extra),合并成 phase6-final-watdiv10.json;
# univ 82:--scale 82 的 snapshot、disk --label materialize、disk --label stream 三次运行,合并成 phase6-final-lubm82.json
$B report --update README.md CHANGELOG.md docs/BENCHMARKS.md
$B report --check README.md CHANGELOG.md docs/BENCHMARKS.md
```

Phase 6 之前在 univ 38 上的记录用了 39 分钟(快照 + 磁盘,其中磁盘构建 27 分钟)和 24 分钟(RocksDB:9 次约 60 s 的打开)(`bench/README.md`);现在 univ 38 的四种构建都在 30 s 以内(§6.1)。单独测 smoke:`target/release/gstore-bench ci`(对 `bench/baselines/smoke.json`)。
