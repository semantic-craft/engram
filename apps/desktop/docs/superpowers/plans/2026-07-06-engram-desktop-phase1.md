# engram-desktop Phase 1 (本机·只读 MVP) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 一个 Tauri 桌面 app，连本机 engram daemon，做中文语义搜索 + 目录树浏览 + 页面查看(frontmatter/正文/backlinks) + daemon 状态条。

**Architecture:** Tauri v2（Rust 后端 + Svelte 前端）。Rust `api_client` 封装 engram 的 `/api/v1`(读) 和 `/mcp`(memory_query 语义搜索)，通过 Tauri command 暴露给前端；前端只通过 `invoke` 调后端，不直接发 HTTP。Phase 1 只连本机 `127.0.0.1:49374`，不做写/隧道/跨机。

**Tech Stack:** Tauri v2, Rust (reqwest, serde, tokio), Svelte + Vite, TypeScript。

---

## 前置：环境依赖（执行者先确认）

- Rust toolchain（`rustc --version`，需 1.77+，仓已装 1.95）。
- Node + npm（Tauri 前端脚手架用）。
- 本机 engram daemon 在跑（`curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:49374/mcp` → 405）。测试会用它做集成验证。
- `cargo install create-tauri-app`（或用 `npm create tauri-app`）。

## 文件结构（Phase 1）

```
engram-desktop/
├─ src-tauri/
│  ├─ Cargo.toml
│  ├─ tauri.conf.json
│  └─ src/
│     ├─ main.rs            — Tauri 入口 + command 注册
│     ├─ api_client.rs      — /api/v1 读 + /mcp 语义搜索（核心）
│     ├─ types.rs           — PageSummary / PageDetail / Hit / DaemonStatus
│     └─ commands.rs        — Tauri command 层（薄封装 api_client）
└─ src/ (前端 Svelte)
   ├─ App.svelte            — 三栏布局骨架 + 状态
   ├─ lib/api.ts            — invoke 封装（typed）
   ├─ lib/Sidebar.svelte    — 目录树
   ├─ lib/PageView.svelte   — 页面视图 + backlinks
   ├─ lib/SearchResults.svelte — 搜索结果
   └─ lib/StatusBar.svelte  — daemon 状态条
```

`api_client.rs` 是技术核心（MCP 握手 + 语义搜索），单独文件、单独测。`commands.rs` 只做 Tauri 绑定，保持薄。前端每个视图一个 `.svelte`，只经 `lib/api.ts` 与后端通信。

---

## Task 1: Scaffold Tauri + Svelte 项目

**Files:**
- Create: `src-tauri/`, `src/`, `package.json`, `vite.config.ts` (脚手架生成)

- [ ] **Step 1: 用官方脚手架生成 Tauri + Svelte-TS 项目到当前 repo**

Run (在 `~/Projects/engram-desktop`，注意别覆盖 `docs/`):
```bash
cd ~/Projects/engram-desktop
npm create tauri-app@latest . -- --template svelte-ts --manager npm --yes
```
若脚手架拒绝非空目录：生成到临时目录再把 `src/ src-tauri/ package.json vite.config.ts index.html` 等移进来，保留现有 `docs/ .git/`。

- [ ] **Step 2: 装依赖 + 确认能起**

Run:
```bash
npm install
npm run tauri dev
```
Expected: 弹出一个空 Tauri 窗口（默认模板）。确认后 Ctrl-C 退出。

- [ ] **Step 3: 加 Rust HTTP 依赖**

Modify `src-tauri/Cargo.toml`，在 `[dependencies]` 加：
```toml
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
```

- [ ] **Step 4: 确认编译**

