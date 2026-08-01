# Codex 与 Claude Code 的项目记忆发现、读取和管理

**日期：** 2026-07-30

**状态：** 外部产品与实现调研；用于 Engram 项目化记忆 UI 的需求判断，不是实现规格

**范围：** 只研究 OpenAI Codex 与 Anthropic Claude Code 的本地持久上下文机制。本文把“人维护的项目指令”和“工具自动积累的跨会话记忆”分开讨论，不把普通对话历史、ChatGPT 网页端记忆或 Claude.ai Projects 混入 Claude Code/Codex 本地记忆。

**证据标准：** 仅使用官方文档、官方仓库源码和官方 changelog。OpenAI 源码核对到 `openai/codex@f9b18d04ba78266b1e802ae2f85ff5ebea1e973a`（提交时间 2026-07-29）；Anthropic changelog 核对到 `anthropics/claude-code@7ef6eec9d9ba84ea6f233f26c45f1df5c5991843`（提交时间 2026-07-25）。所有链接的访问日期均为 2026-07-30。

## 结论

1. **两家的项目指令都已经分层，但加载边界不同。** Codex 以项目根到启动目录为界，启动时一次性拼接沿途的 `AGENTS.md`；Claude Code 从启动目录向上拼接 `CLAUDE.md`，又会在真正读取子目录文件时按需加入子目录指令和路径规则。
2. **Claude Code 的自动记忆是真正按仓库存储和选择的。** 每个 Git 仓库对应一个 `~/.claude/projects/<project>/memory/`，同一仓库的子目录和 worktree 共享它；启动只注入该仓库 `MEMORY.md` 的前 200 行或 25 KB，主题文件再按需读取。
3. **Codex 的本地记忆目前不是按项目物理隔离。** 所有项目的产物汇总在一个 `~/.codex/memories/`；`cwd/project` 是全局 `MEMORY.md` 和 `memory_summary.md` 内部的组织与检索标签。新线程读取的是同一份全局 `memory_summary.md`，当前源码没有用当前 `cwd` 先筛出一个项目分区。
4. **两家的现成 UI 都没有提供“跨项目记忆管理台”。** Claude Code 的 `/memory` 围绕当前会话列出指令位置并打开当前项目的自动记忆目录；Codex 的 `/memories` 和桌面设置只控制当前/后续聊天是否使用、生成记忆，以及全局重置。Codex TUI 源码中的菜单也只有这三个层面的动作，没有项目列表、项目筛选或逐项目删除。
5. **Engram 不应照搬 Codex 的全局平铺。** UI 应把 `workspace + project` 作为第一导航维度，把 global 作为显式叠加层；默认搜索、浏览、统计、导出和删除都约束在选中的项目，只有用户主动切换到“全部项目”时才跨项目聚合。

## 1. 先区分两种“记忆”

| 机制 | 谁写 | 主要用途 | 是否每次自动进入上下文 |
|---|---|---|---|
| 项目指令 | 人或团队 | 构建命令、代码规范、架构边界、工作方式 | 是，但按各自目录发现规则加载 |
| 自动记忆 | Agent/后台整理流程 | 从以往任务学到的事实、偏好、排错经验和工作状态 | 只自动注入入口摘要；详细内容按需读 |

