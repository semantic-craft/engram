//! `engram setup-agent` — one-shot agent integration in a single
//! command.
//!
//! `install-hooks` emits a JSON snippet whose `command` fields are
//! absolute paths to hook scripts, and those paths must exist where the
//! agent CLI runs (Claude Code et al. shell out to them).
//!
//! `setup-agent` bundles the extract + render into one command:
//!
//!     engram setup-agent \
//!       --agent claude-code \
//!       --to "$HOME/.engram/hooks" \
//!       --auth-token "$TOKEN"
//!
//! 1. Copies the bundled `claude-code/*.{sh,ps1}` hook scripts (shipped
//!    beside the binary in the release archive) into
//!    `$HOME/.engram/hooks/claude-code/`.
//! 2. Prints the JSON config snippet whose `command` fields point at
//!    those extracted scripts so Claude Code can exec them.
//!
//! `--host-prefix` overrides the path written into the rendered config
//! when it differs from `--to` (scripts land in `--to`, but the config
//! references `--host-prefix`). It defaults to `--to`, the common case
//! where the two are the same.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::{AgentChoice, SetupAgentArgs};
use crate::commands::render_shared::{
    ANTIGRAVITY_LIFECYCLE_EVENTS, ANTIGRAVITY_TOOL_EVENTS, CODEX_PROFILE, CURSOR_PROFILE,
    GEMINI_PROFILE, build_claude_code_payload, build_grok_payload,
    hook_script_for_current_platform,
};
use crate::config::{Config, DEFAULT_SERVER_URL};

/// Run the `setup-agent` subcommand.
///
/// # Errors
/// Returns an error if the source bundle can't be located, the
/// destination directory can't be created, any script copy fails,
/// or the JSON config can't be serialised.
pub fn run(config: &Config, args: SetupAgentArgs) -> Result<()> {
    let server_url = if args.server_url == DEFAULT_SERVER_URL && config.server_url_configured() {
        normalise_hook_server_url(&config.server_url)
    } else {
        normalise_hook_server_url(&args.server_url)
    };
    let args = SetupAgentArgs {
        server_url,
        auth_token: args.auth_token.or_else(|| config.auth.bearer_token.clone()),
        ..args
    };
    if matches!(
        args.agent,
        AgentChoice::OpenCode | AgentChoice::Pi | AgentChoice::Omp | AgentChoice::Openclaw
    ) {
        emit_extension_setup_hint(&args)?;
        return Ok(());
    }
    let Some(agent_sub) = args.agent.script_hook_subdir() else {
        bail!("internal: generated integration should have returned before staging hooks")
    };

    let source = resolve_source(args.source.as_deref(), agent_sub)?;
    let dest_dir = args.to.join(agent_sub);

    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating destination {}", dest_dir.display()))?;

    let mut copied = 0_usize;
    for entry in fs::read_dir(&source)
        .with_context(|| format!("reading source bundle {}", source.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || !is_hook_script_file(&from) {
            continue;
        }
        let file_name = from
            .file_name()
            .with_context(|| format!("invalid hook script path {}", from.display()))?;
        let to = dest_dir.join(file_name);
        fs::copy(&from, &to)
            .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
        // Preserve executable bit so the agent CLI can actually run
        // the scripts. On Windows this is a no-op.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&to)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&to, perms)?;
        }
        copied += 1;
    }

    copy_support_hook_scripts(&source, &dest_dir)?;

    eprintln!(
        "✓ Extracted {copied} hook script(s) from {} to {}",
        source.display(),
        dest_dir.display(),
    );

    // The path the rendered JSON should reference. Defaults to where
    // we just copied the scripts; override with --host-prefix when the
    // agent CLI resolves the scripts under a different path than --to.
    let emit_root = args
        .host_prefix
        .as_deref()
        .unwrap_or(&args.to)
        .join(agent_sub);

    match args.agent {
        AgentChoice::ClaudeCode => emit_claude_code(&emit_root, &args)?,
        AgentChoice::Grok => emit_grok(&emit_root, &args)?,
        AgentChoice::Codex => emit_other(&emit_root, agent_sub, &args, &[CODEX_PROFILE.events]),
        AgentChoice::Cursor => emit_other(&emit_root, agent_sub, &args, &[CURSOR_PROFILE.events]),
        AgentChoice::GeminiCli => {
            emit_other(&emit_root, agent_sub, &args, &[GEMINI_PROFILE.events]);
        }
        AgentChoice::AntigravityCli => emit_other(
            &emit_root,
            agent_sub,
            &args,
            &[&ANTIGRAVITY_TOOL_EVENTS, &ANTIGRAVITY_LIFECYCLE_EVENTS],
        ),
        AgentChoice::OpenCode | AgentChoice::Pi | AgentChoice::Omp | AgentChoice::Openclaw => {
            bail!(
                "internal: generated integration should have returned before emitting staged hooks"
            )
        }
    }
    Ok(())
}

