# gStore-rust 设计文档

用Rust重写[pkumod/gStore](https://github.com/pkumod/gStore)的主干。本文记录架构、与原C++实现的对应关系,以及取舍。

## 1. gStore是什么

gStore是北大的RDF三元组存储/图数据库:把RDF数据(三元组`<主语,谓语,宾语>`)存成图,用SPARQL做子图匹配查询。原仓库约13万行C++,模块包括Parser、KVstore、Database、Query、Signature(VS-tree)、Server、Cluster、GRPC等。

核心数据通路(主干)是:
```
RDF文件 → RDF解析 → 字典编码(string↔id) → 六重索引存储 → 落盘
SPARQL → SPARQL解析 → BGP查询计划 → 索引匹配+连接 → FILTER → 结果集
```

本次Rust重写聚焦这条主干,使其成为一个可独立运行、测试完备的RDF存储引擎。

## 2. 核心数据模型(对应`src/Util/GlobalTypedef.h`)

gStore用整数ID表示RDF项,并用ID区间区分类别:

| 类别 | C++类型 | Rust类型 | ID空间 |
|------|---------|----------|--------|
| 实体(IRI/空节点) | `TYPE_ENTITY_LITERAL_ID`(u32) | `EntityId`/`EntityLiteralId`(u32) | `[0, LITERAL_FIRST_ID)` |
| 字面量(Literal) | 同上 | 同上 | `[LITERAL_FIRST_ID, 2*LITERAL_FIRST_ID)` |
| 谓语(Predicate) | `TYPE_PREDICATE_ID`(int) | `PredId`(u32) | 独立空间`[0, …)` |

- `LITERAL_FIRST_ID = 2_000_000_000`,与原版一致。宾语位置的ID只要`>= LITERAL_FIRST_ID`就是字面量,否则是实体。这样一个`u32`既能当实体ID也能当字面量ID,查询时无需额外类型标志。
- 无效ID:实体/字面量用`u32::MAX`(`INVALID_ENTITY_LITERAL_ID`),谓语用单独的`NONE`。

RDF项(对应`src/Util/Triple.h`):
- `Term`:`Iri(String)` | `Literal{value, datatype, lang}` | `Blank(String)`
- `Triple`:三个`Term`
- 宾语类型`ObjectType{Entity, Literal}`,对应`TripleWithObjType`。

## 3. 字典层(对应`KVstore`的entity2id/literal2id/predicate2id + id2*)

`src/dict`提供string↔id双向映射,三套独立词典:实体、字面量、谓语。
- `string→id`:`HashMap<String, u32>`
- `id→string`:`Vec<String>`(下标即ID)
- 字面量对外暴露的ID = 内部下标 + `LITERAL_FIRST_ID`。

原版用4种B+树(SS/SI/II/IS)把词典也存在磁盘上。本重写在内存里用哈希表+向量,落盘时整体序列化(见§7)。把"磁盘B+树词典"列入重构待办。

> 现状(Phase 6 后):内存字典每个串只存一次(`Arc<str>`,串→id一侧是开放寻址的id表),三个interner是写时复制的`Arc`,克隆O(1);磁盘库的字典在`kvstore.kv`里(`*2id` B+树 + `id2*` IvArray),Stream模式经out-of-core的`DiskDict`与有界缓存访问;对外API返回`TermStr`(内存借用 / 磁盘`Arc<str>`)。删除后无人引用的id会被回收、最低id优先复用。见`docs/STORAGE_BACKEND.md`"字典"与"id回收"。

## 4. 存储层(对应`KVstore`的subID2values/objID2values/preID2values)

原版KVstore为每个主语/宾语/谓语维护一个值列表(VList),编码成紧凑字节块存进B+树。其布局(见`KVstore.h`顶部注释):
- `s2xx`:subID → 该主语的 (谓语,宾语) 列表 ⇒ 支持 `s→p*`、`s→o*`、`sp→o*`、`s→(p,o)*`
- `o2xx`:objID → 该宾语的 (谓语,主语) 列表 ⇒ 支持 `o→p*`、`o→s*`、`op→s*`、`o→(p,s)*`
- `p2xx`:preID → 该谓语的 (主语,宾语) 对列表 ⇒ 支持 `p→s*`、`p→o*`、`p→(s,o)*`
- 派生:`so→p`(`getpreIDlistBysubIDobjID`),`spo`存在性(`existThisTriple`)

`src/store`的`TripleStore`用同样的索引划分,覆盖全部7类三元组模式:

| 模式 | 已知 | 走的索引 |
|------|------|----------|
| `s p o` | 全已知 | exist检查 |
| `s p ?` | 主+谓 | s2po |
| `s ? o` | 主+宾 | so2p / s2po过滤 |
| `? p o` | 谓+宾 | o2ps |
| `s ? ?` | 主 | s2po |
| `? p ?` | 谓 | p2so |
| `? ? o` | 宾 | o2ps |
| `? ? ?` | 全未知 | 全表扫描 |

内存表示:`HashMap<key, Vec<(a,b)>>` + 排序去重,提供二分/合并连接所需的有序列表。

> 现状(Phase 6 后):三张有序邻接表放在一个`Arc`包着的`StoreData`里,`TripleStore`写时复制(克隆O(1),第一次写复制一次);批量写`insert_many`/`remove_many`一趟归并进每个被触及的列表;每个谓词的去重subject/object数精确维护。所有后端返回列表都遵守`TripleSource`的顺序契约(见`docs/STORAGE_BACKEND.md`"读面契约")。

## 5. 解析层(对应`src/Parser`)

- **N-Triples**(`src/parser/ntriples.rs`):逐行解析`<iri>`/`"literal"`(可带`^^<datatype>`或`@lang`)/`_:blank`,制表符或空格分隔,可选结尾`.`。还提供`parse_term`(从字典key反解出`Term`,供查询引擎FILTER求值用)。
- **Turtle**(`src/parser/turtle.rs`):N-Triples是Turtle的子集,故Turtle解析器是**主导入器**。gStore的多个数据集(如`data/lubm/lubm.nt`)实为Turtle——含`@prefix`指令、前缀名(`rdf:type`)。支持`@prefix`/`@base`(及SPARQL风格`PREFIX`/`BASE`)、前缀名、`a`关键字、谓宾列表(`;`)、宾语列表(`,`)、各类字面量。未支持`[ ]`空节点属性列表与`( )`集合(列入重构待办)。
- **SPARQL**(`src/parser/sparql`):手写词法器+递归下降解析,产出AST。支持:`PREFIX`/`BASE`、`SELECT`(变量列表/`*`/`DISTINCT`)、`WHERE`图模式、**`UNION`与嵌套组**、`FILTER`表达式(比较、`&&`/`||`/`!`、算术、`abs/str/lang/regex/...`等内建)、`ORDER BY`、`LIMIT`/`OFFSET`、`ASK`、`INSERT/DELETE DATA`。仍缺`OPTIONAL`/`MINUS`/聚合/属性路径/子查询(列入重构待办D)。

## 6. 查询引擎(对应`src/Query` + `src/Database/Executor|Join|Optimizer`)

- `Value`(`src/query/value.rs`):FILTER求值用的运行时类型(整数/浮点/字符串/IRI/布尔/类型化字面量),含SPARQL比较语义、有效布尔值(EBV)、`ORDER BY`全序。
- 图模式代数(`GraphPattern`:`Bgp`/`Join`/`Union`/`Filter`):WHERE子句解析成小型代数树。求值(`src/query/engine.rs`):
  - 单个**BGP**:把每个三元组模式编译成对索引的取数,按"已知位数+选择度"贪心定模式顺序,经统一(unification)逐步扩展绑定——索引下推。
  - **Join**:把连接树拍平成多个合取项,各自独立求值,再按"连通+结果小"贪心顺序用**哈希连接**(共享变量为键的兼容合并)两两合并。这避免了朴素左深求值在UNION引入新变量时的笛卡尔爆炸(实测使LUBM q9从挂死降到亚毫秒)。这是原版代价优化器的轻量替身(列入重构待办C)。
  - **Union**:两分支结果拼接。**Filter**:对内层结果按EBV过滤。
- FILTER在所在组求值时应用;DISTINCT、ORDER BY、LIMIT/OFFSET在投影阶段应用。
- `ResultSet`:变量名 + 行(每行是绑定的字符串值)。

## 7. 持久化(对应`KVstore`落盘 + `Database::save/load`)

数据库是一个`<name>.db`目录。本重写把字典与索引整体用`serde`+`bincode`序列化成几个文件(`dict.bin`、`store.bin`、`meta.bin`)。这换来正确、简单、可测的落盘/加载;代价是不像原版那样支持超内存数据集的按需分页。"磁盘原生B+树KVstore + mmap分页"列入重构待办(§见REFACTOR_BACKLOG)。

> 现状(Phase 6 后):有三种布局——快照(上面的bincode文件)、磁盘(`kvstore.kv` + `vstree.kv`,分页B+树,带WAL;Stream模式按需分页读取,默认的Materialize模式载入工作集)、RocksDB(`rocksdb/`)。磁盘与RocksDB布局可由批量构建(外部排序、自底向上)生成。见`docs/STORAGE_BACKEND.md`。

## 8. 与原版的主要差异(有意为之)

1. 索引在内存,落盘用整体序列化(非B+树分页)。
2. 暂不实现VS-tree签名索引(它是查询剪枝优化,不影响正确性)。
3. 连接顺序用贪心(连通+选择度)启发式 + 哈希连接,非原版的代价优化器。
4. SPARQL支持主干子集:SELECT/ASK、BGP、UNION、FILTER、ORDER/LIMIT/OFFSET、DISTINCT、INSERT/DELETE DATA;尚缺OPTIONAL/MINUS/聚合/属性路径/子查询。
5. 暂不含Server/HTTP/GRPC/Cluster/事务MVCC/推理。

以上差异均不影响"导入RDF→存储→SPARQL查询"主干的正确性,且都在REFACTOR_BACKLOG.md中作为可演进项记录。

> 现状:上面1–5是主干初版时的差异,后来的阶段已逐项补齐(VS-tree、代价优化器、完整SPARQL 1.1、Server/事务/推理等,见`docs/CPP_PARITY.md`);Phase 6 的查询路径与并发见§10、§11。

## 9. 测试策略

- **单元测试(UT)**:每个模块`#[cfg(test)]`内联测试,覆盖编码/解码、各访问模式、解析边界、FILTER语义、连接正确性。
- **数据测试(DT)**:`tests/`下端到端,用真实数据(`testdata/small`、`testdata/num`、`testdata/lubm`)构建数据库并断言SPARQL结果,含save/load往返、增删改后的结果一致性。LUBM(10万三元组)兼作规模与正确性回归。

## 10. 查询路径(Phase 6)

Phase 6(`docs/PHASE6_PLAN.md`)没有改动查询语义:每一项优化都要求计划、答案及无 `ORDER BY` 时的解序不变(另有说明的除外)。

- **语句与共享读路径**:`gstore::Statement`(`parse` / `is_update` / `text` / `query`)把解析与执行分开。`Database::read_as(&self, &Statement, cancel, pfn)` 与 `query_read(&self, &str, cancel, pfn)` 在共享的 `&Database` 上运行 SELECT / ASK / CONSTRUCT / DESCRIBE(查询缓存、VS-tree、命名图、函数、PFN、`SERVICE` 策略、CSR 等与 `query` 相同),UPDATE 在做任何事之前就被拒绝;`execute_as(&mut self, Statement, …)` 两种都能跑。`query` / `query_cancellable` / `query_as` 的签名不变,委托给它们。
- **每语句读错误**:每条查询、UPDATE 的 `WHERE` 求值与 UPDATE 操作、CSR 构建、导出都在一个 `ReadScope` 里运行;后端报告的读错误使这条语句失败,结果不进缓存(契约 C2,见 `docs/STORAGE_BACKEND.md`)。
- **代价模型只计数**:规划器与代价模型用 C1 计数方法(`count_*`、`distinct_p_of_*`)而不是物化列表,所有估计值不变;`TripleStore` 精确维护每个谓词的去重 subject / object 数,代价模型不再每条查询扫描(LUBM Q4 的热点)。哈希连接的表用 std-only 的 FxHash 风格哈希(输出顺序不变:左为主、右为次)。
- **候选集生成**(所有布局,所得集合相同):一个变量的常量边列表按长度从短到长求交;比当前集合长得多的列表不读,改为逐个测试集合成员;只有按谓词平均长度估计为短的列表才精确计数,其余按估计排序;已按契约有序的列表不再排序。一个 BGP 编译期间,候选生成与优化器要的精确计数来自同一个备忘(`query::count_memo`)。
- **扫描里的候选查找**:`?s p ?o` 若 subject 或 object 变量的候选集不到该谓词三元组数的 1/8,就逐个查找候选而不是扫描整个谓词;一端常量、另一端变量最多 64 个候选(且相对谓词平均列表长度很少)的模式,对候选做 `exists` 测试——同样的三元组、同样的顺序。LUBM Q4 因此不再随数据增长。
- **常量星形**:至多三个模式、都绑定同一个变量且另两个位置是常量(`?x p o`、`s p ?x`)的 BGP 不做计划搜索:解就是该变量的候选,按升序。
- **VS-tree**:
  - 内存树是带父指针的 arena 加实体 id → 叶子的定位表(构建与载入时重建,不序列化),`update` 是 O(树高);批量构建按签名 limb 再按 id 做非稳定排序(与旧的稳定排序产生相同的 `vstree.bin` / `vstree.kv` 字节)。
  - 内存树上,若一个 subject 变量已有不超过 max(1024, 实体数/32) 个 id 的精确候选集,就逐个 probe 而不遍历树,结果与"遍历 ∩ 候选"完全相等。
  - 删除不再使树失效:签名只会少位,存储的条目仍是可靠超集;`Database::vs_shrink_debt()` 计数,`rebuild_index()` 收紧。对有效的内存树,插入(以及回滚时重新加回被删的三元组)经 `VsTree::union_into` 把新边的位按 O(树高) 并进条目及其祖先;还不在树里的实体、以及得到第一条边的实体(新词或被复用的 id)取精确签名。`CLEAR`、推理,以及对只读 out-of-core 树的插入,仍使树失效。
  - **VS-tree 策略**(`gstore::query::VsPolicy`):`Auto`(默认)计算与 `Tree` 完全相同的候选集,只是在可证明精确的地方更便宜——候选集已满足查询签名的全部位时跳过树(*skipped*);out-of-core 树(`vstree.kv`)不读,剩余的谓词位对 store 检查(*store-checked*),没有候选集的变量从 store 的 posting list 取出带这些位的实体(*listed*);内存树的 probe 与遍历不变。`Tree` 是旧行为,`Off` 从不查树(答案相同,计划与无 `ORDER BY` 的解序可能不同)。优先级:`Evaluator::with_vs_policy` > `query::scoped_vs_policy`(每线程守卫)> `query::set_default_vs_policy` > 环境变量 `GSTORE_VS_POLICY=auto|tree|off`(只读一次;没有 `conf.ini` 键)。每线程的决策计数:`query::vs_stats()` / `reset_vs_stats()`(`skipped`、`store_checked`、`listed`、`probed`、`walked`、`seeded`、`off`)。
  - RocksDB 库打开时不再重建 VS-tree,直到 `rebuild_index()`(或 `vstree_on_open`);没有 `ORDER BY` 时,行序可能与经树过滤的求值不同(答案作为多重集相同)。
- **结果物化**:SELECT 结果按 4096 行一块物化;字典在磁盘上时,每块每个 id 空间一次排序批量解析。FILTER / BIND / ORDER BY 的 id → term / value 备忘(`IdMemo`)只用于 out-of-core 字典,每查询至多 8 MiB。
- **子串索引**:`CONTAINS` / `STRSTARTS` / `STRENDS` / 无元字符 `REGEX` 的 trigram 候选剪枝索引按字典 generation 缓存在数据库里(`Database::string_index()` / `cached_string_index()`),新增或释放一个词就失效;只对内存字典构建,建成后常驻内存(R2-DICT 的安静测量阶段,phase6 `0c9bdb6`:univ 38 时 430,954,544 字节)。out-of-core 字典上子串 FILTER 直接扫描(D5)。

## 11. 并发(Phase 6)

**`Database` 是 `Send + Sync`**(编译期断言保证):`&self` 方法可以并发运行,`&mut self` 方法需要独占。内部同步:查询缓存在 mutex 后面,条目是共享的 `Arc`,在释放锁之后才复制出来;磁盘写穿 sink 与待报告的持久化错误各在一个 mutex 后面;增量备份的检查点是原子量;`persist_lock` 串行化 `save`、`checkpoint`、`sync`、`backup`、`backup_incremental`。被 panic 毒化的锁会被恢复:缓存清空,磁盘 sink 标记为 broken(之后的提交点失败,直到重新打开)。

**锁顺序(契约 C8)**,从外到内,永不反向:

```text
ApiState(持有它时从不等待数据库锁)
  → 一个数据库的守卫(单库 Server 的 RwLock<Database> / ApiServer 的 DbCell)
    → 该库规则目录的锁
      → Database 内部:persist_lock → 磁盘 sink → persist_error → pager
        → DiskTriples 写闸门 → pager
```

叶子锁只持有几条指令、其下不调用任何东西:取消桥(cancel bridge)的注册表、`gapi::ops_db` 正在运行的统计刷新的注册表。没有线程同时持有两个数据库守卫、重复获取自己已持有的守卫,或不先放下共享守卫就升级为独占;同一库在同一线程上的重入 `DbCell::read` / `write` 在所有构建里都返回错误("refused instead of deadlocking")而不是死锁。每个逻辑 kvstore 写在整个操作期间持有 pager 写锁;`DiskDict::intern` 只拿 pager 写锁(闸门 → pager 从不反向)。

**HTTP 服务器:读共享、写独占**(`src/server.rs`、`src/http_api.rs`):

- 单库 `Server`(`gserver <dir>`):`RwLock<Database>`(容忍毒化的 `read_locked` / `write_locked`)。请求在取任何锁**之前**解析成 `Statement`(语法错误直接 `400`);读在共享守卫下经 `Database::read_as` 运行,UPDATE 在独占守卫下经 `execute_as` 运行,check_point 保存在释放守卫之前完成。`/status` 不取库锁,读服务器在每次写之后发布的计数(`DbCounters`,顺序锁)。
- 多库 `ApiServer`(`gserver --api`,REST 与上游 `/api`):每个已载入库一个 `DbCell`。共享:语句读(含事务所有者的读)、导出、备份、`loadcsr`、统计从未算过时的 `monitor` / `schema`、推理规则的 Check。独占:更新、begin / commit / rollback、checkpoint / save、upload、batch、unload / drop / restore / retire、关闭时的最后一轮、推理 Execute / Disable;安装新的 `[query]` 设置与发布展开(unwind)了的写段的数字只用 `try_write`(库忙时由下一个发现它空闲的请求或它的下一次写来做,`read_synced`)。无守卫:`/status`、`/monitor`、`/show`、`/api` `show` / `stat`,以及统计已算过的 `/api` `monitor` / `schema`。语句只解析一次(权限检查用的那次就是执行的那次)。
- **监控不等库锁**:每个库在每个独占段结束时和载入后,经顺序锁发布三元组数(事务期间是已提交的数)、实体 / 字面量 / 谓词数、变更计数、上次保存时间、索引 / 事务 / dirty 标志;`/api monitor` / `schema` 的类统计、subject 数与 schema 图由后台刷新(每库同时一个,共享守卫),请求最多等它 40 ms,否则用上一版数据的统计加当前计数作答;只有从未算过统计的库才在请求路径上计算。事务打开期间它们报告已提交状态(事务快照的统计,按 `/begin` 时的变更计数缓存)。`diskUsed` 只在改变了数据或保存了的独占段之后、释放守卫之后测量,从不在请求路径上遍历目录。
- **PFN**:`PfnRegistry` 把每个用户解析过的 `data.json` 缓存为共享的 `Arc<[PfnInfo]>`;模块加载按函数 single-flight,并发的首次调用等待同一次加载。
- 同一秒对同一库(同一目标路径)的多个备份,REST 与 `/api` 都算,按 `BackupClaim` 排成先后。

最终参考结果 `bench/results/phase6-final-lubm38.json`(univ 38)里,Q4 从 1 个客户端到 8 个客户端的吞吐:快照布局 `/sparql` 6223.2 → 33157.8 qps、`/api` 2504.1 → 11600.3 qps;Stream 磁盘库 `/sparql` 4801.1 → 19492.9 qps、`/api` 1967.6 → 9309.4 qps。

**写时复制快照**:

- **字典**:三个 interner 是 `Arc<InternerData>`,克隆 O(1),第一次改动只复制被改的那个 interner;插入已有词、查找、释放不存在的键从不复制。
- **`TripleStore`**:索引在一个 `Arc` 包着的 `StoreData` 里,克隆 O(1),对共享 store 的第一次写复制一次,不改变任何东西的写从不复制;命名图的 store 同样共享。
- **`ConcurrentDb`**(`src/concurrent.rs`):发布一个内存库的 `Snapshot` 是 O(1)——字典、默认图与命名图都共享;权威库的 store 保留一份 **spare copy**(`TripleStore::keep_spare_copies`):发布后的第一次写重放自上一版以来的变更(O(变更))把不再被读者持有的旧副本追上,而不是复制整个 store(仍有读者持有两版之前的快照时退回整份复制)。代价是内存里至多三份索引(库、spare、构造时的基准快照)。没有内存 store 的库(Stream 磁盘库、RocksDB)每次发布仍物化整个 store(在 `ReadScope` 里):`ConcurrentDb` 的快照是长期存在的版本视图,而 `DiskReadView` 在下一次写时就过期(pager MVCC 延后,D2)。
- **HTTP `/begin`**:事务的 `ReadSnapshot` 持有一个 `SnapStore`——内存 store(也包括 Materialize 磁盘库的工作集)的写时复制克隆,或 Stream 磁盘库的 `DiskTriples::read_view()`;RocksDB 库仍物化整个 store(D4)。字典与命名图共享,`/begin` 时记下已提交的三元组数;内存库上事务的第一次写复制一次 store,打开的事务在结束前一直持有 `/begin` 时的副本。最终参考结果的 `/api begin` 点(univ 38,20 个样本):快照布局中位数 0.38 ms、RSS 增长 0.1 MiB,Stream 0.44 ms、0.1 MiB,RocksDB 2132.55 ms、269.3 MiB(`bench/results/phase6-final-lubm38.json`)。

**`DiskReadView`:磁盘快照**。磁盘快照读在 store 的写闸门下运行(`DiskReadView::with_base`,经 `DiskSource`,在一个 `ReadScope` 里),求值器拿闸门的取消标志;请求自己的取消标志(`/cancel`、超过宽限期的关闭)由一个共享的小线程转发给它(取消桥,有转发登记时每 5 ms 轮询一次)。快照的 CSR 按视图的 epoch 作键,逐谓词构建,谓词之间检查闸门的取消标志(`CsrGraph::build_cancellable`),在自己的 read scope 里构建,出错不缓存。提交的 drain 取消在途的快照读并使视图过期;过期的读在当时的数据库上重跑(`ApiInner::read_committed`,在守卫下重新检查它是否仍被服务、是否有事务打开),回答已提交状态,三次尝试中最后一次持守卫读取;视图已关闭(库被卸载)则回答"not loaded"。所以 `/commit` 不再等待长的快照读。一个事务快照的非事务读只用库的守卫找到快照,进入闸门之前就放掉守卫——持独占守卫的 `/commit` 再 drain 闸门时从不等它。

**pin 与 id 回收**(`docs/STORAGE_BACKEND.md` "id回收"):被删词的 id 先 retire,在没有比 retire 更早的 `DictPin` 存活时才 release 供复用。

- 内存快照(`ConcurrentDb` 的、`/begin` 的)拥有写时复制的 id 版本,不需要 pin。
- 磁盘 `/begin` 快照只在自己的 `DiskReadView` 里读字典,写闸门在任何释放之前先 drain 它,所以取 `clone_unpinned()`:提交的释放不被推迟。
- `ConcurrentDb` 包着 Stream 库时,快照在读视图之外解析物化的 store,所以 pin 住共享的磁盘字典;它构造时的基准快照(`VersionView` 的后备)在整个生命周期都 pin 着,因而它存活期间不释放 id,被丢弃时才运行这些释放并持久化(丢弃时仍被读者持有的快照的 id 随库一起丢失,同重启)。
- Stream 库的释放只等待**比候选资格更早**的 pin:候选带 pin 时钟的戳、按戳排序,pin 期间不重复检查,pin 消失后每个候选检查一次;读错误使某个候选悬而未决时只有它等下一次检查(至多 4 次,之后留给 `gcompact --reclaim`)。被推迟的释放只在内存里,重启时丢失(孤儿词,`gcompact --reclaim` 补收)。

**已知限制**(各有后续项,见 `docs/REFACTOR_BACKLOG.md` §I):

- 快照布局开着 check_point 时,每个已确认的更新在独占守卫下重写整个快照,读者只能用保存之间的空隙:最终参考结果的混合负载(8 个读 Q4 的客户端 + 每 100 ms 一次单条插入,univ 38)里,快照布局 check_point on 的读吞吐 `/sparql` 12.5 qps、`/api` 10.1 qps,off 时 31966.8 / 10764.0 qps;磁盘布局按提交点增量提交,Stream on 时 19226.9 / 8675.0 qps(`bench/results/phase6-final-lubm38.json`)。
- `/api stat` 每个请求做一次阻塞的 200 ms CPU 采样(上游语义)。
- 在内存 `ConcurrentDb` 上,一次释放了某个词的删除会在已发布快照共享该 interner 时复制整个 interner(`Interner::retire` 里的 `Arc::make_mut`,O(词数));HTTP `/commit` 已在提交前放掉 `/begin` 快照,不受影响。
