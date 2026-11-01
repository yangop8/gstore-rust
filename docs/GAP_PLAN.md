# gStore 1.4 对标复核 + 补齐计划

> 基线:上游 `pkumod/gStore` master `bebe1ea`(2026-04-09,"support: delete {} insert {} where {}",即 1.4 稳定版之后);本仓 `a0071f0`。
> 方法:6 路只读审计逐模块对读两边源码(不采信 `CPP_PARITY.md` 的自评),再对关键结论**实测复现**(标 🔬)。
> 基线测试:`cargo test --workspace` 867 通过 / 3 失败(均为 `cluster::raft_tests` 网络用例,重跑时失败的组合会变,属时序不稳定)。

## 0. 总结论

`CPP_PARITY.md` 里"对 C++ 的功能差距已基本归零"的结论**不成立**。本仓功能覆盖面广,不少地方领先上游(见 §6),但与 gStore 1.4 相比有四类实质缺口:

1. **数据安全 / 正确性缺陷**:磁盘库更新静默丢失、RocksDB 库更新后无法重开、服务端从不落盘、UPDATE 注入、查询线程 panic 等。必须先修(§1)。
2. **线协议与使用方式不兼容**:上游 SDK、Workbench 和 `scripts/test` 测试脚本都走 `/api?operation=…` 协议,本仓一个请求都接不住;`gconsole`/`gserver` 的用法也和 1.4 不同(§2)。
3. **gStore 标志性能力只做到"同名"**:24 个图分析函数在上游语法下**无一能解析**,语义和输出也不同;推理引擎和 PFN 自定义函数没有对外入口(§3)。
4. **规模与运维**:没有真正的磁盘原生查询路径,导入不流式、不容错,没有 conf.ini、日志体系、打包和 CI(§4、§5)。

**进度(截至 Phase 4 与 Phase 5 合并)**:Phase 0–5 已完成并合入主线(各阶段状态见下文各节的"状态"段与 §8 表格,逐项变更见 `CHANGELOG.md`);Phase 6(规模与性能,§8 第 6 行)正在实施,其执行计划(12 个工作包、4 轮、退出闸门 G1–G7)见 `docs/PHASE6_PLAN.md`。

---

## 1. P0 · 数据安全与正确性(先止血)

