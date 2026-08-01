<script lang="ts">
  import { projectOverview, type ProjectSummary } from "./api";
  import { relTime } from "./kinds";

  let {
    projects,
    pendingCount,
    onOpen,
    onRefresh,
  }: {
    projects: ProjectSummary[];
    pendingCount: number;
    onOpen: (name: string) => void;
    onRefresh: () => void;
  } = $props();

  // 每卡懒取 overview：健康计数 + 是否有待接 handoff。
  let extras = $state<Record<string, { health: number; handoff: boolean }>>({});

  $effect(() => {
    for (const p of projects) {
      const name = p.project_name;
      if (name in extras) continue;
      projectOverview(name)
        .then((o) => {
          const h = o.health ?? { stale: 0, duplicates: 0, orphans: 0 };
          extras[name] = {
            health: (h.stale ?? 0) + (h.duplicates ?? 0) + (h.orphans ?? 0),
            handoff: o.handoff != null,
          };
        })
        .catch(() => {
          extras[name] = { health: 0, handoff: false };
        });
    }
  });

  let totalPages = $derived(projects.reduce((n, p) => n + p.page_count, 0));
  let handoffTotal = $derived(Object.values(extras).filter((e) => e.handoff).length);
</script>

<div class="ph">
  <h1>项目总览</h1>
  <span class="sub">default workspace · {projects.length} 个项目</span>
  <div class="acts"><button class="btn" onclick={onRefresh}>刷新</button></div>
</div>

<div class="tiles">
  <div class="tile"><div class="v">{projects.length}</div><div class="l">项目</div></div>
  <div class="tile"><div class="v">{totalPages}</div><div class="l">总页面</div></div>
  <div class="tile"><div class="v">{pendingCount}</div><div class="l">待审提案</div></div>
  <div class="tile"><div class="v">{handoffTotal}</div><div class="l">待接 handoff</div></div>
</div>

{#if projects.length === 0}
  <div class="empty">
    <div class="big">◌</div>
    还没有项目记忆——在任意项目里跑一次带 engram hooks 的会话即可自动出现。
  </div>
{:else}
  <div class="pgrid">
    {#each projects as p (p.project_name)}
      {@const ex = extras[p.project_name]}
      <button class="pcard" onclick={() => onOpen(p.project_name)}>
        <div class="ws">{p.workspace_name}</div>
        <div class="nm">{p.project_name}</div>
        <div class="meta">{p.page_count} 页 · 活跃于 {relTime(p.last_updated)}</div>
        <div class="foot">
          {#if ex == null}
            <span class="loading" title="加载健康度中">·</span>
          {:else}
            {#if ex.health > 0}
              <span class="hbadge warn">⚠ 健康 {ex.health}</span>
            {:else}
              <span class="ok">✓ 健康</span>
            {/if}
            {#if ex.handoff}<span class="hbadge hand">📥 handoff</span>{/if}
          {/if}
        </div>
      </button>
    {/each}
  </div>
{/if}

<style>
  .pgrid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
    gap: 12px;
    margin-top: 14px;
  }

  .pcard {
    text-align: left;
    font: inherit;
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: 14px;
    padding: 14px 16px;
    cursor: pointer;
    color: var(--ink);
  }

  .pcard:hover {
    border-color: var(--accent-border);
  }

  .ws {
    font-size: 10.5px;
    color: var(--muted);
    margin-bottom: 6px;
  }

  .nm {
    font-size: 14px;
    font-weight: 700;
  }

  .meta {
    font-size: 12px;
    color: var(--ink2);
  }

  .foot {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 10px;
    min-height: 20px;
    font-size: 11px;
    color: var(--muted);
  }

  .ok {
    color: var(--st-good);
  }

  .hbadge {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-size: 11px;
    padding: 1px 8px;
    border-radius: 99px;
  }

  .hbadge.warn {
    background: rgba(250, 178, 25, 0.14);
    color: var(--st-warn);
  }

  .hbadge.hand {
    background: var(--accent-weak);
    color: var(--accent);
  }
</style>
