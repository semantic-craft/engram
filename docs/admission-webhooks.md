# Admission webhooks: pre-persistence HTTP hooks

> Operator-configured HTTP hooks invoked on the engine's write path
> (`Wiki::write_page`, `delete_page`, `purge_project`, `move_project`)
> just before the durable mutation commits. Write hooks can mutate the page
> (return a new frontmatter / body); delete/purge/move hooks are notifications
> that can observe, mirror, or reject. Sourced from
> `crates/engram-wiki/src/admission.rs` and the wiring in
> `crates/engram-cli/src/commands/serve.rs` — keep both as the
> canonical reference if anything here drifts.

## 1. What this is (and isn't)

| | What you can do | What you can't do |
|---|---|---|
| Chain | Add canonical frontmatter fields (e.g. `contributors`); mirror the write into an external system (git, search index, audit log); reject writes that fail policy (e.g. `validate-no-secrets`). | Talk back to the engine's writer or store directly — the chain only sees one page at a time and mutates it in place. |
| Engine | Stays closed for modification: every new behaviour ships as an independent HTTP service in any language. | The engine doesn't discover or auto-register webhooks — operators name them in config. |

If your idea doesn't fit "mutate the page or observe the write", it's
probably a different extension point (`/hook` ingress, `/admin/*` admin
surface, or out-of-band scheduled job).

## 2. Lifecycle

The blocking chain fires inside `Wiki::write_page`, **after** the markdown is
parsed and initially sanitised but **before** the atomic write and store upsert.
Webhook mutations are sanitised again before persistence. A mutation applied
by a webhook propagates to both the on-disk markdown file and the SQLite row
in a single atomic step (see
[ARCHITECTURE.md](ARCHITECTURE.md) on the writer actor / atomic write
invariants).

Webhooks run **sequentially**, in the order declared in config. Each one
sees the (possibly mutated) page from the previous webhook — so a
`contributors` hook that adds frontmatter is followed by a `git-mirror`
hook that mirrors the enriched page.

Webhooks fire on these `op` values today (extensible enum):

- `write_page` — direct writes via MCP `memory_write_page`, the CLI
  `write-page`, `/admin/write-page`, the lint rewriter, hook synthesis.
- `consolidate` — LLM consolidation writes from the consolidator
  (SessionEnd opt-in + PreCompact + manual `memory_consolidate`).
- `delete` — a single page is removed (`Wiki::delete_page`, triggered by the
  `memory_delete_page` MCP tool). Carries the page path, no body; fired
  **before** the file is removed so a mirror can `git rm` the same path. The
  SQLite index is deleted directly by the writer actor; the watcher does not
  reconcile delete events.
- `purge_project` — a whole project is purged (`Wiki::purge_project` →
  `remove_dir_all`, routed from `/admin/purge-project`). Carries the
  project in `ctx`, **no** page path; fired before the directory is
  removed so a mirror can drop the project.
- `move_project` — a whole project is moved between workspaces without
  changing `project_id` (fresh-destination true move). Carries the source
  project in `ctx.workspace` / `ctx.project`, destination names in
  `ctx.destination_workspace` / `ctx.destination_project`, and no page path;
  fired before the directory rename + DB re-stamp so a mirror can rename or
  reject the project move.

`delete` / `purge_project` / `move_project` are notifications — there is no
body to mutate; a `Reject`-policy webhook still aborts the operation
(admission fires BEFORE the SQL destruction in both the `/admin/purge-project`
and `/admin/move-project` copy-purge paths, so reject leaves the source
intact). Each webhook opts into the ops it cares about via `events`; the
chain checks the op against `WebhookConfig::events` before dispatching.

A copy-purge `/admin/move-project` fires **two** webhook events from
one request: one or more `write_page` notifications as the pages copy
into the destination, then one terminal `purge_project` notification
when the source is torn down. The `purge_project` event carries
`partial_failure: true` if the SQL purge committed but the on-disk
dir removal failed afterwards.

