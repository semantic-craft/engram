//! Shared rendering helpers for the install-* / setup-agent commands.
//!
//! These three subcommands (`install-hooks`, `install-mcp`,
//! `setup-agent`) all emit configuration snippets that share two
//! pieces of state:
//!
//! 1. The per-agent lifecycle-hook event lists engram wires up
//!    (Claude/Grok share `CLAUDE_CODE_EVENTS`; Codex, Cursor, Gemini,
//!    and Antigravity define their own profiles) — kept in sync between
//!    hook-bundle generation (setup-agent) and config rendering
//!    (install-hooks).
//! 2. The optional `Authorization: Bearer <token>` header used by
//!    both MCP client configs (install-mcp) and hook env blocks
//!    (install-hooks / setup-agent).
//!
//! Each subcommand still owns its per-client output formatting (the
//! commentary that frames the JSON snippet differs from client to
//! client and is the part that makes the printout readable). What
//! lives here is only the *data* both consume.

use std::borrow::Cow;
use std::path::Path;

use serde_json::{Value, json};

use crate::commands::path_util::strip_windows_verbatim_prefix;

/// Claude Code lifecycle events engram hooks. Each pair is
/// `(event-name-in-Claude-Code-settings, POSIX hook-script-filename)`.
///
/// Adding a hook event means updating this list AND adding the matching `.sh`
/// and `.ps1` files under the agents that use this profile (`claude-code` and
/// `grok`). Agents with different vocabularies keep their own event arrays
/// below. The install-hooks parity test fails if a bundle drifts.
pub(crate) const CLAUDE_CODE_EVENTS: [(&str, &str); 9] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
    // Subagent boundaries — let the server seed/forget a subagent session id so
    // `drop_subagent_captures` can drop the whole nested session (Claude Code +
    // grok both emit these; other agents keep their own event lists).
    ("SubagentStart", "subagent-start.sh"),
    ("SubagentStop", "subagent-stop.sh"),
];

/// Format an `Authorization: Bearer <token>` header value, or `None`
/// when no token is supplied. Used by every MCP client renderer in
/// `install-mcp` and every hook-config renderer that wants to
/// embed an auth token.
///
/// Centralised because the prefix is `Bearer` per RFC 7235 / OAuth
/// 2.1 / the MCP spec — if anyone ever decides to support a
/// different scheme (e.g. `DPoP`) this is the one place that
/// changes.
#[must_use]
pub(crate) fn bearer_header_value(token: Option<&str>) -> Option<String> {
    token.map(|t| format!("Bearer {t}"))
}

/// Emit a TypeScript string literal containing `s`. Escapes
/// backslashes, double quotes, and common control characters. This is
/// sufficient for URL, auth-token, and path strings embedded into
/// generated TypeScript integrations.
#[must_use]
pub(crate) fn ts_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Build the Claude Code `settings.json` fragment that wires the
/// lifecycle hooks (`CLAUDE_CODE_EVENTS`). Used by both:
/// - `install-hooks --agent claude-code` (script paths are
///   wherever the user told us via `--hooks-dir`)
/// - `setup-agent --agent claude-code` (script paths are where
///   `--host-prefix` says they'll live on the host)
///
/// `emit_root` is the directory that will contain hook scripts; it is
/// expected to be an absolute path on the system that will run the
/// agent CLI. This function does NOT verify the path exists on the
/// local filesystem — that decision belongs to the caller because
/// `setup-agent --host-prefix` legitimately renders paths for another
/// machine that don't yet exist locally.
///
/// `auth_token`, when set, lands in each hook's `env` block as
/// `ENGRAM_AUTH_TOKEN`, which the shell scripts forward as
/// `Authorization: Bearer …` to the server.
#[must_use]
pub(crate) fn build_claude_code_payload(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    build_hook_payload_for_platform(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_script_runner(),
            "claude-code",
            None,
            None,
        ),
    )
}

pub(crate) fn build_claude_code_payload_with_data_dir(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    build_hook_payload_for_platform(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_runner(),
            "claude-code",
            data_dir,
            project_strategy,
        )
        .allow_claude_windows_exec(),
    )
}

/// Grok Build CLI hook payload for setup-agent script snippets.
/// Grok shares Claude Code's JSON shape and event vocabulary, but uses
/// its own script bundle so script fallback keeps `agent=grok` and never
/// destructively fetches handoffs on SessionStart.
#[must_use]
pub(crate) fn build_grok_payload(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    build_hook_payload_for_platform(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_script_runner(),
            "grok",
            None,
            None,
        ),
    )
}

/// Grok Build CLI hook payload for apply/render paths. Native commands are the
/// default; explicit script fallback still points at the Grok script bundle.
pub(crate) fn build_grok_payload_with_data_dir(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    build_hook_payload_for_platform(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_runner(),
            "grok",
            data_dir,
            project_strategy,
        ),
    )
}

/// Different agents nest hook entries differently. Two shapes
/// cover everyone we support:
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookShape {
    /// Claude Code / Codex / Gemini CLI:
    /// `"E": [ { "matcher":"", "hooks":[ {"type":"command",
    /// "command":"..."} ] } ]`
    /// Gemini CLI tolerates (but doesn't require) a sibling
    /// `sequential` key at the outer level — we don't set it.
    Nested,
    /// Cursor: `"e": [ { "type":"command", "command":"...",
    /// "matcher":"" } ]` (no inner `hooks` array). Cursor's
    /// `hooks.json` also requires a sibling `version: 1` key at
    /// the top level — handled by the caller's apply path.
    Flat,
}

/// One hook profile = (event vocabulary, JSON shape). Each agent
/// gets its own constant so the install path is purely data-
/// driven: pick the profile, build the payload, write the file.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HookProfile {
    /// `(EventName, script_basename)` tuples in the order the
    /// agent surfaces them. Event names are case-sensitive and
    /// agent-specific — Claude Code uses `SessionStart` while
    /// Cursor uses `sessionStart`. The POSIX script filename resolves
    /// against `hooks/<agent-dir>/`; Windows rendering rewrites the
    /// `.sh` suffix to `.ps1`.
    pub events: &'static [(&'static str, &'static str)],
    /// JSON shape the file uses.
    pub shape: HookShape,
}

/// Codex's hook-event vocabulary (per the openai/codex source —
/// see `codex-rs/config/src/hooks_tests.rs`). Same shape as Claude
/// Code's six common events, EXCEPT: Codex has no `SessionEnd` (it
/// uses `Stop` for both turn-end and session-end signalling).
pub(crate) const CODEX_EVENTS: [(&str, &str); 6] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
];

/// Cursor's hook-event vocabulary (per
/// <https://cursor.com/docs/agent/hooks>). camelCase event names
/// and a FLAT JSON shape (no inner `hooks: [...]` wrapper).
/// `beforeSubmitPrompt` maps to engram's `user-prompt-submit`
/// concept. Cursor has no `userPromptSubmit` event.
pub(crate) const CURSOR_EVENTS: [(&str, &str); 8] = [
    ("sessionStart", "session-start.sh"),
    ("sessionEnd", "session-end.sh"),
    ("beforeSubmitPrompt", "user-prompt-submit.sh"),
    ("preToolUse", "pre-tool-use.sh"),
    ("postToolUse", "post-tool-use.sh"),
    ("postToolUseFailure", "post-tool-use.sh"),
    ("preCompact", "pre-compact.sh"),
    ("stop", "stop.sh"),
];

