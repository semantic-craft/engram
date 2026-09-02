# Day-to-day usage

This page covers what happens after engram is installed: handoffs,
compaction recovery, proactive memory queries, the web UI, and the
managed routing snippet + Agent Skills package.

## Cross-agent handoff

You normally do not create handoffs by hand. With lifecycle hooks
installed, session-end capture writes a WorkItem/Handoff and the next
session-start hook discovers it without changing state.

```text
$ claude
> Working on the auth refactor. JWT rotation is broken; trying session cookies.
[work for an hour]
> /exit

$ codex   # in the same directory, later
[SessionStart hook discovers the handoff; Codex sees it before your prompt.]
> Picking up: you were investigating session cookies as an alternative...
```

If an agent has MCP but no lifecycle hook surface, ask it to call
`memory_handoff_begin` before quitting. The next agent discovers the open
Handoff, claims its exact revision for the current Run, and acknowledges it
only by writing the first durable checkpoint. Merely reading or claiming does
not complete the WorkItem. Claims have bounded leases, so another receiver can
recover the task if the first disappears.

`memory_handoff_begin` and `memory_checkpoint_write` can attach typed
ArtifactRefs and explicit WorkItem relationships, and both responses return
those refs and relationships with stable identities and revisions. The first
receiving checkpoint with a live Claim may attach artifacts and relationships
while acknowledging the Handoff. File identity is repository (or project)
coordinates plus a repository-relative locator; Git identity is repository plus
revision; worktree identity distinguishes dirty checkouts at the same commit.
Absolute cwd is a hint, not identity, and is rejected as a worktree locator.
Observation metadata (source Run, timestamp, provenance, dirty, local-path
hint, git ref, content hash, tree hash) belongs to the attachment, not the
shared identity. Deleting the first writer's project does not CASCADE through
the shared identity into another project's attachments. SessionStart
pending-handoff markdown lists artifacts (kind, locator, repository/revision,
id) and relationships (kind, from/to ids) without copying claim ids or treating
cwd as identity. Delivery facts stay independent: committed does not imply
pushed, and verified evidence that names an older revision is stale. Related
work uses `depends_on`, `derived_from`, or `child_of` and creates a new
WorkItem instead of inheriting a claim or blocker. Engram records these facts;
it does not run Git checkout, commit, push, merge, release, or deploy.

If an agent creates a handoff by mistake, cancel it immediately with
`memory_handoff_cancel` with the id, revision, and source Run returned by
`memory_handoff_begin`. Only that source owner can expire an open Handoff.

Claims, releases, and checkpoints require a fresh caller-supplied `attempt_id`.
If a response is lost, retry the identical request with the same Attempt id;
Engram returns the recorded result without extending a lease or duplicating a
transition. Reusing an Attempt for a changed actor, scope, target, revision,
Run, lease, or checkpoint payload fails closed.

## Compaction recovery

When Claude Code or Codex compact their working context, the
`PreCompact` hook fires and engram writes a fresh
`sessions/<id>.md` page summarising the session so far. After
compaction, the agent can recover the summary via `memory_recent` even
though its raw chat history was compacted away.

## Proactive memory queries

Hooks handle capture without prompting. Proactive querying depends on
the agent knowing which MCP tool to call for each situation. Install the
managed routing package once: a slim always-loaded snippet points agents
at the managed engram Agent Skills that carry detailed tool routing.

