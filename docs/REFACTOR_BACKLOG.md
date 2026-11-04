# 大重构待办(需与用户讨论后再做)

这些是超出"主干"范围、或需要重大设计决策的项。主干(导入RDF→存储→SPARQL查询→落盘)已实现;以下按价值/风险排序,逐项可独立推进。

## A. 存储引擎:磁盘原生B+树 + 页缓存 ★高价值 —— ✅ 已完成

- **已实现**(`src/kvstore`):
  - `pager`:固定4KB块的分页文件 + 写回式LRU页缓存 + 空闲页链表 + 头页(magic/页数/free链/16个root槽),持久化、可重开。
  - `bptree`:磁盘B+树(变长字节key→变长字节value),节点序列化进页、分裂(叶/内部)、有序叶链表、前缀范围扫描、重开后可读。
  - `store::DiskStore`:把上述组合成gStore式KVstore——字典树(entity/literal/predicate的`*2id`与`id2*`共6棵)+ 三元组三序索引(SPO/POS/OSP,12字节复合key),前缀扫描覆盖全部访问模式;构建/落盘/重开;`to_memory()`桥接到内存查询引擎。
  - `Database::build_disk`/`load_disk` + `is_disk`;CLI `gbuild --disk`、`gquery`自动识别磁盘库。DT:`tests/dt_disk.rs`把整个LUBM(10万三元组)建到磁盘B+树、重开、14条查询结果与内存版一致。
- **删除/再平衡**(已补):`BTree::delete` 实现借位(redistribute)+合并(merge)+根收缩,回收页入free-list,投影大小精确校验保证节点不溢页;`DiskStore::delete_triple` 同步删除SPO/POS/OSP三索引并递减计数。UT见`bptree`/`store`测试。
- **后续可优化**:查询直接流式读盘(当前`load_disk`把工作集经页缓存载入内存索引后查询);VList紧凑值编码、mmap、崩溃一致性(WAL)、并发(见E)、磁盘上的VS-tree(见B)、删除后字典id的freelist复用。
- **更新(Phase 6)**:上面的"写回式LRU页缓存 … 16个root槽"已过时——页缓存是共享写时复制页(`Arc<[u8; 4096]>`)上的CLOCK,脏页只经提交离开缓存;头部有32个根槽加feature位掩码,版本标记由格式注册表`kvstore/format.rs`统一管理(V1 `GSTOREKV` / V2 `GSTOREK2`);每个逻辑写是一个原子pager操作(no-steal:超大的操作让缓存暂时超出容量,结束时再提交);`DiskStore`的读与`TripleStore`同序。"后续可优化"里:查询直接流式读盘(Stream模式)、VList、WAL、磁盘VS-tree、id freelist复用(最低id优先)都已完成;mmap属于延后的段布局(§I D1)。见`docs/STORAGE_BACKEND.md`。

## B. VS-tree签名索引(gStore的标志性特性) ★中价值 —— ✅ 已完成

- **已实现**(`src/signature`):`Signature`(944位`EntityBitSet`,逐位对齐gStore的`Signature.cpp`三段编码:str 600 + predicate 200 + combined 144)、`VsTree`(签名树,bulk-build按签名聚类分叶、内部节点存子树并集、自顶向下剪枝搜索)。`Database`构建时建树并随库持久化(`vstree.bin`),更新后置脏、仅在一致时用于过滤(保证正确性)。查询引擎为每个实体型变量(出现在主语位)按其常量邻边算查询签名,用VS-tree取候选集做连接前剪枝。
- **正确性**:候选集是真匹配的超集(包含性过滤),DT测试`vstree_filter_preserves_results`断言开/关VS-tree的LUBM 14条查询结果完全一致。
- **后续可优化**:更优的S-tree分裂启发式(当前用签名排序聚类)、把签名索引也下到磁盘(配合A)。

## C. 代价式查询优化器 ★中价值 —— ✅ 已完成

