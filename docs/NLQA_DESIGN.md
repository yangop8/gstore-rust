# gNLQA 设计文档:LLM + gStore 的下一代自然语言知识图谱问答系统

> 状态:设计草案(2026-06-30)。基于对 `pkumod/gAnswer`(TKDE 2018,QAKB 子图匹配)的源码级逆向分析 + 现代 LLM-KBQA 调研 + 本仓库 gStore(Rust)现有能力。
> 目标读者:实现团队。本文给出调研结论、目标架构、详细设计与分阶段开发计划。

---

## 1. 目标与范围

构建 **gNLQA** —— 一个以 LLM 为前端、以本项目 gStore 图数据库为后端的自然语言问答系统。它要:

1. **覆盖并超越 gAnswer 的全部能力**(NL→SPARQL、实体/类型/谓语链接、多计划消歧、聚合、HTTP API)。
2. 提供**自然语言接口**:用户用自然语言提问,系统返回**有据可查(grounded/可引用三元组)**的答案。
3. 补齐 gAnswer 的硬伤:英文限定、精确匹配、top-k 截断漏答、≥6 节点失效、人工词典维护、无近似语义匹配。
4. 新增现代能力(详见 §5):多轮对话、答案解释与引用、混合检索(SPARQL + GraphRAG)、多语言、置信度与拒答、图分析型问题、自动评测。

**非目标**:不重新实现一个图数据库(用 gStore);不训练大模型(用 Claude API);第一阶段不做分布式问答(gStore 集群另线)。

---

## 2. gAnswer 能力对标表(逐项映射到新设计)

gAnswer 的核心是**数据驱动消歧**:为实体/谓语保留多个候选计划,在查询执行阶段用"fragment(实体/类型/谓语的邻域结构)"校验合法性来消歧。新设计保留这一思想,但用 **LLM 候选生成 + gStore 实时校验** 取代手工流水线。

| gAnswer 能力 | gAnswer 实现(源码) | gNLQA 新实现 | 提升 |
|---|---|---|---|
| 分词/POS/词形/NER | Stanford CoreNLP + 3-class NER | LLM 直接理解(无需独立 NLP 栈);可选轻量 NER 兜底 | 去掉 5+ 个重型 NLP 依赖;多语言 |
| 依存分析(双解析器) | Stanford + MaltParser 投票 | LLM 的隐式句法理解 + 结构化抽取(function calling) | 无需双解析器;更鲁棒 |
| 句型分类 | `recognizeSentenceType()` | LLM 意图分类(factoid/count/list/boolean/compare/path/analytics) | 类别更丰富、可扩展 |
| 实体识别(top-3 多计划) | `EntityRecognition.dfs()` DFS 冲突消解 | LLM 抽取 mention + **向量检索**候选实体(替代 Lucene TF-IDF + 编辑距离) | 近似语义匹配、召回更高 |
| 实体链接 | Lucene `entity_fragment_index` + 编辑距离重排 + 在线 DBpedia Lookup | 向量索引(实体 label/别名 embedding)+ gStore 精确/前缀回查;离线、无外部依赖 | 无需 DBpedia 在线服务 |
| 类型识别/链接 | Lucene `type_fragment_index` + 驼峰拆分 | 向量检索 + gStore `?x rdf:type` 校验 | 近似匹配 |
| 谓语/关系映射 | `ParaphraseDictionary`(PATTY 释义 + 手写,support/选择度打分) | **谓语 label/描述的 embedding 检索** + LLM 关系抽取;释义词典作为可选先验 | 摆脱人工词典维护;新谓语零成本 |
| 查询图构建 | `BuildQueryGraph` 在依存树上 BFS/DFS | LLM 产出**结构化查询意图**(实体/变量/关系/聚合)+ 程序化组装 | 复杂图(≥6 节点)不再失效 |
| 多计划 top-k 连接 | `SemanticItemMapping.topkJoin()` best-first DFS,top-t=10 | LLM 生成 **N 个候选 SPARQL** + gStore 逐一执行校验 + 重排 | 不再因 top-t 截断漏答 |
| fragment 合法性校验 | `isTripleCompatibleCanSwap()` 查 Entity/Type/RelationFragment | **gStore 原生**:实体邻域=`po_by_s/ps_by_o`,类型域=`so_by_p(rdf:type)`,谓语 domain/range 可查——无需预计算 fragment | 实时、与数据一致、零额外索引 |
| 主宾序消歧 | 投票 + 编辑距离 | gStore 试探执行(哪个方向有结果)+ LLM 先验 | 数据驱动、更准 |
| 隐式关系补全 | `ExtractImplicitRelation` 采样 top-100 实体找公共谓语 | gStore k-hop/谓语频率查询 + LLM 补全 | 精确、可解释 |
| 聚合识别 | `AggregationRecognition` 模式匹配 COUNT/MAX/FILTER/GROUP BY/ORDER BY | LLM 识别 + 直接生成 SPARQL 1.1 聚合(gStore 全支持) | 覆盖更广 |
| SPARQL 生成+排序 | `scoringAndRanking()` 复合分,`toStringForGStore2()` | LLM 生成 → **用 gStore 自带 SPARQL 解析器校验**(我们拥有 parser!)→ 执行排序 | 语法保证有效、可自动修复 |
| 执行 | `GstoreConnector` HTTP(tab 结果) | gStore HTTP `/sparql`(JSON Results)+ SERVICE 联邦 | 标准协议、内容协商 |
| HTTP 服务 | `GanswerHttp` Jetty :9999 `/gSolve` `/gInfo` | gNLQA HTTP + MCP;提供 **gAnswer 兼容子集** `/gSolve` | 向后兼容 + 现代接口 |

