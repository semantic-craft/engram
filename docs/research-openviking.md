# OpenViking 研究：能力、架构与 Engram 引入边界

> 调研日期：2026-09-02
>
> 上游基线：[`volcengine/OpenViking@c486088`](https://github.com/volcengine/OpenViking/commit/c486088ee7c43c46ba0e6d494e5622086a499aab)（main，2026-09-01）
>
> 最新正式版：[`v0.4.17.1`](https://github.com/volcengine/OpenViking/releases/tag/v0.4.17.1)（2026-08-31）
>
> 资料边界：只采用 OpenViking 官方仓库、仓库内文档与源码；所有源码链接固定到上述 commit。

## 结论先行

OpenViking 不是一个普通的“向量记忆库”，而是一套面向 Agent 的**上下文数据平面**：它把 Resource、Memory、Skill 放进统一的 `viking://` 虚拟文件系统，用 L0/L1/L2 三层表示控制加载深度，通过 Session 捕获交互并异步提取长期记忆，再通过 HTTP、MCP、CLI、WebDAV 和各 Agent 的 lifecycle hook 将这一能力接入运行时。[官方 README 对其定位、文件系统范式和三层加载的说明](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/README_CN.md#L32-L91)。

对 Engram 最有价值的不是照搬 Python/RAGFS/向量库实现，而是吸收四个产品级合同：

1. **上下文可寻址、可浏览**：检索结果不是脱离位置的 chunk，而是稳定 URI、父子目录和可继续钻取的上下文对象。
2. **先广后深、预算内加载**：先让每个候选获得低成本表示，再把剩余 token 花在最相关项的 overview/full，而不是把第一条完整内容塞满窗口。[源码中的 breadth-first-then-depth 预算策略](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/retrieve/context_assembler/budget.py#L1-L8)。
3. **Session 是记忆形成的证据单元**：消息、工具结果、已使用上下文、归档和 memory diff 保持可追溯关系，而不是只保留脱离来源的总结。
4. **各 Agent 共享服务端语义，适配器只做生命周期翻译**：核心召回、预算、去重和提交逻辑应统一，宿主插件只映射事件与身份。[自定义 Agent 的三种官方接入路径](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/16-capability-reference.md#L619-L665)。

但必须明确：OpenViking 当前提供的是**跨 Agent 共享记忆**，不是完整的**跨 Agent / 跨任务交接协议**。它没有面向工作交接的 `handoff` 实体、定向投递、claim/lease、ack、完成状态、开放问题或工件清单；其 `task_id` 只跟踪资源导入、会话提交、索引维护、快照恢复等**服务端后台处理作业**，不是用户交给 Agent 的工作任务。[Task API 的官方定义](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/api/17-tasks.md#L1-L24)。不同客户端通常创建独立 session，主要在各自 commit 并完成抽取后，通过共同记忆空间共享；subagent 是否独立建 session、并入父 session 或完全不接入，又由具体 harness 决定。[官方跨 harness 的 session ID 表](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/16-capability-reference.md#L159-L172)及[subagent 对照表](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/16-capability-reference.md#L355-L365)。Engram 已有 typed handoff 和 workspace/project scope，应在现有基础上扩展，而不是把 OpenViking 的后台 task 模型误当成交接模型。

许可也构成硬边界：OpenViking 主项目为 AGPL-3.0，Engram 为 MIT。[OpenViking 许可边界](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/README_CN.md#L273-L280)、[Engram workspace 许可证声明](https://github.com/semantic-craft/engram/blob/6a9e67837fccdac66e90c0efefefb5a2985165b5/Cargo.toml#L18-L23)。为保持 Engram 的 MIT 代码与分发边界，本文只建议以公开文档为规格、用 Engram 自己的域模型和测试做 **clean-room 独立实现**；不复制、不修改、不链接 OpenViking 主项目代码。是否构成衍生作品仍应由正式法律审查判断。

## 1. 项目定位、能做什么、不能做什么

### 1.1 项目定位

OpenViking 自称“AI 智能体的上下文数据库”，核心对象是三类上下文：

| 类型 | OpenViking 的定义 | 主要用途 |
|---|---|---|
| Resource | 用户添加、相对静态的外部知识 | 文档、代码仓、网页、论文、规范 |
| Memory | Agent 从交互与任务中动态提取的长期认知 | profile、preference、entity、event、case、trajectory、experience |
| Skill | 可声明的 Agent 能力配置 | `SKILL.md`、脚本与可调用工作流 |

这三类的官方生命周期差异见[上下文类型表](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/02-context-types.md#L1-L12)，统一索引模型的枚举和 L0/L1/L2 level 则落实在 [`ContextType` / `ContextLevel` 源码](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/core/context.py#L16-L40)。

它同时提供一个可选的 VikingBot Agent 框架和 `ov compile` 上下文编译能力，但这不是核心数据库本身。`ov compile` 依赖启用 Bot 的服务，由独立 Agent Loop 按 Skill 读取来源、组织并写出知识产物。[官方编译流程](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/context-compilation/01-overview.md#L1-L17)。

### 1.2 核心能力全景

| 能力面 | 当前能做到什么 | 一手依据 |
|---|---|---|
| 统一寻址与浏览 | 用 `viking://` 表达共享资源、用户记忆/资源/技能、peer 子空间和 session；提供 `ls/tree/read/write/edit/grep/glob/rm/mv` 等文件系统操作 | [URI 与目录布局](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/04-viking-uri.md#L84-L164)、[MCP 工具源码](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/server/mcp_endpoint.py#L235-L305) |
| 分层上下文 | 每个目录可有 L0 `.abstract.md`、L1 `.overview.md` 和 L2 原始内容；L0/L1 是目录级 sidecar，不是每个文件一份副本 | [层级定义与限制](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/03-context-layers.md#L1-L17) |
| 资源摄取 | 支持 Markdown、文本、PDF、HTML、Word/EPUB、Excel/PowerPoint、代码文件/仓库、飞书/Lark、单页/递归网页、sitemap、RSS/Atom；图片是实验特性，视频/音频仍在规划；可按周期 watch 刷新 | [资源类型与成熟度标记](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/api/02-resources.md#L7-L61)、[Watch 控制面](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/api/15-watches.md#L1-L25) |
| 语义处理 | Parser 无 LLM 地解析和建树；TreeBuilder 入库；后台 SemanticQueue 自底向上生成 L0/L1 并向量化 | [提取架构](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/06-extraction.md#L1-L14)、[单目录语义处理步骤](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/06-extraction.md#L94-L124) |
| 语义检索 | `find` 做低延迟单查询；`search` 可结合 session 做意图分析和查询扩展；层级检索从全局候选目录开始，再用优先队列向下探索 | [`find` / `search` 对比](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/07-retrieval.md#L13-L38)、[层级检索流程](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/07-retrieval.md#L74-L122) |
| 上下文组装 | `search(mode="context")` 在一次请求内完成查询扩展、候选召回、跨轮去重、按类别配额、L0/L1/L2 档位选择、token 预算和可选 digest | [MCP search 参数与语义](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/server/mcp_endpoint.py#L274-L354)、[统一组装流水线](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/retrieve/context_assembler/pipeline.py#L52-L165) |
| Session 与记忆 | 记录文本、图片、上下文引用和工具调用；commit 同步归档，后台生成摘要、提取/去重长期记忆并记录 `memory_diff.json` | [Session API 与 Part 类型](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/08-session.md#L19-L96)、[两阶段 commit](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/08-session.md#L97-L115)、[记忆提取与审计](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/08-session.md#L136-L170) |
| Agent 经验演化 | 从 case 进一步生成 trajectory、experience 和可选 session skill；可查询 experience 被应用后的 trajectory 与结果分布 | [提取触发条件](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/06-extraction.md#L200-L211)、[Agent Evolution API](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/api/19-agent-evolution.md#L1-L14) |
| 上下文编译 | 用 Skill 把资料编译成 LLM Wiki、知识图谱、日报或知识蒸馏产物 | [官方产物类型](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/context-compilation/01-overview.md#L30-L43) |
| 多租户与权限 | account 隔离团队，user 隔离记忆/会话；共享资源支持 ACL；actor peer 可限制某一 peer 子树；提供 ROOT/ADMIN/USER | [身份模型](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/11-multi-tenant.md#L21-L45)、[共享与隔离边界](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/11-multi-tenant.md#L72-L117) |
| 版本与搬运 | VikingFS 上提供 Git-backed commit/log/show/diff/restore；另有 OVPack 导入导出、backup/restore | [快照模型](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/api/11-snapshot.md#L1-L5)、[Service 列表中的 PackService](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/01-architecture.md#L63-L74) |
| 集成与可观测性 | 官方覆盖 Claude Code、Codex、Cursor、OpenCode、OpenClaw、Hermes、pi、LangChain/LangGraph、MCP；并提供 Studio、Prometheus、OpenTelemetry、retrieval/session/queue observer | [集成总览](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/01-overview.md#L1-L26)、[已完成功能路线图](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/about/03-roadmap.md#L63-L93) |

### 1.3 明确的非能力与边界

1. **不是 Agent 工作调度器。** `task` 状态机跟踪的是 `add_resource`、`session_commit`、`admin_reindex`、`snapshot_restore_reindex` 等服务端后台处理作业，不是用户工作任务，也不表达“Agent A 把 Issue X 交给 Agent B”。[可取消 task 类型](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/api/17-tasks.md#L141-L166)。
2. **没有第一类 handoff 协议。** 官方 MCP 面只有 `find/search/read/list/tree/remember/write/edit/add_resource/watch/grep/glob/forget/health`；没有 publish/claim/accept/complete handoff 工具。[权威 MCP 注册表起点](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/server/mcp_endpoint.py#L3-L10)及[各工具注册位置](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/server/mcp_endpoint.py#L247-L299)。源码中的 `handoff` 主要是 RAGFS path-lock 在线程/队列间移交所有权，不能等同于 Agent 工作交接；记忆模板中出现的 `terminal_handoff` / `handoff_after_verification` 也只是供未来召回的经验文本字段，不是有状态的交接控制面。[trajectory 模板中的字段语义](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/prompts/templates/memory/trajectories.yaml#L38-L109)。
3. **不是无模型成本的纯文件数据库。** 文件落盘和基础解析可以不调用 LLM，但 L0/L1 生成、意图扩展、记忆抽取、可选 rerank/digest 会调用 VLM/LLM/Embedding 服务。[解析与语义分离说明](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/06-extraction.md#L7-L15)。
4. **不是已冻结的稳定平台。** 包元数据仍标为 Alpha，官方 README 也称“还在早期阶段”；主干文档和接口仍快速变化。[`Development Status :: 3 - Alpha`](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/pyproject.toml#L11-L31)、[官方早期阶段声明](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/README_CN.md#L258-L266)。
5. **开源版还不是分布式集群数据库。** 当前有 localfs、S3、memory 内容后端、向量后端和 primary+backup 多写，但路线图仍把“分布式存储后端”列为未来计划。[存储后端与多写](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/05-storage.md#L60-L82)、[未来计划](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/about/03-roadmap.md#L97-L107)。
6. **MCP 直连不自动形成记忆。** 通用 MCP 只给模型主动工具面，没有自动召回、capture 和 commit；想要完整 lifecycle 必须做 hook 适配或复用 shared core。[三条接入路径能力矩阵](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/16-capability-reference.md#L619-L651)。

## 2. 数据模型与虚拟文件系统

### 2.1 URI 与身份空间

当前主干的主要目录语义是：

```text
viking://
├── resources/                         # account 共享资源，可叠加 ACL
├── agent/skills/                      # account 全局共享技能
└── user/{user_id}/
    ├── memories/                      # 用户长期记忆
    ├── resources/                     # 用户私有资源
    ├── skills/                        # 用户私有技能（默认）
    ├── peers/{peer_id}/
    │   ├── memories/
    │   └── resources/
    └── sessions/{session_id}/         # 会话、工具结果和归档
```

`viking://~/...` 在请求边界展开成当前认证用户的显式路径；account、user、peer 是不同概念：account 是租户，user 是数据 owner，peer 是当前 user 内的稳定交互对象或受限视图。[家目录别名和 peer/session 路径](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/04-viking-uri.md#L98-L164)。源码也明确把 `peers` 定义为用户关于稳定交互对象的长期记忆根，而不是新的 tenant。[预置目录定义](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/core/directories.py#L109-L134)。

这个 owner/peer 分离值得 Engram 借鉴，但不能直接拿 peer 代替 agent_id 或 task_id：交互对象、执行者和工作任务是三条不同的身份轴。

### 2.2 L0/L1/L2 与 freshness

OpenViking 的三层不是传统 RAG 的固定 chunk 大小，而是“目录语义 + 原文”的渐进视图：

| 层级 | 存储 | 默认上限 | 使用位置 |
|---|---|---:|---|
| L0 | `.abstract.md` | 256 字符 | 向量召回、快速相关性判断 |
| L1 | `.overview.md` | 4000 字符 | rerank、目录导航、组装上下文 |
| L2 | 原文件/子目录 | 无统一上限 | 精确读取与完整证据 |

上述限制和 sidecar 语义见[官方层级表](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/03-context-layers.md#L5-L17)。新 sidecar 使用 OKF Markdown，frontmatter 记录 `directory`、来源、生成组件和 freshness；正常预览/embedding 只消费正文及明确白名单元数据，避免把内部控制字段混进模型。[OKF 与读取表面](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/03-context-layers.md#L78-L138)。

freshness 只统计目录的直接子项，并在直接子项超过 32 时稳定采样。当前 resource/skill 每次语义任务成功都会向父级冒泡；官方文档明确把热点目录的重复刷新和写放大列为待优化项。[freshness 与冒泡 TODO](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/03-context-layers.md#L140-L178)。

对 Engram 的含义：L0/L1 应是可重建的派生视图，必须带来源版本、生成器、更新时间和 stale 状态；不能让 sidecar 变成第二份不可判定的新事实源。

### 2.3 双层存储

OpenViking 将内容和索引分离：RAGFS/AGFS 保存 L0/L1/L2 和多媒体；向量库保存 URI、父 URI、类型、level、向量和标量元数据，不保存文件正文。[官方双层架构](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/05-storage.md#L1-L33)。`Context` 源码中也把 URI、parent、type、level、session、account、owner 等索引字段汇集到统一对象。[`Context` 数据结构](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/core/context.py#L58-L115)。

Engram 与此理念一致，但底层真相源不同：Engram 的 Git-versioned Markdown wiki 是 source of truth，SQLite 是可重建派生索引。引入 OpenViking 理念时应保留 Engram 的这个不变式，不改成 RAGFS 主存储 + 独立向量数据库。

## 3. 摄取、检索与分层加载

### 3.1 资源摄取

摄取流水线分为三个边界：

```text
输入 → Parser（格式转换/结构化，无 LLM）
     → TreeBuilder（临时树进入存储并排队）
     → SemanticQueue（异步 L0/L1 + embedding）
```

Parser 和语义生成分离是一个重要可靠性设计：原始内容先安全落地，昂贵、可失败的模型处理在后台完成。文档还定义了目录扫描、标题/长度拆分、代码骨架提取、多模态摘要、watch 更新和异步 task 跟踪。[Parser/TreeBuilder/SemanticQueue 的职责](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/06-extraction.md#L16-L124)。

Engram 可复用的不是所有 parser，而是摄取合同：`source snapshot -> normalize -> atomic publish -> derive summaries/index -> report provenance/failure`。这样 Agent 在语义处理未完成时仍可读取原文，并能明确看到 pending/failed，而不是把“暂无向量”误认为“资源不存在”。

### 3.2 两种检索与一种上下文组装

OpenViking 实际存在三个不同层次：

1. `find`：单一查询、无 session、低延迟的向量/标量检索；
2. `search(mode="list")`：可用 IntentAnalyzer 把 session 摘要、最近消息和当前查询改写为多个 typed queries，再做层级检索；
3. `search(mode="context")`：返回可以直接注入 Agent 的预算化上下文块，并附 query expansion、配额、档位、去重和 rewrite 统计。

层级检索会先全局定位起始目录，再递归搜索子节点；目录不是仅用于展示，而是检索空间的一部分。[检索算法](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/07-retrieval.md#L74-L130)。

更值得 Engram 引入的是 context assembler：

- 原查询永远保留，session-aware expansion 只追加有限查询；失败或超时回退原查询。[扩展源码](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/retrieve/context_assembler/expansion.py#L1-L68)。
- recall ledger 在 session 内冷却已注入 URI，避免每轮重复塞入同一正文。[流水线的 ledger 与统计](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/retrieve/context_assembler/pipeline.py#L65-L95)。
- 只读取计划档位需要正文的候选，不为每个 hit 做 read。[按需读取](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/retrieve/context_assembler/pipeline.py#L97-L115)。
- 先把候选放入默认低成本档，再用剩余预算升级 overview/full；超大项降级而不是任意截断。[预算实现](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/retrieve/context_assembler/budget.py#L85-L193)。
- 输出包含 planned query、阈值、tier counts、token 使用、dedup 和 rewrite 状态，检索过程可调试。[组装统计](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/retrieve/context_assembler/pipeline.py#L143-L165)。

Engram 不应为了模仿“层级检索”而放弃现有 FTS5 + graph RRF + optional vector RRF。更合理的是把 OpenViking 式 assembler 放在现有召回器之后：**召回负责找候选，assembler 负责选表示层级、控制预算、避免重复并保留 trace。**

## 4. Session、Memory 与跨 Agent 支持

### 4.1 Session 是原始证据和记忆提交边界

Session 保存当前消息、tool result 和 history archive。`commit()` 的 Phase 1 同步写归档、清空活动消息并返回 task；Phase 2 后台生成 L0/L1、提取记忆、更新使用计数、写 `memory_diff.json` 和 `.done`。[两阶段 commit 与目录结构](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/08-session.md#L97-L115)及[持久化布局](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/08-session.md#L229-L263)。

同一个 session 内还有一个明确的“恢复上下文”原语：`get_session_context()` 在 token budget 内返回最新完成归档的 overview，以及其后的未归档消息和 live messages；预算先保活动消息，再给最新 overview。[Session API 的恢复语义](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/api/05-sessions.md#L684-L715)。这适合**同一 session 的续跑**，仍不等于另一 Agent 对工作任务的 claim/accept。

记忆提取不是单纯 append：先召回相似记忆，再由 LLM 决定 skip/create/merge/delete，最后写文件和索引。每次提交的 add/update/delete 和 skipped operation 都进入审计 diff。[记忆去重决策](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/08-session.md#L136-L170)、[`memory_diff.json` 结构](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/08-session.md#L168-L227)。

Engram 已有 observation、session summary、wiki supersession、pending-writes、audit 和 admission chain，治理边界更适合作为长期记忆主线。可借鉴 OpenViking 的 message parts、context-used 记录、tool-output externalization 和 per-commit diff，但不应让模型可以绕过 Engram 的验证/审批策略直接 merge/delete canonical wiki。

### 4.2 OpenViking 如何实现跨 Agent 共享

它依靠四层组合，而不是一个 handoff 对象：

1. **同一服务端**：多个 Agent runtime 连接一个 OpenViking Server，读取同一 account/user 的上下文。[所有集成共同前置](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/01-overview.md#L24-L30)。
2. **统一 lifecycle 语义**：Claude Code/Codex 等插件在 prompt 前召回、turn 后捕获、compact/end 时 commit；Codex 还在恢复时注入最新 archive digest。[Codex 插件工作原理](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/04-codex.md#L52-L63)。
3. **workspace-derived peer**：无显式 peer 时，JS 系插件把 cwd 中非字母数字字符替换为 `-`，作为 `X-OpenViking-Actor-Peer`，实现按项目隔离。[workspace peer 规则](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/16-capability-reference.md#L199-L208)及[实现源码](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/agent-plugins/servers/shared/workspace-peer.mjs#L1-L16)。
4. **历史日志反向导入**：可以把 Claude Code、Codex、OpenCode、Hermes、OpenClaw 等本地日志标准化后重放为 Session，再触发记忆抽取；用 SQLite cursor 实现幂等续传。[log ingestion 的模型与幂等流程](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/09-log-ingestion.md#L1-L15)及[重放/游标机制](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/09-log-ingestion.md#L104-L117)。

这里的“共享”不能理解成跨客户端复用同一个 session。各 harness 使用不同的 session ID 前缀；官方能力参考更明确说明 Trae 与 Trae CN 的同一份工作落在两组 session，跨客户端依赖服务端**抽取后的记忆空间**而不是 session 复用。[跨客户端 session 说明](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/16-capability-reference.md#L506-L513)。subagent 行为也没有统一语义：Claude Code 建独立子会话，Codex 折叠进主会话，DSH 建独立会话但不保留父子关系，Hermes 子任务可完全跳过 OpenViking，其余 harness 多数无专门处理。[官方 subagent 会话对照](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/16-capability-reference.md#L355-L365)。

所以，Agent B 能“知道”Agent A 过去的工作，前提是 A 的会话被 capture/commit，相关内容被抽取或归档，B 使用兼容的 account/user/peer scope，并在新 prompt 上召回或主动查询。默认 Codex 插件甚至采用 broad recall：全局、当前 workspace 和其他 workspace 的记忆都可能进入候选，只是其他 workspace 被降权；只有 actor 模式才严格限制到全局加当前 peer。[Codex broad/actor scope 规则](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/examples/codex-memory-plugin/README.md#L119-L127)。因此 scope 必须显式纳入 Engram 的安全不变式。这一链路不是 A 明确把一个未完成工作项定向交给 B。

### 4.3 为什么这还不是跨任务交接

一个完整交接至少要回答：

- 谁发起、交给谁（具体 Agent、能力标签、队列或任意下一 Agent）；
- 交接的是哪一个工作任务，而不只是哪个对话 session；
- 当前目标、完成标准、约束、下一步、开放问题是什么；
- 哪些文件、commit、worktree、外部对象和来源属于本次工作；
- 接收方是否 claim/accept，租约是否过期，是否允许并行接收；
- 交接之后谁拥有写权，如何避免双写或重复执行；
- 最终是 completed、rejected、superseded 还是 expired；
- 接收时应该注入哪些摘要、哪些 URI 指针、哪些原始证据。

OpenViking 的 Session、peer、memory 和后台 task 不能单独回答这些问题。Engram 的 handoff 已经有 open/accepted/expired 状态与项目作用域，[现有架构说明](./ARCHITECTURE.md#storage-architecture)；下一步应把 OpenViking 的分层 context packet 和 provenance 引入 Engram handoff，而不是新增一套平行的“OpenViking task”。

## 5. 关键架构与接口

### 5.1 架构分层

官方架构将系统划分为 Client、Service、Retrieve、Session、Parse、Compressor 和 Storage。Service 把 HTTP/CLI 与业务逻辑解耦；Storage 再把 RAGFS 内容和向量索引分开。[模块职责表](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/01-architecture.md#L51-L83)。三条主数据流分别是：

```text
资源：输入 → Parser → TreeBuilder → RAGFS → SemanticQueue → Vector Index
检索：查询 → Intent/Expansion → Recall → Rerank → Context Assembler
会话：消息 → Archive → Summary/Memory Extract → Memory Diff → Index
```

### 5.2 对外接口

| 表面 | 作用 | 关键边界 |
|---|---|---|
| HTTP REST | 完整服务面：filesystem、content、resources、search、sessions、tasks、watches、ACL、admin、snapshot、observer | 适合 SDK、跨语言和管理面 |
| MCP `/mcp` | Agent 主动工具面，当前权威实现注册 15 个工具 | 默认不等同于自动 capture/commit |
| Rust `ov` CLI | 文件浏览、资源导入、session、snapshot、admin、reindex、task、watch 等 | 管理/运维能力多于 MCP |
| Python/TypeScript/Go SDK | 程序化客户端 | 宿主自己决定生命周期 |
| Agent hooks/plugins | 自动 recall/capture/commit | 必须正确映射宿主事件、session id 与关闭路径 |
| WebDAV | 将资源树暴露为文件协议 | 只是一种资源访问表面 |
| Studio/Helper | 浏览、配置、会话分析 | Helper 目前 Beta，且官方 capability 文档将其标为不在代码基线内 |

MCP endpoint 源码说明其工具注册是权威列表，并复用 REST 相同的身份解析与 request context，而非另造一套认证。[MCP 身份传播与 FastMCP 初始化](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/server/mcp_endpoint.py#L1-L15)及[中间件实现](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/server/mcp_endpoint.py#L155-L232)。这条“所有传输共用一个 typed service/auth boundary”值得 Engram 保持。

## 6. 成熟度、依赖、限制与许可证

### 6.1 成熟度判断

积极信号：

- 最新正式版已到 0.4.17.1，仓库已有完整 docs、Docker/Helm、Web Studio、多个 SDK、benchmark 和大量 integration/test 目录；[固定版本发布页](https://github.com/volcengine/OpenViking/releases/tag/v0.4.17.1)、[固定 commit 的仓库树](https://github.com/volcengine/OpenViking/tree/c486088ee7c43c46ba0e6d494e5622086a499aab)。
- 有 session crash recovery、持久队列、path lock、task cancel、snapshot、ACL、加密、Prometheus/OTel 等生产化设施；官方架构文档称 QueueFS SQLite 会在重启后继续任务。[崩溃恢复说明](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/09-transaction.md#L374-L393)。
- 官方发布了 LoCoMo/tau2 结果和可复现 benchmark 目录，但数字依赖特定模型和测试设置，只能视为候选证据，不能直接外推到 Engram。[README 的评测边界](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/README_CN.md#L93-L105)。

风险信号：

- 包元数据仍为 Alpha，官方也明确称项目处于早期阶段。
- 主干变化速度很高，`recall` 已被弃用并折叠到 `search(mode="context")`，0.3.x 到 0.4.x 又经历了 user/peer 身份迁移；集成方需要经常对齐 schema 和 hook 行为。[弃用说明](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/api/16-memory.md#L25-L44)、[0.4.1 身份迁移](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/about/02-changelog.md#L115-L134)。
- 文档在同一 commit 内也有漂移：上下文类型页称 `memories/tools` 和 `memories/skills` 已禁用，[对应说明](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/02-context-types.md#L55-L71)；Memory API 却把 tools/skills 列为“当前启用的内置类型”，[矛盾位置](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/api/16-memory.md#L1-L21)。因此任何落地决策都应以实际 API/schema/test 为准，不能只看概念页。
- freshness 父级冒泡仍有写放大 TODO；分布式后端仍在未来计划。

综合判断：这是一个**能力宽、实现真实、仍快速演进的 Alpha 平台**。适合系统性吸收产品合同和算法模式，不适合未经隔离地作为 Engram 核心依赖。

### 6.2 主要依赖与部署成本

- 主服务要求 Python >= 3.10，并依赖 FastAPI/Uvicorn、OpenAI/LiteLLM/火山引擎、MCP、解析器、tree-sitter、加密和 OpenTelemetry 等较宽依赖面。[`pyproject.toml` 主依赖](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/pyproject.toml#L11-L87)。
- Rust workspace 包含 `ov_cli`、`ragfs`、`ragfs-python`，即核心文件系统和 CLI 并非纯 Python。[Cargo workspace](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/Cargo.toml#L1-L15)。
- Codex/Claude Code 插件共享 Node 实现；Codex 手工安装要求 Node >= 22 和足够新的 Codex/plugin_hooks。[Codex 前置条件](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/04-codex.md#L34-L49)。
- 语义能力需要至少一个 embedding 模型，很多高级路径还需 LLM/VLM/rerank；本地部署的性能、成本和数据边界取决于模型提供商配置。

Engram 当前是自包含 Rust binary。为了引入理念而引入 Python server、Node hook core、RAGFS 和另一套向量存储，会破坏其最重要的操作性优势。

### 6.3 许可证

许可证不是单一口径：

- 主项目：AGPLv3；
- `crates/ov_cli`：Apache 2.0；
- `examples`：Apache 2.0；
- third_party：各自协议。

官方在 [README 许可证节](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/README_CN.md#L273-L280)明确列出以上边界；主项目 [`pyproject.toml`](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/pyproject.toml#L11-L22)也声明 AGPL-3.0。AGPL 专门覆盖通过网络向用户提供修改版的场景，[许可证前言](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/LICENSE#L8-L48)。

Engram 自身 workspace 明确声明 MIT。[Engram `Cargo.toml`](https://github.com/semantic-craft/engram/blob/6a9e67837fccdac66e90c0efefefb5a2985165b5/Cargo.toml#L18-L23)。因此本文的实现建议限于 **clean-room 理念吸收**：把官方文档当行为研究材料，在 Engram 内独立写规格、实现和测试；不复制、不修改、不链接 OpenViking 主项目代码。任何直接复用或部署都应逐文件确认许可证边界并做正式法律审查。不要把 `examples` 的 Apache 2.0 误读成整个仓库都是宽松许可。本说明不是法律意见。

## 7. Engram 应复用的理念

### 7.1 建议吸收的设计合同

| OpenViking 理念 | Engram 中的合理落点 | 不变式 |
|---|---|---|
| 可浏览的 Context FS | 在现有 workspace/project/wiki path 之上提供 `engram://` 或等价 typed URI/view；支持 list/tree/read/abstract/overview | Markdown wiki 仍是 source of truth；URI 是视图，不是第二份存储 |
| Resource/Memory/Skill 统一发现 | 建立 typed context registry，把 wiki page、session/archive、外部 resource 和已安装 skill 映射到统一候选 | Skill 指令文件与 durable memory 的治理规则仍需区分 |
| L0/L1/L2 | page/directory 的 abstract、overview、full body；handoff 也用 brief/plan/evidence 三档 | 派生层必须带 provenance、generator、source revision、freshness，可重建 |
| 一次请求的 context assembler | 放在当前 FTS5 + graph RRF + vector RRF 后，执行 quota、tier、budget、dedup、rewrite 和 trace | 不替换现有多路召回；assembler 不决定事实真伪 |
| Session message parts | observations 增加结构化 text/tool/context/artifact 引用，长 tool output 外置并保留 synopsis/reference | capture 仍 fire-and-forget、有界、先 sanitize |
| commit 后异步提取 | SessionEnd/PreCompact 后异步整理；给每次学习写入保存 diff、source session 和提案状态 | 继续经过 admission、validation、pending-writes/approval 和 audit |
| user / peer 分离 | 保持 owner(user)、executor(agent/harness)、counterparty(peer)、scope(workspace/project)、work item(task) 为独立轴 | 不能用 cwd 或 peer 冒充稳定 project/task identity |
| 插件 shared core | 一套标准 capture/recall/commit contract，各 harness 只做事件适配和 deadline/identity 转换 | MCP-only 接入不得宣称有自动 capture |
| 资源 import/watch | 外部资源使用 source manifest、dry-run、增量 cursor/watch、atomic publish、派生索引 | 外部资源与 canonical wiki 分区，凭据不进入记忆/日志 |
| Context compile | 复用 Engram 已有 Karpathy wiki consolidation，把“来源 -> skill/policy -> 产物”显式化并可评测 | 编译产物必须有来源定位，不自动提升为批准事实 |
| 检索/提交可观察 | 保存 query plan、候选、层级、token、去重、降级、写入 diff | 默认输出保持紧凑，详细 trace 按需读取 |

### 7.2 Engram 已经比 OpenViking 更适合保留的部分

根据当前 [`docs/ARCHITECTURE.md`](./ARCHITECTURE.md)，Engram 已有以下不应退化的能力：

- Git-versioned Markdown source of truth + SQLite derived index；
- FTS5、graph RRF、optional vector RRF 和 raw observation fallback；
- typed cross-agent handoff；
- workspace/project scope 与跨项目 link；
- 单 writer actor、bounded hook ingestion、sanitizer；
- wiki supersession、retention、audit、admission webhook；
- auto-improvement 的 validated proposal / pending approval 路径。

OpenViking 可以补齐的是上下文的**可寻址视图、分层供给、resource ingestion、context assembly、session parts 和多 Agent adapter contract**，不是替换这些底座。

## 8. 不应照搬的部分

1. **不照搬存储底座。** 不把 Engram 从 Markdown+Git/SQLite 改成 RAGFS+独立向量库，也不引入 Python 常驻服务作为核心。
2. **不照搬 cwd 字符替换得到 peer id。** 该算法把所有非字母数字统一成 `-`，存在碰撞，而且绝对路径在不同机器上不稳定；Engram 应继续使用解析后的 workspace/project stable ID，把 cwd 只当发现证据。
3. **不把 peer 当 Agent，不把 background task 当工作任务。** owner、peer、agent、session、work item 和 async job 必须分开建模。
4. **不让自动记忆抽取直接改 canonical wiki。** OpenViking 有 memory diff，但其 create/merge/delete 仍主要由模型决策；Engram 应保留验证、提案、审批、审计与可恢复写路径。
5. **不照搬宽依赖和宽工具面。** OpenViking 的完整平台覆盖 parser、bot、Studio、多租户、OAuth、WebDAV、GPU 向量等；Engram 应按当前任务逐层引入，不为了“理念完整”一次性复制产品面积。
6. **不机械复制层级检索算法。** OpenViking 文档中的 `score_propagation_alpha` 默认 1.0，等于忽略父分数；Engram 已有图和词法证据，不应退回单一路径的向量递归。[参数说明](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/07-retrieval.md#L124-L130)。
7. **不让 L0/L1 冒泡阻塞 capture。** 官方已承认当前每次任务向上刷新会产生热点写放大；Engram 应 coalesce、debounce，并允许 stale-but-readable。
8. **不直接复制、修改或链接 AGPL 主项目代码。** OpenViking 主项目 AGPL-3.0，而 Engram 是 MIT；即使产品理念相同，也只做 clean-room 的域模型、规格和测试独立实现。若未来考虑 Apache 2.0 子目录，也必须逐文件确认边界、保留 notice，并经许可证审查后另行决定。
9. **不把 benchmark headline 当验收。** 必须在 Engram 自己的多 Agent handoff、跨任务恢复、检索预算和来源忠实度数据集上做 A/B 与回放评测。

## 9. 跨 Agent、跨任务交接的目标模型

这一节是基于上游能力缺口与 Engram 现状的设计推论，不是 OpenViking 已有功能。

### 9.1 最小交接对象

Engram 的 handoff 应从“下一 Agent 可读的一段摘要”扩展为可审计的 continuation envelope，至少包含：

```text
identity
  handoff_id, workspace_id, project_id, work_item_id
  source_session_id, source_agent/harness, target_selector

state
  open | claimed | accepted | completed | expired | superseded
  lease_owner, lease_expires_at, revision

intent
  objective, acceptance_criteria, constraints
  next_steps[], open_questions[], blockers[]

artifacts
  files[], git/worktree/ref, external_object_refs[]
  changed/verified/uncommitted status

context
  L0 brief, L1 plan/status, L2 evidence refs
  memory/page/resource/session URIs + source revisions

provenance
  created_at, updated_at, source observations
  tests/checks performed, evidence, confidence/known-unknown
```

关键点是把大正文留在原始 page/session/resource，用稳定 URI 和 revision 引用；handoff 本身只携带能让下一 Agent 决定“接不接、先读什么、第一步做什么”的预算化包。这正是 OpenViking L0/L1/L2 和 context assembler 可补给 Engram handoff 的位置。

### 9.2 交接状态流

```text
Agent A capture/commit
  → publish handoff(open, revision=1)
  → Agent B discover by exact project/work item/target selector
  → claim with lease + compare revision
  → accept and materialize L0/L1 context packet
  → on-demand read L2 evidence
  → checkpoint progress / publish successor handoff
  → complete or release/expire
```

需要的并发语义：

- claim 必须 compare-and-set，避免两个 Agent 都以为自己拥有写权；
- handoff 更新使用 revision/expected_revision，防止旧 Agent 覆盖新状态；
- lease 超时允许恢复，但不能隐式抹掉原 owner 的未提交工件；
- accept 只表示接收，不表示任务已完成；
- 任务完成、Git commit、push、PR、merge、release 仍是不同事实字段；
- 同一 task 可以有多次 handoff，使用 predecessor/successor 链，而不是覆盖历史。

### 9.3 跨 Agent 与跨任务的 scope 规则

- **同一任务跨 Agent**：固定 `work_item_id`，source/target agent 变化；默认继承 project scope 与 artifact refs。
- **同一 Agent 跨任务**：新的 `work_item_id`，可以显式引用前一任务的 page/resource，但不能自动继承它的 working state。
- **跨项目依赖**：用 Engram 已有跨项目 link 显式引用，默认只读；目标项目必须真实存在，缺失或 partial scope fail closed。
- **跨机器**：project/work item 用稳定 ID，不用绝对 cwd；artifact 引用要带 repo identity/ref 和可验证 hash。
- **子 Agent**：parent task 与 child task 分开；child completion 回传结构化 result/evidence，不能直接把 parent handoff 标成 completed。

## 10. 建议的引入顺序

1. **先补语义，不换底座**：冻结 Context URI、context type、detail tier、agent/task/session identity 和 handoff envelope；继续用现有 wiki/SQLite。
2. **先让 handoff 真正可接管**：增加 work item、target selector、claim/lease/revision、artifact manifest 和 L0/L1/L2 context refs；做两个不同 harness 的端到端恢复测试。
3. **再做 context assembler**：基于现有 RRF 候选实现 quota、tier、budget、dedup、trace；把 handoff accept、session start、普通 query 共用同一组装器。
4. **再做 Resource plane**：从最小的本地目录/Git/HTTP Markdown 开始，落 source manifest 和 watch cursor；parser 与语义派生分离。
5. **最后扩展经验学习与 compile**：只有在 provenance、评测、审批和回滚闭环稳定后，才引入 case→trajectory→experience 和 skill-driven compile。

“完完整整引入理念”的验收不应是拥有同样多的 API，而应是以下端到端故事成立：**任意受支持 Agent 在一个任务中产生可追溯上下文；另一个 Agent 能在正确 scope 下发现、原子接管、按预算加载、继续执行并回写可审计结果；全程不需要复制粘贴，且不会混淆项目、任务、Agent、peer、session 或后台 job。**

## 一手来源索引

- [OpenViking README（中文，固定 commit）](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/README_CN.md)
- [架构概述](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/01-architecture.md)
- [上下文类型](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/02-context-types.md)
- [L0/L1/L2](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/03-context-layers.md)
- [Viking URI](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/04-viking-uri.md)
- [存储架构](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/05-storage.md)
- [摄取与语义提取](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/06-extraction.md)
- [检索机制](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/07-retrieval.md)
- [Session 与 Memory](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/concepts/08-session.md)
- [集成能力参考](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/docs/zh/agent-integrations/16-capability-reference.md)
- [MCP endpoint 源码](https://github.com/volcengine/OpenViking/blob/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/server/mcp_endpoint.py)
- [Context assembler 源码](https://github.com/volcengine/OpenViking/tree/c486088ee7c43c46ba0e6d494e5622086a499aab/openviking/retrieve/context_assembler)