fn normalise_hook_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn emit_extension_setup_hint(args: &SetupAgentArgs) -> Result<()> {
    let (label, agent, restart_note, mcp_client) = match args.agent {
        AgentChoice::OpenCode => (
            "OpenCode",
            "opencode",
            "Then restart OpenCode so it loads ~/.config/opencode/plugins/engram.ts.",
            "opencode",
        ),
        AgentChoice::Omp => (
            "OMP",
            "omp",
            "Then restart OMP so it loads ~/.omp/agent/extensions/engram.ts.",
            "omp",
        ),
        AgentChoice::Pi => (
            "Pi",
            "pi",
            "Then restart Pi so it loads ~/.pi/agent/extensions/engram.ts.",
            "pi",
        ),
        AgentChoice::Openclaw => (
            "OpenClaw",
            "openclaw",
            "Then restart the OpenClaw gateway if it does not auto-restart after plugin install.",
            "openclaw",
        ),
        other => bail!("internal: {other:?} is not a generated-integration agent"),
    };
    println!("# {label} uses a TypeScript extension/plugin, not extracted shell scripts.");
    println!("# Install it directly instead:");
    println!("engram install-hooks --agent {agent} --apply \\");
    if args.auth_token.is_some() {
        println!("  --server-url {} \\", args.server_url);
        println!("  --auth-token <token>");
    } else {
        println!("  --server-url {}", args.server_url);
        println!("  # add --auth-token <token> if the server requires bearer auth");
    }
    println!();
    println!("{restart_note}");
    if matches!(args.agent, AgentChoice::Pi) {
        println!(
            "MCP tools come through the same generated Pi bridge extension; no native mcp.json is written."
        );
    } else {
        println!("Also run `engram install-mcp --client {mcp_client}` to wire MCP separately.");
    }
    Ok(())
}

fn emit_claude_code(emit_root: &Path, args: &SetupAgentArgs) -> Result<()> {
    let payload =
        build_claude_code_payload(emit_root, &args.server_url, args.auth_token.as_deref());
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing Claude Code hook config")?;
    println!("# Claude Code — merge into ~/.claude/settings.json");
    println!("# Hook scripts (must be reachable from the host that runs Claude Code):");
    println!("#   {}", emit_root.display());
    println!("# AI-memory server: {}", args.server_url);
    if args.auth_token.is_some() {
        println!("# Auth: ENGRAM_AUTH_TOKEN embedded in each hook's env block.");
        println!("#       Treat ~/.claude/settings.json as sensitive (chmod 600).");
    }
    println!("# Tip: also run `engram install-mcp --client claude-code --auth-token <…>`");
    println!("#      to register the MCP endpoint (separate from hooks).");
    println!();
    println!("{serialized}");
    Ok(())
}

