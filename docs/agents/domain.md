# Domain Docs

This repository uses a single-context domain-document layout.

## Before exploring

- Read the root `CONTEXT.md` when it exists.
- Read relevant ADRs under `docs/adr/` when that directory exists.
- Use `AGENTS.md` as the canonical project instruction source.
- Use `docs/ARCHITECTURE.md` for the current operational map and
  `docs/design-decisions.md` for historical rationale.
- Read `docs/auto-improvement-loop.md` before changing learning review,
  proposal storage, approval, or prompt-routing behavior.

If `CONTEXT.md` or `docs/adr/` does not exist, proceed without treating its
absence as a task or defect.

## Vocabulary and decisions

Use the repository's established domain terms in specs, issues, tests, and code.
If a proposed change contradicts an existing ADR or documented design decision,
surface the conflict explicitly rather than silently overriding it.

## Intended layout

The root `CONTEXT.md` holds the domain glossary when one is needed. System-wide
ADRs live under `docs/adr/`. Do not introduce per-crate contexts unless the
repository later adopts an explicit multi-context map.