**关键洞察**:gAnswer 为效率把实体/类型邻域**离线物化为 fragment**(20GB 内存、~10h 构建)。**gStore 的六重索引本身就是这些 fragment**(`po_by_s`=实体出边、`ps_by_o`=入边、`so_by_p(rdf:type)`=类型成员、谓语 domain/range 可查),所以新系统**无需预构建 fragment**,校验即一次轻量 SPARQL,实时且与数据强一致。这是 gStore 后端带来的结构性简化。

---

## 3. 目标架构

```
                         ┌─────────────────────────────────────────────┐
   自然语言问题 ───────▶ │                gNLQA Orchestrator             │
   (多轮/多语言)        │                                              │
                         │  ① 意图理解 & 改写 (LLM)                     │
                         │      ↳ 对话上下文消解、语言归一、问题分类     │
                         │  ② 模式&实体链接 (Schema/Entity Linking)     │
                         │      ↳ 向量检索候选 ──┐                      │
                         │  ③ 计划选择路由 ──────┼──────────────┐       │
                         │     factoid/list/count│ analytics    │ open  │
                         │      ▼                 ▼              ▼       │
                         │  ④a Text-to-SPARQL  ④b 图算法调用  ④c GraphRAG│
                         │     (LLM 生成 N 候选)  (最短路/中心性) (子图取回)│
                         │      ▼                                        │
                         │  ⑤ SPARQL 校验+修复 (gStore parser, 自有!)   │
                         │  ⑥ 执行 & 多候选消歧 (择有效且高分者)        │
                         │  ⑦ 答案落地: 取回三元组 → 引用/解释 (LLM)    │
                         │  ⑧ 置信度评估 / 拒答                          │
                         └───────────────┬──────────────────────────────┘
                                         │ HTTP /sparql (JSON), SERVICE,
                                         │ 自定义函数, 图分析
                                         ▼
                         ┌─────────────────────────────────────────────┐
                         │        gStore (本项目, Rust)                  │
                         │  SPARQL 1.1 · 代价优化器 · RDFS/规则推理      │
                         │  图分析(BFS/PageRank/最短路/top-k子图)       │
                         │  pluggable 后端(内存/B+树/RocksDB) · 集群    │
                         └─────────────────────────────────────────────┘
   旁路索引(构建期一次):实体/谓语/类型 label 的向量索引 + 别名表
```

**数据流**:NL 问题 →(LLM 理解+改写)→(向量检索得到候选实体/谓语/类型 + 从 gStore 拉取其邻域作 schema 上下文)→(LLM 按 schema 生成 N 个候选 SPARQL,或路由到图算法/GraphRAG)→(用 gStore 的 SPARQL 解析器校验语法、必要时修复)→(在 gStore 执行,择"有结果且评分高"者实现数据驱动消歧)→(把绑定结果对应的三元组取回,LLM 生成带引用的自然语言答案 + 置信度)。

---

