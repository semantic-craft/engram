<script lang="ts">
  import {
    pendingQueue,
    pendingDetail,
    pendingDiff,
    pendingApprove,
    pendingReject,
    type PendingGroup,
    type PendingDetail,
    type PendingSummary,
  } from "./api";

  let { onError }: { onError: (msg: string) => void } = $props();

  let queue = $state<PendingGroup[]>([]);
  let loading = $state(true);
  let selProject = $state<string | null>(null);
  let detail = $state<PendingDetail | null>(null);
  let diff = $state("");
  let reason = $state("");
  let busy = $state(false);

  async function refresh() {
    loading = true;
    try {
      queue = await pendingQueue();
    } catch (e) {
      onError(String(e));
    } finally {
      loading = false;
    }
  }

  async function openItem(project: string, p: PendingSummary) {
    try {
      selProject = project;
      detail = await pendingDetail(project, p.id);
      diff = (await pendingDiff(project, p.id)).diff ?? "";
      reason = "";
    } catch (e) {
      onError(String(e));
    }
  }

  async function decide(approve: boolean) {
    if (!detail || !selProject) return;
    busy = true;
    try {
      if (approve) {
        await pendingApprove(selProject, detail.summary.id);
      } else {
        await pendingReject(selProject, detail.summary.id, reason);
      }
      detail = null;
      selProject = null;
      await refresh();
    } catch (e) {
      onError(String(e));
    } finally {
      busy = false;
    }
  }

  refresh();
</script>

<div class="pending">
  {#if detail}
    <div class="detail">
      <button class="back" onclick={() => (detail = null)}>← 队列</button>
      <h2>{detail.summary.title}</h2>
      <p class="meta">
        <code>{selProject}/{detail.summary.target_path}</code>
        · {detail.summary.operation} · {detail.summary.kind}
        · 置信 {(detail.summary.confidence * 100).toFixed(0)}%
      </p>
      {#if detail.rationale}
        <p class="rationale">{detail.rationale}</p>
      {/if}
      <pre class="diff">{diff || detail.body_markdown}</pre>
      <div class="actions">
        <input
          placeholder="拒绝理由（回写 rejection context，选填）"
          bind:value={reason}
          disabled={busy}
        />
        <button class="reject" onclick={() => decide(false)} disabled={busy}>拒绝</button>
        <button class="approve" onclick={() => decide(true)} disabled={busy}>批准</button>
      </div>
    </div>
  {:else if loading}
    <p class="empty">加载中…</p>
  {:else if queue.length === 0}
    <p class="empty">没有待审提案 — auto-improve 的产出会出现在这里。</p>
  {:else}
    {#each queue as group (group.project)}
      <h3>{group.project}</h3>
      <ul>
        {#each group.proposals as p (p.id)}
          <li>
            <button class="item" onclick={() => openItem(group.project, p)}>
              <span class="title">{p.title}</span>
              <span class="path">{p.operation} · {p.target_path}</span>
            </button>
          </li>
        {/each}
      </ul>
    {/each}
  {/if}
</div>

<style>
  .pending { padding: 0.5rem 1rem; overflow-y: auto; }
  .empty { color: var(--muted, #888); }
  h3 { margin: 0.8rem 0 0.3rem; font-size: 0.9rem; color: var(--muted, #888); }
  ul { list-style: none; margin: 0; padding: 0; }
  .item { display: flex; flex-direction: column; align-items: flex-start; width: 100%;
          text-align: left; padding: 0.4rem 0.6rem; border: none; background: none;
          cursor: pointer; border-radius: 6px; }
  .item:hover { background: rgba(128, 128, 128, 0.12); }
  .item .path { font-size: 0.75rem; color: var(--muted, #888); }
  .detail h2 { margin: 0.4rem 0; }
  .meta { font-size: 0.8rem; color: var(--muted, #888); }
  .rationale { font-size: 0.9rem; }
  .diff { background: rgba(128, 128, 128, 0.08); padding: 0.8rem; border-radius: 8px;
          overflow-x: auto; white-space: pre-wrap; font-size: 0.8rem; max-height: 50vh; }
  .actions { display: flex; gap: 0.5rem; margin-top: 0.6rem; }
  .actions input { flex: 1; padding: 0.4rem 0.6rem; }
  .approve { background: #2e7d32; color: white; border: none; padding: 0.4rem 1rem;
             border-radius: 6px; cursor: pointer; }
  .reject { background: #b23b3b; color: white; border: none; padding: 0.4rem 1rem;
            border-radius: 6px; cursor: pointer; }
  .back { background: none; border: none; cursor: pointer; color: var(--muted, #888);
          padding: 0; }
</style>
