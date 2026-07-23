---
name: engram-learning-maintenance
description: "Use this skill for any engram knowledge-base maintenance request: consolidating observations, reviewing session lessons, proposing durable learnings, auditing or linting the wiki, finding contradictions, pruning stale memory, or running auto-improvement. Trigger by semantic intent rather than exact wording."
---
<!-- engram-managed: routing-skill -->

# engram learning and maintenance

Use this skill for compilation, learning review, wiki linting, and cleanup of engram's durable knowledge base.

## Tools in this cluster

- `memory_consolidate` compiles raw session observations into topical wiki pages on demand.
- `memory_auto_improve` reviews a completed session for durable lessons and project-rule proposals.
- `memory_lint` audits the wiki for contradictions, stale guidance, and candidate rule placement.
- `memory_forget_sweep` prunes or proposes removal of old pages when the user asks for memory cleanup.

## Consolidation and learning review

The server may already run consolidation on PreCompact and at session end when configured. Use on-demand consolidation only when the user asks to compile or consolidate what happened.

Use the auto-improvement tool when the user asks what durable lessons should be proposed from a completed session, or during an explicit wrap-up learning review. It reads the latest completed session by default when no session id is provided.

## Approval path

Scheduled and manual learning reviews apply or stage validated edits through the auto-improvement approval path. Admins can disable scheduling, or require proposal approval so pending writes remain staged until approved. Do not imply a proposal was applied unless the tool result says it was applied.

## Dry-run and destructive caution

Prefer read-only linting or proposal mode before destructive cleanup. When a maintenance tool exposes dry-run behavior, use it first unless the user explicitly requested immediate deletion. If no dry run exists for a destructive action, report what would be removed and ask before proceeding.

## What not to learn

Generic engram routing guidance, Agent Skill installation details, and temporary prompt-packaging instructions are not durable project knowledge. Do not turn them into wiki pages or project rules unless the user explicitly asks to remember a project-specific decision.

## Scope default

Default to the current project. Pass workspace and project together only when the user explicitly names a different project.
