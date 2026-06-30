# Rust ↔ C++ 模块对照(差距矩阵)

由多agent审计workflow产出(逐模块对比原版C++ + 对抗式代码检视)。记录"已忠实实现"与"仍缺/有差异"。✅=核心已对齐,⚠️=部分,❌=未做。标注「合理省略」的是经判断不纳入当前主干的项。

## 查询主干(已基本对齐)

| 模块 | 状态 | 已对齐 | 仍缺 / 差异 |
|------|------|--------|-------------|
| model `Util/Triple`+`GlobalTypedef` | ✅ | ID区间(LITERAL_FIRST_ID)、实体/字面量判定、`Term`枚举(比C++裸string+类型标志更安全) | C++ predicate id是有符号int(-1无效),Rust用u32+u32::MAX——语义等价,表示相反 |
| dict `KVstore` *2id/id2* | ⚠️ | string↔id三套独立空间、字面量偏移 | Trie前缀压缩(C++存≤32768前缀,省≥30%才用);磁盘B+树词典见kvstore |
| store `KVstore` *ID2values | ⚠️ | 六重访问模式、统计(pre2num/sub/obj) | C++紧凑字节编码(offset数组);字面量单独`objID2values_literal`;`getSubjectPredicateDegree`等度数API |
| parser SPARQL `Parser/SPARQL` | ⚠️ | SELECT/ASK/CONSTRUCT、BGP/UNION/OPTIONAL/MINUS/FILTER/BIND/VALUES/子查询/聚合/属性路径(`/ ^ \| * +`) | DESCRIBE;GRAPH/SERVICE(命名图);属性路径`?`(词法器`?`当变量前缀冲突);完整UPDATE(LOAD/CLEAR/DROP/ADD/MOVE/COPY/CREATE/DELETE WHERE);30+图算法聚合(gpstore) |
| query engine+value `Executor`/`EvalMultitypeValue` | ⚠️ | BGP连接、FILTER求值、合并连接、合成id | xsd:dateTime类型;完整数值类型层级(int/long/float/decimal/double分别建模,现仅Int(i64)/Double(f64)) |
| query planner+candidates `Optimizer`/`PlanGenerator` | ⚠️ | 精确候选生成(常量边求交+传播)、NodeScore启发式、采样基数估计、卫星点延后、策略选择 | 真·DP多计划枚举+plan_cache;**二元(bushy)连接**`ConsiderBinaryJoin`;显式cost字段与边选择度缓存;12种join method显式选择;谓语变量单独采样 |
| signature `Signature` | ✅ | 944位EntityBitSet三段编码(逐位对齐) | VSTree原版疑似增量插入+分裂(我用bulk聚类构建);**注**:此版本C++主路径其实不用VS-tree |
| db `Database` | ⚠️ | build/save/load/query/update、磁盘后端、VS-tree重建 | 见下"系统级"大量缺失 |

## 存储引擎 kvstore vs `KVstore/SITree/IVArray/ISArray/VList`

| 项 | 状态 | 说明 |
|----|------|------|
| 分页文件+LRU页缓存+B+树 | ✅ | 4KB块、分裂、前缀扫描、持久化/重开 |
| VList紧凑值编码 | ❌ | C++变长值列表压缩字节流;我直接存B+树value |
| SITree/IVArray/ISArray分化 | ❌ | C++按用途分化(块管理器);我用单一通用BTree |
| **删除+下溢合并/再平衡** | ❌ | 我只有insert/get/scan,无B+树删除/merge |
| 流式磁盘查询 | ❌ | 现`load_disk`先把工作集载入内存索引再查 |
| WAL/崩溃一致性 | ❌ | 现为写回+最终flush,无恢复 |

## 系统级子系统(整体未做 —— 多数为"合理省略",属REFACTOR_BACKLOG E/F/G)

| 项 | 对应C++ | 判断 |
|----|---------|------|
| MVCC/事务(锁、latch、版本链、GC、rollback/commit) | `Txn_manager`/`GraphLock`/`Latch`/KVstore MVCC | 大功能,需先定隔离级别;合理推迟 |
| 多线程并发(rwlock+8 mutex)、并行加载(9线程)、OpenMP并行排序 | `Database` | 需先定并发模型;合理推迟 |
| 备份/恢复 + 更新日志(update.log) | `Database::backup/restore/write_update_log` | 运维特性;合理推迟 |
| CSR邻接结构 + 图算法套件(PageRank/最短路/介数/louvain等30+) | `src/Query/topk/` | 这是gpstore图计算,非SPARQL查询;合理省略 |
| Schema抽取/管理 | `Database::updateSchema/getSchemaInfo` | 周边特性;合理推迟 |
| ID freelist复用(BlockInfo链) | `Database::allocEntityID`等 | 删除后id回收;随"删除支持"一起做 |
| QueryCache、entity/literal buffer、重要谓语缓存 | `Database`/`QueryCache` | 性能缓存;合理推迟 |
| 进度上报、统计监控API | `DatabaseProgressStatus`/`getDBMonitorInfo` | 周边;合理推迟 |
| RDFParser分批(每10M一组)+ 数值范围校验(xsd:int/short/byte边界) | `Parser/RDFParser` | 分批=超大文件;数值范围校验值得补 |
| Turtle `[ ]` 空节点属性列表 / `( )` 集合 | `TurtleParser` | 值得补(backlog D) |

## 代码检视结果

22条原始发现 → 对抗式验证确认20条真bug,全部修复(见 `Audit workflow` 提交):大整数(>2⁵³)比较/排序经f64丢精度(真·正确性bug)、`i64::MIN`取负溢出、B+树`split_internal`退化节点、页计数/字面量id/`Vec::len`→u32溢出、词法器多小数点、Turtle相对IRI(RFC3986)、HAVING解析、签名越界断言、load损坏检测加强。190测试全过,clippy零告警。

> 校正一处:审计agent称"planner只有贪心、无DP"——实际我已实现**基于采样的基数估计**做较大查询的连接定序(等价于gStore DP的代价来源);但gStore真正的**多计划DP枚举 + 二元连接**确未实现,仍是有效差距。
