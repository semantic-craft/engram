<script lang="ts">
  import { projectOverview, readPage, type Overview, type PageSummary } from "./api";
  import { effectiveKind, kindColor, kindLabel, relTime } from "./kinds";

  let {
    project,
    pages,
    onOpenPage,
    onGotoKb,
    onError,
  }: {
    project: string;
    pages: PageSummary[];
    onOpenPage: (path: string) => void;
    onGotoKb: (kind?: string) => void;
    onError: (msg: string) => void;
  } = $props();

  type LayerId = "l0" | "l1" | "l2" | "l3";
  let layer = $state<LayerId>("l3");
  let overview = $state<Overview | null>(null);
  // L3 原文懒加载：path → body（截断展示）。
  let bodies = $state<Record<string, string>>({});

  $effect(() => {
    overview = null;
    bodies = {};
    projectOverview(project)
      .then((o) => (overview = o))
      .catch((e) => onError(`分层数据加载失败：${e}`));
  });

  let sessionPages = $derived(
    pages.filter((p) => p.path.startsWith("sessions/")).slice(0, 10),
  );
  let topicPages = $derived(
    pages.filter((p) => !["session", "rule", "slot"].includes(effectiveKind(p))),
  );
  let corePages = $derived(
    overview ? [...overview.briefing.slots, ...overview.briefing.rules] : [],
  );

  // 选中 L3 时补取核心页原文（≤8 页，30s 服务端缓存内很便宜）。
  $effect(() => {
    if (layer !== "l3") return;
    for (const p of corePages.slice(0, 8)) {
      if (p.path in bodies) continue;
      readPage(p.path, project)
        .then((d) => (bodies[p.path] = d.body.length > 400 ? d.body.slice(0, 400) + " …" : d.body))
        .catch(() => (bodies[p.path] = "（读取失败）"));
    }
  });

  let counts = $derived({
    l0: overview?.briefing.counts.observations,
    l1: overview?.briefing.counts.sessions,
    l2: topicPages.length,
    l3: corePages.length,
  });

  const CARDS: { id: LayerId; tag: string; name: string; desc: string }[] = [
    { id: "l0", tag: "L0 · RAW", name: "观察流", desc: "hooks 捕获的原始 prompt 与工具调用" },
    { id: "l1", tag: "L1 · SESSION", name: "会话", desc: "sessions/ 每会话一页的自动摘要" },
    { id: "l2", tag: "L2 · TOPIC", name: "主题页", desc: "decisions / gotchas / procedures / concepts / notes" },
    { id: "l3", tag: "L3 · CORE", name: "核心", desc: "_rules 规则 + _slots 槽位，常驻注入" },
  ];
</script>

<div class="ph">
  <h1>记忆分层</h1>
  <span class="sub">{project} · 观察 → 会话 → 主题 → 核心</span>
</div>
<p class="desc">
  hooks 自动捕获原始观察（L0），会话结束生成会话页（L1），整理管线沉淀主题页（L2），最稳定的进入规则与槽位（L3）并常驻注入。
</p>

