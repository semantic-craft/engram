<script lang="ts">
  let {
    project,
    onCreate,
    onCancel,
  }: {
    project: string;
    onCreate: (path: string, kind: string) => void;
    onCancel: () => void;
  } = $props();

  const KIND_OPTIONS = [
    { kind: "decision", prefix: "decisions/", label: "决策 decision" },
    { kind: "gotcha", prefix: "gotchas/", label: "坑 gotcha" },
    { kind: "procedure", prefix: "procedures/", label: "流程 procedure" },
    { kind: "concept", prefix: "concepts/", label: "概念 concept" },
    { kind: "fact", prefix: "notes/", label: "笔记 fact" },
    { kind: "rule", prefix: "_rules/", label: "规则 rule（将进入 _rules/）" },
  ];

  let kind = $state("decision");
  let name = $state("");

  let prefix = $derived(KIND_OPTIONS.find((o) => o.kind === kind)?.prefix ?? "notes/");
  let fullPath = $derived(prefix + (name.trim() || "new-page.md"));

  function create() {
    let n = name.trim();
    if (!n) return;
    if (!n.endsWith(".md")) n += ".md";
    onCreate(prefix + n, kind);
  }
</script>

<div
  class="overlay"
  onclick={(e) => e.target === e.currentTarget && onCancel()}
  role="presentation"
>
  <div class="modal">
    <h3>新建页 · {project}</h3>
    <label for="np-kind">类型（kind）</label>
    <select id="np-kind" bind:value={kind}>
      {#each KIND_OPTIONS as o (o.kind)}
        <option value={o.kind}>{o.label}</option>
      {/each}
    </select>
    <label for="np-path">文件名</label>
    <!-- svelte-ignore a11y_autofocus -->
    <input
      id="np-path"
      placeholder="例如 desktop-scope-wiring.md"
      bind:value={name}
      autofocus
      onkeydown={(e) => e.key === "Enter" && create()}
    />
    <div class="pathhint">完整路径：<span class="mono">{fullPath}</span></div>
    <div class="mrow">
      <button class="btn" onclick={onCancel}>取消</button>
      <button class="btn pri" onclick={create} disabled={!name.trim()}>创建并编辑</button>
    </div>
  </div>
</div>

<style>
  .pathhint {
    font-size: 11px;
    color: var(--muted);
    margin-top: 6px;
  }
</style>