Run: `cd src-tauri && cargo build`
Expected: 编译通过（拉取 reqwest 等）。

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore: scaffold tauri + svelte-ts project"
```

---

## Task 2: types.rs — 数据类型

**Files:**
- Create: `src-tauri/src/types.rs`

- [ ] **Step 1: 定义响应类型（对齐 /api/v1 与 memory_query）**

Create `src-tauri/src/types.rs`:
```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageSummary {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkRef {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageDetail {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    pub body: String,
    #[serde(default)]
    pub frontmatter: serde_json::Value,
    #[serde(default)]
    pub links: Vec<LinkRef>,
    #[serde(default)]
    pub backlinks: Vec<LinkRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(default)]
    pub rank: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonStatus {
    pub reachable: bool,
    #[serde(default)]
    pub page_count: Option<u64>,
}
```

- [ ] **Step 2: 编译确认**

Run: `cd src-tauri && cargo build`
Expected: PASS。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/types.rs
git commit -m "feat: response types for api_client"
```

---

## Task 3: api_client.rs — read (/api/v1) [TDD]

**Files:**
- Create: `src-tauri/src/api_client.rs`
- Test: 同文件 `#[cfg(test)]` 内联，集成测试打真实本机 daemon

- [ ] **Step 1: 写失败测试（list_pages + read_page 打本机 daemon）**

Create `src-tauri/src/api_client.rs`:
```rust
use crate::types::{DaemonStatus, Hit, PageDetail, PageSummary};

const BASE: &str = "http://127.0.0.1:49374";
const WS: &str = "default";
const PROJ: &str = "agent-memory";

pub struct ApiClient {
    http: reqwest::Client,
    base: String,
}

impl ApiClient {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new(), base: BASE.to_string() }
    }

    pub async fn list_pages(&self) -> Result<Vec<PageSummary>, String> {
        let url = format!("{}/api/v1/workspaces/{}/projects/{}/pages", self.base, WS, PROJ);
        let resp = self.http.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        serde_json::from_value(v["pages"].clone()).map_err(|e| e.to_string())
    }

    pub async fn read_page(&self, path: &str) -> Result<PageDetail, String> {
        let url = format!("{}/api/v1/workspaces/{}/projects/{}/pages/{}", self.base, WS, PROJ, path);
        let resp = self.http.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        resp.json().await.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_pages_returns_agent_memory_pages() {
        let c = ApiClient::new();
        let pages = c.list_pages().await.expect("daemon must be running on :49374");
        assert!(pages.len() >= 40, "expected ~46 pages, got {}", pages.len());
        assert!(pages.iter().any(|p| p.path.contains("anamra")));
    }

    #[tokio::test]
    async fn read_page_returns_body_and_backlinks() {
        let c = ApiClient::new();
        let pages = c.list_pages().await.unwrap();
        let target = pages.iter().find(|p| p.path.contains("qmd-self-healing")).expect("page exists");
        let detail = c.read_page(&target.path).await.expect("read ok");
        assert!(!detail.body.is_empty());
        assert!(detail.title.contains("qmd") || detail.title.len() > 0);
    }
}
```

- [ ] **Step 2: 在 main.rs 声明模块**

Modify `src-tauri/src/main.rs`，顶部加：
```rust
mod types;
mod api_client;
```

- [ ] **Step 3: 跑测试确认（需本机 daemon 在跑）**

Run: `cd src-tauri && cargo test list_pages_returns -- --nocapture`
Expected: PASS（若 FAIL 且报连接拒绝 → 先起 daemon）。

- [ ] **Step 4: 跑 read_page 测试**

Run: `cd src-tauri && cargo test read_page_returns -- --nocapture`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/api_client.rs src-tauri/src/main.rs
git commit -m "feat: api_client read (/api/v1 list_pages + read_page)"
```

---

## Task 4: api_client — 中文语义搜索 (MCP memory_query) [TDD]

**Files:**
- Modify: `src-tauri/src/api_client.rs`

技术要点（迁移时实测）：memory_query 只在 `/mcp`，要 MCP 握手（initialize → notifications/initialized → tools/call）；响应可能是纯 JSON 或 SSE（`data: ` 前缀），解析要兼容两者；查询带 `project`/`workspace`，**不用 `global`**。

- [ ] **Step 1: 写失败测试（中文语义召回）**

在 `src-tauri/src/api_client.rs` 的 `#[cfg(test)] mod tests` 里加：
```rust
    #[tokio::test]
    async fn semantic_search_recalls_chinese_by_paraphrase() {
        let c = ApiClient::new();
        let hits = c
            .semantic_search("qmd 索引因 node 升级坏了自动修复")
            .await
            .expect("search ok");
        assert!(!hits.is_empty(), "should recall something");
        assert!(
            hits.iter().take(3).any(|h| h.path.contains("qmd-self-healing")),
            "target page should be in top-3, got: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }
```

- [ ] **Step 2: 实现 semantic_search + MCP 握手 + 兼容解析**

在 `impl ApiClient` 里加：
```rust
    async fn mcp_call(&self, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let url = format!("{}/mcp", self.base);
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let text = resp.text().await.map_err(|e| e.to_string())?;
        // 兼容 SSE(data: ...) 与纯 JSON
        let payload = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .last()
            .map(|s| s.to_string())
            .unwrap_or(text);
        serde_json::from_str(&payload).map_err(|e| e.to_string())
    }

    pub async fn semantic_search(&self, query: &str) -> Result<Vec<Hit>, String> {
        // MCP 握手：initialize + initialized（stateless，无需 session id）
        let init = serde_json::json!({
            "jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},
                      "clientInfo":{"name":"engram-desktop","version":"0.1"}}
        });
        let _ = self.mcp_call(init).await?;
        let notified = serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        let _ = self.http.post(format!("{}/mcp", self.base))
            .header("Content-Type","application/json")
            .header("Accept","application/json, text/event-stream")
            .json(&notified).send().await.map_err(|e| e.to_string())?;
        // tools/call memory_query，带 project scope（不用 global）
        let call = serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_query","arguments":{
                "query": query, "project": PROJ, "workspace": WS, "limit": 10}}
        });
        let resp = self.mcp_call(call).await?;
        let text = resp["result"]["content"][0]["text"]
            .as_str().ok_or("no content in memory_query response")?;
        let parsed: serde_json::Value =
            serde_json::from_str(text).map_err(|e| e.to_string())?;
        serde_json::from_value(parsed["hits"].clone()).map_err(|e| e.to_string())
    }
```

- [ ] **Step 3: 跑测试确认中文召回**

Run: `cd src-tauri && cargo test semantic_search_recalls -- --nocapture`
Expected: PASS（qmd-self-healing 在 top-3）。若空 → 确认本机 daemon 有向量（`engram embed`）。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/api_client.rs
git commit -m "feat: api_client semantic_search via MCP memory_query"
```

---

## Task 5: api_client — daemon_status [TDD]

**Files:**
- Modify: `src-tauri/src/api_client.rs`

- [ ] **Step 1: 写失败测试**

在 tests 里加：
```rust
    #[tokio::test]
    async fn daemon_status_reachable_when_running() {
        let c = ApiClient::new();
        let st = c.daemon_status().await;
        assert!(st.reachable);
        assert!(st.page_count.unwrap_or(0) >= 40);
    }
```

- [ ] **Step 2: 实现 daemon_status（用 workspaces overview 拿页数 + 可达性）**

在 `impl ApiClient` 里加：
```rust
    pub async fn daemon_status(&self) -> DaemonStatus {
        let url = format!("{}/api/v1/workspaces/{}/projects/{}/overview?limit=1", self.base, WS, PROJ);
        match self.http.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                let pc = v["briefing"]["counts"]["pages_latest"].as_u64();
                DaemonStatus { reachable: true, page_count: pc }
            }
            _ => DaemonStatus { reachable: false, page_count: None },
        }
    }
