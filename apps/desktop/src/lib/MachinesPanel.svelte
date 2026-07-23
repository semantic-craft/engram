<script lang="ts">
  import { onMount } from "svelte";
  import {
    adminStatus,
    memoryHealth,
    runEmbed,
    runSweep,
    runBackup,
    daemonStart,
    daemonStop,
    type MemoryHealth,
    type HealthPageRef,
  } from "./api";

  let {
    onSelect,
    onError,
  }: { onSelect: (path: string) => void; onError: (msg: string) => void } = $props();

  let status = $state<Record<string, unknown> | null>(null);
  let health = $state<MemoryHealth | null>(null);
  let busy = $state<string | null>(null);
  let log = $state<string[]>([]);

  function note(msg: string) {
    log = [msg, ...log].slice(0, 20);
  }

  // Unreachable daemon renders as inline hints (status/health = null)
  // rather than spamming the global error banner.
  async function refresh() {
    try {
      status = await adminStatus();
    } catch {
      status = null;
    }
    try {
      health = await memoryHealth();
    } catch {
      health = null;
    }
  }

  onMount(refresh);

  async function op(name: string, fn: () => Promise<string>) {
    busy = name;
    try {
      note(await fn());
    } catch (e) {
      onError(String(e));
    } finally {
      busy = null;
      refresh();
    }
  }

  const start = () => op("daemon", daemonStart);
  const stop = () => op("daemon", daemonStop);
  const embedPreview = () =>
    op("embed", async () => {
      const r = await runEmbed(false, true);
      return `embed 预览：待补 ${r.would_embed} 页（${r.provider}/${r.model}/${r.dim}）`;
    });
  const embedRun = () =>
    op("embed", async () => {
      const r = await runEmbed(false, false);
      return `embed 完成：新增 ${r.embedded}，跳过 ${r.skipped}，失败 ${r.failed}`;
    });
  const sweepPreview = () =>
    op("sweep", async () => `sweep 预览：${JSON.stringify(await runSweep(true))}`);
  const sweepRun = () =>
    op("sweep", async () => `sweep 完成：${JSON.stringify(await runSweep(false))}`);
  const backup = () =>
    op("backup", async () => {
      const ts = new Date().toISOString().slice(0, 19).replace(/[T:]/g, "-");
      const dest = await runBackup(`engram-backup-${ts}.tar.gz`);
      return `备份已保存：${dest}`;
    });

  const healthSections: { title: string; key: keyof MemoryHealth; listKey: keyof MemoryHealth }[] = [
    { title: "陈旧 stale", key: "stale", listKey: "stale_pages" },
    { title: "重复 duplicate", key: "duplicates", listKey: "duplicate_pages" },
    { title: "孤儿 orphan", key: "orphans", listKey: "orphan_pages" },
  ];
</script>

<div class="panel">
  <section>
    <div class="sec-head">
      <h2>本机 daemon</h2>
      <div class="btns">
        <button onclick={start} disabled={busy !== null}>启动</button>
        <button onclick={stop} disabled={busy !== null}>停止</button>
        <button onclick={refresh} disabled={busy !== null}>刷新</button>
      </div>
    </div>
    {#if status}
      <div class="kv">版本 {String(status.version ?? "?")}</div>
      <div class="kv">数据目录 {String(status.data_dir ?? "?")}</div>
      {#if status.bind}<div class="kv">监听 {String(status.bind)}</div>{/if}
    {:else}
      <div class="hint">daemon 不可达（未运行或 /admin 无响应）。</div>
    {/if}
  </section>

  <section>
    <div class="sec-head">
      <h2>Embedding</h2>
      <div class="btns">
        <button onclick={embedPreview} disabled={busy !== null}>预览缺失</button>
        <button onclick={embedRun} disabled={busy !== null}>补 embedding</button>
      </div>
    </div>
  </section>

  <section>
    <div class="sec-head">
      <h2>记忆健康</h2>
      <div class="btns">
        <button onclick={sweepPreview} disabled={busy !== null}>sweep 预览</button>
        <button onclick={sweepRun} disabled={busy !== null}>执行 sweep</button>
      </div>
    </div>
    {#if health}
      {#each healthSections as sec (sec.key)}
        <details>
          <summary>{sec.title} · {health[sec.key] as number}</summary>
          {#each health[sec.listKey] as HealthPageRef[] as p (p.path)}
            <button class="page-link" onclick={() => onSelect(p.path)}>
              {p.title || p.path}
            </button>
          {/each}
        </details>
      {/each}
    {:else}
      <div class="hint">健康数据不可用。</div>
    {/if}
  </section>

  <section>
    <div class="sec-head">
      <h2>备份 / 恢复</h2>
      <div class="btns">
        <button onclick={backup} disabled={busy !== null}>备份到下载目录</button>
      </div>
    </div>
    <div class="hint">恢复请在终端执行：<code>engram restore &lt;备份文件&gt;</code>（先停 daemon）。</div>
  </section>

  {#if log.length}
    <section>
      <h2>操作记录</h2>
      {#each log as line, i (i)}
        <div class="log-line">{line}</div>
      {/each}
    </section>
  {/if}
</div>

<style>
  .panel {
    padding: 1rem 1.5rem;
    overflow-y: auto;
    height: 100%;
    box-sizing: border-box;
  }

  section {
    margin-bottom: 1.25rem;
    border-bottom: 1px solid rgba(0, 0, 0, 0.08);
    padding-bottom: 0.9rem;
  }

  .sec-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    margin-bottom: 0.4rem;
  }

  h2 {
    font-size: 0.95rem;
    margin: 0;
  }

  .btns {
    display: flex;
    gap: 0.4rem;
  }

  .btns button {
    font-size: 0.8rem;
    padding: 0.25rem 0.7rem;
    border-radius: 6px;
    border: 1px solid rgba(0, 0, 0, 0.15);
    background: none;
    cursor: pointer;
  }

  .btns button:hover:not(:disabled) {
    background: rgba(0, 0, 0, 0.06);
  }

  .btns button:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .kv {
    font-size: 0.85rem;
    color: #444;
    padding: 0.1rem 0;
  }

  .hint {
    font-size: 0.8rem;
    color: #888;
  }

  details {
    margin: 0.3rem 0;
  }

  summary {
    font-size: 0.85rem;
    cursor: pointer;
  }

  .page-link {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    padding: 0.25rem 0 0.25rem 1rem;
    cursor: pointer;
    font-size: 0.85rem;
    color: #396cd8;
  }

  .page-link:hover {
    text-decoration: underline;
  }

  .log-line {
    font-size: 0.8rem;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    color: #555;
    padding: 0.15rem 0;
    word-break: break-all;
  }
</style>
