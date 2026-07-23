# Lifecycle operations

Reference for the destructive / state-touching engram commands.
Read this before running anything that mutates wiki + db, especially
on a homelab box where mistakes are harder to undo.

## TL;DR - safety matrix

| Command | Safe with server **running**? | Wipes data? | Reversible? | Notes |
|---|---|---|---|---|
| `purge-project --confirm` | ✅ yes | the one project's data | no | Atomic `rm -rf <project_root>` on the namespaced disk path; sibling projects untouched. |
| `rename-project --from --to` | ✅ yes | no | yes (rename back) | Column-only update on `projects.name`. The on-disk dir is keyed by `project_id` (UUID), so the rename never moves a file. |
| `move-project --confirm` | ✅ yes | source only in the merge case (a `Reject`-policy `purge_project` webhook can still abort the source teardown leaving everything intact) | no | Fresh destination → lossless **true move** (re-stamp `workspace_id`, keep `project_id`, rename the dir): sessions/observations/handoffs + history all survive. Destination with a same-named project → **copy+purge merge**: only latest pages migrate. |
| `backup --output-path` | ✅ yes | no | n/a | Streams a gzipped tarball from the server's online `sqlite3 .backup` plus the wiki tree. Safe alongside the live writer. |
| `checkpoints` | ✅ yes | no | n/a | Lists recent wiki git checkpoints. Read-only. |
| `restore-page --path --from` | ✅ yes | overwrites one markdown page version | yes (restore another checkpoint) | Restores one page from wiki git history, reindexes it into SQLite, and writes a post-restore checkpoint. Does not restore DB-only state. |
| `restore --from <tarball>` | ❌ **stop the server first** | overwrites the data dir | no (without prior backup) | Refuses if any sibling `engram` process is alive (sysinfo guard). |
| `reset --confirm` | ❌ **stop the server first** | yes, all data | no | Refuses if any sibling `engram` process is alive (sysinfo guard). |
| `reindex` | ❌ **stop the server first** | no wiki wipe; requires a clean DB | only with prior DB backup | Rebuilds pages/links/FTS from `wiki/` using `_meta.md` manifests. Refuses if SQLite already has rows so stale DB-only state cannot survive silently. |

State-touching commands route through the HTTP admin API except `reset`,
`restore`, and `reindex`, which are direct-disk lifecycle operations that
fundamentally cannot run while another process holds the SQLite WAL writer. See
[CLAUDE.md §16](../CLAUDE.md) for the invariant.

## What "project isolation" means here

Every project's data lives under an isolated, UUID-keyed root on disk:

```
<wiki_root>/
├── .git/
├── <workspace_id>/
│   ├── _meta.md                 # workspace name for rebuilds
│   └── <project_id>/
│       ├── concepts/
│       ├── decisions/
│       ├── gotchas/
│       ├── sessions/
│       ├── _rules/
│       ├── _meta.md             # project name + repo_path for rebuilds
│       ├── log-YYYY-MM.md      # rolling event log, one file per month
│       └── bootstrap.md
└── <other_workspace_id>/
    └── <other_project_id>/
        └── ...
```

The mutable **project name** (the human-readable `acme-api`
or `web-frontend` you see in `/web/`) never appears in any disk path; the
stable **project_id UUID** does. SQLite's `projects.name` column maps
name → id. Two projects can have the exact same `pages.path` (e.g.
both have `decisions/0001.md`) without colliding on disk - the
namespaced layout guarantees structural isolation.

The git history is rooted at `<wiki_root>` (one repo, all projects
as subtrees). A `git log` from inside the wiki dir shows changes
across every project; per-project diffs are also possible via
`git log -- <workspace_id>/<project_id>/`.

Each workspace directory also carries `<workspace_id>/_meta.md`, and each
project directory carries `<workspace_id>/<project_id>/_meta.md`. Those small
frontmatter-only manifests store human names (plus `repo_path` for projects),
so a clean SQLite DB can be rebuilt from the UUID-keyed wiki tree alone.

## Command-by-command

### `purge-project`