/// Gemini CLI's hook-event vocabulary (per
/// <https://geminicli.com/docs/hooks/reference>). Event names use
/// PascalCase. The vocab DIFFERS from Claude Code's:
///   - `BeforeTool` / `AfterTool` instead of `PreToolUse` / `PostToolUse`
///   - `PreCompress` instead of `PreCompact`
///   - No `UserPromptSubmit` equivalent (skipped)
///   - No `Stop` event (SessionEnd covers it)
pub(crate) const GEMINI_EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start.sh"),
    ("SessionEnd", "session-end.sh"),
    ("BeforeTool", "pre-tool-use.sh"),
    ("AfterTool", "post-tool-use.sh"),
    ("PreCompress", "pre-compact.sh"),
];

/// Per-agent profile constants. Add a new agent by adding one of
/// these + a script-dir name + a config-file path resolver — the
/// payload-build path picks up the rest from `shape`.
pub(crate) const CODEX_PROFILE: HookProfile = HookProfile {
    events: &CODEX_EVENTS,
    shape: HookShape::Nested,
};
pub(crate) const CURSOR_PROFILE: HookProfile = HookProfile {
    events: &CURSOR_EVENTS,
    shape: HookShape::Flat,
};
pub(crate) const GEMINI_PROFILE: HookProfile = HookProfile {
    events: &GEMINI_EVENTS,
    shape: HookShape::Nested,
};

/// Antigravity CLI (`agy`) hook-event vocabulary (per
/// <https://antigravity.google/docs/hooks>). Named-groups format
/// at the top level; events inside each group.
/// Tool events use nested shape (matcher + hooks), lifecycle
/// events use flat shape (direct handler list).
pub(crate) const ANTIGRAVITY_TOOL_EVENTS: [(&str, &str); 2] = [
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
];

pub(crate) const ANTIGRAVITY_LIFECYCLE_EVENTS: [(&str, &str); 2] =
    [("PreInvocation", "session-start.sh"), ("Stop", "stop.sh")];

/// Build the Antigravity CLI (`agy`) `hooks.json` payload.
///
/// Antigravity CLI uses a named-groups format where the top level
/// maps hook-group names to their event configurations. Each group
/// can contain any subset of the supported events. Tool events
/// (`PreToolUse`, `PostToolUse`) use the nested shape with matcher;
/// lifecycle events (`PreInvocation`, `Stop`) use a flat handler
/// list where matcher is ignored.
///
/// The output is `{ "engram": { <events> } }`.
pub(crate) fn build_antigravity_payload_with_data_dir(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    build_antigravity_payload_for_platform(
        emit_root,
        server_url,
        auth_token,
        HookCommandPlatform::current(),
        "antigravity-cli",
        data_dir,
        project_strategy,
    )
}

fn build_antigravity_payload_for_platform(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    platform: HookCommandPlatform,
    agent: &str,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    let mut group = serde_json::Map::new();

    // Tool events: nested shape (matcher + inner hooks array)
    for (event, script) in &ANTIGRAVITY_TOOL_EVENTS {
        let s = script_for_platform(script, platform);
        let abs = emit_root.join(s.as_ref());
        let handler = hook_handler_value(HookHandlerSpec::ShellString(hook_command(
            &abs,
            server_url,
            auth_token,
            HookCommandContext::new(platform, agent, data_dir, project_strategy),
        )));
        group.insert(
            (*event).to_string(),
            json!([{
                "matcher": "",
                "hooks": [handler],
            }]),
        );
    }

    // Lifecycle events: flat shape (direct handler list, no matcher)
    for (event, script) in &ANTIGRAVITY_LIFECYCLE_EVENTS {
        let s = script_for_platform(script, platform);
        let abs = emit_root.join(s.as_ref());
        let handler = hook_handler_value(HookHandlerSpec::ShellString(hook_command(
            &abs,
            server_url,
            auth_token,
            HookCommandContext::new(platform, agent, data_dir, project_strategy),
        )));
        group.insert((*event).to_string(), Value::Array(vec![handler]));
    }

    json!({ "engram": group })
}

/// Build a hook payload for `profile`. The output is always
/// `{ "hooks": { "<EventName>": <profile-specific-array> } }`; the
/// caller is responsible for any sibling top-level keys (e.g.
/// Cursor's `"version": 1`).
#[cfg(test)]
pub(crate) fn build_profile_payload(
    profile: &HookProfile,
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    build_profile_payload_for_agent(
        profile,
        emit_root,
        server_url,
        auth_token,
        "claude-code",
        None,
        None,
    )
}

pub(crate) fn build_profile_payload_for_agent(
    profile: &HookProfile,
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    agent: &str,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    build_hook_payload(
        profile.events,
        emit_root,
        server_url,
        auth_token,
        profile.shape,
        HookCommandContext::new(
            HookCommandPlatform::current(),
            agent,
            data_dir,
            project_strategy,
        ),
    )
}

fn build_hook_payload(
    events: &[(&str, &str)],
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    shape: HookShape,
    context: HookCommandContext<'_>,
) -> serde_json::Value {
    build_hook_payload_for_platform(events, emit_root, server_url, auth_token, shape, context)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookCommandPlatform {
    Posix,
    Windows,
    /// Claude Code on Windows invokes hooks through bash (Git for
    /// Windows), not PowerShell. Commands use POSIX `.sh` scripts
    /// wrapped in `bash -c '...'` with drive-letter paths converted
    /// to Git Bash format (`C:\x` → `/c/x`).
    WindowsBash,
    /// Windows, native: invoke the `engram` binary directly
    /// (`<exe> hook --event … --agent …`) with no shell or child
    /// processes — ~3.5× faster per hook than `WindowsBash`. Default for
    /// Claude Code on Windows; see
    /// `docs/windows.md#native-hook-command-claude-code-on-windows`.
    WindowsNative,
    /// POSIX (Linux/macOS), native: invoke the `engram` binary directly
    /// (`<exe> hook --event …`) instead of the `.sh` script, so the hook gets
    /// the local spool + OIDC-token fallback. The **default** for native
    /// Linux/macOS Claude Code installs (mirrors `WindowsNative`). Override
    /// with `ENGRAM_HOOK_PLATFORM=posix` to get the `.sh` shell scripts.
    PosixNative,
}

#[derive(Clone, Copy)]
struct HookCommandContext<'a> {
    platform: HookCommandPlatform,
    agent: &'a str,
    data_dir: Option<&'a Path>,
    /// Install-time default project strategy baked into the command
    /// (`install-hooks --project-strategy`). `None` bakes nothing.
    project_strategy: Option<&'a str>,
    /// Whether this render path may use Claude Code's exec-form hook handler.
    /// Only `install-hooks --agent claude-code` sets this; setup-agent
    /// snippets keep command-string script fallback even when the platform env
    /// is overridden to `windows-native`.
    claude_windows_exec_allowed: bool,
}

