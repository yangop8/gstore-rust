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

即 Stream 的几何均值是 Materialize 的 1.47 倍,Q14、Q6、Q12、Q13 各是 2.15、2.12、2.07、2.00 倍,插入也更慢。差距在大结果集上:Stream 用 out-of-core 字典(`TermCache`,未命中时读 `id2*`)解析每个结果 id,Materialize 用内存 interner。规模越大越明显:`bench/results/phase6-final-lubm82.json`(univ 82,11,135,159 个三元组)里 Stream 的几何均值 30.18 ms、Materialize 16.09 ms,Q6 / Q13 / Q14 分别是 984.84 / 1303.58 / 876.09 ms 对 220.72 / 282.09 / 165.29 ms。持续写入时还有一个差别:`bench/results/phase6-final-watdiv10.json` 的混合负载(8 个读 F5 的客户端 + 每 100 ms 一次单条插入,`/sparql`,check_point off)里 Stream 读者 191.6 qps、Materialize 2550.3 qps——每次提交都推进 `DiskTriples` 的 epoch,长 posting list 缓存在新 epoch 的第一次读取时被整体清空。反过来,Stream 的打开时间和内存与数据量无关(univ 82:热打开 0.9 ms、打开后 6.4 MiB),所以超出内存的库或频繁打开的库应选 Stream。

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

写失败会使 store 进入 **broken 状态**:之后的写、读(包括字典写,`DiskDict::with_write_check`)都被拒绝,直到重新打开数据库。`CLEAR DEFAULT` 在一个 read scope 里把默认图的 id 读进内存(每个三元组 12 字节),再在一次闸门 drain 下删除(没有范围删除,D9)。Stream 库从不构建内存子串索引(子串 FILTER 直接扫描,结果仍精确,D5)。`into_memory()` 读出整个 store 与全部词(保留 id)。

**`vstree.kv` 与 VS-tree 策略**:Stream 模式下 `vstree.kv` 只在新鲜(fresh)时使用,打开时从不重建(除非 `vstree_on_open`;Materialize 模式打开时会重建过期的 `vstree.kv`);`rebuild_index()` 一次流式扫描 SPO/OSP 重写它,在打开的事务结束后才执行,签名之前先做一次提交点(数据先落盘),并在 store 头部把它标记为 fresh。查询在默认的 `VsPolicy::Auto` 下**不读 `vstree.kv` 的页**:对 out-of-core 树,候选集的剩余谓词位直接对 store 检查,没有候选集的变量从 store 的 posting list 取出带这些位的实体;在与 store 一致的树上,所得候选集与遍历树完全相等(计划、答案和顺序都不变)。`vstree.kv` 的新鲜度仍决定 VS-tree 语义是否适用(过期的在打开时丢弃);`GSTORE_VS_POLICY=tree` 恢复页遍历。R4-VS 的安静测量(代码 `b9e8dc0`,univ 38,Stream):`GSTORE_VS_POLICY=tree` 时 Q1–Q14 几何均值 47.70 ms,`Auto` 时 11.66 ms。会话内发生删除之后,磁盘库的候选集跟随当前 store(答案不变,无 `ORDER BY` 时的顺序与新重建的树一致)。`vstree.kv` 的 pager 头部若带任何 feature bit 会被拒绝(打开和在其上构建时都检查)。

### 事务 overlay 与写闸门(C5)

`gstore::store::{Overlay, OverlayView, OverlayChanges}` 与 `gstore::kvstore::{DiskTriples, DiskReadView, DiskSource}`(`src/store/overlay.rs`、`src/kvstore/disk_triples.rs`)。

**Overlay**:事务的新增与删除与底座分开保存,保持 `added ∩ base = ∅`、`removed ⊆ base`、`added ∩ removed = ∅`,所以 `insert` / `remove`(及可失败的 `try_insert` / `try_remove`,只在 overlay 自己判断不了时才探查底座)返回的 changed 标志与物化后的 store 完全一致。`OverlayView` 在 `(base − removed) ∪ added` 上实现整个 `TripleSource`:列表按契约顺序归并,计数是精确算术,`distinct_*` / `pred_distinct_*` / `num_predicates` / `distinct_p_of_*` 只探查 overlay 碰过的键(全局与逐谓词的结果缓存到下一次变更;计算中遇到读错误则不缓存),`has_*` 读错误时为 `true`。每种键各一个 8 KiB 的单哈希 Bloom 过滤器,读未被触及的键只多一次位测试;触及数十万个键的事务会让过滤器饱和,此后每次读都走(仍精确的)归并路径。