## 4. LLM 管线详解

### 4.1 问题理解与改写(①)
- 用 LLM 做意图分类 + 指代消解 + 多轮上下文合并 + 语言归一(多语言→统一语义)。
- 输出结构化 `QuestionIntent`(function calling 强约束 schema):`{lang, type: factoid|list|count|boolean|compare|path|analytics|open, mentions:[{text, kind: entity|type|literal}], relations:[{arg1, arg2, phrase}], aggregation:{op, by, order, limit}, target}`。
- 对应 gAnswer 的 QuestionParsing + 句型识别,但合并为一次 LLM 调用。

### 4.2 模式 & 实体/谓语/类型链接(②)— 取代 Lucene+释义词典
- **离线构建(一次)**:对 KG 中每个实体的 label/别名、每个谓语的 label/`rdfs:comment`、每个类型的 label,用 embedding 模型建**向量索引**(替代 gAnswer 的 Lucene TF-IDF + 编辑距离)。
- **在线链接**:对每个 mention 取向量 top-k 候选;再用 gStore 回查收紧:
  - 实体候选 → `po_by_s` 取其邻域(出/入谓语)作为 schema 上下文喂给 LLM;
  - 谓语候选 → 查 domain/range(`so_by_p` + `rdf:type`)做类型相容预判;
  - 类型候选 → `so_by_p(rdf:type)` 验证非空。
- 释义词典(若已有)可作为先验加权,但不再是必需依赖。

### 4.3 Text-to-SPARQL 生成(④a)— 自有 parser 是杀手锏
- LLM 拿到 `QuestionIntent` + 链接候选 + 其 gStore 邻域 schema,生成 **N 个候选 SPARQL**(few-shot + schema-grounded 提示)。
- **语法保证**:用 **gStore 自带的 SPARQL 解析器**(`src/parser/sparql`)对每个候选解析——这是相对外部 LLM-KBQA 的独有优势:我们拥有 parser,可即时判定有效性、抽取用到的谓语/类型、并对错误给出精确诊断。
- **受约束解码**(进阶):把 KG 的合法谓语/类型集合注入提示;进一步可做 grammar-guided decoding。

### 4.4 校验、执行与多候选消歧(⑤⑥)
- 校验失败的候选 → 进入**自我修复回路**:把 parser 报错或"空结果 + 邻域 schema"回灌 LLM,要求修正(最多 K 轮)。这复刻 gAnswer 的"执行期数据驱动消歧",但由 LLM 闭环。
- 多候选在 gStore 执行,按 [有结果, 结果规模合理, LLM 置信, 与 schema 相容] 复合排序,择优。对应 `topkJoin` + fragment 校验,但实时、无 top-t 截断。

### 4.5 答案落地:引用与解释(⑦)
- 取回答案绑定对应的**支撑三元组**(用 `CONSTRUCT` 或回查),LLM 据此生成自然语言答案,并**附引用**(实体/谓语 URI + 三元组),控制幻觉。
- 提供 "SPARQL 可见"(把生成的查询返回给用户,可解释、可审计)。

### 4.6 GraphRAG 兜底(④c)与图分析路由(④b)
- **图分析型问题**(最短路、中心性、社区、可达性)→ 路由到 gStore 图分析(已实现 `SHORTESTPATHLEN/KHOPREACHABLE/PageRank/top-k 子图`,且已接入 SPARQL 函数),而非硬塞 SPARQL。
- **SPARQL 难以表达/开放型问题** → 用 gStore 以链接实体为中心做 k-hop 子图取回(BFS/analytics),把子图三元组喂给 LLM 自由作答(带引用)。这是经典 KBQA 覆盖不到的长尾。

---

## 5. 超越 gAnswer 的新能力(逐项理由)

1. **多轮对话**:上下文指代消解 + 会话状态(gAnswer 单轮)。
2. **答案解释与引用**:每个答案附支撑三元组与生成的 SPARQL(可审计、抗幻觉)。
3. **混合检索**:SPARQL(精确)+ GraphRAG(长尾)+ 图分析(结构型)三路由,覆盖面远超纯子图匹配。
4. **多语言**:LLM 原生多语言,去掉英文限定与语言相关的拆分启发式。
5. **置信度与拒答**:无高分有效候选时显式"不确定/拒答",而非强行返回(gAnswer 总会给一个嵌入类型三元组的答案)。
6. **图分析型问题**:把 gStore 的图算法能力暴露为自然语言可问(gAnswer 无)。
7. **自动评测闭环**:内建 QALD-9/10、LC-QuAD 2.0、WebQSP、GrailQA 评测,指标 P/R/F1、执行准确率、引用正确率(gAnswer 仅离线评测)。
8. **零人工词典 / 自适应新 schema**:向量链接 + 自有 parser 校验,接入新数据集无需重建释义词典与 fragment(gAnswer 需 ~10h、20GB)。

