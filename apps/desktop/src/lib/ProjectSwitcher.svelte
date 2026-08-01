<script lang="ts">
  import type { ProjectSummary } from "./api";
  import { relTime } from "./kinds";

  let {
    projects,
    current,
    onSelect,
  }: {
    projects: ProjectSummary[];
    current: string;
    onSelect: (name: string) => void;
  } = $props();

  let open = $state(false);

  function pick(name: string) {
    open = false;
    onSelect(name);
  }
</script>

<svelte:window onclick={() => (open = false)} />

<div class="projsw">
  <button
    class="swbtn"
    onclick={(e) => {
      e.stopPropagation();
      open = !open;
    }}
  >
    <span class="lbl">项目</span>
    <span class="cur">{current || "（未选择）"}</span>
    <span class="caret">▾</span>
  </button>
  {#if open}
    <div class="projdrop" onclick={(e) => e.stopPropagation()} role="presentation">
      <div class="hint">切换项目——各项目记忆相互独立；全局记忆在左侧「全局」区常驻。</div>
      {#each projects as p (p.project_name)}
        <button class="projrow" class:cur={p.project_name === current} onclick={() => pick(p.project_name)}>
          <span class="nm">{p.project_name}</span>
          <span class="meta">{p.page_count} 页 · {relTime(p.last_updated)}</span>
        </button>
      {:else}
        <div class="hint">还没有项目——在任意项目里跑一次带 engram hooks 的会话即可出现。</div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .projsw {
    position: relative;
  }

  .swbtn {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 7px;
    width: 100%;
    font: inherit;
    font-size: 13px;
    font-weight: 650;
    padding: 6px 13px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--surface);
    color: var(--ink);
    cursor: pointer;
  }

  .swbtn:hover {
    background: var(--hover);
  }

  .lbl {
    color: var(--muted);
    font-weight: 400;
    font-size: 12px;
  }

  .cur {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 120px;
  }

  .caret {
    color: var(--muted);
  }

  .projdrop {
    position: absolute;
    top: calc(100% + 6px);
    left: 0;
    z-index: 50;
    width: 280px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 14px;
    box-shadow: 0 8px 30px rgba(0, 0, 0, 0.18);
    padding: 6px;
  }

  .hint {
    font-size: 11px;
    color: var(--muted);
    padding: 4px 10px 8px;
    border-bottom: 1px solid var(--border);
    margin-bottom: 4px;
  }

  .projrow {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    text-align: left;
    font: inherit;
    padding: 7px 10px;
    border: none;
    border-radius: 7px;
    background: none;
    color: var(--ink);
    cursor: pointer;
  }

  .projrow:hover {
    background: var(--hover);
  }

  .projrow.cur {
    background: var(--accent-weak);
  }

  .nm {
    font-weight: 600;
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .meta {
    font-size: 11px;
    color: var(--muted);
    white-space: nowrap;
  }
</style>