**`DiskTriples` 的事务**:`begin()` 之后每个三元组写都进 overlay,**提交前没有三元组到达 pager**;读能看到本事务的写(read-your-writes);`rollback()` 不碰 pager;`commit()` 依次应用排队的命名图操作、删除、新增,每一步是一个原子 pager 操作。字典词可以在提交前进 pager(孤儿词无害)。事务之外的写立即应用(auto-commit)。`commit_point(&self)` 在有已应用写时做一次带 WAL 的持久提交。事务正在新增三元组时 `vstree_stale()` 为真(`vstree.kv` 按已提交状态构建,会剪掉事务自己的新边);删除不算,树仍是可靠超集。D12 仍然适用:单个逻辑写是原子的,但脏页超过页缓存的大事务是分批提交的——持久但整体上不具备崩溃原子性。

**写闸门**:改 pager 之前,写者把闸门标记为 draining、拉起闸门内每个快照读者的取消标志、等它们全部离开、应用变更,然后推进 **epoch**(只在数据确实变化时;空写既不取消谁也不推进)。在同一 store 的快照读里面发起写会被直接拒绝,而不是等待自己;在快照读里 `commit()` 也在取走事务之前被拒绝,事务保持打开。读者若一直不看取消标志,写者等待超过 10 s 会记一条警告。

**`DiskReadView`**(`DiskTriples::read_view()`,`Clone + Send + Sync`,持有共享 pager 与闸门而不是 `DiskTriples`):`with_base(|&DiskStore, &Arc<AtomicBool>| …) -> Result<R, ViewError>` 先登记为读者,每次调用新建一个只读的 `DiskStore::from_shared_pager` 视图,并把读者的取消标志交给闭包(交给 `Evaluator::with_cancel`)。闸门在 drain 或 epoch 变了(调用前后各查一次;被取消的读者总是得到它)时返回 `ViewError::Expired`,`DiskTriples` 被丢弃或关闭后返回 `Closed`,store broken 时返回 `Io`。视图看不到打开的事务的写,也不自动重试:快照读者看到的是开始时的状态,或在调用方重试之后看到的已提交状态。快照查询必须用 `DiskSource` 包装 store,并把取消标志交给求值器。

**broken 状态**:任何写或 flush 错误都会使 store broken:之后拒绝写,读报告读错误,视图回答 `Io`,`disk_store()` / `shared_pager()` 返回错误,直到重新打开;broken 的 store 被丢弃时不 flush。`Drop` 先 drain 闸门、把视图标记为 closed、尽力 flush 已应用的写;`close()` 做同样的事并报告 flush 错误;打开的事务两者都丢弃。

**锁顺序**(C8):`DiskTriples` 闸门 → pager;闸门的 mutex 从不跨 pager 操作持有;`DiskDict::intern` 只拿 pager 写锁(从不反向)。每个逻辑 kvstore 写在整个操作期间持有 pager 写锁(R1-KV 的 operation bracket:操作中途缓存不为腾地方而提交,出错或 panic 时整个操作连同计数器回滚);`DiskStore` 写入中的 panic 被回滚并以错误返回,不会毒化共享 pager。

### 字典(C4)

