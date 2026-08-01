<script lang="ts">
  import { projectOverview, type Overview } from "./api";
  import { kindColor, kindLabel, relTime } from "./kinds";

  let {
    project,
    onOpenPage,
    onGotoKb,
    onGotoLayers,
    onGotoDaemon,
    onError,
  }: {
    project: string;
    onOpenPage: (path: string) => void;
    onGotoKb: () => void;
    onGotoLayers: () => void;
    onGotoDaemon: () => void;
    onError: (msg: string) => void;
  } = $props();

  let overview = $state<Overview | null>(null);
  let loading = $state(true);

  async function load(p: string) {
    loading = true;
    overview = null;
    try {
      overview = await projectOverview(p);
    } catch (e) {
      onError(`概览加载失败：${e}`);
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    load(project);
  });

  const healthRows = [
    { key: "stale", listKey: "stale_pages", label: "陈旧 stale", color: "var(--st-warn)" },
    { key: "duplicates", listKey: "duplicate_pages", label: "重复 duplicate", color: "var(--st-serious)" },
    { key: "orphans", listKey: "orphan_pages", label: "孤儿 orphan", color: "var(--st-critical)" },
  ] as const;
</script>

<div class="ph">
  <h1>概览</h1>
  <span class="sub">{project}{overview ? "" : loading ? " · 加载中…" : ""}</span>
  <div class="acts"><button class="btn" onclick={() => load(project)}>刷新</button></div>
</div>

{#if overview}
  {@const b = overview.briefing}
  {@const h = overview.health}
  {#if overview.handoff}
    {@const ho = overview.handoff}
    <div class="card handoff">
      <h3>📥 待接 handoff <span class="from">来自 {ho.agent} · {relTime(ho.at)}</span></h3>
      <div class="hsummary">{ho.summary}</div>
      {#if ho.next_steps?.length}
        <div class="hlist"><b>下一步</b>：{ho.next_steps.join("；")}</div>
      {/if}
      {#if ho.open_questions?.length}
        <div class="hlist"><b>待决</b>：{ho.open_questions.join("；")}</div>
      {/if}
      <div class="hnote">handoff 为单次使用，将由下一个 agent 会话的 SessionStart hook 自动接收；此处仅供查看。</div>
    </div>
  {/if}

  <div class="tiles">
    <div class="tile"><div class="v">{b.counts.pages_latest}</div><div class="l">页面（最新版）</div></div>
    <div class="tile"><div class="v">{b.counts.sessions}</div><div class="l">会话</div></div>
    <div class="tile"><div class="v">{b.counts.observations.toLocaleString()}</div><div class="l">观察</div></div>
    <div class="tile"><div class="v">{b.activity_7d.observations.toLocaleString()}</div><div class="l">近 7 天观察</div></div>
  </div>

  <div class="cols2">
    <div class="card">
      <h3>记忆健康 <button class="more" onclick={onGotoDaemon}>去清理 →</button></h3>
      {#each healthRows as row (row.key)}
        <details class="exp">
          <summary>
            <span class="dot" style="background:{row.color}"></span>{row.label}
            <span class="n">{h[row.key] ?? 0}</span>
          </summary>
          <div class="exp-body">
            {#each h[row.listKey] ?? [] as p (p.path)}
              <button onclick={() => onOpenPage(p.path)}>{p.title || p.path}</button>
            {:else}
              <span>无</span>
            {/each}
          </div>
        </details>
      {/each}
    </div>

    <div class="card">
      <h3>核心记忆（L3） <button class="more" onclick={onGotoLayers}>分层视图 →</button></h3>
      {#each [...b.slots, ...b.rules].slice(0, 5) as r (r.path)}
        <button class="rulebox rlink" onclick={() => onOpenPage(r.path)}>
          <div class="rp mono">{r.path}</div>
          {r.title}
        </button>
      {:else}
        <div class="hint">还没有 _rules/ 或 _slots/ 页——整理管线沉淀后会出现在这里。</div>
      {/each}
      {#if b.rules.length + b.slots.length > 5}
        <div class="hint">另有 {b.rules.length + b.slots.length - 5} 条 · 在分层视图查看全部</div>
      {/if}
    </div>
  </div>

  <div class="card">
    <h3>最近更新 <button class="more" onclick={onGotoKb}>知识库 →</button></h3>
    <div class="plist">
      {#each b.recent_pages.slice(0, 8) as p (p.path)}
        <button class="prow" onclick={() => onOpenPage(p.path)}>
          <span class="dot" style="background:{kindColor(p.kind)}"></span>
          <span class="t">{p.title || p.path}</span>
          <span class="kpill" style="color:{kindColor(p.kind)}">{kindLabel(p.kind)}</span>
          <span class="p mono">{p.path}</span>
          <span class="tm">{relTime(p.updated_at)}</span>
        </button>
      {/each}
    </div>
  </div>
{:else if !loading}
  <div class="empty"><div class="big">◌</div>概览不可用（daemon 不可达或项目为空）。</div>
{/if}

<style>
  .handoff {
    border-left: 3px solid var(--k-rule);
    margin-bottom: 4px;
  }

  .from {
    margin-left: auto;
    font-size: 11.5px;
    font-weight: 400;
    color: var(--accent);
  }

  .hsummary {
    font-size: 12.5px;
    color: var(--ink2);
  }

  .hlist {
    font-size: 12px;
    margin-top: 8px;
  }

  .hnote {
    font-size: 11px;
    color: var(--muted);
    margin-top: 8px;
  }

  .rlink {
    display: block;
    width: 100%;
    text-align: left;
    font: inherit;
    cursor: pointer;
    color: var(--ink);
  }

  .rlink:hover {
    border-color: var(--accent-border);
  }

  .hint {
    font-size: 11.5px;
    color: var(--muted);
    margin-top: 6px;
  }
</style>