```bash
engram purge-project --workspace default --project my-project --confirm
```

What happens, in order:

1. Server looks up `(workspace_id, project_id)` by name. Returns 404
   if either is missing.
2. Counts rows that will cascade (`pages`, `sessions`,
   `observations`, `handoffs`, `page_embeddings`).
3. Single `DELETE FROM projects WHERE id = ?` - the V01 + V05
   `ON DELETE CASCADE` foreign keys propagate to every dependent
   table in one transaction.
4. `std::fs::remove_dir_all(<wiki_root>/<workspace_id>/<project_id>)`
   wipes the on-disk project root.
5. Returns a summary: `{label, pages_deleted, sessions_deleted, …,
   files_deleted: [<project_root>], files_failed: [...]}`.

Failure modes:

- **Workspace or project name not found** → 404, no mutation.
- **Confirmation flag omitted** → 400, no mutation.
- **`remove_dir_all` partial failure** (e.g. permissions) → DB
   rows are already gone but `files_failed` is populated. Re-run
   the command with the same args is idempotent; the second call
   returns 404 (project already deleted).

Why this is safe with the server running:

- The DB cascade is one transaction; the writer actor serialises
  it against any other writes.
- The on-disk delete touches only the project's UUID-keyed subdir,
  which no other project shares files with. No race with the
  watcher even mid-write - at worst the watcher emits delete
  events for files we just removed, which it ignores (no DB row
  to reindex).

### `rename-project`

```bash
engram rename-project --workspace default --from old-name --to new-name
```

What happens:

1. Look up `(workspace_id, project_id)` by current name. 404 on
   miss.
2. Validate the new name: non-empty, no `/`, no leading/trailing
   whitespace. 422 on bad input.
3. `UPDATE projects SET name = ? WHERE id = ?`. UNIQUE-violation on
   the `(workspace_id, name)` index → 422 with "name taken".
4. Return `{workspace, from, to, pages}`.

Zero files move on disk because the disk path is keyed by
`project_id`, not name. The web UI URL `/web/w/<ws>/<proj-name>/…`
just resolves to the same `project_id` after the column update.

Failure modes:

- **`to` name already exists in this workspace** → 422.
- **`to` invalid (empty, slash, whitespace)** → 422.
- **Source `from` not found** → 404.

### `move-project`

```bash
engram move-project --from-workspace default --project my-project \
  --to-workspace other-workspace --confirm
```

Moves a project into a **different** workspace. Unlike `rename-project`
(a same-workspace column update), this crosses the workspace boundary.
The destination decides which of two strategies runs — reported as
`moved_via` in the response:

**1. Fresh destination → `"true-move"` (lossless, the common case).**
When the destination workspace has **no** same-named project, the move is
a low-level re-stamp:

1. Resolve the source `(from_workspace, project)`. 404 on miss.
2. Reject `from_workspace == to_workspace` (use `rename-project`) → 422.
3. Get-or-create the destination **workspace** row (not a new project).
4. Take the wiki's exclusive mutation gate and run `op=move_project` admission
   webhooks with source names in `ctx.workspace` / `ctx.project` and
   destination names in `ctx.destination_workspace` /
   `ctx.destination_project`. A reject-policy webhook aborts before files or DB
   rows move.
5. While still holding that gate, check that the destination dir is still
   absent, then `fs::rename` the project dir
   `<wiki>/<from_ws>/<proj>` → `<wiki>/<to_ws>/<proj>` (atomic within one
   wiki root).
6. Re-stamp `workspace_id` across every domain table for the project in
   **one transaction**, keeping the same `project_id`
   (`projects`, `pages`, `sessions`, `observations`, `handoffs`,
   `audit_log`). `page_embeddings` and `links` are keyed by `page_id`, so
   they follow with no re-stamp.