- **API**(`gstore::dict`):`TermStr<'a>`(`Borrowed(&str)` 来自内存字典,`Shared(Arc<str>)` 来自后端;`Deref<Target = str>`,按文本比较与哈希);`Dictionary::term` / `pred`(`Option<TermStr>`,后端错误记入语句的 read scope)、`try_term` / `try_pred` / `try_*_id`、`prefetch`、`resolve_sorted` / `resolve_many`(排序批量)、`for_each_term`、`materialize()`、`id_limit`、`generation()`、`backing()`、`cache_stats()`。
- **内存布局**:每个串只存一次(id → 串向量里的 `Arc<str>`),串 → id 一侧是开放寻址的 id 表(每槽 8 字节,经向量比较);三个 interner 是 `Arc<InternerData>`,克隆 `Dictionary` 是 O(1),对共享克隆的第一次写只复制它改动的那个 interner。
- **`dict.bin`**:格式不变(正向表、带空洞的反向向量、空闲列表),由手写序列化器按 id 升序写正向表,同样的输入写出相同的字节(Phase 6 之前按 `HashMap` 的随机顺序);老二进制读新文件、新代码读老文件都得到相等的字典;正反两半不一致的文件被拒绝。空闲列表仍是同样的 `seq` of `u32`,现在升序。
- **out-of-core 后端** `DiskTermSource`:可失败、`&self`、`Send + Sync`;`lookup`、`resolve -> Arc<str>`、`resolve_sorted`、`intern` / `free`(默认拒绝;只读后端拒绝新词)、`prefetch`、`for_each`、`count`、`id_limit`、`generation`、`sync`。`DiskDict` 用 `BTree::get_with` / `IvArray::get_with`(无逐次拷贝)实现它;批量与预取按 id 顺序逐 run 读取(`IvArray::for_each_key`,只解码请求的槽);`DiskDict::writable()` 能 intern(新词推进 generation,缓存原地更新),`DiskDict::free` 同时推进 generation 与缓存 epoch(清空缓存);`slot()` 以已分配上限而不是存活数为界(有空洞时)。
- **有界缓存**(`gstore::dict::cache`):`TermCache`(`(space, id) → Arc<str>`,默认 64 MiB)与 `KeyCache`(`(space, key) → id`,默认 16 MiB),各 16 个分片、CLOCK 淘汰、按字节计费(串长 + 72)。`DiskDict` 的缓存只在某个 id 或键可能改变含义时(释放了一个 id)整体清空;intern 只会新增,缓存原地更新。
- **老 `&str` API**:`id_to_string` / `predicate_to_string` 只对内存字典回答;对 out-of-core 字典返回 `None` 并让语句失败(fail closed,不再 `Box::leak`)。生产代码已不再调用它们(图函数和 PFN 的 `GraphCtx` 经 `term()` / `pred()` 读标签)。
- **查询侧**:SELECT 结果按 4096 行一块物化,字典在磁盘上时每块每个 id 空间一次排序批量解析;每查询的 id → term / id → value 备忘(`IdMemo`)只用于 out-of-core 字典,上限 8 MiB,超过 16 KiB 的键不进。子串索引(`CONTAINS` / `STRSTARTS` / `STRENDS` / 无元字符 `REGEX` 的候选剪枝)按字典 generation 缓存(`Database::string_index()`),只对内存字典构建,建好后常驻;R2-DICT 安静测量阶段(phase6 `0c9bdb6`)测得 univ 38 时它有 430,954,544 字节(D5)。

### 格式注册表与兼容矩阵

**注册表**(`src/kvstore/format.rs`,C3)是 `kvstore.kv` 与 `vstree.kv`(也是 pager 文件)全部版本标记、根槽与 feature bit 的唯一所有者;节点、IvArray、溢出链与 WAL 的格式在 Phase 6 中没有变化。

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

#### 兼容矩阵(Phase 6 结束时实际情况)

| 目录 | 新二进制 | Phase 6 之前的二进制 |
|---|---|---|
| 快照(任何年代) | 读写;id 回收开启(下次 save 时持久化) | 读写(字节格式相同) |
| `kvstore.kv` V1,Phase 6 之前就有的 | 读写,Stream 或 Materialize;id 回收关闭(`reclaim_unused` 被拒),直到 `gcompact --enable-reclaim` 把它升为 V2 | 读写 |
| `kvstore.kv` V1,新二进制在 `id_reclaim=off` 下新建的(批量构建或增量构建) | 读写 | 读写 |
| `kvstore.kv` V2(`FREE_IDS`;`id_reclaim` 为默认 `on` 时新建的每个磁盘库,或升级过的库) | 读写 | **拒绝**:无注册表的老二进制报 "not a gStore KV file";有注册表但早于 R4 的报 "this gStore KV file uses feature bits 0x1 that this binary does not support" |
| RocksDB layout `"1"`(含批量构建的) | 读写;打开时不重建 VS-tree | 读写 |
| `vstree.kv` | 只接受 `FEATURES == 0` | 读写 |