- **候选与启发式**(`src/query/candidates.rs` + `planner.rs`):精确候选生成(常量边求交+选择度传播)、NodeScore启发式、采样基数估计、卫星点延后。模式数>14时由`planner`贪心定序兜底。
- **DP优化器**(`src/query/optimizer.rs`):
  - **左深DP**(`n·2ⁿ`子集DP):`dp[S]`=物化`S`内模式的最优代价,逐个追加连通模式,产出最优左深pattern序,替换原贪心序。
  - **二元(bushy)连接**(`3ⁿ`子集划分DP = gStore `ConsiderBinaryJoin`):枚举把`S`划成两个连通半区的所有方式,得到最优二元连接树`JoinTree`;当其严格比最优左深更省时,由engine的hash-join树执行器(`eval_join_tree`)执行,否则仍走左深流水线。
  - **System-R代价模型**:模式基数取自要扫的索引区间大小;连接输出基数=`|A|·|B| / Π max(NDV_A(v),NDV_B(v))`,NDV(distinct值数)取自谓语统计`pre2sub`/`pre2obj`并用精确候选集收紧。
  - **plan_cache**:DP表本身即子计划代价缓存;evaluator另置结构同构BGP的plan缓存(子查询/重复BGP复用枚举结果)。
  - 验证:`tests/dt_optimizer.rs`(bow-tie二元连接端到端)+ LUBM部分查询实际走bushy且计数全对。
- **后续可优化**:跨查询持久化plan cache、直方图/相关性估计、topk优化(原版`topk`/`DFSPlan`)、命名物理join算子枚举。

## D. 完整SPARQL 1.1 ★中价值 —— ✅ 已完成

- **已实现**:`SELECT`/`ASK`/`CONSTRUCT`/`DESCRIBE`;图模式代数含`Bgp`/`Join`/`Union`/`Filter`/`LeftJoin`(OPTIONAL)/`Minus`/`Extend`(BIND)/`Values`/`SubSelect`/`Path`/`Graph`(命名图);聚合(`GROUP BY`/`HAVING`/全套agg含`DISTINCT`)与`(expr AS ?v)`;属性路径(`/ ^ | * + ? !`);`EXISTS`/`NOT EXISTS`(变量代入);完整UPDATE(INSERT/DELETE DATA、DELETE/INSERT WHERE、DELETE WHERE、LOAD、CLEAR/DROP/CREATE、`;`序列、WITH/USING);命名图GRAPH(查询/四元组DATA/CLEAR/持久化);xsd:dateTime比较;Turtle `[ ]`/`( )`。DT:`dt_sparql11`/`dt_update`/`dt_graph`/`dt_reason`。
- **缺(合理推迟/省略)**:`SERVICE`(联邦,需出网→解析即报错);GRAPH出现在DELETE/INSERT WHERE模板内;完整数值类型层级(现Int(i64)/Double(f64),比较正确);子查询相关性优化;30+图算法聚合(gpstore)。

## E. 并发、事务与MVCC ★中价值 —— ⚠️ 事务+快照隔离已完成,完整MVCC待做

- **已实现**:`Database::begin/commit/rollback`(undo日志单写者事务,原子+回滚,覆盖全部UPDATE与命名图);WAL(`pager`)存储层崩溃一致性;`concurrent::ConcurrentDb`(`src/concurrent`)—— 快照隔离:多读线程对immutable `Arc<Snapshot>`无锁评估,单写者串行提交后原子换快照,读者永不见半写态、不互相阻塞;`snapshot()`/`version()`/`update()`/`write()`。
- **缺**:每键版本链/细粒度MVCC、多写并发、并行加载(9线程)/OpenMP并行排序、快照GC(现靠Arc引用计数)、无锁读(现Arc-swap)。
- **更新(Phase 6)**:`Database: Send + Sync`,`&self`读路径;两个HTTP服务器读共享、写独占;`TripleStore`与字典写时复制,`ConcurrentDb`发布与`/begin`对内存库O(1);Stream磁盘库的`/begin`是`DiskReadView`。见`docs/DESIGN.md` §11。仍缺的(pager MVCC、多写者事务等)见§I D2、D14。