```

- [ ] **Step 3: 跑测试**

Run: `cd src-tauri && cargo test daemon_status_reachable -- --nocapture`
Expected: PASS。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/api_client.rs
git commit -m "feat: api_client daemon_status"
```

---

## Task 6: commands.rs — Tauri command 层

**Files:**
- Create: `src-tauri/src/commands.rs`
- Modify: `src-tauri/src/main.rs`

- [ ] **Step 1: 写 command 薄封装**

Create `src-tauri/src/commands.rs`:
```rust
use crate::api_client::ApiClient;
use crate::types::{DaemonStatus, Hit, PageDetail, PageSummary};

#[tauri::command]
pub async fn list_pages() -> Result<Vec<PageSummary>, String> {
    ApiClient::new().list_pages().await
}

#[tauri::command]
pub async fn read_page(path: String) -> Result<PageDetail, String> {
    ApiClient::new().read_page(&path).await
}

#[tauri::command]
pub async fn semantic_search(query: String) -> Result<Vec<Hit>, String> {
    ApiClient::new().semantic_search(&query).await
}

#[tauri::command]
pub async fn daemon_status() -> DaemonStatus {
    ApiClient::new().daemon_status().await
}
```

- [ ] **Step 2: 注册 commands（main.rs）**

Modify `src-tauri/src/main.rs`：加 `mod commands;`，并在 `tauri::Builder` 上加：
```rust
.invoke_handler(tauri::generate_handler![
    commands::list_pages,
    commands::read_page,
    commands::semantic_search,
    commands::daemon_status,
])
```

- [ ] **Step 3: 编译确认**

