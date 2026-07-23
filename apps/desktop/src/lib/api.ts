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

export const listPages = () => invoke<PageSummary[]>("list_pages");
export const readPage = (path: string) => invoke<PageDetail>("read_page", { path });
export const semanticSearch = (query: string) => invoke<Hit[]>("semantic_search", { query });
export const daemonStatus = () => invoke<DaemonStatus>("daemon_status");
export const writePage = (args: WritePageArgs) => invoke<WritePageResult>("write_page", { args });
export const deletePage = (path: string) => invoke<void>("delete_page", { path });
export const adminStatus = () => invoke<Record<string, unknown>>("admin_status");
export const memoryHealth = () => invoke<MemoryHealth>("memory_health");
export const runEmbed = (reembed: boolean, dryRun: boolean) =>
  invoke<EmbedReport>("run_embed", { reembed, dryRun });
export const runSweep = (dryRun: boolean) =>
  invoke<Record<string, unknown>>("run_sweep", { dryRun });
export const runBackup = (filename: string) => invoke<string>("run_backup", { filename });
export const daemonStart = () => invoke<string>("daemon_start");
export const daemonStop = () => invoke<string>("daemon_stop");

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