Claude Code 官方明确把 `CLAUDE.md` 与 auto memory 称为两个互补系统：前者由人写，后者由 Claude 写；两者都是上下文而非强制配置。来源：[How Claude remembers your project](https://code.claude.com/docs/en/memory#claudemd-vs-auto-memory)（Anthropic，访问：2026-07-30）。

Codex 官方同样要求把必须遵守的团队规则放在 `AGENTS.md` 或仓库文档中，把 memories 只当作辅助召回层。来源：[Memories](https://learn.chatgpt.com/docs/customization/memories)（OpenAI，访问：2026-07-30）。

这个区分直接影响 UI：项目指令需要展示“实际加载链与优先级”，自动记忆需要展示“属于哪个项目、来自哪些会话、何时会被召回”。不能把二者混成一个无来源的 Markdown 列表。

## 2. OpenAI Codex

### 2.1 `AGENTS.md`：项目级指令如何发现和读取

Codex 在每次 run 开始时构造一次指令链；TUI 通常是在每个新会话开始时构造。官方顺序是：

1. 在 `CODEX_HOME`（默认 `~/.codex`）先找 `AGENTS.override.md`，否则找 `AGENTS.md`，这一层最多取一个非空文件。
2. 确定项目根（通常是 Git 根），从项目根向下走到当前工作目录；沿途每一级依次尝试 `AGENTS.override.md`、`AGENTS.md` 和配置的 fallback 文件名，每级最多取一个。
3. 按“根目录在前、当前目录在后”的顺序拼接；越靠近当前目录的内容越晚出现，因而用于覆盖较宽泛的上层指导。
4. 空文件跳过；合并内容达到 `project_doc_max_bytes` 后停止，默认总上限为 32 KiB。

来源：[Custom instructions with AGENTS.md](https://learn.chatgpt.com/docs/agent-configuration/agents-md#how-codex-discovers-guidance)（OpenAI，访问：2026-07-30）；源码也明确以默认 `.git` 标记找项目根、只收集“项目根到当前工作目录”链条，且不越过项目根：[agents_md.rs](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/core/src/agents_md.rs#L1-L16)（OpenAI，访问：2026-07-30）。

这意味着：

- 根 `AGENTS.md` 是整个仓库的共享规则；组件规则可以放在更深目录。
- 同级若有 `AGENTS.override.md`，普通 `AGENTS.md` 不会同时加载。
- Codex 的边界是“启动时的当前工作目录”。它不会像 Claude Code 那样因为后来读取了更深的文件，自动按需加载那个子目录里的 `AGENTS.md`。官方文档也明确说搜索到当前目录即停止。来源：[Layer project instructions](https://learn.chatgpt.com/docs/agent-configuration/agents-md#layer-project-instructions)（OpenAI，访问：2026-07-30）。
- 全局 `~/.codex/AGENTS.md` 是所有项目的显式叠加层，不属于任何一个项目。

### 2.2 自动记忆：生成、存储与读取

Codex 的本地 memories 默认关闭；开启后，后台流程从符合条件的历史线程中抽取记忆。它会跳过仍活跃或过短的会话，等待会话空闲后再整理，并在接近 rate limit 时跳过后台生成。来源：[How local Codex memories work](https://learn.chatgpt.com/docs/customization/memories#how-local-codex-memories-work)（OpenAI，访问：2026-07-30）。

官方源码把写入分成两阶段：

- Phase 1 按 thread 抽取结构化 `raw_memory` 和 rollout summary；
- Phase 2 使用一个**全局**锁，把选中的 thread 产物汇总到单一 memory workspace，并维护 `raw_memories.md`、`rollout_summaries/`、`MEMORY.md`、`memory_summary.md` 和 skills。

来源：[codex-rs/memories/README.md](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/memories/README.md#L29-L38) 与 [Phase 2: Global Consolidation](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/memories/README.md#L79-L100)（OpenAI，访问：2026-07-30）。

落盘根目录固定为 `CODEX_HOME/memories`。官方用户文档也只给出一个 `~/.codex/memories/`，其中同时包含 summaries、durable entries、recent inputs 和 supporting evidence。来源：[Local memory storage](https://learn.chatgpt.com/docs/customization/memories#local-memory-storage)（OpenAI，访问：2026-07-30）；本地 backend 也只以 `codex_home.join("memories")` 构造唯一根：[local.rs](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/ext/memories/src/local.rs#L24-L43)（OpenAI，访问：2026-07-30）。

`cwd/project` 并非完全丢失：全局 consolidation prompt 要求把 `memory_summary.md` 的索引先按 `cwd / project scope`、再按 topic 组织，并把可搜索的仓库名、路径和项目标签写进关键词。来源：[consolidation.md](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/memories/write/templates/memories/consolidation.md#L568-L596)（OpenAI，访问：2026-07-30）。但这是**一个全局文件中的内容级分组**，不是每项目独立目录或查询强制条件。

当前读取路径如下：

1. memories 开启且 `use_memories=true` 时，线程上下文 contributor 运行。
2. 它从唯一的 `CODEX_HOME/memories/memory_summary.md` 读取整份摘要，截断到 2,500 tokens 后作为 developer policy 注入。
3. 注入的读取说明要求模型先从该摘要提取当前任务关键词，再搜索全局 `MEMORY.md`；需要证据时再进入 `rollout_summaries/` 或 skills。
4. 新的专用 memories tools 可以 list/read/search，但请求只有 memory-root 内的相对 `path` 和文本 query，没有 `workspace`、`project` 或当前 `cwd` 作为强制 scope。

来源：[extension.rs](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/ext/memories/src/extension.rs#L33-L68)、[prompts.rs](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/ext/memories/src/prompts.rs#L23-L50)、[token limit and tool names](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/ext/memories/src/lib.rs#L11-L22)、[read_path.md](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/ext/memories/templates/memories/read_path.md#L19-L46) 与 [backend request schema](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/ext/memories/src/backend.rs#L44-L97)（OpenAI，访问：2026-07-30）。

因此，截至当前源码，Codex 的“按项目”主要依靠摘要里的项目标签和模型检索纪律，而不是存储路由或查询层的 fail-closed 隔离。一个项目的摘要会和其他项目一起进入相同的全局 `memory_summary.md` 预算；这正是 Engram 不应复制的平铺形态。

### 2.3 Codex 已有 UI 能管理到什么程度

官方文档说明：

- ChatGPT 桌面端在 Settings > Personalization 全局开启/关闭本地 memories；
- 桌面端和 Codex TUI 的 `/memories` 可决定当前 chat 能否使用已有记忆、能否成为未来记忆的输入；
- chat-level 设置不改变全局设置；
- 文档建议必要时检查生成文件，但不把手工编辑这些文件当作主要控制面。

来源：[Memories](https://learn.chatgpt.com/docs/customization/memories)（OpenAI，访问：2026-07-30）。

开源 TUI 菜单进一步确认只有三项：`Use memories`、`Generate memories`、`Reset all memories`；重置范围是当前整个 Codex home，而非一个项目。来源：[memories_settings_view.rs](https://github.com/openai/codex/blob/f9b18d04ba78266b1e802ae2f85ff5ebea1e973a/codex-rs/tui/src/bottom_pane/memories_settings_view.rs#L70-L130)（OpenAI，访问：2026-07-30）。

结论必须分两层表达：

- **可以确认：** 当前 Codex TUI 没有项目列表、项目筛选、项目详情、逐项目导出或逐项目删除。
- **对闭源桌面外壳只能按文档判断：** 官方文档只记载全局开关和 per-chat use/generate controls，没有记载跨项目记忆管理 UI。

## 3. Anthropic Claude Code

### 3.1 `CLAUDE.md`：用户、项目、本地与子目录作用域

Claude Code 官方列出四个常驻指令层：

| 层 | 默认位置 | 作用域 |
|---|---|---|
| Managed policy | macOS `/Library/Application Support/ClaudeCode/CLAUDE.md` 等 | 机器/组织 |
| User | `~/.claude/CLAUDE.md` | 当前用户的所有项目 |
| Project | `./CLAUDE.md` 或 `./.claude/CLAUDE.md` | 团队共享项目规则 |
| Local | `./CLAUDE.local.md` | 当前用户在当前 checkout 的私有项目规则 |

来源：[Choose where to put CLAUDE.md files](https://code.claude.com/docs/en/memory#choose-where-to-put-claudemd-files)（Anthropic，访问：2026-07-30）。

启动时，Claude Code 从当前工作目录向上检查 `CLAUDE.md` 与 `CLAUDE.local.md`，按文件系统根到当前目录的顺序拼接，而不是互相替换；同一级先 `CLAUDE.md`、后 `CLAUDE.local.md`。来源：[How CLAUDE.md files load](https://code.claude.com/docs/en/memory#how-claudemd-files-load)（Anthropic，访问：2026-07-30）。

与 Codex 的关键差异是子目录按需加载：Claude Code 在启动目录下面发现嵌套 `CLAUDE.md`，但不在启动时全部注入；当它实际读取该子目录中的文件时才加入上下文。`.claude/rules/*.md` 无 `paths` frontmatter 时启动加载，有 `paths` 时在读取匹配文件时触发。来源：[How CLAUDE.md files load](https://code.claude.com/docs/en/memory#how-claudemd-files-load) 与 [Path-specific rules](https://code.claude.com/docs/en/memory#path-specific-rules)（Anthropic，访问：2026-07-30）。

还需要注意：

- Claude Code 原生读 `CLAUDE.md`，不原生读 `AGENTS.md`；官方方案是在 `CLAUDE.md` 中 `@AGENTS.md` 或使用 symlink。来源：[AGENTS.md](https://code.claude.com/docs/en/memory#agentsmd)（Anthropic，访问：2026-07-30）。
- `@` import 在启动时展开进入上下文，最多递归四跳；它改善组织，不减少启动 token。项目文件首次导入工作目录外部内容时会出现批准对话框。来源：[Import additional files](https://code.claude.com/docs/en/memory#import-additional-files)（Anthropic，访问：2026-07-30）。
- `CLAUDE.md` 是上下文而非强制执行；必须阻止的动作要用 settings、sandbox 或 hooks。来源：[Manage CLAUDE.md for large teams](https://code.claude.com/docs/en/memory#manage-claudemd-for-large-teams)（Anthropic，访问：2026-07-30）。

### 3.2 Auto memory：真正的每仓库目录

Auto memory 自 Claude Code 2.1.59 引入，官方 changelog 直接写明会自动保存有用上下文并由 `/memory` 管理；2.1.63 又把项目配置和 auto memory 改为同一 Git 仓库的 worktree 共享。来源：[Claude Code changelog 2.1.59](https://github.com/anthropics/claude-code/blob/7ef6eec9d9ba84ea6f233f26c45f1df5c5991843/CHANGELOG.md#L3335-L3338) 与 [2.1.63](https://github.com/anthropics/claude-code/blob/7ef6eec9d9ba84ea6f233f26c45f1df5c5991843/CHANGELOG.md#L3298-L3302)（Anthropic，访问：2026-07-30）。

默认目录是：

```text
~/.claude/projects/<project>/memory/
├── MEMORY.md
├── debugging.md
├── api-conventions.md
└── ...
```

`<project>` 从 Git 仓库派生，所以同一仓库的 worktrees 与子目录共享一个 auto-memory 目录；不在 Git 仓库中时用项目根；文件只在本机保存，不跨机器或云环境同步。来源：[Storage location](https://code.claude.com/docs/en/memory#storage-location)（Anthropic，访问：2026-07-30）。

启动读取与按需读取分两层：

1. 每个新会话自动加载当前仓库 `MEMORY.md` 的前 200 行或前 25 KB，以先到者为限；YAML frontmatter 和块级 HTML 注释会先剥离，不计入这个限额。
2. `debugging.md`、`patterns.md` 等主题文件不在启动时加载，Claude 需要时用普通文件工具读取。
3. Claude 会在会话中读写这些记忆文件；带 frontmatter 的文件由新版本维护 `modified` 时间。

来源：[How it works](https://code.claude.com/docs/en/memory#how-it-works)（Anthropic，访问：2026-07-30）。

`autoMemoryDirectory` 可以改写存储位置；当前文档允许它来自 user、project、local、policy 或 `--settings`，但项目/本地设置只有通过 workspace trust 后才生效。来源：[Storage location](https://code.claude.com/docs/en/memory#storage-location)（Anthropic，访问：2026-07-30）；该设置最初加入于 2.1.74：[Claude Code changelog 2.1.74](https://github.com/anthropics/claude-code/blob/7ef6eec9d9ba84ea6f233f26c45f1df5c5991843/CHANGELOG.md#L3016-L3019)（Anthropic，访问：2026-07-30）。

由上述机制可以作出一个有边界的推论：默认配置下，一个仓库不会自动加载另一个仓库的 auto memory；用户层 `~/.claude/CLAUDE.md`、祖先 `CLAUDE.md` 和显式外部 import 仍会跨项目叠加，用户把多个项目显式指向同一个 `autoMemoryDirectory` 时也会主动打破默认隔离。

### 3.3 Claude Code 已有 UI 能管理到什么程度

`/memory` 会列出当前会话的 `CLAUDE.md`、`CLAUDE.local.md` 和其他 user/project scope 位置，允许切换 auto memory，并提供打开当前 auto-memory 文件夹的入口；`/context` 才显示当前实际已加载的文件。用户可以在编辑器中直接修改或删除 Markdown。来源：[View and edit with /memory](https://code.claude.com/docs/en/memory#view-and-edit-with-memory)（Anthropic，访问：2026-07-30）。

因此 Claude Code 已经具备“进入当前项目后管理当前项目”的入口，但本次核对的官方文档和 changelog没有提供一个列出全部仓库、比较项目记忆、跨项目搜索或逐项目批量治理的总览 UI。它解决的是当前项目定位与打开文件，不是多项目记忆管理台。

## 4. 对比表

| 维度 | Codex | Claude Code | 对 Engram 的含义 |
|---|---|---|---|
| 人工项目指令 | `AGENTS.md`；Git 根到启动 cwd；每级最多一份 | `CLAUDE.md`；祖先链拼接；子目录和 path rules 可按需加载 | UI 要单独展示实际指令链，不与自动记忆混排 |
| 全局指令 | `~/.codex/AGENTS.md` 或 override | `~/.claude/CLAUDE.md`、`~/.claude/rules/`、managed policy | global 应是独立节点和显式 overlay |
| 自动记忆根 | 单一 `~/.codex/memories/` | 每仓库 `~/.claude/projects/<project>/memory/` | Engram 应选择 Claude 式物理/逻辑项目边界 |
| 启动注入 | 全局 `memory_summary.md`，当前源码截到 2,500 tokens | 当前仓库 `MEMORY.md` 前 200 行或 25 KB | 项目摘要应独立生成和独立预算 |
| 详细读取 | 模型从全局 `MEMORY.md`、rollout summaries、skills 搜索；项目是内容标签 | 当前仓库 topic files 按需读取 | 查询 API 必须有强制 project scope，而不只是 prompt 提醒 |
| Worktree | 记忆中记录 cwd，但所有项目仍汇入全局池 | 同 Git 仓库 worktree 共享 auto memory | project identity 应以 canonical repo 为主，checkout 作为别名/来源 |
| 跨项目隔离 | 指令隔离较清楚；自动记忆未在存储/注入层隔离 | auto memory 默认按 repo 隔离；用户/祖先指令可叠加 | 分开实现 project memory 和 global preferences |
| 当前管理入口 | per-chat use/generate、全局开关、reset all | 当前会话 `/memory` 列出位置并打开当前项目目录 | Engram 的差异化机会是跨项目总览与严格作用域操作 |

## 5. Engram 项目化 UI 应形成的最小合同

### 5.1 导航必须项目优先

左侧第一层应是：

```text
Global
Workspaces
└── <workspace>
    ├── <project A>
    ├── <project B>
    └── <project C>
All projects  （显式进入，不是默认）
```

打开 Memory 页面时默认选择当前项目；没有当前项目上下文时，回到最近使用的项目或项目选择页，不直接进入全量平铺。

### 5.2 读取和操作都要带真实 scope

项目选择不只是前端 filter。列表、搜索、读取、统计、导出、lint、consolidate、forget 和 delete 请求都必须把 `workspace + project` 作为后端约束；缺少或只解析出一半 scope 时 fail closed。只有明确选择 Global 或 All projects 才扩大范围。

这比 Codex 当前“在全局摘要里写项目标题，再让模型自己挑”更可靠，也保留 Claude Code“当前仓库只加载当前仓库 auto memory”的核心优点。

### 5.3 每个项目详情页至少回答五个问题

1. **这是哪个项目？** workspace、project、canonical root、已知 checkout/worktree 别名。
2. **里面有什么？** wiki pages、rules/gotchas/procedures/decisions、sessions/observations、handoffs、pending proposals 的数量和最近更新时间。
3. **Agent 启动时会看到什么？** 项目摘要预览、自动注入预算、全局 overlay，以及实际项目指令文件链。
4. **为什么会召回这条？** 来源页面/会话、scope 命中、更新时间、检索方式和 provenance。
5. **我能怎么管？** 当前项目内搜索、编辑/删除单页、导入/导出、consolidate、lint；跨项目或 destructive 动作必须再次确认精确范围。

### 5.4 Global 与 Project 不应相互污染

- Global 只放稳定、确实跨项目的个人偏好与通用工作方式。
- 项目事实、路径、构建命令、服务拓扑和当前状态默认只能写入该项目。
- 项目页可以显示“继承了哪些 global 条目”，但不能把 global 条目复制进每个项目。
- 从 Project 晋升到 Global 应是显式 proposal，而不是后台 consolidation 的顺手归纳。

### 5.5 Worktree 需要“共享项目、保留来源”

采用 Claude Code 的有用语义：同一 Git 仓库的多个 worktree 默认归入一个项目记忆域；同时保留 observation 发生时的具体 checkout/cwd，避免把分支、临时路径和机器状态误当成全项目事实。对于没有 Git 的目录，应让用户确认项目 identity，而不是只用展示名称猜测合并。

## 6. 验收判断

第一版如果满足下面条件，就真正解决了“现在都是罗列在一起”的问题：

- 进入 Memory 默认只看到当前项目，页面上始终显示 scope。
- 切换项目后，列表、搜索结果、计数和可执行动作同步切换，不存在只换前端标签的假隔离。
- Global、单项目、All projects 三种范围视觉上和 API 上都可区分。
- 同一仓库的 worktrees 汇总到一个项目，但每条证据仍保留原 cwd/branch 来源。
- 项目摘要独立生成和预算，不让无关项目挤占启动上下文。
- 删除、导出、consolidate 等动作在确认框中显示准确的 workspace/project 和预计影响数量。
- UI 能展示项目指令文件与自动记忆的差别，以及 Agent 实际加载/按需读取的路径。

最小产品定位可以写成：

> Engram 把每个项目的记忆、来源和加载边界作为一等对象管理；默认只让 Agent 和用户看到当前项目，并以显式 Global 层承载真正跨项目的偏好。跨项目汇总是一种主动选择，不再是默认平铺。
