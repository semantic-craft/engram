<script lang="ts">
  import { writePage, deletePage, type PageDetail } from "./api";

  let {
    page,
    autoEdit = false,
    onSelect,
    onSaved,
    onDeleted,
    onCanceled,
    onError,
  }: {
    page: PageDetail | null;
    autoEdit?: boolean;
    onSelect: (path: string) => void;
    onSaved: (path: string) => void;
    onDeleted: (path: string) => void;
    onCanceled: () => void;
    onError: (msg: string) => void;
  } = $props();

  let editing = $state(false);
  let draft = $state("");
  let saving = $state(false);
  let deleteArmed = $state(false);

  // Reset editor state whenever a different page object arrives.
  $effect(() => {
    draft = page?.body ?? "";
    editing = autoEdit;
    deleteArmed = false;
  });

  function startEdit() {
    draft = page?.body ?? "";
    editing = true;
  }

  function cancelEdit() {
    editing = false;
    draft = page?.body ?? "";
    onCanceled();
  }

  async function save() {
    if (!page) return;
    saving = true;
    try {
      const fm = (page.frontmatter ?? {}) as Record<string, unknown>;
      const tags = Array.isArray(fm.tags) ? (fm.tags as string[]) : [];
      await writePage({
        path: page.path,
        body: draft,
        title: page.title || undefined,
        kind: page.kind,
        tier: page.tier,
        tags,
        pinned: page.pinned ?? false,
        frontmatter: fm,
      });
      editing = false;
      onSaved(page.path);
    } catch (e) {
      onError(`保存失败：${e}`);
    } finally {
      saving = false;
    }
  }

  async function confirmDelete() {
    if (!page) return;
    try {
      await deletePage(page.path);
      onDeleted(page.path);
    } catch (e) {
      onError(`删除失败：${e}`);
    } finally {
      deleteArmed = false;
    }
  }
</script>

{#if page === null}
  <div class="empty-hint">选左侧一页，或上方搜索。</div>
{:else}
  <div class="page-view">
    <div class="head-row">
      <div class="breadcrumb">{page.path}</div>
      <div class="actions">
        {#if editing}
          <button class="primary" onclick={save} disabled={saving}>
            {saving ? "保存中…" : "保存"}
          </button>
          <button onclick={cancelEdit} disabled={saving}>取消</button>
        {:else}
          <button onclick={startEdit}>编辑</button>
          {#if deleteArmed}
            <button class="danger" onclick={confirmDelete}>确认删除</button>
            <button onclick={() => (deleteArmed = false)}>取消</button>
          {:else}
            <button class="danger" onclick={() => (deleteArmed = true)}>删除</button>
          {/if}
        {/if}
      </div>
    </div>

    {#if editing}
      <!-- svelte-ignore a11y_autofocus -->
      <textarea class="editor" bind:value={draft} autofocus spellcheck="false"></textarea>
    {:else}
      <h1>{page.title}</h1>
      <div class="chips">
        {#if page.kind}<span class="chip">{page.kind}</span>{/if}
        {#if page.tier}<span class="chip">{page.tier}</span>{/if}
        {#if page.updated_at}<span class="chip">{page.updated_at.slice(0, 10)}</span>{/if}
      </div>
      <pre class="body">{page.body}</pre>
      {#if page.backlinks?.length}
        <div class="backlinks">
          <h2>Backlinks</h2>
          {#each page.backlinks as b (b.path)}
            <button class="backlink-item" onclick={() => onSelect(b.path)}>
              {b.title || b.path}
            </button>
          {/each}
        </div>
      {/if}
    {/if}
  </div>
{/if}

<style>
  .empty-hint {
    color: #888;
    padding: 2rem;
    text-align: center;
  }

  .page-view {
    display: flex;
    flex-direction: column;
    padding: 1rem 1.5rem;
    overflow-y: auto;
    height: 100%;
    box-sizing: border-box;
  }

  .head-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
  }

  .breadcrumb {
    font-size: 0.75rem;
    color: #888;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
  }

  .actions button {
    font-size: 0.8rem;
    padding: 0.25rem 0.7rem;
    border-radius: 6px;
    border: 1px solid rgba(0, 0, 0, 0.15);
    background: none;
    cursor: pointer;
  }

  .actions button:hover:not(:disabled) {
    background: rgba(0, 0, 0, 0.06);
  }

  .actions button:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .actions .primary {
    background: #396cd8;
    border-color: #396cd8;
    color: #fff;
  }

  .actions .danger {
    color: #c0392b;
    border-color: rgba(192, 57, 43, 0.4);
  }

  .editor {
    flex: 1;
    min-height: 60vh;
    margin-top: 0.75rem;
    padding: 0.75rem;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.85rem;
    line-height: 1.5;
    border: 1px solid rgba(0, 0, 0, 0.15);
    border-radius: 6px;
    resize: none;
    box-sizing: border-box;
  }

  .chips {
    display: flex;
    gap: 0.4rem;
    margin-bottom: 1rem;
  }

  .chip {
    font-size: 0.75rem;
    background: rgba(0, 0, 0, 0.06);
    border-radius: 999px;
    padding: 0.15rem 0.6rem;
  }

  .body {
    white-space: pre-wrap;
    word-wrap: break-word;
    font-family: inherit;
    font-size: 0.9rem;
    line-height: 1.5;
  }

  .backlinks {
    margin-top: 1.5rem;
    border-top: 1px solid rgba(0, 0, 0, 0.1);
    padding-top: 0.75rem;
  }

  .backlink-item {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    padding: 0.35rem 0;
    cursor: pointer;
    font-size: 0.9rem;
    color: #396cd8;
  }

  .backlink-item:hover {
    text-decoration: underline;
  }
</style>