老文件**从不被隐式升级**。降级(V2 → 老二进制)的办法:新二进制 `gexport`,老二进制 `gbuild`。另外,`redo.log` / 增量备份里的 `CLEAR` / `DROP` / `MOVE` / `COPY` 记录(`C`、`N`)是新的,老二进制恢复这样的增量备份会以 "unknown redo.log op 'C'" 失败。

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
- **RocksDB 目标**(`BulkTarget::RocksDb`,需 `rocksdb` feature):同样的排序流按 CF 写成 `SstFileWriter` 文件(每个 CF 用自己的选项,256 MiB 切分):`spo`、`pos`、`osp`、`gspo`、`dict` 及每个统计计数器(精确,零值计数器不写);在 `.bulk-tmp/rocksdb` 新建的 store 依次 ingest(`move_files`,同一 CF 的文件互不重叠,直接落到最底层,字典最先),最后写 `meta` 名字和 `layout = "1"`,flush、sync WAL、关闭、重开校验计数器、fsync 每个文件。键值与逐条构建 `Database::build_rocksdb_*` 完全相同(任意长度的词都能构建)。安装(`bulk::ROCKS_CRASH_POINTS`):`staged` → `swapped`(经 `rocksdb.new` 改名为 `rocksdb/`,**提交点**,下次打开会补完)→ `rules-removed` → `layouts-removed` → `cleaned`。
- **谁走批量路径**:布局为 disk 或 RocksDB 的构建。`gbuild` 的布局由 `--disk`、`--rocksdb` 或 `--layout snapshot|disk|rocksdb` 决定,都没给时取 `./conf/conf.ini` 的 `[storage] default_layout`,再缺省为 snapshot(快照在内存中构建后保存);`--layout` 与 `--disk` / `--rocksdb` 矛盾是用法错误。库函数 `Database::build_disk`、`build_disk_from_sources`、`build_disk_bulk_from_sources`。REST `/build` 与 `/api build` 的 `layout=snapshot|disk|rocksdb`(也接受 `memory`、`kvstore`、`rocks`;默认取服务器配置的 `default_layout`,否则 snapshot;RocksDB 需要服务器带 `rocksdb` feature,否则 `400` / `1003`)。旧的逐条路径保留为 `#[doc(hidden)] build_disk_incremental` / `build_disk_from_sources_incremental`(`gbuild --disk --incremental`、比较测试)。`Database::build_rocksdb_*` 是逐条写的小数据辅助函数(D15)。
- **gbuild 参数与配置**:`--memory MB`(默认 1024)、`--threads N`(`0` = 全部核心,默认)、`--tmp-dir DIR`、`--fill PCT`(50–100,默认 90,只用于 disk 布局)、`--incremental`;布局不是 disk / RocksDB 时这些参数是用法错误(退出码 2)。`conf.ini` `[system]`:`build_memory_mb`、`build_threads`、`build_tmp_dir`、`build_fill_factor`,`build_multi_thread=off` 表示一个线程;gbuild 读 `./conf/conf.ini`(若存在),包括 `[system] id_reclaim` 与 RocksDB 调优键。
- **构建拒绝的目录**:另一进程持有的目录、本进程有数据库打开着的目录(`DirLock::held_here`,"is open in this process")、正被另一个批量构建重建的目录,以及 RocksDB store 正被任何进程打开的目录(Linux 上从 `/proc/locks` 读 `rocksdb/LOCK`,`bulk::commit::rocksdb_lock_holder`)。服务器要重建一个已载入的库,先 unload(释放句柄)再构建,REST `/build` 就是这么做的:先保存(check_point off 时未保存的已确认写)、退役已载入的副本、构建、再打开并服务新库;构建失败则重新服务旧库。

### id 回收

`src/db/reclaim.rs`,`gstore::db::{IdReclaim, set_default_id_reclaim, default_id_reclaim, ENV_ID_RECLAIM}`。

