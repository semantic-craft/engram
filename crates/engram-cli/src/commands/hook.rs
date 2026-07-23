//! `engram hook` — emit a single lifecycle event natively.
//!
//! Reads the event payload from stdin. Instead of POSTing synchronously on the
//! agent's hot path (which would block every tool call on the network and drop
//! events against a slow/remote server), the event is **spooled** locally — an
//! instant write. `session-start` performs a short, lock-aware synchronous
//! cleanup pass before fetching handoff context. `session-end` returns quickly:
//! after enqueue it spawns a detached `hook-drain` process, whose stdout/stderr
//! are redirected away from the agent, and that process drains under an
//! exclusive spool lock with a longer bounded budget.
//!
//! See `docs/windows.md#native-hook-command-claude-code-on-windows`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use engram_core::AgentKind;
use engram_llm::OidcToken;

use crate::cli::HookArgs;

use super::hook_capture::{build_client, extract_cwd, get_handoff, marker_query_suffix};
use super::hook_drain_process;
use super::hook_spool;
use super::path_util::strip_windows_verbatim_prefix;

// All drain/handoff timings default to the current short values and can be
// overridden by whole-minute env vars for very high-latency or large-backlog
// instances. Two kinds: per-request timeouts cap each individual POST / handoff
// GET; session-boundary budgets cap how long a boundary spends draining (so a
// boundary never hangs unbounded).
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_HANDOFF_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_START_BUDGET: Duration = Duration::from_secs(3);
const DEFAULT_BACKGROUND_DRAIN_BUDGET: Duration = Duration::from_secs(5 * 60);
const MAX_OVERRIDE_MINUTES: u64 = 60;

const DRAIN_TIMEOUT_ENV: &str = "ENGRAM_HOOK_DRAIN_TIMEOUT_MINUTES";
const HANDOFF_TIMEOUT_ENV: &str = "ENGRAM_HOOK_HANDOFF_TIMEOUT_MINUTES";
const START_BUDGET_ENV: &str = "ENGRAM_HOOK_START_BUDGET_MINUTES";
const BACKGROUND_DRAIN_BUDGET_ENV: &str = "ENGRAM_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES";

const INCREMENTAL_THRESHOLD_ENV: &str = "ENGRAM_HOOK_INCREMENTAL_THRESHOLD";
/// Backlog size at which `post-tool-use` does a mid-session catch-up drain, so a
/// light session pays only a `read_dir`. Override via the env var above.
const DEFAULT_INCREMENTAL_THRESHOLD: usize = 32;
/// Total budget AND per-event timeout for the mid-session catch-up drain — kept
/// well under a second so a `post-tool-use` hook never stalls a tool call (one
/// in-flight POST against a slow server is bounded by this too).
const INCREMENTAL_DRAIN_BUDGET: Duration = Duration::from_millis(250);

/// Per-event POST timeout during a drain. Env: `ENGRAM_HOOK_DRAIN_TIMEOUT_MINUTES`.
fn drain_event_timeout() -> Duration {
    drain_event_timeout_from(env_lookup)
}
/// Synchronous handoff GET timeout. Env: `ENGRAM_HOOK_HANDOFF_TIMEOUT_MINUTES`.
fn handoff_timeout() -> Duration {
    handoff_timeout_from(env_lookup)
}
/// Total budget for the `session-start` cleanup drain (kept tight so session
/// start stays snappy even when the server is down — leftovers wait). Env:
/// `ENGRAM_HOOK_START_BUDGET_MINUTES`.
fn start_drain_budget() -> Duration {
    start_drain_budget_from(env_lookup)
}
/// Total budget for detached background drains. Env:
/// `ENGRAM_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES`.
fn background_drain_budget() -> Duration {
    background_drain_budget_from(env_lookup)
}

fn drain_event_timeout_from(lookup: impl FnMut(&str) -> Option<String>) -> Duration {
    env_minutes(DRAIN_TIMEOUT_ENV, DEFAULT_DRAIN_TIMEOUT, lookup)
}

fn handoff_timeout_from(lookup: impl FnMut(&str) -> Option<String>) -> Duration {
    env_minutes(HANDOFF_TIMEOUT_ENV, DEFAULT_HANDOFF_TIMEOUT, lookup)
}

fn start_drain_budget_from(lookup: impl FnMut(&str) -> Option<String>) -> Duration {
    env_minutes(START_BUDGET_ENV, DEFAULT_START_BUDGET, lookup)
}