| # | 问题 | 证据 | 修复方向 | 量级 |
|---|------|------|----------|------|
| 1.1 | 🔬 **`--disk` 库的写入静默丢失**:`gbuild --disk` 建库后,`gadd` 提示 "2 now total",但 `gquery` 只查到 1 行 | `load_disk` 把数据物化成 `Backend::Memory`(`src/db/mod.rs:1195`);`save` 写的是 `store.bin`(`:1113`);下次打开时 `is_disk` 仍读旧的 `kvstore.kv`(`:1159`) | 新增 `Backend::Disk`,写入走 `DiskStore::insert_triple/delete_triple`;在此之前先让磁盘库拒绝写入 | M |
| 1.2 | **RocksDB 库更新后无法重开** | `open_rocksdb` 校验 meta 中的三元组数(`src/db/mod.rs:499-504`),但 `gserver --rocksdb` 从不回写 `meta.bin`/`dict.bin`;`backup_dir` 不包含 `rocksdb/`,`save()` 也不写三元组,备份是空的 | 字典和计数改存 RocksDB 的 CF,或每次提交时原子回写;备份/恢复覆盖 RocksDB;CLI 统一用 `load_any` 打开 | M |
| 1.3 | **服务端不落盘** | 单库 `Server` 没有任何 `save` 调用;`ApiServer` 只在 build/checkpoint/restore 时保存;`/unload` 直接丢弃改动(`src/http_api.rs:882-893`);也不处理 SIGINT/SIGTERM | `/unload` 和优雅退出时 flush;支持 `check_point=on` 自动保存;单库模式在更新后保存 | S–M |
| 1.4 | **快照写入非原子** | `write_bincode` 原地截断重写,不 fsync(`src/db/mod.rs:2003`) | 先写临时文件,fsync 后 rename | S |
| 1.5 | **`/batchInsert` 存在 UPDATE 注入** | 用户提交的三元组直接拼进 `INSERT DATA { … }`(`src/http_api.rs:1187-1191`),只有 update 权限的用户可借此执行 `DROP ALL`/`LOAD` | 先按 N-Triples/Turtle 解析,再直接插入;`LOAD` 限制在白名单目录内 | S |
| 1.6 | 🔬 **内建函数参数个数错误时 panic**:`FILTER(STR())` 直接崩掉线程 | `src/query/engine.rs:3265` 直接取 `args[0]`;ABS、ISNUMERIC 等同理 | 解析期或调用期统一校验参数个数;HTTP worker 加 `catch_unwind` | S |
| 1.7 | **密码哈希不稳定** | 用 `DefaultHasher` 迭代(`src/http_users.rs:409`),标准库不保证其算法跨版本不变,升级工具链后 `users.db` 可能全部失效 | 改用已有的 SHA-256(`query/hash.rs`)做 PBKDF2 式迭代,并带版本号迁移 | S |
| 1.8 | **HTTP 事务没有隔离** | `/begin` 直接在共享库上开 undo 日志;同时到来的非事务 `/update`、`/batch*` 会混进这个事务,随它一起回滚;任何人都能 commit/rollback 别人的事务;`isolevel` 参数被忽略 | 事务期间对其它写操作排队或拒绝;校验事务归属;接入 `isolevel` | M |
| 1.9 | **实体 ID 无上限检查** | 第 2,000,000,000 个实体会拿到字面量区间的 ID(`src/dict/mod.rs:182`) | 分配 ID 时检查上限并报错 | S |
| 1.10 | **CI 缺失,测试不稳定** | 没有 `.github/`;Raft 网络用例在"刚选出 leader 就 propose"时存在竞态;`.DS_Store` 被提交进了仓库 | 加 GitHub Actions(fmt、clippy、test,覆盖 default/gzip/tls/rocksdb 四种 feature);propose 遇到 NotLeader 时重试;删除 `.DS_Store` | S |
| 1.11 | **文档自评失真** | `CPP_PARITY.md` 中至少 20 处表述与代码不符(如 HTTP ✅、图函数"已对齐"、流式磁盘查询 ✅、四级隔离、PFN 等价……) | 按本复核重写差距矩阵 | S |

**状态(Phase 0 已完成)**:1.1–1.10 全部修复——磁盘库与 RocksDB 库的写入逐次落到各自存储(WAL 提交点、RocksDB 列族 + 撤销日志),快照保存按组原子化,`Database::open` 自动识别布局;restore 先暂存校验再整体替换;服务端按 `check_point` 落盘、优雅退出时保存,`/batchInsert` 只应用解析后的三元组,`LOAD` 受 `LoadPolicy` 约束;字典 ID 空间有界,内建函数参数个数在解析期校验;密码改为带版本的 PBKDF2-HMAC-SHA256;HTTP 事务对非持有者提供开始时快照;GitHub Actions CI。1.11:`CPP_PARITY.md` 保留原矩阵,各阶段的更正与补齐以"更新"段落记在其中。

## 2. P1 · 上游兼容层(让 SDK / Workbench / 上游测试脚本直接可用)

### 2.1 HTTP `/api` 线协议(收益最大的单项)

上游 1.4 的路由只有 `/api`(GET/POST)、`POST /`、`/shutdown`、`/restart`、`/file/upload`、`/file/download`(`src/Main/grpc.cpp:826-900`)。所谓 "gRPC" 其实是基于 workflow 的 HTTP+JSON,不是 protobuf。

