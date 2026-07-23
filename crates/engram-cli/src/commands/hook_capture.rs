//! Native lifecycle-hook capture helpers.
//!
//! Mirrors the POSIX `hooks/lib/_lib.sh` logic so the native
//! `engram hook` subcommand produces the same HTTP request the shell
//! scripts do: extract cwd from the payload, walk up for a
//! `.engram.toml` marker, and build the query-string suffix. The two
//! request helpers are best-effort with shell-parity timeouts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::commands::path_util::home_dir;

/// First top-level `cwd` string in the payload (parity with
/// `engram_extract_cwd`: take the top-level value, ignore nested
/// `cwd` fields in tool payloads).
pub fn extract_cwd(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Percent-encode everything outside the RFC 3986 unreserved set
/// (`A-Z a-z 0-9 - _ . ~`), byte-wise, so multibyte UTF-8 is encoded
/// per byte. Parity with `engram_url_encode` in `hooks/_lib.sh`.
///
/// An allow-list on purpose: the old deny-list missed `\` (and friends),
/// so a Windows cwd like `C:\dev\myproject` went into the query string
/// raw and the HTTP layer refused the request — the session-start hook
/// printed `{}` and the pending handoff was never fetched (#188).
/// Over-encoding is always safe; the server percent-decodes uniformly.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            other => {
                out.push_str(&format!("%{other:02X}"));
            }
        }
    }
    out
}

/// Build `&cwd=…[&workspace=…&project=…&project_strategy=…]`, mirroring
/// `engram_marker_qs`: always include cwd; append marker-declared
/// fields when a `.engram.toml` is found walking up toward $HOME.
///
/// `default_strategy` is the install-time default baked into the native hook
/// command by `install-hooks --project-strategy` (passed via the `hook
/// --project-strategy` flag). It fills `project_strategy` only when no marker
/// pinned one — a marker's explicit `project` / `project_strategy` always win
/// (§3.3). repo-root is resolved here, host-side, because a containerized
/// server cannot see this checkout.
pub fn marker_query_suffix(cwd: &str, default_strategy: Option<&str>) -> String {
    let mut qs = format!("&cwd={}", url_encode(cwd));
    let (mut workspace, mut project, mut strategy, mut drop_subagent) = (None, None, None, None);
    let (mut briefing, mut briefing_budget) = (None, None);
    if let Some(marker) = find_marker(cwd) {
        workspace = parse_toml_key(&marker, "workspace");
        project = parse_toml_key(&marker, "project");
        strategy = parse_toml_key(&marker, "project_strategy");
        drop_subagent = parse_toml_key(&marker, "drop_subagent_captures");
        // `[briefing] inject_on_session_start = true` + optional
        // `max_chars = N` — opt this repo into the compiled project brief
        // appended to the session-start handoff fetch (#176).
        briefing = parse_toml_flag(&marker, "inject_on_session_start");
        briefing_budget = parse_toml_flag(&marker, "max_chars");
    }
    if strategy.is_none() {
        strategy = default_strategy.map(str::to_owned);
    }
    if project.is_none() && matches!(strategy.as_deref(), Some("repo-root" | "repo_root")) {
        project = repo_root_project(cwd);
    }
    if let Some(val) = workspace {
        qs.push_str(&format!("&workspace={}", url_encode(&val)));
    }
    if let Some(val) = project {
        qs.push_str(&format!("&project={}", url_encode(&val)));
    }
    if let Some(val) = strategy {
        qs.push_str(&format!("&project_strategy={}", url_encode(&val)));
    }
    // Per-project `drop_subagent_captures` opt-in: forward the marker's value as
    // the `drop_subagent` flag so the server scopes the drop to this project.
    // The server interprets truthiness (`1`/`true`/…).
    if let Some(val) = drop_subagent.filter(|v| !v.is_empty()) {
        qs.push_str(&format!("&drop_subagent={}", url_encode(&val)));
    }
    // Per-repo session-start brief opt-in: forwarded on every request for
    // simplicity (the capture path ignores it); only the `/handoff` GET at
    // session start acts on it. Truthiness and the char-budget clamp are
    // decided server-side.
    if let Some(val) = briefing.filter(|v| !v.is_empty()) {
        qs.push_str(&format!("&briefing={}", url_encode(&val)));
    }
    if let Some(val) = briefing_budget.filter(|v| !v.is_empty()) {
        qs.push_str(&format!("&briefing_budget={}", url_encode(&val)));
    }
    qs
}