| You say | Agent calls | Effect |
|---|---|---|
| "Have we discussed X?" / "search memory for Y" | `memory_query` with `context_budget` | Existing FTS + graph/vector RRF candidates become a deterministic brief/overview/full-evidence package with stable ContextRefs and trace. |
| Before proposing architecture | `memory_query` with `context_budget` | Checks prior decisions and gotchas within a caller-bounded context package. |
| "Show the exact evidence for this result" | `memory_context_read` | Resolves the selected ContextRef's exact source revision through an existing, fail-closed scope. |
| "Catch me up" / "I've been away" | `memory_explore` | Prose digest whose verbosity scales with time since last activity. |
| "Where did we leave off?" | Existing handoff block, or `memory_handoff_discover` if no block exists | Reads the latest claimable Handoff without mutation. |
| "Continue that work" | `memory_handoff_claim` with `context_budget`, then `memory_checkpoint_write` | Claims an exact revision, returns the shared ContextPackage, and acknowledges receipt with the first durable checkpoint. |
| "I cannot continue this claim" | `memory_handoff_release` | Reopens the exact live Claim for another receiver. |
| "Save context for the next session" | `memory_handoff_begin` | Creates or continues a WorkItem and publishes a terse open Handoff. |
| "Discard that handoff" / "I created a handoff by mistake" | `memory_handoff_cancel` | Lets the source owner expire an exact open Handoff revision. |
| "Consolidate this session" | `memory_consolidate` | Manually runs LLM consolidation. Also runs on PreCompact, and at session end only when `ENGRAM_CONSOLIDATE_ON_SESSION_END` is set (off by default; session end otherwise writes a rule-based summary page). |
| "What did we learn from this session?" / "what memory should we add?" | `memory_auto_improve` | Manually reviews the latest completed session by default. The server also runs scheduled auto-improvement for new completed sessions when an LLM is configured. `[auto_improve.scheduler] enabled = false` disables automatic review; `[auto_improve] require_approval = true` leaves scheduled and manual proposals in pending-writes for review. |
| "Remember this permanently" / "add an annotation" | `memory_write_page` | Writes durable wiki knowledge; not operational WorkItem state. |
| "Delete this page" / "remove the note about X" | `memory_delete_page` | Removes a page by exact path. Pass `workspace` + `project` together when the page lives in a sibling workspace, so a project name shared between workspaces never silently routes the delete to the wrong slot. |
| "Audit the wiki" / "any contradictions?" | `memory_lint` | Runs stale-page, contradiction, and rule-suggestion checks. |
| "How big is the wiki?" / "stats?" | `memory_status`, `memory_briefing` | Counts and recent activity windows; `memory_briefing` is read-only. |

Agents should treat retrieved memory as operating guidance. When a package
selects matching `_rules/`, `gotchas/`, `procedures/`, or `decisions/` context,
resolve the ContextRef's full evidence before acting: rules are constraints, gotchas are preflight warnings,
procedures are checklists, and decisions are settled architecture unless the
user explicitly asks to revisit them.

## Audit project instruction chains

Run the instruction doctor before deciding which project instruction file to
maintain:

```bash
engram instructions doctor
engram instructions doctor --json
```

The command is deterministic and read-only. It does not load Engram
configuration, construct an LLM provider, open the wiki or SQLite store, create
an audit record, or modify instruction files. The human report is intended for
terminal diagnosis; `--json` emits the same facts as a versioned schema for
automation.

Claude Code and Codex are formally supported. For Claude Code, the report
models ancestor, root, local, imported, `.claude/rules/`, and descendant
on-demand sources, including `paths` frontmatter and the five-hop `@path`
import limit. For Codex, it models root-to-current-directory selection,
`AGENTS.override.md` precedence, configured fallback filenames, and
the `project_doc_max_bytes` limit. Global instruction files themselves remain
outside this project-scoped inventory. Recognized files for other harnesses are
inventory-only and carry `support = "best_effort"`; the doctor does not infer
their loading order.