---

## 6. 技术选型(给出明确建议)

| 维度 | 选型 | 理由 |
|---|---|---|
| LLM | **Claude(默认 Opus 4.8 复杂理解/生成,Sonnet 4.6 走量/低延迟路由)** | 最新最强 Claude;function calling 强约束输出;长上下文容纳 schema |
| Embedding | 可换:本地(candle/bge 类)或 Voyage/OpenAI embedding API | 实体/谓语向量链接;离线建索引 |
| 向量索引 | 第一阶段 brute-force/HNSW(单机);可复用 gStore 的 RocksDB 后端落盘 | KG 实体量可大,但单机够用;后续可换专用向量库 |
| 编排器语言 | **建议 Rust**(本仓库新增 crate/bin `gnlqa`)调 Claude API(HTTP)+ gStore(进程内库或 HTTP) | 单二进制部署、复用 gStore 的 parser/类型;无 Python 运行时;团队已是 Rust |
| gStore 集成 | **HTTP `/sparql`(JSON Results)** 为主;进程内直接 link `gstore` crate 为可选高性能路径;**SPARQL 校验直接调用 `gstore::parser::sparql`** | 解耦 + 复用自有 parser 是核心优势 |
| 对外接口 | HTTP REST + **MCP server**(把 gNLQA 暴露为 Claude/agent 的工具) | 既兼容 gAnswer,又面向 agent 生态 |
| 自定义函数/分析 | 复用 gStore 函数注册表 + 图分析 SPARQL 函数 | 图分析型问题直达后端 |

> 备注:编排器若团队更熟 Python,可第二选择 Python(LLM/embedding 生态更全),通过 gStore HTTP 集成;但会失去"直接调用 gStore parser 校验"的便利(可改用 HTTP 提交一个 `ASK`/`LIMIT 0` 探测语法)。**默认推荐 Rust**。

---

## 7. 对外 API

### 7.1 gNLQA 原生
- `POST /ask` `{question, lang?, session_id?, top_k?, explain?}` → `{answer, citations:[{triple, uri}], sparql, confidence, candidates?, latency_ms}`
- `POST /ask/stream`(SSE 流式答案)
- `GET /health` / `GET /info`(数据集、schema 统计)
- **MCP**:工具 `ask_kg(question)`、`run_sparql(query)`、`link_entity(text)`、`graph_analytics(op, args)`。

### 7.2 gAnswer 兼容子集
- `POST /gSolve` `{question, ...}` → gAnswer 风格 JSON(SPARQL + 答案列表),便于现有 gAnswer 客户端平滑迁移。
- `GET /gInfo` → 元数据。

---

## 8. 风险与对策

| 风险 | 对策 |
|---|---|
| LLM 幻觉(编造实体/谓语/答案) | 强制 schema-grounded 生成 + 自有 parser 校验 + gStore 执行验证 + 答案必须有支撑三元组引用,否则拒答 |
| 生成 SPARQL 无效/语义错 | parser 即时校验 + 自我修复回路(报错回灌)+ 多候选择优 |
| 实体/谓语链接召回不足 | 向量检索 + 别名表 + 释义先验 + gStore 模糊回查(StringIndex 子串/前缀,待接线) |
| 延迟/成本(多次 LLM 调用) | 路由分级(简单问题走 Sonnet 单次)、候选数自适应、SPARQL/答案缓存(gStore QueryCache + 编排器缓存)、schema 上下文裁剪 |
| 大 KG 向量索引规模 | 分片/HNSW;仅索引有 label 的实体;落盘 RocksDB |
| 多跳/复杂图查询正确率 | gStore 优化器 + 自我修复;评测驱动迭代 |
| 评测可比性 | 复用标准基准(QALD/LC-QuAD/WebQSP/GrailQA)与官方指标 |