## F. 服务化:HTTP API / gRPC / 控制台 / 集群 ★按需 —— ⚠️ HTTP已完成

- **已实现**:`src/server`(`gserver`bin)—— 零依赖HTTP/1.1 SPARQL端点:`GET/POST /sparql`(SELECT→SPARQL JSON、ASK→boolean、CONSTRUCT/DESCRIBE→N-Triples)、`POST /update`、`GET /status`;`Mutex<Database>`共享。(更新(Phase 6):改为`RwLock<Database>`,读并行、写独占,`/status`不取库锁;多库`ApiServer`每库一个`DbCell`。)
- **缺**:gRPC(`tonic`)、HTTPS/鉴权、内容协商、流式响应、集群分片与分布式查询(`src/Cluster`,工作量最大)。SPARQL `SERVICE`(联邦,需出网)同属此类,现解析→明确报错。

## H. 图算法(gpstore) ★低优先 —— ⚠️ 核心算法已完成

- **已实现**:`src/analytics`(`GraphView`)—— 从`TripleStore`建CSR邻接(实体为节点、三元组为有向边),提供出/入度、BFS单源最短距离+路径重建、弱连通分量(union-find:路径折半+按秩合并)、PageRank(含悬挂节点均匀再分配+收敛判定)、无向三角计数(排序归并求交)。
- **缺**:介数/接近中心性、Louvain社区发现、SCC(Tarjan/Kosaraju)、加权/带谓语边变体、topk子图邻近查询(原版`topk`)。

## G. 推理(RDFS/OWL) ★低优先 —— ✅ RDFS已完成

- **已实现**(`src/reason`):前向链物化到不动点 —— 子类传递、rdf:type传播、子属性传递+数据传播、domain/range类型断言;`Database::materialize_rdfs()`。跨实体/谓语id空间用字典桥接。DT:`dt_reason`。
- **缺**:OWL更丰富的公理(等价类/属性、传递/对称属性、`sameAs`等);查询时展开(现为物化)。

## I. Phase 6 延后项与后续项 ★按需 —— ⚠️ Phase 6 已结束,以下未做

Phase 6(`docs/PHASE6_PLAN.md`)的12个工作包都已合并;退出闸门 G1a、G1b、G1c、G1d、G3、G6、G7 通过,G2、G4、G5 未通过(每个闸门的目标、实测值、结论与原因见 `docs/BENCHMARKS.md` 的闸门表)。本节收录计划 §8 的延后项(D1–D15)在 Phase 6 结束时的状态,以及各工作包记录下来、尚未处理的请求与后续项。文中数字注明出处:`bench/results/*.json`(提交的最终参考结果),或某个包的安静测量阶段。

### I.1 计划 §8 延后项的状态