Run: `cd src-tauri && cargo build`
Expected: PASS。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/commands.rs src-tauri/src/main.rs
git commit -m "feat: tauri command layer"
```

---

## Task 7: 前端 lib/api.ts — typed invoke 封装

**Files:**
- Create: `src/lib/api.ts`

- [ ] **Step 1: 写 typed 封装**

Create `src/lib/api.ts`:
```ts
import { invoke } from "@tauri-apps/api/core";

export interface PageSummary { path: string; title: string; kind?: string; tier?: string; updated_at?: string; }
export interface LinkRef { path: string; title: string; kind?: string; }
export interface PageDetail extends PageSummary { body: string; frontmatter: unknown; links: LinkRef[]; backlinks: LinkRef[]; }
export interface Hit { path: string; title: string; snippet?: string; rank?: number; }
export interface DaemonStatus { reachable: boolean; page_count?: number; }

export const listPages = () => invoke<PageSummary[]>("list_pages");
export const readPage = (path: string) => invoke<PageDetail>("read_page", { path });
export const semanticSearch = (query: string) => invoke<Hit[]>("semantic_search", { query });
export const daemonStatus = () => invoke<DaemonStatus>("daemon_status");
```

- [ ] **Step 2: Commit**

```bash
git add src/lib/api.ts
git commit -m "feat: frontend typed invoke wrappers"
```

---

## Task 8: 前端视图组件 + 三栏布局

**Files:**
- Create: `src/lib/Sidebar.svelte`, `src/lib/PageView.svelte`, `src/lib/SearchResults.svelte`, `src/lib/StatusBar.svelte`
- Modify: `src/App.svelte`

- [ ] **Step 1: Sidebar（目录树，按 path 顶层目录分组）**

Create `src/lib/Sidebar.svelte`:
```svelte
<script lang="ts">
  import type { PageSummary } from "./api";
  export let pages: PageSummary[] = [];
  export let onSelect: (path: string) => void;
  $: groups = pages.reduce((acc, p) => {
    const dir = p.path.includes("/") ? p.path.split("/")[0] : "(root)";
    (acc[dir] ??= []).push(p);
    return acc;
  }, {} as Record<string, PageSummary[]>);
</script>

