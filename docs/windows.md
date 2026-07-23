# Windows Support

engram runs on Windows as a native x86_64 process. Download the prebuilt
`engram-windows-x86_64.zip`, extract it, and run `engram.exe` directly — no
Rust toolchain required. Jump to [Install](#install-prebuilt-release-binary)
for the step-by-step flow.

## Rule Of Thumb

Run `install-mcp` and `install-hooks` from the same PowerShell environment
that launches Claude Code, Codex, Cursor, Gemini CLI, or another agent, so the
generated config points at native Windows paths.

Hook configs contain executable paths, and the hook runner is agent-specific:
Claude Code invokes its hooks with Claude's direct exec form
(`command: "…engram.exe"`, `args: ["hook", "--event", …]`) with no shell — see
[Native Hook Command](#native-hook-command-claude-code-on-windows). Set
`ENGRAM_HOOK_PLATFORM=windows-bash` to fall back to the older `bash -c` + `.sh`
Git Bash commands. Other native Windows script-hook agents keep the PowerShell
`.ps1` default until their harness behavior is verified.

## Install (Prebuilt Release Binary)

This is the standard Windows install. The agent CLI runs as a native Windows
process and gets the fast native hook path **without** installing a Rust
toolchain. Each tagged release publishes `engram-windows-x86_64.zip` (see the
repo's Releases page).

```powershell
# Download + extract into your user data dir (any stable path works; the
# native hook exec-form command is rendered from wherever engram.exe lives).
$Dest = "$env:LOCALAPPDATA\engram"
New-Item -ItemType Directory -Force $Dest | Out-Null
Invoke-WebRequest `
    -Uri "https://github.com/semantic-craft/engram/releases/latest/download/engram-windows-x86_64.zip" `
    -OutFile "$env:TEMP\engram.zip"
Expand-Archive "$env:TEMP\engram.zip" -DestinationPath $Dest -Force
Get-ChildItem "$Dest\engram.exe" | Unblock-File

# Put it on PATH for future terminals (optional but convenient).
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ';') -notcontains $Dest) {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Dest", "User")
    $env:Path = "$env:Path;$Dest"
}

# Wire MCP + lifecycle hooks against your server.
& "$Dest\engram.exe" install-mcp --client claude-code --apply
& "$Dest\engram.exe" install-hooks --agent claude-code --apply `
    --server-url "https://memory.example.com" --auth-token "<token>"
```

The zip mirrors the macOS release tarball layout: it contains `engram.exe`,
the full `hooks/` bundle (`.ps1` + `.sh`),
`crates/engram-cli/templates/config.default.toml`, `README.md`, `LICENSE`, and
`docs/{install,windows}.md`. Because `install-hooks` reads the `engram.exe`
path from the running binary, keep the extracted `.exe` at a stable location
(re-run `install-hooks` if you move it).

## Native Windows Source Build

Use this when developing engram itself on Windows.

```powershell
git clone https://github.com/semantic-craft/engram .\engram
Set-Location .\engram
cargo build --workspace
cargo test --workspace

target\debug\engram.exe init
target\debug\engram.exe serve --transport http --bind 127.0.0.1:49374
```

For release validation from Git Bash on native Windows, use the same checkout
with the Rust MSVC toolchain active:

```bash
cargo test --workspace
cargo build --locked --release -p engram-cli
./target/release/engram.exe --version
```

The version output should match the package version for the checkout.

The Tailwind build step supports the pinned
`tailwindcss-windows-x64.exe` binary and falls back to PowerShell
`Invoke-WebRequest` when `curl`/`wget` are unavailable. You should not
need `TAILWIND_SKIP=1` for normal Windows builds.

Keep Git for Windows' `git.exe` on `PATH` for native builds and hook runs. When
libgit2 hits a Windows path-resolution error while opening a newly initialized
wiki repository, engram falls back to the Git CLI instead of treating that
specific condition as fatal.

From another PowerShell window in the repo:

```powershell
target\debug\engram.exe install-mcp --client claude-code --apply
target\debug\engram.exe install-hooks --agent claude-code --apply
```

Native Windows builds render agent-specific lifecycle hooks. Claude Code
defaults to the native binary command (see below); other script-hook agents
use the PowerShell `.ps1` default. The hook bundle still ships matching `.sh`
and `.ps1` event scripts as a fallback, and tests enforce one-to-one
event/agent parity between them.

## Native Hook Command (Claude Code on Windows)

By default on native Windows, Claude Code hooks are rendered using Claude's
exec form: `command` is the real `engram.exe` path and `args` is an argv
array. This directly spawns the binary instead of sending one quoted string to a
shell or using a `bash -c` wrapper around a `.sh` script:

```json
{
  "type": "command",
  "command": "C:\\Users\\you\\.cargo\\bin\\engram.exe",
  "args": ["hook", "--event", "pre-tool-use", "--agent", "claude-code", "--server-url", "http://host:49374", "--auth-token", "..."]
}
```

This avoids spawning Git Bash plus `cat`/`sed`/`curl` child processes on
every tool call. Process spawning is expensive on Windows, so the native
path is roughly 3-5× faster per hook (measured ~735 ms shell → ~150-205 ms
native on an i7-6700HQ). Notes:

- The binary path comes from the `engram` that runs `install-hooks`, so
  `cargo install --path crates/engram-cli` puts it on a stable
  `~/.cargo/bin` path.
- Exec form requires a real executable path (`.exe`). It does not run `.cmd` or
  `.bat` shims through a shell. `install-hooks` uses the path of the running
  `engram.exe`, so release binaries and Cargo-built binaries work directly.
- The `.sh`/`.ps1` scripts stay bundled as a fallback — the `setup-agent`
  flow (no local binary) keeps emitting the shell command.
- `ENGRAM_HOOK_PLATFORM` accepts four values:
  - `windows-native` — Claude exec-form direct binary call (default on native Windows).
  - `windows-bash` — `bash -c` + `.sh` through Git Bash (the previous
    default; set this to opt back in, or as a fallback for older Claude Code
    builds that do not support exec form).
  - `posix` — POSIX `.sh`. Used by the `setup-agent` flow when the host has no
    local binary; set it explicitly to opt a native install back into the
    scripts.
  - `posix-native` — direct binary call on macOS (`<exe> hook --event …`)
    instead of the `.sh` script, so the hook uses the local event spool +
    OIDC-token fallback. The **default for native macOS Claude Code installs**
    (cargo / release binary), mirroring `windows-native`. The `setup-agent`
    flow forces `posix`, so its host-rendered config keeps the `.sh` scripts.

  Set the env var before running `install-hooks` so the chosen platform
  is baked into the rendered hook commands.

Project auto-scope treats Windows backslashes and POSIX slashes as the same path
separator when comparing hook `cwd`, stored `repo_path`, and the home-directory
catch-all guard. Wrappers or tests that need a host home different from the
process `HOME` can set `ENGRAM_HOME`; it is normalized through the same path
boundary before startup healing or cwd-prefix matching.

### Tuning the spool timings (high-latency instances)

The native hook spools events locally. Session start does a short bounded cleanup
drain before fetching a handoff; session end starts a detached `hook-drain`
helper so Claude Code and other agents are not kept open by a large backlog. The
built-in timings stay short on agent-facing paths, but high-latency or
large-backlog instances can raise them with whole-minute overrides. Unlike
`ENGRAM_HOOK_PLATFORM`, these are read by the hook **at runtime**, so they
apply to the agent's environment (no re-`install-hooks` needed):

| Env var | Built-in default | Max override | What it caps |
|---|---:|---:|---|
| `ENGRAM_HOOK_DRAIN_TIMEOUT_MINUTES` | 3 seconds | 60 minutes | each event POST during a drain |
| `ENGRAM_HOOK_HANDOFF_TIMEOUT_MINUTES` | 3 seconds | 60 minutes | the synchronous `session-start` handoff GET |
| `ENGRAM_HOOK_START_BUDGET_MINUTES` | 3 seconds | 60 minutes | total time `session-start` may spend waiting for the drain lock and cleanup draining |
| `ENGRAM_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES` | 5 minutes | 60 minutes | total time the detached `hook-drain` helper may spend after `session-end` |
| `ENGRAM_HOOK_INCREMENTAL_THRESHOLD` | 32 events | positive integer | spool backlog size that triggers a 250 ms `post-tool-use` catch-up drain |

Timing values must be positive whole minutes. Missing, empty, non-numeric, or
zero values fall back to the built-in defaults; values above 60 are clamped. The
incremental threshold is a positive event count; invalid values fall back to 32.
The session-start budget caps how long the hook may block before handoff fetch;
the background budget caps detached cleanup after session-end and does not keep
the agent waiting.

On Windows, a contended drain lock can be reported as the native
`ERROR_LOCK_VIOLATION` code instead of Rust's `WouldBlock` error kind.
engram treats both as normal lock-busy states, so concurrent drains wait,
skip, or expire according to the same spool timing rules instead of failing the
hook.

## Current Harness Caveats

Windows hook support is new and needs real-world testing against native
Windows agent builds.

- Native Claude Code on Windows invokes hooks as a direct binary call (no
  shell) by default; `ENGRAM_HOOK_PLATFORM=windows-bash` restores the Git Bash
  `bash -c` path.
- Codex, OpenCode, Cursor, Gemini CLI, Grok Build CLI, and OpenClaw may each choose different
  Windows config locations or shell execution behavior. engram uses
  the current best-known defaults, but they need validation on real
  installations.
- MCP over HTTP should be less path-sensitive than hooks, but
  `install-mcp --apply` still writes to a client-specific config file;
  confirm the agent actually loads it.
- OpenClaw, OpenCode, OMP / Oh My Pi, and Pi use generated TypeScript
  integrations rather than the shell hook bundle, so their Windows
  behavior depends on the host runtime loading those files correctly.
  Pi's generated extension also bridges MCP tools because Pi has no native
  `mcp.json` install surface.

## Suggested Test Checklist

1. Run all install commands from PowerShell (or `cmd.exe`) using `engram.exe`.
2. Confirm generated hook commands match the agent: Claude Code should use
   the native `"…engram.exe" hook --event …` command (or `bash -c` + `.sh`
   when `ENGRAM_HOOK_PLATFORM=windows-bash`); other script-hook agents
   should use `.ps1` files under your Windows home directory.
3. Launch the native Windows agent.
4. Call `memory_status` from the agent.
5. Send a prompt, then run `engram status` or `engram recent`.

Report which agent and version you used, and whether the hook command executed
or failed with a path/shell error.