- **策略**:`IdReclaim::Immediate`(默认,`on`)或 `Off`(Phase 6 之前的行为:id 只由显式的 `Database::reclaim_unused` / `gcompact --reclaim` 释放)。`conf.ini` `[system] id_reclaim=on|off` 由 gserver(启动时与 `/api refreshconf`)和 gbuild 读取;其它命令行工具(gadd、gsub、gquery…)只看环境变量 `GSTORE_ID_RECLAIM=on|off`,环境变量总是优先。`Database::set_id_reclaim` / `id_reclaim` / `reclaims_ids` 针对单个库。
- **何时**:每次实际生效的删除——`DELETE DATA` / `DELETE … WHERE`、四元组删除、`CLEAR` / `DROP` / `MOVE` / `COPY`、`remove_sources`(`gsub`、`/api batchRemove`)、失败 `LOAD` 的撤销、事务新增的回滚——之后,被删三元组的 subject、predicate、object(及图 IRI)成为候选;语句结束时(auto-commit)或 `commit` 时(事务把释放推迟成一批)释放其中无人引用的。事务 `rollback` 时,它新铸造的词也是候选。
- **"被引用"**:节点 id 是默认图的 subject 或 object(`has_subject` / `has_object`,读错误时为 `true`),或命名图计数它(`NamedGraphs::node_refs`,含图 IRI——`GSPO` 以图为首键,所以四元组的引用以内存中的 `NamedGraphs` 计数为准);谓词同理(`has_predicate`、`NamedGraphs::pred_refs`)。每个候选凭自己的探查独立判定:读错误使其悬而未决的候选不释放也不丢弃,在下一条语句再查,最多 `CHECK_ATTEMPTS`(4)次,之后记一条警告留给 `gcompact --reclaim`;它不拖住其它候选。
- **顺序与各布局的持久化**:释放总在语句的 redo 记录和写穿之后;先持久化,再由字典 retire 该 id、`touched()`,等到没有比 retire 更早的 pin 时才 release 供复用。
  - 快照:下次 `save` 时(`dict.bin` 的空闲列表,升序)。
  - Materialize:键级写穿 `DiskOp::Free`;复用之后 store 自己的 id 可能与工作集不同(写本来就按键,重开时重读 store 的 id)。
  - Stream:`DiskTriples::free_ids` 在写闸门下按 id 释放,先防御性地复查三元组索引——若仍被引用则拒绝(并使 store broken),而不是破坏它。
  - RocksDB:自动提交时在三元组批之后删除 `dict` CF 条目,事务中放进清日志的那一批;打开时日志重放留下的无引用词也被释放(`take_replayed`;每条语句和每次提交后都会接手重放交出的词)。`delete_terms` 失败则词保留在内存与 `dict` CF 中(孤儿,下个提交点报告)。
