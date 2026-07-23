<script lang="ts">
  import type { PageSummary } from "./api";

  let { pages, onSelect }: { pages: PageSummary[]; onSelect: (path: string) => void } = $props();

  let groups = $derived.by(() => {
    const map = new Map<string, PageSummary[]>();
    for (const p of pages) {
      const slashIdx = p.path.indexOf("/");
      const key = slashIdx === -1 ? "(root)" : p.path.slice(0, slashIdx);
      const list = map.get(key) ?? [];
      list.push(p);
      map.set(key, list);
    }
    return [...map.entries()];
  });
</script>

<nav class="sidebar">
  {#each groups as [group, items] (group)}
    <div class="group">
      <div class="group-header">{group} <span class="count">({items.length})</span></div>
      {#each items as p (p.path)}
        <button class="page-item" onclick={() => onSelect(p.path)}>
          {p.title || p.path}
        </button>
      {/each}
    </div>
  {/each}
</nav>

<style>
  .sidebar {
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    height: 100%;
  }

  .group {
    display: flex;
    flex-direction: column;
  }

  .group-header {
    font-size: 0.75rem;
    font-weight: 600;
    text-transform: uppercase;
    color: #888;
    padding: 0.5rem 0.75rem 0.25rem;
  }

  .count {
    font-weight: 400;
  }

  .page-item {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    padding: 0.35rem 0.75rem;
    cursor: pointer;
    font-size: 0.9rem;
    color: inherit;
  }

  .page-item:hover {
    background: rgba(0, 0, 0, 0.06);
  }
</style>
