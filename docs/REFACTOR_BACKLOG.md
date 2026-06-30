# 大重构待办(需与用户讨论后再做)

这些是超出"主干"范围、或需要重大设计决策的项。主干(导入RDF→存储→SPARQL查询→落盘)已实现;以下按价值/风险排序,逐项可独立推进。

## A. 存储引擎:磁盘原生B+树 + mmap分页 ★高价值

- **现状**:索引常驻内存,落盘用`serde`+`bincode`整体序列化。数据集必须能放进内存。
- **原版**:`KVstore`基于固定块(4KB)的B+树文件(`SITree`/`IVArray`/`ISArray`),配合LRU缓存与mmap,支持远超内存的数据集(目标42亿三元组)。
- **重构**:实现块式B+树 + 页缓存 + VList紧凑编码;`get*list*`改为按需读盘。
- **风险/工作量**:大。需要仔细的页布局、并发与崩溃一致性设计。建议先定块格式与缓存策略再动手。

## B. VS-tree签名索引(gStore的标志性特性) ★中价值 —— ✅ 已完成

- **已实现**(`src/signature`):`Signature`(944位`EntityBitSet`,逐位对齐gStore的`Signature.cpp`三段编码:str 600 + predicate 200 + combined 144)、`VsTree`(签名树,bulk-build按签名聚类分叶、内部节点存子树并集、自顶向下剪枝搜索)。`Database`构建时建树并随库持久化(`vstree.bin`),更新后置脏、仅在一致时用于过滤(保证正确性)。查询引擎为每个实体型变量(出现在主语位)按其常量邻边算查询签名,用VS-tree取候选集做连接前剪枝。
- **正确性**:候选集是真匹配的超集(包含性过滤),DT测试`vstree_filter_preserves_results`断言开/关VS-tree的LUBM 14条查询结果完全一致。
- **后续可优化**:更优的S-tree分裂启发式(当前用签名排序聚类)、把签名索引也下到磁盘(配合A)。

## C. 代价式查询优化器 ★中价值 —— ✅ 已完成

- **已实现**(`src/query/optimizer.rs`):基于谓语统计(pre2num/pre2sub/pre2obj,见`TripleStore`新增的统计方法)的基数估计 + Selinger式子集DP,生成最小化中间结果规模的左深连接顺序;模式数>16时退化为连通+最小基数贪心。已替换引擎里原先的贪心`order_plans`。
- **后续可优化**:浓密树枚举、直方图/相关性估计、topk优化(原版`topk`/`DFSPlan`)。

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
