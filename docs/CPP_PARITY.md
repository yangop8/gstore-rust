# Rust ↔ C++ 模块对照(差距矩阵)

由多agent审计workflow产出(逐模块对比原版C++ + 对抗式代码检视)。记录"已忠实实现"与"仍缺/有差异"。✅=核心已对齐,⚠️=部分,❌=未做。标注「合理省略」的是经判断不纳入当前主干的项。

## 查询主干(已基本对齐)

| 模块 | 状态 | 已对齐 | 仍缺 / 差异 |
|------|------|--------|-------------|
| model `Util/Triple`+`GlobalTypedef` | ✅ | ID区间(LITERAL_FIRST_ID)、实体/字面量判定、`Term`枚举(比C++裸string+类型标志更安全) | C++ predicate id是有符号int(-1无效),Rust用u32+u32::MAX——语义等价,表示相反 |
| dict `KVstore` *2id/id2* | ✅ | string↔id三套独立空间、字面量偏移;**Trie/共享前缀压缩词典**(`dict/prefix.rs`);磁盘B+树词典见kvstore | (前缀压缩阈值与C++具体策略略有差异,语义等价) |
| store `KVstore` *ID2values | ⚠️ | 六重访问模式、统计(pre2num/sub/obj) | C++紧凑字节编码(offset数组);字面量单独`objID2values_literal`;`getSubjectPredicateDegree`等度数API |
| parser SPARQL `Parser/SPARQL` | ✅ | SELECT/ASK/CONSTRUCT/**DESCRIBE**、BGP/UNION/OPTIONAL/MINUS/FILTER/BIND/VALUES/子查询/聚合/**EXISTS·NOT EXISTS**/属性路径(`/ ^ \| * + ?`)、**命名图GRAPH**、**完整UPDATE**(INSERT/DELETE DATA、DELETE/INSERT WHERE、DELETE WHERE、LOAD、CLEAR/DROP/CREATE、`;`序列、WITH/USING)、**SERVICE联邦**(`eval_service`+HTTP客户端) | 30+图算法聚合(gpstore专有扩展);GRAPH出现在DELETE/INSERT WHERE模板内 |
| query engine+value `Executor`/`EvalMultitypeValue` | ✅ | BGP连接、FILTER求值、合并连接、合成id、EXISTS(变量代入)、GRAPH(常量/变量图)、**xsd:dateTime/date比较**(解析为UTC瞬时,跨时区按时序;含闰年/日域校验) | 完整数值类型层级(int/long/float/decimal分别建模,现归并为Int(i64)/Double(f64),比较/排序已正确) |
| query planner+candidates+optimizer `Optimizer`/`PlanGenerator` | ✅ | 精确候选生成(常量边求交+传播)、NodeScore启发式、采样基数估计、卫星点延后;**真·DP多计划枚举**(左深`n·2ⁿ` DP得最优pattern序)、**二元(bushy)连接**(`3ⁿ`子集划分DP=`ConsiderBinaryJoin`,选出二元连接树由hash-join执行器跑)、**显式System-R代价模型**(NDV取自`pre2sub`/`pre2obj`谓语统计、再用精确候选集收紧)、**plan_cache**(结构同构BGP复用DP结果)。LUBM部分查询已实际走bushy计划且计数正确 | 12种命名物理join method(本引擎每pattern直接选最紧索引,等价);跨查询持久化plan cache(现为单查询evaluator内缓存) |
| signature `Signature` | ✅ | 944位EntityBitSet三段编码(逐位对齐) | VSTree原版疑似增量插入+分裂(我用bulk聚类构建);**注**:上游1.4主路径不用VS-tree(源码树无`VSTree`目录,`Strategy.cpp`里`vstree->retrieve`已注释),较早的gStore(注释所述"filter by vstree and then verified by join")每条查询先用VS-tree取候选。Phase 6 的VS-tree对齐见下表"VS-tree候选过滤" |
| db `Database` | ✅ | build/save/load/query/**完整UPDATE**、磁盘后端、VS-tree重建、**命名图**(持久化)、**事务**(begin/commit/rollback)、**QueryCache**、**RDFS推理**、**Schema抽取**、**监控stats** | 见下"系统级"剩余项(并发/集群/服务化等)。Phase 6:**载入不再物化store**——磁盘库以Stream模式打开时只读命名图与O(P)谓词统计,查询直接走B+树与out-of-core字典(默认的Materialize模式仍把工作集载入内存,见`docs/STORAGE_BACKEND.md`);RocksDB库打开时不再重建VS-tree。上游`Database::load`打开KVstore各索引(`IVArray`在打开时读入每个键的块指针,值按需经缓存读取),也不把三元组全量读入内存 |

## 存储引擎 kvstore vs `KVstore/SITree/IVArray/ISArray/VList`

| 项 | 状态 | 说明 |
|----|------|------|
| 分页文件+页缓存+B+树 | ✅ | 4KB块、分裂(按字节,永不超页;键最长2038字节)、前缀扫描、持久化/重开。Phase 6:页缓存是共享写时复制页(`Arc<[u8; 4096]>`)上的CLOCK(取代O(容量)扫描的LRU),脏页只经提交离开缓存;头部32个根槽+feature位掩码(`kvstore/format.rs`,V1/V2);每个逻辑写(intern一个词、增删一个三元组/四元组)是一个原子pager操作(no-steal,出错或panic整体回滚);B+树读经零拷贝`NodeView`,定长键节点二分查找 |
| VList紧凑值编码 | ✅ | `kvstore/vlist.rs`:有序id列表delta+varint变长编码,接入磁盘值(反)序列化(round-trip+size测试) |
| SITree/IVArray/ISArray分化 | ❌ | C++按用途分化(块管理器);我用单一通用BTree(纯内存微优化,合理推迟) |
| **删除+下溢合并/再平衡** | ✅ | `BTree::delete`:借位(redistribute,叶/内部节点经父分隔键旋转)+ 合并(merge,回收页入free-list)+ 根收缩(空叶清树、单孩内部节点降级);投影大小精确校验,节点永不溢页。`DiskStore::delete_triple` 三索引(SPO/POS/OSP)同步删除并递减计数 |
| 流式磁盘查询 + out-of-core字典 | ✅ | `TripleSource` trait抽象访问模式,内存/磁盘统一;`Evaluator<S>`+优化器/候选/planner泛型化。`DiskStore::query` 三元组索引按需经页缓存流式读取(不全量物化);**字典也留盘**(`kvstore/disk_dict.rs`:str→id/id→str按需走页缓存B+树,查询常量解析与结果物化均不全量载词典)。Phase 6:接入`Database`(Stream模式,`Backend::Disk(DiskTriples)`,读错误按语句上报,写按id、事务进overlay);字典有界缓存(`TermCache`/`KeyCache`),结果按排序批量解析(相当于上游`trySequenceAccess`)。原"剩余:流式路径上的VS-tree过滤"已由VS-tree策略`Auto`解决(见下表) |
| VS-tree候选过滤 | ✅ | 上游1.4不用VS-tree(见signature行);较早的gStore每条查询先遍历VS-tree取候选再join。本版保留VS-tree(944位签名逐位对齐)作为候选集的可靠超集过滤器。Phase 6:删除不再使树失效(签名只会少位,存储条目仍是超集;`Database::vs_shrink_debt()`计数,`rebuild_index()`收紧);插入经`VsTree::union_into`按O(树高)更新;内存树对小候选集逐个probe;**VS-tree策略**(`VsPolicy::Auto`默认、`Tree`、`Off`,环境变量`GSTORE_VS_POLICY`):在与store一致的树上得到与遍历整棵树完全相同的候选集(计划、答案、顺序都不变),但跳过不可能剔除候选的细化,对out-of-core树(`vstree.kv`)从store精确算出,查询不再读它的页;RocksDB库打开时不再建树 |
| 批量建库 | ✅ | `gbuild --disk` / `--rocksdb`(及`/api build`、REST `/build`的`layout=disk\|rocksdb`)是外部排序+自底向上写页镜像(RocksDB:SST ingest),有内存预算、多个源可并行解析、分阶段崩溃安全安装;id与内存构建相同。见`docs/STORAGE_BACKEND.md`"批量构建" |
| WAL/崩溃一致性 | ✅ | `flush`为原子提交:头页+脏页先写带CRC+commit标记的`<file>.wal`并fsync,再落主文件fsync,再清日志;`open`时重放完整committed日志(redo)、丢弃残缺日志。脏页只经提交落盘(evict优先清干净页,全脏则先提交) |

## 系统级子系统

| 项 | 状态 | 对应C++ | 判断/说明 |
|----|------|---------|------|
| 事务(commit/rollback) | ✅ | `Txn_manager` | `begin/commit/rollback`经undo日志实现原子性+回滚(单写者),覆盖所有UPDATE与命名图 |
| 并发(快照隔离 + 每键版本链MVCC) | ✅ | `GraphLock`/`Latch`/KVstore MVCC | `concurrent::ConcurrentDb`:多读并发(无锁评估immutable `Arc<Snapshot>`)+ OCC多写者(first-committer-wins、写键集历史)+ **每键版本链MVCC**(每triple-key版本表,读者按snapshot选可见版本/墓碑,版本GC回收最老活跃snapshot以下);读者不被阻塞。**并行加载**:`Database::build_from_ntriples_parallel`(行对齐分块多线程解析)。缺:OpenMP级排序细节。Phase 6:`Database: Send + Sync`,读语句走`&self`路径(`read_as`/`query_read`);两个HTTP服务器对每个库读共享、写独占(`RwLock`/`DbCell`),监控端点不等库锁;内存`TripleStore`与字典写时复制,`ConcurrentDb`发布与HTTP `/begin`对内存库为O(1),Stream磁盘库的`/begin`是`DiskReadView`(提交时取消并重跑在途快照读);RocksDB库的`/begin`仍物化(D4)。见`docs/DESIGN.md` §11 |
| RDFS/OWL推理 | ✅ | `src/Reason` | `src/reason`前向链物化:子类/子属性传递、type传播、domain/range(range不再给字面量加类型);`Database::materialize_rdfs` |
| 推理规则 `reasonManage` | ✅ | `src/Reason`、`ApiReasonOperation.cpp` | Phase 4:上游JSON规则的新增/列表/编译/执行/失效/查看/删除/校验(type 1–8),编译结果逐字节一致,规则存`<db>.db/reason_rule_files/`;偏离:精确撤回(账本)、规则校验防SPARQL注入、已执行规则不可重编译(见README「Reasoning」) |
| PFN 用户图函数 | ✅ | `ApiPFNOperation.cpp`、`GeneralEvaluation` pfn | Phase 4:`funquery`/`funcudb`/`funreview`按用户管理,`PFN.name({…})`求值并按上游返回`{"results":…}`;偏离:函数体为WebAssembly文本,在`wasmi`沙箱中运行(feature `pfn-wasm`),非g++编译+dlopen(见[`docs/PFN.md`](PFN.md)) |
| Schema抽取 | ✅ | `Database::getSchemaInfo` | `Database::schema()` 抽类与属性 |
| QueryCache | ✅ | `Database`/`QueryCache` | 读查询按SPARQL串缓存,任何写入即失效 |
| 监控/统计API | ✅ | `getDBMonitorInfo` | `Database::stats()`→`DbStats`(计数+索引/事务状态) |
| Turtle `[ ]` / `( )` | ✅ | `TurtleParser` | 空节点属性列表 + 集合(降级为rdf:first/rest/nil链),含嵌套 |
| 备份/恢复 + 更新日志(update.log) | ✅ | `Database::backup/restore/write_update_log` | `backup`/`restore`(一致快照)+ `backup_dir`(含磁盘KVstore/日志的文件级拷贝);`enable_update_log`按UPDATE追加长度前缀记录、`replay_update_log`顺序重放(重放时挂起记录、body不透明任意字节安全) |
| CSR邻接 + 图算法 + topk | ✅ | `src/Query/topk/` | `src/analytics`(`GraphView`):CSR + 出/入度、BFS最短路、弱连通分量、PageRank、三角计数、Tarjan SCC、Brandes介数、接近中心性、Louvain、k-core;**带权/带谓语边**(Dijkstra加权最短路、谓语过滤遍历、加权PageRank);**top-k子图匹配**(分支限界,可自定义打分,确定性tie-break)。缺:topk的GPU/并行实现细节 |
| 服务化:HTTP | ✅ | `src/Server`/`Api`/`ghttp` | `src/server`(`gserver`bin):HTTP/1.1 SPARQL端点(GET/POST /sparql、POST /update、GET /status),零依赖;**HTTP Basic鉴权**(可选凭据,401+WWW-Authenticate)、**内容协商**(SPARQL Results JSON/XML/CSV/TSV,`sparql_results.rs`)、**chunked流式**响应。缺:HTTPS(记为TLS反代/未来feature范围外)、gRPC |
| 集群/分布式分片 | ✅ | `src/Cluster` | `src/cluster`:进程内`ShardedStore` + **网络`NetworkShardedStore`**(`rpc.rs`长度前缀TCP RPC、`gnode`分片节点bin、`RemoteShard`客户端),跨本地/远程分片的scatter-gather查询 + **路由插入**。缺:副本/容错、gRPC线格式(序列化可换) |
| SERVICE联邦查询 | ✅ | — | `eval_service` + `http_client`:对远程SPARQL端点发HTTP查询并join本地解;SILENT语义(§18.5 join-identity)。缺:VALUES下推等优化 |
| ID freelist复用(BlockInfo链) | ✅ | `Database::allocEntityID`/`freeEntityID`/`freeLiteralID`/`freePredicateID` | 上游在非事务删除(`txn == nullptr`)后检查度数:实体、字面量、谓词的度数降到0即删字典项并把id放进`BlockInfo`空闲链(LIFO,后放先用;释放最高id时只把上限减一)。Phase 6(R4-RECLAIM)起本版同样释放:每条删除语句结束时(事务内在`commit`时)释放所有不再被三元组/四元组引用的实体、字面量、谓词(含图IRI),所有布局(快照、磁盘Stream/Materialize、RocksDB)都持久化;差异:复用**最低空闲id优先**(上游LIFO),释放最高id时连带下调(trim cascade),事务删除也释放(在提交时),`[system] id_reclaim=off`可关。早期本行写"gStore删除三元组时保留字典项、不适用",与上游源码不符,已更正 |
| RDFParser分批 + 并行加载 + 数值范围校验 | ✅ | `Parser/RDFParser` | `build_from_ntriples_batched`(有界内存分批flush)+ `build_from_ntriples_parallel`(多线程并行解析);dateTime日域/时域已校验 |

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
>
> 更新(2026-06-30,「三档全部写完」批次 — 5个opus worktree子agent并行 + 我集成):
> - **第一档**:out-of-core字典(`kvstore/disk_dict.rs`,词典也留盘按需查)、每键版本链MVCC(`concurrent.rs`)、std-TCP RPC网络集群(`rpc.rs`+`gnode`bin+`NetworkShardedStore`,路由插入+网络scatter-gather)。
> - **第二档**:VList变长值编码(`kvstore/vlist.rs`)、Trie前缀压缩词典(`dict/prefix.rs`)、带权/带谓语边图算法(Dijkstra/加权PageRank/谓语过滤)、top-k子图匹配(分支限界)、多线程并行加载。
> - **第三档**:HTTP Basic鉴权 + 内容协商(JSON/XML/CSV/TSV,`sparql_results.rs`)+ chunked流式;backup/restore + update.log(`db`);RDFParser分批导入。
> - 期间消化云端ultrareview(PR#3)3个真bug:`COUNT(DISTINCT *)`忽略DISTINCT、全谓语变量BGP的planner panic、子查询谓语变量走错字典(`tests/dt_review_fixes.rs`)。
> - 5个子agent各owns不相交文件集(kvstore+dict / concurrent / cluster+rpc / analytics / server),cherry-pick全部干净;我负责db+parser+query集成与解冲突。
> - 测试**392全过**(单线程;满并行时仅LUBM磁盘测试因~12测试二进制资源竞争偶发,disk/store代码与baseline字节相同、非回归),clippy `--all-targets`零告警。
> - **架构观察**(用户提出):gStore核心价值=VS-tree+优化器+图算法,存储后端应可插拔。现`TripleSource` trait已是干净只读seam(返回owned Vec、4实现、查询栈全泛型)。下一步:抽 StorageBackend(读+写+dict)、Database泛型化,先证可插拔(零依赖),再feature门控接入RocksDB(6 column family/复合key前缀range scan)。MySQL/MyRocks定位冷源而非热后端。
>
> 剩余大颗粒:可插拔StorageBackend抽象层 + RocksDB后端(下一阶段);完全out-of-core的VS-tree流式过滤;集群副本/容错;HTTPS(TLS反代)。
>
> 更新(2026-06-30,**可插拔后端 + 全量C++对标批次**):基于对克隆的C++ master(`pkumod/gStore`,~135K行/14模块)的**逐模块源码对读**(6个只读agent分头核实,跨模块误判已纠正),把三档差距补到对标C++。新增**528→544测试段位、双feature(默认 + `--features rocksdb`)全绿、clippy `--all-targets`零告警**。
>
> - **可插拔存储后端**:`store::{MutableStore, StorageBackend, Backend}` 写面seam + 运行期可选后端枚举;`Database` 持 `Backend`(默认内存,`--features rocksdb` 可选RocksDB)。`backend/rocks.rs`:3 column family复合key索引 + 词典CF + 统计计数,持久化/重开/查询parity测试。见 `docs/STORAGE_BACKEND.md`。
> - **第一档**:补齐内置标量函数(SUBSTR/REPLACE/IF/RAND/NOW/YEAR…SECONDS/TIMEZONE/TZ/UUID/STRUUID/IRI/BNODE/STRDT/STRLANG/LANGMATCHES/ENCODE_FOR_URI + 手写MD5/SHA1/256/384/512,`query/hash.rs`);gStore图函数接入SPARQL(SHORTESTPATHLEN/KHOPREACHABLE/CYCLEBOOLEAN…);**集群HA**(`cluster.rs`:Raft式选举/term/日志复制/quorum/心跳/failover/follower恢复);**用户自定义推理规则**(`reason`:规则定义/启停/effect计数);**PFN→Rust函数注册表**(`query/functions.rs`,替代.so dlopen)。
> - **第二档**:存储深度(`kvstore/overflow.rs` VList溢出链、增量VS-tree插入/分裂、`kvstore/string_index.rs` 子串二级索引、Pager RwLock细粒度latch);事务(redo日志、隔离级别选择、ID freelist回收、QueryCache LRU淘汰、build进度);查询优化(WCOJoin按对选nested-loop/hash、top-k记忆化树、planner大BGP 2-opt)。
> - **第三档**:服务API(`http_users.rs`/`http_api.rs`:用户/RBAC/会话、事务overHTTP、DB生命周期端点、backup/restore/export、批量insert/remove、查询/访问/事务日志 + monitor);8个CLI工具(gadd/gsub/gdrop/gshow/gbackup/grestore/gexport/gmonitor);RDF输入格式 N-Quads/TriG/RDF-XML(`parser/{nquads,trig,rdfxml}.rs`,**超过C++**)。
>
> **经判定的偏离**(C++机制对安全/轻依赖的Rust重写不合适,故对标能力、偏离机制):
> - 客户端SDK(C++/Java/Node/PHP/Python 5种):不照搬;Rust HTTP端点讲标准SPARQL 1.1 Protocol,任何HTTP客户端通用。
> - gRPC/Protobuf:保留零依赖std-TCP RPC(网络RPC+分布式查询能力已对标);gRPC线格式留作未来feature。
> - PFN .so动态插件:改为安全的Rust进程内函数注册表(不做unsafe dlopen ABI耦合)。
> - JSON-LD输入:**C++亦无**,非差距,跳过(其余RDF格式已超C++)。
>
> 更新(2026-06-30,**A类接线 + B类深度项**):
> - A类(实现了但没接线):增量VS-tree已接入`Database`更新路径(不再每次全量重建);有向Louvain(尊重边方向);StringIndex查询加速接线判定为侵入式、暂defer(结构已在)。
> - B类(最硬深度项):**完全out-of-core VS-tree**(`signature/disk_vstree.rs`:节点落盘+按需流式,查询期只读触达节点)接入磁盘库路径;**整数键IVArray**(`kvstore/ivarray.rs`,稠密定宽);**弹性集群**(`cluster.rs`:Raft config-change动态成员加入/离开 + 分片重平衡数据迁移 + 副本分片容错)。ISArray(字符串键数组)的进一步分化仍按需保留单BTree+overflow(已正确覆盖)。
> - 测试:默认**578全过**;`--features rocksdb`下lib477+dt_rocksdb4全过;clippy `--all-targets`双feature零告警。
>
> **至此对C++的功能差距已基本归零**。剩余仅:HTTPS(TLS反代/未来rustls feature)、ISArray按用途分块的存储微优化、集群副本快照增量同步的生产级硬化、StringIndex接入查询规划——均为深度/运维项,非功能缺口;以及已声明的机制偏离(gRPC线格式 / .so热加载 / 多语言客户端SDK / JSON-LD[C++亦无])。
>
> 另:基于`pkumod/gAnswer`源码逆向 + LLM-KBQA调研,产出 `docs/NLQA_DESIGN.md` —— LLM+gStore的自然语言问答系统(gNLQA)设计与开发计划。
>
> 更新(2026-07-02,**独立复检 + 补齐剩余gStore侧缺口**批次 —— 4个worktree子agent并行 + 集成):基于对官方 `pkumod/gStore` 与 `pkumod/gAnswer` README 的独立复核(不再只信本doc自评),定位并补齐了几处真实差距。
> - **两处本doc旧表述纠错**(与官方README矛盾):(1)官方README称C++ gStore "excludes property paths",而**本Rust版完整支持属性路径**(`/ ^ | * + ? !`)——此项我们**领先**;(2)官方README列出JSON-LD为受支持输入格式,故此前"JSON-LD C++亦无"的说法**有误**——现我们也补上了JSON-LD。
> - **图函数作为SPARQL内建:4 → 24**(`src/query/engine.rs`+`analytics`):`SSSP/SSSPLEN`、`KHOPCOUNT/KHOPNEIGHBOR/KHOPALLNEIGHBORS/KHOPENUMERATEPATH/KHOPREACHABLE`、`WCC/LOUVAIN/LABELPROP`、`BETWEENNESSCENTRALITY/CLOSENESSCENTRALITY`、`TRIANGLECOUNTING/CLUSTERINGCOEFF`、`PPR`(个性化PageRank)、`JACCARDSIMILARITY/SIMILARITY/DEGREECORRELATION/DIAMETERESTIMATION/BFSCOUNT`、`SHORTESTPATHS`、`CYCLEBOOLEAN/SIMPLECYCLEBOOLEAN`。已对齐/超过C++的~21个内建;并补齐了此前`analytics`库缺的算法(clusteringCoeff/diameter/Jaccard/degree-correlation/labelProp/PPR/similarity/k-hop枚举)。
> - **RDF输入格式补齐**:`src/parser/{jsonld,rdfa}.rs`(零依赖、手写、common-subset,与既有hand-rolled parser风格一致)。`parser::parse_file_by_ext` 统一按扩展名分派(gbuild/gadd/gsub 共用);http_api `/upload` 与 `parse_rdf` 也支持。至此输入格式:N-Triples/N-Quads/Turtle/TriG/RDF-XML/**JSON-LD**/**RDFa**(仅RDFa相对官方仍属子集)。
> - **服务化HTTP**:`/upload`(上传RDF入库)、`/export` 下载头、内置**Web控制台**(`GET /`·`/console`,`http_api`+`server` 均可)、**任务注册表 + `/tasklist` + `/cancel`**。
> - **事务**:四级命名隔离(`IsolationLevel`:ReadUncommitted/ReadCommitted/RepeatableRead/Serializable,`concurrent.rs`);**增量备份/恢复**(`Database::backup_incremental`/`restore_incremental`,基于update.log checkpoint)。
> - **查询取消**:引擎级协作式取消(`Evaluator::with_cancel` + `Database::query_cancellable`,在pattern/phase边界检查,`GStoreError::Cancelled`);已接入http_api `/sparql`。**注**:内置HTTP服务是单线程、整请求持state锁,故它自身无法在一条查询运行中投递并发`/cancel`——取消对"自驱线程的库使用者"及未来多线程前端有效(见 `Database::query_cancellable` 文档)。
> - 测试:默认 **803全过**;`--features rocksdb` **650全过**;clippy `--all-targets` 双feature零告警。图函数经`gquery`真端到端冒烟(SSSPLEN/TRIANGLECOUNTING/CLOSENESS/PPR/JACCARD…)。
>
> **仍属刻意偏离(不补)**:gRPC线格式、多语言客户端SDK(5种)、.so动态编译UDF(改用安全Rust函数注册表)、发行工程(Docker/RPM/DEB/loongarch)。**仍为深度/子集项**:RDFa/JSON-LD的完整1.1语义(现为common-subset)、gzip/异步/回调HTTP(手写DEFLATE性价比低,留feature)、内置HTTP服务多线程化(使在途查询的HTTP `/cancel` 可投递)。
>
> gAnswer侧:gNLQA 是**换代实现**(LLM Text-to-SPARQL 取代词典+子图匹配),非同款算法移植;能力对比见 `docs/NLQA_DESIGN.md`。
>
> 更新(2026-07-02,**深度/生产硬化批次** —— 4个worktree子agent并行 + 主线程集成):把此前列为"深度/子集项"的一档全部补齐(除刻意偏离项)。8项:
> - **HTTP多线程化 + 并发`/cancel`**:`Server`/`ApiServer` 改 per-connection worker(`Arc`共享state),请求不再全程持state锁——在途查询运行时并发`POST /cancel`可投递并中止之(`concurrent_cancel_aborts_running_query`),协作式取消端到端闭环。
> - **HTTP gzip**(feature `gzip`,`flate2` 纯Rust `miniz_oxide` 后端,默认关):按`Accept-Encoding`压缩缓冲响应;默认构建仍零依赖不压缩。
> - **HTTPS/TLS**(feature `tls`,`rustls` ring后端 + `rustls-pemfile`,默认关):`serve_forever_tls`/`load_tls_config`,`gserver` cfg门控`--tls-cert/--tls-key`;`tls_https_round_trip` 真HTTPS往返。
> - **StringIndex接查询规划**:`CONTAINS`/`STRSTARTS`/`STRENDS`(及无元字符`REGEX`)对 s/o 变量经字典trigram索引剪/播候选(sound超集,精确FILTER兜底),补齐 `kvstore::string_index` 的"wiring follow-up"。
> - **集群副本容错生产级硬化**:per-shard N副本组(容忍 ⌊(N-1)/2⌋ 故障)+ 日志压缩快照 + InstallSnapshot式落后follower追赶 + 副本重入收敛。
> - **out-of-core VS-tree内存路径**:统一签名过滤seam(内存树/磁盘流式树同实现),内存库也可走流式过滤;候选集与内存树逐一等价验证。
> - **store按用途分块 + 度数API**:IVArray(定宽稠密)/ISArray(字符串键,新增)分化 + subject/object度数、pre2sub/pre2obj/pre2num、getSubjectPredicateDegree 维护式统计。
> - **数值类型分层**:Int(i64)→BigInt(i128)→Decimal(精确mantissa×10⁻ˢᶜᵃˡᵉ)→Double(f64) 数值塔 + XPath类型提升;超i64整数与xsd:decimal的比较/排序/±× 精确(0.1+0.2==0.3、1.0==1.00),÷ 与含double走f64。
> - 4个子agent各owns不相交文件集(server+http_api / cluster+rpc / signature / kvstore),我owns查询侧(StringIndex+数值塔),集成用整文件覆盖 + db/mod.rs按hunk合并,全部干净。
> - 测试:默认workspace **689全过**;`--features rocksdb` 编译+lib全过;`--features gzip`/`tls` 各自编译+测试(含 `gzip_compresses_when_accepted`/`tls_https_round_trip`)+ clippy `--all-targets` 零告警(默认/gzip/tls/rocksdb 四组合)。
>
> **至此原"深度/子集项"仅剩**:RDFa/JSON-LD的完整1.1语义(仍common-subset);Decimal为i128界(非任意精度,刻意零依赖界)、÷与SUM走f64;StringIndex惰性按查询构建(未做持久化Database级索引)。**刻意偏离仍不补**:gRPC线格式、多语言客户端SDK、.so热加载UDF、发行工程。
>
> 更新(Phase 4,**推理规则 + PFN**):`reasonManage`(type 1–8,上游JSON规则全生命周期,精确撤回)与 PFN 用户图函数(`funquery`/`funcudb`/`funreview` + `PFN.name({…})`)已接入 `/api`。上文"PFN .so动态插件:改为安全的Rust进程内函数注册表"的偏离据此更新:PFN 现以**沙箱化 WebAssembly**(feature `pfn-wasm`,`wasmi` 解释器)实现上游的按用户函数管理与调用协议,仍不做 dlopen;上游 C++ 函数体需改写为 WAT。详见 [`docs/PFN.md`](PFN.md) 与 README 的 Reasoning / PFN 两节。
>
> 更新(Phase 5 集成,GAP_PLAN §4.2 + §5):**zip**(feature `zip`,默认关):`/api` build/batchInsert/batchRemove 读 zip 包、`backup compress=true` / `restore` zip 备份、`export compress=true` 与 `format=nt|nq`(含命名图时默认 N-Quads),`gbuild`/`gexport`/`gbackup`/`grestore` 同步支持;**日志**:`src/logging.rs` 按上游 `log_mode` 属性文件 + 扩展键分级、按天滚动与保留,REST 查询写 JSON 查询日志,访问日志带 IP;**老工具**:上游 1.3 参数拼写、`grestore` 参数顺序与上游一致、`gshow` 列出全部库、`gbuild` 建空库与防覆盖;**发行工程**:Dockerfile、systemd unit、deb/rpm 元数据(见 `docs/DEPLOY.md`)——此前"刻意偏离"中的 Docker/RPM/DEB 已补齐(loongarch 仍未做)。
>
> 更新(Phase 4 与 Phase 5 合并):两阶段在同一主线上衔接——`gconsole` 的 reason 系列(`addreason` … `deletereason`)与 pfn 系列(`showpfn`/`addpfn`/`mdfpfn`/`delpfn`/`buildpfn`)命令对真实服务端走完全生命周期,PFN 查询的 `{"results": …}` 按上游 `pfn_type` 方式打印;推理规则与其账本随 zip 备份、`gbackup -z`/`grestore x.zip`、`drop` 的 `.bak` 保留(`export` 与上游一样只导出数据);导入流水线分域铸造的空节点上推出的三元组可精确撤回;PFN 函数按用户存于 `pfn_base_path`(打包与容器布局 `/var/lib/gstore/dbhome/pfn`,备份见 `docs/DEPLOY.md`);推理与 PFN 事件写入诊断日志,其操作进入 `/api` 访问与查询日志;Docker 镜像默认开启 `pfn-wasm`,CI 特性矩阵含 `gzip`/`tls`/`zip`/`readline`/`pfn-wasm`。上文"刻意偏离仍不补"中的".so热加载UDF"由沙箱化 WASM PFN 取代,"发行工程"已在 Phase 5 补齐。
>
> 更新(Phase 6,**规模与性能**,`docs/PHASE6_PLAN.md`):本阶段对标的是上游的规模表现,对照表中改动的行:
> - **ID回收**("ID freelist复用"行):此前本doc写"gStore删除三元组时保留字典项",与上游源码不符——上游在非事务删除后把度数降为0的实体、字面量、谓词的id放回`BlockInfo`空闲链。本版现在所有布局都释放这些id,最低id优先复用(上游LIFO),事务删除在提交时释放;`gcompact --reclaim`补收孤儿词。细节见`docs/STORAGE_BACKEND.md`"id回收"。
> - **VS-tree**("VS-tree候选过滤"行):上游1.4不用VS-tree;本版保留,并让磁盘库在默认策略下由store精确算出与树相同的候选集,不再读`vstree.kv`。
> - **存储与载入**(db行、"分页文件+页缓存+B+树"行、"流式磁盘查询"行):磁盘库Stream模式打开时不物化store;页缓存CLOCK+共享写时复制页;逻辑写原子;格式注册表(`kvstore.kv` V1/V2,`FREE_IDS`);批量建库。
> - **并发**("并发"行):读共享/写独占的服务器,写时复制快照,`DiskReadView`。
> - **与上游lubm10M同量级**(计划§2 G7):univ 82(11,135,159个三元组,`bench/results/phase6-final-lubm82.json`)对上游gStore 0.5.0在lubm10M上的数字(`formal_experiment.tex`:23 µs/三元组、264 B/三元组,q1≈Q2 144 ms,q13≈Q14 223 ms):快照布局 / 磁盘布局(默认Materialize模式)建库6.27 / 4.45 µs/三元组,库大小83.2 / 126.2 B/三元组,Q2 85.26 / 71.19 ms,Q14 141.28 / 165.29 ms,每项都不高于上游。注意:生成器不同(LUBM-like而非UBA)、硬件不同、上游测试框架每条查询约23 ms的下限,本版数字是进程内的热中位数(5个进程各5次的中位数取最小)。Stream模式(可选)的Q14为876.09 ms,是上游的3.93倍。
> - **仍属差异/未做**:RocksDB库打开时仍把字典读入内存、无原生快照、`/begin`物化(D4);段布局/磁盘格式v2(D1)等延后项见`docs/REFACTOR_BACKLOG.md` §I。