Ordering is **rename-FIRST, SQL-commit-LAST**, so the **DB is never ahead of
disk**: a rename failure touches nothing; a crash between the two steps leaves
at most an orphan dir at the destination with the DB still wholly at the source
(recoverable), never a DB row pointing at a missing file. A SQL failure renames
the dir back, so the move is all-or-nothing unless the filesystem also refuses
the rollback, in which case the error names the manual repair. In-process page
writes/reindexes take the shared side of the same mutation gate and validate the
`(workspace_id, project_id)` pair before touching disk, so stale source writes
fail without creating orphan files after the move.

This is O(1) (one transaction + one rename), re-embeds nothing, and
**preserves everything** — sessions, observations, handoffs and the full
supersession history all travel with the project.

**Live-session guard.** The server refuses (409) to move the project the
hook router has published as the *active* project (a live session's next
observation would carry a now-stale `workspace_id`). Pass `--force` /
`force: true` to override — still safe: the move republishes the active
pointer, and the wiki pair validator plus `(workspace_id, project_id)` insert
trigger (V18) reject stale writes cleanly, so the router re-resolves instead of
corrupting or creating old-workspace files.

**2. Destination already has a same-named project → `"copy-purge"`
(merge).** Two distinct `project_id`s can't be re-stamped into one (it
would collide on `UNIQUE (workspace_id, name)`), so the source's latest
pages are copied into the existing destination project via
`Wiki::write_page` (sanitization, link re-resolution, FTS, and — on
deploy — the admission/git-mirror webhooks all fire), source embeddings
are carried over verbatim, and only then is the source purged
(`merged_into_existing: true`, `source_purged: true`).

Copy-before-purge means any copy failure aborts **before** the purge,
leaving the source intact. An unreadable source file is skipped and also
blocks the purge (`source_purged: false`) so a fixed re-run is safe
(re-running is idempotent — copied pages just supersede).

**Same-path conflicts (`on_conflict`).** When a source page's path already
exists in the destination with a different body, frontmatter, title, tier, or
pinned bit, the policy decides (identical pages are always a no-op supersession
at the same path):

- **`block`** (default) — abort the whole move with 409, listing the
  conflicting paths; the source is left intact. The safe default for a
  destructive op: nothing is overwritten or split silently. The operator
  resolves the conflicts or re-runs with an explicit policy.
- **`overwrite`** — the source page supersedes the destination page at the
  same path (the destination's prior version becomes history).
- **`duplicate`** — keep both: the source page lands at
  `<stem>-from-<src_workspace_slug>.md`, then `-2`, `-3`, … on
  further collisions. The `-from-` literal is the `DEDUP_FROM_TOKEN`
  constant in `crates/engram-mcp/src/admin.rs`; if you ever
  change one, change the other. Wikilinks pointing at the original
  path are not rewritten, so the lossless `true-move` path remains
  the way to preserve paths and links.

Every conflict (overwrite/duplicate) is listed in the response `conflicts`
array (`path` → `moved_to`). Set the policy via `--on-conflict` on the CLI
or `"on_conflict": "block" | "overwrite" | "duplicate"` in the JSON body
for direct `/admin/move-project` callers.

**What does NOT migrate (merge case only):** in the `copy-purge` path the
source's `sessions`, `observations`, and `handoffs` (the raw episodic
capture log) are dropped by the purge, and the moved pages start a fresh
supersession chain (the real page history lives in the wiki's git
mirror). The `true-move` path has no such loss.

> **Operational caveat — moving the project the current session writes
> to.** Lifecycle hooks stamp an observation on every tool call into the
> session's project. If you move that very project mid-session, the next
> hook re-creates the source (`scratch`-style) under the old workspace.
> Before moving a live project, point the repo's `.engram.toml` at the
> **destination** workspace first, so new hook events already land there
> and the move is a clean no-contention operation.

Failure modes:

- **Missing `--confirm`** → 400.
- **`from_workspace == to_workspace`** → 422 (use `rename-project`).
- **Source project not found** → 404.
- **Destination workspace directory already exists** (true-move only)
  → 409 with `WikiError::DestinationExists` body — the destination
  has on-disk content for the same `(workspace, project)` UUID pair
  without a corresponding DB row; refuse and let the operator
  reconcile manually.