Formal support follows the current primary documentation for
[Claude Code instruction loading](https://code.claude.com/docs/en/memory) and
[Codex `AGENTS.md` discovery](https://learn.chatgpt.com/docs/agent-configuration/agents-md).

Each source includes line, byte, and estimated-token counts plus symlink,
Engram marker, and routing-asset status. Each chain entry includes its order,
classification (`canonical`, `adapter`, `tool_specific`, `path_scoped`, or
`override`), load mode, effective state, loaded bytes, and the reason for the
decision. Split independent files and near-duplicate `CLAUDE.md` / `AGENTS.md`
files remain ambiguous unless an import, thin pointer, safe in-repository
symlink, or explicit configuration establishes one source. Malformed,
duplicate, crossed, nested, or incomplete routing markers are reported only;
the doctor never repairs them.

Schema version 2 adds `placement_findings`. Every placement finding includes a
content category, source line range when applicable, recommended `action`,
`destination`, protection status, concise evidence, rationale, and related
locations. The destination vocabulary is intentionally small:

| Destination | Meaning |
|---|---|
| `root_instructions` | Keep a non-obvious universal project invariant in always-loaded instructions. |
| `path_rules` | Scope a component-specific constraint to its repository subtree. |
| `agent_skill` | Load a multi-step or private procedure only for the relevant task. |
| `wiki` | Retain rationale, evidence, history, and rejected alternatives as durable knowledge. |
| `enforcement` | Back mandatory controls with permissions, hooks, sandboxing, authentication, or an equivalent mechanism. |
| `no_change` | No relocation target is recommended; the paired action says whether to keep, review, or remove the diagnosed text. |

The deterministic analysis reports generic how-to-think phrases, exact and
normalized duplication, opposite-polarity directives, missing relative
repository paths or command files, missing referenced project Skills, and
likely wrong-layer content. It explicitly protects team coding conventions,
private deployment knowledge, internal tool boundaries, database migration
rules, business boundaries, and security requirements as content a model
cannot safely infer. Mandatory security prose is preserved as a requirement
but directed to enforcement; detailed procedures are directed to Skills; and
history or evidence is directed to the Wiki.

These are review findings, not edits. A generic or duplicate finding can
recommend removal only when its evidence is independent of file length. A line
or byte threshold produces `action = "review"` and never a deletion
recommendation by itself. Claude imports remain included in loaded bytes, and
Codex pressure uses the configured combined project-document limit. The
optimization target is correct placement and fewer conflicts, not the shortest
possible root file. An operator may explicitly select one finding for the
proposal-only workflow below; Engram never stages every finding automatically.

To make the source-of-truth choice explicit, add this to the repository-root
`.engram.toml`:

```toml
[instructions]
canonical = "AGENTS.md"
```

The path must be repository-relative, must remain inside the repository, and
must name a readable file. This declaration affects doctor classification
only; it does not change Claude Code or Codex loading behavior and does not
change `install-instructions` output.

## Stage and apply project instruction proposals

`engram instructions propose` turns exactly one explicit evidence source into a
pending `project_instruction` record:

```bash
# Promote one durable Wiki rule into an explicit repository target.
engram instructions propose \
  --workspace default --project my-project \
  --rule _rules/single-writer.md --target AGENTS.md

# Select one deterministic doctor finding. Add --source/--line when a code is
# repeated; the finding always targets the diagnosed readable source.
engram instructions propose \
  --workspace default --project my-project \
  --finding generic_harness_guidance --source AGENTS.md --line 12

# Ask for bounded semantic duplication/conflict/placement assistance over an
# explicitly selected durable rule.
engram instructions propose \
  --workspace default --project my-project \
  --rule _rules/single-writer.md --target AGENTS.md --semantic

# A correction must match user prompts in at least two distinct project
# sessions. A durable review finding must be an authoritative lint-report page.
engram instructions propose \
  --workspace default --project my-project \
  --correction "full gate before merging" --target AGENTS.md --semantic
engram instructions propose \
  --workspace default --project my-project \
  --review-finding _lint/2026-07-29.md --target AGENTS.md --semantic
```

Durable-rule evidence must name an existing `_rules/` page and is read through
the configured Engram server. Doctor evidence is recomputed from the current
repository and must resolve to one readable inventoried instruction source;
context-budget and other source-less audit findings are not proposal targets.
Both paths are deterministic and require no LLM.

`--semantic` is an optional bridge layered on those deterministic paths. It
also accepts repeated project corrections and durable lint-review findings.
The server, not the client or provider, loads and classifies the authoritative
evidence:

- an explicitly selected `_rules/` page is `explicit_user_rule`;
- the same page is `approved_durable_rule` when it came through an approved
  Wiki proposal;
- `--correction` must resolve to `user-prompt` observations in at least two
  distinct sessions in the selected project;
- `--review-finding` must resolve to a latest `_lint/` page with
  `kind: lint-report`;
- deterministic doctor evidence remains tied to the current target snapshot.

The provider gets quoted evidence as data and may return semantic duplication,
semantic conflict, and placement findings. Every finding and the sole optional
proposal must cite an exact substring from a server-loaded source. Citations to
assistant/model text or unknown sources are rejected. New web/issue
instructions, transient or resolved state, secret-shaped input/output, and
destructive relocation without deterministic doctor evidence are rejected.
Rewrites also preserve any target line carrying team, deployment, production,
internal-tool, migration, business, or security context.

The fixed budgets are one provider call, at most 8 evidence items / 12,000
evidence characters (2,000 per item), a 16,000-character target snapshot and
approximately 8,000 input tokens, 4,000 output tokens / 16,000 structured
output characters, one proposal, 12,000 changed characters, and a 32,000
character final body. The JSON response reports calls, evidence, input,
output, proposal count, and changed characters. Validation failures stage no
instruction proposal and are recorded in the existing rejection buffer.

The proposal stores `target_kind = "project_instruction"`, its operation
(`add`, `update`, `stale_delete`, `move_to_skill`, `move_to_path_rule`,
`move_to_wiki`, `move_to_enforcement`, or `no_change`), logical target, context
layer, base SHA-256, exact anchor or owned region, proposed content, unified
diff, estimated token delta, rationale, complete provenance, proposing actor,
and timestamps. The server recomputes the hash, diff, token delta, and approval
binding instead of trusting client-supplied derived fields. Missing evidence,
assistant-only restatements, web or issue instructions, one-off or resolved
transient state, and secret-shaped base/proposed/evidence content are rejected
before staging.

Review uses the existing surface:

```bash
engram pending-writes list --workspace default --project my-project
engram pending-writes show <proposal-id> --workspace default --project my-project
engram pending-writes diff <proposal-id> --workspace default --project my-project
engram pending-writes edit <proposal-id> \
  --content "reviewed instruction wording" \
  --workspace default --project my-project
engram pending-writes approve <proposal-id> \
  --workspace default --project my-project
engram instructions apply <proposal-id> \
  --workspace default --project my-project
engram pending-writes reject <proposal-id> --reason "not a project invariant" \
  --workspace default --project my-project
```

The record is DB-only and persists across server restarts. It creates no Wiki
sidecar and does not change the repository, Wiki target, index, or Git state.
Each edit is an optimistic, hash-bound full-content replacement: the server
rejects stale review state, recomputes the target diff and token delta against
the stored base content, and appends the reviewed content and actor to the
revision audit. `pending-writes approve` binds to that exact revision and moves
only the proposal state to `approved`/apply-ready with a separate deciding
actor. It does not apply the instruction. Repeated identical edits and
approvals are idempotent and do not duplicate audit events.

`instructions apply` must run from the repository that owns the target. It
accepts only an approved `project_instruction` revision, reruns the instruction
doctor to resolve the repository root and logical target, requires the canonical
repository-root plus canonical-target identity hash captured at staging, recomputes the approval
binding, and verifies the stored base SHA-256 and approved ownership boundary
on the atomic writer's final read. The normal
`<!-- engram:start -->` / `<!-- engram:end -->` routing block must remain
byte-identical and disjoint from the
`<!-- engram:approved-rules:start -->` /
`<!-- engram:approved-rules:end -->` owned region. Every managed marker family
must contain at most one complete, ordered pair; duplicate, missing, crossed,
or nested regions require manual repair. A changed existing file is moved into
an unpredictably named private sibling recovery directory before the synced
tempfile is installed with no-clobber semantics; the command
prints that exact path and records it with the before/after hashes and all three
actors. To recover, copy the reported backup over the target after inspecting
both files. An interrupted post-write audit is recoverable only when a
proposal-bound HMAC receipt in local Git metadata authenticates the canonical
target, hashes, outcome, and backup; matching bytes or a lookalike backup alone
conflict. No-op and repeated applies create no backup, and the command never
runs Git stage, commit, or push.

The executor rejects a changed or Git-dirty target, any active merge, rebase,
cherry-pick, or similar ambiguous repository state, ambiguous line anchors,
managed Skill targets, unsupported encodings, unresolved/cyclic/external
imports, repository-escaping paths, and unsafe symlinks before mutation. Safe
in-repository symlinks and imports resolve to the canonical file so an adapter
is not replaced or written twice. Successful writes preserve all bytes outside
the approved boundary, the target's Unix permissions, and its existing LF or
CRLF convention. A last-moment content check runs before the original is moved
into private recovery storage; the proposed tempfile is then installed only if
the target path remains absent. A failed preflight or write changes the proposal to typed
`conflict` or `failed` and appends one diagnostic event with a stable code and
manual repair guidance; it never records `applied`, forces an overwrite, or
tries a merge/rebase fallback.

This first executor supports `add`, `update`, `stale_delete`, and `no_change`.
It rejects `move_to_skill`, `move_to_path_rule`, `move_to_wiki`, and
`move_to_enforcement` because deleting the source without writing the approved
destination would lose instructions. If the local replacement completed but
the audit request was interrupted, rerunning the command can complete the
record only when the target already equals the approved content and a
proposal-bound HMAC receipt authenticates the exact base backup (or the
approved create), canonical target, and hashes. It otherwise fails closed.

Existing `wiki_page` proposals continue to use their current sidecar and Wiki
approval path. Conversely, provider availability, the background scheduler,
`[auto_improve] require_approval`, and Wiki auto-approval behavior cannot
approve or apply a `project_instruction`; only the explicit human review route
can mark it apply-ready. Application remains an explicit local CLI action; the
application endpoint only records its hashes, outcome, actor, and recovery path
and never receives target content or a repository path to mutate.
Semantic assistance is available only through the explicit CLI/admin proposal
path. No repository-writing MCP tool is added, and scheduled learning review
has no repository access: it can at most stage pending data through the same
writer actor and can never approve, apply, or write project instructions.

Agents should load the managed `engram-project-instruction-maintenance` Skill
for this workflow. It keeps the always-loaded routing block small while
requiring this order: read-only doctor, one cited proposal, show/diff review,
explicit human approval, and only then an explicitly requested local apply.
Before any server-scoped step, it resolves the same scope as lifecycle capture:
the closest `.engram.toml` ancestor of the actual working directory wins,
explicit workspace/project values outrank strategy, `repo-root` alone follows
the Git common directory, and the fail-safe default is the actual working
directory basename. A marker found only in a linked worktree's main checkout is
not applicable. The Skill passes the effective `--workspace` and `--project`
explicitly because instruction CLI commands do not inherit MCP auto-scope.
The agent must stop on CAS, dirty-target, marker, import, symlink, repository,
or canonical-layout conflicts and must not stage, commit, push, merge, or open
a pull request as a follow-on maintenance action.

## Install the routing snippet and Agent Skills

From an agent, say:

```text
Install engram routing into this project.
```

The agent calls `memory_install_self_routing` and receives the slim
`markered_block`, marker strings, rules-file hints, managed skill payloads,
skill target hints, and overwrite guidance. It then uses its normal file-edit
tool to preserve unrelated user content, replace or append the
`<!-- engram:start -->` / `<!-- engram:end -->` block, and write each
managed skill below the selected skill root. Skill files are engram-managed
only when they contain the managed marker, so unmanaged same-name skills should
not be overwritten unless the human explicitly forces replacement.

From a terminal:

```bash
engram install-instructions
engram install-instructions --target AGENTS.md
engram install-instructions --print
engram install-instructions --no-skills
```

`install-instructions` installs or updates managed skills by default. Use
`--no-skills` only when you intentionally want a snippet-only refresh.
The CLI replaces only the markered engram block, preserves unrelated content,
preserves a disjoint approved-rules region, permissions, and newline style, and
retains the old file in a private, unpredictably named sibling recovery
directory before changing an existing instruction file.
Malformed, duplicate, crossed, or nested managed markers abort the refresh
without writing.
`install-instructions --print` previews the instruction snippet only; use
`install-skills --print` to preview skill payloads. Skill flags mirror
`install-skills` with an `--skills-` prefix:
`--skills-scope project|global`, `--skills-agent claude-code|agents|both`,
`--skills-target-dir <dir>`, and `--skills-force`.

Auto-detect extends `CLAUDE.md` when it exists, `AGENTS.md` when it
exists, both when both exist, or creates `CLAUDE.md` when neither exists. Use
`--target AGENTS.md` for non-Claude-only projects. The skill target follows the
instruction target unless you override it: `CLAUDE.md` implies
`.claude/skills`, `AGENTS.md` implies `.agents/skills`, and both files imply
both skill roots.

To refresh only the managed Agent Skills:

```bash
engram install-skills
engram install-skills --scope global --agent agents
engram install-skills --agent both --print
engram install-skills --target-dir .custom/skills --force
```

Project-local skill roots are `.claude/skills` for Claude-compatible installs
and `.agents/skills` for cross-client installs. Global roots are
`~/.claude/skills` and `~/.agents/skills`. `--target-dir` points at an explicit
skill root and bypasses scope/agent inference. `--print` previews target paths
and `SKILL.md` contents. `--force` allows replacement of unmanaged same-name
skills; without it, user-authored skills are preserved. Uninstall removes
engram-managed skills from the default project/global roots after marker
validation; custom `--target-dir` roots are a manual cleanup path.

This is prompt packaging only. engram does not run a runtime skill router,
does not store durable memory in `SKILL.md`, and does not turn the
auto-improvement loop into a skill-authoring system. Durable knowledge still
lives in the wiki.

The installed package contains six managed Skills:

- `engram-retrieval`
- `engram-handoff`
- `engram-durable-pages`
- `engram-learning-maintenance`
- `engram-project-instruction-maintenance`
- `engram-routing-install`

The project-instruction Skill documents a CLI workflow, not a new MCP
capability. A remote server stores and audits proposals; only a local CLI in
the intended repository can apply an approved revision.

## Bootstrap an existing project

If you install engram into a project that already has months of
history, the wiki starts empty. `engram bootstrap` seeds it from the
existing repo history and docs.

```bash
export ENGRAM_SERVER_URL="http://localhost:49374"
engram bootstrap --dry-run
engram bootstrap
```

The bootstrap collector reads `git log`, the root README, `docs/`,
project rule files, and Rust module docs, then POSTs the selected
sources to the running server. It requires an LLM provider on the
server. See [Installation cookbook - bootstrap mid-project](install.md#bootstrap-mid-project)
for flags, token budgets, and source priority.

## Migrate from another memory tool

When replacing an existing memory system, treat the old data as untrusted
historical input until you curate it. Do not pipe raw transcripts or old memory
stores directly into engram.

Migration checklist:

1. Export the old memory or history before changing hooks.
2. Keep the raw export as an archive, not as current project truth.
3. Scrub secrets, tokens, credentials, API keys, and raw logs that should not
   become durable memory.
4. Curate the useful material into reviewed Markdown pages under a temporary
   docs directory or directly into `concepts/`, `decisions/`, `gotchas/`,
   `procedures/`, `notes/`, or `_rules/`.
5. If this checkout might be ambiguous, add `.engram.toml` to pin the intended
   workspace/project before importing or installing hooks.
6. Start `engram serve` locally and confirm `engram status` can reach the
   server before touching existing client configs.
7. Import curated material first; avoid importing the full legacy raw history.
8. Verify expected pages are searchable with `memory_query` or `engram search`.
9. Configure MCP and lifecycle hooks for one client at a time.
10. Only after engram capture and retrieval work, disable the old memory
    hooks, plugins, or MCP servers.
11. Search each client config for stale references to the old tool and remove
    stale `Authorization` headers or env vars if bearer auth changed.
12. Restart each agent CLI after changing hooks, plugins, or MCP config.

Client cleanup hints:

- Claude Code: check plugins, hooks, old SessionStart injection, and MCP servers.
- Codex: check MCP config plus session/user-prompt/tool/compaction/stop hooks.
- Gemini CLI and Antigravity CLI: check `settings.json` or equivalent hook/MCP
  config files.
- OpenCode, OpenClaw, and OMP: check MCP config and plugin/extension directories;
  move old memory plugins to a disabled/quarantine directory before deleting.
- VS Code Copilot and Claude Desktop: these are usually MCP-only, so confirm
  whether the old tool was providing capture hooks elsewhere.

If you want a visible startup reminder during the transition, keep it small. A
rules-file note such as “Active memory: engram; legacy export is historical
reference only; use memory_query for retrieval” is safer than dumping large
legacy context into every session.

If you use the ChatGPT/Codex OAuth provider, sign in once before starting the
server with `ENGRAM_LLM_PROVIDER=openai-oauth`:

```bash
engram auth login openai-oauth
engram auth status
```

The login command stores only provider credentials in `<data_dir>/auth.json`.
It is separate from `ENGRAM_AUTH_TOKEN`, which protects MCP, hooks, and the
web UI.

For GitHub Copilot, use the matching provider login before starting the server
with `ENGRAM_LLM_PROVIDER=copilot`:

```bash
engram auth login copilot
engram auth status
```

Copilot auth stores a GitHub user token, then the provider exchanges it for a
short-lived Copilot API token before each LLM call.

## Browse the wiki in a browser

Start the server with `--enable-web` and open
`http://<host>:49374/web`.

```bash
engram serve --transport http --bind 127.0.0.1:49374 --enable-web
```

The web UI is read-only: project list, per-project page tree,
breadcrumbs, rendered markdown, metadata, and FTS5 search. In rendered
pages, `[[wiki links]]` become clickable links to the target page —
`[[path]]`, `[[path|label]]`, `[[project:path]]`, and
`[[workspace/project:path]]` are all supported (resolved against the
current page's project unless the target carries its own scope).
`[[…]]` stays literal inside fenced code (` ``` ` and `~~~` close
only by their own glyph), inline `` `…` `` code, and 4-space-indented
code; external schemes inside the brackets (`http://`, `https://`,
`mailto:`, `data:`, `javascript:`, `vbscript:`, `tel:`, `file:`)
stay literal too. If the server has `ENGRAM_AUTH_TOKEN` set, the
browser uses HTTP Basic auth: leave the username blank and paste the
token as the password. MCP and hook clients continue to use
`Authorization: Bearer <token>`.

To host the web UI under a URL subpath behind a reverse proxy, the
`--base-path` / `--web-slug` flags do the work — see
[`docs/frontend-api.md`](frontend-api.md#6-custom-ui-hosting-and-base-paths)
for the flag semantics and
[`docs/https-via-proxy.md`](https-via-proxy.md#hosting-under-a-subpath)
for the proxy-side walk-through.

## Inspect the raw wiki

The wiki is plain markdown plus git history, stored under `<data_dir>/wiki`
(the data dir defaults to `~/Library/Application Support/engram` on macOS and
`%LOCALAPPDATA%\engram` on Windows; override with `ENGRAM_DATA_DIR`).

```bash
WIKI="$HOME/Library/Application Support/engram/wiki"
ls "$WIKI/sessions/"
cat "$WIKI/sessions/<uuid>.md"

# It's already a local directory — point Obsidian or any markdown viewer at it:
open "$WIKI"

# Time-travel:
git -C "$WIKI" log --oneline
```

## Rules vs facts

Durable project rules belong in the agent's rules file, not only in the
wiki. For Claude Code that is `CLAUDE.md`; for Codex, OpenCode,
Cursor, and Gemini CLI it is usually `AGENTS.md`.

The consolidator classifies compiled observations as `decision`,
`fact`, `rule`, or `gotcha`. Rule-tagged pages are routed to
`wiki/_rules/<slug>.md`, and `memory_lint` reports a suggestion when a
rule looks durable enough to copy into `CLAUDE.md` or `AGENTS.md`.

engram never edits the rules file on its own. The lint suggestion is
the whole workflow: copy the rule if it should apply every turn, ignore
it if it was temporary context.

## Architecture Decision Records (ADRs)

Two facts frame how ADRs and engram interact:

1. **engram never touches files in your repository.** Its wiki lives
   in the server's data dir; the background jobs (consolidation,
   curation, retention decay, auto-improvement) read and write wiki
   pages only. A `docs/adr/` directory in the repo — maintained by hand
   or by a dedicated ADR tool/MCP server (e.g.
   [joshrotenberg/adrs](https://github.com/joshrotenberg/adrs)) — is
   categorically outside engram's write surface. Run both side by
   side without ceremony: the ADR tool owns the canonical log, engram
   owns cross-session recall.

2. **Wiki pages marked `pinned: true` are immutable to automation.**
   Retention decay and curation skip them, and the auto-improvement
   apply path hard-refuses to rewrite them (the proposal is recorded as
   a conflict with the reason). Unpinning is the explicit opt-out.

For decisions recorded *in* the wiki, the managed durable-pages Agent
Skill teaches agents the recipe: `decisions/<slug>.md`, ADR structure
(Status / Context / Decision / Consequences, including rejected
alternatives), `pinned: true`, and supersede-by-new-page instead of
editing history. Ask an agent to "record this as an architectural
decision" and the skill does the rest; the structured shape also
retrieves noticeably better through `memory_query` than free-form
prose.
