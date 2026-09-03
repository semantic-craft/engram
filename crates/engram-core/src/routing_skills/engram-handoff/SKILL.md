---
name: engram-handoff
description: "Use this skill for any request whose goal is recoverable task continuity across agents or time: finding or claiming a Handoff, checkpointing or completing a WorkItem, saving next-session context, releasing a claim, or discarding a mistaken Handoff. Trigger by semantic intent rather than exact wording."
---
<!-- engram-managed: routing-skill -->

# engram handoff

Use WorkItems for stable user work and Handoffs for revisioned transfer offers. A read never consumes a Handoff, acknowledgement happens only with the receiver's first durable checkpoint, and acknowledgement is distinct from WorkItem completion.

## Tools in this cluster

- `memory_handoff_begin` creates a WorkItem and open Handoff at session end, or publishes a *successor* Handoff for an exact existing WorkItem owned by the same authenticated actor and source Run. Attach a bounded brief and revisioned ContextRefs; do not copy canonical page bodies into the Handoff. Attach typed ArtifactRefs (`file`, `git`, `worktree`, `external`) and explicit `depends_on` / `derived_from` / `child_of` relationships on this existing tool; do not invent a new MCP tool for artifacts. A WorkItem has at most one `child_of` parent; a second `child_of` is rejected. Absolute cwd is a local-path hint, never identity.
- `memory_handoff_discover` reads the latest claimable Handoff without mutating it, and never delivers a superseded, cancelled, or expired one. A SessionStart handoff block is this same read-only discovery result. Responses expose ArtifactRefs, WorkItem relationships, `latest_checkpoint` (the outstanding acceptance criteria and the revision a successor must assert), and the ordered transfer `chain` with stable identities and revisions.
- `memory_handoff_claim` claims an exact Handoff revision for the authenticated actor and current Run under a bounded lease. Always pass `context_budget` in selected-content UTF-8 bytes. Success returns the claim envelope plus the shared ContextPackage and assembly trace; compare-and-set failure never returns a package. Assembly never marks the Handoff accepted. Retrying an identical Attempt replays the claim and lease without extending them, then assembles again against current evidence — entries always name exact revisions, so re-assembly is what keeps a retry from handing you refs that have since been superseded. Changing the budget, quotas, or already-used set is a different request and needs a fresh Attempt. Resolve a selected ContextRef through the retrieval skill for exact full evidence. A child WorkItem cannot claim its parent.
- `memory_checkpoint_write` appends ordered WorkItem progress, typed ArtifactRefs, and explicit WorkItem relationships. The response returns those ArtifactRefs and relationships with stable identities and revisions. Include the exact Handoff, Claim, and Handoff revision on the receiver's first checkpoint to acknowledge it transactionally; that ack checkpoint may attach relationships. Records status, performs no Git or external mutation. Record `active`, `blocked`, `completed`, or `abandoned` explicitly. Delivery facts (changed, verified, committed, pushed, reviewed, merged, released, deployed, submitted, approved) are independent and never inferred from one another. A child may return `parent_result` evidence; it cannot complete, abandon, claim, or supersede the parent.
- `memory_handoff_release` returns an exact live Claim to `open` when the receiver will not continue. An expired lease is discoverable and claimable by another receiver.
- `memory_handoff_cancel` lets only the source actor and source Run cancel an exact open Handoff at its current revision.

## Publishing a successor

A receiving agent continues work by publishing a successor Handoff for the same WorkItem, never by editing the one it received. Supply the exact `work_item_id`, `expected_work_item_revision`, and `expected_checkpoint_revision` — the `work_item_revision` of the WorkItem's latest Checkpoint, omitted only while it has none. Both values come back from the preceding transition, or from `memory_handoff_discover`'s `work_item.revision` and `latest_checkpoint.work_item_revision`. Either revision being stale is a conflict that mutates nothing; re-read discovery and retry with the current values.

A successful successor records its predecessor Handoff, the exact Checkpoint it was constructed from, and its own source Run/Session, and atomically supersedes only the older transfer still sitting at `open`. Claimed, acknowledged, expired, cancelled, and already-superseded Handoffs are immutable history and stay readable in the `chain` that discovery returns, ordered oldest to newest with source and receiving Run/Session provenance on every hop. Target selector, authenticated actor, execution agent, and Run stay separate dimensions throughout; never substitute one for another.

The four ways a transfer ends are distinct and separately audited: source cancellation, claim release (which returns it to `open`), lease expiry, and supersession by a successor.

## Retry and ownership rules

Every claim, release, and checkpoint uses a fresh caller-supplied Attempt id. If the response is lost, retry the identical request with the same Attempt id to receive the original result. Never reuse that Attempt id with a different actor, scope, target, revision, Run, lease, or checkpoint payload; the server fails such reuse closed.

Keep authenticated actor identity separate from execution-agent and Run identity. Always use the exact identities and revisions returned by discovery or the preceding transition. A live Claim belongs only to its actor and Run. Do not copy opaque Claim ids into notes, logs, or durable wiki pages.

## Creating and completing work

Create a WorkItem only at session end or when the user explicitly asks to save recoverable continuation context. Supply a stable objective and acceptance criteria. Do not use Handoffs for status checks, permanent memory, or routine lifecycle capture.

A terminal WorkItem requires an explicit checkpoint with state `completed` or `abandoned`; merely reading, claiming, acknowledging, ending a Run, or expiring a lease never completes it. Terminal is then irreversible: a `completed` or `abandoned` WorkItem rejects further checkpoints, Handoffs, and claims, every transfer still open or claimed when it closes is retired, and its lease resolved, in the same transaction, and it is never silently reopened. Create a distinct WorkItem and link it with `depends_on`, `derived_from`, or `child_of`. A WorkItem has at most one `child_of` parent, so express any further link as `depends_on` or `derived_from`. Creating related work always creates a new WorkItem; it does not copy the prior WorkItem's active claim, blockers, or other transient state. Cross-project relationships require a complete existing workspace/project pair and fail closed on missing or partial scope.

Engram records observed artifact and relationship status. It does not check out, commit, push, merge, release, deploy, submit, or approve Git or external systems.

## Scope default

Default to the current project. Pass workspace and project together only when the user names a different project. Reads and transitions use no-create scope resolution and fail closed on partial or missing explicit scope.
