<script lang="ts">
  import { onMount } from "svelte";
  import {
    adminStatus,
    daemonStatus,
    listPages,
    listProjectsStats,
    pendingQueue,
    readPage,
    runEmbed,
    semanticSearch,
    type DaemonStatus,
    type Hit,
    type PageDetail,
    type PageSummary,
    type ProjectSummary,
  } from "$lib/api";
  import Dashboard from "$lib/Dashboard.svelte";
  import Instructions from "$lib/Instructions.svelte";
  import KnowledgeBase from "$lib/KnowledgeBase.svelte";
  import Layers from "$lib/Layers.svelte";
  import MachinesPanel from "$lib/MachinesPanel.svelte";
  import NewPageModal from "$lib/NewPageModal.svelte";
  import PageView from "$lib/PageView.svelte";
  import PendingPanel from "$lib/PendingPanel.svelte";
  import ProjectsOverview from "$lib/ProjectsOverview.svelte";
  import ProjectSwitcher from "$lib/ProjectSwitcher.svelte";
  import SearchResults from "$lib/SearchResults.svelte";
  import Sessions from "$lib/Sessions.svelte";

  type View =
    | "overview"
    | "global"
    | "pending"
    | "dash"
    | "layers"
    | "kb"
    | "sessions"
    | "page"
    | "instructions"
    | "search"
    | "daemon";

  const PROJECT_KEY = "engram.scope.project";
  const GLOBAL = "_global";

  let view = $state<View>("dash");
  let prevView = $state<View>("dash");
  let project = $state(localStorage.getItem(PROJECT_KEY) ?? "");
  let projects = $state<ProjectSummary[]>([]); // 不含 _global
  let pages = $state<PageSummary[]>([]);
  let globalPages = $state<PageSummary[]>([]);
  let page = $state<PageDetail | null>(null);
  let pageProject = $state(GLOBAL);
  let creating = $state(false);
  let hits = $state<Hit[]>([]);
  let query = $state("");
  let lastQuery = $state("");
  let searchGlobal = $state(false);
  let status = $state<DaemonStatus | null>(null);
  let version = $state("");
  let errorMsg = $state<string | null>(null);
  let pendingCount = $state(0);
  let kbFilter = $state("all");
  let globalFilter = $state("all");
  let newPageOpen = $state(false);
  let newPageTarget = $state(GLOBAL);
  let searchEl = $state<HTMLInputElement | null>(null);

  let repoPath = $derived(
    projects.find((p) => p.project_name === project)?.repo_path ?? null,
  );

  const showError = (e: unknown) => (errorMsg = String(e));

  async function loadProjects() {
    const all = await listProjectsStats();
    projects = all.filter((p) => p.project_name !== GLOBAL);
    if (!project || !all.some((p) => p.project_name === project)) {
      project = projects[0]?.project_name ?? GLOBAL;
    }
  }

  async function loadPages() {
    pages = project ? await listPages(project) : [];
  }

  async function loadGlobalPages() {
    globalPages = await listPages(GLOBAL);
  }

  function refreshPending() {
    pendingQueue()
      .then((groups) => (pendingCount = groups.reduce((n, g) => n + g.proposals.length, 0)))
      .catch(() => {});
  }

  onMount(async () => {
    try {
      status = await daemonStatus(project || undefined);
      await loadProjects();
      await Promise.all([loadPages(), loadGlobalPages()]);
    } catch (e) {
      showError(e);
    }
    refreshPending();
    adminStatus()
      .then((s) => (version = String(s.version ?? "")))
      .catch(() => {});
  });

  function goto(v: View) {
    if (view === "page") prevView = "kb";
    view = v;
  }

  function setProject(name: string) {
    if (name === project) return;
    project = name;
    localStorage.setItem(PROJECT_KEY, name);
    pages = [];
    kbFilter = "all";
    loadPages().catch(showError);
    // 全局区视图保持不动，项目区/页面视图回到项目概览。
    if (!["overview", "global", "pending", "daemon"].includes(view)) view = "dash";
  }

  async function openPage(path: string, proj?: string) {
    const target = proj ?? (view === "global" ? GLOBAL : project);
    try {
      page = await readPage(path, target);
      pageProject = target;
      creating = false;
      if (view !== "page") prevView = view;
      view = "page";
    } catch (e) {
      showError(e);
    }
  }

  function runSearch(globalScope: boolean) {
    const q = (view === "search" ? lastQuery : query).trim() || query.trim();
    if (!q) return;
    searchGlobal = globalScope;
    semanticSearch(q, globalScope ? { global: true } : { project })
      .then((h) => {
        hits = h;
        lastQuery = q;
        if (view !== "search") prevView = view;
        view = "search";
      })
      .catch(showError);
  }

  function startCreate(path: string, kind: string) {
    newPageOpen = false;
    page = {
      path,
      title: "",
      kind,
      body: "",
      pinned: false,
      frontmatter: {},
      links: [],
      backlinks: [],
    };
    pageProject = newPageTarget;
    creating = true;
    if (view !== "page") prevView = view;
    view = "page";
  }

  async function onSaved(path: string) {
    creating = false;
    try {
      await (pageProject === GLOBAL ? loadGlobalPages() : loadPages());
      await openPage(path, pageProject);
    } catch (e) {
      showError(e);
    }
    // 数据流约定：写入成功后补 embedding，失败仅提示不阻塞。
    runEmbed(false, false, pageProject).catch((e) => showError(`embed 失败：${e}`));
  }

  function onDeleted(_path: string) {
    page = null;
    creating = false;
    (pageProject === GLOBAL ? loadGlobalPages() : loadPages()).catch(showError);
    view = prevView;
  }

  /** 移动完成：两侧列表都要刷新，然后在新作用域重新打开这一页。 */
  async function onPageMoved(path: string, toProject: string) {
    try {
      await Promise.all([loadPages(), loadGlobalPages()]);
      await openPage(path, toProject);
    } catch (e) {
      showError(e);
    }
  }

  function onCanceled() {
    if (creating) {
      page = null;
      creating = false;
      view = prevView;
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      searchEl?.focus();
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="app">
  <div class="body">
    <aside class="sb">
      <div class="sbtop">
        <span class="machine" title="多机切换在二期提供">本机</span>
        <ProjectSwitcher {projects} current={project} onSelect={setProject} />
      </div>

      <div class="grp">全局</div>
      <button class="sbi" class:on={view === "overview"} onclick={() => goto("overview")}>
        项目总览
      </button>
      <button class="sbi" class:on={view === "global"} onclick={() => goto("global")}>
        全局记忆
        {#if globalPages.length}<span class="badge amber">{globalPages.length}</span>{/if}
      </button>
      <button class="sbi" class:on={view === "pending"} onclick={() => goto("pending")}>
        审批台
        {#if pendingCount}<span class="badge">{pendingCount}</span>{/if}
      </button>

      <div class="grp">项目 · {project}</div>
      <button class="sbi" class:on={view === "dash"} onclick={() => goto("dash")}>概览</button>
      <button class="sbi" class:on={view === "layers"} onclick={() => goto("layers")}>
        记忆分层
      </button>
      <button class="sbi" class:on={view === "kb"} onclick={() => goto("kb")}>
        知识库
        {#if pages.length}<span class="badge dim">{pages.length}</span>{/if}
      </button>
      <button class="sbi" class:on={view === "sessions"} onclick={() => goto("sessions")}>
        会话与交接
      </button>
      <button class="sbi" class:on={view === "instructions"} onclick={() => goto("instructions")}>
        指令文件
      </button>

      <div class="grp">系统</div>
      <button class="sbi" class:on={view === "daemon"} onclick={() => goto("daemon")}>
        Daemon 管理
      </button>
    </aside>

    <main class="main">
      <div class="mainsearch">
        <input
          bind:this={searchEl}
          bind:value={query}
          type="text"
          placeholder={`搜索 ${project || "…"}  (⌘K · Enter 搜索)`}
          onkeydown={(e) => e.key === "Enter" && query.trim() && runSearch(false)}
        />
      </div>

      {#if errorMsg}
        <div class="error-banner">
          <span>{errorMsg}</span>
          <button onclick={() => (errorMsg = null)}>✕</button>
        </div>
      {/if}

      <div class="viewpad">
        {#if view === "overview"}
          <ProjectsOverview
            {projects}
            {pendingCount}
            onOpen={(name) => {
              setProject(name);
              view = "dash";
            }}
            onRefresh={() => {
              loadProjects().catch(showError);
              refreshPending();
            }}
          />
        {:else if view === "global"}
          <KnowledgeBase
            title="全局记忆"
            scopeLabel="default / _global"
            pages={globalPages}
            banner="这里的页面作为 global_scope_hits 自动并入每个项目的 memory_query 结果——只放跨项目长期有效的偏好与红线，不放项目细节。"
            bind:kindFilter={globalFilter}
            onOpen={(p) => openPage(p, GLOBAL)}
            onNew={() => {
              newPageTarget = GLOBAL;
              newPageOpen = true;
            }}
          />
        {:else if view === "pending"}
          <div class="ph">
            <h1>审批台</h1>
            <span class="sub">全部项目 · {pendingCount} 条待审</span>
          </div>
          <p class="desc">
            auto-improve 与整理管线暂存（staged）的提案；通过后写入 wiki，驳回理由会回流给后续提案。
          </p>
          <PendingPanel onError={showError} />
        {:else if view === "dash"}
          <Dashboard
            {project}
            onOpenPage={(p) => openPage(p)}
            onGotoKb={() => goto("kb")}
            onGotoLayers={() => goto("layers")}
            onGotoDaemon={() => goto("daemon")}
            onError={showError}
          />
        {:else if view === "layers"}
          <Layers
            {project}
            {pages}
            onOpenPage={(p) => openPage(p)}
            onGotoKb={(k) => {
              kbFilter = k ?? "all";
              goto("kb");
            }}
            onError={showError}
          />
        {:else if view === "kb"}
          <KnowledgeBase
            title="知识库"
            scopeLabel={project}
            {pages}
            bind:kindFilter={kbFilter}
            onOpen={(p) => openPage(p)}
            onNew={() => {
              newPageTarget = project;
              newPageOpen = true;
            }}
          />
        {:else if view === "sessions"}
          <Sessions {project} onOpenPage={(p) => openPage(p)} onError={showError} />
        {:else if view === "instructions"}
          <Instructions {project} {repoPath} onError={showError} />
        {:else if view === "search"}
          <SearchResults
            {hits}
            query={lastQuery}
            globalScope={searchGlobal}
            onSelect={(p, proj) => openPage(p, proj)}
            onScopeChange={(g) => runSearch(g)}
          />
        {:else if view === "daemon"}
          <div class="ph">
            <h1>Daemon 管理</h1>
            <span class="sub">本机</span>
          </div>
          <MachinesPanel {project} onSelect={(p) => openPage(p)} onError={showError} />
        {:else if view === "page"}
          <div class="crumb">
            <button class="back" onclick={() => (view = prevView)}>← 返回</button>
            <span>{pageProject}</span>
            <span>/</span>
            <span class="mono">{page?.path}</span>
          </div>
          <PageView
            {page}
            project={pageProject}
            homeProject={project}
            autoEdit={creating}
            onSelect={(p) => openPage(p, pageProject)}
            {onSaved}
            {onDeleted}
            {onCanceled}
            onMoved={onPageMoved}
            onError={showError}
          />
        {/if}
      </div>
    </main>
  </div>

  <footer class="statusbar">
    <span class="sd" class:bad={status != null && !status.reachable}></span>
    <span>daemon{version ? ` v${version}` : ""}{status && !status.reachable ? " · 不可达" : ""}</span>
    <span>项目 <b>{project}</b> · {pages.length} 页</span>
  </footer>
</div>

{#if newPageOpen}
  <NewPageModal
    project={newPageTarget}
    onCreate={startCreate}
    onCancel={() => (newPageOpen = false)}
  />
{/if}

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  .body {
    flex: 1;
    display: flex;
    min-height: 0;
    gap: 10px;
    padding: 12px 14px 4px 12px;
  }

  /* ── 侧栏浮岛 ── */
  .sb {
    width: 208px;
    flex-shrink: 0;
    overflow-y: auto;
    padding: 10px 8px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 16px;
    box-shadow: var(--island-shadow);
  }

  .sbtop {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 2px 4px 12px;
    border-bottom: 1px solid var(--border);
    margin-bottom: 8px;
  }

  .machine {
    font-size: 12px;
    font-weight: 600;
    color: var(--ink2);
    padding: 6px 11px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--surface);
    text-align: center;
  }

  .grp {
    font-size: 10.5px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--muted);
    padding: 12px 10px 4px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .sbi {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    text-align: left;
    font: inherit;
    font-size: 13px;
    padding: 6px 10px;
    border: none;
    border-radius: 7px;
    background: none;
    color: var(--ink);
    cursor: pointer;
  }

  .sbi:hover {
    background: var(--hover);
  }

  .sbi.on {
    background: var(--accent-weak);
    color: var(--accent);
    font-weight: 650;
  }

  .badge {
    margin-left: auto;
    font-size: 11px;
    font-weight: 650;
    padding: 1px 7px;
    border-radius: 99px;
    background: var(--accent-weak);
    color: var(--accent);
  }

  .badge.amber {
    background: rgba(237, 161, 0, 0.15);
    color: var(--k-rule);
  }

  .badge.dim {
    background: var(--hover);
    color: var(--muted);
  }

  /* ── 主内容浮岛 ── */
  .main {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 16px;
    box-shadow: var(--island-shadow);
  }

  .mainsearch {
    position: sticky;
    top: 0;
    z-index: 10;
    padding: 16px 26px 12px;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
    border-radius: 16px 16px 0 0;
    display: flex;
    justify-content: center;
  }

  .mainsearch input {
    width: 100%;
    max-width: 640px;
    font: inherit;
    font-size: 13px;
    padding: 9px 18px;
    border: 1px solid var(--border);
    border-radius: 99px;
    background: var(--card);
    color: var(--ink);
  }

  .mainsearch input:focus {
    outline: 2px solid var(--accent-border);
    outline-offset: -1px;
  }

  .viewpad {
    padding: 20px 26px 40px;
    max-width: 1060px;
  }

  .error-banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    margin: 10px 26px 0;
    padding: 0.4rem 1rem;
    background: rgba(208, 59, 59, 0.1);
    color: var(--st-critical);
    font-size: 0.85rem;
    border-radius: 9px;
  }

  .error-banner button {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 0.85rem;
  }

  /* ── 状态栏（画布上，无边框） ── */
  .statusbar {
    flex-shrink: 0;
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 6px 22px;
    font-size: 11.5px;
    color: var(--ink2);
  }

  .sd {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--st-good);
  }

  .sd.bad {
    background: var(--st-critical);
  }

  /* ── 页面视图面包屑 ── */
  .crumb {
    font-size: 12px;
    color: var(--muted);
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 4px;
  }

  .crumb .back {
    font: inherit;
    font-size: 12px;
    color: var(--accent);
    background: none;
    border: none;
    cursor: pointer;
    padding: 0;
  }

  .crumb .back:hover {
    text-decoration: underline;
  }
</style>