fn repo_root_project(cwd: &str) -> Option<String> {
    let root = engram_consolidate::discover_main_repo_root(Path::new(cwd)).ok()?;
    root.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Walk up from `cwd` toward `$HOME` (or the filesystem root) looking
/// for `.engram.toml`. Stops at `$HOME` to avoid leaking a parent
/// user's declaration on shared machines (parity with
/// `engram_find_marker`).
fn find_marker(cwd: &str) -> Option<PathBuf> {
    let home = home_dir();
    let mut dir = Path::new(cwd);
    loop {
        let candidate = dir.join(".engram.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if home.as_deref() == Some(dir) {
            return None;
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent,
            _ => return None,
        }
    }
}

/// Parse a root-level `key = "value"` line (no nesting, arrays, or
/// tables), mirroring `engram_parse_toml_key`. Returns the first
/// match. Avoids pulling in a TOML parser dependency.
fn parse_toml_key(file: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(after_key) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = after_key.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Like [`parse_toml_key`] but also accepts a bare (unquoted) token —
/// `inject_on_session_start = true`, `max_chars = 6000` — and strips a
/// trailing `#` comment. Section headers are ignored (flat key scan), so
/// keys under `[briefing]` resolve the same as root-level keys.
fn parse_toml_flag(file: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(after_key) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = after_key.trim_start().strip_prefix('=') else {
            continue;
        };
        let val = rest
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        if !val.is_empty() {
            return Some(val.to_string());
        }
    }
    None
}

/// Build a reqwest client for the hook's one-shot requests. `no_proxy`
/// skips Windows proxy auto-detection (registry / WinINET lookups), which
/// is pure overhead for a loopback/LAN POST. Built once per invocation and
/// reused for both the event POST and the handoff GET. Default root certs
/// are kept so HTTPS targets (e.g. a TLS proxy) still work.
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Outcome of one spooled-event POST — enough for the drain loop to decide
/// whether a miss should cost the entry a retry attempt. Never errors, so a
/// hook/flush never fails the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostOutcome {
    /// Server acknowledged with a 2xx (the engine answers `202 queued`): the
    /// entry was delivered and can be removed from the spool.
    Delivered,
    /// Server answered `429 Too Many Requests` (`hook queue full`): transient
    /// backpressure, the event was never processed. Keep it queued WITHOUT
    /// bumping attempts so saturation never burns the entry's retry budget.
    Saturated,
    /// Any other non-2xx, or a transport error: a genuine miss that should
    /// count against `MAX_ATTEMPTS`.
    Failed,
}

/// POST the payload as JSON, best-effort. `timeout` is caller-chosen: the
/// per-tool-call hot path no longer POSTs at all (it spools); the drain calls
/// this with a budget that tolerates a remote/slow server. Returns a
/// [`PostOutcome`] (never errors) so the drain can give a 429 (saturation) a
/// free retry while still bounding genuine failures by `MAX_ATTEMPTS`.
pub async fn post_hook(
    client: &reqwest::Client,
    url: &str,
    body: &str,
    token: Option<&str>,
    timeout: Duration,
) -> PostOutcome {
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .timeout(timeout)
        .body(body.to_owned());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => PostOutcome::Delivered,
        Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
            PostOutcome::Saturated
        }
        Ok(_) => PostOutcome::Failed,
        Err(_) => PostOutcome::Failed,
    }
}

/// Outcome of one `POST /hook/batch` request — many spooled events delivered in
/// a single round-trip, so a draining client amortizes TLS + network RTT + the
/// edge auth hop over the whole batch instead of paying it per event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOutcome {
    /// Server committed the leading `usize` items (contiguous prefix, oldest
    /// first). Equals the request length on full success; a smaller value means
    /// the server stopped on that item (fail-fast) — the caller deletes the
    /// prefix and charges the next item a retry.
    Accepted(usize),
    /// `429` — ingest saturated after committing this many leading items. The
    /// caller deletes that prefix and retries the rest later WITHOUT bumping
    /// attempts (saturation isn't a failure).
    Saturated(usize),
    /// `404`/`405` — the server has no `/hook/batch` (a pre-upgrade build). The
    /// caller falls back to per-event `POST /hook` for the rest of the drain.
    Unsupported,
    /// Transport error or any other non-2xx: the batch outcome is unknown. The
    /// drain charges conservatively so trailing events that may never have been
    /// attempted do not burn retry budget.
    Failed,
}