- **Block-policy same-path conflict** (copy-purge merge only) → 409
  with `{"error": "...", "conflicts": [paths...]}` listing every
  conflicting path. Re-run with `on_conflict=overwrite` or
  `on_conflict=duplicate` to proceed.
- **True-move admission or SQL re-stamp failure** → 500 and no
  committed move. If a rare rollback double-fault happens after the
  directory moved but before SQL committed, the error includes the
  exact manual repair.

### `checkpoints`

```bash
engram checkpoints
```

Lists recent wiki git commits, newest first. The short OID is enough for
`restore-page`, but the JSON output includes the full OID:

```bash
engram checkpoints --json
```

What it is for:

- Finding the checkpoint just before a bad page write, delete, purge, move, or
  restore.
- Inspecting wiki history without shelling into the server's `wiki/.git` repo.

Startup creates a one-time `upgrade baseline: existing wiki tree before recovery
checkpoints` commit for existing data dirs whose wiki repo has zero commits.
Fresh empty installs still have no commit until there is content to save.

### `restore-page`

```bash
engram restore-page --workspace default --project my-project \
  --path notes/foo.md --from <checkpoint>
```

What happens:

1. Server resolves `(workspace, project)` without auto-creating anything.
2. Server validates the page path.
3. Server checkpoints the current wiki tree first (`pre-restore-page ...`) when
   there are uncommitted changes.
4. Server reads the exact markdown blob for that project/page from git at
   `--from`, parses it, writes it back to the live wiki tree, and upserts a new
   latest page row in SQLite so search, links, and `/web` agree with disk.
5. Server writes a post-restore checkpoint (`restore-page ...`) when the live
   tree changed.

Failure modes:

- **Workspace or project name not found** → 404, no mutation.
- **Invalid page path** → 422, no mutation.
- **Checkpoint or file not found** → 500 with the git/libgit2 error; any
  pre-restore checkpoint remains as an audit breadcrumb.
- **Historical markdown is malformed or non-UTF-8** → 500, live file is not
  replaced.

What it does not recover:

- Sessions, observations, handoffs, users, audit rows, access counters, and
  embeddings. Those live only in SQLite and require a full `backup` / `restore`
  if you need to roll them back.

### `backup`

```bash
engram backup --output-path /tmp/engram-backup.tar.gz
```

What happens on the server:

1. SQLite online-backup API copies the live WAL DB to a temp file -
   guaranteed consistent snapshot without stopping the writer.
2. Server tar-gzips the snapshot + the wiki tree + `config.toml`.
3. Response body IS the gzipped tarball
   (`Content-Type: application/gzip`).

CLI writes the response body to `--output-path`. For a homelab user
this is the standard "snapshot before doing something dangerous"
move - `engram backup` first, then proceed.

Restoring a backup follows the inverse. `restore` is a direct-disk operation, so
the server must be **stopped** first — its sysinfo guard refuses while any engram
process is alive:

```bash
# 1. Stop the server. If you keep it running at login, stop that launchd agent
#    (macOS) or Windows service / Task Scheduler task; otherwise just Ctrl-C the
#    `engram serve` process.
# 2. Restore into the data dir.
engram restore --from /tmp/engram-backup.tar.gz \
  --data-dir "$HOME/Library/Application Support/engram" --confirm
# 3. Start the server back up.
```

`restore` writes straight to disk rather than through the HTTP admin API, so
`--data-dir` must name the same data dir the server uses. It defaults to
`~/Library/Application Support/engram` on macOS and `%LOCALAPPDATA%\engram` on
Windows; pass the path explicitly if you set `ENGRAM_DATA_DIR`.

### `restore`

```bash
engram restore --from <tarball> --data-dir <path> --confirm
```

Direct-disk operation. Refuses if any other `engram` process is
alive (uses `sysinfo` to scan the process table).

Order of operations:

1. Check the data dir is empty (or the user passed `--force`).
2. Extract the tarball into the data dir.
3. Restore the SQLite snapshot in place.
4. Print a one-line summary.

Failure modes:

