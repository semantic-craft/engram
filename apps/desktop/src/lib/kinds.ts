import type { PageSummary } from "./api";

/** kind → 中文标签 + CSS 变量名（app.css 里的八色）。 */
export const KINDS: Record<string, { label: string; cssVar: string }> = {
  rule: { label: "规则", cssVar: "--k-rule" },
  decision: { label: "决策", cssVar: "--k-decision" },
  gotcha: { label: "坑", cssVar: "--k-gotcha" },
  procedure: { label: "流程", cssVar: "--k-procedure" },
  concept: { label: "概念", cssVar: "--k-concept" },
  session: { label: "会话", cssVar: "--k-session" },
  slot: { label: "槽位", cssVar: "--k-slot" },
  note: { label: "笔记", cssVar: "--k-fact" },
  fact: { label: "事实", cssVar: "--k-fact" },
};

export const KIND_ORDER = [
  "rule",
  "decision",
  "gotcha",
  "procedure",
  "concept",
  "session",
  "slot",
  "note",
  "fact",
];

/** 展示用 kind：会话页按目录归到 session，未知 kind 归 fact。 */
export function effectiveKind(p: PageSummary): string {
  if (p.path.startsWith("sessions/")) return "session";
  const k = p.kind ?? "fact";
  return k in KINDS ? k : "fact";
}

export function kindLabel(k: string): string {
  return KINDS[k]?.label ?? k;
}

export function kindColor(k: string): string {
  return `var(${KINDS[k]?.cssVar ?? "--k-fact"})`;
}

/** 相对时间（中文，无秒级精度诉求）。 */
export function relTime(iso?: string | null): string {
  if (!iso) return "—";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return iso.slice(0, 10);
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 90) return "刚刚";
  if (s < 3600) return `${Math.round(s / 60)} 分钟前`;
  if (s < 86400 * 2) return `${Math.round(s / 3600)} 小时前`;
  if (s < 86400 * 14) return `${Math.round(s / 86400)} 天前`;
  if (s < 86400 * 60) return `${Math.round(s / 86400 / 7)} 周前`;
  return iso.slice(0, 10);
}

export function fmtBytes(n?: number | null): string {
  if (n == null) return "—";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/** 转义 HTML 后仅恢复 FTS 高亮的 <mark> 标签，供 {@html} 安全使用。 */
export function renderSnippet(s?: string | null): string {
  if (!s) return "";
  const esc = s
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
  return esc
    .replaceAll("&lt;mark&gt;", "<mark>")
    .replaceAll("&lt;/mark&gt;", "</mark>");
}
