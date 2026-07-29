# Research: Engram 维护 `CLAUDE.md` / `AGENTS.md` 与项目行为守则

**日期：** 2026-07-29

**状态：** 产品与架构调研，不是实现规格

**范围：** 项目级长期记忆、行为守则和 Agent Skills 的分层治理。这里不讨论普通对话记忆或把原始文档切块后直接检索的常规 RAG。

> **后续状态（2026-07-29）：** 本文完成后，GitHub Issues #5–#13 已按这里的边界完成实现并合并。下文第 3.2–6 节保留的是实施前的差距分析与设计依据，不代表当前版本仍缺少这些能力；现行行为以 `README.md`、`docs/usage.md`、`docs/ARCHITECTURE.md` 和 `docs/auto-improvement-loop.md` 为准。

## 结论

Engram 适合增加这项能力，但它应当被设计成一个**可审计的指令编译器与维护器**，而不是让模型自行改写 `CLAUDE.md` / `AGENTS.md`。

核心边界是两条写入通道必须分开：

1. **Engram 自有路由资产**：只更新 Engram 明确拥有的 marker 区块和带 managed marker 的 skills。用户执行安装/刷新命令后，可以幂等更新。
2. **项目行为守则**：从项目记忆中发现候选规则，生成带来源、目标文件、基线哈希和差异的 proposal；逐项经人批准后才落盘。不得把 auto-improve 的自动批准默认值延伸到行为守则。

这既符合 Anthropic 文章所说的“减少常驻规则、让上下文按需披露”，也保留 LLM Wiki 的复利优势：观察和详细知识留在 Wiki，只有每个任务都必须知道、而且无法从仓库直接看出的少量不变式才提升为常驻指令。

## 1. Anthropic 文章给出的约束

