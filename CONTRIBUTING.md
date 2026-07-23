# Contributing to engram

## Dev setup

```bash
git clone https://github.com/semantic-craft/engram
cd engram
cargo build --workspace
cargo test --workspace
```

Rust 1.95 is required (pinned in `rust-toolchain.toml`). The build is
self-contained: SQLite is bundled via `rusqlite`'s `bundled` feature, and
`libgit2` is vendored via `git2`'s `vendored-libgit2` feature. No system
libraries need installing beyond a standard C toolchain.

## Required gates before every PR

All four must pass — the CI workflow enforces them and so does the `bin/release`
script:

```bash
cargo fmt --all -- --check          # formatting
cargo clippy --workspace --all-targets -- -D warnings   # lints
cargo test --workspace              # tests
cargo deny check                    # dependency policy
```

If `cargo-deny` or `cargo-audit` are not installed:

```bash
cargo install cargo-deny cargo-audit
```

## Workflow rules (condensed from CLAUDE.md)

The full authoritative rules are in [`CLAUDE.md`](CLAUDE.md). Short version:

1. Work milestone by milestone. Do not start M(n+1) until every "Done when"
   bullet in M(n) passes (see `docs/design-decisions.md`).
2. No dead code, no half-built features. Stubs are documented with
   `// M<n> TODO` in the module doc-comment.
3. Write tests before claiming done. Parsers, ID derivation, and
   retention/decay math especially.
4. Do not refactor outside the milestone. Only touch what the current
   milestone requires.
5. Comments explain *why*, never *what*. No comments that restate the line
   above them.

## Cross-cutting invariants

Never violate any of the invariants in `CLAUDE.md §Cross-cutting invariants`.
Highlights for contributors:

- All SQLite writes go through the single writer actor (`WriterHandle`).
- Config is read once at startup; never call `std::env::var` outside `Config::load`.
- Atomic file writes only: tmp + rename + fsync; never write in-place.
- Every wiki page is namespaced by `(workspace_id, project_id)`.
- The CLI is always a thin HTTP client to the running server — it never
  opens the SQLite file or the wiki directory directly.

## Versioning and deprecation policy

This project follows [Semantic Versioning](https://semver.org/):

- **Patch** (`x.y.Z`): bug fixes that do not change public API or
  on-disk format.
- **Minor** (`x.Y.0`): additive changes; new CLI subcommands, new MCP
  tools, new config keys. Existing behaviour is preserved.
- **Major** (`X.0.0`): breaking changes. This includes on-disk format
  changes that are not handled by a migration, removal of CLI subcommands,
  or changes to the MCP tool schema that would break existing agents.

Breaking changes only ship in major releases. Deprecated items are
documented in the CHANGELOG under `### Deprecated` and removed no sooner
than the following major release.