| # | 项 | 状态 | 现在的影响 / 备注 |
|---|---|---|---|
| D1 | 段布局 / 磁盘格式 v2(mmap 的不可变代、toggle-delta 日志、压实) | 未做 | 仍以"B1+C1 原型在 LUBM-38 上的基准"为前提。格式注册表已能承载新 feature bit(`kvstore/format.rs`) |
| D2 | 持久化写时复制结构、版本化读视图、pager MVCC | 未做 | Phase 6 用 `RwLock` + `Arc` 写时复制 + `DiskReadView` 闸门。后果:`ConcurrentDb` 在 Stream / RocksDB 库上每次发布都物化 store;`ConcurrentDb` 包着 Stream 库时存活期间不释放 id(基准快照 pin 住字典) |
| D3 | 批量构建的并行确定性字典编码器、字典溢出段 | 未做 | 词表在内存预算之外,单独报告:univ 38 词表 155.0 MiB(`bench/results/phase6-final-lubm38.json` 的构建摘要) |
| D4 | RocksDB out-of-core 字典(`term2id` CF,layout 2)与原生快照 | 未做 | RocksDB 打开仍读入字典(univ 38 热打开 678.0 ms);`/begin` 仍物化 store(univ 38 中位数 2132.55 ms、RSS +269.3 MiB),同上文件 |
| D5 | 持久化 trigram 子串索引(`lexindex.kv`) | 未做 | 内存字典的索引按 generation 缓存并常驻:univ 38 时 430,954,544 字节(R2-DICT 安静测量,phase6 `0c9bdb6`),需要上限或开关;out-of-core 字典不建索引,子串 FILTER 扫描 |
| D6 | 增量维护的分页 VS-index;RocksDB 的 VS-tree | 未做 | R4-VS 的 `VsPolicy::Auto` 让磁盘库查询不再读 `vstree.kv`;树开 / 关的对照(R4-VS 安静测量,代码 `b9e8dc0`,univ 38 Stream):`GSTORE_VS_POLICY=tree` 几何均值 47.70 ms,`Auto` 11.66 ms。RocksDB 打开不建树(`vstree_on_open` 在内存里建) |
| D7 | IvArray 每空间超过 33,554,432 项;超长字典键 | 未做 | feature bit 1(`IVARRAY_WIDE`)、2(`LONG_KEYS`)保留。实际键上限是 2038 字节(`bptree::MAX_KEY_LEN`,半页),不是计划写的 4 KB;两者超限都明确报错 |
| D8 | 流水线 / morsel 执行器、零拷贝 chunk 访问、UNION 去物化、自适应候选求交 | 未做 | 下一个引擎项 |
| D9 | 空间回收:pager vacuum 与整理、RocksDB compact-on-deletion、`delete_range` 的 CLEAR、`compact_all` | 未做 | 文件只增不减:R4-RECLAIM 安静测量(代码 `b9e8dc0`,univ 8 起 10 轮 churn)里 Stream 的 `kvstore.kv` 从 107.8 MB 涨到 158.9 MB,是第 0 轮的 1.474 倍(第 8 轮起持平;`Off` 2.112 倍且仍在增长)。Stream 的 `CLEAR DEFAULT` 先把默认图的 id 读进内存(每三元组 12 字节) |
| D10 | 写者准入控制、限时写锁、fail-fast try-lock | 未做 | 监控已无锁、读已并行,饥饿基本消除 |
| D11 | 磁盘上的命名图(GSPO 支撑的 `NamedSource`) | 未做 | Stream 模式把命名图留在内存,内存随四元组数增长 |
| D12 | 大于页缓存的磁盘事务的崩溃原子性 | 未做 | 单个逻辑写原子;脏页溢出页缓存的事务分批提交,持久但整体非原子。需要 undo 日志或 pager 的 shadow paging |
| D13 | 分片或无锁页缓存 | 未做 | 未专门测竞争;Stream 库 Q4 从 1 到 8 个客户端:`/sparql` 4.06 倍、`/api` 4.73 倍(`bench/results/phase6-final-lubm38.json`) |
| D14 | 多写者 MVCC 事务与有意义的隔离级别 | 未做 | HTTP 契约仍是单写者(`409`) |
| D15 | 库函数 `Database::build_rocksdb_*` 改走 SST ingest | 按计划不做 | `gbuild`、`/api build`、REST `/build` 已走批量路径;`build_rocksdb_*` 保留为逐条写的小数据辅助函数,测试夹具行为不变 |

### I.2 未通过的退出闸门的后续项