fn background_drain_budget_from(lookup: impl FnMut(&str) -> Option<String>) -> Duration {
    env_minutes(
        BACKGROUND_DRAIN_BUDGET_ENV,
        DEFAULT_BACKGROUND_DRAIN_BUDGET,
        lookup,
    )
}

/// Backlog size at which `post-tool-use` triggers a mid-session catch-up drain.
/// Env: `ENGRAM_HOOK_INCREMENTAL_THRESHOLD` (positive integer).
fn incremental_drain_threshold() -> usize {
    incremental_drain_threshold_from(env_lookup)
}

fn incremental_drain_threshold_from(mut lookup: impl FnMut(&str) -> Option<String>) -> usize {
    lookup(INCREMENTAL_THRESHOLD_ENV)
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_INCREMENTAL_THRESHOLD)
}

/// Whether to run a mid-session catch-up drain for this event: only
/// `post-tool-use` (the highest-frequency event) and only once the spool backlog
/// has crossed `threshold`. Boundaries run their own cleanup/background drains,
/// so a light session never drains mid-session.
fn should_incremental_drain(event: &str, spool_len: usize, threshold: usize) -> bool {
    event == "post-tool-use" && spool_len >= threshold
}

fn spawn_background_drainer(data_dir: &Path) -> std::io::Result<()> {
    hook_drain_process::spawn(data_dir)
}

fn should_spawn_background_drainer(event: &str) -> bool {
    matches!(event, "session-end" | "stop" | "pre-compact")
}

fn after_background_drain_event_enqueue(
    data_dir: &Path,
    spawn: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    spawn(data_dir)
}

/// Hidden drain-only fast path. Reads no stdin and writes no stdout.
pub async fn run_drain(data_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let dd = resolve_data_dir(data_dir.as_deref());
    let spool = hook_spool::spool_dir(&dd);
    match hook_spool::drain_until_quiescent(
        &spool,
        &dd,
        background_drain_budget(),
        drain_event_timeout(),
        hook_spool::DrainLockWait::Bounded(Duration::from_secs(30)),
    )
    .await
    {
        Ok(hook_spool::LockedDrainResult::Drained(_))
        | Ok(hook_spool::LockedDrainResult::LockBusy) => {}
        Err(err) => eprintln!("engram hook-drain warning: failed to acquire drain lock: {err}"),
    }
    Ok(())
}

fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Read a positive-integer minute override from `name`, falling back to the
/// built-in short default for missing / empty / non-numeric / zero values. Clamp
/// large values so a typo cannot block a hook boundary for hours or days.
fn env_minutes(
    name: &str,
    default: Duration,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Duration {
    parse_minutes(lookup(name), default)
}

fn parse_minutes(raw: Option<String>, default: Duration) -> Duration {
    let minutes = raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(|n| n.min(MAX_OVERRIDE_MINUTES));
    match minutes {
        Some(n) => Duration::from_secs(n * 60),
        None => default,
    }
}

/// Run a single hook end-to-end. Always returns Ok and always writes a JSON
/// object to stdout — a hook must never fail the agent.
///
/// `data_dir` is the resolved global `--data-dir` (if any); used to locate the
/// spool and the stored OIDC token.
pub async fn run(data_dir: Option<PathBuf>, args: HookArgs) -> anyhow::Result<()> {
    let mut payload = String::new();
    std::io::stdin().read_to_string(&mut payload).ok();
    let mut stdout = std::io::stdout();
    run_with_payload(
        data_dir,
        args,
        payload,
        &mut stdout,
        spawn_background_drainer,
    )
    .await
}

async fn run_with_payload<W, S>(
    data_dir: Option<PathBuf>,
    args: HookArgs,
    payload: String,
    stdout: &mut W,
    spawn_background_drainer: S,
) -> anyhow::Result<()>
where
    W: std::io::Write,
    S: FnOnce(&Path) -> std::io::Result<()>,
{
    let json: serde_json::Value = serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);

    let qs = extract_cwd(&json)
        .map(|cwd| marker_query_suffix(&cwd, args.project_strategy.and_then(|s| s.baked())))
        .unwrap_or_default();
    let base = args.server_url.trim_end_matches('/');
    let dd = resolve_data_dir(data_dir.as_deref());
    let spool = hook_spool::spool_dir(&dd);

    // Spool THIS event — an instant local write, never the network. The auth
    // mode is decided without a round-trip: an explicit `--auth-token` is
    // stored inline; otherwise a present OIDC token marks the event `oidc`
    // (resolved + refreshed at drain time); otherwise anonymous.
    let oidc_present = args.auth_token.is_none()
        && OidcToken::load(&dd.join("auth.json"))
            .ok()
            .flatten()
            .is_some();
    let event_url = format!("{base}/hook?event={}&agent={}{qs}", args.event, args.agent);
    let entry = hook_spool::entry_for(
        event_url,
        payload.clone(),
        args.auth_token.as_deref(),
        oidc_present,
    );
    if hook_spool::enqueue(&spool, &entry).is_err() {
        eprintln!(
            "engram hook warning: failed to spool lifecycle event; capture for this event was skipped"
        );
    }

    // Mid-session catch-up: per-event hooks only enqueue, so a heavy session
    // outpaces the boundary-only drain and the spool grows until the next
    // boundary. On `post-tool-use`, once the backlog crosses the threshold, do a
    // tightly time-boxed drain (budget == per-event timeout, sub-second) so the
    // spool stays flat without ever stalling a tool call.
    if should_incremental_drain(
        &args.event,
        hook_spool::spool_len(&spool),
        incremental_drain_threshold(),
    ) {
        let _ = hook_spool::drain_exclusive(
            &spool,
            &dd,
            INCREMENTAL_DRAIN_BUDGET,
            INCREMENTAL_DRAIN_BUDGET,
            hook_spool::DrainLockWait::NoWait,
        )
        .await;
    }

    // session-start: drain any backlog (e.g. from a previous session that ended
    // abruptly), then fetch + inject the pending handoff for the resuming agent.
    if args.event == "session-start" {
        let _ = hook_spool::drain_exclusive_within_budget(
            &spool,
            &dd,
            start_drain_budget(),
            drain_event_timeout(),
        )
        .await;
        // Only fetch the handoff for agents that inject the session-start
        // hook's stdout as context. Grok ignores it, so fetching here would
        // consume the handoff server-side (the GET is destructive) and then
        // discard the result — silently losing it. Those agents recover the
        // handoff on demand via the MCP `memory_handoff_accept` tool.
        if AgentKind::from_wire(&args.agent).session_start_injects_handoff() {
            let client = build_client();
            let bearer = hook_spool::resolve_bearer(&client, &dd, args.auth_token.as_deref()).await;
            let handoff_url = format!("{base}/handoff?agent={}{qs}", args.agent);
            if let Some(handoff) =
                get_handoff(&client, &handoff_url, bearer.as_deref(), handoff_timeout()).await
            {
                let envelope = serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "SessionStart",
                        "additionalContext": handoff,
                    }
                });
                writeln!(stdout, "{envelope}")?;
                return Ok(());
            }
        }
    }

    // Boundary drain trigger: enqueue first, then ask a detached native drainer
    // to flush the shared spool. `session-end` remains the primary close path,
    // but `stop` and `pre-compact` also trigger the helper so delivery does not
    // rely on the single hook most likely to be cancelled during agent shutdown.
    if should_spawn_background_drainer(&args.event)
        && let Err(err) = after_background_drain_event_enqueue(&dd, spawn_background_drainer)
    {
        eprintln!(
            "engram hook warning: failed to start background spool drainer; event remains queued: {err}"
        );
    }

    writeln!(stdout, "{{}}")?;
    Ok(())
}