impl<'a> HookCommandContext<'a> {
    const fn new(
        platform: HookCommandPlatform,
        agent: &'a str,
        data_dir: Option<&'a Path>,
        project_strategy: Option<&'a str>,
    ) -> Self {
        Self {
            platform,
            agent,
            data_dir,
            project_strategy,
            claude_windows_exec_allowed: false,
        }
    }

    const fn allow_claude_windows_exec(mut self) -> Self {
        self.claude_windows_exec_allowed = true;
        self
    }
}

impl HookCommandPlatform {
    /// Parse an explicit `ENGRAM_HOOK_PLATFORM` override. `None` when the
    /// var is unset or names no known platform — callers then apply their own
    /// per-render-path default. One parser so a new platform value can't be
    /// recognised by one render path and silently ignored by another.
    fn from_env_override() -> Option<Self> {
        let v = std::env::var("ENGRAM_HOOK_PLATFORM").ok()?;
        match v {
            v if v.eq_ignore_ascii_case("windows") => Some(Self::Windows),
            v if v.eq_ignore_ascii_case("posix") || v.eq_ignore_ascii_case("unix") => {
                Some(Self::Posix)
            }
            v if v.eq_ignore_ascii_case("windows-bash") => Some(Self::WindowsBash),
            v if v.eq_ignore_ascii_case("windows-native") => Some(Self::WindowsNative),
            v if v.eq_ignore_ascii_case("posix-native") => Some(Self::PosixNative),
            _ => None,
        }
    }

    fn current() -> Self {
        Self::from_env_override().unwrap_or(if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        })
    }

    /// Platform for agents known to use bash as their hook runner on
    /// Windows (currently Claude Code). Returns `WindowsNative` on
    /// Windows unless overridden by `ENGRAM_HOOK_PLATFORM`.
    fn for_bash_runner() -> Self {
        // Native macOS / Linux defaults to the binary hook command (spool +
        // OIDC), same as Windows.
        Self::from_env_override().unwrap_or(if cfg!(windows) {
            Self::WindowsNative
        } else {
            Self::PosixNative
        })
    }

    /// Script fallback for setup-agent snippets. Respects an explicit
    /// override, but defaults to the shell command because setup-agent
    /// copies scripts, not a host-local native binary.
    fn for_bash_script_runner() -> Self {
        Self::from_env_override().unwrap_or(if cfg!(windows) {
            Self::WindowsBash
        } else {
            Self::Posix
        })
    }
}

fn build_hook_payload_for_platform(
    events: &[(&str, &str)],
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    shape: HookShape,
    context: HookCommandContext<'_>,
) -> serde_json::Value {
    let mut hooks_block = serde_json::Map::new();
    for (event, script) in events {
        let script = script_for_platform(script, context.platform);
        let abs = emit_root.join(script.as_ref());

        // Claude Code's hook schema (per
        // https://code.claude.com/docs/en/hooks):
        //   "<EventName>": [
        //     { "matcher": "<tool-name regex or empty>",
        //       "hooks": [ { "type": "command", "command": "..." } ]
        //     }
        //   ]
        //
        // Shell-form handlers INLINE env vars into the command string itself
        // (`ENGRAM_HOOK_URL=... ENGRAM_AUTH_TOKEN=... /path`)
        // rather than passing them through an `env` field on the hook entry.
        // Native Claude Code Windows installs instead use official exec form,
        // where server/auth/project options are passed as raw argv tokens.
        // Reasons shell-form handlers keep inline env vars:
        //   1. CC doesn't appear to honour an `env` field at this
        //      level — observed empirically: the hook fires but
        //      the script sees neither var and falls back to the
        //      127.0.0.1 default, so POSTs go nowhere.
        //   2. Inlining the env into the command string is
        //      portable across any shell-style hook runner — POSIX
        //      `VAR=val command` syntax is universally honoured.
        //   3. The hook scripts already read those env vars (see
        //      `hooks/claude-code/session-start.sh` etc.), so no
        //      script changes are required on POSIX. Windows uses an
        //      explicit PowerShell command with equivalent env setup.
        let handler = hook_handler_value(hook_handler_spec(
            &abs, server_url, auth_token, context, shape,
        ));

        // Empty matcher = fire on every event of this kind. Right
        // for engram's capture hooks (every prompt, every tool
        // call, every session boundary).
        let entry = match shape {
            HookShape::Nested => json!([{
                "matcher": "",
                "hooks": [handler],
            }]),
            HookShape::Flat => Value::Array(vec![hook_handler_with_matcher(handler)]),
        };
        hooks_block.insert((*event).to_string(), entry);
    }
    json!({ "hooks": hooks_block })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HookHandlerSpec {
    ShellString(String),
    Exec { command: String, args: Vec<String> },
}

fn hook_handler_spec(
    script: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
    shape: HookShape,
) -> HookHandlerSpec {
    if shape == HookShape::Nested
        && context.claude_windows_exec_allowed
        && context.platform == HookCommandPlatform::WindowsNative
        && context.agent == "claude-code"
    {
        return windows_native_exec_spec(script, server_url, auth_token, context);
    }
    HookHandlerSpec::ShellString(hook_command(script, server_url, auth_token, context))
}

fn hook_handler_value(spec: HookHandlerSpec) -> Value {
    match spec {
        HookHandlerSpec::ShellString(command) => json!({
            "type": "command",
            "command": command,
        }),
        HookHandlerSpec::Exec { command, args } => json!({
            "type": "command",
            "command": command,
            "args": args,
        }),
    }
}

fn hook_handler_with_matcher(mut handler: Value) -> Value {
    if let Some(obj) = handler.as_object_mut() {
        obj.insert("matcher".to_string(), Value::String(String::new()));
    }
    handler
}

fn script_for_platform(script: &str, platform: HookCommandPlatform) -> Cow<'_, str> {
    match platform {
        HookCommandPlatform::Posix
        | HookCommandPlatform::PosixNative
        | HookCommandPlatform::WindowsBash
        | HookCommandPlatform::WindowsNative => Cow::Borrowed(script),
        HookCommandPlatform::Windows => match script.strip_suffix(".sh") {
            Some(stem) => Cow::Owned(format!("{stem}.ps1")),
            None => Cow::Borrowed(script),
        },
    }
}

pub(crate) fn hook_script_for_current_platform(script: &str) -> Cow<'_, str> {
    script_for_platform(script, HookCommandPlatform::current())
}

pub(crate) fn hook_script_for_claude_code(script: &str) -> Cow<'_, str> {
    script_for_platform(script, HookCommandPlatform::for_bash_runner())
}