/// POST a pre-serialized JSON array of `{url, body}` events to `<batch_url>`.
/// `bearer` authenticates the whole batch (every item shares the drain's single
/// identity). Best-effort: never errors. Reads `{"accepted": K}` from a 2xx
/// body; a 2xx whose body can't be read is treated as `Failed` (re-send rather
/// than risk dropping undelivered events).
pub async fn post_batch(
    client: &reqwest::Client,
    batch_url: &str,
    payload: &str,
    token: Option<&str>,
    timeout: Duration,
) -> BatchOutcome {
    let mut req = client
        .post(batch_url)
        .header("Content-Type", "application/json")
        .timeout(timeout)
        .body(payload.to_owned());
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                match resp.json::<serde_json::Value>().await {
                    Ok(v) => {
                        let accepted = v
                            .get("accepted")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0) as usize;
                        BatchOutcome::Accepted(accepted)
                    }
                    Err(_) => BatchOutcome::Failed,
                }
            } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let accepted = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("accepted").and_then(serde_json::Value::as_u64))
                    .unwrap_or(0) as usize;
                BatchOutcome::Saturated(accepted)
            } else if status == reqwest::StatusCode::NOT_FOUND
                || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            {
                BatchOutcome::Unsupported
            } else {
                BatchOutcome::Failed
            }
        }
        Err(_) => BatchOutcome::Failed,
    }
}

