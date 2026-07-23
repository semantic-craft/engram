# engram-importer

Standalone optional companion for importing external memory corpora into a
running engram server. This crate is deliberately isolated from the root
workspace: its `Cargo.toml` has its own `[workspace]`, uses only crates.io
dependencies, and is not included in root `cargo test --workspace`.

## Supported source: OMC wiki directory

The first importer supports oh-my-claudecode / OMC flat Markdown wiki
directories. It reads only top-level `*.md` files, skips `index.md` and
`session-log-*` by default, and writes deterministic destination paths under
`omc/<slug>.md`.

## Supported source: Obsidian folder tree

The `obsidian` subcommand imports one folder of an Obsidian vault (for
example `<vault>/Knowledge`) into a destination path prefix (for example
`knowledge/…`). Unlike the OMC source it:

- walks the folder recursively and preserves original file names,
  including non-ASCII ones (the wiki accepts them as path segments);
- skips underscore-prefixed files/dirs (`_index.md`, `_Templates`) and
  hidden dirs (`.obsidian`, `.trash`);
- passes every frontmatter key not mapped to a dedicated request field
  (`type`, `aliases`, `sources`, `created`, …) through verbatim via the
  `/admin/write-page` `frontmatter` map;
- rewrites Obsidian short-name wikilinks (`[[name]]`, `[[name|label]]`)
  to root-relative destination paths when exactly one imported page's
  file stem matches; ambiguous or unresolved names are left verbatim
  (engram stores them as unresolved forward links) with a warning;
- falls back to the file stem for `title` when the page has neither a
  frontmatter title nor a leading `# H1`;
- applies `--tag <t>` (repeatable) to every imported page.

Claude memory graph and Qdrant imports are roadmap items only; there are no code
stubs for them in v1.

## Safety contract

- Default mode is dry-run; live mode requires `--apply`.
- Live mode requires explicit `--workspace`, `--project`, and
  `--manifest-out <path>`.
- Live writes use only `POST /admin/write-page`; the importer never opens
  engram SQLite or wiki files directly and never deletes pages.
- The destination workspace/project must already exist unless
  `--create-destination` is passed.
- Existing destination pages abort the import unless `--overwrite` is passed.
  The importer also re-checks each page immediately before writing.
  This is best-effort protection: a concurrent writer could still race between
  the check and `/admin/write-page`, so avoid running competing import/write jobs
  into the same destination.
- It stops on the first live-write error and updates the manifest with completed
  writes and the failed checkpoint.
- A write that *times out* is treated as unknown rather than failed. The server
  persists the page row and the on-disk file before it embeds, and embedding a
  long page is one provider call per markdown chunk, sequentially, inside the
  same request — so a timeout usually means the page landed. On timeout the
  importer re-checks the destination: if the page is there it records status
  `uncertain` and keeps importing the remaining pages; if it is absent the write
  genuinely did not land and the run aborts with `failed` as before. An
  `uncertain` page may be indexed without its vectors — run `engram embed` to
  backfill — and re-importing it needs `--overwrite`.
- `--write-timeout-secs` (default 300) bounds a single write-page request.
  Raise it for very large pages against a slow embedding provider; the short
  metadata timeout for preflight/list/exists calls is unaffected.
- Path handling fails closed: absolute paths, `..`, unsafe destination paths,
  and reserved/internal destination prefixes are rejected. Duplicate generated
  destination paths abort planning.
- Dry-run output does not print full page bodies unless `--show-body` is passed.
- The OMC source maps only dedicated metadata fields: `title`, `kind`, `tier`,
  `tags`, `pinned`, and `body`; unknown frontmatter is ignored. The Obsidian
  source additionally passes remaining frontmatter keys through the
  `/admin/write-page` `frontmatter` map, which the server persists verbatim.
- Auth comes only from `ENGRAM_AUTH_TOKEN`; there is intentionally no CLI
  token argument.

## Usage

Dry-run with a summary:

```bash
cargo run --manifest-path companions/engram-importer/Cargo.toml -- \
  omc-wiki --dir /path/to/omc/wiki --workspace default --project my-project
```

Dry-run with a manifest:

```bash
cargo run --manifest-path companions/engram-importer/Cargo.toml -- \
  omc-wiki --dir /path/to/omc/wiki --workspace default --project my-project \
  --manifest-out /tmp/omc-import-manifest.json
```

Live import:

```bash
ENGRAM_AUTH_TOKEN=... \
cargo run --manifest-path companions/engram-importer/Cargo.toml -- \
  omc-wiki --dir /path/to/omc/wiki --workspace default --project my-project \
  --apply --manifest-out /tmp/omc-import-manifest.json
```

Options:

- `--server-url URL`: engram server URL; defaults to
  `http://127.0.0.1:49374`, or `ENGRAM_SERVER_URL` when set. A URL path is
  treated as the base path.
- `--create-destination`: allow `/admin/write-page` to auto-create the
  workspace/project after the read preflight fails.
- `--overwrite`: replace existing destination pages.
- `--include-session-logs`: include `session-log-*` pages (OMC source only).
- `--show-body`: print full page bodies during dry-run.
- `--pinned`: pin all imported pages.
- `--tag <t>`: extra tag applied to every page, repeatable (Obsidian source
  only).

Obsidian dry-run and live import:

```bash
cargo run --manifest-path companions/engram-importer/Cargo.toml -- \
  obsidian --dir /path/to/vault/Knowledge --dest-prefix knowledge

ENGRAM_AUTH_TOKEN=... \
cargo run --manifest-path companions/engram-importer/Cargo.toml -- \
  obsidian --dir /path/to/vault/Knowledge --dest-prefix knowledge \
  --workspace scholar --project academic --pinned --tag academic \
  --apply --manifest-out /tmp/obsidian-import-manifest.json --create-destination
```

## Validation

Run these from the repository root:

```bash
cargo fmt --check --manifest-path companions/engram-importer/Cargo.toml
cargo test --manifest-path companions/engram-importer/Cargo.toml
cargo clippy --manifest-path companions/engram-importer/Cargo.toml --all-targets -- -D warnings
```

Root hygiene checks remain separate:

```bash
cargo fmt --check
git diff --check
```

## Roadmap

- Claude Code memory graph export import.
- Qdrant collection import with user-supplied schema mapping.
- Optional deterministic normalization passes after OMC import is stable.