### What does NOT fire the chain (by design)

- **`log.md` / `log-YYYY-MM.md` appends** — written on every hook event
  (per prompt/tool-call). Routing each through the chain would mean an
  HTTP POST per observation, violating the fire-and-forget hook budget. The
  per-event log is a local audit artifact; back it up out-of-band (batched
  rsync), not per-line.
- **Handoffs** — SQLite rows, transient cross-agent state, not wiki pages.
- **Forget-sweep soft/hard-delete** — DB-only (`is_latest=0` / row delete);
  the markdown file stays on disk, so there is nothing for a file mirror to
  do. (Only `purge_project` removes files in bulk.)
- **`rename-project`** — a `projects.name` column update; the on-disk path
  is the stable UUID, so no file moves and nothing to propagate.
- **External / manual edits on disk** — reconciled by the watcher, not the
  admission chain (the chain is for the engine's own write path).

## 3. Wire contract

### Request (engine → webhook)

```http
POST <webhook.url>
Content-Type: application/json
X-Memory-Op: write_page | consolidate | delete | purge_project | move_project
```

```jsonc
{
  "page": {
    "path": "gotchas/example.md",            // relative wiki path (PagePath)
    "frontmatter": { "title": "...", ... },  // arbitrary JSON, may be null
    "body": "..."                            // markdown body, no frontmatter block
  },
  "ctx": {
    "workspace": "default",                  // resolved name (see §5)
    "project": "engram-ops",              // resolved name
    "destination_workspace": "archive",       // move_project only; omitted otherwise
    "destination_project": "engram-ops",   // move_project only; omitted otherwise
    "actor": {                               // request-layer identity
      "agent": "claude-code",                // claude-code | codex | opencode | hook | cli | …
      "user": "djalmajr",                    // null when unauthenticated
      "sub": "8f3a-...",                     // JWT sub
      "client": "72836f52-...",              // DCR client UUID
      "session_id": "019e6d-..."
    },
    "op": "write_page",                      // write_page | consolidate | delete | purge_project | move_project
    "partial_failure": true                  // purge_project only, and ONLY when set
                                             //   (skipped on the wire when false).
                                             //   true → the DB rows were purged but
                                             //   `remove_project_dir` failed afterwards;
                                             //   a filesystem-tracking mirror (git push)
                                             //   should refuse to drop its own copy.
  }
}
```

The `WebhookRequestBody` / `WebhookPagePayload` / `ActorContext` /
`AdmissionContext` types in
`crates/engram-wiki/src/admission.rs` are the authoritative
serialisation source.

### Response (webhook → engine)

| Status | Body | Behaviour |
|---|---|---|
| `200 OK` | `{ "page": { "frontmatter": ..., "body": ... } }` — both inner fields optional. Anything missing means "leave that field unchanged". | The engine swaps in the returned values before the next webhook (or the final atomic write). |
| `204 No Content` | (empty) | The engine treats the webhook as a pure observer / side-effect — no mutation, no parse. |
| `4xx` / `5xx` | (optional textual body, logged) | See **§4 Failure policy**. |

The engine bounds the response read at `MAX_RESPONSE_BYTES` (1 MiB).
Anything beyond that is treated as a no-op with a `warn` log — webhooks
have no legitimate reason to return more than the page envelope.

## 4. Failure policy

Each webhook picks one when the engine can't reach it or it returns
non-2xx:

- **`ignore` (default, recommended)** — Engine logs a `warn` and
  continues with the unmutated page. The page write still succeeds.
  This is the right choice for everything except safety-critical
  enforcers.
- **`reject`** — Engine aborts the write, propagating the error up to
  the caller. Use this **only** when the webhook is a hard precondition
  for persistence (e.g. a future `validate-no-secrets` enforcer).

A webhook that subscribes to multiple ops uses the same policy across
all of them.

## 5. Workspace / project names

The engine resolves `workspace_id` and `project_id` into the same
human-readable names the UI and on-disk wiki use, so webhooks can
address pages by name without re-implementing UUID lookup. Resolution
happens just before the chain fires; both fields are empty when the
wiki was built without [`Wiki::with_store_reader`] (e.g. legacy
embedders, tests that wire a chain without a reader).

External webhooks should treat the names as opaque strings (workspace /
project values use the same validation as `--workspace` / `--project`
CLI flags). They are stable for the lifetime of the workspace / project
— the engine doesn't rename them silently. `rename-project` is a manual
op that an operator runs and that will eventually trigger a fresh
webhook fan-out if you wire one.

## 6. Loop prevention

A webhook that turns around and writes back to the engine (e.g. via
`/admin/write-page` or `memory_write_page`) must include the header

```
X-Memory-Skip-Admission-Chain: <name>[,<name>...]
```

on its re-entrant call. The engine matches the CSV against
`WebhookConfig::name` and short-circuits those hooks for that write, but only
for trusted re-entry (root/auth-disabled requests). Regular DB-user writes
cannot set this header to bypass a reject-policy webhook. Without the skip
header on trusted re-entry, you get infinite recursion (engine → webhook →
engine → webhook → ...). The header propagates only for the single re-entrant
write; the next external write picks the chain back up normally.

## 7. Limits

Constants are exported from the `engram-wiki` crate root:

| Constant | Value | What it caps |
|---|---|---|
| `MAX_ADMISSION_WEBHOOKS` | `16` | Chain length. `AdmissionChain::new` errors out beyond this — a misconfigured template (helm loop, duplicated block) can't push N hooks into the write-path. |
| `MAX_RESPONSE_BYTES` | `1 MiB` | Webhook response body. Beyond this the response is dropped (treated as no-op + `warn`). |
| Per-webhook `timeout_ms` | operator-set (default `2000`) | Single request. The chain is sequential, so total worst case ≈ `Σ timeout_ms`. |

## 8. Configuration

`config.toml`:

```toml
[[admission_webhooks]]
name = "contributors"                                    # stable identifier (used by skip list + logs)
url  = "http://contributors.memory.svc.cluster.local:8080/enrich"
timeout_ms = 2000                                        # per request
failure_policy = "ignore"                                # ignore | reject
events = ["write_page", "consolidate"]
blocking = true                                          # runs synchronously; may mutate / reject

[[admission_webhooks]]
name = "git-mirror"
url  = "http://git-mirror.memory.svc.cluster.local:8080/sync"
timeout_ms = 2000
failure_policy = "ignore"
events = ["write_page", "consolidate", "delete", "purge_project", "move_project"]
blocking = false                                         # fire-and-forget after the write; never blocks it
```

### `blocking` (default `true`)

A webhook is either **blocking** or **non-blocking**:

- **`blocking = true`** (default) — runs *synchronously* inside the write path.
  It can mutate the page (`write_page`/`consolidate`), and a `reject` failure
  aborts the write. The write waits for it (up to `timeout_ms`). Use for
  enrichers/validators (e.g. `contributors`, `validate-no-secrets`).
- **`blocking = false`** — dispatched *fire-and-forget* **after** the durable
  operation has completed. For writes that means the final page is on disk and
  indexed in SQLite; for deletes that means the file and index row are gone;
  for project purges that means the DB purge has completed and filesystem
  removal has been attempted; for project moves it means the directory and DB
  rows now point at the destination workspace. The engine does not wait for it
  and ignores its response, so it **cannot mutate or reject** — it only
  observes/mirrors the final state. Use for pure backups/mirrors (e.g.
  `git-mirror`) so a slow or down sink never adds latency to writes. Still
  honours `events` and the skip list.

Since the blocking chain is sequential, total worst-case write latency is
`Σ timeout_ms` over the **blocking** webhooks only; non-blocking ones add none.

Env override:

```bash
ENGRAM_ADMISSION_WEBHOOKS_JSON='[{"name":"contributors","url":"http://contributors.memory.svc.cluster.local:8080/enrich","timeout_ms":2000,"failure_policy":"ignore","events":["write_page","consolidate"],"blocking":true}]'
```

The JSON env var is canonical for webhook lists because the figment env layer
does not reliably reconstruct `Vec<Struct>` from indexed nested variables.

Empty config = no chain attached → zero overhead per write (no client
built, no per-write branch).

## 9. Worked examples

### Mutating: append the writer to `frontmatter.contributors`

```jsonc
// POST /enrich
{
  "page": { "path": "gotchas/x.md", "frontmatter": { "title": "X" }, "body": "..." },
  "ctx":  { "workspace": "default", "project": "engram-ops",
            "actor": { "agent": "claude-code", "user": "djalmajr", "client": "72836f52-..." }, ... }
}

// → 200 OK
{
  "page": {
    "frontmatter": {
      "title": "X",
      "contributors": [
        { "agent": "claude-code", "user": "djalmajr", "client": "72836f52-...",
          "first_seen": "...", "last_seen": "...", "writes": 1 }
      ]
    }
  }
}
```

The engine replaces `frontmatter` with the returned object before
persisting. `body` is left untouched (not in the response).

### Side-effect: mirror the write into an external git repo

```jsonc
// POST /sync (same request body)
// → 204 No Content
```

The webhook materialises the page into a local clone of an external
repo, batches commits, and pushes asynchronously. The engine doesn't
wait for the push — only the local enqueue runs inside the write path
under the webhook's `timeout_ms`.

## 10. Tests

`crates/engram-wiki/tests/admission.rs` covers the wire contract end
to end against an axum loopback server. Categories:

- Mutating frontmatter and body propagates correctly.
- `204` is a no-op (frontmatter / body unchanged).
- `failure_policy=ignore` swallows errors; `failure_policy=reject`
  aborts.
- Multi-webhook chain runs in declared order; each sees the previous
  mutation.
- `X-Memory-Skip-Admission-Chain` short-circuits named hooks.
- `X-Memory-Op` header is set correctly per op.
- `op`-filtering: webhooks only fire on subscribed events.
- `MAX_ADMISSION_WEBHOOKS` rejection at construct time.
- `MAX_RESPONSE_BYTES` cap drops oversized responses.
- `workspace` / `project` resolution propagates into the payload.

`crates/engram-wiki/src/wiki.rs::tests::write_page_resolves_workspace_and_project_names_for_chain`
covers the integrated path (`Wiki::write_page` → store reader resolution
→ chain → recorded payload).

## 11. Where to look in source (the canonical spec)

| Concept | File:line |
|---|---|
| `AdmissionContext` / `ActorContext` / wire structs | `crates/engram-wiki/src/admission.rs` |
| `AdmissionChain::run` (the hot loop) | `crates/engram-wiki/src/admission.rs` |
| Invocation inside `write_page` (resolution + chain call) | `crates/engram-wiki/src/wiki.rs::Wiki::write_page` |
| Config schema (`[[admission_webhooks]]`) | `crates/engram-cli/src/config.rs::Config::admission_webhooks` |
| Server wiring (`with_admission_chain` + `with_store_reader`) | `crates/engram-cli/src/commands/serve.rs` |
| Header → `ActorContext` mapping (mcp-auth → engine) | `crates/engram-mcp/src/actor.rs` |

## 12. Non-goals (planned iterations, not blockers)

- **Parallel fan-out.** The chain is sequential by design today —
  mutation composition is well-defined that way. A future
  `parallel = true` for side-effect-only webhooks (no body mutation
  expected) is possible but not in scope.
- **Webhook discovery / dynamic registration.** Hooks are operator-named
  in config. A future `/admin/admission-webhooks` POST surface is
  conceivable but explicitly out of scope here — single-tenant config
  is simpler and matches how the engine treats every other extension
  point (`[[etl_sources]]`, etc.).
- **Per-webhook metrics surface.** The chain logs via `tracing` today.
  Surfacing per-webhook counters via `/admin/status` is a natural
  follow-up but lives outside this contract.