- **G2:Q11 / Q12 的 univ 38 / univ 8 比值**(快照布局 5.58 / 4.91,目标 ≤ 2.0;Q11 1.305 / 0.234 ms,Q12 0.494 / 0.101 ms,`bench/results/phase6-final-lubm{38,8}.json`)。亚毫秒查询,内存 VS-tree 的 probe 随数据增长;磁盘布局对 store 检查,比值低得多。后续(`src/db`、`src/query`;R4-VS 请求4):在 `Database` 一侧给出"树是精确的"信号(如 `vs_shrink_debt() == 0`)之后,对内存树也做 store 检查而不是 probe——没有这个信号时,删除后保存的树带着旧位,store 检查会改变候选集,从而改变无 `ORDER BY` 时的解序。
- **G4:`/api stat`**(每请求阻塞 200 ms 采样 CPU,上游语义;R4-SNAP 安静测量:201.6–219.8 ms,其余监控端点 ≤ 48.4 ms)。后续:`src/gapi/ops_sys.rs` 改为后台 CPU 采样器。可选:`/api stat` 报告磁盘库的查询模式与字典缓存计数(`Dictionary::cache_stats`,R4-SNAP 请求4)。
- **G4 之外但一起测量的:快照布局 check_point on 的混合负载**(univ 38 读者 12.5 / 10.1 qps,`bench/results/phase6-final-lubm38.json`;与基线相同)。每次已确认的更新在独占守卫下重写整个快照。后续(`src/db/persist.rs`、`src/http_api.rs`;R4-SNAP 请求2):从捕获的写时复制镜像保存(`Database::save_image` 之类:字典、store、命名图现在都是 O(1) 克隆,VS-tree 需要写时复制形式或从 store 保存),HTTP 层捕获后即可释放独占守卫,保存落盘之前非事务读者读更新前的快照(事务快照机制已就绪)。
- **G5:id 回收**(R4-RECLAIM 安静测量,代码 `b9e8dc0`):
  - 与第 0 轮的 ±5%:+5.20%,而数据本身就变了 +5.23%(每轮 live / triples 都是 0.2480)。后续:`examples/reclaim_churn.rs` 用同样大小的大学,或按每三元组定界。
  - 内存布局的删除开销(Immediate / Off 1.13 / 1.12;回收步骤本身 3.4%)。后续:每条语句每个 interner 只做一次 retire / release,用位图代替 `BTreeSet` 空闲列表,一批释放一条语句的串。
  - Materialize 的删除开销(1.14;它是默认磁盘查询模式):每次释放是两次 B+ 树写(`*2id` 删除、空闲列表插入),每次写都把整个节点解析成逐项分配再序列化回去。后续:`kvstore::bptree` 里批量有序的叶子更新路径(每条语句每个被触及的叶子只解析、序列化一次),供 `free_term_in` / `release_index` 和 `delete_keys` 使用;或不分配的原地叶子编辑。
  - R4-RECLAIM 自己的验收界(不属 §2):内存 RSS 是第 0 轮的 1.53 倍(第 1 轮的 1.13 倍),Stream `kvstore.kv` 1.474 倍。第 0 轮是精确定长的批量镜像;后续:批量归并时 `reserve_exact`、大删除后 `malloc_trim`,或以稳态轮次为界;文件见 D9。

### I.3 Stream 模式(默认查询模式翻转的前提)

Stream 已满足 G1b / G1c / G1d,但 R3-STREAM 自己的翻转条件仍不满足(每条查询 ≤ 2.0 × Materialize:Q14 2.15 倍、Q6 2.12 倍、Q12 2.07 倍;几何均值 ≤ 1.4 倍:1.48 倍;单条插入中位数 ≤ Materialize:1.98 对 1.70 ms;`bench/results/phase6-final-lubm38.json`)。做完下面几项后重新测量,再改 `DEFAULT_DISK_QUERY_MODE`(`src/db/open_options.rs`):