需要补的内容:
- **`operation=` 分发器**:挂在 `/api` 和 `POST /` 上(顺带为旧路径 `/grpc/api` 做别名),未知操作返回 1100;请求体支持 JSON、form 和 multipart。
- **按请求鉴权**:每个请求携带 `username`/`password`,并支持 `encryption=1`(客户端发送密码的 MD5)。这要求额外存一份可校验的 MD5,与 1.7 一起设计。
- **响应信封**:HTTP 状态恒为 200,业务码放在 `StatusCode`(0、1000–1008、1100、1101);查询结果在 `head`/`results` 之外合并 `StatusMsg`、`AnsNum`、`OutputLimit`、`ThreadId`、`QueryTime`;ASK 返回 `_askResult`;字面量类型标为 `typed-literal`,语言标签字段为 `lang`;`format` 支持 `json`/`file`/`n-triple`/`n-triple-file`;遵守 `max_output_size`。
- **REST 路由不动**:现有 `/sparql`、`/update` 等 SPARQL 1.1 Protocol 端点继续保留,作为 Rust 版的扩展。

### 2.2 补齐缺失或不兼容的操作

| 状态 | 操作 |
|------|------|
| ❌ 缺失 | `check`、`testConnect`、`getCoreVersion`、`ipmanage`、`stat`(CPU/内存/磁盘)、`shutdown`/`restart`、`rename`、`backuppath`、`checkOperationState`、`querylogdate`、`accesslogdate`、`schema`、`loadcsr`、`file/download`、`reasonManage`(type 1–8)、`funquery`/`funcudb`/`funreview`、`init`/`refreshconf` |
| ⚠️ 语义或字段不兼容 | `login`(应返回 `CoreVersion`/`licensetype`;失败时为 200+1001)、`show`(`ResponseBody` 里应有 creator/built_time/status)、`build`(应拒绝覆盖已有库,支持空库、多路径和 zip)、`drop`(默认 `is_backup` 只改名为 `.bak`;仅 root 可执行)、`backup`(带时间戳目录、`max_backups` 轮转、zip;返回 `backupfilepath`)、`restore`(路径可选,恢复前保留 `.bak`)、`export`(写服务端文件并返回 `filepath`)、`batchInsert`/`batchRemove`(读服务端 `file`/`dir`,返回 `success_num`/`failed_num`)、`upload`(先暂存到 upload 目录,再由 build 引用)、`monitor`(尊重 `db_name`,字段名对齐)、日志类接口(按日期分页的 JSON)、`usermanage`(`type` 1/2/3 分发)、`begin`(`isolevel`) |

**状态(Phase 4)**:`reasonManage`(type 1–8,见 §3.2)与 `funquery`/`funcudb`/`funreview`(见 §3.3)已补齐。

### 2.3 权限与系统库

- 权限改为**按库授权**:上游通过 `setpriv <usr> <db>` 授权,而本仓目前是全局权限。
- 新增库注册表(creator、built_time、status)。
- `/show`、`/monitor` 按用户过滤;`drop` 要求 root 或全部权限。

### 2.4 异步任务

- build、backup、restore、batchInsert、batchRemove 支持 `async=1`:立即返回 `opt_id`,配合 `checkOperationState` 查询进度,并支持 `callback` 回推结果。
- 现有 TaskManager 只管查询,需要扩展到上述操作。

### 2.5 配置与服务治理

- **`conf.ini` 加载**:默认端口 9000(本仓目前是 7000),另含 `thread_num`、`max_output_size`、上传大小与扩展名白名单、`ip_allow`/`ip_deny`、root 密码、`backup_path`、`max_backups`、`check_point`、日志相关配置。
- **访问控制**:IP 黑白名单;连续登录失败超过 4 次锁定该 IP。
- **连接管理**:线程池上限(目前每个连接一个线程,不设上限)、keep-alive、CORS/OPTIONS。

### 2.6 `gserver` 生命周期

