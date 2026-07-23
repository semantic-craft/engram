# engram-desktop — 设计 spec

> 日期：2026-07-06 ｜ 状态：待用户 review
> **2026-07-18 修订**：三机拓扑已定案为单一 server（单一宿主机）。Phase 3 的「三机对等 + SSH 隧道」
> 范围相应改写：Tunnel Manager 降级为「远端 server 连接配置」（URL +
> bearer token + 连接状态），机器切换器退役；「三机健康聚合」退化为单
> server 健康 + 各机 hook spool 积压可见性。Phase 1/2 内容不受影响。
> 一句话：一个跨平台（mac + win）桌面 app，把三机 semantic-craft/engram 的操作记忆做成"搜/看 + 编辑 + 三机管理"的工作台。

## 1. 目标与非目标

**目标**
- 中文语义搜索 + 浏览三机的 agent-memory 记忆（页面、frontmatter、关联 backlinks）。
- 就地编辑记忆页、新建/删页。
- 管理三机 daemon：健康、启停、补 embedding、backup/restore 同步、清 stale/duplicate/orphan。
- 三机对等：每台机器各装一份，默认连本机（独立、快、安全），按需连 peer 看别机。

**非目标（YAGNI）**
- 不做多用户 / 团队协作（单人、三机私有）。
- 不做 auto-improve / consolidation 的 UI（当前无 LLM provider，不跑；将来需要再说）。
- 不重实现记忆引擎——app 只是 engram 的前端，引擎仍是 engram daemon。
- 不做发布/商店分发（个人工具）。
- Phase 1 明确不碰：写入、SSH 隧道、跨机。

## 2. 技术栈

**Tauri**（Rust 后端 + web 前端）。理由：
- 唯一同时满足"原生桌面 app + 能管 SSH 隧道/daemon 进程 + 跨平台 mac/win + 和 engram 同为 Rust"。
- 前端：React 或 Svelte（实现阶段定；倾向 Svelte，轻）。
- app 体积小（~15–30MB），远小于 Electron。

## 3. 架构（三层）

```
前端 web UI（搜索 / 浏览 / 编辑 / 三机状态）
        │  Tauri invoke (IPC)
Tauri 核心（Rust）
  ├─ API Client      — /api/v1 读 · /admin 写 · /mcp memory_query 语义
  ├─ Tunnel Manager  — 按需 ssh -L 到 peer，切机时建/收隧道
  └─ Daemon Manager  — 本机 daemon 启停 · 健康 · embed · backup/restore
        │
daemon 层（loopback-only 127.0.0.1:49374）
  本机（直连）  ·  host-b（经隧道）  ·  host-c（经隧道）
```

## 4. engram 接口用法（关键约束，来自 docs/frontend-api.md 实读）

- **读**：`/api/v1/*`（只读）——workspaces / projects / pages 列表 / page 全文(含 frontmatter + links + backlinks) / recent / briefing / overview / cross-project graph / memory_health(stale/duplicate/orphan)。
- **写**：`/admin/*`（`/api/v1` 是 read-only by construction）——write-page 等。
- **中文语义搜索**：必须走 **MCP `memory_query`**（`/mcp`）。`/api/v1/search` 是 FTS5 unicode61，**对中文失效**——只用于英文/ASCII 关键词或 slug。查询要带 `project`（scope），**不用 `global`**（有 bug 返回空）。
- **auth**：loopback 单用户，当前无 token（`/api/v1` 无 token 时可直接读）。若将来加 token，API Client 统一带 `Authorization: Bearer`。
- **三机 embedding 三元组**已一致（`openai / text-embedding-v4 / 1024`）——app 不改它，只读写页；补 embedding 走 Daemon Manager 调 `engram embed`。

## 5. 跨机拓扑（三机对等）

- 每台机器装同一个 app，配置里列出另外两台为 peer（复用现成 SSH host 别名，如 `host-b` / `host-c`）。
- 切到 peer 时，Tunnel Manager `ssh -L <本地端口>:127.0.0.1:49374 <peer>`，API Client 连该本地端口 = peer 的 loopback daemon；平时不建隧道。
- daemon 全保持 loopback-only，不对外暴露。远端机异地时隧道走 SSH（能通）。

## 6. UI 布局（三栏 + 顶栏 + 状态栏）