---

## 9. 分阶段开发计划

> 每阶段都有可运行交付物 + 评测口径。建议在本仓库新增 `gnlqa` crate/bin,gStore 作为库或 HTTP 服务。

### Phase 0 — 地基(1–2 周)
- 新增 `gnlqa` 骨架(Rust):配置、gStore 客户端(HTTP `/sparql` + 进程内库二选一)、Claude API 客户端、日志/计时(对应 gAnswer `QueryLogger`)。
- 装一个小型 KG(LUBM 或 DBpedia 子集)到 gStore。
- 交付:`/ask` 端到端打通最简直通(LLM 直接生成 SPARQL→执行→原始结果)。评测:跑通 5 个样例问。

### Phase 1 — gAnswer 功能对标 MVP(3–4 周)
- 实体/谓语/类型**向量链接**(离线建索引 + 在线检索)+ gStore 邻域回查作 schema。
- **Text-to-SPARQL**:LLM 生成 N 候选 → **gStore parser 校验** → 执行 → 多候选消歧排序。
- 聚合(COUNT/GROUP BY/ORDER BY/LIMIT)、ASK/SELECT、主宾序消歧。
- `/gSolve` 兼容端点。
- 交付:覆盖 gAnswer 主路径。评测:**QALD-9** 子集,报告 P/R/F1 对比 gAnswer。

### Phase 2 — 落地与鲁棒(3–4 周)
- 自我修复回路(报错/空结果回灌)、答案**引用与解释**、SPARQL 可见。
- 置信度与拒答;缓存与延迟优化;Sonnet/Opus 分级路由。
- 交付:生产级单轮问答。评测:QALD-9/10 全量 + 引用正确率。

### Phase 3 — 超越 gAnswer(4–6 周)
- 多轮对话;GraphRAG 子图取回兜底;**图分析型问题**路由(最短路/中心性/社区,直达 gStore 分析)。
- 多语言;MCP server 暴露。
- 交付:混合检索问答 + agent 工具。评测:LC-QuAD 2.0 / WebQSP / GrailQA + 自建图分析问集。

### Phase 4 — 规模化与运维(持续)
- 向量索引分片/落盘;接 gStore 集群(Raft)做 HA;监控/日志/审计端点;A/B 与回归评测流水线。

---

## 10. 与 gStore 现有能力的契合点(为什么是天作之合)

- **自有 SPARQL parser** → LLM 生成的 SPARQL 可即时校验/修复(外部系统做不到)。
- **六重索引 = 天然 fragment** → 实体/类型/谓语邻域实时可查,免去 gAnswer 的 20GB/10h 预构建。
- **图分析已接入 SPARQL 函数** → 图结构型问题直达后端。
- **SERVICE 联邦** → 可跨多个 KG / 远程端点问答。
- **自定义函数注册表** → 可注入领域特定的打分/链接函数。
- **pluggable 后端(RocksDB)** → 向量索引与 KG 可共用持久化层。
- **标准 HTTP `/sparql` + 内容协商** → 编排器与后端干净解耦。

---

## 附:本设计的来源与待补
- gAnswer 架构:对 `pkumod/gAnswer` 的源码级逆向(qa/paradict/fgmt/lcn/nlp/rdf/application/jgsc 四组模块,见 §2)。
- LLM-KBQA 调研维度:Text-to-SPARQL(schema linking / 受约束解码 / 自修复)、GraphRAG(向量+图遍历混合)、agentic KGQA + 评测基准(QALD/LC-QuAD/WebQSP/GrailQA)。
- §11 已补充联网调研(2025–2026)得到的最新论文与基准 SOTA。

---

## 11. 参考文献与基准 SOTA(联网调研,2025–2026)

### 11.1 KGQA 基准当前 SOTA(用于 §5 评测口径)