- 支持 `-s`(启动,已在运行时拒绝)、`-b`(后台)、`-t`(停止)、`-r`(重启)、`-k`(强杀)、`-S`(状态),配合 pid 文件。
- 提供 `/shutdown` 接口,收到信号时优雅退出并落盘;守护进程自动拉起作为可选项。

### 2.7 一致性验收(本阶段的闸门)

把上游 `scripts/test/` 的套件(basic、update、transaction、api、parser_test 50 组、bfs_test 25 组)和 `data/bbug` 等数据集接进 CI,直接对 Rust 版 `gserver` 运行:
- **第一步**:先把期望结果移植成 Rust 集成测试,最快能形成 CI 闸门。
- **第二步**:兼容层完成后,原样运行上游 shell 脚本。

**状态(Phase 1 已完成)**:`src/gapi` 实现 `/api` 线协议(`GET|POST /api`、`/grpc/api`、HTTP 200 + `StatusCode` 信封、上游字段与消息)与 §2.2 的全部操作(`reasonManage`、PFN 三个操作见 Phase 4);按库授权与系统库、异步任务(`opt_id`、回调、取消)、`conf.ini`(`src/config.rs`)、`gserver` 生命周期(`-s/-b/-t/-r/-k/-S`、pid 文件、`deamon` 守护)。上游 `scripts/test` 脚本与 Python SDK 原样跑通(`scripts/conformance.sh`,CI 的 conformance 任务)。

## 3. P1 · gStore 标志性能力对齐

### 3.1 高级图函数(按"上游语义为准"重做)

🔬 实测:`shortestPathLen(<a>,<b>,true,{<p>})` 报 `unexpected token in expression: LBrace`。

**① 语法**
- 支持谓词集 `{<p1>,!<p2>}`(含取反)、命名参数(`alpha=` `maxIter=` `tol=` `increase=`)、布尔类型的 `directed` 参数,以及 `PFN.f({json})` 调用。

**② 图结构**
- 现状:`GraphView` 丢弃谓词、把字面量当成节点、同一对 (s,o) 的多条边被去重,而且每次查询都从全量三元组重建 CSR。
- 目标:改成上游那样的**按谓词、双向、不含字面量**的 CSR;每个库缓存一份,写入后标脏、按需重建;兼容 `csr=1` 加载参数和 `loadcsr` 接口。

**③ 逐个函数对齐签名、语义和输出**
- 输出:改为上游的聚合 JSON,例如 `{"paths":[{"src","dst","edges":[{"fromNode","toNode","predIRI"}],"nodes":[…]}]}`。
- 变量参数:取所有不同取值的笛卡尔积,而不是逐行计算。
- **缺失的函数**:shortestPath、cyclePath、kHopReachablePath、kHopEnumerate(u,v)、PR、shortestPathCount、clusterCoeff(本仓拼作 CLUSTERINGCOEFF)、confidenceDegree、maximumKplex、coreTruss。
- **同名但语义不同的函数**:SSSP、SSSPLen、shortestPaths、bfsCount、cycleBoolean、PPR(上游是 FORA 近似 + top-k)、JaccardSimilarity、degreeCorrelation、WCC、louvain、labelProp、closenessCentrality(公式不同)、SIMILARITY(上游是 Jaro 字符串相似度,不是图相似度)。
- **待决定**:现有 Rust 语义的这批函数(尤其是允许出现在 FILTER/BIND 中的用法)是改名放到 `rs:` 命名空间下保留,还是直接废弃(见 §7)。

**④ 测试**
- 以上游 `SPARQL_QUERY.md` 中的全部示例和 `scripts/test/bfs_test` 作为黄金用例。

**状态(Phase 2 已完成)**:上游 1.4 的 32 个高级图函数按上游语法、语义与 JSON 输出重做,运行在按库缓存的 CSR 上(`loadcsr` / `load csr=1` 预建);旧的 Rust 语义保留在 `rs:` 命名空间下(§7 第 2 项按推荐执行);`SPARQL_QUERY.md` 示例与 bfs_test 输出与上游一致。

