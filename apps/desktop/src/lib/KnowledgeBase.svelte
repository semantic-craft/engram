<script lang="ts">
  import type { PageSummary } from "./api";
  import { KIND_ORDER, effectiveKind, kindColor, kindLabel, relTime } from "./kinds";

  let {
    title,
    scopeLabel,
    pages,
    banner,
    kindFilter = $bindable("all"),
    onOpen,
    onNew,
  }: {
    title: string;
    scopeLabel: string;
    pages: PageSummary[];
    banner?: string;
    kindFilter?: string;
    onOpen: (path: string) => void;
    onNew: () => void;
  } = $props();

  let counts = $derived.by(() => {
    const m = new Map<string, number>();
    for (const p of pages) {
      const k = effectiveKind(p);
      m.set(k, (m.get(k) ?? 0) + 1);
    }
    return KIND_ORDER.filter((k) => m.has(k)).map((k) => [k, m.get(k)!] as const);
  });

  let maxCount = $derived(Math.max(1, ...counts.map(([, c]) => c)));

  let list = $derived(
    (kindFilter === "all" ? [...pages] : pages.filter((p) => effectiveKind(p) === kindFilter)).sort(
      (a, b) => (b.updated_at ?? "").localeCompare(a.updated_at ?? ""),
    ),
  );
</script>

<div class="ph">
  <h1>{title}</h1>
  <span class="sub">{scopeLabel} · {pages.length} 页</span>
  <div class="acts"><button class="btn" onclick={onNew}>＋ 新建页</button></div>
</div>

{#if banner}
  <div class="banner">✦&nbsp;{banner}</div>
{/if}

{#if pages.length === 0}
  <div class="empty">
    <div class="big">◌</div>
    这里还没有页面——hooks 捕获会话后由整理管线沉淀，或点右上角手动新建。
  </div>
{:else}
  <div class="chips">
    <button class="chip" class:on={kindFilter === "all"} onclick={() => (kindFilter = "all")}>
      全部 <b>{pages.length}</b>
    </button>
    {#each counts as [k, c] (k)}
      <button class="chip" class:on={kindFilter === k} onclick={() => (kindFilter = k)}>
        <span class="dot" style="background:{kindColor(k)}"></span>{kindLabel(k)} <b>{c}</b>
      </button>
    {/each}
  </div>

  <div class="dist">
    {#each counts as [k, c] (k)}
      <div class="drow">
        <div class="dl"><span class="dot" style="background:{kindColor(k)}"></span>{kindLabel(k)}</div>
        <div class="db"><i style="width:{Math.round((c / maxCount) * 100)}%;background:{kindColor(k)}"></i></div>
        <div class="dv">{c}（{((c / pages.length) * 100).toFixed(1)}%）</div>
      </div>
    {/each}
  </div>

  <div class="plist">
    {#each list as p (p.path)}
      {@const k = effectiveKind(p)}
      <button class="prow" onclick={() => onOpen(p.path)}>
        <span class="dot" style="background:{kindColor(k)}"></span>
        <span class="t">{p.title || p.path}</span>
        <span class="kpill" style="color:{kindColor(k)}">{kindLabel(k)}</span>
        <span class="p mono">{p.path}</span>
        <span class="tm">{relTime(p.updated_at)}</span>
      </button>
    {:else}
      <div class="empty"><div class="big">◌</div>该类型暂无页面</div>
    {/each}
  </div>
{/if}