fn emit_grok(emit_root: &Path, args: &SetupAgentArgs) -> Result<()> {
    let payload = build_grok_payload(emit_root, &args.server_url, args.auth_token.as_deref());
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing Grok hook config")?;
    println!("# Grok Build CLI — write to ~/.grok/hooks/engram.json");
    println!("# Hook scripts (must be reachable from the host that runs Grok):");
    println!("#   {}", emit_root.display());
    println!("# AI-memory server: {}", args.server_url);
    if args.auth_token.is_some() {
        println!("# Auth: ENGRAM_AUTH_TOKEN embedded in each hook command below.");
        println!("#       Treat ~/.grok/hooks/engram.json as sensitive (chmod 600).");
    }
    println!("# NOTE: Grok ignores SessionStart stdout, so this config captures");
    println!("#       lifecycle events but does not inject handoffs automatically.");
    println!("#       Recover handoffs via the MCP memory_handoff_accept tool.");
    println!();
    println!("{serialized}");
    Ok(())
}

fn emit_other(
    emit_root: &Path,
    label: &str,
    args: &SetupAgentArgs,
    event_lists: &[&[(&str, &str)]],
) {
    // These clients have hook surfaces, but their print-mode config
    // snippets are intentionally conservative: apply-mode owns the
    // exact merge/plugin generation where engram knows the format.
    println!("# {label} hook scripts (manual wire-up; use install-hooks --apply when available)");
    println!("# Scripts located at: {}", emit_root.display());
    println!("# Server URL:         {}", args.server_url);
    if args.auth_token.is_some() {
        println!("# Auth: set ENGRAM_AUTH_TOKEN in each hook's environment to the");
        println!("#       value you passed via --auth-token (not echoed).");
    }
    println!();
    for path in event_script_paths(emit_root, event_lists) {
        println!("- {}", path.display());
    }
    println!();
    println!("Set ENGRAM_HOOK_URL in each hook's environment to override the default.");
    println!("Also run `engram install-mcp --client {label}` to wire MCP separately.");
}

fn event_script_paths(emit_root: &Path, event_lists: &[&[(&str, &str)]]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for events in event_lists {
        for (_, script) in *events {
            let script = hook_script_for_current_platform(script);
            paths.push(emit_root.join(script.as_ref()));
        }
    }
    paths
}

fn is_hook_script_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("sh" | "ps1")
    )
}