- 大结果集的词解析(`src/kvstore/disk_dict.rs`、`src/dict`):按 id 稠密索引的解码词缓存,或不逐词分配的批量 `id2*` 读取;`TermCache` 的大小随字典伸缩(默认 64 MiB 在 univ 82 已装不下:Q6 / Q13 / Q14 分别 984.84 / 1303.58 / 876.09 ms,Materialize 220.72 / 282.09 / 165.29 ms,`bench/results/phase6-final-lubm82.json`)。
- `SpoWalk::collect` 先用 `LeafCursor::seek_in_leaf(key)`(已有,未接线)再逐项步进(`src/kvstore/store.rs`;R3-STREAM 请求6、R4-VS 请求2),针对 Q2 的 `o_by_sp` probe。
- `DiskTriples` 里按 epoch 缓存长范围的计数(R4-VS 请求1):磁盘上一次范围计数要读范围内的每个叶子,R4-VS 让常量星形和估计为短的列表避开了它,但优化器对其它 BGP 的每个模式仍每查询计数一次。
- `ListCache` 只丢弃提交触及的条目(按提交的 delta 逐 `ListKey` 失效),而不是每个 epoch 整体清空(`src/kvstore/disk_triples.rs`):持续写入时 Stream 读者大幅下降(WatDiv-like F5 混合负载 191.6 qps 对 Materialize 2550.3 qps,`bench/results/phase6-final-watdiv10.json`)。
- `DiskReadView::with_base` 每次调用新建一个 `DiskStore` 视图(重载 O(P) 统计);谓词很多的库可按 epoch 缓存视图(R2-DISKLIB)。overlay 的键过滤器固定 8 KiB / 键种,触及数十万键的事务会饱和(仍精确,只是每次读都走归并)。
- Stream 库的 `rebuild_index()` 写 `vstree.kv` 时把所有实体签名放在内存里;需要签名的外部排序,再用 `VsTree::write_disk_sorted` 流式写出。

### I.4 RocksDB

- Q2 扩展性最差:univ 8 5.49 ms → univ 38 585.74 ms(数据 5 倍,时间约 107 倍;`bench/results/phase6-final-lubm{8,38}.json`)。打开时没有 VS-tree,Q2 的连接由 `s_by_po` 读主导(R4-VS 请求6);可看 `vstree_on_open` 或更便宜的类型模式候选源。
- out-of-core 字典与原生快照见 D4;univ 82 未测 RocksDB(G7 只看快照与磁盘布局)。
- `ps_by_o` 仍对该 object 的列表排序(以入度为界),`count_p_by_so` 数 OSP 键(没有 `(s, o)` 计数器);256 MiB 的 SST 切分在已测的规模下都用不到(多文件 ingest 经隐藏的 `GSTORE_BULK_SST_SPLIT_BYTES` 钩子测试)。

### I.5 并发与服务器

- 内存 `ConcurrentDb` 上,释放了某个词的删除在已发布快照共享 interner 时复制整个 interner(`Interner::retire` 的 `Arc::make_mut`,O(词数))。插入新词从 Round 2 起就有同样的复制。去掉它需要分块或持久化的 interner、给 interner 也加 spare copy(像 `TripleStore::keep_spare_copies`),或推迟释放直到 interner 不再共享。先用 `ConcurrentDb` 的更新延迟测试驱动(`dt_cow_store`)测量。
- `ConcurrentDb` 包着 Stream 库时存活期间不释放 id:基准快照(`VersionView` 的后备)读共享的磁盘字典,需要 pin。要释放,基准就得能解析自己的 id(一份字典副本,O(词数)),或用 D2 的版本化读。
- 每库的 `[query]` 设置改为语句开始时读取的无锁单元(R3 review):现在 `read_synced` 在库一直忙于读时延迟应用新设置。
- `/api monitor` / `schema`:从未为本实例算过统计的库仍在请求路径上计算(共享守卫,排在后面的写者要等它)。
- 事务快照 CSR 的取消延迟是一个谓词的 `so_by_p` 读(`rdf:type` 这样数百万三元组的谓词仍整个读完才看到标志)。
- 单库 / 多库服务器的重入注册表只拒绝同一库的第二个守卫;同时持有两个不同库的守卫不拒绝(没有路径这样做,C8 禁止)。

### I.6 存储、构建与回收的其它后续