- **复用顺序**:每个空间**最低 id 优先**;释放最高 id 会降低该空间的高水位,并连带降低随之成为最高的空闲 id(trim cascade),所以计数根可以下降。`entity_num` / `literal_num` / `predicate_num` 现在恰好等于数据引用的词数。
- **pin**:内存字典的克隆是写时复制的,各自拥有自己的 id 版本,不需要 pin。out-of-core(Stream)字典的克隆共享磁盘字典,克隆期间 pin 住它(`Dictionary::pin()` / `DictPin`);Stream 库在有比候选更早的 pin 时推迟释放:候选带上 pin 时钟的戳,按戳排序,pin 消失后每个候选只检查一次,pin 期间不重复探查。磁盘 `/begin` 快照只在自己的 `DiskReadView` 里读字典(写闸门会先 drain 它),所以用 `clone_unpinned()`;`ConcurrentDb` 的 Stream 快照在任何读视图之外解析物化的 store,所以保持 pin——`ConcurrentDb` 包着 Stream 库时,它构造时的基准快照在它整个生命周期都 pin 着字典,不释放任何 id,丢弃时才运行这些释放并持久化。被 pin 推迟的释放只在内存中:重启(或丢弃一个普通 `Database`)时仍被 pin 住的释放会丢失,这些词成为孤儿,直到 `gcompact --reclaim`。
- **VS-tree**:被释放的实体留在树里(可靠超集,直到 `rebuild_index()`;没有自动重建阈值,`Database::vs_shrink_debt()` 只计数);一个实体得到第一条边(新词或被复用的 id)时获得精确签名,而不是并到旧条目上。
- **内存库的批量写**:`INSERT DATA`、`DELETE DATA`、`DELETE/INSERT … WHERE`、`LOAD`(及其撤销)、`ADD` / `COPY` / `MOVE`、`gadd` / `gsub`、`/api batchInsert` / `batchRemove` 和事务回滚,把一条语句(或一个导入批)的默认图三元组一次交给 store(`TripleStore::insert_many` / `remove_many`)。最低 id 优先复用会让新词拿到小 id,逐条插入会把长列表的大部分移动一次又一次;批量归并消除了这一点。结果、计数、错误语义、undo/redo、写穿和回收都与逐条路径相同;Stream 与 RocksDB 仍逐条写。
- **redo 日志**:`CLEAR` / `DROP` / `MOVE` / `COPY` 写 redo 记录(一个图 `C`,全部命名图 `N`),`replay_redo_log` 与 `restore_incremental` 会重放它们;RocksDB 上部分被拒的 clear 按实际删除的每个三元组 / 四元组记 `D` 记录。
- **工具与 API**:`gcompact <db> [--reclaim] [--enable-reclaim] [--check]`(可组合;取目录锁)——`--reclaim` 运行 `Database::reclaim_unused` 并持久化(快照库重新保存),`--enable-reclaim` 把 Phase 6 之前的 `kvstore.kv` 升为 V2(老二进制从此拒绝它),`--check` 运行 `Database::verify_integrity`。`gmonitor` 显示 `id_reclaim` 和每个空间的 `live / free / id_limit`(`Database::id_space_stats`)。`reclaim_unused` 返回 `Result<usize>`,在事务内和 V1 `kvstore.kv` 上被拒,释放了东西时推进 `mutation_count`。

**行为变化**:删除之后 id 会被复用,跨删除缓存 id 的应用不能再这样做;实体 / 字面量 / 谓词计数随删除下降;`DiskStore::entity_num` 计存活词(已分配减空闲)。

### 内存布局:写时复制的 `TripleStore`

`TripleStore` 的索引放在一个 `Arc` 包着的 `StoreData` 里(字段与之前相同、顺序相同):`clone()` 是 O(1) 共享;对共享 store 的第一次变更复制一次,之后原地修改;不改变任何东西的变更(插入已有的、删除不存在的、空的或全已存在的批量加载)从不复制。序列化委托给 `StoreData`,`store.bin` / `named.bin` 与 `Debug` 输出不变。降到容量四分之一以下的有序列表会归还多余内存。`TripleStore` 精确维护每个谓词的去重 subject / object 数(`pre2sub` / `pre2obj`),不序列化,读入时重算。

- **spare copy**(`keep_spare_copies`,`#[doc(hidden)]`,默认关,只有 `ConcurrentDb` 打开):store 必须复制共享索引时,把复制出的索引作为 spare 留着并记录之后的插入 / 删除日志;下次必须复制时,若没有克隆再持有那份 spare,就重放日志(O(变更))把它追上,而不是复制所有列表(O(三元组))。批量加载或日志长于 4096 + 三元组数/8 时丢弃 spare。代价是内存里多一份索引。
- **批量排序归并**(`insert_many` / `remove_many`):批次(任意顺序,可有重复)按每种索引顺序排序一次,一趟归并进每个被触及的列表(只移动该列表第一个新项之后的尾部,且只移动一次);删除同样一趟压缩。结果第 `i` 项等于逐条执行到第 `i` 个三元组时的返回值,最终 store(三元组、统计、剪枝、序列化字节)与逐条执行完全相同;不改变任何东西的批次不复制共享索引。

### RocksDB