/// Resolve the data dir cheaply, without loading the full config (the hook
/// fast-path skips config for latency). Mirrors `config.rs`: explicit
/// `--data-dir`, else `ENGRAM_DATA_DIR`, else the platform local-data dir.
fn resolve_data_dir(data_dir: Option<&Path>) -> PathBuf {
    let dir = data_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("ENGRAM_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("engram")
        });
    // Recover already-installed hooks that baked a safe verbatim data-dir form.
    match dir.to_str() {
        Some(s) if s.starts_with(r"\\?\") => {
            PathBuf::from(strip_windows_verbatim_prefix(s).into_owned())
        }
        _ => dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_data_dir_strips_verbatim_prefix_from_baked_arg() {
        // Recover safe verbatim data dirs baked by older installs (#116).
        let resolved = resolve_data_dir(Some(Path::new(r"\\?\C:\Users\me\AppData\Local\engram")));
        assert_eq!(resolved, PathBuf::from(r"C:\Users\me\AppData\Local\engram"));
    }

    #[test]
    fn resolve_data_dir_leaves_plain_path_untouched() {
        let resolved = resolve_data_dir(Some(Path::new(r"C:\Users\me\engram")));
        assert_eq!(resolved, PathBuf::from(r"C:\Users\me\engram"));
    }

    #[test]
    fn should_incremental_drain_only_post_tool_use_over_threshold() {
        assert!(should_incremental_drain("post-tool-use", 32, 32));
        assert!(should_incremental_drain("post-tool-use", 100, 32));
        // below threshold: a light session never drains mid-session
        assert!(!should_incremental_drain("post-tool-use", 31, 32));
        // other events only enqueue; boundaries do the real flush
        assert!(!should_incremental_drain("pre-tool-use", 999, 32));
        assert!(!should_incremental_drain("session-start", 999, 32));
        assert!(!should_incremental_drain("session-end", 999, 32));
        assert!(!should_incremental_drain("stop", 999, 32));
    }

    #[test]
    fn boundary_events_trigger_background_drainer() {
        assert!(should_spawn_background_drainer("session-end"));
        assert!(should_spawn_background_drainer("stop"));
        assert!(should_spawn_background_drainer("pre-compact"));

        assert!(!should_spawn_background_drainer("session-start"));
        assert!(!should_spawn_background_drainer("post-tool-use"));
        assert!(!should_spawn_background_drainer("pre-tool-use"));
        assert!(!should_spawn_background_drainer("user-prompt"));
    }

    #[test]
    fn incremental_threshold_parses_and_falls_back() {
        assert_eq!(incremental_drain_threshold_from(|_| Some("64".into())), 64);
        assert_eq!(
            incremental_drain_threshold_from(|_| None),
            DEFAULT_INCREMENTAL_THRESHOLD
        );
        // zero / non-numeric fall back to the default (a 0 threshold would drain
        // on every post-tool-use)
        assert_eq!(
            incremental_drain_threshold_from(|_| Some("0".into())),
            DEFAULT_INCREMENTAL_THRESHOLD
        );
        assert_eq!(
            incremental_drain_threshold_from(|_| Some("abc".into())),
            DEFAULT_INCREMENTAL_THRESHOLD
        );
    }

    #[test]
    fn parse_minutes_falls_back_on_invalid() {
        assert_eq!(
            parse_minutes(None, DEFAULT_DRAIN_TIMEOUT),
            DEFAULT_DRAIN_TIMEOUT
        );
        assert_eq!(
            parse_minutes(Some(String::new()), DEFAULT_DRAIN_TIMEOUT),
            DEFAULT_DRAIN_TIMEOUT
        );
        assert_eq!(
            parse_minutes(Some("abc".into()), DEFAULT_DRAIN_TIMEOUT),
            DEFAULT_DRAIN_TIMEOUT
        );
        // Zero is rejected (a 0-minute timeout would drop every request).
        assert_eq!(
            parse_minutes(Some("0".into()), DEFAULT_DRAIN_TIMEOUT),
            DEFAULT_DRAIN_TIMEOUT
        );
    }

    #[test]
    fn parse_minutes_honours_valid_override() {
        assert_eq!(
            parse_minutes(Some("2".into()), DEFAULT_DRAIN_TIMEOUT),
            Duration::from_secs(120)
        );
        assert_eq!(
            parse_minutes(Some("  3 ".into()), DEFAULT_DRAIN_TIMEOUT),
            Duration::from_secs(180)
        );
    }

    #[test]
    fn parse_minutes_clamps_large_values() {
        assert_eq!(
            parse_minutes(Some("999".into()), DEFAULT_DRAIN_TIMEOUT),
            Duration::from_secs(MAX_OVERRIDE_MINUTES * 60)
        );
    }

    #[test]
    fn background_drain_budget_defaults_and_clamps() {
        assert_eq!(
            background_drain_budget_from(|_| None),
            DEFAULT_BACKGROUND_DRAIN_BUDGET
        );
        assert_eq!(
            background_drain_budget_from(|_| Some("1".into())),
            Duration::from_secs(60)
        );
        assert_eq!(
            background_drain_budget_from(|_| Some("999".into())),
            Duration::from_secs(60 * 60)
        );
    }

    #[tokio::test]
    async fn session_end_run_enqueues_outputs_empty_json_and_spawns_after_enqueue() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let called = std::cell::Cell::new(0);
        let mut stdout = Vec::new();
        let args = HookArgs {
            event: "session-end".into(),
            agent: "claude-code".into(),
            server_url: "http://127.0.0.1:1".into(),
            auth_token: None,
            project_strategy: None,
        };

        run_with_payload(
            Some(data_dir.clone()),
            args,
            r#"{"session_id":"s","cwd":"/tmp"}"#.into(),
            &mut stdout,
            |path| {
                assert_eq!(path, data_dir.as_path());
                assert_eq!(hook_spool::spool_len(&spool), 1, "spawn runs after enqueue");
                called.set(called.get() + 1);
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        assert_eq!(called.get(), 1);
        assert_eq!(
            hook_spool::spool_len(&spool),
            1,
            "session-end must not drain inline"
        );
    }

    #[tokio::test]
    async fn stop_and_pre_compact_spawn_background_drainer_after_enqueue() {
        for event in ["stop", "pre-compact"] {
            let tmp = tempfile::tempdir().unwrap();
            let data_dir = tmp.path().to_path_buf();
            let spool = hook_spool::spool_dir(&data_dir);
            let called = std::cell::Cell::new(0);
            let mut stdout = Vec::new();
            let args = HookArgs {
                event: event.into(),
                agent: "claude-code".into(),
                server_url: "http://127.0.0.1:1".into(),
                auth_token: None,
                project_strategy: None,
            };

            run_with_payload(
                Some(data_dir.clone()),
                args,
                r#"{"session_id":"s","cwd":"/tmp"}"#.into(),
                &mut stdout,
                |path| {
                    assert_eq!(path, data_dir.as_path());
                    assert_eq!(hook_spool::spool_len(&spool), 1, "spawn runs after enqueue");
                    called.set(called.get() + 1);
                    Ok(())
                },
            )
            .await
            .unwrap();

            assert_eq!(stdout, b"{}\n", "{event} should keep hook stdout clean");
            assert_eq!(called.get(), 1, "{event} should start background drain");
            assert_eq!(
                hook_spool::spool_len(&spool),
                1,
                "{event} must not drain inline"
            );
        }
    }

    #[tokio::test]
    async fn session_end_run_spawn_failure_keeps_event_queued_and_stdout_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let mut stdout = Vec::new();
        let args = HookArgs {
            event: "session-end".into(),
            agent: "claude-code".into(),
            server_url: "http://127.0.0.1:1".into(),
            auth_token: None,
            project_strategy: None,
        };

        run_with_payload(Some(data_dir), args, "{}".into(), &mut stdout, |_path| {
            Err(std::io::Error::other("spawn failed"))
        })
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        assert_eq!(hook_spool::spool_len(&spool), 1);
    }

    #[test]
    fn session_end_spawn_failure_is_returned_for_warning_only() {
        let tmp = tempfile::tempdir().unwrap();
        let err = after_background_drain_event_enqueue(tmp.path(), |_path| {
            Err(std::io::Error::other("spawn failed"))
        })
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }

    #[test]
    fn background_drain_event_policy_spawns_without_inline_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let called = std::cell::Cell::new(false);

        after_background_drain_event_enqueue(tmp.path(), |path| {
            assert_eq!(path, tmp.path());
            called.set(true);
            Ok(())
        })
        .unwrap();

        assert!(called.get());
    }

    #[test]
    fn timing_accessors_read_the_expected_env_vars() {
        fn one_minute_for(expected_name: &'static str) -> impl FnMut(&str) -> Option<String> {
            move |actual_name| {
                assert_eq!(actual_name, expected_name);
                Some("1".to_string())
            }
        }

        assert_eq!(
            drain_event_timeout_from(one_minute_for(DRAIN_TIMEOUT_ENV)),
            Duration::from_secs(60)
        );
        assert_eq!(
            handoff_timeout_from(one_minute_for(HANDOFF_TIMEOUT_ENV)),
            Duration::from_secs(60)
        );
        assert_eq!(
            start_drain_budget_from(one_minute_for(START_BUDGET_ENV)),
            Duration::from_secs(60)
        );
        assert_eq!(
            background_drain_budget_from(one_minute_for(BACKGROUND_DRAIN_BUDGET_ENV)),
            Duration::from_secs(60)
        );
    }
}
