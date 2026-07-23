---
name: engram-handoff
description: "Use this skill for any request whose goal is session continuity across agents or time: finding a pending handoff, resuming previous work, saving next-session context, wrapping up, or discarding a mistaken handoff. Trigger by semantic intent rather than exact wording."
---
<!-- engram-managed: routing-skill -->

# engram handoff

Use this skill for single-use cross-session handoffs. Handoffs are for the next agent, not durable project documentation.

## Tools in this cluster

- `memory_handoff_accept` consumes the pending handoff when the user asks where we left off and no already-fetched handoff block is visible.
- `memory_handoff_begin` creates a terse next-session handoff only when the user is wrapping up, ending the session, or explicitly asks to save context for the next session.
- `memory_handoff_cancel` expires a mistaken pending handoff by exact handoff id.

## Single-use handoff behavior

The SessionStart hook usually fetches and consumes any pending handoff before the agent sees its first prompt. If the current context already contains a pending handoff block, answer from that block directly. Do not call the accept tool again to find it in another project, because handoffs are single-use and the tool will normally return null after SessionStart consumed it.

If no pending handoff block is visible and the user asks where we left off, then use the accept tool. Keep the default current-project scope unless the user explicitly names a sibling workspace and project.

## Creating a handoff

Create a handoff only at session end or when the user explicitly asks to save context for the next session. Do not use handoffs for status checks, briefings, project notes, or permanent memory. Keep the summary to two or three concise sentences, and put details in open questions and next steps bullets.

Lifecycle hooks already capture routine prompts and tool calls, so do not manually write a handoff just to record normal progress.

## Canceling a handoff

Cancel only when the user asks to discard a handoff or you created one by mistake. Use the exact handoff id returned by the begin tool. Cancellation is idempotent from the user's point of view, but it should still target only the known handoff.

## Scope default

Default to the current project. Pass workspace and project together only when the user names a different project. Never pass scope arguments just because the user says this project, here, we, or our work.