### 3.2 推理引擎(`reasonManage`)

- **规则格式**:兼容上游 JSON(`rulename/type/logic/conditions[].patterns|filters/return{source,target,label,value}`),支持 UNION(`logic=0`)和 FILTER。
- **编译**:把规则编译成 SPARQL `INSERT…WHERE` / `DELETE WHERE`,可复用现有的 Modify AST。
- **持久化**:规则按库存到 `<db>/reason_rule_files/`。
- **生命周期**:状态流转(已编译/已执行/已失效),失效时**撤回推出的三元组**,已执行的规则禁止删除;推出的三元组使用 `Rule:` 前缀谓词以标明来源。
- **接入**:HTTP 接口加 gconsole 命令。
- **修 bug**:RDFS range 规则会生成以字面量为主语的三元组(`src/reason/mod.rs:182-190`)。

**状态(Phase 4 已完成)**:`src/reason/{rules,compile,store,manage}.rs` + `src/gapi/ops_reason.rs`。`reasonManage` type 1–8(新增/列表/编译/执行/失效/查看/删除/校验)沿用上游的参数、字段、状态字符串(`新建`/`已编译`/`已执行`/`已失效`/`已校验`)与消息;编译结果与上游 1.4 `Reason.cpp` 逐字节一致;规则按库存于 `<db>.db/reason_rule_files/`,随 unload/load、重启、`backup`/`restore` 保留,`drop` 时删除;仅 root(及 admin)可调用。执行/失效走数据库正常更新路径(查询缓存、CSR、持久化、日志、可取消),事务打开时拒绝。刻意偏离:按 `reason_rule_files/inferred/<rule>.nt` 账本**精确撤回**(上游 `delete where` 会误删用户断言或其他规则推出的同谓词三元组);规则逐项校验以杜绝 SPARQL 注入;已执行规则不可重新编译。RDFS range 字面量 bug 已修。测试:`tests/dt_reason_manage.rs`、`tests/dt_phase4_wiring.rs`。与 Phase 5 合并后:gconsole 的 reason 系列命令对真实服务端走完全生命周期(`tests/dt_gconsole.rs`);规则与账本随 zip 备份、`gbackup`/`grestore`、`drop` 的 `.bak` 一起保留,`export` 只导出数据(与上游一致);导入流水线按每次加载分域的空节点上推出的三元组可精确撤回(`tests/dt_merge4_wiring.rs`)。

### 3.3 PFN 自定义图函数

- **图访问接口**:定义 `GraphCtx` trait,对应上游 GAnalysis/CSRQueryHandler 提供的能力(点数、边数、按谓词取出/入邻居、id↔string)。
- **运行时机制(需决策)**:推荐 **WASM**(wasmtime,feature 门控,沙箱、性能好);备选 Rhai 脚本(纯 Rust,"编译"即语法检查);不建议走 dlopen(unsafe,且 Rust 无稳定 ABI)。
- **管理接口**:`funquery`/`funcudb`/`funreview`,函数按用户存储。
- **调用与输出**:`pfn.` 调用语法,结果形如 `{"results":…}`。
- **现有问题**:`ConcurrentDb` 的快照没有带上函数注册表(`src/concurrent.rs:221-228`)。