一手来源：[The new rules of context engineering for Claude 5 generation models](https://claude.com/blog/the-new-rules-of-context-engineering-for-claude-5-generation-models)（Anthropic，2026-07-24）。

文章对本功能最相关的判断是：

- Anthropic 在 Claude 5 代模型上删除了 Claude Code 系统提示词的 80% 以上，而编码评测没有可测损失；其经验是旧提示过度约束，且系统提示、skills、`CLAUDE.md` 和用户请求之间容易形成冲突。
- 新模型更适合依据周边代码和任务语境判断；行为守则应描述目标、边界和接口，而不是枚举所有例子和机械规则。
- 通用上下文不应一次性全部装入。验证、代码审查等任务说明可以进入按需加载的 skill；长 skill 也应继续拆分。
- `CLAUDE.md` 应轻量：简述仓库用途，把常驻 token 主要留给无法从代码或目录推断的 gotcha；复杂流程应指向 skills 或引用材料。
- 记忆与 `CLAUDE.md` 的职责已分离：自动记忆承载工作中学到的事实和模式，`CLAUDE.md` 承载人希望每次会话都生效的行为指导。
- 指令、skills、引用材料和工具接口是一套整体；维护器不能只做“删短 Markdown”，还要能够建议把内容迁移到更合适的层。

这篇文章是 Claude Code 团队对 Claude 5 的产品经验，不应直接外推为所有模型或 agent harness 的共同加载语义。Engram 应借鉴其**分层原则**，同时为各客户端维护明确的发现和优先级适配器。

### 1.1 官方文件语义不能混用

Anthropic 的[项目记忆文档](https://code.claude.com/docs/en/memory)补充了几个必须进入产品约束的事实：

- Claude Code 原生读取 `CLAUDE.md`，不原生读取 `AGENTS.md`；共享单一来源时，官方推荐在 `CLAUDE.md` 中用 `@AGENTS.md` 导入，或在适合的平台使用 symlink。
- Claude Code 从工作目录向上加载祖先文件，并在访问子目录时按需加载子目录的 `CLAUDE.md`；`.claude/rules/` 支持按路径条件加载。
- `@` 导入只改善组织，不减少启动 token；外部导入首次出现时还会触发用户批准。
- 官方建议每个 `CLAUDE.md` 目标控制在 200 行以内，并定期检查嵌套文件与 rules 之间的过期和冲突。
- `CLAUDE.md` 是行为提示，不是安全强制层；必须保证的限制应落在 permissions、sandbox 或 hooks。

OpenAI 的[AGENTS.md 官方文档](https://learn.chatgpt.com/docs/agent-configuration/agents-md)则规定：

- Codex 在全局范围读取 `AGENTS.override.md` 或 `AGENTS.md`；项目范围从 Git 根一路走到当前工作目录，每级最多读取一个文件。
- 越接近当前目录的文件越晚进入提示，因此可覆盖上层指导；`AGENTS.override.md` 在同级优先。
- 项目指令合并大小默认上限为 32 KiB。Codex 的层级边界是启动时当前工作目录，不等同于 Claude Code 的子目录按需加载行为。

因此，“如果两个文件都存在就同时写入”只是安装启发式，不是可靠的 canonical-file 判断。

## 2. 公开 LLM Wiki 实践比较

以下只收录截至 2026-07-29 公开、明确实现或引用“raw sources → compiled wiki → schema/instructions”模式的项目。项目 README 的功能和指标是作者自述，除正式论文外不视为独立验证。

| 实践 | Raw sources | Compiled wiki | Schema / instructions | Lint / contradiction | Human review、Git / Obsidian | 对 Engram 的启示 |
|---|---|---|---|---|---|---|
| [Karpathy LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) | 人选择、LLM 不修改的不可变来源 | LLM 维护的互链 Markdown；新来源改写已有综合，而非只建索引 | 明确把 `CLAUDE.md` / `AGENTS.md` 视为第三层 schema，由人和 LLM 共演化 | 定期检查矛盾、过期、孤儿页、缺链和知识空缺 | 建议逐份 ingest、人参与摘要和强调重点；Git 留历史，Obsidian 是可视化 IDE | 行为守则是 Wiki 编译器的 schema，不是记忆本体；人应控制来源和规则升级 |
| [akitaonrails/ai-memory](https://github.com/akitaonrails/ai-memory#what-it-is) | 生命周期 observations，经边界清洗；原始召回是受限 fallback | 会话被编译为 Git 中的 Markdown Wiki，并提供 handoff、搜索和维护 | 安装 `CLAUDE.md` / `AGENTS.md` 路由及按需 skills | `memory_lint`、consolidation、pending proposals | Markdown 可由 Obsidian 打开；Git 保留演化；写入有审计/审批路径 | 自动捕获可取代人工投递来源，但行为规则仍需比普通 Wiki 页面更强的晋升门槛 |
| [My Second Brain by YFW](https://github.com/EricWcr7/My-Second-Brain-by-YFW#my-second-brain-by-yfw) | `raw/` 保存导入文件和资产 | `wiki/` 保存概念、来源页、overview、index、log；Obsidian 兼容 | 可检查的 General baseline 和 scope override 分别控制 schema、ingest、Ask、Review | structural/deep Review 检查 provenance、链接和缺失引用 | Git 是依赖；页面和来源保留在本机，可由浏览器或 Obsidian 检查 | scope override 表明“规则树”可以分层，但要把访问范围与权限边界明确区分 |
| [Eva Brain](https://github.com/jp-lorenc1o/Eva-brain#eva--llm-brain) | `raw/` 保持不可变 | Markdown brain，带 frontmatter、wikilink 和图 | `eva.json` marker、`EVA.md` profile、versioned Brain Standard 和模板 schema | 确定性 structural lint；新 lint issue 和删除会进入 review | 每次 ingest 在隔离 worktree；本地 Git 提供审阅与撤销，不要求远端；Claude/Codex adapters | marker 应代表可验证的格式/所有权；危险变更进入 gate；Git review 不必等同于 GitHub PR |
| [Kherad](https://github.com/mohammadmaso/kherad#kherad) | AI-compiled bundle 可指向 raw 文档目录 | LLM 编译为 concepts/entities/articles；Markdown 是正文真源 | 以 bundle/role/path policy 约束流程，未把 schema 全塞进单一根文件 | 冲突在 merge-request 阶段处理 | 每次显式保存形成 commit；提交 review 后 squash merge；逐行 diff、评论、批准/拒绝和 conflict editor | 非技术用户也能通过 proposal/diff 审阅；“有 Git”不等于“允许代理自行 commit/merge” |
| [Link](https://github.com/gowtham0992/link#link) | `raw/` 保存笔记、文档、转录等 | source-backed Markdown wiki | 多客户端官方 skills 负责按需动作；紧凑检索包而非把 Wiki 全塞进提示 | 写入层确定性，无 LLM 自动把推断变事实；提供 provenance 和 hygiene benchmark | 每条 memory 都可检查；agent 只 proposal、用户批准；文件可 grep/git-diff | 行为守则应沿用“agent proposes, human approves”；助手自身复述不得被当成用户偏好 |

### 2.1 层级访问的补充观察

[Semantic XPath](https://aclanthology.org/2026.acl-demo.28/)（ACL 2026 Demo）把对话记忆组织为树，并用结构路径选择性访问和更新。论文作者报告，相对其 flat-RAG baseline，性能提高 176.7%，同时只使用 in-context memory 方案 9.1% 的 token。

对本功能有价值的是**层级选择性访问**这一机制：根规则、子目录规则、task skill 和 Wiki 页面可以组成可导航的规则树，不必全量常驻。但它与 Anthropic 的 progressive disclosure 不是同一个证据：前者是特定对话记忆系统和作者实验，后者是 Claude Code 的产品实践。MVP 应先记录实际加载文件、token 预算和命中原因，再决定是否引入更复杂的语义路径选择器。

## 3. Engram 当前已经具备什么

### 3.1 已有基础

- Engram 已把 Markdown Wiki 定为真源，SQLite 只是派生索引；自动捕获、consolidation、auto-improve、搜索和衰减已经组成完整循环（[`docs/ARCHITECTURE.md`](ARCHITECTURE.md#L16-L21)、[`docs/ARCHITECTURE.md`](ARCHITECTURE.md#L44-L89)）。
- Wiki 有 Git 历史，也可直接由 Obsidian/vim 编辑，外部编辑由 watcher 回填索引（[`docs/ARCHITECTURE.md`](ARCHITECTURE.md#L123-L135)）。
- `memory_lint` 已能识别 `_rules/` 或 `kind: rule` 页面并建议复制到 `CLAUDE.md` / `AGENTS.md`，但不会比较文件或提出具体 patch（[`lint.rs`](../crates/engram-consolidate/src/lint.rs#L188-L214)）。
- auto-improve 已有证据、proposal、pending-writes、审计和可选人工审批机制；`_rules/` 被视为高影响目标（[`auto-improvement-loop.md`](auto-improvement-loop.md#L180-L214)、[`auto-improvement-loop.md`](auto-improvement-loop.md#L234-L262)）。
- `install-instructions` 从 core 单一来源取得 snippet，支持 `--print`、幂等写入、备份与原子替换（[`install_instructions.rs`](../crates/engram-cli/src/commands/install_instructions.rs#L27-L81)、[`apply_shared.rs`](../crates/engram-cli/src/commands/apply_shared.rs#L1-L18)）。
- 当前 marker 合并只修改 `<!-- engram:start -->` 到 `<!-- engram:end -->` 的第一个完整区块，保留前后用户内容（[`install_instructions.rs`](../crates/engram-cli/src/commands/install_instructions.rs#L159-L193)）。
- managed skill 只有在包含 `<!-- engram-managed: routing-skill -->` 时才允许无 force 覆盖（[`install_skills.rs`](../crates/engram-cli/src/commands/install_skills.rs#L163-L188)）。
- `memory_install_self_routing` 是只读工具：服务器返回 marker block、文件名、skills 和目标提示，由 agent 使用宿主文件工具落盘（[`server.rs`](../crates/engram-mcp/src/server.rs#L2311-L2379)）。这个边界对远端 Engram 服务尤其重要。

### 3.2 仍缺的维护能力

1. **没有 canonical source 检测。** 当前逻辑在两个文件都存在时写两份，在都不存在时默认建 `CLAUDE.md`（[`install_instructions.rs`](../crates/engram-cli/src/commands/install_instructions.rs#L84-L123)）。它不识别 `@AGENTS.md` pointer、symlink、stub、tool-specific override 或项目已经声明的单一真源。
2. **规则只有提示，没有 proposal。** 现有文档明确说 Engram 不自行编辑 rules file；lint 工作流止于“考虑复制”（[`docs/usage.md`](usage.md#L269-L282)）。
3. **预览不是 diff。** `--print` 展示将写入的完整 snippet，不包含旧值、逐项行为规则、来源和基线哈希（[`routing_instructions.rs`](../crates/engram-cli/tests/routing_instructions.rs#L156-L177)）。
4. **没有并发冲突检测。** 原子写和备份能恢复，但从读取到 rename 之间没有 expected hash；另一个 agent 在此期间改同一文件时可能被覆盖（[`apply_shared.rs`](../crates/engram-cli/src/commands/apply_shared.rs#L61-L96)）。
5. **畸形 marker 会被静默绕开。** 缺失 end marker 时现逻辑会追加新块；重复完整块只替换第一块。维护器需要先验证 marker 结构并 fail closed。
6. **没有跨层冲突和膨胀审计。** 当前 lint 不比较 Wiki `_rules/`、根指令、嵌套规则、skills、hooks/permissions 之间的重复、矛盾、失效路径和职责错放。
7. **安装资产也可能漂移。** 本仓库当前 `AGENTS.md` 的 Engram 区块占第 1–136 行，而 core 中所谓 slim snippet 位于 [`routing_snippet.rs`](../crates/engram-core/src/routing_snippet.rs#L29-L96)。这正说明需要报告“已安装版本与当前二进制资产是否一致”，而不是等待用户偶然刷新。

## 4. 建议的分层模型

| 层 | 装什么 | 加载策略 | 谁能写 |
|---|---|---|---|
| 强制技术边界 | sandbox、permissions、hooks、auth | harness 强制执行 | 管理员/用户；Engram 只报告错放 |
| canonical 根指令 | 仓库用途、少量跨任务不变式、最关键 gotcha、下层入口 | 每次任务 | 人工维护；Engram proposal 后批准写入 |
| 路径/组件规则 | 只对某目录或文件类型适用的约束 | Claude path rule 或 Codex nested `AGENTS.md` | 人工批准 |
| Agent Skills | 多步骤操作手册、验证、发布、审查等 | 语义触发、按需加载 | managed skills 可由 Engram marker 更新；用户 skills 不覆盖 |
| Engram Wiki | decisions、facts、gotchas、procedures、完整证据和历史 | `memory_query` / `memory_read_page` 按需 | 正常 Wiki 审批与写入链 |
| 原始 observations | 会话和工具事件 | 只作证据或 bounded fallback | 追加式捕获，不直接晋升成行为规则 |

重要推论：根文件里的“去这里找详细规则”应该足够短；把长内容拆成 `@` import 并不会减少 Claude Code 启动 token，因此真正的减负必须依靠 path-scoped rules、skills 或 Engram 检索，而不是单纯拆文件。

## 5. 产品需求

### 5.1 两个互不混写的 managed region

继续保留现有：

```text
<!-- engram:start -->
...binary-owned routing bootstrap...
<!-- engram:end -->
```

如果用户启用行为守则维护，再创建独立区域，例如：

```text
<!-- engram-rules:start -->
...only human-approved promoted rules...
<!-- engram-rules:end -->
```

要求：

- routing refresh 绝不能覆盖 approved rules；rule maintenance 绝不能修改 routing block。
- marker 外所有内容都视为 human-owned，只能报告或生成 anchored diff，不能直接重写。
- 每种 marker 必须恰好零或一对、顺序正确、不可嵌套。缺失、重复、交叉时只报告修复方案，不自动猜测。
- 已批准规则的 provenance 放在 Engram proposal/audit 记录中，不把长证据塞进每次加载的文件。需要稳定关联时只保留短 rule ID。

### 5.2 Canonical-file 检测

检测顺序应当是：

1. 用户或项目显式配置优先，例如 `[instructions] canonical = "AGENTS.md"`；显式配置不存在时才启用推断。
2. 记录文件类型、真实路径、symlink 目标、内容哈希、marker、导入/指针关系和 Git 状态。
3. 只有 `CLAUDE.md` 或只有 `AGENTS.md` 时，可把它作为对应客户端的候选，但仍需检查祖先/嵌套文件。
4. 两者都存在时：
   - `CLAUDE.md` 是 `@AGENTS.md` 或明确 thin pointer：`AGENTS.md` 为共享 canonical，Claude 文件只是 adapter；只更新 canonical rules，routing 可按客户端需要分别存在。
   - `AGENTS.md` 明确指向 `CLAUDE.md`：反向处理。
   - 两份内容不同且都含真实规则：视为 tool-specific sources，不擅自合并。
   - 两份近似复制：报告 drift 风险，并要求用户选择 canonical；不得静默挑一个。
5. symlink 指向仓库外部、无法解析的 import、循环或不受支持的 harness 时 fail closed。

MVP 正式支持 Claude Code 与 Codex 的官方语义。OpenCode、Cursor、Gemini 等可以沿用已有文件名提示，但在没有各自版本化适配器前只能标为“best effort”，不能宣称优先级等价。

### 5.3 Audit 与 rule candidate

只读 audit 至少输出：

- 实际加载链和判定理由：root、ancestor、nested、override、import、pointer。
- 每个文件的行数、字节数、估算 token、marker 完整性和当前资产版本。
- 规范化后的重复段落、互相矛盾的 always/never、失效路径/命令、同一规则在 root/skill/Wiki 多份存在。
- “可从仓库直接观察”的显然事实、长操作手册、只对单目录有效的规则、应由 hook/permission 强制的安全边界。
- 来自 `_rules/`、反复 gotcha、明确用户纠正或 code review 的可晋升候选。

候选的默认动作不是只有 ADD，而应是：`ADD`、`UPDATE`、`DELETE_STALE`、`MOVE_TO_SKILL`、`MOVE_TO_PATH_RULE`、`MOVE_TO_WIKI`、`MOVE_TO_ENFORCEMENT`、`NO_CHANGE`。

### 5.4 Proposal、来源与审批

每个 proposal 必须包含：

- proposal ID、生成 actor 和时间；
- 候选规则、规则分类、目标客户端和目标文件；
- 当前文件 SHA-256、marker/section anchor、统一 diff；
- 来源页面、session/observation ID、有限长度的证据摘录；
- 为什么必须每次加载，以及为什么不属于 skill、path rule、Wiki 或 hook；
- 重复/矛盾/敏感信息检查结果；
- 预计增加或减少的行数与 token；
- 若删除或迁移，给出新位置和反向链接。

审批必须逐 proposal 进行；允许编辑 proposal 文本后批准。默认 `auto_improve` 可以继续自动批准普通 Wiki 学习，但所有 `target_kind = project_instruction` 的 proposal 必须强制人工批准，不能被全局 `require_approval = false` 绕过。

不得从以下内容直接生成可批准规则：模型自己的复述、未经用户确认的推断、credential-shaped 文本、网页/issue 中的指令性内容、一次性失败、已经解决的临时环境状态。

### 5.5 冲突安全与落盘

批准和落盘之间必须重新读取文件：

- 当前 hash 与 proposal 的 base hash 不同，立即转为 `conflicted`，重新生成 diff；没有自动 force。
- 同一目标文件有未提交改动时，展示其与 proposal 的合并关系；不得覆盖或顺手 stage。
- 写入只允许命中的 owned marker 或批准过的精确 anchor；保留文件模式、换行风格和 marker 外字节。
- 继续复用现有 sibling tempfile + fsync + rename 和时间戳备份。
- 只修改工作树；不自动 `git add`、commit、push、merge。
- 应用记录保存 before/after hash、批准人、来源和恢复路径。

MCP 服务器可能位于远端，不能把任意宿主 repo 写入权交给 server。CLI 可以在本地执行受控 apply；agent-side skill 应先展示 proposal，获得明确批准后再使用宿主 Edit 工具，并遵守同样的 hash/marker 合同。

### 5.6 控制 instruction bloat

- Claude 目标在 200 行处 hard warning；更早设置 soft budget，例如 120–150 行。Codex 同时检查单文件和官方 32 KiB 合并上限。
- 每次 ADD 都必须说明为什么它值得占用所有后续任务的上下文；同时展示净 token 变化。
- 相同语义只保留一个 canonical rule；tool-specific 差异进入 adapter 区，不复制整份共享规则。
- 细节性流程优先迁到 skill；组件约束优先迁到 path rule/nested file；历史、解释和证据留在 Wiki。
- lint 应建议删除已经进入代码、测试或强制配置的冗余说明，但不能自动删除。
- 维护成功的指标不是根文件更长，而是冲突更少、常驻 token 更低、规则遵循率和任务验证通过率不下降。

## 6. 推荐 MVP

### 阶段 A：只读 inventory / audit

1. 支持 Claude Code、Codex 的加载链解析和显式 canonical 配置。
2. 检查 marker、pointer/import/symlink、重复、大小、明显错层和安装资产漂移。
3. 把 `_rules/` 和 gotcha 候选与现有指令做 exact/semantic 去重，只输出报告。

### 阶段 B：proposal-only

1. 为 ADD/UPDATE/DELETE/MOVE 生成逐项 diff、证据、base hash 和 token delta。
2. 在现有 pending-writes UI/审计模型中增加 `project_instruction` 类型，但与 Wiki `apply_batch` 执行器分开。
3. 所有行为守则 proposal 强制人工审批；routing asset 仍沿用现有显式刷新命令。

### 阶段 C：安全 apply

1. CLI 本地应用批准 proposal：expected-hash、marker/anchor、原子写、备份、冲突状态。
2. Agent Skill 提供等价的远端/agent-side review 流程；MCP server 保持无宿主文件写权限。
3. 加入 fixture：单文件、两个真实 sources、pointer、symlink、nested override、脏工作树、并发修改、重复/畸形 marker、CRLF、unmanaged skill。

### MVP 验收标准

- 在本仓库结构中，识别 `AGENTS.md` 是 canonical，`CLAUDE.md` 是 `@AGENTS.md` adapter，绝不把完整规则写两份。
- 无批准时 audit/propose 对工作树零写入；批准后只改 owned rule marker 或批准 anchor。
- 计划后人工/另一个 agent 改过目标文件时，apply 必须报告 conflict，不能覆盖。
- 删除或重复 marker 时 fail closed；恢复原文件可由时间戳备份完成。
- 每条晋升规则都有证据、目标层选择理由和 token delta；无法证明“每个任务都要知道”时不得进入根文件。
- 不因维护 `AGENTS.md` 而声称 Claude 已加载它；必须验证 `CLAUDE.md` import/pointer。反向也一样。

## 7. 非目标

- 不自动重写 marker 外的人类指令，不自动解决语义矛盾。
- 不把普通 Wiki auto-improve 的自动批准策略复用于行为守则。
- 不自动创建、提交或推送 Git 分支/PR；Git 只提供状态、diff、历史和恢复证据。
- 不以拆成多个 `@` import 冒充 token 优化。
- 不把 secrets、临时环境状态或模型自述提升为规则。
- 不尝试在第一版统一所有 agent harness；未知客户端采用只读报告和显式 target。
- 不用行为文本代替 sandbox、permissions、hooks、auth 等强制控制。
- 不把 Engram 扩张成通用工作流引擎或自动技能生成器；它只管理记忆到上下文层的晋升、布局和审计。

## 8. 最小的产品定位

可以把功能描述成：

> Engram 从有来源的项目记忆中发现行为守则候选，检查 `CLAUDE.md`、`AGENTS.md` 与 skills 的真实加载关系和冲突，生成节省上下文的可审阅 diff；只有人批准后才更新项目的 canonical 指令层。

这个定位比“自动维护 CLAUDE.md/AGENTS.md”更准确，也更安全。它把 Karpathy 的 schema 共演化、Anthropic 的 progressive disclosure，以及 Engram 已有的 provenance/pending-writes/atomic write 组合成一条闭环，同时不把模型生成的记忆直接升级成每次任务都会执行的规则。
