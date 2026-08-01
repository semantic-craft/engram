import { invoke } from "@tauri-apps/api/core";

export interface PageSummary {
  path: string;
  title: string;
  kind?: string;
  tier?: string;
  updated_at?: string;
}

export interface LinkRef {
  path: string;
  title: string;
  kind?: string;
}

export interface PageDetail extends PageSummary {
  body: string;
  pinned: boolean;
  frontmatter: unknown;
  links: LinkRef[];
  backlinks: LinkRef[];
}

export interface Hit {
  path: string;
  title: string;
  snippet?: string;
  rank?: number;
  // Present only on global (cross-project) hits.
  workspace?: string;
  project?: string;
}

export interface DaemonStatus {
  reachable: boolean;
  page_count?: number;
}

export interface WritePageArgs {
  path: string;
  body: string;
  title?: string;
  kind?: string;
  tier?: string;
  tags: string[];
  pinned: boolean;
  // Frontmatter as read from the page: sent as the authoritative base so
  // custom keys survive the edit (and deliberate deletions stick).
  frontmatter?: Record<string, unknown>;
}

export interface WritePageResult {
  page_id: string;
  path: string;
}

export interface HealthPageRef {
  path: string;
  title: string;
  kind?: string;
}

export interface MemoryHealth {
  stale: number;
  duplicates: number;
  orphans: number;
  stale_pages: HealthPageRef[];
  duplicate_pages: HealthPageRef[];
  orphan_pages: HealthPageRef[];
}

export interface EmbedReport {
  embedded: number;
  skipped: number;
  failed: number;
  would_embed: number;
  provider: string;
  model: string;
  dim: number;
}

export interface ProjectSummary {
  workspace_name: string;
  project_name: string;
  page_count: number;
  last_updated?: string | null;
  repo_path?: string | null;
}

export interface BriefingPage {
  path: string;
  title: string;
  kind: string;
  updated_at: string;
}

export interface Briefing {
  counts: {
    pages_latest: number;
    pages_all: number;
    sessions: number;
    observations: number;
  };
  activity_7d: { days: number; sessions: number; observations: number; pages_updated: number };
  activity_30d: { days: number; sessions: number; observations: number; pages_updated: number };
  last_observation_at?: string | null;
  pending_handoff_count: number;
  rules: BriefingPage[];
  slots: BriefingPage[];
  recent_pages: BriefingPage[];
}

export interface Handoff {
  agent: string;
  at: string;
  project: string;
  summary: string;
  open_questions: string[];
  next_steps: string[];
}

export interface OverviewHealth extends MemoryHealth {
  contradictions?: number;
  audited_at?: string | null;
}

export interface Overview {
  handoff: Handoff | null;
  briefing: Briefing;
  health: OverviewHealth;
}

export interface InstructionFile {
  path: string;
  abs_path: string;
  exists: boolean;
  size?: number | null;
  modified_ms?: number | null;
  content?: string | null;
  truncated?: boolean;
}

// ── 页面 / 搜索（project 缺省 = _global，兼容旧调用） ──
export const listPages = (project?: string) =>
  invoke<PageSummary[]>("list_pages", { project });
export const readPage = (path: string, project?: string) =>
  invoke<PageDetail>("read_page", { path, project });
export const writePage = (args: WritePageArgs, project?: string) =>
  invoke<WritePageResult>("write_page", { args, project });
export const deletePage = (path: string, project?: string) =>
  invoke<void>("delete_page", { path, project });
export const semanticSearch = (
  query: string,
  opts?: { project?: string; global?: boolean },
) => invoke<Hit[]>("semantic_search", { query, project: opts?.project, global: opts?.global });

// ── 项目 / 概览 ──
export const listProjectsStats = () => invoke<ProjectSummary[]>("list_projects_stats");
export const projectOverview = (project: string) =>
  invoke<Overview>("project_overview", { project });

// ── 指令文件（本机直读） ──
export const readInstructionFiles = (paths: string[]) =>
  invoke<InstructionFile[]>("read_instruction_files", { paths });
export const discoverProjectInstructions = (root: string) =>
  invoke<InstructionFile[]>("discover_project_instructions", { root });
export const openInEditor = (path: string) => invoke<void>("open_in_editor", { path });

// ── daemon / 运维 ──
export const daemonStatus = (project?: string) =>
  invoke<DaemonStatus>("daemon_status", { project });
export const adminStatus = () => invoke<Record<string, unknown>>("admin_status");
export const memoryHealth = (project?: string) =>
  invoke<MemoryHealth>("memory_health", { project });
export const runEmbed = (reembed: boolean, dryRun: boolean, project?: string) =>
  invoke<EmbedReport>("run_embed", { reembed, dryRun, project });
export const runSweep = (dryRun: boolean, project?: string) =>
  invoke<Record<string, unknown>>("run_sweep", { dryRun, project });
export const runBackup = (filename: string) => invoke<string>("run_backup", { filename });
export const daemonStart = () => invoke<string>("daemon_start");
export const daemonStop = () => invoke<string>("daemon_stop");

// ── 审批台 ──
export interface PendingGroup {
  project: string;
  proposals: PendingSummary[];
}

export interface PendingSummary {
  id: string;
  status: string;
  operation: string;
  target_path: string;
  kind: string;
  title: string;
  confidence: number;
  staged_at: number;
}

export interface PendingDetail {
  summary: PendingSummary;
  rationale: string;
  body_markdown: string;
}

export const pendingQueue = () => invoke<PendingGroup[]>("pending_queue");
export const pendingDetail = (project: string, id: string) =>
  invoke<PendingDetail>("pending_detail", { project, id });
export const pendingDiff = (project: string, id: string) =>
  invoke<{ proposal_id: string; diff: string }>("pending_diff", { project, id });
export const pendingApprove = (project: string, id: string) =>
  invoke("pending_approve", { project, id });
export const pendingReject = (project: string, id: string, reason: string) =>
  invoke("pending_reject", { project, id, reason });