**状态(Phase 4 已完成,见 [`docs/PFN.md`](PFN.md))**:运行时选定 **WASM**——用 `wasmi` 解释器(feature `pfn-wasm`,默认关闭,默认构建不引入依赖)运行 WebAssembly 文本写的函数体,而非 wasmtime;沙箱只开放 gStore 宿主接口(`GraphCtx`:点/边/谓词计数、按谓词出入度与邻居、id↔string、参数、输出),带指令预算、内存上限、输出上限,并响应查询取消。`funquery`/`funcudb`/`funreview` 按用户存储与隔离,沿用上游消息与状态;`PFN.name({…})` 按上游 `pfnQuery`/`pfnQueryByVar` 求值,`/api` `query` 返回 `{"results": …}`。`ConcurrentDb` 快照、版本视图、事务与 HTTP 事务快照都带上调用者。`conf.ini` 新增 `[pfn]` 段(`refreshconf` 重新生效),`[system] pfn_base_path` 决定存放位置。刻意偏离见 `docs/PFN.md`(上游 C++ 函数体需改写为 WAT)。测试:`tests/dt_pfn.rs`、`tests/dt_phase4_wiring.rs`。与 Phase 5 合并后:gconsole 的 pfn 系列命令(`showpfn`/`addpfn`/`mdfpfn`/`delpfn`/`buildpfn`)与 PFN 查询结果打印对齐上游;函数按用户存于 `pfn_base_path`(打包与容器布局:`/var/lib/gstore/dbhome/pfn`),备份方法见 `docs/DEPLOY.md`;Docker 镜像默认开启 `pfn-wasm`,默认 `cargo build` 仍不含。§7 第 3 项据此定为 WASM。

### 3.4 Top-K 与查询提前终止

- DP-B Top-K:针对 `ORDER BY 线性表达式 + LIMIT k`、近无环的 BGP。
- `LIMIT` 不带 `ORDER BY` 时提前终止;`ASK` 拿到首个解即停。
- 目前三者都是全量求值后再排序、截断。

**状态(Phase 3 已完成)**:`src/query/topk.rs`。SELECT/CONSTRUCT 带 `LIMIT` 无 `ORDER BY`、`ASK`、`EXISTS` 流式求值并提前终止;`ORDER BY … LIMIT` 用有界堆(O(k) 内存);整数线性表达式 + 树形 BGP 走 DP-B(按统计信息估算,收益不足 2 倍时退回有界堆)。所有路径与全量求值结果逐行一致(含并列顺序),由 `tests/dt_topk.rs` 随机化验证;`TopKOptions` / `Database::set_topk_options` 可关闭。尚未覆盖:带环 BGP 的 DP-B(上游会切断一条环边)、非整数排序键。parser_test 50 组在优化开/关两种设置下均通过(`tests/dt_upstream_parser_suite.rs`)。

## 4. P2 · SPARQL 细节与导入导出

### 4.1 SPARQL 语法与语义

🔬 已实测复现:`IN`、`-5`、`true` 均报解析错误。

- **语法**
  - `IN`/`NOT IN`
  - 三元组、`VALUES`、`DATA` 中的带符号数值和 `true`/`false`
  - `CONTAINALL`/`CONTAINANY`
  - `ORDER BY STRLEN(?x)` 这种不加括号的内建函数排序键
  - `CONSTRUCT WHERE` 简写
  - `HAVING c1 c2`(多个条件共用一个 HAVING)
  - 写在整个查询末尾的 `VALUES`
  - 模式中的 `[ … ]` 和集合 `( … )`
  - `xsd:integer(?x)` 这类 IRI 函数和类型转换
  - `ADD`/`MOVE`/`COPY`
  - PN_LOCAL 中的转义和 `:`
- **语义**
  - 聚合外层再套内建函数(如 `ROUND(AVG(?x))`)目前静默返回未绑定
  - Modify 的 WHERE 看不到命名图;GRAPH 不能出现在模板里
  - `LOAD … INTO GRAPH` 忽略了目标图
  - `WITH`/`USING` 解析后被丢弃
  - 带类型字面量不做取值范围校验
- **查询缓存**:跳过含 NOW/RAND/UUID/SERVICE 的查询;按内存而非条数设上限。
- **与 W3C 冲突的上游行为**:整数除法上游截断取整,W3C 结果是 decimal。默认保持 W3C 行为,另加兼容开关。

