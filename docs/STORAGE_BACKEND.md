# 可插拔存储后端(Pluggable Storage Backend)

## 动机

gStore的核心价值是 **VS-tree签名索引 + 代价优化器 + 图算法集成**,而非自研的字节级存储引擎。因此存储后端应当**可配置/可插拔**:同一套查询栈既能跑纯内存,也能跑磁盘B+树,未来还能接 RocksDB(MyRocks)、甚至 MySQL。

## 现状:读面已是干净的 seam

`src/store/mod.rs` 的 `TripleSource` trait 是只读查询面(6种三元组访问模式 + 优化器需要的统计:`pred_card`/`pred_distinct_subj`/`pred_distinct_obj` 等),**返回 owned `Vec`**(对外部KV后端友好,不强求借用内部内存)。已有 4 个实现:

- `TripleStore`(纯内存,三套有序邻接图 s2po/o2ps/p2so)
- `DiskStore`(磁盘B+树KVstore,流式查询 + out-of-core 字典 `disk_dict.rs`)
- `ShardedStore`(进程内 hash 分片,scatter-gather)
- `NetworkShardedStore`(std-TCP RPC 跨节点分片)

整个 `Evaluator<S: TripleSource>`、planner、optimizer、candidates、analytics(`GraphView` 由 `TripleSource` 构建 CSR)都坐在这个 seam 之上,**与存储实现解耦**。

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

## Phase 2(下一步):Database 接入 + 运行期可配置

把 `Database` 对存储的硬编码依赖换成 seam。两条可选实现路线:

| 路线 | 做法 | 取舍 |
|------|------|------|
| **A. 配置枚举(推荐)** | `enum Backend { Memory(TripleStore), Disk(DiskStore), #[cfg(feature="rocksdb")] Rocks(RocksStore) }`,对枚举实现 `TripleSource`+`MutableStore`(按 variant 分派);`Database` 持有 `Backend`,由 `db.conf`/构造参数选择 `backend=memory\|bptree\|rocksdb` | 运行期可配(正是"可配置后端"诉求);静态分派+分支(无 vtable);改动集中、无泛型扩散 |
| B. 泛型化 `Database<B>` | `Database<B: StorageBackend>` | 零分派开销;但 `Database` 是 facade,全调用点(server/bins/tests)都要带类型参数,扩散大 |
| C. `Box<dyn StorageBackend>` | 动态分派 | 最灵活;热路径每次访问模式调用走 vtable,有(轻微)代价 |

字典侧:已有 `Dictionary::from_backing(Arc<dyn DiskTermSource>)` 抽象其磁盘后端(out-of-core 用),RocksDB 后端可复用此 seam 把 str↔id 放进 RocksDB CF。

## Phase 3:RocksDB 后端(feature 门控)

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

**Phase 3(RocksDB 后端,`--features rocksdb`):** `src/backend/rocks.rs` 的 `RocksStore` 实现 `StorageBackend`:

- 3 个排列 column family——`spo`(键 `s‖p‖o`)、`pos`(键 `p‖o‖s`)、`osp`(键 `o‖s‖p`),id 用 4 字节**大端** u32,故字节序==数值序,前缀 range scan(`iterator_cf`)即可回答 6 种访问模式,返回已排序结果以与 `TripleStore` 完全对齐。
- `stats` CF 用**维护式计数器**(引用计数,插入/删除时 O(1) 维护;计数归零即删键)给出**精确**优化器统计:`triple_count`/`distinct_subjects`/`distinct_objects`/`num_predicates`(4 个全局值另缓存于内存)、`pred_card`/`pred_distinct_subj`/`pred_distinct_obj`(逐谓词),避免热路径上的全表扫描。
- `RocksStore::open(path)` 持久化并可重开;`Database::build_rocksdb_from_str` / `open_rocksdb` / `is_rocksdb` 提供 Rocks 后端的库级 API(三元组存于 `dir/rocksdb`,字典/元数据 bincode 快照于同目录)。
- 测试 `tests/dt_rocksdb.rs`(feature 门控):泛型 `fn <B: StorageBackend>` 对 `TripleStore` 与 `RocksStore` 跑同一指纹证明结果一致;close→reopen 持久化;以及经 `Evaluator` 跑真实 SPARQL 的行数对拍。

仍未做(backlog):Rocks 后端的字典 out-of-core 化(复用 `DiskTermSource` seam)、merge-operator 式计数、按谓词的 prefix-extractor 调优、Rocks 后端的事务/快照接线。

## 不变量

任何新后端只需实现 `TripleSource + MutableStore`(即 `StorageBackend`)+ 字典 seam。**VS-tree、优化器、图算法、SPARQL 引擎一行都不改**——这正是本设计要保护的核心资产。
