---
name: engram-project-instruction-maintenance
description: "Use this skill when asked to inspect, diagnose, propose, review, approve, or locally apply maintenance to a repository's CLAUDE.md, AGENTS.md, or other canonical project instruction file through engram. Trigger for project-instruction stewardship, context-budget cleanup, durable rule placement, or pending instruction proposals; keep proposal storage on the server and repository apply on the local host."
---
<!-- engram-managed: routing-skill -->

# engram project-instruction maintenance

Maintain project instructions through a reviewable doctor, propose, review, approve, and local-apply workflow. Treat repository knowledge and user-authored content as protected evidence, not cleanup material.

## Match the requested stage

Do not automatically advance from one stage to the next. Choose only the branch
the user requested, then stop unless the user independently requested a later
stage too.

- **Inspect or diagnose:** inspect the repository's canonical instruction
  policy and current Git status, run `engram instructions doctor --json`,
  report the findings, and stop. Doctor is read-only; do not create a proposal.
- **Propose:** only after an explicit proposal request, choose one cited doctor
  finding or explicit durable rule. Stage it with
  `engram instructions propose --finding ...` or
  `engram instructions propose --rule ...`, then report the proposal ID and
  stop. Add `--semantic` only when the user explicitly requests provider
  assistance.
- **Review an existing proposal:** run
  `engram pending-writes show <id> --json` and
  `engram pending-writes diff <id>` for the requested ID. Report the target,
  context layer, provenance, base hash, token delta, and exact diff, then stop.
  Do not create or approve a proposal merely because review succeeded.
- **Approve:** re-display the current revision with show and diff. Run
  `engram pending-writes approve <id>` only after explicit human approval of
  that displayed revision, then stop. Approval changes server state only; it
  does not authorize an automatic local apply.
- **Apply:** only after a separate explicit request to apply an already
  approved revision, run `engram instructions apply <id> --json` from a local
  host inside the intended repository. Verify the outcome, before/after hashes,
  backup path, and audit actors, then stop. A remote server or MCP client must
  never apply repository bytes.

The server stores proposal and audit state only; proposing, reviewing, and
approving do not change the repository.

Use explicit `--workspace` and `--project` only when the user names a different scope. Otherwise let the CLI resolve the current repository and project.

## Safety boundaries

- Never apply a proposal automatically. Never stage, commit, push, merge, or open a pull request as part of this workflow.
- There is no override path for a dirty target, stale CAS base, changed repository identity, unsafe symlink, ambiguous anchor, malformed marker block, or unsupported canonical/import layout. Stop and report the conflict.
- Preserve protected project knowledge, user-owned bytes, adapter imports, and the engram marker-bounded routing block. Context length alone is not deletion evidence.
- Do not turn generic harness advice into durable project knowledge. Do not broaden a cited finding or rule into unrelated instruction edits.
- Keep server and host authority separate: the server may store proposals, review revisions, provenance, approvals, and audit records; only the local CLI may write the repository target.
- Local apply creates a private backup for updates, leaves the Git index untouched, records a receipt and audit event, and is idempotent on retry. Report these facts; do not perform follow-on Git operations.

## Managed Skill ownership

The routing installer may update this file only when it contains `<!-- engram-managed: routing-skill -->`. A same-name Skill without that marker is user-owned and must be preserved unless the user separately invokes the installer's explicit Skill overwrite option.