- **顶栏**：机器切换（本机/其它机器，带 daemon 状态点）+ 中文语义搜索框。
- **左栏**：目录树（decisions/preferences/projects/workflows/people/snippets，带计数）+ 底部"三机健康"入口 + 设置。
- **主区**：页面视图（breadcrumb + 标题 + frontmatter chips + 正文 + backlinks），右上"编辑"按钮就地切 markdown 编辑器。
- **底部状态栏**：三机 daemon 健康 + 当前 embedding 模型。
- 次要视图：三机健康/管理面板（Daemon Manager 的 UI）。

## 7. 模块划分

**Rust 核心**
- `api_client`：封装 read(`/api/v1`) / write(`/admin`) / semantic_search(`/mcp memory_query`)，按当前选中机器路由到本机端口或 peer 隧道端口。输入输出用 engram 的响应类型（能复用则复用）。
- `tunnel_manager`：peer 的 ssh -L 生命周期（建/查/收），端口分配，健康探测。
- `daemon_manager`：本机 daemon 启停（launchd/systemd 交互）、`engram status/embed/backup/restore` 封装、health(stale/duplicate/orphan) 拉取。
- Tauri command 层：把上述能力暴露给前端 `invoke`。

**前端**
- 视图：Search、Browse(tree+page)、Editor、Machines(health)。
- 状态：当前机器、当前页、搜索结果、三机健康。每个视图组件独立、只通过 Tauri command 与后端通信。

## 8. 数据流（举例）

1. 用户在搜索框输中文 → 前端 `invoke("semantic_search", {q, machine})` → `api_client` 判断 machine=本机→直连；=peer→确保隧道在 → POST `/mcp` `memory_query {query:q, project:"agent-memory"}` → 返回 hits → 前端渲染结果列表。
2. 点结果 → `invoke("read_page", {machine, path})` → `/api/v1/.../pages/{path}` → 渲染正文 + backlinks。
3. 点编辑→改→存 → `invoke("write_page", {machine, path, body})` → `/admin/write-page` → 成功后可选触发 `embed`。

## 9. 错误处理

- **daemon 离线**：`/mcp` 查询会 Connection refused → 前端显示"本机 daemon 未运行"+ 一键"启动"（Daemon Manager）。
- **隧道失败**（peer 不可达/SSH 断）：切机时探测失败 → 提示"peer 不可达"，回退本机，不阻塞。
- **写冲突/失败**：`/admin` 返回非 2xx → 展示错误消息，不静默吞。
- **中文搜索空**：区分"真无结果" vs "scope 错/daemon 无向量"——若 `/mcp` 正常但 hits 空，提示可能未 embed。
- **token（若启用）**：401 → 提示重新填 token。

## 10. 测试策略

- Rust 核心：`api_client` / `tunnel_manager` / `daemon_manager` 各写单元测试（对 daemon 用本地起的临时 data-dir daemon 做集成测试，复用迁移时验证过的 scratch-daemon 手法）。
- 关键回归：中文语义搜索走 memory_query 带 project、不走 global；写走 /admin；隧道建/收幂等。
- 前端：视图组件对 mock 的 Tauri command 做基本渲染测试。
- 不追求高覆盖率数字——覆盖三条 API 路径 + 隧道 + daemon 离线降级即可。

## 11. 分阶段

| 阶段 | 范围 | 交付 |
|---|---|---|
| **Phase 1 · MVP（本机 · 只读）** | Tauri 壳 + 前端；连本机 daemon；中文语义搜索(memory_query) + 目录树 + 页面查看(frontmatter/正文/backlinks) + 本机 daemon 状态条。不碰写/隧道/跨机。 | 一个能用的本机记忆浏览器，验证价值 |
| **Phase 2 · 编辑 + 本机管理** | markdown 编辑器(写 /admin) + 新建/删页；Daemon Manager 面板：启停、补 embedding、stale/duplicate 清理、backup/restore。 | 本机可读可写可管 |
| **Phase 3 · 跨机（三机对等）** | Tunnel Manager 按需隧道 + 机器切换 + 跨机浏览/搜索/健康聚合；mac+win 跨平台打包分发到各机器（Windows 走 WSL）。 | 三机对等工作台 |

每阶段独立可用，跑通再决定下一阶段。Phase 1 是止损点：不好用就停，不白干。

## 12. 待实现阶段再定的小决策

- 前端框架 React vs Svelte（倾向 Svelte）。
- daemon 启停在 Rust 里调 launchctl/systemctl 的确切封装。
- peer 隧道端口分配策略（固定 49375/49376 vs 动态）。
- 编辑器用什么 markdown 组件。