- 解析是批量构建的主要开销(univ 38 `disk@materialize` 那次 parse+encode 18,861 ms,`bench/results/phase6-final-lubm38.json`):`.nt` 的 N-Triples 快速路径,或按行分块并行解析(共享一个空白节点作用域、保留文件行号)——`src/parser` 的所有者(R2-BULK 请求3)。
- `fsutil::read_bincode` 从内存切片反序列化而不是经 `BufReader`(R4-SNAP 请求3):`BufReader::read_exact` 没有内联进 bincode 的元素循环,打开时每个快照文件都多花指令;但先整读文件会把大 `store.bin` 的打开峰值内存翻倍,需要测量后取舍。
- `*2id` 字典树仍线性遍历变长节点(没有偏移表);`sp2o_vlist` 没有声明形状(值长不定)(R1-KV)。
- 代价模型里基于 `ps_by_o` 的 subject NDV 带着 FIXME(数的是谓词不是 subject;为保持计划不变而保留,R1-CORE)。
- 没有自动的 VS-tree 重建阈值:被删 / 被释放的实体作为幻影留在树里(可靠超集),`vs_shrink_debt()` 只计数,直到 `rebuild_index()`。
- 被 pin 推迟的 Stream 释放只在内存里,重启后丢失(孤儿词,`gcompact --reclaim` 补收);RocksDB 上 `delete_terms` 失败也留下孤儿词。
- 新 `kvstore.kv` 落盘与删除被替换库的 `reason_rule_files/` 之间的短窗口里崩溃,旧规则会留在新库旁边;要关闭它需要把规则与它们所对应的数据绑定(规则账本里的 generation id),属推理格式的改动(R2 review)。
- 早于 R2 review 的构建留下的、没有 `LOCK` 的 scratch 目录不会被自动清扫,需手工删除。
- `gbuild` 清理自己创建的目录(`CreatedDir`)时不取目录锁(早于 Round 4;`holds_data` 检查保证不删已建好的库)。
- 部分 rustdoc 模块文档的叙述已过时:`src/bulk/mod.rs` 说批量构建写"version-1 `kvstore.kv`",`src/db/open_options.rs` 与 `src/db/persist.rs` 说两种模式读写"the same version-1 `kvstore.kv`"——R4-RECLAIM 之后,`id_reclaim` 为默认 `on` 时新建的磁盘库是 V2(`FREE_IDS`)。

### I.7 配置、工具与测试

- `GSTORE_VS_POLICY` 只有环境变量,没有 `conf.ini` 键(可加 `[query] vs_policy`,R4-VS 请求3)。
- `gadd`、`gsub`、`gquery` 等工具不读 `conf.ini`:`[system] id_reclaim` 与 RocksDB 调优键对它们只经环境变量(`GSTORE_ID_RECLAIM`、`GSTORE_ROCKS_BLOCK_CACHE_MB`)生效;gserver 与 gbuild 会读。
- 没有测试端到端地服务一个"只有 staging 区没删掉"的 REST 批量构建(需要注入 I/O 故障;`BulkReport::opens_as_built` 有单元测试)。
- 没有 CI 作业在 MSRV(1.89)上构建,只有 Docker 镜像用 MSRV;依赖升级可能悄悄抬高实际 MSRV(CI 工具链固定在 1.99.1,升级要连同新版本的 clippy / rustfmt 修正一起提交)。
- 基准:计时闸门未收紧(空闲机上的波动达 1.42),等有 CI runner 的数据再看;WatDiv-like 模板只有 uniform 映射,未移植 WatDiv 的 zipfian / normal 映射与压力测试查询生成器;混合负载的写者插入的是查询不用的谓词,`/begin` 点测的是 harness 进程自己的 RSS。

## 已做的小重构/clean-code(已直接落地,记录备查)

- 用Rust枚举`Term`统一IRI/Literal/Blank,替代原版到处用裸`string`+类型标志的写法。
- 用ID区间常量集中表达实体/字面量/谓语ID空间,替代分散的魔数。
- 索引访问统一为返回有序切片的方法,去掉原版手工`char*`+长度的裸指针接口。
- 错误用`Result<_, GStoreError>`显式传播,替代原版的bool返回+全局状态。