fn hook_command(
    script: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
) -> String {
    match context.platform {
        HookCommandPlatform::Posix => {
            let mut prefix = format!("ENGRAM_HOOK_URL={} ", shell_quote(server_url));
            if let Some(t) = auth_token {
                prefix.push_str(&format!("ENGRAM_AUTH_TOKEN={} ", shell_quote(t)));
            }
            if let Some(s) = context.project_strategy {
                prefix.push_str(&format!("ENGRAM_PROJECT_STRATEGY={} ", shell_quote(s)));
            }
            format!("{prefix}{}", shell_quote(&script.to_string_lossy()))
        }
        HookCommandPlatform::Windows => {
            let mut setup = format!("$env:ENGRAM_HOOK_URL={}", powershell_quote(server_url));
            if let Some(t) = auth_token {
                setup.push_str(&format!("; $env:ENGRAM_AUTH_TOKEN={}", powershell_quote(t)));
            }
            if let Some(s) = context.project_strategy {
                setup.push_str(&format!(
                    "; $env:ENGRAM_PROJECT_STRATEGY={}",
                    powershell_quote(s)
                ));
            }
            format!(
                "powershell.exe -NoProfile -ExecutionPolicy Bypass -Command \"{setup}; & {}\"",
                powershell_quote(&script.to_string_lossy())
            )
        }
        HookCommandPlatform::WindowsBash => {
            let bash_path = to_git_bash_path(&script.to_string_lossy());
            let mut inner = format!("ENGRAM_HOOK_URL={} ", shell_quote(server_url));
            if let Some(t) = auth_token {
                inner.push_str(&format!("ENGRAM_AUTH_TOKEN={} ", shell_quote(t)));
            }
            if let Some(s) = context.project_strategy {
                inner.push_str(&format!("ENGRAM_PROJECT_STRATEGY={} ", shell_quote(s)));
            }
            inner.push_str(&shell_quote(&bash_path));
            format!("bash -c {}", shell_quote(&inner))
        }
        HookCommandPlatform::WindowsNative => {
            // Legacy/setup-agent/fallback string form: invoke the binary
            // directly as `"<exe>" hook --event <e> --agent ...`. Primary
            // Claude Code WindowsNative `install-hooks` uses exec form instead
            // (see `windows_native_exec_spec`). The event token is the script
            // stem (`pre-tool-use.sh` → `pre-tool-use`). No shell, no child
            // processes in the intended runner.
            //
            // Quote with DOUBLE quotes, not POSIX single quotes: Claude Code
            // on Windows shell-form runners may treat '…' literally and error
            // out; double quotes + the native Windows path work in cmd.exe and
            // Git Bash. The event name is a fixed slug with no shell
            // metacharacters, so it is left unquoted.
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "engram".to_string());
            let event = script
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let mut cmd = format!(
                "{}{} hook --event {event} --agent {agent} --server-url {}",
                win_double_quote(&exe),
                native_data_dir_arg(context.data_dir, NativeQuote::Windows),
                win_double_quote(server_url),
                agent = context.agent,
            );
            if let Some(t) = auth_token {
                cmd.push_str(&format!(" --auth-token {}", win_double_quote(t)));
            }
            cmd.push_str(&native_project_strategy_arg(
                context.project_strategy,
                NativeQuote::Windows,
            ));
            cmd
        }
        HookCommandPlatform::PosixNative => {
            // Native POSIX (opt-in): invoke the binary directly so the hook
            // gets the local spool + OIDC fallback, instead of the `.sh` script
            // that POSTs via curl. Mirrors `WindowsNative` but with POSIX
            // single-quote quoting. The event name is the script stem.
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "engram".to_string());
            let event = script
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let mut cmd = format!(
                "{}{} hook --event {event} --agent {agent} --server-url {}",
                shell_quote(&exe),
                native_data_dir_arg(context.data_dir, NativeQuote::Posix),
                shell_quote(server_url),
                agent = context.agent,
            );
            if let Some(t) = auth_token {
                cmd.push_str(&format!(" --auth-token {}", shell_quote(t)));
            }
            cmd.push_str(&native_project_strategy_arg(
                context.project_strategy,
                NativeQuote::Posix,
            ));
            cmd
        }
    }
}

fn windows_native_exec_spec(
    script: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
) -> HookHandlerSpec {
    let exe = std::env::current_exe().unwrap_or_else(|_| Path::new("engram.exe").to_path_buf());
    windows_native_exec_spec_with_exe(&exe, script, server_url, auth_token, context)
}

fn windows_native_exec_spec_with_exe(
    exe: &Path,
    script: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
) -> HookHandlerSpec {
    let event = script
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let mut args = Vec::new();
    if let Some(data_dir) = context.data_dir {
        args.push("--data-dir".to_string());
        args.push(plain_windows_path_arg(data_dir));
    }
    args.extend([
        "hook".to_string(),
        "--event".to_string(),
        event.to_string(),
        "--agent".to_string(),
        context.agent.to_string(),
        "--server-url".to_string(),
        server_url.to_string(),
    ]);
    if let Some(t) = auth_token {
        args.push("--auth-token".to_string());
        args.push(t.to_string());
    }
    if let Some(strategy) = context.project_strategy {
        args.push("--project-strategy".to_string());
        args.push(strategy.to_string());
    }
    HookHandlerSpec::Exec {
        command: plain_windows_path_arg(exe),
        args,
    }
}

fn plain_windows_path_arg(path: &Path) -> String {
    let lossy = path.to_string_lossy();
    strip_windows_verbatim_prefix(&lossy).into_owned()
}

#[derive(Clone, Copy)]
enum NativeQuote {
    Posix,
    Windows,
}

fn native_data_dir_arg(data_dir: Option<&Path>, quote: NativeQuote) -> String {
    let Some(data_dir) = data_dir else {
        return String::new();
    };
    // Render safe verbatim Windows data-dir forms as plain paths (#116).
    let lossy = data_dir.to_string_lossy();
    let path = strip_windows_verbatim_prefix(&lossy);
    match quote {
        NativeQuote::Posix => format!(" --data-dir {}", shell_quote(&path)),
        NativeQuote::Windows => format!(" --data-dir {}", win_double_quote(&path)),
    }
}

/// Append ` --project-strategy <value>` to a native hook command when an
/// install-time default was baked in (`install-hooks --project-strategy`).
/// `None` appends nothing, keeping the command byte-identical to before.
fn native_project_strategy_arg(strategy: Option<&str>, quote: NativeQuote) -> String {
    let Some(strategy) = strategy else {
        return String::new();
    };
    match quote {
        NativeQuote::Posix => format!(" --project-strategy {}", shell_quote(strategy)),
        NativeQuote::Windows => format!(" --project-strategy {}", win_double_quote(strategy)),
    }
}

/// Convert a Windows path to Git Bash (MSYS2) format.
/// `C:\Users\alice\hooks\x.sh` → `/c/Users/alice/hooks/x.sh`
fn to_git_bash_path(path: &str) -> String {
    let s = path.replace('\\', "/");
    if s.len() >= 3
        && s.as_bytes()[0].is_ascii_alphabetic()
        && s.as_bytes()[1] == b':'
        && s.as_bytes()[2] == b'/'
    {
        let drive = (s.as_bytes()[0] as char).to_ascii_lowercase();
        format!("/{drive}{}", &s[2..])
    } else {
        s
    }
}

