---
name: engram-handoff
description: "Use this skill for any request whose goal is recoverable task continuity across agents or time: finding or claiming a Handoff, checkpointing or completing a WorkItem, saving next-session context, releasing a claim, or discarding a mistaken Handoff. Trigger by semantic intent rather than exact wording."
---
<!-- engram-managed: routing-skill -->

# engram handoff

Use WorkItems for stable user work and Handoffs for revisioned transfer offers. A read never consumes a Handoff, acknowledgement happens only with the receiver's first durable checkpoint, and acknowledgement is distinct from WorkItem completion.

## Tools in this cluster

- `memory_handoff_begin` creates a WorkItem and open Handoff at session end, or publishes another Handoff for an exact existing WorkItem owned by the same authenticated actor and source Run. Attach typed ArtifactRefs (`file`, `git`, `worktree`, `external`) and explicit `depends_on` / `derived_from` / `child_of` relationships on this existing tool; do not invent a new MCP tool for artifacts. Absolute cwd is a local-path hint, never identity.
- `memory_handoff_discover` reads the latest claimable Handoff without mutating it. A SessionStart handoff block is this same read-only discovery result. Responses expose ArtifactRefs and WorkItem relationships with stable identities and revisions.
- `memory_handoff_claim` claims an exact Handoff revision for the authenticated actor and current Run under a bounded lease. A child WorkItem cannot claim its parent.
- `memory_checkpoint_write` appends ordered WorkItem progress, typed ArtifactRefs, and explicit WorkItem relationships. The response returns those ArtifactRefs and relationships with stable identities and revisions. Include the exact Handoff, Claim, and Handoff revision on the receiver's first checkpoint to acknowledge it transactionally. Record `active`, `blocked`, `completed`, or `abandoned` explicitly. Delivery facts (changed, verified, committed, pushed, reviewed, merged, released, deployed, submitted, approved) are independent and never inferred from one another. A child may return `parent_result` evidence; it cannot complete, abandon, claim, or supersede the parent.
- `memory_handoff_release` returns an exact live Claim to `open` when the receiver will not continue. An expired lease is discoverable and claimable by another receiver.
- `memory_handoff_cancel` lets only the source actor and source Run expire an exact open Handoff at its current revision.

## Retry and ownership rules

Every claim, release, and checkpoint uses a fresh caller-supplied Attempt id. If the response is lost, retry the identical request with the same Attempt id to receive the original result. Never reuse that Attempt id with a different actor, scope, target, revision, Run, lease, or checkpoint payload; the server fails such reuse closed.

Keep authenticated actor identity separate from execution-agent and Run identity. Always use the exact identities and revisions returned by discovery or the preceding transition. A live Claim belongs only to its actor and Run. Do not copy opaque Claim ids into notes, logs, or durable wiki pages.

## Creating and completing work

Create a WorkItem only at session end or when the user explicitly asks to save recoverable continuation context. Supply a stable objective and acceptance criteria. Do not use Handoffs for status checks, permanent memory, or routine lifecycle capture.

A terminal WorkItem requires an explicit checkpoint with state `completed` or `abandoned`; merely reading, claiming, acknowledging, ending a Run, or expiring a lease never completes it. Creating related work always creates a new WorkItem; it does not copy the prior WorkItem's active claim, blockers, or other transient state. Cross-project relationships require a complete existing workspace/project pair and fail closed on missing or partial scope.

Engram records observed artifact and relationship status. It does not check out, commit, push, merge, release, deploy, submit, or approve Git or external systems.

## Scope default

Default to the current project. Pass workspace and project together only when the user names a different project. Reads and transitions use no-create scope resolution and fail closed on partial or missing explicit scope.
