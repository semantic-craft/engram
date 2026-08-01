<script lang="ts">
  import { listHandoffs, listSessions, type HandoffRow, type SessionRow } from "./api";
  import { relTime } from "./kinds";

  let {
    project,
    onOpenPage,
    onError,
  }: {
    project: string;
    onOpenPage: (path: string) => void;
    onError: (msg: string) => void;
  } = $props();

  let tab = $state<"sessions" | "handoffs">("sessions");
  let sessions = $state<SessionRow[] | null>(null);
  let handoffs = $state<HandoffRow[] | null>(null);
  // 这两个端点比已发布的 daemon 新：404 说明服务端还没升级，
  // 是可解释的状态而不是错误，不该弹全局错误横幅。
  let needsNewerDaemon = $state(false);

  async function load(p: string) {
    sessions = null;
    handoffs = null;
    needsNewerDaemon = false;
    try {
      const [s, h] = await Promise.all([listSessions(p), listHandoffs(p)]);
      sessions = s;
      handoffs = h;
    } catch (e) {
      const msg = String(e);
      sessions = [];
      handoffs = [];
      if (msg.includes("404")) needsNewerDaemon = true;
      else onError(`会话数据加载失败：${msg}`);
    }
  }

  $effect(() => {
    load(project);
  });

  const AGENT_LABEL: Record<string, string> = {
    "claude-code": "Claude Code",
    codex: "Codex",
    "open-code": "OpenCode",
    cursor: "Cursor",
    "gemini-cli": "Gemini CLI",
    "claude-desktop": "Claude Desktop",
    openclaw: "OpenClaw",
    omp: "OMP",
    other: "其他",
  };

  const HANDOFF_STATE: Record<string, { label: string; color: string }> = {
    open: { label: "待接", color: "var(--accent)" },
    accepted: { label: "已接收", color: "var(--st-good)" },
    expired: { label: "已过期", color: "var(--muted)" },
  };

  /** 本地日期键，用于按天分组。 */
  function dayKey(iso: string): string {
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? iso.slice(0, 10) : d.toLocaleDateString();
  }

  function clock(iso: string): string {
    const d = new Date(iso);
    return Number.isNaN(d.getTime())
      ? ""
      : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }

  /** 时长；会话未结束时返回 null（显示「进行中」而不是假的 0 分钟）。 */
  function duration(s: SessionRow): string | null {
    if (!s.ended_at) return null;
    const ms = Date.parse(s.ended_at) - Date.parse(s.started_at);
    if (!Number.isFinite(ms) || ms < 0) return null;
    const min = Math.round(ms / 60000);
    if (min < 1) return "< 1 分钟";
    if (min < 60) return `${min} 分钟`;
    return `${Math.floor(min / 60)} 小时 ${min % 60} 分`;
  }

  let grouped = $derived.by(() => {
    const out: { day: string; rows: SessionRow[] }[] = [];
    for (const s of sessions ?? []) {
      const day = dayKey(s.started_at);
      const last = out.at(-1);
      if (last && last.day === day) last.rows.push(s);
      else out.push({ day, rows: [s] });
    }
    return out;
  });

  let openCount = $derived((handoffs ?? []).filter((h) => h.state === "open").length);
</script>

