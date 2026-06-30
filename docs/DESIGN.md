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

## 8. 与原版的主要差异(有意为之)

1. 索引在内存,落盘用整体序列化(非B+树分页)。
2. 暂不实现VS-tree签名索引(它是查询剪枝优化,不影响正确性)。
3. 连接顺序用贪心(连通+选择度)启发式 + 哈希连接,非原版的代价优化器。
4. SPARQL支持主干子集:SELECT/ASK、BGP、UNION、FILTER、ORDER/LIMIT/OFFSET、DISTINCT、INSERT/DELETE DATA;尚缺OPTIONAL/MINUS/聚合/属性路径/子查询。
5. 暂不含Server/HTTP/GRPC/Cluster/事务MVCC/推理。

以上差异均不影响"导入RDF→存储→SPARQL查询"主干的正确性,且都在REFACTOR_BACKLOG.md中作为可演进项记录。

## 9. 测试策略

- **单元测试(UT)**:每个模块`#[cfg(test)]`内联测试,覆盖编码/解码、各访问模式、解析边界、FILTER语义、连接正确性。
- **数据测试(DT)**:`tests/`下端到端,用真实数据(`testdata/small`、`testdata/num`、`testdata/lubm`)构建数据库并断言SPARQL结果,含save/load往返、增删改后的结果一致性。LUBM(10万三元组)兼作规模与正确性回归。
