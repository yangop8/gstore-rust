# 大重构待办(需与用户讨论后再做)

这些是超出"主干"范围、或需要重大设计决策的项。主干(导入RDF→存储→SPARQL查询→落盘)已实现;以下按价值/风险排序,逐项可独立推进。

## A. 存储引擎:磁盘原生B+树 + mmap分页 ★高价值

- **现状**:索引常驻内存,落盘用`serde`+`bincode`整体序列化。数据集必须能放进内存。
- **原版**:`KVstore`基于固定块(4KB)的B+树文件(`SITree`/`IVArray`/`ISArray`),配合LRU缓存与mmap,支持远超内存的数据集(目标42亿三元组)。
- **重构**:实现块式B+树 + 页缓存 + VList紧凑编码;`get*list*`改为按需读盘。
- **风险/工作量**:大。需要仔细的页布局、并发与崩溃一致性设计。建议先定块格式与缓存策略再动手。

## B. VS-tree签名索引(gStore的标志性特性) ★中价值

- **现状**:未实现。BGP直接走六重索引+连接,正确但在高选择度模式下不如签名剪枝快。
- **原版**:`src/Signature` + Database里的VSTREE,用实体邻域的bitset签名构建S-tree,查询时先用签名过滤候选,再验证。
- **重构**:实现`Signature`(EntityBitSet编码)+ 平衡签名树 + 候选生成。属于查询加速,不改变结果。

## C. 代价式查询优化器 ★中价值

- **现状**:贪心选择度启发式定连接顺序。
- **原版**:`Optimizer`/`PlanGenerator`/`PlanTree`/`Strategy`,基于谓语统计(pre2num/pre2sub/pre2obj)与动态规划做代价估计,生成左深/浓密连接树;含`topk`、`DFSPlan`等。
- **重构**:引入统计直方图 + DP连接枚举 + 代价模型。建议在有大查询基准后再做。

## D. 完整SPARQL 1.1 ★中价值

- **现状**:SELECT + ASK + BGP + **UNION/嵌套组** + FILTER + ORDER/LIMIT/OFFSET + DISTINCT + INSERT/DELETE DATA。图模式已是代数树(`Bgp`/`Join`/`Union`/`Filter`),加新算子较顺。
- **缺**:`OPTIONAL`(左连接)、`MINUS`、子查询、属性路径(`+`/`*`/`/`)、聚合(`GROUP BY`/`COUNT`/`SUM`…)、`CONSTRUCT`/`DESCRIBE`、`BIND`/`VALUES`、Turtle的`[ ]`/`( )`、完整的日期/数值类型体系。
- **重构**:逐特性扩展AST + 代数算子(LeftJoin/Minus/Aggregate/Path)。可增量推进,每个特性配UT/DT。
- **已完成(本次)**:`UNION`+嵌套组(代数`GraphPattern::Union`/`Join`)、`ASK`、Turtle导入。LUBM 14条查询(7条含UNION)全部可跑且结果正确。

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
