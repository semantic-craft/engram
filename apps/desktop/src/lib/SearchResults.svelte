<script lang="ts">
  import type { Hit } from "./api";

  let { hits, onSelect }: { hits: Hit[]; onSelect: (path: string) => void } = $props();

  // Escape everything, then re-allow only the daemon's <mark> highlight tags.
  function renderSnippet(snippet: string): string {
    return snippet
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/&lt;mark&gt;/g, "<mark>")
      .replace(/&lt;\/mark&gt;/g, "</mark>");
  }
</script>

<div class="search-results">
  {#each hits as h (h.path)}
    <button class="result-item" onclick={() => onSelect(h.path)}>
      <div class="title">{h.title || h.path}</div>
      {#if h.snippet}
        <div class="snippet">{@html renderSnippet(h.snippet)}</div>
      {/if}
    </button>
  {/each}
</div>

<style>
  .search-results {
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    height: 100%;
    padding: 0.5rem 1rem;
    box-sizing: border-box;
  }

  .result-item {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    border-bottom: 1px solid rgba(0, 0, 0, 0.08);
    padding: 0.6rem 0;
    cursor: pointer;
    color: inherit;
  }

  .result-item:hover {
    background: rgba(0, 0, 0, 0.06);
  }

  .title {
    font-weight: 600;
    margin-bottom: 0.2rem;
  }

  .snippet {
    font-size: 0.85rem;
    color: #666;
  }

  .snippet :global(mark) {
    background: #ffe58a;
    padding: 0 0.1em;
  }
</style>
