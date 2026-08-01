<script lang="ts">
  import type { Hit } from "./api";
  import { renderSnippet } from "./kinds";

  let {
    hits,
    query,
    globalScope,
    onSelect,
    onScopeChange,
  }: {
    hits: Hit[];
    query: string;
    globalScope: boolean;
    onSelect: (path: string, project?: string) => void;
    onScopeChange: (global: boolean) => void;
  } = $props();
</script>

<div class="ph">
  <h1>搜索：{query}</h1>
  <span class="sub">{hits.length} 条结果</span>
  <div class="acts">
    <div class="seg">
      <button class:on={!globalScope} onclick={() => onScopeChange(false)}>本项目</button>
      <button class:on={globalScope} onclick={() => onScopeChange(true)}>全部项目</button>
    </div>
  </div>
</div>

{#if hits.length === 0}
  <div class="empty">
    <div class="big">◌</div>
    没搜到。语义检索需要 embedding——如果刚写入大量页面，可先在 Daemon 管理里补 embedding。
  </div>
{:else}
  <div class="plist">
    {#each hits as h (h.project ? `${h.project}/${h.path}` : h.path)}
      <button class="srow" onclick={() => onSelect(h.path, h.project ?? undefined)}>
        <div class="st">
          {h.title || h.path}
          {#if h.project}<span class="projtag">{h.project}</span>{/if}
        </div>
        <div class="sp mono">
          {h.path}{h.rank != null ? ` · rank ${h.rank.toFixed(3)}` : ""}
        </div>
        {#if h.snippet}
          <div class="sn">{@html renderSnippet(h.snippet)}</div>
        {/if}
      </button>
    {/each}
  </div>
  <div class="note">语义检索：memory_query（FTS + 向量 RRF）·「全部项目」走 global=true，命中带项目标签</div>
{/if}

<style>
  .srow {
    display: block;
    width: 100%;
    text-align: left;
    font: inherit;
    padding: 11px 12px;
    border: none;
    border-bottom: 1px solid var(--border);
    background: none;
    color: var(--ink);
    cursor: pointer;
  }

  .srow:hover {
    background: var(--hover);
  }

  .st {
    font-weight: 650;
    font-size: 13px;
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .sp {
    font-size: 11px;
    color: var(--muted);
    margin-top: 2px;
  }

  .sn {
    font-size: 12px;
    color: var(--ink2);
    margin-top: 4px;
  }

  .note {
    font-size: 11.5px;
    color: var(--muted);
    padding: 12px 2px;
  }
</style>