**状态(Phase 3 已完成)**:上述语法与语义缺口全部补齐(`IN`/`NOT IN`、`[ … ]`/`( … )`、尾部 `VALUES`、`GROUP BY (expr AS ?v)`、`CONSTRUCT WHERE`、`WITH`/`USING`、`ADD`/`MOVE`/`COPY` 等),整数除法默认 W3C,`conf.ini` `[query]` 的 `integer_division=truncate` 切到上游行为;上游 parser_test 50 组通过。

### 4.2 RDF 导入导出

- **流式解析**:目前整个文件读成 `Vec<char>`(`src/parser/turtle.rs:41`),改为基于 BufRead 的流式解析加分批 flush。
- **容错导入**:跳过坏行,写 `parse_error.log`,统计 `failed_num`;目前遇到第一处错误就中止整次构建。
- **输入来源**:zip(feature 门控)、目录、URL。
- **按扩展名分派格式**:`--disk`/`--rocksdb` 目前一律按 Turtle 解析(`src/kvstore/store.rs:181`、`src/db/mod.rs:434`),需要修。
- **空节点**:匿名空节点的标签每个文件都从头计数,跨文件或多次 `gadd` 会误合并,改为按一次构建全局唯一。
- **Turtle 宽松度**:与上游对齐,接受 `ex:a/b`、`ex:a#b` 这类本地名。
- **命名图**:TriG/N-Quads 导入时保留命名图;导出支持命名图(N-Quads)和 zip。
- **RDF/XML**:支持 DTD 实体和 `parseType="Collection"`。

**状态(Phase 5 已完成)**:Turtle/TriG/N-Triples/N-Quads 流式读取,所有批量加载走同一条导入流水线(`src/parser/import.rs`),上游式容错导入(`parse_error.log`、`success_num`/`failed_num`);目录、URL、zip(feature `zip`)来源;按扩展名分派格式;空节点按每次加载分域;本地名宽松度对齐;TriG/N-Quads 保留命名图,导出支持 N-Quads 与 zip;RDF/XML 支持 DTD 实体与 `parseType="Collection"`;SPARQL `LOAD` 经同一流水线且原子。

## 5. P2 · CLI / gconsole / 运维

- **`gconsole` 重写为 gserver 的 HTTP 客户端**,对齐 1.4 的 43 条命令:
  - 库操作:create、use、showdbs、drop、backup、restore、export、batchinsert、batchremove、`sparql <file>`
  - 用户与权限:addusr、setpriv 等
  - 推理与 PFN:reason 系列、pfn 系列
  - 其他:version、pwd、clear
- **gconsole 交互体验**:`-u` 登录、历史记录和补全、`>`/`>>` 输出重定向、Ctrl-C 取消当前查询、`gStore[db]>` 提示符、耗时显示。目前 `quit;` 会被当成 SPARQL 而报错。
- **日志**:`log` 门面加文件日志,支持级别、按天滚动和保留期;查询日志改为 JSON,带 IP、结果数和耗时;访问日志补上 IP。
- **打包分发**:Dockerfile(暴露 9000 端口)、cargo-deb/rpm、systemd unit。
- **老工具兼容**:
  - 1.4 已不再发布这些工具;本仓保留,作为扩展。
  - 补 `-db`/`-f` 参数别名。
  - 修正 `grestore` 的参数顺序(目前与上游相反)。
  - `gshow` 改为列出全部库(上游语义)。
  - `gbuild` 支持建空库,并加防覆盖保护。

**状态(Phase 5 已完成)**:`gconsole` 重写为 `/api` 客户端,实现上游 43 条命令(reason 与 pfn 系列在与 Phase 4 合并后对真实服务端可用)及交互体验(feature `readline` 提供行编辑与补全);`src/logging.rs` 按上游 `log_mode` 属性文件配置诊断日志,按天滚动与保留,查询日志为 JSON 并带 IP、结果数与耗时;Dockerfile、systemd unit、deb/rpm 元数据与 `docs/DEPLOY.md`;老工具兼容项全部完成。