/// Minimal shell quoting for embedding values into a `VAR=val cmd` prefix or
/// command path. Leaves only conservative shell-safe characters unquoted;
/// wraps everything else in single quotes and escapes embedded `'` via
/// `'\''`.
fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '%' | '+' | '=' | ',')
    }) {
        return s.to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn powershell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Wrap a value in double quotes for the `WindowsNative` hook command.
/// Claude Code on Windows runs hook commands via cmd.exe, which does not
/// honour POSIX single quotes; double quotes work in both cmd.exe and Git
/// Bash. The quoted values (binary path, URL, hex auth token) never
/// contain a literal `"`; any is stripped defensively rather than risk a
/// broken command line.
fn win_double_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', ""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn build_posix_hook_payload(
        events: &[(&str, &str)],
        root: &Path,
        server_url: &str,
        auth_token: Option<&str>,
        shape: HookShape,
    ) -> serde_json::Value {
        build_hook_payload_for_platform(
            events,
            root,
            server_url,
            auth_token,
            shape,
            HookCommandContext::new(HookCommandPlatform::Posix, "claude-code", None, None),
        )
    }

    #[test]
    fn bearer_header_is_none_when_no_token() {
        assert!(bearer_header_value(None).is_none());
    }

    #[test]
    fn bearer_header_prefixes_with_bearer() {
        let h = bearer_header_value(Some("abc123")).unwrap();
        assert_eq!(h, "Bearer abc123");
    }

    #[test]
    fn claude_code_payload_has_all_events() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        let hooks = v.get("hooks").and_then(|h| h.as_object()).unwrap();
        assert_eq!(hooks.len(), CLAUDE_CODE_EVENTS.len());
        for (event, _) in CLAUDE_CODE_EVENTS {
            assert!(hooks.contains_key(event), "missing event {event}");
        }
    }

    #[test]
    fn grok_native_payload_uses_grok_agent() {
        let root = PathBuf::from("/host/hooks/grok");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            None,
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::PosixNative, "grok", None, None),
        );
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(command.contains("--agent grok"), "{command}");
        assert!(!command.contains("claude-code"), "{command}");
    }

    #[test]
    fn grok_script_payload_uses_grok_bundle() {
        let root = PathBuf::from("/host/hooks/grok");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            None,
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::Posix, "grok", None, None),
        );
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        let normalized = command.replace('\\', "/");
        assert!(
            normalized.contains("/host/hooks/grok/session-start.sh"),
            "{command}"
        );
        assert!(!command.contains("claude-code"), "{command}");
    }

    #[test]
    fn claude_code_payload_embeds_auth_token_when_provided() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_posix_hook_payload(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            Some("tok"),
            HookShape::Nested,
        );
        // Env vars are inlined into the command string so CC's
        // hook runner sees them regardless of whether it honours
        // a separate `env` field. Assert the token landed in the
        // command prefix.
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(
            command.contains("ENGRAM_AUTH_TOKEN=tok"),
            "command should inline the auth token; got: {command}"
        );
        assert!(
            command.contains("ENGRAM_HOOK_URL=http://localhost:49374"),
            "command should inline the hook URL; got: {command}"
        );
    }

    /// Regression guard: Claude Code's hook schema requires the
    /// outer array entries to have `matcher` + a nested `hooks`
    /// array (containing the actual `type: "command"` payload).
    /// We shipped the wrong shape briefly — bare `command` at the
    /// outer level — which made Claude Code refuse to load
    /// settings.json with "hooks: Expected array, but received
    /// undefined" on every event.
    #[test]
    fn cursor_payload_uses_flat_shape() {
        // Flat shape: no inner `hooks: [...]` array; each event
        // maps to an array of {type, command, matcher} entries.
        let root = PathBuf::from("/host/hooks/cursor");
        let v = build_posix_hook_payload(
            CURSOR_PROFILE.events,
            &root,
            "http://localhost:49374",
            Some("tok"),
            CURSOR_PROFILE.shape,
        );
        let session_start = v
            .pointer("/hooks/sessionStart/0")
            .and_then(|e| e.as_object())
            .expect("missing /hooks/sessionStart/0");
        assert_eq!(
            session_start.get("type").and_then(|t| t.as_str()),
            Some("command"),
            "Cursor flat entries put `type` at the outer level"
        );
        assert!(
            session_start.contains_key("command"),
            "Cursor flat entries put `command` at the outer level"
        );
        // No nested hooks array.
        assert!(
            !session_start.contains_key("hooks"),
            "Cursor must NOT use the nested hooks shape — found one: {session_start:?}"
        );
        // Auth token still inlined into command.
        let cmd = session_start
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert!(cmd.contains("ENGRAM_AUTH_TOKEN=tok"));
        // Events are camelCase, not PascalCase.
        let events: Vec<&str> = v
            .pointer("/hooks")
            .and_then(|h| h.as_object())
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        assert!(events.contains(&"sessionStart"));
        assert!(events.contains(&"preToolUse"));
        assert!(events.contains(&"postToolUseFailure"));
        assert!(
            !events.contains(&"SessionStart"),
            "Cursor uses camelCase, not PascalCase"
        );
    }

    #[test]
    fn gemini_payload_uses_nested_shape_with_gemini_event_names() {
        // Same nested shape as Claude Code, but DIFFERENT event
        // names (BeforeTool / AfterTool / PreCompress; no
        // UserPromptSubmit, no Stop).
        let root = PathBuf::from("/host/hooks/gemini-cli");
        let v = build_profile_payload(
            &GEMINI_PROFILE,
            &root,
            "http://localhost:49374",
            Some("tok"),
        );
        let session_start = v
            .pointer("/hooks/SessionStart/0")
            .and_then(|e| e.as_object())
            .expect("missing /hooks/SessionStart/0");
        // Outer level has matcher + hooks (nested shape).
        assert!(session_start.contains_key("matcher"));
        let inner = session_start
            .get("hooks")
            .and_then(|h| h.as_array())
            .unwrap();
        assert_eq!(inner.len(), 1);
        let entry = inner[0].as_object().unwrap();
        assert_eq!(entry.get("type").and_then(|t| t.as_str()), Some("command"));
        // Event vocab: Gemini-specific names present, Claude Code-
        // only names absent.
        let events: Vec<&str> = v
            .pointer("/hooks")
            .and_then(|h| h.as_object())
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        for expected in [
            "SessionStart",
            "SessionEnd",
            "BeforeTool",
            "AfterTool",
            "PreCompress",
        ] {
            assert!(
                events.contains(&expected),
                "missing Gemini event {expected}"
            );
        }
        for unexpected in [
            "PreToolUse",
            "PostToolUse",
            "UserPromptSubmit",
            "Stop",
            "PreCompact",
        ] {
            assert!(
                !events.contains(&unexpected),
                "Gemini should NOT have CC-only event {unexpected}; got {events:?}"
            );
        }
    }

    #[test]
    fn claude_code_payload_uses_matcher_plus_inner_hooks_shape() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        for (event, _) in CLAUDE_CODE_EVENTS {
            let outer = v
                .pointer(&format!("/hooks/{event}/0"))
                .and_then(|s| s.as_object())
                .unwrap_or_else(|| panic!("missing /hooks/{event}/0"));
            assert!(outer.contains_key("matcher"), "{event}: missing matcher");
            let inner = outer
                .get("hooks")
                .and_then(|h| h.as_array())
                .unwrap_or_else(|| panic!("{event}: missing inner hooks array"));
            assert_eq!(inner.len(), 1);
            let entry = inner[0].as_object().unwrap();
            assert_eq!(
                entry.get("type").and_then(|t| t.as_str()),
                Some("command"),
                "{event}: inner entry must have type: command"
            );
            assert!(
                entry.contains_key("command"),
                "{event}: inner entry missing command"
            );
        }
    }

    #[test]
    fn claude_code_payload_omits_auth_token_when_absent() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        // Format-agnostic: POSIX/WindowsBash inline `ENGRAM_HOOK_URL=…`,
        // WindowsNative passes `--server-url …`. Both carry the host:port,
        // and neither must carry a token when none was supplied.
        assert!(
            command.contains("localhost:49374"),
            "server url expected: {command}"
        );
        assert!(
            !command.contains("AUTH_TOKEN") && !command.contains("--auth-token"),
            "no token expected in command: {command}"
        );
    }

    #[test]
    fn windows_native_emits_binary_command_with_event_token() {
        let root = PathBuf::from(r"C:\hooks");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://h:49374",
            Some("tok"),
            HookShape::Nested,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                None,
                None,
            )
            .allow_claude_windows_exec(),
        );
        // Each native exec-form handler must carry `hook --event <stem>` in
        // argv, where <stem> matches the .sh script the other platforms invoke.
        for (event, script) in CLAUDE_CODE_EVENTS {
            let stem = script.strip_suffix(".sh").unwrap();
            let handler = v.pointer(&format!("/hooks/{event}/0/hooks/0")).unwrap();
            let cmd = handler.get("command").and_then(|s| s.as_str()).unwrap();
            let args: Vec<&str> = handler
                .get("args")
                .and_then(|a| a.as_array())
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            assert!(!cmd.contains("--event"), "{event}: executable only: {cmd}");
            assert!(
                args.windows(2).any(|w| w == ["--event", stem]),
                "{event}: {args:?}"
            );
            assert!(
                args.windows(2).any(|w| w == ["--agent", "claude-code"]),
                "{event}: {args:?}"
            );
            assert!(
                args.windows(2)
                    .any(|w| w == ["--server-url", "http://h:49374"]),
                "{event}: {args:?}"
            );
            assert!(
                args.windows(2).any(|w| w == ["--auth-token", "tok"]),
                "{event}: {args:?}"
            );
        }
    }

    #[test]
    fn windows_native_claude_code_nested_payload_uses_exec_form() {
        let root = PathBuf::from(r"C:\hooks");
        let data_dir = Path::new(r"\\?\C:\Users\me\AppData\Local\engram");
        let v = build_hook_payload_for_platform(
            &[("SessionStart", "session-start.sh")],
            &root,
            "http://h:49374",
            Some("tok"),
            HookShape::Nested,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                Some(data_dir),
                Some("repo-root"),
            )
            .allow_claude_windows_exec(),
        );
        let handler = v.pointer("/hooks/SessionStart/0/hooks/0").unwrap();
        let command = handler.get("command").and_then(|v| v.as_str()).unwrap();
        let args: Vec<&str> = handler
            .get("args")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        assert!(command.contains("engram") || command.contains("engram"));
        assert!(
            !command.contains(" --event "),
            "command is executable only: {command}"
        );
        assert_eq!(
            args,
            vec![
                "--data-dir",
                r"C:\Users\me\AppData\Local\engram",
                "hook",
                "--event",
                "session-start",
                "--agent",
                "claude-code",
                "--server-url",
                "http://h:49374",
                "--auth-token",
                "tok",
                "--project-strategy",
                "repo-root",
            ]
        );
        assert!(
            args.iter().all(|arg| !arg.contains('"')),
            "args are raw argv tokens"
        );
    }

    #[test]
    fn windows_native_exec_spec_strips_verbatim_exe_and_data_dir() {
        let spec = windows_native_exec_spec_with_exe(
            Path::new(r"\\?\C:\Program Files\engram\engram.exe"),
            Path::new(r"C:\hooks\post-tool-use.sh"),
            "http://h:49374",
            None,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                Some(Path::new(r"\\?\C:\Data\engram")),
                None,
            ),
        );
        let HookHandlerSpec::Exec { command, args } = spec else {
            panic!("expected exec spec")
        };
        assert_eq!(command, r"C:\Program Files\engram\engram.exe");
        assert_eq!(args[1], r"C:\Data\engram");
        assert!(args.iter().all(|arg| !arg.contains(r"\\?\")));
    }

    #[test]
    fn exec_form_is_narrow_to_claude_code_windows_native_nested() {
        fn handler_for(
            platform: HookCommandPlatform,
            agent: &str,
            shape: HookShape,
        ) -> serde_json::Value {
            let v = build_hook_payload_for_platform(
                &[("SessionStart", "session-start.sh")],
                Path::new(r"C:\hooks"),
                "http://h:49374",
                Some("tok"),
                shape,
                HookCommandContext::new(platform, agent, None, None).allow_claude_windows_exec(),
            );
            match shape {
                HookShape::Nested => v.pointer("/hooks/SessionStart/0/hooks/0").unwrap().clone(),
                HookShape::Flat => v.pointer("/hooks/SessionStart/0").unwrap().clone(),
            }
        }

        for (platform, agent, shape) in [
            (
                HookCommandPlatform::WindowsBash,
                "claude-code",
                HookShape::Nested,
            ),
            (
                HookCommandPlatform::Windows,
                "claude-code",
                HookShape::Nested,
            ),
            (HookCommandPlatform::Posix, "claude-code", HookShape::Nested),
            (
                HookCommandPlatform::PosixNative,
                "claude-code",
                HookShape::Nested,
            ),
            (
                HookCommandPlatform::WindowsNative,
                "grok",
                HookShape::Nested,
            ),
            (
                HookCommandPlatform::WindowsNative,
                "claude-code",
                HookShape::Flat,
            ),
        ] {
            let handler = handler_for(platform, agent, shape);
            assert!(
                handler.get("args").is_none(),
                "{platform:?}/{agent}/{shape:?} must keep command-string form: {handler}"
            );
        }

        let setup_like = build_hook_payload_for_platform(
            &[("SessionStart", "session-start.sh")],
            Path::new(r"C:\hooks"),
            "http://h:49374",
            Some("tok"),
            HookShape::Nested,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                None,
                None,
            ),
        );
        assert!(
            setup_like
                .pointer("/hooks/SessionStart/0/hooks/0/args")
                .is_none(),
            "setup-agent style Claude Code payload must not inherit exec form: {setup_like}"
        );

        let antigravity = build_antigravity_payload_for_platform(
            Path::new(r"C:\hooks"),
            "http://h:49374",
            Some("tok"),
            HookCommandPlatform::WindowsNative,
            "antigravity-cli",
            None,
            None,
        );
        assert!(
            antigravity
                .pointer("/engram/PreInvocation/0/args")
                .is_none(),
            "antigravity must not inherit exec form: {antigravity}"
        );
    }

    #[test]
    fn claude_code_payload_emits_absolute_paths() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("hooks")
            .join("claude-code");
        let v = build_posix_hook_payload(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            None,
            HookShape::Nested,
        );
        let cmd = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        let expected = root.join("session-start.sh").to_string_lossy().to_string();
        assert!(
            cmd.contains(&expected),
            "command should contain the absolute script path: {cmd}"
        );
    }

    #[test]
    fn posix_hook_command_quotes_script_path_and_shell_metachars() {
        let cmd = hook_command(
            &PathBuf::from("/tmp/hooks dir/session-start.sh"),
            "http://localhost:49374/mcp?x=1&y=2",
            Some("tok;rm -rf /"),
            HookCommandContext::new(HookCommandPlatform::Posix, "claude-code", None, None),
        );

        assert!(
            cmd.contains("ENGRAM_HOOK_URL='http://localhost:49374/mcp?x=1&y=2'"),
            "URL with query metacharacters must be quoted: {cmd}"
        );
        assert!(
            cmd.contains("ENGRAM_AUTH_TOKEN='tok;rm -rf /'"),
            "token with shell metacharacters must be quoted: {cmd}"
        );
        assert!(
            cmd.ends_with("'/tmp/hooks dir/session-start.sh'"),
            "script path with spaces must be quoted: {cmd}"
        );
    }

    #[test]
    fn windows_payload_uses_powershell_and_ps1_hooks() {
        let root = PathBuf::from("C:/Users/alice/.local/share/engram/hooks/claude-code");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            Some("tok'en"),
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::Windows, "claude-code", None, None),
        );
        let cmd = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(cmd.starts_with("powershell.exe -NoProfile -ExecutionPolicy Bypass -Command"));
        assert!(cmd.contains("$env:ENGRAM_HOOK_URL='http://localhost:49374'"));
        assert!(cmd.contains("$env:ENGRAM_AUTH_TOKEN='tok''en'"));
        assert!(
            cmd.contains("session-start.ps1"),
            "expected ps1 script path: {cmd}"
        );
        assert!(
            !cmd.contains("session-start.sh"),
            "Windows command must not use sh: {cmd}"
        );
    }

    #[test]
    fn antigravity_payload_uses_named_groups_with_mixed_shape() {
        let root = PathBuf::from("/host/hooks/antigravity-cli");
        let v = build_antigravity_payload_for_platform(
            &root,
            "http://localhost:49374",
            Some("tok"),
            HookCommandPlatform::Posix,
            "antigravity-cli",
            None,
            None,
        );

        // Top-level key is the named group "engram", not "hooks"
        let group = v
            .get("engram")
            .and_then(|g| g.as_object())
            .expect("missing top-level 'engram' named group");
        assert!(
            !v.as_object().unwrap().contains_key("hooks"),
            "Antigravity uses named groups, not a 'hooks' wrapper"
        );

        // Tool events: nested shape (matcher + hooks array)
        let pre_tool = group
            .get("PreToolUse")
            .and_then(|e| e.as_array())
            .expect("missing PreToolUse");
        let outer = pre_tool[0].as_object().unwrap();
        assert!(outer.contains_key("matcher"));
        let inner = outer.get("hooks").and_then(|h| h.as_array()).unwrap();
        assert_eq!(inner.len(), 1);
        let entry = inner[0].as_object().unwrap();
        assert_eq!(entry.get("type").and_then(|t| t.as_str()), Some("command"));

        // Lifecycle events: flat shape (no matcher, direct handler list)
        let pre_invocation = group
            .get("PreInvocation")
            .and_then(|e| e.as_array())
            .expect("missing PreInvocation");
        let handler = pre_invocation[0].as_object().unwrap();
        assert!(
            !handler.contains_key("matcher"),
            "PreInvocation should not have matcher (flat shape)"
        );
        assert!(
            !handler.contains_key("hooks"),
            "PreInvocation should not have inner hooks array (flat shape)"
        );
        assert_eq!(
            handler.get("type").and_then(|t| t.as_str()),
            Some("command")
        );

        // Auth token inlined into commands
        let cmd = handler.get("command").and_then(|c| c.as_str()).unwrap();
        assert!(cmd.contains("ENGRAM_AUTH_TOKEN=tok"));

        let stop = group
            .get("Stop")
            .and_then(|e| e.as_array())
            .expect("missing Stop");
        let stop_cmd = stop[0]
            .get("command")
            .and_then(|c| c.as_str())
            .expect("Stop command missing");
        assert!(
            stop_cmd.contains("stop.sh"),
            "Stop must record a stop observation, not synthesize session-end handoffs: {stop_cmd}"
        );

        // All expected events present
        for expected in ["PreToolUse", "PostToolUse", "PreInvocation", "Stop"] {
            assert!(
                group.contains_key(expected),
                "missing Antigravity event {expected}"
            );
        }
    }

    #[test]
    fn to_git_bash_path_converts_drive_letter_and_backslashes() {
        assert_eq!(
            to_git_bash_path(r"C:\Users\alice\hooks\x.sh"),
            "/c/Users/alice/hooks/x.sh"
        );
        assert_eq!(to_git_bash_path(r"D:\Projects\repo"), "/d/Projects/repo");
    }

    #[test]
    fn to_git_bash_path_preserves_posix_paths() {
        assert_eq!(
            to_git_bash_path("/already/posix/path"),
            "/already/posix/path"
        );
    }

    #[test]
    fn to_git_bash_path_handles_forward_slash_windows_paths() {
        assert_eq!(
            to_git_bash_path("C:/Users/alice/hooks/x.sh"),
            "/c/Users/alice/hooks/x.sh"
        );
    }

    #[test]
    fn windows_bash_hook_command_wraps_in_bash_c_with_git_bash_paths() {
        let cmd = hook_command(
            &PathBuf::from(
                r"C:\Users\alice\.local\share\engram\hooks\claude-code\session-start.sh",
            ),
            "https://my-server.example.com",
            Some("tok123"),
            HookCommandContext::new(HookCommandPlatform::WindowsBash, "claude-code", None, None),
        );
        assert!(
            cmd.starts_with("bash -c "),
            "command must be bash-wrapped: {cmd}"
        );
        assert!(
            cmd.contains("/c/Users/alice/"),
            "Windows path must be converted to Git Bash format: {cmd}"
        );
        assert!(
            cmd.contains("session-start.sh"),
            "must use .sh script: {cmd}"
        );
        assert!(
            cmd.contains("ENGRAM_HOOK_URL=https://my-server.example.com"),
            "must inline hook URL: {cmd}"
        );
        assert!(
            cmd.contains("ENGRAM_AUTH_TOKEN=tok123"),
            "must inline auth token: {cmd}"
        );
    }

    #[test]
    fn windows_bash_hook_command_omits_token_when_absent() {
        let cmd = hook_command(
            &PathBuf::from(r"C:\Users\alice\hooks\session-start.sh"),
            "http://localhost:49374",
            None,
            HookCommandContext::new(HookCommandPlatform::WindowsBash, "claude-code", None, None),
        );
        assert!(cmd.starts_with("bash -c "));
        assert!(
            !cmd.contains("ENGRAM_AUTH_TOKEN"),
            "no token expected: {cmd}"
        );
    }

    #[test]
    fn windows_bash_script_for_platform_keeps_sh_extension() {
        let s = script_for_platform("session-start.sh", HookCommandPlatform::WindowsBash);
        assert_eq!(s, "session-start.sh");
    }

    // ── install-time --project-strategy baking (#128) ────────────────
    // A baked `Some("repo-root")` must surface in every command arm; the
    // default `None` must leave every arm byte-identical (no strategy).

    fn strategy_cmd(platform: HookCommandPlatform, strategy: Option<&str>) -> String {
        hook_command(
            &PathBuf::from("/tmp/hooks/claude-code/session-start.sh"),
            "http://localhost:49374",
            None,
            HookCommandContext::new(platform, "claude-code", None, strategy),
        )
    }

    #[test]
    fn posix_hook_command_bakes_project_strategy_env() {
        let cmd = strategy_cmd(HookCommandPlatform::Posix, Some("repo-root"));
        assert!(
            cmd.contains("ENGRAM_PROJECT_STRATEGY=repo-root"),
            "posix must bake the strategy env: {cmd}"
        );
    }

    #[test]
    fn windows_ps_hook_command_bakes_project_strategy_env() {
        let cmd = strategy_cmd(HookCommandPlatform::Windows, Some("repo-root"));
        assert!(
            cmd.contains("$env:ENGRAM_PROJECT_STRATEGY='repo-root'"),
            "powershell must bake the strategy env: {cmd}"
        );
    }

    #[test]
    fn windows_bash_hook_command_bakes_project_strategy_env() {
        let cmd = strategy_cmd(HookCommandPlatform::WindowsBash, Some("repo-root"));
        assert!(cmd.starts_with("bash -c "), "{cmd}");
        assert!(
            cmd.contains("ENGRAM_PROJECT_STRATEGY=repo-root"),
            "windows-bash must bake the strategy env inside bash -c: {cmd}"
        );
    }

    #[test]
    fn posix_native_hook_command_passes_project_strategy_flag() {
        let cmd = strategy_cmd(HookCommandPlatform::PosixNative, Some("repo-root"));
        assert!(
            cmd.contains("--project-strategy repo-root"),
            "posix-native must pass the strategy flag: {cmd}"
        );
    }

    #[test]
    fn windows_native_hook_command_passes_project_strategy_flag() {
        let cmd = strategy_cmd(HookCommandPlatform::WindowsNative, Some("repo-root"));
        assert!(
            cmd.contains(r#"--project-strategy "repo-root""#),
            "windows-native must pass the strategy flag (double-quoted): {cmd}"
        );
    }

    #[test]
    fn hook_command_omits_project_strategy_when_none() {
        for platform in [
            HookCommandPlatform::Posix,
            HookCommandPlatform::Windows,
            HookCommandPlatform::WindowsBash,
            HookCommandPlatform::PosixNative,
            HookCommandPlatform::WindowsNative,
        ] {
            let cmd = strategy_cmd(platform, None);
            assert!(
                !cmd.contains("ENGRAM_PROJECT_STRATEGY"),
                "{platform:?}: no strategy env when None: {cmd}"
            );
            assert!(
                !cmd.contains("--project-strategy"),
                "{platform:?}: no strategy flag when None: {cmd}"
            );
        }
    }

    #[test]
    fn posix_native_hook_command_invokes_binary_directly() {
        let cmd = hook_command(
            &PathBuf::from("/home/alice/.local/share/engram/hooks/claude-code/session-start.sh"),
            "https://my-server.example.com",
            Some("tok123"),
            HookCommandContext::new(HookCommandPlatform::PosixNative, "claude-code", None, None),
        );
        assert!(
            cmd.contains("hook --event session-start"),
            "invokes the binary subcommand with the event stem: {cmd}"
        );
        assert!(cmd.contains("--agent claude-code"), "{cmd}");
        assert!(cmd.contains("https://my-server.example.com"), "{cmd}");
        assert!(
            cmd.contains("--auth-token") && cmd.contains("tok123"),
            "{cmd}"
        );
        assert!(
            !cmd.contains("session-start.sh"),
            "must NOT reference the .sh script: {cmd}"
        );
        assert!(!cmd.starts_with("bash -c"), "no shell wrapper: {cmd}");
    }

    #[test]
    fn posix_native_hook_command_omits_token_when_absent() {
        let cmd = hook_command(
            &PathBuf::from("/home/alice/hooks/pre-tool-use.sh"),
            "http://localhost:49374",
            None,
            HookCommandContext::new(
                HookCommandPlatform::PosixNative,
                "codex",
                Some(Path::new("/home/alice/.local/share/custom memory")),
                None,
            ),
        );
        assert!(cmd.contains("hook --event pre-tool-use"), "{cmd}");
        assert!(cmd.contains("--agent codex"), "{cmd}");
        assert!(
            cmd.contains("--data-dir '/home/alice/.local/share/custom memory'"),
            "{cmd}"
        );
        assert!(!cmd.contains("--auth-token"), "no token expected: {cmd}");
    }

    #[test]
    fn windows_native_command_strips_verbatim_data_dir() {
        // Regression for #116: native hook commands must render a plain data dir.
        let cmd = hook_command(
            &PathBuf::from(r"C:/Users/me/AppData/Local/engram/hooks/claude-code/post-tool-use.sh"),
            "https://srv.example.com",
            None,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                Some(Path::new(r"\\?\C:\Users\me\AppData\Local\engram")),
                None,
            ),
        );
        assert!(
            cmd.contains(r#"--data-dir "C:\Users\me\AppData\Local\engram""#),
            "plain data dir expected: {cmd}"
        );
        assert!(cmd.contains("hook --event post-tool-use"), "{cmd}");
        assert!(
            !cmd.contains(r"\\?\"),
            "verbatim prefix must not leak into the hook command: {cmd}"
        );
    }

    #[test]
    fn windows_bash_payload_uses_bash_c_and_sh_hooks() {
        let root = PathBuf::from(r"C:\Users\alice\.local\share\engram\hooks\claude-code");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "https://my-server.example.com",
            Some("tok123"),
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::WindowsBash, "claude-code", None, None),
        );
        let cmd = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(
            cmd.starts_with("bash -c "),
            "command must be bash-wrapped: {cmd}"
        );
        assert!(
            cmd.contains("/c/Users/alice/"),
            "path must be in Git Bash format: {cmd}"
        );
        assert!(
            cmd.contains("session-start.sh"),
            "must use .sh script: {cmd}"
        );
        assert!(
            !cmd.contains("session-start.ps1"),
            "must not use .ps1: {cmd}"
        );
        assert!(cmd.contains("ENGRAM_HOOK_URL="), "must inline URL: {cmd}");
        assert!(
            cmd.contains("ENGRAM_AUTH_TOKEN=tok123"),
            "must inline token: {cmd}"
        );
    }
}