<nav>
  {#each Object.entries(groups) as [dir, ps]}
    <div class="dir">{dir} <span>{ps.length}</span></div>
    {#each ps as p}
      <button class="page" on:click={() => onSelect(p.path)}>{p.title || p.path}</button>
    {/each}
  {/each}
</nav>
```

- [ ] **Step 2: PageView（正文 + frontmatter chips + backlinks）**

Create `src/lib/PageView.svelte`:
```svelte
<script lang="ts">
  import type { PageDetail } from "./api";
  export let page: PageDetail | null = null;
  export let onSelect: (path: string) => void;
</script>

{#if page}
  <article>
    <div class="crumb">{page.path}</div>
    <h1>{page.title}</h1>
    <div class="chips">
      {#if page.kind}<span>kind · {page.kind}</span>{/if}
      {#if page.tier}<span>tier · {page.tier}</span>{/if}
      {#if page.updated_at}<span>updated · {page.updated_at.slice(0,10)}</span>{/if}
    </div>
    <pre class="body">{page.body}</pre>
    {#if page.backlinks?.length}
      <div class="backlinks">
        <div class="bl-title">关联 · backlinks ({page.backlinks.length})</div>
        {#each page.backlinks as b}
          <button on:click={() => onSelect(b.path)}>{b.title || b.path}</button>
        {/each}
      </div>
    {/if}
  </article>
{:else}
  <div class="empty">选左侧一页，或上方搜索。</div>
{/if}
```

- [ ] **Step 3: SearchResults**

Create `src/lib/SearchResults.svelte`:
```svelte
<script lang="ts">
  import type { Hit } from "./api";
  export let hits: Hit[] = [];
  export let onSelect: (path: string) => void;
</script>

<div class="results">
  {#each hits as h}
    <button on:click={() => onSelect(h.path)}>
      <div class="r-title">{h.title || h.path}</div>
      {#if h.snippet}<div class="r-snip">{@html h.snippet}</div>{/if}
    </button>
  {/each}
</div>
```

- [ ] **Step 4: StatusBar**

Create `src/lib/StatusBar.svelte`:
```svelte
<script lang="ts">
  import type { DaemonStatus } from "./api";
  export let status: DaemonStatus | null = null;
</script>

<footer>
  <span class="dot" class:ok={status?.reachable}></span>
  {#if status?.reachable}
    本机 daemon · {status.page_count ?? "?"} 页
  {:else}
    本机 daemon 未运行
  {/if}
</footer>
```

- [ ] **Step 5: App.svelte 组装三栏 + 状态**

Modify `src/App.svelte`（替换模板内容）:
```svelte
<script lang="ts">
  import { onMount } from "svelte";
  import { listPages, readPage, semanticSearch, daemonStatus,
           type PageSummary, type PageDetail, type Hit, type DaemonStatus } from "./lib/api";
  import Sidebar from "./lib/Sidebar.svelte";
  import PageView from "./lib/PageView.svelte";
  import SearchResults from "./lib/SearchResults.svelte";
  import StatusBar from "./lib/StatusBar.svelte";

  let pages: PageSummary[] = [];
  let page: PageDetail | null = null;
  let hits: Hit[] = [];
  let mode: "browse" | "search" = "browse";
  let query = "";
  let status: DaemonStatus | null = null;

  onMount(async () => {
    status = await daemonStatus();
    if (status.reachable) pages = await listPages();
  });

  async function open(path: string) {
    page = await readPage(path);
    mode = "browse";
  }
  async function runSearch() {
    if (!query.trim()) return;
    hits = await semanticSearch(query);
    mode = "search";
  }
</script>

<div class="app">
  <header>
    <span class="machine">本机</span>
    <input placeholder="中文语义搜索…" bind:value={query}
           on:keydown={(e) => e.key === "Enter" && runSearch()} />
  </header>
  <div class="body">
    <aside><Sidebar {pages} onSelect={open} /></aside>
    <main>
      {#if mode === "search"}
        <SearchResults {hits} onSelect={open} />
      {:else}
        <PageView {page} onSelect={open} />
      {/if}
    </main>
  </div>
  <StatusBar {status} />
</div>

<style>
  .app { display: flex; flex-direction: column; height: 100vh; }
  header { display: flex; gap: 12px; align-items: center; padding: 8px 12px; border-bottom: 1px solid #444; }
  header input { flex: 1; padding: 6px 10px; }
  .body { display: flex; flex: 1; min-height: 0; }
  aside { width: 200px; overflow: auto; border-right: 1px solid #444; }
  main { flex: 1; overflow: auto; padding: 14px 18px; }
</style>
```

- [ ] **Step 6: 端到端手动验证**

Run: `npm run tauri dev`（本机 daemon 需在跑）
Expected: 窗口起来 → 左侧列出 ~46 页目录树 → 点一页看到正文+backlinks → 顶部输入"多个AI改同一仓库怎么防冲突"回车 → 结果列表含"并行会话竞态"页 → 点结果打开。底部状态条显示"本机 daemon · 46 页"。

- [ ] **Step 7: Commit**

```bash
git add src/
git commit -m "feat: phase-1 UI (sidebar/pageview/search/statusbar + 3-pane layout)"
```

---

## Task 9: daemon 离线降级 [TDD-lite]

**Files:**
- Modify: `src/App.svelte`（已用 `status.reachable` 守卫），`src/lib/StatusBar.svelte`

- [ ] **Step 1: 手动验证离线态**

停本机 daemon（`launchctl unload ~/Library/LaunchAgents/com.semantic-craft.engram.plist`），`npm run tauri dev`。
Expected: 状态条显示"本机 daemon 未运行"，不崩溃、不白屏。恢复：`launchctl load` 回来。

- [ ] **Step 2: 若崩溃/白屏，加 try/catch 守卫**

确认 `App.svelte` 的 `onMount` 里 `daemonStatus()` 不抛（Rust `daemon_status` 返回 `reachable:false` 而非 Err，已满足）。`listPages` 仅在 `reachable` 时调。若仍有未捕获错误，包 try/catch 显示提示。

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: graceful daemon-offline degradation"
```

---

## Phase 1 完成标准（Definition of Done）

- `cargo test`（api_client 4 个测试）全绿（本机 daemon 在跑时）。
- `npm run tauri dev` 起来：目录树浏览 + 中文语义搜索(top-3 命中) + 页面查看(正文/frontmatter/backlinks) + daemon 状态条 + 离线降级。
- 全程只读、只连本机、无隧道无写入。
- 跑几天体感评估，再决定是否进 Phase 2（编辑+管理）。

## 明确不在 Phase 1（YAGNI）

写入(/admin)、编辑器、daemon 启停/embed/backup 管理面板、SSH 隧道、机器切换、跨机聚合、mac+win 打包分发。全部 Phase 2/3。
