# Engram Desktop

Desktop workbench for [Engram](../../README.md) — browse, search, and
curate the long-term memory your AI coding agents accumulate.

Tauri v2 (Rust) + SvelteKit. Talks to the local Engram daemon on
`127.0.0.1:49374`: reads via `/api/v1`, semantic search (CJK-capable)
via MCP `memory_query`.

## Status

Phase 1 (local, read-only) and Phase 2 (edit + local daemon
management) are complete:

- Semantic search box (works for Chinese queries; since the engine's
  trigram shadow index landed, the FTS5 path handles Chinese too)
- Directory tree of memory pages with counts
- Page view: frontmatter chips, markdown body, backlinks
- In-place markdown editing plus page create/delete (writes go through
  `/admin`, with embedding backfill triggered after each save)
- Daemon panel: start/stop via launchd, embedding backfill preview/run,
  memory health (stale/duplicate/orphan) with forget-sweep, backup
  download (restore stays a CLI operation: `engram restore`)
- Daemon status bar
- Phase 2.5: pending-writes approval workbench (cross-project queue,
  per-proposal diff, approve / reject with reason); API client speaks
  bearer auth from `~/.config/engram/engram.env` and scopes to `_global`

Phase 3 (multi-machine peers over SSH tunnels) is planned — see
[`docs/superpowers/specs/2026-07-06-engram-desktop-design.md`](docs/superpowers/specs/2026-07-06-engram-desktop-design.md).
The pre-Phase-2 fixups in [`docs/PHASE-2-TODO.md`](docs/PHASE-2-TODO.md)
are all done.

## Development

```bash
npm install
npm run tauri dev    # needs the Engram daemon running locally
```

Build a sanitized local bundle with `npm run build:release`. It wraps
`tauri build` with `--remap-path-prefix` so the binary carries no absolute
build paths (Rust embeds them in panic/tracing metadata, which would leak the
builder's home directory and username), then fails the build if any such path
survives. When `APPLE_SIGNING_IDENTITY` is set, it also verifies the resulting
Developer ID signature; without that variable, the output is explicitly
local-testing only. Don't ship bundles from a bare `npm run tauri build`.

Public macOS releases use an independent `desktop-vX.Y.Z` tag so the CLI's
`releases/latest` downloads keep pointing at the native engine archives. Run
the complete local release gate with:

```bash
APPLE_SIGNING_IDENTITY="Developer ID Application: …" \
ENGRAM_DESKTOP_NOTARY_PROFILE="<notarytool-keychain-profile>" \
npm run release:macos
```

That command builds, signs, notarizes, staples, mounts, and Gatekeeper-checks
the Apple Silicon DMG, then writes a matching `.sha256` file. The credential
profile stays in Keychain and is never stored in this repository.

The current public build is
[`desktop-v0.2.0`](https://github.com/semantic-craft/engram/releases/tag/desktop-v0.2.0).
It is a workbench for a separately installed, locally running Engram engine;
install the CLI release and start the daemon before opening the desktop app.