<div class="layers">
  {#each CARDS as c (c.id)}
    <button class="lcard" class:on={layer === c.id} onclick={() => (layer = c.id)}>
      <div class="lv">{c.tag}</div>
      <div class="ln">{c.name}</div>
      <div class="ld">{c.desc}</div>
      <div class="lc" title={counts[c.id] == null ? "加载中" : ""}>
        {counts[c.id] == null ? "·" : counts[c.id]!.toLocaleString()}
      </div>
    </button>
  {/each}
</div>

{#if layer === "l0"}
  <div class="card">
    <h3>观察流 · L0</h3>
    <div class="ltext">
      {counts.l0 == null ? "…" : counts.l0.toLocaleString()} 条观察（近 7 天
      {overview?.briefing.activity_7d.observations.toLocaleString() ?? "…"} 条）。hooks
      捕获的原始 prompt 与工具调用是所有上层记忆的原料；一期仅提供计数，逐条浏览在二期提供。
    </div>
    <div style="margin-top:10px"><button class="btn" disabled>浏览观察流（二期）</button></div>
  </div>
{:else if layer === "l1"}
  <div class="card">
    <h3>会话 · L1 <button class="more" onclick={() => onGotoKb("session")}>在知识库过滤 →</button></h3>
    <div class="plist">
      {#each sessionPages as p (p.path)}
        <button class="prow" onclick={() => onOpenPage(p.path)}>
          <span class="dot" style="background:{kindColor('session')}"></span>
          <span class="t">{p.title || p.path}</span>
          <span class="p mono">{p.path}</span>
          <span class="tm">{relTime(p.updated_at)}</span>
        </button>
      {:else}
        <div class="ltext">还没有会话页。</div>
      {/each}
    </div>
    {#if counts.l1 != null && counts.l1 > sessionPages.length}
      <div class="lnote">共 {counts.l1} 个会话 · 显示最近 {sessionPages.length} 个会话页</div>
    {/if}
  </div>
{:else if layer === "l2"}
  <div class="card">
    <h3>主题页 · L2 <button class="more" onclick={() => onGotoKb()}>在知识库过滤 →</button></h3>
    <div class="plist">
      {#each topicPages.slice(0, 10) as p (p.path)}
        {@const k = effectiveKind(p)}
        <button class="prow" onclick={() => onOpenPage(p.path)}>
          <span class="dot" style="background:{kindColor(k)}"></span>
          <span class="t">{p.title || p.path}</span>
          <span class="kpill" style="color:{kindColor(k)}">{kindLabel(k)}</span>
          <span class="p mono">{p.path}</span>
          <span class="tm">{relTime(p.updated_at)}</span>
        </button>
      {:else}
        <div class="ltext">还没有主题页——整理管线（consolidate / auto-improve）沉淀后出现。</div>
      {/each}
    </div>
  </div>
{:else}
  <div class="card">
    <h3>核心 · L3（常驻注入每个会话）</h3>
    {#each corePages as p (p.path)}
      <button class="rulebox rlink" onclick={() => onOpenPage(p.path)}>
        <div class="rp mono">{p.path} · {kindLabel(p.kind)}</div>
        {bodies[p.path] ?? p.title ?? p.path}
      </button>
    {:else}
      <div class="ltext">还没有 _rules/ 或 _slots/ 页。</div>
    {/each}
  </div>
{/if}

<style>
  .layers {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 10px;
    margin: 14px 0;
  }

  .lcard {
    text-align: left;
    font: inherit;
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: 14px;
    padding: 13px 15px;
    cursor: pointer;
    color: var(--ink);
  }

  .lcard:hover {
    border-color: var(--accent-border);
  }

  .lcard.on {
    border-color: var(--accent);
    outline: 1px solid var(--accent);
  }

  .lv {
    font-size: 10px;
    font-weight: 800;
    letter-spacing: 0.08em;
    color: var(--muted);
  }

  .ln {
    font-size: 14px;
    font-weight: 700;
    margin: 2px 0;
  }

  .ld {
    font-size: 11px;
    color: var(--muted);
    line-height: 1.45;
    min-height: 32px;
  }

  .lc {
    font-size: 21px;
    font-weight: 700;
    margin-top: 6px;
    font-variant-numeric: tabular-nums;
  }

  .ltext {
    font-size: 12.5px;
    color: var(--ink2);
  }

  .lnote {
    font-size: 11.5px;
    color: var(--muted);
    padding: 8px 2px 0;
  }

  .rlink {
    display: block;
    width: 100%;
    text-align: left;
    font: inherit;
    cursor: pointer;
    color: var(--ink);
    white-space: pre-wrap;
  }

  .rlink:hover {
    border-color: var(--accent-border);
  }
</style>