## 6. 不需要补 / 本仓已领先

**无需对齐**:以下各项上游 1.4 本身已不构建,或只是死代码。
- **socket API**:1.4 不再构建。
- **上游 Cluster**:leader/follower 复制,1.4 未编译进产物;官方文档也说明分布式版不开源。
- **自动备份调度器**:1.4 不构建。
- **protobuf gRPC**:上游并不是 protobuf。
- **5 种语言 SDK**:兼容层完成后,上游 SDK 可以直接使用,无需另写。

**本仓领先,保留**:
- 完整属性路径:上游只支持 `iri*`,README 也写明不支持属性路径。
- EXISTS、GRAPH、SERVICE、内联 VALUES:上游会静默丢弃。
- 精确数值塔、跨时区的 dateTime 比较。
- RDF/XML、TriG、JSON-LD、RDFa 解析器:上游源码里**根本没有**这几种解析器,README 里的说法没有代码支撑。
- TLS、SPARQL 1.1 Protocol 端点、XML/CSV/TSV 结果格式。
- Raft 选举和副本组:见 §7 第 4 项。

## 7. 需要你拍板的决策

1. **兼容目标**:是否以"与 gStore 1.4 `/api` 线协议兼容"为目标?推荐是。这是让 SDK、Workbench 和上游测试脚本直接可用的前提。
2. **图函数语义冲突**:以上游语义为准重做,还是把现有 Rust 语义的这批函数挪到 `rs:` 命名空间下保留?推荐以上游为准,另设兼容命名空间。
3. **PFN 运行时**:WASM、Rhai 二选一,或者暂不做?(已定:WASM,`wasmi` 解释器,feature `pfn-wasm`,见 §3.3。)
4. **Rust 集群**:接入 gserver(需要复制字典、Raft 状态持久化、分片读失败时报错而不是返回空结果),还是标为实验特性、冻结?推荐冻结,因为上游开源版本里并没有这项能力。
5. **依赖策略**:zip、日志、readline、wasm 都会引入依赖,建议一律走 feature 门控,保持默认构建精简。

## 8. 排期建议(粗估,1 人)

| 阶段 | 内容 | 估时 | 验收 | 状态 |
|------|------|------|------|------|
| 0 | §1 全部 P0 + CI + 修正文档 | 1.5–2 周 | 新增回归用例(磁盘库 gadd→gquery、RocksDB 更新后重开、batch 注入、`STR()`),CI 全绿 | 已完成 |
| 1 | §2 兼容层 + conf.ini + gserver 生命周期 + 移植上游测试 | 3–4 周 | 上游 `basic/update/transaction/api_test` 对 Rust 版 gserver 全部通过 | 已完成 |
| 2 | §3.1 图函数 + CSR | 3 周 | `SPARQL_QUERY.md` 中的全部示例和 bfs_test 输出与上游一致 | 已完成 |
| 3 | §4.1 SPARQL 细节 + §3.4 Top-K / 提前终止 | 2–3 周 | parser_test 50 组通过 | 已完成 |
| 4 | §3.2 推理 + §3.3 PFN | 3 周 | reasonManage 走完全生命周期,PFN 示例可跑 | 已完成(含 gconsole 对接,见 §3.2/§3.3) |
| 5 | §4.2 导入导出 + §5 gconsole/日志/打包 | 3 周 | README 中 `gconsole -u root` 的流程能照做;Docker 镜像可用 | 已完成 |
| 6 | 规模与性能(长期):`Backend::Disk` 流式查询直接穿透 `Database`、字典与 VS-tree 留在磁盘、持久化 StringIndex、批量建索引(B+ 树 bulk load / RocksDB SST ingest)、读写锁或廉价快照替代 `Mutex<Database>`、删除时即时回收 ID | 持续 | LUBM/WatDiv 的规模和延迟与上游同量级 | 待开始 |