fn copy_support_hook_scripts(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    let Some(source_hooks_root) = source_dir.parent() else {
        return Ok(());
    };
    let source_lib = source_hooks_root.join("lib");
    if !source_lib.is_dir() {
        return Ok(());
    }
    let Some(dest_hooks_root) = dest_dir.parent() else {
        return Ok(());
    };
    let dest_lib = dest_hooks_root.join("lib");
    fs::create_dir_all(&dest_lib)
        .with_context(|| format!("creating hook support dir {}", dest_lib.display()))?;
    for entry in fs::read_dir(&source_lib)
        .with_context(|| format!("reading hook support dir {}", source_lib.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || from.extension().and_then(|s| s.to_str()) != Some("ps1") {
            continue;
        }
        let to = dest_lib.join(
            from.file_name()
                .with_context(|| format!("invalid hook support path {}", from.display()))?,
        );
        fs::copy(&from, &to)
            .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
    }
    Ok(())
}

fn resolve_source(explicit: Option<&Path>, sub: &str) -> Result<PathBuf> {
    let candidates = source_candidates(explicit, sub, std::env::current_exe().ok());
    for path in &candidates {
        if path.is_dir() {
            return Ok(path.clone());
        }
    }
    bail!(
        "could not locate hook source bundle for {sub}. \
         Tried: {candidates:?}. Pass --source <dir> to override."
    );
}

/// Ordered directories to probe for the `<sub>` hook bundle.
///
/// `exe` is the running binary's path (`std::env::current_exe()`), threaded
/// in so the derivation is unit-testable. An `explicit` `--source` is trusted
/// verbatim; otherwise we try two binary-relative spots:
///   * `<exe_dir>/hooks/<sub>` — the **release tarball** ships `hooks/` right
///     beside the binary (macOS/Windows archives), and
///   * `<exe_dir>/../../hooks/<sub>` — `cargo run` from the repo, where the
///     binary lives under `target/<profile>/`.
///
/// Without the binary-sibling entry the flat tarball layout was unreachable:
/// from `/private/tmp/<dir>/engram` the `parent×3` dev fallback derived a
/// bogus `/private/hooks/<sub>` and discovery failed (issue #107).
fn source_candidates(explicit: Option<&Path>, sub: &str, exe: Option<PathBuf>) -> Vec<PathBuf> {
    if let Some(p) = explicit {
        return vec![p.join(sub)];
    }
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(exe) = exe {
        // Release tarball: `hooks/` sits in the same dir as the binary.
        if let Some(dir) = exe.parent() {
            v.push(dir.join("hooks").join(sub));
        }
        // Repo-local fallback for `cargo run setup-agent` during dev:
        // target/<profile>/<bin> → repo root.
        if let Some(root) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            v.push(root.join("hooks").join(sub));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_candidates_include_binary_sibling_for_flat_tarball() {
        // Flat release tarball: the binary and its `hooks/` bundle are
        // extracted side by side. On macOS the path resolves under
        // /private/..., which the old `parent×3` dev fallback turned into a
        // bogus `/private/hooks/...` (issue #107).
        let exe = PathBuf::from("/private/tmp/engram-macos-aarch64/engram");
        let candidates = source_candidates(None, "claude-code", Some(exe));

        assert!(
            candidates.contains(&PathBuf::from(
                "/private/tmp/engram-macos-aarch64/hooks/claude-code"
            )),
            "binary-sibling hooks/ dir must be probed; got {candidates:?}"
        );
        // With the packaged install dirs gone, the binary-sibling tarball
        // layout is now the first candidate.
        assert_eq!(
            candidates[0],
            PathBuf::from("/private/tmp/engram-macos-aarch64/hooks/claude-code")
        );
    }

    #[test]
    fn source_candidates_preserve_cargo_run_repo_root() {
        // `cargo run`: target/<profile>/<bin> → repo root holds `hooks/`.
        let exe = PathBuf::from("/home/dev/engram/target/debug/engram");
        let candidates = source_candidates(None, "claude-code", Some(exe));
        assert!(
            candidates.contains(&PathBuf::from("/home/dev/engram/hooks/claude-code")),
            "repo-root hooks/ dir must still be probed; got {candidates:?}"
        );
    }

    #[test]
    fn source_candidates_honour_explicit_override() {
        let candidates = source_candidates(Some(Path::new("/custom/src")), "codex", None);
        assert_eq!(candidates, vec![PathBuf::from("/custom/src/codex")]);
    }

    #[test]
    fn pi_setup_prints_extension_hint_without_copying() {
        let tmp = tempfile::TempDir::new().unwrap();
        let args = SetupAgentArgs {
            agent: AgentChoice::Pi,
            to: tmp.path().join("hooks"),
            host_prefix: None,
            server_url: "http://127.0.0.1:49374".into(),
            auth_token: None,
            source: Some(tmp.path().join("source")),
        };

        run(&Config::default(), args).unwrap();

        assert!(!tmp.path().join("hooks").exists());
    }

    #[test]
    fn manual_script_paths_use_agent_specific_events() {
        let root = Path::new("/hooks/gemini-cli");
        let paths = event_script_paths(root, &[GEMINI_PROFILE.events]);
        let rendered = paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("session-start"));
        assert!(rendered.contains("session-end"));
        assert!(rendered.contains("pre-tool-use"));
        assert!(rendered.contains("post-tool-use"));
        assert!(rendered.contains("pre-compact"));
        assert!(
            !rendered.contains("user-prompt-submit"),
            "Gemini has no UserPromptSubmit hook; setup-agent must not print Claude-only scripts"
        );
        assert!(
            !rendered.contains("subagent-start") && !rendered.contains("subagent-stop"),
            "Gemini has no subagent hook events; setup-agent must not print nonexistent scripts"
        );
    }
}
