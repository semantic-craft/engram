<script lang="ts">
  import { onMount } from "svelte";
  import { listPages, readPage, semanticSearch, daemonStatus, runEmbed } from "$lib/api";
  import type { PageSummary, PageDetail, Hit, DaemonStatus } from "$lib/api";
  import Sidebar from "$lib/Sidebar.svelte";
  import PageView from "$lib/PageView.svelte";
  import SearchResults from "$lib/SearchResults.svelte";
  import MachinesPanel from "$lib/MachinesPanel.svelte";
  import PendingPanel from "$lib/PendingPanel.svelte";
  import StatusBar from "$lib/StatusBar.svelte";

  let pages = $state<PageSummary[]>([]);
  let page = $state<PageDetail | null>(null);
  let hits = $state<Hit[]>([]);
  let mode = $state<"browse" | "search" | "manage" | "pending">("browse");
  let query = $state("");
  let status = $state<DaemonStatus | null>(null);
  let errorMsg = $state<string | null>(null);
  let creating = $state(false);
  let showNewForm = $state(false);
  let newPath = $state("");

  function showError(e: unknown) {
    errorMsg = String(e);
  }

  async function refresh() {
    try {
      pages = await listPages();
    } catch (e) {
      showError(e);
    }
  }

  onMount(async () => {
    try {
      status = await daemonStatus();
      if (status.reachable) {
        pages = await listPages();
      }
    } catch (e) {
      showError(e);
    }
  });

  async function open(path: string) {
    try {
      page = await readPage(path);
      creating = false;
      mode = "browse";
    } catch (e) {
      showError(e);
    }
  }

  async function runSearch() {
    if (!query.trim()) return;
    try {
      hits = await semanticSearch(query);
      mode = "search";
    } catch (e) {
      showError(e);
    }
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === "Enter") runSearch();
  }

  function startCreate() {
    const p = newPath.trim();
    if (!p) return;
    // Empty title: the server derives one from the first H1 or path stem.
    page = { path: p, title: "", body: "", pinned: false, frontmatter: {}, links: [], backlinks: [] };
    creating = true;
    mode = "browse";
    showNewForm = false;
    newPath = "";
  }

  async function onSaved(path: string) {
    creating = false;
    await refresh();
    await open(path);
    // 数据流约定：写入成功后补 embedding，失败仅提示不阻塞。
    runEmbed(false, false).catch((e) => showError(`embed 失败：${e}`));
  }

  function onDeleted(_path: string) {
    page = null;
    creating = false;
    refresh();
  }

  function onCanceled() {
    if (creating) {
      page = null;
      creating = false;
    }
  }

  function toggleManage() {
    mode = mode === "manage" ? "browse" : "manage";
  }
</script>

<div class="app">
  <header class="header">
    <span class="machine-label">本机</span>
    <input
      class="search-input"
      type="text"
      placeholder="搜索..."
      bind:value={query}
      onkeydown={onKeydown}
    />
  </header>

  {#if errorMsg}
    <div class="error-banner">
      <span>{errorMsg}</span>
      <button onclick={() => (errorMsg = null)}>✕</button>
    </div>
  {/if}

  <div class="body">
    <aside class="sidebar-pane">
      <div class="sidebar-scroll">
        <Sidebar {pages} onSelect={open} />
      </div>
      <div class="sidebar-footer">
        {#if showNewForm}
          <input
            class="new-path"
            placeholder="路径，如 notes/foo.md"
            bind:value={newPath}
            onkeydown={(e) => e.key === "Enter" && startCreate()}
          />
          <div class="row">
            <button onclick={startCreate}>创建</button>
            <button
              onclick={() => {
                showNewForm = false;
                newPath = "";
              }}>取消</button
            >
          </div>
        {:else}
          <button onclick={() => (showNewForm = true)}>＋ 新建页</button>
        {/if}
        <button class:active={mode === "manage"} onclick={toggleManage}>⚙ Daemon 管理</button>
        <button class:active={mode === "pending"}
          onclick={() => (mode = mode === "pending" ? "browse" : "pending")}>✅ 审批台</button>
      </div>
    </aside>
    <main class="main-pane">
      {#if mode === "pending"}
        <PendingPanel onError={showError} />
      {:else if mode === "manage"}
        <MachinesPanel onSelect={open} onError={showError} />
      {:else if mode === "search"}
        <SearchResults {hits} onSelect={open} />
      {:else}
        <PageView
          {page}
          autoEdit={creating}
          onSelect={open}
          {onSaved}
          {onDeleted}
          {onCanceled}
          onError={showError}
        />
      {/if}
    </main>
  </div>

  <footer class="footer">
    <StatusBar {status} />
  </footer>
</div>

<style>
  :global(html, body) {
    margin: 0;
    padding: 0;
    height: 100%;
  }

  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
    font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
  }

  .header {
    display: flex;
    align-items: center;
    gap: 1rem;
    padding: 0.5rem 1rem;
    border-bottom: 1px solid rgba(0, 0, 0, 0.1);
    flex-shrink: 0;
  }

  .machine-label {
    font-weight: 600;
    font-size: 0.85rem;
    color: #555;
    white-space: nowrap;
  }

  .search-input {
    flex: 1;
    padding: 0.4rem 0.7rem;
    border-radius: 6px;
    border: 1px solid rgba(0, 0, 0, 0.15);
    font-size: 0.9rem;
  }

  .error-banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 0.4rem 1rem;
    background: rgba(192, 57, 43, 0.1);
    color: #c0392b;
    font-size: 0.85rem;
    flex-shrink: 0;
  }

  .error-banner button {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 0.85rem;
  }

  .body {
    display: flex;
    flex: 1;
    min-height: 0;
  }

  .sidebar-pane {
    width: 200px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    border-right: 1px solid rgba(0, 0, 0, 0.1);
  }

  .sidebar-scroll {
    flex: 1;
    overflow-y: auto;
    min-height: 0;
  }

  .sidebar-footer {
    flex-shrink: 0;
    border-top: 1px solid rgba(0, 0, 0, 0.1);
    padding: 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }

  .sidebar-footer button {
    font-size: 0.8rem;
    padding: 0.3rem 0.5rem;
    border-radius: 6px;
    border: 1px solid rgba(0, 0, 0, 0.15);
    background: none;
    cursor: pointer;
    text-align: left;
  }

  .sidebar-footer button:hover {
    background: rgba(0, 0, 0, 0.06);
  }

  .sidebar-footer button.active {
    background: rgba(57, 108, 216, 0.12);
    border-color: rgba(57, 108, 216, 0.5);
  }

  .sidebar-footer .new-path {
    font-size: 0.8rem;
    padding: 0.3rem 0.5rem;
    border-radius: 6px;
    border: 1px solid rgba(0, 0, 0, 0.15);
    box-sizing: border-box;
    width: 100%;
  }

  .sidebar-footer .row {
    display: flex;
    gap: 0.35rem;
  }

  .sidebar-footer .row button {
    flex: 1;
    text-align: center;
  }

  .main-pane {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
  }

  .footer {
    border-top: 1px solid rgba(0, 0, 0, 0.1);
    flex-shrink: 0;
  }
</style>
