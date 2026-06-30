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

## D. 完整SPARQL 1.1 ★中价值 —— ✅ 主体已完成

- **已实现**:`SELECT`/`ASK`/`CONSTRUCT`;图模式代数`GraphPattern`含`Bgp`/`Join`/`Union`/`Filter`/`LeftJoin`(OPTIONAL)/`Minus`/`Extend`(BIND)/`Values`/`SubSelect`/`Path`;聚合(`GROUP BY`/`HAVING`/`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`/`SAMPLE`/`GROUP_CONCAT`,含`DISTINCT`)与投影表达式`(expr AS ?v)`;属性路径(`/` `^` `|` `*` `+` `!`);丰富内建函数。计算值(BIND/VALUES/聚合结果)通过每查询的synthetic-id interner流经统一的id连接引擎。DT覆盖见`tests/dt_sparql11.rs`(16个用例)。
- **缺**:`GRAPH`/`SERVICE`(命名图)、属性路径`?`(zeroOrOne,词法器把`?`当变量前缀,冲突)、`DESCRIBE`、Turtle的`[ ]`/`( )`、完整日期/时区类型体系、`EXISTS`/`NOT EXISTS`、子查询的相关性优化。
- **后续**:逐项补齐,均可增量加算子/内建并配UT/DT。

## E. 并发、事务与MVCC ★中价值

- **原版**:`Txn_manager`、`GraphLock`、`Latch`、KVstore里的MVCC版本链与两阶段锁。
- **重构**:为Rust设计基于`RwLock`/版本号的并发模型,或集成现成存储引擎事务。需先明确目标隔离级别。

## F. 服务化:HTTP API / gRPC / 控制台 / 集群 ★按需

- **原版**:`src/Server`、`src/Api`、`src/GRPC`、`src/Cluster`(分布式分片),`ghttp`/`grpc`/`gserver`等。
- **重构**:用`axum`/`tonic`等重建对外接口;集群涉及分片与分布式查询,工作量最大,建议最后做。

## G. 推理(RDFS/OWL) ★低优先

- **原版**:`src/Reason`(规模较小)。
- **重构**:基于规则的前向链推理,作为查询前的物化或查询时展开。

## 已做的小重构/clean-code(已直接落地,记录备查)

- 用Rust枚举`Term`统一IRI/Literal/Blank,替代原版到处用裸`string`+类型标志的写法。
- 用ID区间常量集中表达实体/字面量/谓语ID空间,替代分散的魔数。
- 索引访问统一为返回有序切片的方法,去掉原版手工`char*`+长度的裸指针接口。
- 错误用`Result<_, GStoreError>`显式传播,替代原版的bool返回+全局状态。
