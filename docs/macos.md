# macOS Support

macOS (Apple Silicon, arm64) is a supported platform: the workspace test suite
runs on macOS CI and tagged releases publish a native
`engram-macos-aarch64.tar.gz` binary.

On macOS the **native binary** (a prebuilt release or a source build) is the
way to run engram. It binds the server on `127.0.0.1:49374`, and both the MCP
endpoint and the lifecycle hooks talk to that loopback address — which the
native agent can reach and which is already in the default Host-header
allowlist.

Unlike Windows there is only one "path world" on macOS: POSIX paths and POSIX
`.sh` hooks throughout.

## Rule Of Thumb

Run `install-mcp` / `install-hooks` from the same shell that launches Claude
Code, Codex, Cursor, Gemini CLI, or another agent — on macOS that is just your
normal Terminal.

- The agent runs as a native macOS process, so its config must point at a
  **host-reachable** server URL. The `install-mcp` / `install-hooks` commands
  render `http://127.0.0.1:49374`, which works from the host agent.
- Hooks are rendered for one of two platforms:
  - `posix-native` — a direct `engram hook --event …` call. The default for
    native macOS Claude Code installs (cargo / release binary); it uses the
    local event spool + OIDC-token fallback.
  - `posix` — `sh` runs the bundled `.sh` script.

  Set `ENGRAM_HOOK_PLATFORM` before wiring hooks to override the default.

## Scenario A: Prebuilt Release Binary (Recommended, No Toolchain)

Use this when you want a local server plus native hooks without a Rust toolchain.
Each tagged release publishes a macOS (Apple Silicon) tarball.

```bash
# 1. Download the Apple Silicon archive and extract it to a stable location.
mkdir -p ~/Applications/engram && cd ~/Applications/engram
curl -fsSL -O https://github.com/semantic-craft/engram/releases/latest/download/engram-macos-aarch64.tar.gz
tar -xzf engram-macos-aarch64.tar.gz
# `curl` downloads are not Gatekeeper-quarantined, so the binary runs as-is.
# If you downloaded via a browser instead, clear the quarantine flag once:
#   xattr -d com.apple.quarantine ./engram

# 2. Initialise the data dir (defaults to
#    ~/Library/Application Support/engram; override with ENGRAM_DATA_DIR).
./engram init

# 3. Start the server (loopback only).
./engram serve --transport http --bind 127.0.0.1:49374
```

In a second terminal, wire the agent:

```bash
cd ~/Applications/engram
# `install-hooks` auto-discovers the bundled hooks/ directory beside the binary.
./engram install-hooks --agent claude-code --apply
./engram install-mcp --client claude-code --apply
```

Notes:

- The MCP endpoint, capture hooks, and `engram status` work without a token
  in this single-user loopback setup. If you explicitly configure
  `ENGRAM_AUTH_TOKEN` for the server, pass the same token with `--auth-token`
  or export it for CLI commands.
- Keep the extracted `engram` at a stable path; the hook commands reference
  it. Re-run `install-hooks` if you move it.

## Scenario B: Source Build

Use this when developing engram itself. Requires Rust 1.95
(`rust-toolchain.toml`) plus the Xcode Command Line Tools
(`xcode-select --install`); SQLite is bundled and libgit2 is vendored, so no
extra system libraries are needed.

```bash
git clone https://github.com/semantic-craft/engram
cd engram
cargo build --release --workspace
./target/release/engram init
./target/release/engram serve --transport http --bind 127.0.0.1:49374
```

From another shell in the repo, `install-hooks` finds the bundled `hooks/`
automatically (no `--source` needed from the repo root):

```bash
./target/release/engram install-hooks --agent claude-code --apply
./target/release/engram install-mcp   --client claude-code --apply
```

## Hook Platform on macOS

`ENGRAM_HOOK_PLATFORM` selects how hook commands are rendered. On macOS the
two relevant values are `posix-native` (direct binary call; the default)
and `posix` (the bundled `.sh` scripts). Set it before running `install-hooks`
so the choice is baked into the rendered commands. The native hook spools
events locally, does short session-start
cleanup, and starts a detached session-end `hook-drain` helper; the whole-minute
spool-timing overrides are shared with Windows and documented in
[`docs/windows.md`](windows.md#tuning-the-spool-timings-high-latency-instances).

## Troubleshooting on macOS

- **Hooks bundle not found from a release archive:** ensure you extracted the
  whole tarball, not just the binary. Current `install-hooks` probes the sibling
  `hooks/` directory automatically.
- **`"engram" cannot be opened` / Gatekeeper block:** if you downloaded via a
  browser, clear the quarantine flag once with
  `xattr -d com.apple.quarantine ./engram`. `curl` downloads are not
  quarantined.

## Suggested Test Checklist

1. `engram serve --bind 127.0.0.1:49374` starts and logs `bind=127.0.0.1:49374`.
2. `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:49374/mcp` returns
   `405` (reachable; GET not allowed), confirming the loopback server is up.
3. `install-hooks --agent claude-code --apply` writes hook commands that
   reference `http://127.0.0.1:49374` and host-side paths.
4. `install-mcp --client claude-code` renders `http://127.0.0.1:49374/mcp`.
5. Launch the agent, call `memory_status`, send a prompt, then confirm capture
   (`engram status` shows non-zero observations, or query the SQLite
   `observations` table).

Report which scenario you used, the agent and version, and whether hooks
executed or failed with a connect/resolve error.
