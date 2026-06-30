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

## D. 完整SPARQL 1.1 ★中价值 —— ✅ 已完成

- **已实现**:`SELECT`/`ASK`/`CONSTRUCT`/`DESCRIBE`;图模式代数含`Bgp`/`Join`/`Union`/`Filter`/`LeftJoin`(OPTIONAL)/`Minus`/`Extend`(BIND)/`Values`/`SubSelect`/`Path`/`Graph`(命名图);聚合(`GROUP BY`/`HAVING`/全套agg含`DISTINCT`)与`(expr AS ?v)`;属性路径(`/ ^ | * + ? !`);`EXISTS`/`NOT EXISTS`(变量代入);完整UPDATE(INSERT/DELETE DATA、DELETE/INSERT WHERE、DELETE WHERE、LOAD、CLEAR/DROP/CREATE、`;`序列、WITH/USING);命名图GRAPH(查询/四元组DATA/CLEAR/持久化);xsd:dateTime比较;Turtle `[ ]`/`( )`。DT:`dt_sparql11`/`dt_update`/`dt_graph`/`dt_reason`。
- **缺(合理推迟/省略)**:`SERVICE`(联邦,需出网→解析即报错);GRAPH出现在DELETE/INSERT WHERE模板内;完整数值类型层级(现Int(i64)/Double(f64),比较正确);子查询相关性优化;30+图算法聚合(gpstore)。

## E. 并发、事务与MVCC ★中价值 —— ⚠️ 事务+快照隔离已完成,完整MVCC待做

- **已实现**:`Database::begin/commit/rollback`(undo日志单写者事务,原子+回滚,覆盖全部UPDATE与命名图);WAL(`pager`)存储层崩溃一致性;`concurrent::ConcurrentDb`(`src/concurrent`)—— 快照隔离:多读线程对immutable `Arc<Snapshot>`无锁评估,单写者串行提交后原子换快照,读者永不见半写态、不互相阻塞;`snapshot()`/`version()`/`update()`/`write()`。
- **缺**:每键版本链/细粒度MVCC、多写并发、并行加载(9线程)/OpenMP并行排序、快照GC(现靠Arc引用计数)、无锁读(现Arc-swap)。

## F. 服务化:HTTP API / gRPC / 控制台 / 集群 ★按需 —— ⚠️ HTTP已完成

- **已实现**:`src/server`(`gserver`bin)—— 零依赖HTTP/1.1 SPARQL端点:`GET/POST /sparql`(SELECT→SPARQL JSON、ASK→boolean、CONSTRUCT/DESCRIBE→N-Triples)、`POST /update`、`GET /status`;`Mutex<Database>`共享。
- **缺**:gRPC(`tonic`)、HTTPS/鉴权、内容协商、流式响应、集群分片与分布式查询(`src/Cluster`,工作量最大)。SPARQL `SERVICE`(联邦,需出网)同属此类,现解析→明确报错。

## H. 图算法(gpstore) ★低优先 —— ⚠️ 核心算法已完成

- **已实现**:`src/analytics`(`GraphView`)—— 从`TripleStore`建CSR邻接(实体为节点、三元组为有向边),提供出/入度、BFS单源最短距离+路径重建、弱连通分量(union-find:路径折半+按秩合并)、PageRank(含悬挂节点均匀再分配+收敛判定)、无向三角计数(排序归并求交)。
- **缺**:介数/接近中心性、Louvain社区发现、SCC(Tarjan/Kosaraju)、加权/带谓语边变体、topk子图邻近查询(原版`topk`)。

## G. 推理(RDFS/OWL) ★低优先 —— ✅ RDFS已完成

- **已实现**(`src/reason`):前向链物化到不动点 —— 子类传递、rdf:type传播、子属性传递+数据传播、domain/range类型断言;`Database::materialize_rdfs()`。跨实体/谓语id空间用字典桥接。DT:`dt_reason`。
- **缺**:OWL更丰富的公理(等价类/属性、传递/对称属性、`sameAs`等);查询时展开(现为物化)。

## 已做的小重构/clean-code(已直接落地,记录备查)

- 用Rust枚举`Term`统一IRI/Literal/Blank,替代原版到处用裸`string`+类型标志的写法。
- 用ID区间常量集中表达实体/字面量/谓语ID空间,替代分散的魔数。
- 索引访问统一为返回有序切片的方法,去掉原版手工`char*`+长度的裸指针接口。
- 错误用`Result<_, GStoreError>`显式传播,替代原版的bool返回+全局状态。