- **布局**:CF `spo` / `pos` / `osp`(4 字节大端 id 的复合键)、`gspo`、`dict`(`tag ‖ be32(id) → key`,空洞即空闲 id)、`stats`(计数器:全局 `T`/`P`/`S`/`O`,逐谓词 `c|`/`u|`/`v|p`,逐 subject `s|s`、逐 object `o|o`,`x|p|s`、`y|p|o`)、`meta`(名字、`layout = "1"`)。事务的每个写在同一原子批里带 undo 记录,`commit()` 删除日志,`rollback()` 与崩溃后的打开重放它。
- **读**:有界的 raw iterator 扫描(`iterate_upper_bound` = 前缀的后继,键原地解码);iterator 状态错误或失败的点读经 `note_read_error` 上报(C2),语句失败而不是从截短的列表作答;读不出来的写(存在性检查、计数器)直接失败。C1 计数各是一次计数器点读(`count_po_by_s` = `s|s`、`count_ps_by_o` = `o|o`、`count_o_by_sp` = `x|p|s`、`count_s_by_po` = `y|p|o`、`count_so_by_p` = `c|p`),与默认值逐一相等;`c|`/`u|`/`v|` 在内存中有镜像(打开、每次日志重放后重新加载,只在写批成功后更新),`pred_card` 等不再碰 RocksDB;`iter_all` 按契约 `(p, s, o)` 顺序返回;`subs_by_p` / `objs_by_p` 读 `x|p` / `y|p` 键,无需排序。
- **打开**:不重建 VS-tree(打开时索引是 stale 的,`stats().index_valid == false`,`gshow` / `gmonitor` 显示 stale),查询不用 VS-tree,直到 `rebuild_index()`;`vstree_on_open`(或 `GSTORE_VSTREE_ON_OPEN=1`)恢复打开时构建。打开仍要把字典读进内存(D4)。`bench/results/phase6-final-lubm38.json`:univ 38 冷 / 热打开 738.9 / 678.0 ms(`bench/results/pre-phase6-lubm38.json`:60810.7 / 59389.4 ms)。
- **调优键**(只是选项,不改格式,有无 bloom 写出的文件都能打开):`[storage] rocksdb_block_cache_mb`(默认 256;`0` = RocksDB 默认的每 CF 32 MiB 缓存)——一个 LRU block cache 由所有 CF、以及本进程所有同容量的 store 共享;`[storage] rocksdb_bloom`(每键位数,默认 10,`on` / `off`)——给点查 CF(`spo`、`stats`、`gspo`)的整键 bloom filter。gserver 启动时与 `refreshconf` 时、gbuild 启动时安装它们(`GStoreConfig::install_storage_defaults`、`set_default_rocks_tuning`);环境变量 `GSTORE_ROCKS_BLOCK_CACHE_MB`(纯 MiB 数字;其它写法记一次警告后用 256)覆盖 block cache。`RocksStore::open_with(path, &RocksTuning)` 供库直接使用。
- **批量构建**:见[批量构建](#批量构建)的 RocksDB 目标(SST ingest,`rocksdb.new` 交换)。
- **目录锁**:`Database::open` / `open_rocksdb_with` 和逐条构建 `create_rocksdb`(`build_rocksdb_*`)为 RocksDB 布局也取 `<dir>/LOCK`(在 `rocksdb/` 旁边),库存活期间一直持有;RocksDB 自己仍取 `rocksdb/LOCK`。`build_rocksdb_*` 拒绝本进程正服务着的目录。
- **仍未做**:out-of-core 字典与原生快照(D4);`/begin` 与 `ConcurrentDb` 快照仍物化整个 store(`bench/results/phase6-final-lubm38.json` 的 `/api begin` 点:univ 38 中位数 2132.55 ms、RSS 增长 269.3 MiB);不持久化 VS-tree(D6,`vstree_on_open` 时在内存中构建)。

### 目录锁规则

`src/db/dirlock.rs`。一个数据库目录**同一时间只能被一个进程打开写入**。

- **`DirLock`**:`<dir>/LOCK` 上的操作系统咨询锁(Unix `flock`,Windows `LockFileEx`;持有者崩溃时由内核释放),所有布局都取,持有到 `Database` 被丢弃为止。另一进程打开时得到点名持有者 pid 的错误("… is locked by another process (pid N) …")。进程内按规范路径引用计数:同一进程再次打开共享这把锁(测试、服务器恢复后重开都依赖这一点),进程内两个写者的互斥由调用方负责(服务器用已载入库的注册表与按名字的生命周期预留)。不支持咨询锁的文件系统上跳过加锁,库照常打开。
- **`BuildGuard`**:进程内对"正在被批量构建重建的目录"的声明(`DirLock` 在进程内共享,挡不住同进程的第二个构建)。
- **`SideLock`**:目录旁边的锁文件 `<dir><suffix>`,给要创建目录、尚无 `LOCK` 可取的操作用:恢复到一个还不存在的库时持有 `<dir>.restore-lock`,从暂存之前到交换完成;最后一个持有者在持锁时删除该文件。
- **先锁后修复**:`Database::open` 在读任何东西之前、在补完中断的 store 交换(`rocksdb.new` / `.old` / `.tmp`)之前加锁;中断恢复(`<dir>.restore-*`)只在目录(或移开期间的 `<dir>.restore-old`)的锁下修复;另一进程持锁时失败且不碰任何东西。`Database::backup_dir` 只在源目录的锁下修复源里中断的提交或交换。
- **`Database::read_copy`** 不取锁、不碰目录,读一个快照库的分离副本(只限快照布局)。

| 工具 / 路径 | 锁什么、何时 | 目录被另一进程持有时 |
|---|---|---|
| `gquery`、`gadd`、`gsub`、`gexport`、`gshow <db>`、`gmonitor`、`gcompact`、`gconsole --local`、`gserver <dir>`(都经 `Database::open`) | `<dir>/LOCK`,打开时取,持有到库关闭;RocksDB 另有 `rocksdb/LOCK` | 打开失败(上面的错误),不碰目录 |
| `gserver --api`(`ApiServer`) | 每个已载入库持有其 `<dir>/LOCK`(所有布局,`hold_dir_lock`);替换、改名、删除库目录的操作(drop、rename、restore、重建的 `/build`)先 `DirLock::acquire_existing` | 载入失败;生命周期操作回答 `409` / `1005 … locked by another process` |
| `gbuild`(快照,含 `--force`) | 构建在内存中进行;`save` 覆盖一个已有数据库的目录(`--force`)时先取该目录的锁(新目录里没有可锁的库) | 保存失败,原目录不动 |
| `gbuild --disk` / `--rocksdb` / `--layout disk\|rocksdb`(批量) | `DirLock` + `BuildGuard`;安装前再查 `rocksdb/LOCK` | 构建失败,旧库不动;本进程打开着该目录时报 "is open in this process" |
| `gbuild --disk --incremental` | 取目录锁,也拒绝本进程打开着的目录 | 构建失败 |
| `gbackup` | 整个备份期间持有源库的锁(所有布局) | 拒绝("… is locked by another process (pid N)");正被服务的库请经服务器备份(`/api backup`、REST `/backup`、`gconsole` 的 `backup`) |
| `grestore`(`Database::restore_into`)、REST `/restore`、`/api restore` | 暂存之前锁目标(先锁、再修复上一次中断的恢复、再锁修复重建的目标),持有到交换完成;目标不存在时持有 `<dir>.restore-lock` | 拒绝(REST `409`、`/api` `1005 … locked by another process`),不暂存任何东西 |
| `gdrop` | `DirLock::acquire_existing`,然后删除目录 | 拒绝 |
| `gconsole`(连接服务器时) | 不碰目录,是 `/api` 的 HTTP 客户端 | — |

同一秒内对同一个库的多个备份(REST 与 `/api` 都算,目标路径相同)按目标路径的声明(`BackupClaim`)排成先后。

---

## 不变量

任何新后端只需实现 `TripleSource + MutableStore`(即 `StorageBackend`)+ 字典 seam。**VS-tree、优化器、图算法、SPARQL 引擎一行都不改**——这正是本设计要保护的核心资产。Phase 6 之后,一个新后端还要遵守:读面的顺序契约与 C1 计数的精确性([上文](#读面契约顺序契约与-c1-计数)),读错误经 `note_read_error` 上报且 `has_*` 在出错时回答 `true`(C2),词先于引用它的三元组持久化,以及只在数据确实变化时推进 `mutation_count`(C9)。