<div class="ph">
  <h1>会话与交接</h1>
  <span class="sub">
    {project}
    {#if sessions}· {sessions.length} 个会话{/if}
  </span>
  <div class="acts"><button class="btn" onclick={() => load(project)}>刷新</button></div>
</div>

<div class="tabs">
  <button class="tab" class:on={tab === "sessions"} onclick={() => (tab = "sessions")}>
    时间线
  </button>
  <button class="tab" class:on={tab === "handoffs"} onclick={() => (tab = "handoffs")}>
    交接 handoff
    {#if openCount}<span class="tabbadge">{openCount}</span>{/if}
  </button>
</div>

{#if needsNewerDaemon}
  <div class="empty">
    <div class="big">⟳</div>
    当前 daemon 还没有会话/交接接口——这两个视图需要比正在运行的版本更新的 engram
    服务端。升级 daemon 后刷新即可；其余功能不受影响。
  </div>
{:else if tab === "sessions"}
  {#if sessions == null}
    <div class="empty">加载中…</div>
  {:else if sessions.length === 0}
    <div class="empty">
      <div class="big">◌</div>
      还没有会话记录——在该项目里跑一次带 engram hooks 的 agent 会话即可出现。
    </div>
  {:else}
    {#each grouped as g (g.day)}
      <div class="day">
        <span class="dayline"></span>{g.day}<span class="daycount">{g.rows.length}</span>
      </div>
      {#each g.rows as s (s.id)}
        {@const dur = duration(s)}
        <div class="srow" class:live={!s.ended_at}>
          <span class="time">{clock(s.started_at)}</span>
          <span class="agent">{AGENT_LABEL[s.agent] ?? s.agent}</span>
          <span class="dur">{dur ?? "进行中"}</span>
          <span class="obs">{s.observations.toLocaleString()} 观察</span>
          {#if s.cwd}<span class="cwd mono" title={s.cwd}>{s.cwd}</span>{/if}
          {#if s.summary_path}
            <button class="open" onclick={() => onOpenPage(s.summary_path!)}>会话页 →</button>
          {:else}
            <span class="nosum">无摘要页</span>
          {/if}
        </div>
      {/each}
    {/each}
    {#if sessions.length >= 100}
      <div class="note">只显示最近 100 个会话。</div>
    {/if}
  {/if}
{:else if handoffs == null}
  <div class="empty">加载中…</div>
{:else if handoffs.length === 0}
  <div class="empty">
    <div class="big">◌</div>
    还没有交接记录——会话结束时 engram 会自动写一条 handoff。
  </div>
{:else}
  <div class="note top">
    此处为只读审计视图：查看不会消费待接 handoff，下一个 agent 会话仍能正常接收。
  </div>
  {#each handoffs as h (h.id)}
    {@const st = HANDOFF_STATE[h.state] ?? { label: h.state, color: "var(--muted)" }}
    <div class="card hrow">
      <div class="hhead">
        <span class="dot" style="background:{st.color}"></span>
        <span class="hstate" style="color:{st.color}">{st.label}</span>
        <span class="hagent">
          {AGENT_LABEL[h.from_agent] ?? h.from_agent}
          {#if h.to_agent}→ {AGENT_LABEL[h.to_agent] ?? h.to_agent}{/if}
        </span>
        <span class="htime">{relTime(h.created_at)}</span>
      </div>
      <div class="hsum">{h.summary}</div>
      {#if h.accepted_by}
        <div class="hmeta">
          由 {AGENT_LABEL[h.accepted_by] ?? h.accepted_by} 接收 · {relTime(h.accepted_at)}
        </div>
      {/if}
    </div>
  {/each}
{/if}

<style>
  .tabbadge {
    margin-left: 6px;
    font-size: 10.5px;
    font-weight: 650;
    padding: 0 6px;
    border-radius: 99px;
    background: var(--accent-weak);
    color: var(--accent);
  }

  .day {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 11.5px;
    font-weight: 650;
    color: var(--muted);
    margin: 16px 0 4px;
  }

  .dayline {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--grid);
    flex-shrink: 0;
  }

  .daycount {
    font-weight: 400;
    opacity: 0.7;
  }

  .srow {
    display: flex;
    align-items: center;
    gap: 12px;
    font-size: 12.5px;
    padding: 7px 10px 7px 14px;
    border-left: 2px solid var(--grid);
    margin-left: 2px;
  }

  .srow:hover {
    background: var(--hover);
  }

  .srow.live {
    border-left-color: var(--st-good);
  }

  .time {
    font-variant-numeric: tabular-nums;
    color: var(--muted);
    width: 44px;
    flex-shrink: 0;
  }

  .agent {
    font-weight: 600;
    width: 110px;
    flex-shrink: 0;
  }

  .dur,
  .obs {
    color: var(--ink2);
    width: 90px;
    flex-shrink: 0;
  }

  .cwd {
    color: var(--muted);
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
    min-width: 0;
    direction: rtl;
    text-align: left;
  }

  .open,
  .nosum {
    margin-left: auto;
    flex-shrink: 0;
    font-size: 11.5px;
  }

  .open {
    font-family: inherit;
    color: var(--accent);
    background: none;
    border: none;
    cursor: pointer;
    padding: 0;
  }

  .open:hover {
    text-decoration: underline;
  }

  .nosum {
    color: var(--muted);
  }

  .hrow {
    margin-bottom: 8px;
  }

  .hhead {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12px;
  }

  .hstate {
    font-weight: 650;
  }

  .hagent {
    color: var(--ink2);
  }

  .htime {
    margin-left: auto;
    color: var(--muted);
  }

  .hsum {
    font-size: 12.5px;
    margin-top: 6px;
  }

  .hmeta {
    font-size: 11px;
    color: var(--muted);
    margin-top: 6px;
  }

  .note {
    font-size: 11.5px;
    color: var(--muted);
    padding: 12px 2px;
  }

  .note.top {
    padding-top: 0;
  }
</style>