/// GET the handoff text with a caller-chosen budget. Returns None on any error
/// or an empty body. This is the one synchronous read on the agent's critical
/// path (session-start injects it as context), so the budget is larger than a
/// loopback default to tolerate a remote server.
pub async fn get_handoff(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Option<String> {
    let mut req = client.get(url).timeout(timeout);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    // Warn on stderr instead of failing silently: the hook still exits 0 (a
    // hook must never break the agent), but an unreachable server would
    // otherwise be indistinguishable from "no pending handoff" (#188).
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "engram hook warning: handoff fetch failed ({e}); \
                 a pending handoff (if any) was NOT injected"
            );
            return None;
        }
    };
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    if body.is_empty() { None } else { Some(body) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_once(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}/handoff")
    }

    #[test]
    fn extracts_top_level_cwd() {
        let p: serde_json::Value =
            serde_json::from_str(r#"{"cwd":"/d/proj","tool_input":{"cwd":"/nested"}}"#).unwrap();
        assert_eq!(extract_cwd(&p).as_deref(), Some("/d/proj"));
    }

    #[test]
    fn missing_cwd_is_none() {
        let p: serde_json::Value = serde_json::from_str(r#"{"x":1}"#).unwrap();
        assert_eq!(extract_cwd(&p), None);
    }

    #[test]
    fn query_suffix_without_marker_has_only_cwd() {
        let qs = marker_query_suffix("/nonexistent/path/xyz", None);
        assert_eq!(qs, "&cwd=%2Fnonexistent%2Fpath%2Fxyz");
    }

    #[test]
    fn url_encode_escapes_reserved() {
        assert_eq!(url_encode("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[tokio::test]
    async fn post_hook_failed_when_server_unreachable() {
        // Port 1 is unroutable; best-effort means this resolves to `Failed`
        // (a genuine miss) rather than panicking or erroring.
        let client = build_client();
        let outcome = post_hook(
            &client,
            "http://127.0.0.1:1/hook?event=pre-tool-use",
            "{}",
            None,
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(outcome, PostOutcome::Failed);
    }

    #[tokio::test]
    async fn post_hook_saturated_on_429() {
        // 429 = server backpressure; the event was never processed, so the
        // drain must treat it as a free retry, not a failed attempt.
        let url = serve_once("429 Too Many Requests", "hook queue full").await;
        let outcome = post_hook(&build_client(), &url, "{}", None, Duration::from_secs(1)).await;
        assert_eq!(outcome, PostOutcome::Saturated);
    }

    #[tokio::test]
    async fn post_hook_delivered_on_2xx() {
        let url = serve_once("202 Accepted", "queued").await;
        let outcome = post_hook(&build_client(), &url, "{}", None, Duration::from_secs(1)).await;
        assert_eq!(outcome, PostOutcome::Delivered);
    }

    #[tokio::test]
    async fn get_handoff_ignores_non_success_status() {
        let url = serve_once("401 Unauthorized", "unauthorized").await;
        let got = get_handoff(&build_client(), &url, None, Duration::from_secs(1)).await;
        assert!(got.is_none(), "non-2xx body must not become context");
    }

    /// Happy-path TOML parser: extracts each declared root-level
    /// `key = "value"` pair. Mirrors the shell `engram_parse_toml_key`.
    #[test]
    fn parse_toml_key_extracts_root_level_strings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".engram.toml");
        std::fs::write(
            &marker,
            r#"
workspace = "acme"
project = "infra"
project_strategy = "repo-root"
"#,
        )
        .unwrap();
        assert_eq!(
            parse_toml_key(&marker, "workspace").as_deref(),
            Some("acme")
        );
        assert_eq!(parse_toml_key(&marker, "project").as_deref(), Some("infra"));
        assert_eq!(
            parse_toml_key(&marker, "project_strategy").as_deref(),
            Some("repo-root")
        );
        assert_eq!(parse_toml_key(&marker, "absent"), None);
    }

    /// Shapes the naive parser deliberately doesn't handle (parity with
    /// the shell `_lib.sh` helper) — pin the contract so a future
    /// "robustify" refactor doesn't silently start matching them.
    #[test]
    fn parse_toml_key_skips_unsupported_shapes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".engram.toml");
        std::fs::write(
            &marker,
            r#"
# Single-quoted values are not honoured.
workspace = 'acme'
# Comments after the value are not stripped.
project = "infra" # this is fine
"#,
        )
        .unwrap();
        assert_eq!(parse_toml_key(&marker, "workspace"), None);
        // The trailing comment is appended to the value because the parser
        // looks for the first `"` — pin it so the contract is explicit.
        assert_eq!(parse_toml_key(&marker, "project").as_deref(), Some("infra"));
    }

    /// `find_marker` walks up from `cwd` until it finds `.engram.toml`
    /// or reaches `$HOME`. Verify the walking — drop the marker two dirs
    /// above the simulated cwd and confirm it's found.
    #[test]
    fn find_marker_walks_up_from_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".engram.toml");
        std::fs::write(&marker, "workspace = \"w\"\n").unwrap();
        let deep = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        let found = find_marker(deep.to_str().unwrap());
        assert_eq!(found.as_deref(), Some(marker.as_path()));
    }

    /// `marker_query_suffix` appends `&workspace=…&project=…` (and
    /// `&project_strategy=…`, `&drop_subagent=…`) when the marker declares them.
    /// Each value is URL-encoded, so a workspace with a space round-trips as `%20`.
    /// Regression for #188: a Windows cwd must be fully percent-encoded or
    /// the HTTP layer refuses the request URL and the session-start hook
    /// silently returns `{}` while the handoff stays pending.
    #[test]
    fn url_encode_is_an_unreserved_allow_list() {
        // The reported case: raw `\` and `:` broke the request outright.
        assert_eq!(url_encode(r"C:\dev\myproject"), "C%3A%5Cdev%5Cmyproject");
        // RFC 3986 unreserved passes through untouched.
        assert_eq!(url_encode("abc-XYZ_0.9~"), "abc-XYZ_0.9~");
        // Previous deny-list behavior is preserved (space, slash, etc.).
        assert_eq!(url_encode("/home/u/my repo"), "%2Fhome%2Fu%2Fmy%20repo");
        // Multibyte UTF-8 is encoded per byte.
        assert_eq!(url_encode("r\u{e9}po"), "r%C3%A9po");
    }

    /// The full marker suffix built from a Windows cwd must parse as a real
    /// URL query — the end-to-end guarantee behind the #188 fix.
    #[test]
    fn marker_query_suffix_windows_cwd_yields_parseable_url() {
        let qs = marker_query_suffix(r"C:\dev\myproject", None);
        assert!(qs.contains("cwd=C%3A%5Cdev%5Cmyproject"), "{qs}");
        let url = format!("http://127.0.0.1:49374/handoff?agent=claude-code{qs}");
        let parsed = reqwest::Url::parse(&url).expect("must be a valid URL");
        let cwd = parsed
            .query_pairs()
            .find(|(k, _)| k == "cwd")
            .map(|(_, v)| v.into_owned())
            .expect("cwd param present");
        assert_eq!(
            cwd, r"C:\dev\myproject",
            "round-trips through percent-decoding"
        );
    }

    #[test]
    fn marker_query_suffix_appends_marker_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(".engram.toml");
        std::fs::write(
            &marker,
            r#"
workspace = "acme corp"
project = "infra"
project_strategy = "repo-root"
drop_subagent_captures = "true"
"#,
        )
        .unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let qs = marker_query_suffix(cwd, None);
        // cwd is encoded first; marker fields follow in the iteration order
        // of the loop in `marker_query_suffix`.
        assert!(qs.contains("&workspace=acme%20corp"), "{qs}");
        assert!(qs.contains("&project=infra"), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
        assert!(qs.contains("&drop_subagent=true"), "{qs}");
    }

    /// A marker WITHOUT `drop_subagent_captures` does not forward the flag, so
    /// the server keeps that project's subagent captures (opt-in only).
    #[test]
    fn marker_query_suffix_omits_drop_subagent_when_unset() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".engram.toml"),
            "workspace = \"acme\"\nproject = \"infra\"\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), None);
        assert!(!qs.contains("drop_subagent"), "{qs}");
    }

    /// A `[briefing]` section (bare or quoted values) forwards the opt-in
    /// and the char budget so the session-start `/handoff` GET can compose
    /// the project brief (#176).
    #[test]
    fn marker_query_suffix_appends_briefing_opt_in() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".engram.toml"),
            "workspace = \"acme\"\n[briefing]\ninject_on_session_start = true\nmax_chars = 6000\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), None);
        assert!(qs.contains("&briefing=true"), "{qs}");
        assert!(qs.contains("&briefing_budget=6000"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_omits_briefing_when_unset() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".engram.toml"),
            "workspace = \"acme\"\nproject = \"infra\"\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), None);
        assert!(!qs.contains("briefing"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_repo_root_non_git_keeps_project_implicit() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".engram.toml"),
            "workspace = \"oss\"\nproject_strategy = \"repo-root\"\n",
        )
        .unwrap();
        let child = tmp.path().join("plain-dir");
        std::fs::create_dir_all(&child).unwrap();
        let qs = marker_query_suffix(child.to_str().unwrap(), None);
        assert!(qs.contains("&workspace=oss"), "{qs}");
        assert!(!qs.contains("&project="), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_repo_root_collapses_out_of_tree_worktree() {
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repos/acme-api");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "init",
                ])
                .status()
                .unwrap()
                .success()
        );

        let worktrees = tmp.path().join("worktrees");
        std::fs::create_dir_all(&worktrees).unwrap();
        std::fs::write(
            worktrees.join(".engram.toml"),
            "workspace = \"oss\"\nproject_strategy = \"repo-root\"\n",
        )
        .unwrap();
        let wt = worktrees.join("acme-api/wt-feature");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        if !std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-q"])
            .arg(&wt)
            .status()
            .unwrap()
            .success()
        {
            return;
        }

        let qs = marker_query_suffix(wt.to_str().unwrap(), None);
        assert!(qs.contains("&workspace=oss"), "{qs}");
        assert!(qs.contains("&project=acme-api"), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }

    // ── install-time default strategy (#128), no marker required ──────

    #[test]
    fn marker_query_suffix_default_repo_root_non_git_keeps_project_implicit() {
        // Baked repo-root default, no marker, not a git tree: the strategy is
        // forwarded but no project is derived (server falls back to basename).
        let tmp = tempfile::TempDir::new().unwrap();
        let child = tmp.path().join("plain-dir");
        std::fs::create_dir_all(&child).unwrap();
        let qs = marker_query_suffix(child.to_str().unwrap(), Some("repo-root"));
        assert!(!qs.contains("&project="), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_default_repo_root_collapses_git_subdir() {
        // Baked repo-root default, no marker, inside a git subdir: the project
        // collapses to the repo-root basename.
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("contentcreator");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        let sub = repo.join("transcripts");
        std::fs::create_dir_all(&sub).unwrap();
        let qs = marker_query_suffix(sub.to_str().unwrap(), Some("repo-root"));
        assert!(qs.contains("&project=contentcreator"), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }

    #[test]
    fn marker_query_suffix_marker_strategy_overrides_default() {
        // A marker that pins `project_strategy = "basename"` wins over the
        // install-time repo-root default — no repo-root derivation happens.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".engram.toml"),
            "project_strategy = \"basename\"\n",
        )
        .unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), Some("repo-root"));
        assert!(qs.contains("&project_strategy=basename"), "{qs}");
        assert!(!qs.contains("repo-root"), "{qs}");
        assert!(!qs.contains("&project="), "{qs}");
    }

    #[test]
    fn marker_query_suffix_marker_project_overrides_default_repo_root() {
        // A marker's explicit `project` wins over repo-root derivation, while
        // the baked default strategy is still forwarded.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".engram.toml"), "project = \"pinned\"\n").unwrap();
        let qs = marker_query_suffix(tmp.path().to_str().unwrap(), Some("repo-root"));
        assert!(qs.contains("&project=pinned"), "{qs}");
        assert!(qs.contains("&project_strategy=repo-root"), "{qs}");
    }
}
