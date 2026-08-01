<script lang="ts">
  import {
    discoverProjectInstructions,
    openInEditor,
    readInstructionFiles,
    type InstructionFile,
  } from "./api";
  import { fmtBytes } from "./kinds";

  let {
    project,
    repoPath,
    onError,
  }: {
    project: string;
    repoPath: string | null;
    onError: (msg: string) => void;
  } = $props();

  const GLOBAL_CANDIDATES = ["~/.claude/CLAUDE.md", "~/.codex/AGENTS.md"];
  const CUSTOM_KEY = "engram.instructions.custom";
  const TEMPLATE_KEY = "engram.instructions.promptTemplate";
  const rootKey = (p: string) => `engram.instructions.root.${p}`;

  // 外部 agent 修改提示词的脚手架：{{files}} / {{notes}} 由当前文件自动填充，
  // 结尾留「我的需求」空槽，用户粘贴后自行补充，外部 agent 直接执行。
  // 编辑准则取自官方文档：code.claude.com/docs/en/memory、
  // code.claude.com/docs/en/best-practices、agents.md。
  const DEFAULT_TEMPLATE = `请修改以下 agent 指令文件。先完整读一遍目标文件再动手，按文末需求直接执行修改。

目标文件：
{{files}}

这些文件的性质（官方文档）：
- 指令文件在每个会话开始时全文注入上下文——长度即成本，越长遵循度越差（官方建议单文件 < 200 行）。
- 它们是给 agent 的持久指令而非强制配置：越具体、越简洁、结构越清晰，遵循越可靠。

编辑准则：
{{notes}}
- 保留：模型猜不到的命令与环境怪癖、与默认不同的风格约定、架构决策、坑（gotcha）、安全红线（除非下面明确点名要删）。
- 删除：模型读代码就能推出的内容（目录结构、依赖清单等）、模型本来就懂的通用常识、过时或一次性的信息。
- 合并语义重复的条目；互相矛盾的条目会让 agent 随机选边，必须消解。
- 判断标准（官方）：逐条自问"删掉这条会导致 agent 犯错吗？"——不会就删。
- 写法：短 bullet + markdown 标题分组；模糊改具体（如"跑 npm test 再提交"优于"记得测试"）；IMPORTANT/YOU MUST 只用于最关键的少数规则。
- 保持原有语言与结构；最小 diff；不要重排无关段落；改完逐文件用一句话总结改动。

## 我的需求
（粘贴后在这里补充，例如：删掉关于 X 的过时条目 / 把 Y 规则改成 Z / 只做瘦身……）`;

  let tab = $state<"global" | "project">("global");
  let globalFiles = $state<InstructionFile[]>([]);
  let projectFiles = $state<InstructionFile[]>([]);
  let template = $state(localStorage.getItem(TEMPLATE_KEY) ?? DEFAULT_TEMPLATE);
  let editingTemplate = $state(false);
  let templateDraft = $state("");
  let copied = $state<string | null>(null);
  let customPaths = $state<string[]>(JSON.parse(localStorage.getItem(CUSTOM_KEY) ?? "[]"));
  let addingPath = $state("");
  let showAdd = $state(false);
  let manualRoot = $state("");
  // repo_path 缺失时用户手动指定的项目根（按项目记住）。
  let savedRoot = $state<string | null>(null);

  $effect(() => {
    savedRoot = localStorage.getItem(rootKey(project));
  });

  let effectiveRoot = $derived(repoPath ?? savedRoot);

  async function loadGlobal() {
    try {
      globalFiles = await readInstructionFiles([...GLOBAL_CANDIDATES, ...customPaths]);
    } catch (e) {
      onError(`读取全局指令文件失败：${e}`);
    }
  }

  async function loadProject(root: string) {
    try {
      projectFiles = await discoverProjectInstructions(root);
    } catch (e) {
      onError(`读取项目指令文件失败：${e}`);
      projectFiles = [];
    }
  }

  $effect(() => {
    void customPaths;
    loadGlobal();
  });

  $effect(() => {
    if (effectiveRoot) loadProject(effectiveRoot);
    else projectFiles = [];
  });

  function addCustom() {
    const p = addingPath.trim();
    if (!p) return;
    if (!p.startsWith("/") && !p.startsWith("~/")) {
      onError("自定义路径必须是绝对路径或 ~/ 开头");
      return;
    }
    customPaths = [...customPaths, p];
    localStorage.setItem(CUSTOM_KEY, JSON.stringify(customPaths));
    addingPath = "";
    showAdd = false;
  }

  function removeCustom(p: string) {
    customPaths = customPaths.filter((x) => x !== p);
    localStorage.setItem(CUSTOM_KEY, JSON.stringify(customPaths));
  }

  function clearRoot() {
    localStorage.removeItem(rootKey(project));
    savedRoot = null;
  }

  function setManualRoot() {
    const r = manualRoot.trim();
    if (!r.startsWith("/") && !r.startsWith("~/")) {
      onError("项目根必须是绝对路径或 ~/ 开头");
      return;
    }
    localStorage.setItem(rootKey(project), r);
    savedRoot = r;
    manualRoot = "";
  }

  function open(f: InstructionFile) {
    openInEditor(f.abs_path).catch((e) => onError(`打开失败：${e}`));
  }

  function fileLine(f: InstructionFile): string {
    const tags: string[] = [];
    if (isPointer(f)) tags.push("指针文件 → AGENTS.md，规则不要写进这里");
    else if (f.path === "AGENTS.md") tags.push("canonical 指令文件");
    return `- ${f.abs_path}${tags.length ? `（${tags.join("；")}）` : ""}`;
  }

  function buildNotes(files: InstructionFile[]): string {
    const notes: string[] = [];
    if (files.some((f) => (f.content ?? "").includes("<!-- engram:start -->"))) {
      notes.push(
        "- 绝不修改 <!-- engram:start --> … <!-- engram:end --> 托管区块（engram 自动维护，手改会被重写）。",
      );
    }
    if (files.some(isPointer)) {
      notes.push(
        "- CLAUDE.md 是指针文件（@AGENTS.md，官方推荐的两工具共用模式），改动一律落在 AGENTS.md。",
      );
    }
    if (files.some((f) => f.exists && f.path.endsWith("AGENTS.md"))) {
      notes.push(
        "- AGENTS.md 是跨工具开放标准（Codex / Cursor / Gemini CLI 等都读取），改动影响所有 agent，不只某一家。",
      );
    }
    return notes.join("\n");
  }

  function buildPrompt(files: InstructionFile[]): string {
    const fs = files.filter((f) => f.exists);
    return template
      .replace("{{files}}", fs.map(fileLine).join("\n") || "-（无）")
      .replace("{{notes}}", buildNotes(fs) || "-（该文件无 engram 托管区块，可整体编辑）");
  }

  async function copyText(text: string, key: string) {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // WKWebView 剪贴板兜底
      const ta = document.createElement("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    copied = key;
    setTimeout(() => {
      if (copied === key) copied = null;
    }, 1600);
  }

  function startEditTemplate() {
    templateDraft = template;
    editingTemplate = true;
  }

  function saveTemplate() {
    template = templateDraft;
    localStorage.setItem(TEMPLATE_KEY, template);
    editingTemplate = false;
  }

  let tabFiles = $derived(tab === "global" ? globalFiles : projectFiles);

  // 官方加载顺序（code.claude.com/docs/en/memory）：托管策略 → 用户 →
  // 项目 → 项目本地；靠后的更具体、在上下文中更晚出现。macOS 路径。
  const CHAIN_LAYERS = [
    {
      scope: "托管策略",
      note: "组织统一下发，个人设置无法排除",
      paths: ["/Library/Application Support/ClaudeCode/CLAUDE.md"],
    },
    { scope: "用户", note: "本机所有项目通用", paths: ["~/.claude/CLAUDE.md"] },
    { scope: "项目", note: "随仓库共享给团队", paths: ["CLAUDE.md", ".claude/CLAUDE.md"] },
    { scope: "项目本地", note: "个人私有，应加入 .gitignore", paths: ["CLAUDE.local.md"] },
  ];

  let chain = $state<InstructionFile[]>([]);

  $effect(() => {
    const root = effectiveRoot;
    const wanted = CHAIN_LAYERS.flatMap((l) =>
      l.paths.map((p) =>
        p.startsWith("~") || p.startsWith("/") ? p : root ? `${root}/${p}` : null,
      ),
    ).filter((p): p is string => p != null);
    if (wanted.length === 0) {
      chain = [];
      return;
    }
    readInstructionFiles(wanted)
      .then((files) => (chain = files))
      .catch(() => (chain = []));
  });

  /** 链上某条候选路径的读取结果（未请求时为 undefined）。 */
  function chainEntry(displayPath: string): InstructionFile | undefined {
    const abs =
      displayPath.startsWith("~") || displayPath.startsWith("/")
        ? displayPath
        : effectiveRoot
          ? `${effectiveRoot}/${displayPath}`
          : null;
    return abs == null ? undefined : chain.find((f) => f.path === abs);
  }

  function isPointer(f: InstructionFile): boolean {
    return f.path === "CLAUDE.md" && (f.content ?? "").trimStart().startsWith("@AGENTS.md");
  }

  // 官方指导：指令文件每会话全文注入，建议单文件 < 200 行。
  function lineCount(f: InstructionFile): number | null {
    if (!f.exists || f.content == null || f.truncated) return null;
    return f.content.split("\n").length;
  }

  /** 按 engram 托管标记切分内容，托管区块高亮。 */
  function segments(content: string): { managed: boolean; text: string }[] {
    const out: { managed: boolean; text: string }[] = [];
    let rest = content;
    for (;;) {
      const s = rest.indexOf("<!-- engram:start -->");
      if (s < 0) break;
      const e = rest.indexOf("<!-- engram:end -->", s);
      if (e < 0) break;
      if (s > 0) out.push({ managed: false, text: rest.slice(0, s) });
      out.push({ managed: true, text: rest.slice(s, e + "<!-- engram:end -->".length) });
      rest = rest.slice(e + "<!-- engram:end -->".length);
    }
    if (rest) out.push({ managed: false, text: rest });
    return out;
  }
</script>

{#snippet fileCard(f: InstructionFile, removable: boolean)}
  <div class="card filecard" class:missing={!f.exists}>
    <div class="fh">
      <span class="path mono">{f.path}</span>
      {#if isPointer(f)}<span class="ptrtag">指针文件 → AGENTS.md</span>{/if}
      {#if f.exists}
        {@const lines = lineCount(f)}
        <span class="meta">
          {fmtBytes(f.size)}{lines != null ? ` · ${lines} 行` : ""}{f.truncated
            ? " · 预览已截断"
            : ""}
        </span>
        {#if lines != null && lines > 200}
          <span class="linewarn" title="官方指导：指令文件每会话全文注入，单文件建议 < 200 行，越长遵循度越差">
            ⚠ 超官方建议 200 行
          </span>
        {/if}
      {:else}
        <span class="meta">未找到</span>
      {/if}
      <div class="facts">
        {#if f.exists}
          <button class="btn" onclick={() => copyText(buildPrompt([f]), f.abs_path)}>
            {copied === f.abs_path ? "✓ 已复制" : "⧉ 修改提示词"}
          </button>
          <button class="btn" onclick={() => copyText(f.abs_path, `p:${f.abs_path}`)}>
            {copied === `p:${f.abs_path}` ? "✓" : "复制路径"}
          </button>
          <button class="btn" onclick={() => open(f)}>在编辑器打开</button>
        {/if}
        {#if removable}<button class="btn" onclick={() => removeCustom(f.path)}>移除</button>{/if}
      </div>
    </div>
    {#if f.exists && f.content}
      <div class="fprev">
        {#each segments(f.content) as seg, i (i)}
          {#if seg.managed}
            <div class="managed"><span class="mtag">engram 托管区块</span><pre>{seg.text}</pre></div>
          {:else}
            <pre>{seg.text}</pre>
          {/if}
        {/each}
      </div>
    {/if}
  </div>
{/snippet}

<div class="ph">
  <h1>指令文件</h1>
  <span class="sub">AGENTS.md / CLAUDE.md · 本机直读</span>
  <div class="acts">
    <button
      class="btn"
      onclick={() => copyText(buildPrompt(tabFiles), `tab:${tab}`)}
      disabled={!tabFiles.some((f) => f.exists)}
    >
      {copied === `tab:${tab}` ? "✓ 已复制" : "⧉ 本页全部 · 修改提示词"}
    </button>
    <button class="btn" onclick={startEditTemplate}>编辑模板</button>
  </div>
</div>
<div class="banner">
  ⓘ&nbsp;显示的是<b>本机</b>文件（Desktop 直接读取）；查看确认要改的内容后，「⧉ 修改提示词」会把文件路径 +
  注意事项拼成脚手架复制到剪贴板——粘贴到外部 agent（Claude Code / Codex 等）后在「我的需求」处补充具体改法，由外部
  agent 直接执行。此页本身只读。
</div>

<details class="card chain">
  <summary>
    <b>加载链</b>
    <span class="csub">
      按官方加载顺序（托管策略 → 用户 → 项目 → 项目本地）；靠后的更具体，在上下文中更晚出现
    </span>
  </summary>
  {#each CHAIN_LAYERS as layer (layer.scope)}
    <div class="clayer">
      <div class="cscope">{layer.scope}<span class="cnote">{layer.note}</span></div>
      <div class="cpaths">
        {#each layer.paths as p (p)}
          {@const f = chainEntry(p)}
          <div class="cpath" class:absent={!f?.exists}>
            <span class="dot" style="background:{f?.exists ? 'var(--st-good)' : 'var(--grid)'}"
            ></span>
            <span class="mono">{p}</span>
            {#if f?.exists}
              <span class="cmeta">{fmtBytes(f.size)}</span>
              <button class="cbtn" onclick={() => open(f)}>打开</button>
            {:else if !effectiveRoot && !p.startsWith("~") && !p.startsWith("/")}
              <span class="cmeta">未知（项目根未确定）</span>
            {:else}
              <span class="cmeta">未使用</span>
            {/if}
          </div>
        {/each}
      </div>
    </div>
  {/each}
  <div class="cfoot">
    还有两层不在此列：`.claude/rules/*.md`（路径作用域规则，命中文件时才加载）与
    auto memory 的 `MEMORY.md`（Claude 自己写，仅前 200 行/25 KB 入上下文）。
  </div>
</details>

<div class="tabs">
  <button class="tab" class:on={tab === "global"} onclick={() => (tab = "global")}>全局</button>
  <button class="tab" class:on={tab === "project"} onclick={() => (tab = "project")}>
    本项目 · {project}
  </button>
</div>

{#if tab === "global"}
  {#each globalFiles as f (f.path)}
    {@render fileCard(f, customPaths.includes(f.path))}
  {/each}
  {#if showAdd}
    <div class="addrow">
      <input
        class="addinput mono"
        placeholder="~/Projects/CLAUDE.md"
        bind:value={addingPath}
        onkeydown={(e) => e.key === "Enter" && addCustom()}
      />
      <button class="btn pri" onclick={addCustom}>添加</button>
      <button class="btn" onclick={() => (showAdd = false)}>取消</button>
    </div>
  {:else}
    <button class="btn" onclick={() => (showAdd = true)}>＋ 添加自定义路径</button>
  {/if}
{:else if effectiveRoot}
  <div class="rootrow">
    项目根：<b class="mono">{effectiveRoot}</b>
    <span class="src">{repoPath ? "（来自服务器 repo_path）" : "（手动指定）"}</span>
    {#if !repoPath}
      <button class="btn" onclick={clearRoot}>重新指定</button>
    {/if}
  </div>
  {#each projectFiles as f (f.path)}
    {@render fileCard(f, false)}
  {/each}
{:else}
  <div class="empty">
    <div class="big">◌</div>
    服务器未记录此项目的本地路径（repo_path 为空）。手动指定项目根目录：
    <div class="addrow center">
      <input
        class="addinput mono"
        placeholder="/Users/you/Projects/{project}"
        bind:value={manualRoot}
        onkeydown={(e) => e.key === "Enter" && setManualRoot()}
      />
      <button class="btn pri" onclick={setManualRoot}>使用此目录</button>
    </div>
  </div>
{/if}

{#if editingTemplate}
  <div
    class="overlay"
    onclick={(e) => e.target === e.currentTarget && (editingTemplate = false)}
    role="presentation"
  >
    <div class="modal wide">
      <h3>编辑修改提示词模板</h3>
      <div class="tplhint">
        <code>{"{{files}}"}</code> 会替换成当前文件的绝对路径清单，<code>{"{{notes}}"}</code>
        替换成按文件自动生成的注意事项（托管区块 / 指针文件）。模板保存在本机。
      </div>
      <textarea class="tpl mono" rows="16" bind:value={templateDraft}></textarea>
      <div class="mrow">
        <button class="btn" onclick={() => (templateDraft = DEFAULT_TEMPLATE)}>恢复默认</button>
        <button class="btn" onclick={() => (editingTemplate = false)}>取消</button>
        <button class="btn pri" onclick={saveTemplate}>保存</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .filecard {
    margin-bottom: 12px;
  }

  .chain {
    margin-bottom: 14px;
    padding: 10px 16px;
  }

  .chain summary {
    cursor: pointer;
    font-size: 12.5px;
    list-style: none;
    display: flex;
    align-items: baseline;
    gap: 10px;
    flex-wrap: wrap;
  }

  .chain summary::before {
    content: "›";
    color: var(--muted);
    transition: transform 0.15s;
  }

  .chain[open] summary::before {
    transform: rotate(90deg);
  }

  .csub {
    font-size: 11px;
    color: var(--muted);
    font-weight: 400;
  }

  .clayer {
    display: flex;
    gap: 14px;
    padding: 7px 0 7px 16px;
    border-bottom: 1px solid var(--border);
  }

  .cscope {
    width: 88px;
    flex-shrink: 0;
    font-size: 12px;
    font-weight: 600;
  }

  .cnote {
    display: block;
    font-size: 10.5px;
    font-weight: 400;
    color: var(--muted);
    line-height: 1.4;
  }

  .cpaths {
    flex: 1;
    min-width: 0;
  }

  .cpath {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12px;
    padding: 2px 0;
  }

  .cpath.absent {
    opacity: 0.45;
  }

  .cmeta {
    font-size: 11px;
    color: var(--muted);
  }

  .cbtn {
    font: inherit;
    font-size: 11px;
    color: var(--accent);
    background: none;
    border: none;
    cursor: pointer;
    padding: 0;
  }

  .cbtn:hover {
    text-decoration: underline;
  }

  .cfoot {
    font-size: 11px;
    color: var(--muted);
    padding-top: 8px;
    line-height: 1.5;
  }

  .modal.wide {
    width: 640px;
  }

  .tplhint {
    font-size: 11.5px;
    color: var(--muted);
    margin-bottom: 8px;
  }

  .tpl {
    width: 100%;
    font-size: 12px;
    line-height: 1.55;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 9px;
    background: var(--page);
    color: var(--ink);
    resize: vertical;
  }

  .fh {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }

  .fh .path {
    font-weight: 650;
  }

  .fh .meta {
    font-size: 11px;
    color: var(--muted);
  }

  .facts {
    margin-left: auto;
    display: flex;
    gap: 6px;
  }

  .filecard.missing {
    opacity: 0.55;
  }

  .fprev {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 10px 14px;
    margin-top: 10px;
    font-size: 12px;
    max-height: 260px;
    overflow-y: auto;
    line-height: 1.65;
  }

  .fprev pre {
    margin: 0;
    white-space: pre-wrap;
    word-break: break-word;
    font-family: inherit;
  }

  .managed {
    border: 1px solid var(--accent-border);
    background: var(--managed-bg);
    border-radius: 6px;
    padding: 6px 10px;
    margin: 8px 0;
    position: relative;
  }

  .managed .mtag {
    position: absolute;
    top: -9px;
    right: 8px;
    font-size: 10px;
    font-weight: 700;
    background: var(--accent);
    color: #fff;
    padding: 0 8px;
    border-radius: 99px;
  }

  .ptrtag {
    font-size: 10.5px;
    font-weight: 650;
    padding: 1px 8px;
    border-radius: 99px;
    background: rgba(74, 58, 167, 0.12);
    color: var(--k-slot);
  }

  .linewarn {
    font-size: 10.5px;
    font-weight: 650;
    padding: 1px 8px;
    border-radius: 99px;
    background: rgba(250, 178, 25, 0.14);
    color: var(--st-warn);
    cursor: help;
  }

  .rootrow {
    font-size: 12.5px;
    color: var(--ink2);
    margin-bottom: 12px;
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .rootrow .src {
    font-size: 11px;
    color: var(--muted);
  }

  .addrow {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }

  .addrow.center {
    justify-content: center;
    margin-top: 14px;
  }

  .addinput {
    font: inherit;
    font-size: 12px;
    padding: 6px 10px;
    border: 1px solid var(--border);
    border-radius: 9px;
    background: var(--surface);
    color: var(--ink);
    width: 320px;
  }
</style>