| 基准 | KG | 难度 | 当前 SOTA(系统 / 分数) | 备注 |
|---|---|---|---|---|
| **QALD-9** | DBpedia | 中(多语言,408 训练/150 测试) | 早期强基线 ~0.67 F1;LLM 方法持续刷新 | 经典多语言基准 |
| **QALD-10** | Wikidata | 中(从 DBpedia 迁到 Wikidata) | COT-SPARQL(D'Abramo 2025,CoT + 实体/关系提示) | 现行主流 LLM 基准 |
| **LC-QuAD 2.0** | Wikidata | 较易(≤2 跳) | DeSQ ~95%/70%(Raw/Reformulated 执行 F1);Diallo 2024 ~94% | 执行式 F1 已很高 |
| **WebQSP** | Freebase | 较易(≤2 跳) | GraphWalker(agentic,2026);ChatKBQA / TFS-KBQA(微调 LLM 产逻辑形式) | |
| **GrailQA** | Freebase | 难(i.i.d./组合/零样本泛化) | ~79%(SG-KBQA,Gao 2025,schema-guided + LLaMA-3.1-8B);前 SOTA Pangu | 主力泛化基准 |

> 建议:gNLQA 评测先打 **QALD-9/10**(与 gAnswer 同源,DBpedia/Wikidata,可直接对比),再上 **LC-QuAD 2.0 / WebQSP / GrailQA**(泛化)。指标:执行式 Precision/Recall/F1 + 答案引用正确率。

### 11.2 与本设计相关的关键方法(印证 §4 管线)

- **Text-to-SPARQL / 受约束生成**:in-context learning(检索 top-k 相似 NL-SPARQL 示例注入提示,LLM 生成多候选后由引擎全部执行)是当前主流——与本设计 §4.3「N 候选 + gStore 执行择优」一致。schema-guided 生成(SG-KBQA,[arXiv:2502.12737](https://arxiv.org/pdf/2502.12737))把 schema 上下文喂给较小 LLM 即达 GrailQA SOTA,印证 §4.2「gStore 邻域作 schema 上下文」的有效性。
- **agentic KGQA(ReAct + 自修复)**:ARUQULA(ReAct + KG 探索的 Text2SPARQL)、GraphWalker([arXiv:2603.28533](https://arxiv.org/html/2603.28533v2))、KnowCoder-A1([arXiv:2510.25101](https://arxiv.org/pdf/2510.25101))、ODA 观察驱动 agent([arXiv:2404.07677](https://arxiv.org/pdf/2404.07677))——印证 §4.4「执行反馈 + 自修复回路」。
- **GraphRAG**:Microsoft [Project GraphRAG](https://www.microsoft.com/en-us/research/project/graphrag/)(2024,实体抽取 + Leiden 社区聚类 + 分层摘要;LazyGraphRAG)与综述印证 §4.6「子图取回兜底」;注意 KG-RAG 数据集存在质量陷阱([arXiv:2505.23495](https://arxiv.org/pdf/2505.23495)),评测需谨慎。
- **综述**:*Large Language Models Meet Knowledge Graphs for Question Answering: Synthesis and Opportunities*([arXiv:2505.20099](https://arxiv.org/pdf/2505.20099))——LLM×KG QA 全景,可作技术选型背书。
- **Text-to-SPARQL 专题**:[SPARQL-LLM 实时生成](https://arxiv.org/pdf/2512.14277)、[Investigating LLMs for Text-to-SPARQL](https://aclanthology.org/2025.knowledgenlp-1.5.pdf)、[Text-to-SPARQL 综述/topic](https://www.emergentmind.com/topics/text-to-sparql)。

### 11.3 对本设计的校准结论
1. 主流已收敛到「**schema-grounded 生成 + 多候选 + 执行校验 + 自修复**」——正是本设计 §4 的骨架;**我们自有 SPARQL parser** 让"执行前语法校验/修复"比依赖端点 `ASK` 探测更高效,是差异化优势。
2. LC-QuAD/WebQSP 这类 ≤2 跳基准执行 F1 已达 90%+,gNLQA 的 MVP 应能较快接近;GrailQA 的零样本/组合泛化是难点,需 schema 检索 + agentic 自修复重点投入。
3. GraphRAG 适合长尾/开放问题兜底,但不替代精确 SPARQL;本设计的三路由(SPARQL / 图分析 / GraphRAG)与社区共识一致。

> Sources: 见上各 arXiv/官方链接;另:[QALD-10 论文](https://www.researchgate.net/publication/376009186)、[Beyond Seen Data (schema-guided)](https://arxiv.org/pdf/2502.12737)、[DeSQ](https://arxiv.org/html/2606.00203)、[KGQA benchmarks 概览](https://www.emergentmind.com/topics/knowledge-graph-question-answering-kgqa-benchmarks)。

## 12. 实现补充(as-built,在 §3–§7 骨架之上落地的能力)

以下几项是实现阶段在原设计之上补齐的能力,均已落在 `gnlqa` crate 与 CLI 中(见 `gnlqa/README.md`)。

### 12.1 问答模式(Mode)——把路由做成用户可控的隐私/信任开关
§4 的三路由(结构化 SPARQL / 图分析 / GraphRAG)默认由意图分类自动选择;实现中把它进一步暴露为**用户可选的模式**(`GNLQA_MODE` 或 chat 里 `/mode`),因为"走哪条路"本质是一个**数据是否出域**的决定,应交给用户:

| 模式 | 允许的路径 | 语义 |
|------|-----------|------|
| `auto`(默认) | 结构化 → 不行再 GraphRAG;**绝不**纯 LLM | 安全默认 |
| `structured` | 仅结构化 SPARQL / 图分析,**数据绝不出域**,否则拒答 | 强隐私 |
| `graphrag` | 取私域子图 → 交 LLM 作答 | 中间档 |
| `open` | 纯 LLM 世界知识,不碰 KB(`answer_direct`) | 无私域数据 |

**红线**:一条 in-domain 问题「SPARQL 生成失败」≠「可用 LLM 世界知识作答」——否则会在私域问题上产生自信的幻觉。因此 `auto` 失败只回退 GraphRAG 或拒答,**只有显式 `open` 才会纯 LLM 直答**。

### 12.2 Provenance(答案来源)——数据出域边界,而不只是署名
每个答案带一个 `Provenance` 标签,三档正好对齐"隐私 + 可信度":

- **`gStore`**:结构化 SPARQL / 图分析,本地计算,**底层数据从未离开本机**(只有问题 + schema 发给 LLM)。
- **`gStore+LLM (GraphRAG)`**:答案由 LLM 从 gStore 取回的**私域三元组**综合而来(这些三元组发给了 LLM)。
- **`LLM`**:LLM 用自身世界知识作答,**不涉及任何 KB 数据**。

CLI 打 `[provided by …]`,HTTP `/ask` 返回 `provenance` 字段,MCP `ask_kg` 附带来源。这让"用本地存储保护私域数据"的初衷有了**可观测、可审计**的边界。

### 12.3 可插拔 LLM 后端(§6 技术选型的落地)
`LlmClient` trait 之下现有两个实现:`AnthropicClient`(Claude Messages API)与 `OpenAiClient`(OpenAI 兼容 `/chat/completions`,适配 OpenAI / **DeepSeek** / Together / 本地 vLLM 等)。经 `GNLQA_LLM_PROVIDER` 选择;推荐/默认后端为 DeepSeek(OpenAI 兼容)。注意点:推理型模型(如 `deepseek-v4-pro`)会先花 completion token 做隐藏推理再产出可见答案,故 `OpenAiClient` 对 `max_tokens` 设了下限(`MIN_MAX_TOKENS`)以免答案为空。

### 12.4 配置文件与容错
- **配置文件** `gnlqa.conf`(`KEY=VALUE`,路径可用 `GNLQA_CONFIG` 覆盖;env 优先于文件)让配置一次落盘、无需每次 export。仓库只提交假的 `gnlqa.conf.example`,真实 `gnlqa.conf`(含 key)被 gitignore。
- **异常兜底**:`GNLQA_LLM_PROVIDER` 规范化(trim/小写/别名),未知值**告警并回退** anthropic;`GNLQA_MODE` 未知值告警回退 auto;空配置值按缺失处理→用默认;不可解析的数值告警回退默认;缺 key 启动告警 + 调用时优雅 `Err`;KB 启动不可达则降级为无 linker;推理模型空 content 视为失败以便降级。

### 12.5 进度日志
交互式(CLI 单问 / chat)默认开启阶段日志(stderr),明确当前在"问模型"还是"查 gStore"(`understanding…(model)` / `gathering schema…(gStore)` / `generating SPARQL…(model)` / `querying gStore…` …),给用户一个显式的等待目标;serve/mcp/eval 默认关闭。

> 落地映射见 `gnlqa/src/{solve,pipeline,config,llm/openai,session,mcp,http_server,main}.rs`;评测 harness 见 `gnlqa/src/eval.rs`(QALD/LC-QuAD + P/R/F1)。
