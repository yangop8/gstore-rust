# 可插拔存储后端(Pluggable Storage Backend)

## 动机

gStore的核心价值是 **VS-tree签名索引 + 代价优化器 + 图算法集成**,而非自研的字节级存储引擎。因此存储后端应当**可配置/可插拔**:同一套查询栈既能跑纯内存,也能跑磁盘B+树,未来还能接 RocksDB(MyRocks)、甚至 MySQL。

## 现状:读面已是干净的 seam

`src/store/mod.rs` 的 `TripleSource` trait 是只读查询面(6种三元组访问模式 + 优化器需要的统计:`pred_card`/`pred_distinct_subj`/`pred_distinct_obj` 等),**返回 owned `Vec`**(对外部KV后端友好,不强求借用内部内存)。实现:

- `TripleStore`(纯内存,三套有序邻接图 s2po/o2ps/p2so;Phase 6 起写时复制,见[下文](#内存布局写时复制的-triplestore))
- `DiskStore`(磁盘B+树KVstore,`kvstore/store.rs`)。它自己的 `TripleSource` 实现把读错误当作空结果,查询请经下面两个会上报错误的包装
- `DiskSource`(把任意 `&DiskStore` 包成"读错误按语句上报"的读者)与 `DiskTriples`(Stream 模式的 `Backend::Disk` 载体:读错误按语句上报,写按 id,带事务 overlay)
- `OverlayView`(事务 overlay 视图:`(base − removed) ∪ added`,完整实现 `TripleSource`,含 C1 计数)
- `RocksStore`(`--features rocksdb`,`src/backend/rocks.rs`)
- `ShardedStore`(进程内 hash 分片,scatter-gather)
- `NetworkShardedStore`(std-TCP RPC 跨节点分片,实验性)

整个 `Evaluator<S: TripleSource>`、planner、optimizer、candidates、analytics(`GraphView` 由 `TripleSource` 构建 CSR)都坐在这个 seam 之上,**与存储实现解耦**。读面的顺序契约与计数方法见 [§读面契约](#读面契约顺序契约与-c1-计数)。

## 本阶段(Phase 1,已落地):补齐写面 seam

新增 `src/store/mod.rs`:

```rust
pub trait MutableStore {                 // 写面(读面 TripleSource 的对偶)
    fn insert(&mut self, t: IdTriple) -> bool;
    fn remove(&mut self, t: IdTriple) -> bool;
    fn bulk_load(&mut self, triples: Vec<IdTriple>);
}
pub trait StorageBackend: TripleSource + MutableStore {}   // 可读可写的完整后端
impl<T: TripleSource + MutableStore> StorageBackend for T {}
```

- 对 `TripleStore` 实现 `MutableStore`(转发到既有 inherent 方法)。
- 两个 trait 都保持 **object-safe**(参数/返回均为具体类型),为将来 `dyn StorageBackend` 留路。
- 单测 `storage_backend_seam_is_generic`:**一个泛型函数同时驱动写(MutableStore)与读(TripleSource)**,不出现任何具体类型——可插拔的类型级证明;同一函数体可原样接受未来的 RocksDB/MySQL 后端。

> 这一步零新依赖、可逆,确立了"写"这一缺失的 seam(读 seam 早已存在)。
>
> 更新(Phase 6):`MutableStore` 增加带默认实现的 `insert_many` / `remove_many`(默认逐条调用 `insert` / `remove`);`TripleStore` 以排序归并重写它们(见[下文](#内存布局写时复制的-triplestore)),`Backend` 把这两个方法分派到各变体。

## Phase 2(下一步):Database 接入 + 运行期可配置

> 历史记录:本节是当时的设计选项,已按路线 A 落地(见"实现状态")。

把 `Database` 对存储的硬编码依赖换成 seam。两条可选实现路线:

| 路线 | 做法 | 取舍 |
|------|------|------|
| **A. 配置枚举(推荐)** | `enum Backend { Memory(TripleStore), Disk(DiskStore), #[cfg(feature="rocksdb")] Rocks(RocksStore) }`,对枚举实现 `TripleSource`+`MutableStore`(按 variant 分派);`Database` 持有 `Backend`,由 `db.conf`/构造参数选择 `backend=memory\|bptree\|rocksdb` | 运行期可配(正是"可配置后端"诉求);静态分派+分支(无 vtable);改动集中、无泛型扩散 |
| B. 泛型化 `Database<B>` | `Database<B: StorageBackend>` | 零分派开销;但 `Database` 是 facade,全调用点(server/bins/tests)都要带类型参数,扩散大 |
| C. `Box<dyn StorageBackend>` | 动态分派 | 最灵活;热路径每次访问模式调用走 vtable,有(轻微)代价 |

字典侧:已有 `Dictionary::from_backing(Arc<dyn DiskTermSource>)` 抽象其磁盘后端(out-of-core 用),RocksDB 后端可复用此 seam 把 str↔id 放进 RocksDB CF。

## Phase 3:RocksDB 后端(feature 门控)

> 历史记录:本节是当时的设计;落地情况见"实现状态"与 [§RocksDB](#rocksdb)。

- Cargo `--features rocksdb` → `rocksdb` crate(librocksdb 原生 C++);默认不启用,保持轻依赖。
- 6 个排列索引 → 6 个 column family,key 用复合字节串前缀:
  - `SPO` CF:key=`s|p|o` → `po_by_s`/`o_by_sp`/`exists` 走前缀 range scan
  - `POS` CF:key=`p|o|s` → `s_by_po`/`so_by_p`
  - `OSP` CF:key=`o|s|p` → `ps_by_o`/`p_by_so`
- 字典:`term→id`、`id→term` 两个 CF(或复用 `DiskTermSource`)。
- 统计(`pred_card`/`pred_distinct_*`):计数 key,写时维护或用 RocksDB merge operator。
- 白送:block-cache、bloom filter、压缩、compaction、快照。

## MySQL / MyRocks 定位

MyRocks = MySQL 套 RocksDB 存储引擎,**经 SQL 访问**。作为**热查询后端是次优**:优化器发大量细粒度候选查询,SQL 往返 chatty 开销大。更合适的角色是**冷存储 / source-of-truth / 导入导出连接器**(查询时灌进 RocksDB 或内存)。若只为 RocksDB 的存储收益,直连 RocksDB 即可,无需 SQL 层。

## 实现状态(Phase 2 + Phase 3,已落地)

**Phase 2(Database 接入,路线 A):** 新增 `enum Backend { Memory(TripleStore), #[cfg(feature="rocksdb")] Rocks(RocksStore) }`(`src/store/mod.rs`),对其实现 `TripleSource`+`MutableStore`,用一个 `backend_dispatch!` 宏做**静态分派**(`match` 分支,无 vtable)。`Database` 改为持有 `store: Backend`,默认构造仍走 `Backend::Memory(TripleStore::new())`,故 feature OFF 时行为与既有 393 个测试**逐字节不变**。`build_vstree` / `reason::materialize` 已泛型化为 `<S: TripleSource(+MutableStore)>`,`schema()` 的 `predicates()` 经 `Backend::predicates()` 分派,均不靠 downcast。`Database::store()` 仍返回 `&TripleStore`(经 `Backend::as_memory()`,内存后端专用,供快照 MVCC/测试)。

> 更新(Phase 6):`Backend` 与 `backend_dispatch!` 移到 `src/store/backend.rs`(仍以 `gstore::store::Backend` 导出,仍是静态分派),并多了第三个变体 `Backend::Disk(DiskTriples)`(Stream 模式的磁盘库)。`Database::store()` 对没有内存副本的库——Stream 模式磁盘库与 RocksDB 库——会 panic,这类库请读 `Database::backend()`。

**Phase 3(RocksDB 后端,`--features rocksdb`):** `src/backend/rocks.rs` 的 `RocksStore` 实现 `StorageBackend`:

- 3 个排列 column family——`spo`(键 `s‖p‖o`)、`pos`(键 `p‖o‖s`)、`osp`(键 `o‖s‖p`),id 用 4 字节**大端** u32,故字节序==数值序,前缀 range scan(`iterator_cf`)即可回答 6 种访问模式,返回已排序结果以与 `TripleStore` 完全对齐。
- `stats` CF 用**维护式计数器**(引用计数,插入/删除时 O(1) 维护;计数归零即删键)给出**精确**优化器统计:`triple_count`/`distinct_subjects`/`distinct_objects`/`num_predicates`(4 个全局值另缓存于内存)、`pred_card`/`pred_distinct_subj`/`pred_distinct_obj`(逐谓词),避免热路径上的全表扫描。
- `RocksStore::open(path)` 持久化并可重开;`Database::build_rocksdb_from_str` / `build_rocksdb_from_files` / `open_rocksdb` / `is_rocksdb` 提供 Rocks 后端的库级 API,`Database::open` 自动识别。三元组、字典(`dict` CF,新词先于引用它的三元组写入)、命名图(`gspo` CF)与库名(`meta` CF)全部存于 `dir/rocksdb` 这一个 RocksDB 实例,提交点 fsync WAL,无需另存 `dict.bin`;备份走 RocksDB checkpoint。旧版 `dict.bin` 布局在首次打开时迁移。
- **CLI 入口**:`gbuild --rocksdb <name> <data.nt>` 建库、`gserver <dir> --rocksdb` 起 HTTP 服务(均需 `--features rocksdb` 编译);feature OFF 时给出明确报错。gNLQA 通过 HTTP SPARQL 指向该服务即可端到端跑在 RocksDB 上(与后端无关)。
- 测试 `tests/dt_rocksdb.rs`(feature 门控):泛型 `fn <B: StorageBackend>` 对 `TripleStore` 与 `RocksStore` 跑同一指纹证明结果一致;close→reopen 持久化;经 `Evaluator` 跑真实 SPARQL 的行数对拍;以及经 `gbuild --rocksdb` 二进制建库、`open_rocksdb` 重开查询的 CLI 端到端用例。

当时仍未做(backlog):Rocks 后端的字典 out-of-core 化(复用 `DiskTermSource` seam)、merge-operator 式计数、按谓词的 prefix-extractor 调优、Rocks 后端的事务/快照接线。

> 更新(Phase 6):RocksDB 的读路径、精确计数、批量构建与调优见 [§RocksDB](#rocksdb)。字典 out-of-core 化与原生快照仍未做(`docs/PHASE6_PLAN.md` §8 D4);事务早已接线(undo 日志,见 `db/persist.rs` 模块文档)。

---

## Phase 6(规模与性能):现状

以下各节描述 Phase 6 结束时(phase6 分支)的实现,契约编号 C1–C9 指 `docs/PHASE6_PLAN.md` §4。文中的数字都注明出处:`bench/results/*.json` 中提交的结果(用 `gstore-bench report` 生成表格),或某个包"安静测量阶段"(quiet stage)的实测记录。

之后的 review-fix campaign(对 Phase 6 代码 `7bbf4ae` 的代码审查,分五轮修复,2026-11)对这些节的更正就地标为"更新(review-fix campaign)";它新增或改写的提交协议、文件锁、替换与恢复规则集中在[下面的一节](#review-fix-campaign提交恢复与格式)。这次修复**没有改变任何数据库文件格式**,也没有新增 feature bit。

### 三种布局与布局检测

| 布局 | 目录里的标志 | 查询时的 `Backend` | 说明 |
|---|---|---|---|
| snapshot(快照) | `meta.bin` | `Memory(TripleStore)` | bincode 快照:`dict.bin`、`store.bin`、`named.bin`、`vstree.bin`、`meta.bin`;打开即全量载入,`save()` 时整体重写 |
| disk(`gbuild --disk`) | `kvstore.kv` | Stream 模式:`Disk(DiskTriples)`;Materialize 模式:`Memory(TripleStore)` | 分页 B+ 树文件(带 WAL),三元组、字典、命名图都在里面;旁边的 `vstree.kv` 是 out-of-core VS-tree |
| RocksDB(`gbuild --rocksdb`) | `rocksdb/` | `Rocks(RocksStore)` | 一个 RocksDB 实例存全部内容 |

检测顺序就是表的倒序:`rocksdb/` 优先于 `kvstore.kv`,`kvstore.kv` 优先于快照(`Database::storage_kind`,不变量 C9)。向一个目录 `save` 某种布局会删掉其它布局的文件,所以 save 之后目录不会有歧义;被替换的 `rocksdb/` 先持久地改名移开再删,半删的 store 不会抢在新布局之前被检测到。

### 读面契约:顺序契约与 C1 计数

`TripleSource` 的文档里写明了每个实现都必须遵守的**顺序契约**,计划与无 `ORDER BY` 时的解序因此与后端无关:

- `po_by_s` 返回 `(p, o)`,`ps_by_o` 返回 `(p, s)`,`so_by_p` 返回 `(s, o)`,每个列表按对升序、无重复;
- `o_by_sp`、`s_by_po`、`p_by_so`、`subs_by_p`、`objs_by_p` 和键列表 `subject_keys` / `object_keys` 升序、无重复;
- `iter_all` 按 `(p, s, o)` 顺序返回三元组。

**C1 计数方法**(provided methods):`count_po_by_s`、`count_o_by_sp`、`count_p_by_so`、`count_ps_by_o`、`count_s_by_po`、`count_so_by_p`(`u64`,恰为对应列表的长度)、`distinct_p_of_s`、`distinct_p_of_o`、`has_subject` / `has_object` / `has_predicate`。默认实现经列表方法定义,对任何实现都正确;后端可以覆盖它们以免物化列表(`TripleStore` 用切片算术,`DiskStore` / `DiskTriples` 用前缀计数,`RocksStore` 用维护式计数器,`OverlayView` 用精确算术),但**覆盖必须与默认值逐一相等**——代价模型、计划和解序因此不变。读出错时 `has_*` 回答 `true`:拿不准时绝不把一个 id 当作无人引用(id 回收依赖这一点)。

### 每语句读错误(C2,`ReadScope`)

`gstore::store::read_scope`:`ReadScope::enter()` 返回一个按线程、可嵌套、`!Send` 的守卫;`note_read_error(msg)` 记入当前线程最内层的 scope,没有 scope 时计入进程级计数 `unscoped_read_errors()` 并写日志;`take_error()` 取出。

- 每条查询、UPDATE 的 `WHERE` 求值、UPDATE 操作本身(删除时查词)、CSR 构建、导出(`gexport`、`/api export`、`Database::export_ntriples`)、事务快照查询、`ConcurrentDb` / `VersionView` 查询都在 scope 里运行;记录到错误的语句失败("a read from the store failed while …"),结果也不进查询缓存。错误只属于发生它的那条语句,不会落到别的线程上。
- 会上报的后端:`DiskTriples` / `DiskSource`、`DiskDict`、`RocksStore`。它们只经 `note_read_error` 报告,从不清除共享状态。`DiskStore::query` 在自己的 scope 里经 `DiskSource` 求值,读错误使查询失败;但 `DiskStore` 自身的 `TripleSource` 实现仍把读错误当空结果,所以 Stream 模式只经 `DiskTriples` 或 `DiskSource` 求值。
- 求值器是单线程的;将来若并行求值,必须显式把 scope 交给工作线程。

### 磁盘布局的两种查询模式:Stream 与 Materialize

`kvstore.kv` 数据库按 `DiskQueryMode` 打开(`src/db/open_options.rs`):

- **Stream**:查询直接在 B+ 树上求值(`Backend::Disk(DiskTriples)`,读错误按语句上报),字典是同一 pager 上的 out-of-core `DiskDict`(每个数据库打开一次,所有语句与快照视图共用;字典与 store 共用一个 id 空间)。打开时只读命名图(留在内存,D11)和 O(P) 的谓词统计,其余按需读。
- **Materialize**:Phase 6 之前的路径。打开时把字典、默认图和命名图载入内存工作集(`Backend::Memory`)供查询,每次变更**按字典 key** 写穿到 `DiskStore`,工作集的 id 不必与 store 的 id 对齐。事务内的变更排队,`commit` 时才落到 store,`rollback` 丢弃。工作集保留 store 里的 id(被释放的 id 是空洞,同一个词存了两次则拒绝打开),所以 `vstree.kv` 与工作集对齐;`vstree.kv` 过期、缺失或无法证明对齐时,打开时就从刚载入的 store 重建它。

两种模式读写同一个 `kvstore.kv`(格式与模式无关),同一目录可以轮流用两种模式打开;Phase 6 新建的 V2 文件老二进制不能打开,见[兼容矩阵](#兼容矩阵phase-6-结束时实际情况)。

**默认是 Materialize**,唯一的开关是常量 `gstore::db::DEFAULT_DISK_QUERY_MODE`。翻转条件是 R3-STREAM 的验收项(计划 §6 R3-STREAM):每条查询 ≤ 2.0 × Materialize、Q1–Q14 几何均值 ≤ 1.4 × Materialize、单条插入中位数 ≤ Materialize,外加 §2 的 G1b/G1c/G1d。G1b/G1c/G1d 已过,但 R3-STREAM 的三项在最终测量里仍不过,所以 Stream 是可选模式。最终参考结果 `bench/results/phase6-final-lubm38.json`(univ 38,5,181,103 个三元组):

| 指标(univ 38) | Stream | Materialize |
|---|---:|---:|
| 打开:清页缓存后首次 / 热 ms | 7.5 / 0.9 | 1817.3 / 1720.8 |
| 打开后 RSS MiB | 6.1 | 351.0 |
| Q1–Q14 期间 VmHWM MiB | 207.3 | 457.1 |
| Q1–Q14 几何均值 ms | 11.55 | 7.83 |
| Q14 / Q6 / Q12 / Q13 ms | 155.96 / 194.76 / 0.165 / 215.91 | 72.48 / 91.93 / 0.080 / 107.74 |
| 单条 `INSERT DATA` 中位数 ms | 1.98 | 1.70 |

即 Stream 的几何均值是 Materialize 的 1.48 倍,Q14、Q6、Q12、Q13 各是 2.15、2.12、2.07、2.00 倍,插入也更慢。差距在大结果集上:Stream 用 out-of-core 字典(`TermCache`,未命中时读 `id2*`)解析每个结果 id,Materialize 用内存 interner。规模越大越明显:`bench/results/phase6-final-lubm82.json`(univ 82,11,135,159 个三元组)里 Stream 的几何均值 30.18 ms、Materialize 16.09 ms,Q6 / Q13 / Q14 分别是 984.84 / 1303.58 / 876.09 ms 对 220.72 / 282.09 / 165.29 ms。持续写入时还有一个差别:`bench/results/phase6-final-watdiv10.json` 的混合负载(8 个读 F5 的客户端 + 每 100 ms 一次单条插入,`/sparql`,check_point off)里 Stream 读者 191.6 qps、Materialize 2550.3 qps——每次提交都推进 `DiskTriples` 的 epoch,长 posting list 缓存在新 epoch 的第一次读取时被整体清空。反过来,Stream 的打开时间和内存与数据量无关(univ 82:热打开 0.9 ms、打开后 6.4 MiB),所以超出内存的库或频繁打开的库应选 Stream。

**选择模式**(契约 C7):

- `OpenOptions { disk_query_mode, disk_cache_pages, dict_cache_bytes, vstree_on_open, default_layout }`,`Database::open_with(dir, &options)` 原样使用给定选项;
- `Database::open` 用进程默认值:内置默认 < `set_default_open_options` 安装的值(`gserver` 启动时安装 `conf.ini` 的 `[storage]` 段,`/api refreshconf` 重新安装;已载入的库保持打开时的模式)< 环境变量;
- `conf.ini` `[storage]`:`disk_query_mode=memory|stream`(默认 `memory`)、`disk_cache_mb`(16)、`dict_cache_mb`(80,五分之四给 id → 串,五分之一给串 → id)、`vstree_on_open`(off)、`default_layout`(见[批量构建](#批量构建));
- 环境变量:`GSTORE_DISK_QUERY_MODE=stream|memory`、`GSTORE_DISK_CACHE_MB`、`GSTORE_VSTREE_ON_OPEN`。

**Stream 模式常驻内存的东西**:命名图(D11)、O(P) 的谓词统计镜像、页缓存(`disk_cache_mb`,默认 16 MiB)、字典缓存(`dict_cache_mb`)、`DiskTriples` 的长 posting list 缓存(committed store 上 2048 项以上的列表,32 MiB,按 epoch 失效,任何写入清空)、每个读线程一个 SPO 前向游标。其余一律按需经页缓存读取。

**Stream 模式的写入**按 id 进行:

1. 新词先经共享 pager 写入字典(`DiskDict::intern`:`id2*`,再 `*2id`,再计数根;一个 pager 操作,只持 pager 写锁,不进写闸门,不推进 epoch)——若它的三元组没来,它只是一个无害的孤儿词;
2. 再经 `DiskTriples` 写三元组;`Ok(true)` 时才 `touched()`(C9);命名图四元组经 `DiskTriples::named_op`;
3. 事务就是 store 的 overlay([下节](#事务-overlay-与写闸门c5)):提交前没有三元组进入 `kvstore.kv`,`rollback` 不碰它;默认图不需要 undo 日志(命名图仍有)。

写失败会使 store 进入 **broken 状态**:之后的写、读(包括字典写,`DiskDict::with_write_check`)都被拒绝,直到重新打开数据库(更新(review-fix campaign):broken 就是共享 pager 的 failed 状态,见[下节](#事务-overlay-与写闸门c5)的 broken 状态段与 [§Pager 与 WAL](#pager-与-wal提交协议失败状态与文件锁))。`CLEAR DEFAULT` 在一个 read scope 里把默认图的 id 读进内存(每个三元组 12 字节),再在一次闸门 drain 下删除(没有范围删除,D9)。Stream 库从不构建内存子串索引(子串 FILTER 直接扫描,结果仍精确,D5)。`into_memory()` 读出整个 store 与全部词(保留 id)。

**`vstree.kv` 与 VS-tree 策略**:Stream 模式下 `vstree.kv` 只在新鲜(fresh)时使用,打开时从不重建(除非 `vstree_on_open`;Materialize 模式打开时会重建过期的 `vstree.kv`);`rebuild_index()` 一次流式扫描 SPO/OSP 重写它,在打开的事务结束后才执行,签名之前先做一次提交点(数据先落盘),并在 store 头部把它标记为 fresh。查询在默认的 `VsPolicy::Auto` 下**不读 `vstree.kv` 的页**:对 out-of-core 树,候选集的剩余谓词位直接对 store 检查,没有候选集的变量从 store 的 posting list 取出带这些位的实体;在与 store 一致的树上,所得候选集与遍历树完全相等(计划、答案和顺序都不变)。`vstree.kv` 的新鲜度仍决定 VS-tree 语义是否适用(过期的在打开时丢弃);`GSTORE_VS_POLICY=tree` 恢复页遍历。R4-VS 的安静测量(代码 `b9e8dc0`,univ 38,Stream):`GSTORE_VS_POLICY=tree` 时 Q1–Q14 几何均值 47.70 ms,`Auto` 时 11.66 ms。会话内发生删除之后,磁盘库的候选集跟随当前 store(答案不变,无 `ORDER BY` 时的顺序与新重建的树一致)。`vstree.kv` 的 pager 头部若带任何 feature bit 会被拒绝(打开和在其上构建时都检查)。

### 事务 overlay 与写闸门(C5)

`gstore::store::{Overlay, OverlayView, OverlayChanges}` 与 `gstore::kvstore::{DiskTriples, DiskReadView, DiskSource}`(`src/store/overlay.rs`、`src/kvstore/disk_triples.rs`)。

**Overlay**:事务的新增与删除与底座分开保存,保持 `added ∩ base = ∅`、`removed ⊆ base`、`added ∩ removed = ∅`,所以 `insert` / `remove`(及可失败的 `try_insert` / `try_remove`,只在 overlay 自己判断不了时才探查底座)返回的 changed 标志与物化后的 store 完全一致。`OverlayView` 在 `(base − removed) ∪ added` 上实现整个 `TripleSource`:列表按契约顺序归并,计数是精确算术,`distinct_*` / `pred_distinct_*` / `num_predicates` / `distinct_p_of_*` 只探查 overlay 碰过的键(全局与逐谓词的结果缓存到下一次变更;计算中遇到读错误则不缓存),`has_*` 读错误时为 `true`。每种键各一个 8 KiB 的单哈希 Bloom 过滤器,读未被触及的键只多一次位测试;触及数十万个键的事务会让过滤器饱和,此后每次读都走(仍精确的)归并路径。

**`DiskTriples` 的事务**:`begin()` 之后每个三元组写都进 overlay,**提交前没有三元组到达 pager**;读能看到本事务的写(read-your-writes);`rollback()` 不碰 pager;`commit()` 依次应用排队的命名图操作、删除、新增,每一步是一个原子 pager 操作。字典词可以在提交前进 pager(孤儿词无害)。事务之外的写立即应用(auto-commit)。`commit_point(&self)` 在有已应用写时做一次带 WAL 的持久提交。事务正在新增三元组时 `vstree_stale()` 为真(`vstree.kv` 按已提交状态构建,会剪掉事务自己的新边);删除不算,树仍是可靠超集。D12 仍然适用:单个逻辑写是原子的,但脏页超过页缓存的大事务是分批提交的——持久但整体上不具备崩溃原子性。

**写闸门**:改 pager 之前,写者把闸门标记为 draining、拉起闸门内每个快照读者的取消标志、等它们全部离开、应用变更,然后推进 **epoch**(只在数据确实变化时;空写既不取消谁也不推进)。在同一 store 的快照读里面发起写会被直接拒绝,而不是等待自己;在快照读里 `commit()` 也在取走事务之前被拒绝,事务保持打开。读者若一直不看取消标志,写者等待超过 10 s 会记一条警告。

**`DiskReadView`**(`DiskTriples::read_view()`,`Clone + Send + Sync`,持有共享 pager 与闸门而不是 `DiskTriples`):`with_base(|&DiskStore, &Arc<AtomicBool>| …) -> Result<R, ViewError>` 先登记为读者,每次调用新建一个只读的 `DiskStore::from_shared_pager` 视图,并把读者的取消标志交给闭包(交给 `Evaluator::with_cancel`)。闸门在 drain 或 epoch 变了(调用前后各查一次;被取消的读者总是得到它)时返回 `ViewError::Expired`,`DiskTriples` 被丢弃或关闭后返回 `Closed`,store broken 时返回 `Io`。视图看不到打开的事务的写,也不自动重试:快照读者看到的是开始时的状态,或在调用方重试之后看到的已提交状态。快照查询必须用 `DiskSource` 包装 store,并把取消标志交给求值器。

**broken 状态**:任何写或 flush 错误都会使 store broken:之后拒绝写,读报告读错误,视图回答 `Io`,`disk_store()` / `shared_pager()` 返回错误,直到重新打开;broken 的 store 被丢弃时不 flush。`Drop` 先 drain 闸门、把视图标记为 closed、尽力 flush 已应用的写;`close()` 做同样的事并报告 flush 错误;打开的事务两者都丢弃。

> 更新(review-fix campaign,C-1:durability#1、disk#1;M5:gap-txn-lifecycle#4):broken 的 `DiskTriples` 现在**就是**共享 pager 的 failed 状态(两者共用一个标志):pager 的一次提交在 WAL 得到正式名字之后失败,之后的写、分配页和提交都以 `GStoreError::StorageFailed` 拒绝,读照常,直到重新打开时重放完整的日志([§Pager 与 WAL](#pager-与-wal提交协议失败状态与文件锁))。Stream 模式下 out-of-core 字典做的提交失败同样使 store broken。Materialize 模式不只在写穿失败时 broken,提交点失败时也 broken:之后每个提交点都报告这次失败(`storage failed: …`)而不重试,丢弃数据库时什么也不提交。服务器不必重启就能重开这样的库:`/api` 或 REST 的 `unload` 不做提交点直接退役它(回答带 `warning`,写明失败原因和最近一次成功提交点之后丢弃了多少变更),再 `load`;对已载入的 broken 库直接 `load` 会就地重开。单库模式(`gserver DIR`)没有 unload,仍需重启。`Database::storage_failure` 说明原因与不持久的变更数。

**锁顺序**(C8):`DiskTriples` 闸门 → pager;闸门的 mutex 从不跨 pager 操作持有;`DiskDict::intern` 只拿 pager 写锁(从不反向)。每个逻辑 kvstore 写在整个操作期间持有 pager 写锁(R1-KV 的 operation bracket:操作中途缓存不为腾地方而提交,出错或 panic 时整个操作连同计数器回滚);`DiskStore` 写入中的 panic 被回滚并以错误返回,不会毒化共享 pager。

### 字典(C4)

- **API**(`gstore::dict`):`TermStr<'a>`(`Borrowed(&str)` 来自内存字典,`Shared(Arc<str>)` 来自后端;`Deref<Target = str>`,按文本比较与哈希);`Dictionary::term` / `pred`(`Option<TermStr>`,后端错误记入语句的 read scope)、`try_term` / `try_pred` / `try_*_id`、`prefetch`、`resolve_sorted` / `resolve_many`(排序批量)、`for_each_term`、`materialize()`、`id_limit`、`generation()`、`backing()`、`cache_stats()`。
- **内存布局**:每个串只存一次(id → 串向量里的 `Arc<str>`),串 → id 一侧是开放寻址的 id 表(每槽 8 字节,经向量比较);三个 interner 是 `Arc<InternerData>`,克隆 `Dictionary` 是 O(1),对共享克隆的第一次写只复制它改动的那个 interner。
- **`dict.bin`**:格式不变(正向表、带空洞的反向向量、空闲列表),由手写序列化器按 id 升序写正向表,同样的输入写出相同的字节(Phase 6 之前按 `HashMap` 的随机顺序);老二进制读新文件、新代码读老文件都得到相等的字典;正反两半不一致的文件被拒绝。空闲列表仍是同样的 `seq` of `u32`,现在升序。
- **out-of-core 后端** `DiskTermSource`:可失败、`&self`、`Send + Sync`;`lookup`、`resolve -> Arc<str>`、`resolve_sorted`、`intern` / `free`(默认拒绝;只读后端拒绝新词)、`prefetch`、`for_each`、`count`、`id_limit`、`generation`、`sync`。`DiskDict` 用 `BTree::get_with` / `IvArray::get_with`(无逐次拷贝)实现它;批量与预取按 id 顺序逐 run 读取(`IvArray::for_each_key`,只解码请求的槽);`DiskDict::writable()` 能 intern(新词推进 generation,缓存原地更新),`DiskDict::free` 同时推进 generation 与缓存 epoch(清空缓存);`slot()` 以已分配上限而不是存活数为界(有空洞时)。
- **有界缓存**(`gstore::dict::cache`):`TermCache`(`(space, id) → Arc<str>`,默认 64 MiB)与 `KeyCache`(`(space, key) → id`,默认 16 MiB),各 16 个分片、CLOCK 淘汰、按字节计费(串长 + 72)。`DiskDict` 的缓存只在某个 id 或键可能改变含义时(释放了一个 id)整体清空;intern 只会新增,缓存原地更新。
- **老 `&str` API**:`id_to_string` / `predicate_to_string` 只对内存字典回答;对 out-of-core 字典返回 `None` 并让语句失败(fail closed,不再 `Box::leak`)。生产代码已不再调用它们(图函数和 PFN 的 `GraphCtx` 经 `term()` / `pred()` 读标签)。
- **查询侧**:SELECT 结果按 4096 行一块物化,字典在磁盘上时每块每个 id 空间一次排序批量解析;每查询的 id → term / id → value 备忘(`IdMemo`)只用于 out-of-core 字典,上限 8 MiB,超过 16 KiB 的键不进。子串索引(`CONTAINS` / `STRSTARTS` / `STRENDS` / 无元字符 `REGEX` 的候选剪枝)按字典 generation 缓存(`Database::string_index()`),只对内存字典构建,建好后常驻;R2-DICT 安静测量阶段(phase6 `0c9bdb6`)测得 univ 38 时它有 430,954,544 字节(D5)。更新(review-fix campaign,gap-accel-soundness#3、db#5):缓存的索引只存候选(不再另存一份词串和按串排序的 id 表),新增或释放词的写入之后由下一次子串查询增量扩展,而不是从整个字典重建(`docs/GAP_PLAN.md` §9.1 第 6c 行)。

### 格式注册表与兼容矩阵

**注册表**(`src/kvstore/format.rs`,C3)是 `kvstore.kv` 与 `vstree.kv`(也是 pager 文件)全部版本标记、根槽与 feature bit 的唯一所有者;节点、IvArray、溢出链与 WAL 的格式在 Phase 6 中没有变化,在 review-fix campaign 中也没有(该轮每项"无格式变化"的说明汇总在[§注册表:review-fix campaign 的结论](#注册表review-fix-campaign-的结论))。

- 头部:`[magic 8 B][page_count u32][free_head u32][root slot × NROOTS u64]`;`NROOTS` 从 24 增至 32,24 槽的老文件把多出的槽读作 0,新代码写的 V1 头与 Phase 6 之前逐字节相同。
- `MAGIC_V1 = "GSTOREKV"`(`FEATURES == 0`)、`MAGIC_V2 = "GSTOREK2"`(至少一个 feature bit)。每次打开都检查(`check_header`):`FEATURES == 0` ⇔ V1,两种不一致都拒绝;本二进制不支持的位被拒绝;某 feature 第一次使用时,它的位与 `MAGIC_V2` 随第一个需要它的结构在同一次 pager flush 中写入(`Pager::enable_feature`);`vstree.kv` 必须 `FEATURES == 0`。

| 槽 | 用途 |
|---|---|
| 0–20 | SPO/POS/OSP、六个字典结构、计数根(10–12 是实体 / 字面量 / 谓词 id 的高水位,有了 id 回收后可以下降)、`SP2O_VLIST`、`COMPACTED`、`PRED_STATS`、去重计数、`GSPO`、四元组计数、`VSTREE_STALE` |
| 21 | `FEATURES` 位掩码 |
| 22 | `FREE_IDS` B+ 树根(5 字节键 `E\|L\|P ‖ be32(index)`,空值) |
| 23–25 | `FREE_COUNT_E` / `FREE_COUNT_L` / `FREE_COUNT_P` |
| 26–31 | 保留 |

| 位 | feature | 状态 |
|---|---|---|
| 0 | `FEATURE_FREE_IDS` | 已实现(R4-RECLAIM),`SUPPORTED_FEATURES` 只含它 |
| 1 | `FEATURE_IVARRAY_WIDE` | 保留(解除每空间 33,554,432 项的 IvArray 上限,D7) |
| 2 | `FEATURE_LONG_KEYS` | 保留(超长字典键,D7) |
| 3–63 | — | 保留 |

磁盘布局的硬限制:字典键(IRI、字面量)最长 2038 字节(`bptree::MAX_KEY_LEN`,半页:内部节点的分隔键必须给二路分裂留地方),更长的键使插入或构建以点名该限制的错误失败;每个 id 空间的 IvArray 容量 33,554,432 项。两者都由保留位 1、2 解除(D7)。RocksDB 没有这两个限制。

**快照布局**的 `dict.bin`、`store.bin`、`named.bin`、`vstree.bin`、`meta.bin` 字节格式不变(写时复制的 `TripleStore`、`NamedGraphs`、手写的 `VsTree` 序列化都与之前的 bincode 逐字节一致,有 golden 测试)。**RocksDB** 的 `LAYOUT_VERSION` 仍是 `"1"`,CF 集合与键编码不变,批量构建写出的每个键值与逐条构建完全相同。

更新(review-fix campaign):快照布局的读取更严格,字节格式不变——`named.bin` 的读错误使打开失败,只有 `named.bin` 不存在才表示"没有命名图";快照的提交记录 `SNAPSHOT.commit` 只列成员文件名,不带每个成员的长度或校验和(r1-durable-ack)。RocksDB 的 `LAYOUT_VERSION` 仍是 `"1"`,只在 `meta` CF 里加了一个附加的键区间(见[§RocksDB 语句日志](#rocksdb语句日志与崩溃恢复))。

#### 兼容矩阵(Phase 6 结束时实际情况)

| 目录 | 新二进制 | Phase 6 之前的二进制 |
|---|---|---|
| 快照(任何年代) | 读写;id 回收开启(下次 save 时持久化) | 读写(字节格式相同) |
| `kvstore.kv` V1,Phase 6 之前就有的 | 读写,Stream 或 Materialize;id 回收关闭(`reclaim_unused` 被拒),直到 `gcompact --enable-reclaim` 把它升为 V2 | 读写 |
| `kvstore.kv` V1,新二进制在 `id_reclaim=off` 下新建的(批量构建或增量构建) | 读写 | 读写 |
| `kvstore.kv` V2(`FREE_IDS`;`id_reclaim` 为默认 `on` 时新建的每个磁盘库,或升级过的库) | 读写 | **拒绝**:无注册表的老二进制报 "not a gStore KV file";有注册表但早于 R4 的报 "this gStore KV file uses feature bits 0x1 that this binary does not support" |
| RocksDB layout `"1"`(含批量构建的) | 读写;打开时不重建 VS-tree | 读写 |
| `vstree.kv` | 只接受 `FEATURES == 0` | 读写 |

老文件**从不被隐式升级**。降级(V2 → 老二进制)的办法:新二进制 `gexport`,老二进制 `gbuild`。

`redo.log` / 增量备份里的 `CLEAR` / `DROP` / `MOVE` / `COPY` 记录(`C`、`N`)是 Phase 6 新增的。更正(review-fix campaign,gap-doc-claims#1;此处原先写"老二进制恢复这样的增量备份会以 "unknown redo.log op 'C'" 失败",对至今写出的日志并不成立):Phase 6 起到 review-fix campaign 第 2 轮为止写出的 clear 记录正文为空,Phase 6 之前的二进制(`eaf5320`、`6362b97^`)会**静默跳过**它们——用这些二进制恢复这样的增量,被清掉的数据会不报错地回来;不要跨这样的记录降级。从本版本起写出的 clear 记录带占位三元组 `<urn:x-gstore:redo> <urn:x-gstore:redo#op> "clear" .`,那些二进制以 "unknown redo.log op 'C'" 拒绝它(先在 `tests/redo_log_durability.rs` 里对照其读取器的逐行仿制检查,r4-storage-format 又用真实的 `eaf5320` 二进制经 `scripts/compat-fixtures.sh` 确认)。日志的新分帧见 [§redo 日志与增量备份](#redo-日志与增量备份)。

**兼容矩阵现在有跨版本检查**(review-fix campaign,gap-doc-claims#8):`scripts/compat-fixtures.sh` 在临时 worktree 里构建 `eaf5320`(Phase 6 之前)并用两个版本的真实二进制检查上表(每周运行,`.github/workflows/compat.yml`,也可手动触发);它生成的夹具 `testdata/compat/eaf5320/`(`eaf5320` 写出的快照与 V1 磁盘库)由 `tests/format_compat.rs` 在每次 `cargo test` 中打开并查询。实测与上表一致:`eaf5320` 的 `gquery` 读新 `gbuild` 写出的快照与 `GSTORE_ID_RECLAIM=off` 磁盘库,拒绝默认的 V2 磁盘库("not a gStore KV file");`eaf5320` 的 `restore_incremental` 在本版本写出、含 `CLEAR DEFAULT` 的增量上以 "unknown redo.log op 'C'" 失败。

### 批量构建

`gstore::bulk::build(dir, name, BulkInput, &ImportOptions, &mut dyn ErrorLog, BulkTarget, &BulkOptions) -> Result<BulkReport>`(C6,`src/bulk/`):

```text
sources ─▶ 解析(每个源一个解析线程,按源顺序重放) ─▶ 编码(词表,首次出现的 id)
        ─▶ 外部排序(SPO,再 POS / OSP / GSPO,签名)
        ─▶ 页镜像(按 fill factor 的 B+ 树、IvArray、头部)+ vstree.kv,或 SST 文件
        ─▶ 分阶段安装进数据库目录
```

- **id 与内存构建相同**:按 subject、predicate、object、graph 的首次出现分配,导入报告、`parse_error.log` 与 strict 模式的第一个错误都与顺序导入一致(任何线程数下)。
- **磁盘目标**(`BulkTarget::Disk`):自底向上写 SPO / POS / OSP(12 字节键)与 GSPO(16 字节键)B+ 树(默认 90% 填充,每层最后两个节点重新平衡)、`entity2id` / `literal2id` / `predicate2id` 树、稠密 `id2*` IvArray 及溢出链、逐谓词统计数组和头部;`vstree.kv` 由排序后的签名流式写出(`VsTree::write_disk_sorted`),与增量构建写出的逐字节相同,store 的 VS-tree 标志为 fresh。`id_reclaim` 为 `on`(默认)时镜像带 `FREE_IDS`(V2,空闲列表为空),`off` 时是 V1。较密的树使之后的插入更常分裂节点,`--fill 70` 可以留更多余量。
- **内存**:排序缓冲、签名 spool、归并与 run 读写的 I/O 缓冲都计入 `memory_budget`(排序缓冲超出自己的份额时把排好序的 run 溢写到 scratch 文件,归并 fan-in 为 `min(64, share / 256 KiB)`;spool 超出份额时同样写进 scratch 文件——univ 38 的两次磁盘构建都是 0 个溢写 run、scratch 写入 82.7 MiB,`bench/results/phase6-final-lubm38.json`);词表(每个 `L` 字节的词约 `L + 26` 字节)在预算之外,单独报告(`BulkReport::term_map_bytes`,D3)。最终参考结果的构建摘要(`bench/results/phase6-final-lubm38.json`,univ 38,`disk@materialize` 那次)里 parse+encode 占 18,861 ms,预算峰值 384.2 MiB,词表 155.0 MiB:解析是主要开销。
- **scratch**:默认在 staging 区内,`--tmp-dir` / `build_tmp_dir` 可改;每个 scratch 目录以新名字独占创建(`gstore-bulk-scratch-<pid>-<seq>-<nonce>`),存续期间持有其中 `LOCK` 文件的咨询锁,构建会清扫无人持锁的残留目录(更早版本留下的、没有 `LOCK` 的目录不会被碰)。需要的空闲磁盘:新库加旧库,溢写时再加约一份编码后的三元组和签名。
- **输出确定**:`kvstore.kv` 与 `vstree.kv` 与线程数、预算无关,逐字节相同(含空白节点的数据除外:每个文档的空白节点作用域是随机的)。
- **磁盘安装**(`bulk::CRASH_POINTS`,`src/bulk/commit.rs` 的表):构建持有目录的 `DirLock` 与进程内的 `BuildGuard`,所有东西写在 `<db>/.bulk-tmp/`(从来不是数据库;残留的由下次构建删除)。步骤:`staged` → `marked`(写 `COMMIT`)→ `cleared`(把旧 store 残留的 `kvstore.kv.wal` 重放进旧文件并删除——重放失败则停在这里、日志保留;删除旧 `vstree.kv`)→ `kv-renamed`(新 `kvstore.kv` 改名覆盖,**提交点**)→ `kv-synced` → `vstree-placed`(在规则目录锁下删除被替换库的 `reason_rule_files/`,放入新 `vstree.kv`)→ `layouts-removed`(删除快照文件;`rocksdb/` 先持久改名移开再删)→ `cleaned`。每一步后都 sync 目录,崩溃总是留下旧库或新库,未修改的二进制都能打开。提交点之前的任何错误都不动旧库;之后的错误不使构建失败,而是进 `BulkReport::warnings`(gbuild 打印 "built (the new database is in place), but its install did not finish: …" 并以 1 退出)。安装前会再查一次 `rocksdb/LOCK`,防止构建期间有只拿 RocksDB 自身锁的老二进制打开了它。
  更新(review-fix campaign,durability#12、bulkcli#15、bulkcli#13、bulkcli#12、tests-a#19;无格式变化,D7):
  - `marked` 步写的 `COMMIT` 标记是**咨询性**的:它记录安装开始时暂存的文件已完整并已 sync,没有任何恢复读它(没有前滚:提交点之前旧库就是数据库,之后新 store 已在位),只作事后取证;残留的 `.bulk-tmp/` 不论是否带标记,都由下一次构建删除。
  - 构建用 `BuildGuard::lock(dir)` 一步完成声明目录与取锁(在锁注册表的 mutex 下);声明存续期间,本进程的其它线程打开该目录(`DirLock::acquire`,因而 `Database::open`)被拒绝。此前"本进程是否打开着"的检查与取可重入目录锁是两步,同进程的另一线程(如 `/build` 进行中的 REST `/load`)能共享构建的锁,它已确认的写落进随后被安装替换掉的文件。增量构建同样如此。
  - 并行写 POS、OSP、统计与字典的写者各在按预测数预留的页区间里写;页数与预留不符原先只是 `debug_assert`,现在 release 构建也检查,不符时在安装之前使构建失败("the POS index took N page(s), but M were reserved for it …"),旧库不动。
  - `GSTORE_BULK_CRASH=<step>` 是崩溃测试的失败点(release 二进制里也生效):设置时,进程的每个批量构建在该安装步骤之后立即中止,并在标准错误上说明;值不是安装步骤名时说明它被忽略。进程内的对应钩子是 `debug_crash_at`。不要在崩溃测试之外设置它。
  - 增量磁盘构建(`gbuild --disk --incremental`、`build_disk_from_sources_incremental`)也不再写在被替换的库上:它把 `kvstore.kv` + `vstree.kv` 暂存在 `.bulk-tmp/`,再走同一个批量安装(见 [§布局替换](#布局替换与崩溃安全))。
- **RocksDB 目标**(`BulkTarget::RocksDb`,需 `rocksdb` feature):同样的排序流按 CF 写成 `SstFileWriter` 文件(每个 CF 用自己的选项,256 MiB 切分):`spo`、`pos`、`osp`、`gspo`、`dict` 及每个统计计数器(精确,零值计数器不写);在 `.bulk-tmp/rocksdb` 新建的 store 依次 ingest(`move_files`,同一 CF 的文件互不重叠,直接落到最底层,字典最先),最后写 `meta` 名字和 `layout = "1"`,flush、sync WAL、关闭、重开校验计数器、fsync 每个文件。键值与逐条构建 `Database::build_rocksdb_*` 完全相同(任意长度的词都能构建)。安装(`bulk::ROCKS_CRASH_POINTS`):`staged` → `swapped`(经 `rocksdb.new` 改名为 `rocksdb/`,**提交点**,下次打开会补完)→ `rules-removed` → `layouts-removed` → `cleaned`。更新(review-fix campaign,r2-integration):`rocksdb.new` 无法改名就位时(安装、它的恢复与重试中),构建失败,不再打开——从而新建——一个空的 `rocksdb/`(下次打开的恢复会留下它而删掉真正的构建 `rocksdb.new`),也不删除其它布局;下次打开把构建移到位。
- **谁走批量路径**:布局为 disk 或 RocksDB 的构建。`gbuild` 的布局由 `--disk`、`--rocksdb` 或 `--layout snapshot|disk|rocksdb` 决定,都没给时取 `./conf/conf.ini` 的 `[storage] default_layout`,再缺省为 snapshot(快照在内存中构建后保存);`--layout` 与 `--disk` / `--rocksdb` 矛盾是用法错误。库函数 `Database::build_disk`、`build_disk_from_sources`、`build_disk_bulk_from_sources`。REST `/build` 与 `/api build` 的 `layout=snapshot|disk|rocksdb`(也接受 `memory`、`kvstore`、`rocks`;默认取服务器配置的 `default_layout`,否则 snapshot;RocksDB 需要服务器带 `rocksdb` feature,否则 `400` / `1003`)。旧的逐条路径保留为 `#[doc(hidden)] build_disk_incremental` / `build_disk_from_sources_incremental`(`gbuild --disk --incremental`、比较测试)。`Database::build_rocksdb_*` 是逐条写的小数据辅助函数(D15)。
- **gbuild 参数与配置**:`--memory MB`(默认 1024)、`--threads N`(`0` = 全部核心,默认)、`--tmp-dir DIR`、`--fill PCT`(50–100,默认 90,只用于 disk 布局)、`--incremental`;布局不是 disk / RocksDB 时这些参数是用法错误(退出码 2)。`conf.ini` `[system]`:`build_memory_mb`、`build_threads`、`build_tmp_dir`、`build_fill_factor`,`build_multi_thread=off` 表示一个线程;gbuild 读 `./conf/conf.ini`(若存在),包括 `[system] id_reclaim` 与 RocksDB 调优键。
- **构建拒绝的目录**:另一进程持有的目录、本进程有数据库打开着的目录(`DirLock::held_here`,"is open in this process")、正被另一个批量构建重建的目录,以及 RocksDB store 正被任何进程打开的目录(Linux 上从 `/proc/locks` 读 `rocksdb/LOCK`,`bulk::commit::rocksdb_lock_holder`)。服务器要重建一个已载入的库,先 unload(释放句柄)再构建,REST `/build` 就是这么做的:先保存(check_point off 时未保存的已确认写)、退役已载入的副本、构建、再打开并服务新库;构建失败则重新服务旧库。

### id 回收

`src/db/reclaim.rs`,`gstore::db::{IdReclaim, set_default_id_reclaim, default_id_reclaim, ENV_ID_RECLAIM}`。

- **策略**:`IdReclaim::Immediate`(默认,`on`)或 `Off`(Phase 6 之前的行为:id 只由显式的 `Database::reclaim_unused` / `gcompact --reclaim` 释放)。`conf.ini` `[system] id_reclaim=on|off` 由 gserver(启动时与 `/api refreshconf`)和 gbuild 读取;其它命令行工具(gadd、gsub、gquery…)只看环境变量 `GSTORE_ID_RECLAIM=on|off`,环境变量总是优先。`Database::set_id_reclaim` / `id_reclaim` / `reclaims_ids` 针对单个库。更新(review-fix campaign,disk#8、design#6):`GSTORE_ID_RECLAIM` / `[system] id_reclaim` 只决定**新建** store 文件的格式(带 `FREE_IDS` 的 V2 或 V1),并显式传给它们——`BulkOptions::free_ids`(批量镜像的头部按它写,不再读进程策略)与 `OpenOptions::free_ids_if_new` / `with_free_ids_if_new`(打开时新建 store 文件的情形);已有的 `kvstore.kv` 保持其头部记录的格式,不论进程策略如何,只有 `gcompact --enable-reclaim` 升级。
- **何时**:每次实际生效的删除——`DELETE DATA` / `DELETE … WHERE`、四元组删除、`CLEAR` / `DROP` / `MOVE` / `COPY`、`remove_sources`(`gsub`、`/api batchRemove`)、失败 `LOAD` 的撤销、事务新增的回滚——之后,被删三元组的 subject、predicate、object(及图 IRI)成为候选;语句结束时(auto-commit)或 `commit` 时(事务把释放推迟成一批)释放其中无人引用的。事务 `rollback` 时,它新铸造的词也是候选。
- **"被引用"**:节点 id 是默认图的 subject 或 object(`has_subject` / `has_object`,读错误时为 `true`),或命名图计数它(`NamedGraphs::node_refs`,含图 IRI——`GSPO` 以图为首键,所以四元组的引用以内存中的 `NamedGraphs` 计数为准);谓词同理(`has_predicate`、`NamedGraphs::pred_refs`)。每个候选凭自己的探查独立判定:读错误使其悬而未决的候选不释放也不丢弃,在下一条语句再查,最多 `CHECK_ATTEMPTS`(4)次,之后记一条警告留给 `gcompact --reclaim`;它不拖住其它候选。
- **顺序与各布局的持久化**:释放总在语句的 redo 记录和写穿之后;先持久化,再由字典 retire 该 id、`touched()`,等到没有比 retire 更早的 pin 时才 release 供复用。
  - 快照:下次 `save` 时(`dict.bin` 的空闲列表,升序)。
  - Materialize:键级写穿 `DiskOp::Free`;复用之后 store 自己的 id 可能与工作集不同(写本来就按键,重开时重读 store 的 id)。
  - Stream:`DiskTriples::free_ids` 在写闸门下按 id 释放,先防御性地复查三元组索引——若仍被引用则拒绝(并使 store broken),而不是破坏它。
  - RocksDB:释放的 `dict` CF 条目放进删除 undo 日志的那一批——事务在提交时,自动提交语句在语句结束时(更新(review-fix campaign,H-11、gap-rocksdb-exec#1、#4):此前自动提交时在三元组批之后删除;现在每条自动提交语句也记日志,它释放的 id 等到语句结束才删除和复用);rollback 的重放留下的无引用词在删日志的那一批里删除;打开时恢复崩溃留下的日志,则把这些候选与删除的日志记录写进同一批、放进 `meta` CF 的待定区间,`Database` 在服务之前处理完——没有哪个崩溃窗口会留下孤儿词(见 [§RocksDB 语句日志](#rocksdb语句日志与崩溃恢复))。底层用户经 `take_replayed` 接手重放交出的词(每条语句和每次提交后都会接手)。`delete_terms` 失败则词保留在内存与 `dict` CF 中(孤儿,下个提交点报告)。
- **复用顺序**:每个空间**最低 id 优先**;释放最高 id 会降低该空间的高水位,并连带降低随之成为最高的空闲 id(trim cascade),所以计数根可以下降。`entity_num` / `literal_num` / `predicate_num` 现在恰好等于数据引用的词数。
- **pin**:内存字典的克隆是写时复制的,各自拥有自己的 id 版本,不需要 pin。out-of-core(Stream)字典的克隆共享磁盘字典,克隆期间 pin 住它(`Dictionary::pin()` / `DictPin`);Stream 库在有比候选更早的 pin 时推迟释放:候选带上 pin 时钟的戳,按戳排序,pin 消失后每个候选只检查一次,pin 期间不重复探查。磁盘 `/begin` 快照只在自己的 `DiskReadView` 里读字典(写闸门会先 drain 它),所以用 `clone_unpinned()`;`ConcurrentDb` 的 Stream 快照在任何读视图之外解析物化的 store,所以保持 pin——`ConcurrentDb` 包着 Stream 库时,它构造时的基准快照在它整个生命周期都 pin 着字典,不释放任何 id,丢弃时才运行这些释放并持久化。被 pin 推迟的释放只在内存中:重启(或丢弃一个普通 `Database`)时仍被 pin 住的释放会丢失,这些词成为孤儿,直到 `gcompact --reclaim`。
- **VS-tree**:被释放的实体留在树里(可靠超集,直到 `rebuild_index()`;没有自动重建阈值,`Database::vs_shrink_debt()` 只计数);一个实体得到第一条边(新词或被复用的 id)时获得精确签名,而不是并到旧条目上。
- **内存库的批量写**:`INSERT DATA`、`DELETE DATA`、`DELETE/INSERT … WHERE`、`LOAD`(及其撤销)、`ADD` / `COPY` / `MOVE`、`gadd` / `gsub`、`/api batchInsert` / `batchRemove` 和事务回滚,把一条语句(或一个导入批)的默认图三元组一次交给 store(`TripleStore::insert_many` / `remove_many`)。最低 id 优先复用会让新词拿到小 id,逐条插入会把长列表的大部分移动一次又一次;批量归并消除了这一点。结果、计数、错误语义、undo/redo、写穿和回收都与逐条路径相同;Stream 与 RocksDB 仍逐条写。
- **redo 日志**:`CLEAR` / `DROP` / `MOVE` / `COPY` 写 redo 记录(一个图 `C`,全部命名图 `N`),`replay_redo_log` 与 `restore_incremental` 会重放它们;RocksDB 上部分被拒的 clear 按实际删除的每个三元组 / 四元组记 `D` 记录。降级时这些记录的行为见[兼容矩阵](#兼容矩阵phase-6-结束时实际情况)下的更正,review-fix campaign 的分帧见 [§redo 日志与增量备份](#redo-日志与增量备份)。
- **工具与 API**:`gcompact <db> [--reclaim] [--enable-reclaim] [--check]`(可组合;取目录锁)——`--reclaim` 运行 `Database::reclaim_unused` 并持久化(快照库重新保存),`--enable-reclaim` 把 Phase 6 之前的 `kvstore.kv` 升为 V2(老二进制从此拒绝它),`--check` 运行 `Database::verify_integrity`。`gmonitor` 显示 `id_reclaim` 和每个空间的 `live / free / id_limit`(`Database::id_space_stats`)。`reclaim_unused` 返回 `Result<usize>`,在事务内和 V1 `kvstore.kv` 上被拒,释放了东西时推进 `mutation_count`。

**行为变化**:删除之后 id 会被复用,跨删除缓存 id 的应用不能再这样做;实体 / 字面量 / 谓词计数随删除下降;`DiskStore::entity_num` 计存活词(已分配减空闲)。

### 内存布局:写时复制的 `TripleStore`

`TripleStore` 的索引放在一个 `Arc` 包着的 `StoreData` 里(字段与之前相同、顺序相同):`clone()` 是 O(1) 共享;对共享 store 的第一次变更复制一次,之后原地修改;不改变任何东西的变更(插入已有的、删除不存在的、空的或全已存在的批量加载)从不复制。序列化委托给 `StoreData`,`store.bin` / `named.bin` 与 `Debug` 输出不变。降到容量四分之一以下的有序列表会归还多余内存。`TripleStore` 精确维护每个谓词的去重 subject / object 数(`pre2sub` / `pre2obj`),不序列化,读入时重算。

- **spare copy**(`keep_spare_copies`,`#[doc(hidden)]`,默认关,只有 `ConcurrentDb` 打开):store 必须复制共享索引时,把复制出的索引作为 spare 留着并记录之后的插入 / 删除日志;下次必须复制时,若没有克隆再持有那份 spare,就重放日志(O(变更))把它追上,而不是复制所有列表(O(三元组))。批量加载或日志长于 4096 + 三元组数/8 时丢弃 spare。代价是内存里多一份索引。
- **批量排序归并**(`insert_many` / `remove_many`):批次(任意顺序,可有重复)按每种索引顺序排序一次,一趟归并进每个被触及的列表(只移动该列表第一个新项之后的尾部,且只移动一次);删除同样一趟压缩。结果第 `i` 项等于逐条执行到第 `i` 个三元组时的返回值,最终 store(三元组、统计、剪枝、序列化字节)与逐条执行完全相同;不改变任何东西的批次不复制共享索引。

### RocksDB

- **布局**:CF `spo` / `pos` / `osp`(4 字节大端 id 的复合键)、`gspo`、`dict`(`tag ‖ be32(id) → key`,空洞即空闲 id)、`stats`(计数器:全局 `T`/`P`/`S`/`O`,逐谓词 `c|`/`u|`/`v|p`,逐 subject `s|s`、逐 object `o|o`,`x|p|s`、`y|p|o`)、`meta`(名字、`layout = "1"`)。事务的每个写在同一原子批里带 undo 记录,`commit()` 删除日志,`rollback()` 与崩溃后的打开重放它。更新(review-fix campaign):自动提交语句也走这个日志,提交点只 fsync WAL,打开时的恢复分块进行——见 [§RocksDB 语句日志](#rocksdb语句日志与崩溃恢复)。
- **读**:有界的 raw iterator 扫描(`iterate_upper_bound` = 前缀的后继,键原地解码);iterator 状态错误或失败的点读经 `note_read_error` 上报(C2),语句失败而不是从截短的列表作答;读不出来的写(存在性检查、计数器)直接失败。C1 计数各是一次计数器点读(`count_po_by_s` = `s|s`、`count_ps_by_o` = `o|o`、`count_o_by_sp` = `x|p|s`、`count_s_by_po` = `y|p|o`、`count_so_by_p` = `c|p`),与默认值逐一相等;`c|`/`u|`/`v|` 在内存中有镜像(打开、每次日志重放后重新加载,只在写批成功后更新),`pred_card` 等不再碰 RocksDB;`iter_all` 按契约 `(p, s, o)` 顺序返回;`subs_by_p` / `objs_by_p` 读 `x|p` / `y|p` 键,无需排序。
- **打开**:不重建 VS-tree(打开时索引是 stale 的,`stats().index_valid == false`,`gshow` / `gmonitor` 显示 stale),查询不用 VS-tree,直到 `rebuild_index()`;`vstree_on_open`(或 `GSTORE_VSTREE_ON_OPEN=1`)恢复打开时构建。打开仍要把字典读进内存(D4)。`bench/results/phase6-final-lubm38.json`:univ 38 冷 / 热打开 738.9 / 678.0 ms(`bench/results/pre-phase6-lubm38.json`:60810.7 / 59389.4 ms)。
- **调优键**(只是选项,不改格式,有无 bloom 写出的文件都能打开):`[storage] rocksdb_block_cache_mb`(默认 256;`0` = RocksDB 默认的每 CF 32 MiB 缓存)——一个 LRU block cache 由所有 CF、以及本进程所有同容量的 store 共享;`[storage] rocksdb_bloom`(每键位数,默认 10,`on` / `off`)——给点查 CF(`spo`、`stats`、`gspo`)的整键 bloom filter。gserver 启动时与 `refreshconf` 时、gbuild 启动时安装它们(`GStoreConfig::install_storage_defaults`、`set_default_rocks_tuning`);环境变量 `GSTORE_ROCKS_BLOCK_CACHE_MB`(纯 MiB 数字;其它写法记一次警告后用 256)覆盖 block cache。`RocksStore::open_with(path, &RocksTuning)` 供库直接使用。更新(review-fix campaign,H-12):`[storage] rocksdb_max_open_files`(默认 4096,`0` = 不限,即 RocksDB 默认)限制每个已载入 RocksDB 库打开的文件数;另外只保留 4 个旧 info 日志(`LOG.old.*`),WAL 总量上限 256 MiB(超出时 flush 仍持有最老日志的 CF)。同样只是选项,文件不变,任何取值都能打开。
- **批量构建**:见[批量构建](#批量构建)的 RocksDB 目标(SST ingest,`rocksdb.new` 交换)。
- **目录锁**:`Database::open` / `open_rocksdb_with` 和逐条构建 `create_rocksdb`(`build_rocksdb_*`)为 RocksDB 布局也取 `<dir>/LOCK`(在 `rocksdb/` 旁边),库存活期间一直持有;RocksDB 自己仍取 `rocksdb/LOCK`。`build_rocksdb_*` 拒绝本进程正服务着的目录。
- **仍未做**:out-of-core 字典与原生快照(D4);`/begin` 与 `ConcurrentDb` 快照仍物化整个 store(`bench/results/phase6-final-lubm38.json` 的 `/api begin` 点:univ 38 中位数 2132.55 ms、RSS 增长 269.3 MiB);不持久化 VS-tree(D6,`vstree_on_open` 时在内存中构建)。

### 目录锁规则

`src/db/dirlock.rs`。一个数据库目录**同一时间只能被一个进程打开写入**。

- **`DirLock`**:`<dir>/LOCK` 上的操作系统咨询锁(Unix `flock`,Windows `LockFileEx`;持有者崩溃时由内核释放),所有布局都取,持有到 `Database` 被丢弃为止。另一进程打开时得到点名持有者 pid 的错误("… is locked by another process (pid N) …")。进程内按被锁的文件引用计数:同一进程再次打开共享这把锁(测试、服务器恢复后重开都依赖这一点),进程内两个写者的互斥由调用方负责(服务器用已载入库的注册表与按名字的生命周期预留)。不支持咨询锁的文件系统上跳过加锁,库照常打开。更新(review-fix campaign,db#10):进程内的注册表按被锁的 `LOCK` 文件的 (device, inode) 作键(Unix 之外是规范路径),不再按路径:`DirLock::acquire` 只在本进程持有的正是 `<dir>/LOCK` 所指的那个文件时共享,否则取新的系统锁(打开与加锁之间文件被移走就重试)——一个带着锁被移走的目录(恢复交换、`/api rename`)不再把注册表条目借给同一路径上的新目录。新增 `DirLock::holds(dir)`。同一进程里**往同一个目录保存**由写者互斥(`DirLock::writing`)串行化:每次保存到外部目录都持有它,快照库在自己目录里的保存持有自己锁的写者互斥,第二个保存等待,不会装出两次保存的混合(db#11)。
- **`BuildGuard`**:进程内对"正在被批量构建重建的目录"的声明(`DirLock` 在进程内共享,挡不住同进程的第二个构建)。更新(review-fix campaign,bulkcli#13):`BuildGuard::lock(dir)` 一步完成声明与取锁,声明存续期间同进程的其它打开被拒绝;`gbuild` 用 `BuildGuard::create_and_lock` 声明数据库目录(持久地新建它及缺失的上级目录)。
- **锁冲突的错误类型**(review-fix campaign,r3-rest-parity、r3-integration):`DirLock` / `SideLock` / `BuildGuard` 的冲突,以及另一进程持有的 pager 文件([§Pager 与 WAL](#pager-与-wal提交协议失败状态与文件锁)),都是 `GStoreError::Locked`(`ErrorKind::Locked`,每个 REST 端点答 `409`);锁文件上的 I/O 错误是 `Io`。无格式变化。
- **`SideLock`**:目录旁边的锁文件 `<dir><suffix>`,给要创建目录、尚无 `LOCK` 可取的操作用:恢复到一个还不存在的库时持有 `<dir>.restore-lock`,从暂存之前到交换完成;最后一个持有者在持锁时删除该文件。
- **先锁后修复**:`Database::open` 在读任何东西之前、在补完中断的 store 交换(`rocksdb.new` / `.old` / `.tmp`)之前加锁;中断恢复(`<dir>.restore-*`)只在目录(或移开期间的 `<dir>.restore-old`)的锁下修复;另一进程持锁时失败且不碰任何东西。`Database::backup_dir` 只在源目录的锁下修复源里中断的提交或交换。
- **`Database::read_copy`** 不取锁、不碰目录,读一个快照库的分离副本(只限快照布局)。
- **保存到另一个目录也锁它**(review-fix campaign,db#11、db#17):`Database::save` / `backup` 保存到不是数据库自己目录的地方时,先建目标目录并取它的锁——新目录、空目录也一样(此前只在目录里已有数据库时才锁),所以另一进程持有的目录立即以 "is locked by another process" 失败、什么也不写;锁在保存返回时释放。这样的新目标(以及 `Database::backup` 写的备份目录:REST `/backup`、`/api backup` 备份已载入的库,`gbackup` 备份快照库)此后都有一个 `LOCK` 文件,zip 备份里带一个 `LOCK` 条目(几个字节,写者的 pid;没有恢复路径复制它)。公开函数 `Database::load_disk` / `load_disk_with` 像 `open` 一样取目录锁并持有到库关闭(此前不取锁,嵌入方在 gserver 旁边用它们会有两个写者)。

| 工具 / 路径 | 锁什么、何时 | 目录被另一进程持有时 |
|---|---|---|
| `gquery`、`gadd`、`gsub`、`gexport`、`gshow <db>`、`gmonitor`、`gcompact`、`gconsole --local`、`gserver <dir>`(都经 `Database::open`) | `<dir>/LOCK`,打开时取,持有到库关闭;RocksDB 另有 `rocksdb/LOCK` | 打开失败(上面的错误),不碰目录 |
| `gserver --api`(`ApiServer`) | 每个已载入库持有其 `<dir>/LOCK`(所有布局,`hold_dir_lock`);替换、改名、删除库目录的操作(drop、rename、restore、重建的 `/build`)先 `DirLock::acquire_existing` | 载入失败;生命周期操作回答 `409` / `1005 … locked by another process` |
| `gbuild`(快照,含 `--force`) | `gbuild` 从开始到结束持有数据库目录(`BuildGuard::create_and_lock`);构建在内存中进行,`save` 保存到外部目录时取它的锁(新目录、空目录也取) | 立即失败(状态 1),原目录不动;失败的构建只删除它自己新建、仍持有且不含数据库的目录 |
| `gbuild --disk` / `--rocksdb` / `--layout disk\|rocksdb`(批量) | `BuildGuard::lock`(一步声明并取 `DirLock`);安装前再查 `rocksdb/LOCK` | 构建失败,旧库不动;本进程打开着该目录时报 "is open in this process" |
| `gbuild --disk --incremental` | 取目录锁,也拒绝本进程打开着的目录;暂存后经批量安装替换 | 构建失败 |
| `gbackup` | 整个备份期间持有源库的锁(所有布局) | 拒绝("… is locked by another process (pid N)");正被服务的库请经服务器备份(`/api backup`、REST `/backup`、`gconsole` 的 `backup`) |
| `grestore`(`Database::restore_into`)、REST `/restore`、`/api restore` | 暂存之前锁目标(先锁、再修复上一次中断的恢复、再锁修复重建的目标),持有到交换完成;目标不存在时持有 `<dir>.restore-lock` | 拒绝(REST `409`、`/api` `1005 … locked by another process`),不暂存任何东西 |
| `gdrop` | `DirLock::acquire_existing`,然后把目录持久地改名为 `<name>.db.drop-old` 再删除(见 [§布局替换](#布局替换与崩溃安全)) | 拒绝 |
| `gconsole`(连接服务器时) | 不碰目录,是 `/api` 的 HTTP 客户端 | — |

同一秒内对同一个库的多个备份(REST 与 `/api` 都算,目标路径相同)按目标路径的声明(`BackupClaim`)排成先后。

---

## Review-fix campaign:提交、恢复与格式

本节汇总 review-fix campaign(2026-11,见 `CHANGELOG.md` 的 "Review-fix campaign" 一节)对提交点、崩溃恢复和磁盘文件的改动。各项都带审查编号;括号里的包名(如 r1-storage-failstate)是做这项修复的工作包,可在 git 历史里按名字找到(合并的分支 `fix/<包名>`,以及收尾时删除的片段 `changelog.d/<包名>.md`)。所有改动都**不改变数据库文件格式**([§注册表结论](#注册表review-fix-campaign-的结论))。仍未做的后续(WAL 不记录所属文件的身份、只读的 pager 打开模式、`vstree.kv` 的页校验和等)见 `docs/REFACTOR_BACKLOG.md`。

### Pager 与 WAL:提交协议、失败状态与文件锁

`src/kvstore/pager.rs`(模块文档是权威描述)。

- **提交协议**(C-1:durability#1、disk#1;durability#8、robust#12;r1-storage-failstate):`flush` 把这一批(头页 + 脏页)暂存为 `<file>.wal.tmp`,fsync,改名为 `<file>.wal`,fsync 目录;然后把这一批应用到文件,fsync 文件,删除日志,再 fsync 目录。日志**从不被截断或原地重写**,文件仍需要的日志只能被一个完整的新日志原子地替换。改名之前的失败不改变磁盘上的任何东西,在 pager 层可以重试;残留的 `*.wal.tmp`(暂存时崩溃)不是提交,下次打开时删除。
- **failed 状态**:日志得到正式名字之后的任何错误(应用某页、某次 fsync、删除日志)都使 pager **failed**:之后的写、分配页和提交都以 `GStoreError::StorageFailed` 拒绝,读照常(这一批的页都在缓存里且是脏的),它再也不写这个文件(也没有 `Drop` 时的 flush)。重新打开时重放完整的日志,所以文件恰好是最后一个日志完整的批次,绝不是两者的混合。此前下一次提交点、保存、关闭或丢弃会重试提交,截断唯一完整的日志,重写时再次失败,留下一个再也打不开的库(`I/O error: failed to fill whole buffer`)。broken 状态与服务器上的恢复见[上文](#事务-overlay-与写闸门c5)。
- **打开**(M1:disk#2、durability#7、robust#12):只有 "not found" 表示"没有日志";其它读错误(EIO、EACCES、日志位置上是一个目录……),或重放之后删不掉日志,都使 `Pager::open` 失败——因而数据库打开失败——而不是打开一个可能只应用了一半、日志会被下次提交覆盖的文件。比头部 `page_count × 4096` 字节短的 `kvstore.kv` / `vstree.kv` 被拒绝("torn or truncated")。
- **提交时只短暂独占**(disk#10,r3-storage-kernel):`DiskStore::commit`(每个提交点)经 `Pager::commit_shared` 在 pager 的**共享**守卫下计算校验和、写日志、应用、fsync;只有取走这一批和把它标记为干净(`Pager::finish_commit`)持有独占守卫。共享这个 pager 的读者(out-of-core 字典、只读视图、`ConcurrentDb` 的快照读者)不再等日志写入和两次 fsync;两步之间被写者改动的页保持脏,留给下一次提交。日志的 CRC-32 改为查表(slicing by 8),日志经 1 MiB 缓冲流式写出,不再先整份复制。r3-storage-kernel 的实测:默认页缓存的整批(4,097 页,16,797,704 字节)的 CRC-32,逐位循环 68.0 ms,查表 11.3 ms(5 次取最好,`rustc +1.99.1 -O`,负载约 9)。日志格式不变。
- **毒化的锁**(conc#8):`Pager::lock_read` / `lock_write` 恢复被 panic 毒化的锁,并把 pager 标记为 failed——读继续,之后的写和提交以 `StorageFailed` 拒绝,而不是 panic。
- **每个文件一个进程**(disk#9,r3-storage-kernel、r3-integration):`Pager::open` 在看日志**之前**对文件取咨询排他锁(Unix `flock`,Windows `LockFileEx`,即 std 的 `File::try_lock`),只要本进程还有这个文件上的 pager 活着就一直持有。另一进程的打开以 `GStoreError::Locked`("'…/kvstore.kv' is in use: another process has it open")失败,什么也不碰(连日志也不碰);此前两个进程可以互相重放、删除对方的日志并覆盖对方的页。进程内这把锁是共享的(与目录锁一样):同一进程对同一文件的第二个 pager 不会被拒绝,一个文件一个 pager 仍是调用方的责任(共享 `Arc<RwLock<Pager>>`,见 `DiskStore::shared_pager`;服务器在目录锁下只打开一次数据库)。进程的最后一个 pager 显式释放这把锁,所以正在 spawn 的子进程(短暂共享描述符)不会把它延续到 `exec`。不支持咨询锁的文件系统照旧不加锁打开。
- **唯一的 WAL 路径规则**(disk#9):`Pager::wal_path(path)` 是 `<path>.wal` 的唯一推导,`persist::wal_of`、批量安装的 `wal_of` 与 `DiskStore::open_with` 都调用它。`Pager::remove_with_wal(path)` 删除一个分页文件连同它的日志和暂存的 `.wal.tmp`——先删日志,日志绝不比文件活得久——并 fsync 目录,只忽略 "not found";`Database::use_streaming_vstree` 用它,删不掉的陈旧日志使调用在构建任何东西之前失败。
- **提交结果与 D12**(gap-txn-lifecycle#3,r2-txn):一次提交的结果取决于它的某次 flush 是否已给 WAL 起了正式名字(`Pager::wal_batches` / `DiskStore::wal_batches` / `DiskTriples::wal_batches`,只是内存中的计数):暂存不了日志 → 回滚,不可见,重载后也不持久;日志完整但应用失败 → 不持久的回答,可见,重新打开时保留。D12 的交互:脏页超过页缓存的事务,中途的 flush 已成功、之后的 flush 失败时回答"不持久",因为它的前缀已经持久。已提交事务释放的 id 留在它自己的提交点里;Materialize 模式在那个提交点成功之后才在内存字典里 retire 它们。
- **格式**:无变化。没有 feature bit;暂存文件从不被重放,所以老二进制仍然正确;WAL 记录分帧与每个头部都与之前相同。WAL 仍不记录所属文件的身份(disk#9 的那一半需要新的分帧和 feature bit,会让每个新文件对老二进制不可读,见 `docs/REFACTOR_BACKLOG.md`);跨进程锁、唯一的路径规则和先删日志的检查删除已经关闭了审查指出的失败路径。

### 布局替换与崩溃安全

`src/db/fsutil.rs` 的 stage + swap + fsync 原语(`StagedDir`:暂存在目标旁边,整棵树 fsync,换入时 fsync 父目录;`InstallError::committed` 区分提交点之后的失败;durability#15)与 `src/db/persist.rs`。

- **构建从不写在它要替换的库上**(durability#5、durability#15,r2-layout-cli):批量构建以及现在的增量磁盘构建把 `kvstore.kv` + `vstree.kv` 暂存在 `.bulk-tmp/`,再走批量安装([§批量构建](#批量构建));增量 RocksDB 构建暂存 `rocksdb.tmp/` 再换入(`rocksdb.tmp` → `rocksdb.new` →(`rocksdb` → `rocksdb.old`)→ `rocksdb`,每次打开照旧恢复)。卡在交换提交点之后的构建被拒绝,由下一次打开补完(r2-integration)。
- **删除数据库**(durability#10):目录先持久地改名为 `<name>.db.drop-old`(改名后 fsync 父目录,从此数据库就没有了),再删除。中断的删除(崩溃、断电、删除时的 I/O 错误)留下的 `<name>.db.drop-old` 不被列出、不被注册,也没有谁读它的数据——手工删除,或再删一次同名库(下一次删除这个名字时会清掉它)。
- **备份复制**(`Database::backup_dir`,`gbackup` 对未打开目录的文件级复制;durability#4):复制之前先持久地删除目标中的 `vstree.kv`、各 WAL、快照提交记录、`meta.bin`、陈旧的 `rocksdb/`(r2-integration)以及源里没有的每个数据库文件,最后才复制 `meta.bin`(快照布局的标志),所以复制中途失败或崩溃不会留下新 store 配旧 VS-tree(header 说它新鲜,`GSTORE_VS_POLICY=tree` 时会剪掉之后加入的实体)或混合的快照。`gbackup` 与 `backup_dir` 拒绝数据库目录本身或其内部的目标(bulkcli#4)。
- **旧式影子快照**(durability#9):在磁盘库上保存的快照,若保存在删掉 `kvstore.kv` 之前崩溃,下次打开时合并进 `kvstore.kv`(需要几次页提交就几次)。"应用影子"这一决定现在先记在持久的标记 `SHADOW.apply` 里(内容 `kvstore.kv dev=<d> ino=<i>`,绑定 store 文件的设备与 inode);发现它的打开会再次(幂等地)应用影子,然后才退役影子文件(`store.bin` 最后)和标记。此前合并的提交让 `kvstore.kv` 比影子新,第二次中途崩溃后的打开按 mtime 跳过影子,服务两种状态的混合。`remove_other_layouts` 删除这个标记;指向另一个 store 文件的陈旧标记什么也不决定。目录切换到别的布局时,先删快照的提交记录和 `store.bin`,磁盘布局的文件先于它们的 WAL 删除。第一次决定是否应用影子仍按 mtime(`>=`),那是旧版构建留下的唯一信息。
- **规则目录的交换**(durability#11,r4-ext-docs、r4-integration):`reason_rule_files/` 经数据库目录里 `STORE_SWAP` 的兄弟名字(`reason_rule_files.tmp` / `.new` / `.old`)替换;残留的由下一次规则操作补完或删除(强制重建时由 `remove_rule_dir` 删除)。`gbuild --force`、批量重建与覆盖已有库的 REST `/build` 在规则锁下把被替换库的规则先改名移开再删除,中断的规则交换或删除途中的崩溃都不会让旧规则回到重建的库里。

### RocksDB:语句日志与崩溃恢复

`src/backend/rocks.rs`,r1-rocksdb(H-11、H-12,gap-rocksdb-exec#1、#2、#4、#6、#7,disk#11)。

1. **每条自动提交语句都记 undo 日志**,与事务一样:一个 SPARQL UPDATE 请求(`INSERT DATA`、`DELETE`/`INSERT … WHERE`、`LOAD`、`CLEAR`、`DROP`、`ADD`/`COPY`/`MOVE`)和一次 `insert_sources` / `remove_sources` 导入(`gadd`、`gsub`、`/api batchInsert` / `batchRemove`)。中途崩溃(SIGKILL、OOM、断电)在下次打开时整体撤销,库总是停在语句边界上,不论语句多大(磁盘布局的语句只在脏页装得下页缓存时才崩溃原子,D12)。中途失败的语句保留它已应用的部分(既定语义),它的日志也照常提交——panic 时也是——所以日志不会一直开着去拒绝之后的语句或在下次打开时撤销之后的写。语句释放的 id 在语句结束时、在删除日志的那一批里释放,`dict` CF 条目也在那一批删除。仍然只按三元组原子的:直接 API 的写(`insert_triple`、`materialize_rdfs` …)、REST `/batchInsert` / `/batchRemove` 和对已载入库的 REST `/upload`(见 `docs/REFACTOR_BACKLOG.md`)。
2. **日志的词记录**:事务或语句里新写的词与它在同一批里记进日志(种类字节 2,`-` 操作,`(id, id, id)`),所以词与三元组之间的崩溃让下次打开释放这个词。每个更早的二进制都读得懂:它把词记录重放成一次空删除加上同样的回收。没有新 feature bit,`LAYOUT_VERSION` 仍是 `"1"`,CF 与键不变。
3. **提交点 = WAL fsync**:在库自己目录的保存(服务器每次已确认写之后的 `check_point` 保存)只 fsync RocksDB 的 WAL,不再 flush 全部七个 CF 的 memtable(此前每次写留下约三个表文件和三个打开的描述符,写入慢几倍——审查测得 5–6 倍,这里约 13 倍——几百次写之后在 `ulimit -n 1024` 下以 `Too many open files` 结束)。memtable 由显式的 `Database::checkpoint`(`gcompact`)、备份和关闭时 flush。打开文件数、info 日志与 WAL 总量有界([上文](#rocksdb)的调优键)。
4. **代价**:记日志的语句为它改动的每个三元组 / 四元组(再加每个新词)在同一批里往 `meta` CF 写一条日志记录,语句结束时删除(至多 1,024 条用点删除,否则一次范围删除)。所以 RocksDB 上非常大的 `gadd` / `gsub` / `LOAD` 会暂时存下与其变更集一样大的日志;它释放的 id 一直留到语句结束(`remove_sources` 在批次之间只保留已死的候选,每个候选在它所在批被读到时和结束时各探查一次)。
5. **打开时的崩溃恢复是分块、可续的**:每次撤销最新的 65,536 条日志记录(在上一块之下的反向迭代器),撤销后把这一块的记录在一批里删除,同一批把回收候选写进 `meta` CF 新的**待定区间**(`\0reclaim-pending\0` ‖ 空间标记 ‖ be32(id),空值)。磁盘上的日志始终是更老的前缀,恢复中途被打断就从停下的地方继续。`Database` 在服务之前以每批 65,536 个处理待定候选,把每批与它的无引用词的 `dict` CF 条目一起删除(`RocksStore::pending_reclaim` / `settle_pending`;`take_replayed` 一次性交出待定候选,供底层用户使用)。代价:内存以一块(加 RocksDB 的 memtable)为界;时间约为每条记录一次写批,与语句本身相当。此前打开把整个日志读进内存,9000 万条记录的日志每次打开都会重复耗尽内存。
6. **格式**:`meta` CF 里附加的键区间,没有 feature bit,`LAYOUT_VERSION` 仍是 `"1"`。早于它的二进制忽略这个区间,崩溃恢复留在那里的候选一直保持已分配,直到 `gcompact --reclaim`(没有损坏)。
7. **校验**:`gcompact --check`(`Database::verify_integrity`)运行 `RocksStore::verify_stats`:`stats` CF 的每个计数器都等于从 SPO 键重新计数的结果,POS 与 OSP 恰好持有 SPO 的三元组,全局、缓存与镜像的计数器一致——流式进行(内存恒定,每种排序与计数器前缀各读一次)。每个解码的键都检查长度,长度不对的键(不完整的复制、外部工具、位腐烂)使语句以点名 CF 的读错误失败,而不是 panic。载入 `dict` CF 时先按 id 空间的界检查每个 id,再可失败地预留槽位:越界的 id 使打开以 "corrupt RocksDB dictionary: entity id N is beyond its id space" 失败,不再分配约 100 GB 后中止进程(db#18,r3-storage-kernel)。

### redo 日志与增量备份

`src/db/logs.rs`(分帧、遍历、追加器),r3-db-redo(durability#6、db#2、db#3、db#8、db#9、robust#10、gap-doc-claims#1)与 r2-terms。这些日志只是库 API(`Database::enable_redo_log*`、`enable_update_log*`、`replay_*`、`backup_incremental`、`restore_incremental`),服务器与命令行工具都不打开它们。

- **提交规则**:一次提交(一个自动提交的 UPDATE、一个事务、一个直接 API 变更或批量调用、一次 `materialize_rdfs` / `run_rules`)的记录一次 `write` 追加;自动提交 UPDATE 与事务的记录在其提交点**之后**追加并 `fdatasync`。直接 API 的变更没有自己的提交点,应用时追加,由下一个提交点 `fdatasync`;`Database::sync()` 也是日志的提交点。update 日志只收已提交的语句。
- **分帧**:每条记录的头部带 CRC-32 记号 `c=<8 位十六进制>`,一次提交的最后一条带 `e=<8 位十六进制>`(`REDO I 0 44 e=1a2b3c4d`、`REC <millis> <changed> <len> e=…`)。重放只在读到某次提交的 `e=` 记录后才应用它,被崩溃截断的事务追加整个不重放。没有记号的记录(此前写的所有日志)照旧重放,每条自成一次提交;早于记号的读取器忽略这个尾部头部记号,逐条读新的插入 / 删除记录(没有全有或全无规则),对新的 clear 记录则明确拒绝("unknown redo.log op 'C'",见[兼容矩阵](#兼容矩阵phase-6-结束时实际情况)下的更正)。本版本读每一种更老的日志。增量备份的 `INCREMENTAL` 标记仍是 `GSTORE-INCREMENTAL 1`。
- **残尾与损坏**:追加途中崩溃留下的不完整的最后一次提交,重放时跳过(带警告),下一次追加(或备份)把它截掉,从最后一个完整的提交开始;此前下一会话接在残尾之后追加,之后的每条记录都读不到。日志中间损坏的记录(校验和、终止符)报告为损坏(`redo.log is corrupt at byte N: …`),绝不静默跳过。判据:有缺陷的记录是残尾,除非它后面跟着一个完整的记录(完整且校验和正确)——那说明它的长度字段坏了,是损坏;正文里只是看起来像记录头的文本(update 日志的语句可能有一行以 `REC ` 开头)不算。增量的 `incr.log` 必须整体可重放:不完整,或长度与 `INCREMENTAL` 标记不符,都使 `restore_incremental` 失败。
- **追加失败**:失败的追加(`ENOSPC`、`EFBIG`、`fdatasync` 失败)立即再截掉,文件仍止于提交边界,它的记录按顺序欠给下一次追加;自动提交 UPDATE 报错("the update is applied and committed, but appending to redo.log '…' failed: …"),`commit()` 报 `CommitFailure::CommittedWithError`,`sync()` 报告它。`backup_incremental` 在有欠账时拒绝,在提交结果未知的提交点失败之后("logs in doubt")也拒绝,日志范围里有损坏记录时同样拒绝,直到一次完整的 `backup()` 开始新的链(r3-integration)。打不开的日志(除 "not found" 之外的 I/O 错误)对 `backup` / `backup_incremental` 是错误。
- **畸形的旧词**(r2-terms):格式良好的词的记录不变。只有更老的版本才可能存下严格读取器拒绝的空白节点标签或语言标签;这样的记录把标签写成 `\uXXXX` 转义——老二进制本来就重放不了这种记录。快照、磁盘与 RocksDB store 的格式都不变(D7:字典键不动,`Term` 的 `Display` 仍是存储的键形式)。
- **格式注册表不覆盖它们**:`redo.log`、`update.log` 与 `incr.log` 不是分页文件,kvstore 的格式注册表(feature bit,`kvstore/format.rs`)管不到它们;`c=` / `e=` 分帧不需要 feature bit(每个老读取器都忽略的尾部头部记号)。

### 读取时的校验

r4-storage-format(disk#4、disk#5、disk#15、gap-accel-soundness#4、robust#11),无格式变化:

- **`vstree.kv` 边读边校验**:种类字节、条目数 ≤ 32、子节点 id 在 `1..page` 且在父节点之下、每个节点的签名是其条目签名的并且等于父节点里它那一项、叶子深度一致、深度 ≤ 8、每个节点至多访问一次、头部的根与计数。损坏的文件不过滤任何东西;`open_disk` 拒绝缺失、为空或头部损坏的 `vstree.kv`("… (remove it to rebuild the index)")。没有页校验和,有些损坏仍看不出来(叶子里被改掉的实体 id,或某个叶子条目丢了一位而同一叶子的另一条目也置了这一位,仍可能藏住一个匹配)。`eaf5320` 夹具里的 `vstree.kv` 能通过新校验。
- **`pred_stats` 计数饱和**:磁盘布局逐谓词的 `pred_stats` 计数是 `u32`,在 `u32::MAX` 处饱和(表示"未知,至少这么大"),删除不会让它回落,直到 `DiskStore::rebuild_stats` 重新计数;RocksDB 的 `c|p` 是精确的 `u64`(批量与增量构建都是)。
- **溢出链的读取有界**:记录长度至多为文件各页的有效载荷,提前结束的链是错误。

### 持久性审计(fsync)

r5-test-audit(tests-b#3、tests-a#13),无格式变化。

- 提交点依赖的 fsync 都经 `fsutil` 的 `sync_file` / `sync_file_data` / `sync_dir`(pager 的 WAL、批量安装、redo 与 update 日志、规则文件),RocksDB 的 WAL sync 也自行报告;doc-hidden 的 `gstore::sync_probe` 为每个活着的 `Probe` 记录每次 sync(文件身份、长度、mtime、文件内容摘要、目录项、调用点)。没有 probe 时每次 fsync 只多一次原子读。
- `tests/fsync_audit.rs`(23 个测试,`--features rocksdb` 再加 4 个)检查每个提交点前后:每个改动过的文件在最后一次写之后被 sync 过,每个改动过的目录在最后一次目录项变化之后被 sync 过,RocksDB store 的日志在最后一次写之后被 sync 过;协议文档化的每个中间状态(pager 的 WAL;快照提交记录;批量安装的 `cleared` / `kv-synced` / 删除规则 / `vstree-placed` / `layouts-removed` / `cleaned`;目录交换;外部保存与备份的复制;规则账本先于规则文件、先于它记录的插入)都按顺序被某次目录 sync 记录下来。覆盖的提交点列表见 `CHANGELOG.md`。
- 审计发现并修好的:提交点**新建的每个目录**都在父目录里持久化(`fsutil::create_dir_durable`:数据库目录、备份目录、服务器的根目录、PFN 与规则目录);构建或外部保存之前就已存在的数据库目录(工具的声明),其目录项同样 sync(`fsutil::ensure_dir_durable`,每次构建或保存一次目录 fsync;`BuildGuard::create_and_lock` 持久地新建目录及缺失的上级目录)。
- 审计不能察觉其删除的三个 sync 作为双保险保留(`persist::write_vstree_kv` 的目录 sync,RocksDB 批量构建在 memtable flush 之后的 `sync_wal` 与它对暂存区的 `sync_dir`);整体改名数据库目录的操作(`/api rename`、`is_backup=true` 的 `drop`)不在审计之内(审计按路径记文件)。见 `docs/REFACTOR_BACKLOG.md`。

### 注册表:review-fix campaign 的结论

格式注册表(`src/kvstore/format.rs`)没有新增 feature bit,`MAGIC_V1` / `MAGIC_V2`、根槽表与上面的兼容矩阵都不变;RocksDB 的 `LAYOUT_VERSION` 仍是 `"1"`。逐项(D7):

| 改动 | 落在哪里 | 为什么不需要格式变化 |
|---|---|---|
| WAL 暂存为 `<file>.wal.tmp` 再改名 | pager 文件旁的临时文件 | 暂存文件从不被重放;老二进制看不到它也照样正确;WAL 分帧不变 |
| pager 文件锁、唯一的 WAL 路径 | 进程内状态 | 不写任何东西 |
| `named.bin` 读错误使打开失败;`SNAPSHOT.commit` 只列成员名 | 快照布局 | 只是读得更严格,字节不变 |
| `SHADOW.apply` | 磁盘库目录里的临时恢复标记 | 不是格式变化;老二进制忽略它,照旧按 mtime 决定 |
| 规则目录交换的 `reason_rule_files.tmp` / `.new` / `.old` | 数据库目录 | 老二进制忽略它们;残留的由下一次规则操作处理 |
| `<name>.db.drop-old` | 数据库目录的兄弟 | 不被列出、不被注册,只是中断的删除 |
| 保存 / 备份目标里的 `LOCK` 文件,zip 备份里的 `LOCK` 条目 | 目录 / zip | 锁文件,恢复路径不复制它 |
| 批量安装的 `COMMIT` 标记 | `.bulk-tmp/` | 咨询性,没有恢复读它 |
| RocksDB 日志的词记录(种类字节 2) | `meta` CF 的日志 | 每个更早的二进制把它读成空删除加同样的回收 |
| RocksDB 回收待定区间 `\0reclaim-pending\0…` | `meta` CF | 附加的键区间;老二进制忽略,候选保持已分配直到 `gcompact --reclaim` |
| redo / update 日志的 `c=` / `e=` 记号;clear 记录的占位三元组;畸形旧词的 `\uXXXX` 转义 | `redo.log`、`update.log`、`incr.log` | 不是分页文件,注册表不覆盖;老读取器忽略尾部记号,对新 clear 记录明确拒绝 |
| `Pager::wal_batches` | 内存计数 | 不写任何东西 |
| `vstree.kv` 校验、`pred_stats` 饱和 | 读取路径 | 读得更严格;饱和值 `u32::MAX` 是已有的 `u32` 字段 |
| `raft.journal`(魔数 `GSRAFTJ1`,CRC 校验的只追加记录,头部记录所属节点 id) | 实验性集群 `gnode cluster --state-dir` | 是集群的新文件,不是数据库格式,自带魔数(r5-cluster) |

---

## 不变量

默认图的读路径(VS-tree、优化器、图算法、SPARQL 引擎)对 `TripleSource` 泛型,新后端在这一面只需实现 `TripleSource + MutableStore`(即 `StorageBackend`)+ 字典 seam,**这几样一行都不改**——这正是本设计要保护的核心资产。命名图始终在内存 `TripleStore` 中(D11:`Evaluator.named` 的类型是 `Option<&BTreeMap<u32, TripleStore>>`,SERVICE 构建的是 `Evaluator<TripleStore>`),持久化、日志、事务与 id 回收按布局写在 `Database` 里(封闭的 `Backend` 枚举加 Materialize sink,约 70 处分派):新增布局要改这些位置;`src/db/layout_matrix_tests.rs` 在每种布局上跑同一组写/维护操作(结果、内容、计数、`verify_integrity` 都必须与快照布局相同,并经得起 checkpoint 与重开;布局经对 `StorageKind` / `DiskQueryMode` 的穷尽 `match` 列出,新变体不加入矩阵就编译不过),漏掉的分支会让它失败;`tests/storage_backend_conformance.rs` 对 `TripleStore`、`Backend` 的各变体、`DiskTriples`、`DiskStore`、`DiskSource`、`OverlayView`、`ShardedStore` 与 `RocksStore` 跑同一套泛型检查(契约顺序、写的回答、C1 计数;r4-api-hygiene,design#17)。(更正,review-fix campaign,gap-doc-claims#7、db#6、design#5:本段原先说新后端只需实现这两个 trait 与字典 seam、引擎一行都不改,那只对默认图的读路径成立。)Phase 6 之后,一个新后端还要遵守:读面的顺序契约与 C1 计数的精确性([上文](#读面契约顺序契约与-c1-计数)),读错误经 `note_read_error` 上报且 `has_*` 在出错时回答 `true`(C2),词先于引用它的三元组持久化,以及只在数据确实变化时推进 `mutation_count`(C9)。