- **Server still running** → exits with "another engram process is
  alive (pid X); stop it before restoring" - same wording as `reset`.
- **`--confirm` omitted** → exits with usage hint.
- **Data dir not empty + no `--force`** → exits with "data dir not
  empty; pass `--force` to overwrite".

### `reset`

```bash
engram reset --confirm
```

Direct-disk operation. Refuses if any sibling `engram` process is
alive. Removes the contents of `wiki/`, `db/`, and `raw/` under the
configured data dir. `config.toml` is preserved.

Identical sysinfo guard to `restore`. The use case is "wipe and start
over" - typically when changing major version with a breaking
migration, or when bootstrapping a new install on top of an old
data dir.

Once the server is stopped you could also just `rm -rf <data-dir>/*` by hand,
but `engram reset` is the safer path: it clears `wiki/`, `db/`, and `raw/`
while preserving `config.toml`, and its sysinfo guard refuses to run while a
server still holds the SQLite writer.

### `reindex`

```bash
engram reindex --data-dir <path>
```

Direct-disk lifecycle operation. Refuses if any sibling `engram` process is
alive, and also refuses if SQLite already contains rows. `reindex` is a
rebuild-from-files path, not an in-place dirty-index repair.

Use it when the markdown wiki is intact but you intentionally want a fresh
SQLite migration lineage:

1. Stop the server.
2. Take a backup of the current data directory.
3. Move or remove `<data-dir>/db/memory.sqlite` and its WAL/SHM siblings.
4. Run `engram reindex --data-dir <data-dir>`.
5. Run `engram embed` after restart if you need embeddings rebuilt.

What is rebuilt:

- Workspaces and projects from `_meta.md`, preserving the UUIDs encoded in the
  wiki directory names.
- Latest page rows, page links, and FTS from markdown files.

What is not rebuilt:

- Sessions, observations, handoffs, users/tokens, audit rows, access counters,
  and embeddings. Those are DB-only state; keep a backup if you need them.

## Operator workflows

### "Fresh start" (wipe everything)

Stop the server, wipe the data dir, and restart:

```bash
# 1. Stop the server (stop the launchd agent / Windows service you run it
#    under, or Ctrl-C the `engram serve` process) so the sysinfo guard passes.
# 2. Wipe wiki/ + db/ + raw/, keeping config.toml.
engram reset --confirm
# 3. Start `engram serve` again.
```

### "Snapshot before risky op"

```bash
engram backup --output-path "/tmp/engram-$(date +%Y%m%d-%H%M).tar.gz"
# … do the risky thing …
# … oh no something broke …
# Stop the server (launchd agent / Windows service, or Ctrl-C).
engram restore --from /tmp/engram-2026-05-23-1530.tar.gz --confirm
# Start the server back up.
```

### "Drop one experimental project, keep everything else"

```bash
engram purge-project --project experimental --confirm
# Sibling projects (engram, acme-api, …) untouched.
```

### "Rename a project after moving its directory"

```bash
engram rename-project --from old --to new
# Future sessions in /path/to/new will append to the same project
# (the hook router stamps by basename(cwd) = "new"); past
# observations stay under that project too because the project_id
# is stable.
```

## Why this matters: the flat-wiki incident

Before the per-project disk layout (commits up to `e7b9a17`), the
wiki was flat: `wiki/<page-path>` regardless of project. Two
projects with the same `pages.path` shared one file on disk. The
`purge-project` handler then iterated and deleted those files,
clobbering pages owned by the sibling project. The DB rows for the
sibling survived (FK is scoped by `project_id`), but every `/web/`
click returned 404 because the on-disk file was gone.

The shipped band-aid was a `path_still_referenced` check before each
delete. The proper fix landed in `e7b9a17`: per-project disk roots
make path-collision structurally impossible. Both the band-aid and
the underlying class of bug are gone. Lifecycle ops are now safe
by construction.

This is also why `rename-project` is free: the disk path is keyed
by surrogate `project_id`, not the mutable name. Rename touches one
column; nothing moves.
