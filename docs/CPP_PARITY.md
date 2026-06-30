# Rust ↔ C++ 模块对照(差距矩阵)

由多agent审计workflow产出(逐模块对比原版C++ + 对抗式代码检视)。记录"已忠实实现"与"仍缺/有差异"。✅=核心已对齐,⚠️=部分,❌=未做。标注「合理省略」的是经判断不纳入当前主干的项。

## 查询主干(已基本对齐)

| 模块 | 状态 | 已对齐 | 仍缺 / 差异 |
|------|------|--------|-------------|
| model `Util/Triple`+`GlobalTypedef` | ✅ | ID区间(LITERAL_FIRST_ID)、实体/字面量判定、`Term`枚举(比C++裸string+类型标志更安全) | C++ predicate id是有符号int(-1无效),Rust用u32+u32::MAX——语义等价,表示相反 |
| dict `KVstore` *2id/id2* | ⚠️ | string↔id三套独立空间、字面量偏移 | Trie前缀压缩(C++存≤32768前缀,省≥30%才用);磁盘B+树词典见kvstore |
| store `KVstore` *ID2values | ⚠️ | 六重访问模式、统计(pre2num/sub/obj) | C++紧凑字节编码(offset数组);字面量单独`objID2values_literal`;`getSubjectPredicateDegree`等度数API |
| parser SPARQL `Parser/SPARQL` | ✅ | SELECT/ASK/CONSTRUCT/**DESCRIBE**、BGP/UNION/OPTIONAL/MINUS/FILTER/BIND/VALUES/子查询/聚合/**EXISTS·NOT EXISTS**/属性路径(`/ ^ \| * + ?`)、**命名图GRAPH**、**完整UPDATE**(INSERT/DELETE DATA、DELETE/INSERT WHERE、DELETE WHERE、LOAD、CLEAR/DROP/CREATE、`;`序列、WITH/USING) | SERVICE(联邦,需网络→明确报错);30+图算法聚合(gpstore);GRAPH出现在DELETE/INSERT WHERE模板内 |
| query engine+value `Executor`/`EvalMultitypeValue` | ✅ | BGP连接、FILTER求值、合并连接、合成id、EXISTS(变量代入)、GRAPH(常量/变量图)、**xsd:dateTime/date比较**(解析为UTC瞬时,跨时区按时序;含闰年/日域校验) | 完整数值类型层级(int/long/float/decimal分别建模,现归并为Int(i64)/Double(f64),比较/排序已正确) |
| query planner+candidates+optimizer `Optimizer`/`PlanGenerator` | ✅ | 精确候选生成(常量边求交+传播)、NodeScore启发式、采样基数估计、卫星点延后;**真·DP多计划枚举**(左深`n·2ⁿ` DP得最优pattern序)、**二元(bushy)连接**(`3ⁿ`子集划分DP=`ConsiderBinaryJoin`,选出二元连接树由hash-join执行器跑)、**显式System-R代价模型**(NDV取自`pre2sub`/`pre2obj`谓语统计、再用精确候选集收紧)、**plan_cache**(结构同构BGP复用DP结果)。LUBM部分查询已实际走bushy计划且计数正确 | 12种命名物理join method(本引擎每pattern直接选最紧索引,等价);跨查询持久化plan cache(现为单查询evaluator内缓存) |
| signature `Signature` | ✅ | 944位EntityBitSet三段编码(逐位对齐) | VSTree原版疑似增量插入+分裂(我用bulk聚类构建);**注**:此版本C++主路径其实不用VS-tree |
| db `Database` | ✅ | build/save/load/query/**完整UPDATE**、磁盘后端、VS-tree重建、**命名图**(持久化)、**事务**(begin/commit/rollback)、**QueryCache**、**RDFS推理**、**Schema抽取**、**监控stats** | 见下"系统级"剩余项(并发/集群/服务化等) |

## 存储引擎 kvstore vs `KVstore/SITree/IVArray/ISArray/VList`

| 项 | 状态 | 说明 |
|----|------|------|
| 分页文件+LRU页缓存+B+树 | ✅ | 4KB块、分裂、前缀扫描、持久化/重开 |
| VList紧凑值编码 | ❌ | C++变长值列表压缩字节流;我直接存B+树value |
| SITree/IVArray/ISArray分化 | ❌ | C++按用途分化(块管理器);我用单一通用BTree |
| **删除+下溢合并/再平衡** | ✅ | `BTree::delete`:借位(redistribute,叶/内部节点经父分隔键旋转)+ 合并(merge,回收页入free-list)+ 根收缩(空叶清树、单孩内部节点降级);投影大小精确校验,节点永不溢页。`DiskStore::delete_triple` 三索引(SPO/POS/OSP)同步删除并递减计数 |
| 流式磁盘查询 | ✅ | `TripleSource` trait抽象访问模式,内存/磁盘统一;`Evaluator<S>`+优化器/候选/planner泛型化。`DiskStore::query` 仅把字典载入内存,三元组索引按需经页缓存流式读取(不全量物化)。**剩余**:字典也留盘(完全out-of-core)、流式路径上的VS-tree过滤 |
| WAL/崩溃一致性 | ✅ | `flush`为原子提交:头页+脏页先写带CRC+commit标记的`<file>.wal`并fsync,再落主文件fsync,再清日志;`open`时重放完整committed日志(redo)、丢弃残缺日志。脏页只经提交落盘(evict优先清干净页,全脏则先提交) |

## 系统级子系统

| 项 | 状态 | 对应C++ | 判断/说明 |
|----|------|---------|------|
| 事务(commit/rollback) | ✅ | `Txn_manager` | `begin/commit/rollback`经undo日志实现原子性+回滚(单写者),覆盖所有UPDATE与命名图 |
| 并发(快照隔离) | ⚠️ | `GraphLock`/`Latch`/KVstore MVCC | `concurrent::ConcurrentDb`:多读并发(无锁评估immutable `Arc<Snapshot>`)+串行写者(写后原子换快照);版本号、读者不被阻塞。缺:每键版本链、多写并发、并行加载(9线程)/OpenMP排序、快照GC |
| RDFS/OWL推理 | ✅ | `src/Reason` | `src/reason`前向链物化:子类/子属性传递、type传播、domain/range;`Database::materialize_rdfs` |
| Schema抽取 | ✅ | `Database::getSchemaInfo` | `Database::schema()` 抽类与属性 |
| QueryCache | ✅ | `Database`/`QueryCache` | 读查询按SPARQL串缓存,任何写入即失效 |
| 监控/统计API | ✅ | `getDBMonitorInfo` | `Database::stats()`→`DbStats`(计数+索引/事务状态) |
| Turtle `[ ]` / `( )` | ✅ | `TurtleParser` | 空节点属性列表 + 集合(降级为rdf:first/rest/nil链),含嵌套 |
| 备份/恢复 + 更新日志(update.log) | ❌ | `Database::backup/restore/write_update_log` | 运维特性;合理推迟(WAL已提供崩溃一致性) |
| CSR邻接 + 图算法 | ⚠️ | `src/Query/topk/` | `src/analytics`(`GraphView`):CSR邻接 + 出/入度、BFS最短路+路径、弱连通分量(union-find)、PageRank(含悬挂节点)、三角计数。缺:介数/接近中心性、Louvain、SCC、加权/带谓语边、topk子图查询 |
| 服务化:HTTP | ⚠️ | `src/Server`/`Api`/`ghttp` | `src/server`(`gserver`bin):HTTP/1.1 SPARQL端点(GET/POST /sparql→JSON结果、POST /update、GET /status),零依赖。缺:gRPC、HTTPS/鉴权、内容协商、流式 |
| 集群/分布式分片 | ❌ | `src/Cluster` | 分片+分布式查询;最后做 |
| SERVICE联邦查询 | ❌ | — | 需HTTP出网;已解析→明确报错 |
| ID freelist复用(BlockInfo链) | N/A | `Database::allocEntityID`等 | 仅在删除字典项时才有意义;gStore与本版删除三元组时均保留字典项(id不回收),故不适用 |
| RDFParser分批(每10M一组)+ 数值范围校验 | ⚠️ | `Parser/RDFParser` | 分批=超大文件(推迟);dateTime日域/时域已校验 |

## 代码检视结果

22条原始发现 → 对抗式验证确认20条真bug,全部修复(见 `Audit workflow` 提交):大整数(>2⁵³)比较/排序经f64丢精度(真·正确性bug)、`i64::MIN`取负溢出、B+树`split_internal`退化节点、页计数/字面量id/`Vec::len`→u32溢出、词法器多小数点、Turtle相对IRI(RFC3986)、HAVING解析、签名越界断言、load损坏检测加强。190测试全过,clippy零告警。

> 更新(2026-06-30):原"仍缺DP多计划枚举+二元连接"已补完。新增 `src/query/optimizer.rs`:左深DP求最优pattern序 + bushy子集划分DP(`ConsiderBinaryJoin`)产出二元连接树,由engine的hash-join树执行器执行;System-R代价模型(NDV取自谓语统计`pre2sub`/`pre2obj`并用候选集收紧);DP表+evaluator级plan_cache。`eval_bgp` 在bushy严格更省时切换到树执行器,否则走原左深流水线。LUBM等真实查询已验证(部分走bushy,计数全对)。
>
> 同时补完B+树删除(借位/合并/根收缩+页回收)与 `DiskStore::delete_triple`。
>
> 更新(2026-06-30,第一档+第三档批次):
> - **第一档**:完整UPDATE(DELETE/INSERT WHERE、LOAD、CLEAR/DROP/CREATE、`;`序列);WAL崩溃一致性;事务(commit/rollback);流式磁盘查询(`TripleSource` trait + 泛型引擎,`DiskStore::query`)。
> - **第三档**:属性路径`?`、EXISTS/NOT EXISTS、DESCRIBE;Turtle `[ ]`/`( )`;xsd:dateTime比较;RDFS推理;QueryCache;Schema抽取;监控stats;命名图GRAPH(查询/四元组更新/CLEAR/持久化)。
> - 期间跑独立code-reviewer检视并修复2个MEDIUM(EXISTS内层FILTER代入、DESCRIBE谓语列)+若干LOW。
> - 测试190→**272全过**,clippy零告警。剩余大颗粒:完整MVCC(并发)、服务化/集群、SERVICE联邦、gpstore图算法。
>
> 更新(2026-06-30,三大块批次):
> - **并发/MVCC**:`src/concurrent`(`ConcurrentDb`)——快照隔离的多读并发+串行写者(写后原子换`Arc<Snapshot>`)。
> - **服务化HTTP**:`src/server`(`gserver`bin)——零依赖HTTP/1.1 SPARQL端点(/sparql、/update、/status)。
> - **图算法gpstore**:`src/analytics`(`GraphView`)——CSR + 度/BFS最短路/弱连通分量/PageRank/三角计数。
> - 经两轮独立code-reviewer检视(第二轮4个MEDIUM已修);图算法块由subagent完成、我做集成检视。
> - 测试**288全过**,clippy零告警。剩余大颗粒:完整MVCC(每键版本链/多写并发)、gRPC/集群分布式、SERVICE联邦、更多图算法(介数/Louvain/SCC/topk)。
