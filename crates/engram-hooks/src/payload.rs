//! Wire envelope received on `POST /hook`.

use engram_core::{AgentKind, ObservationKind};
use serde::{Deserialize, Serialize};

/// Query-string parameters on `POST /hook`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookQuery {
    /// Lifecycle event identifier (kebab-case or snake_case).
    pub event: String,
    /// Agent CLI identifier (`claude-code`, `codex`, `cursor`, etc.).
    pub agent: Option<String>,
    /// Working directory of the agent at the time the hook fired.
    /// Most agents put this in the JSON body, but accepting it on the
    /// query string too lets `curl` / tests / non-Claude bridges
    /// populate it without constructing a body envelope.
    pub cwd: Option<String>,
    /// Workspace name override (typically declared by the agent's
    /// host-side hook via a `.engram.toml` walk-up). When `None`
    /// the server falls back to `DEFAULT_WORKSPACE_NAME`.
    pub workspace: Option<String>,
    /// Project name override (same source as `workspace`). When
    /// `None` the server falls back to `basename(cwd)`.
    pub project: Option<String>,
    /// Optional project derivation strategy from `.engram.toml`.
    /// `repo-root` makes the server derive project identity from the
    /// main git repository root instead of `basename(cwd)`.
    pub project_strategy: Option<String>,
    /// Optional third-party extension namespace. When present, engram
    /// preserves a validated source event name without expanding the
    /// closed core event vocabulary.
    pub extension: Option<String>,
    /// Optional explicit source event name for extension vocabularies.
    /// When omitted and `extension` is present, unknown `event` values
    /// are preserved as the source event.
    pub source_event: Option<String>,
    /// Per-project opt-in for `drop_subagent_captures`, forwarded by the
    /// host-side hook from a project's `.engram.toml`. A truthy value
    /// (`1`/`true`/…) makes the server accept-but-drop this project's subagent
    /// captures; absent/falsy leaves them stored. Scoping the drop to the
    /// project that asked for it avoids a server-global switch that would shed
    /// subagent captures for every project on a shared instance.
    pub drop_subagent: Option<String>,
}

/// Coalesced view of an incoming hook event after light parsing of the
/// body. We keep the original raw JSON around so consumers can extract
/// agent-specific fields they care about.
#[derive(Debug, Clone, Serialize)]
pub struct HookEnvelope {
    /// Mapped lifecycle event.
    pub event: HookEvent,
    /// Agent CLI identifier.
    pub agent: AgentKind,
    /// Session identifier, if found in the body. Required for everything
    /// except the initial `SessionStart`.
    pub session_id: Option<String>,
    /// Current working directory at the time of the event.
    pub cwd: Option<String>,
    /// Workspace name override declared by the hook (via marker file
    /// walk-up). Empty / `None` defers to `DEFAULT_WORKSPACE_NAME`.
    pub workspace_override: Option<String>,
    /// Project name override declared by the hook. Empty / `None`
    /// defers to `basename(cwd)`.
    pub project_override: Option<String>,
    /// Project derivation strategy declared by the hook marker.
    pub project_strategy: ProjectStrategy,
    /// Whether this project opted into `drop_subagent_captures` via its
    /// `.engram.toml` (forwarded as the `drop_subagent` query flag). The
    /// ingest router consults this per-event so the drop is scoped to the
    /// project that asked for it.
    pub drop_subagent_requested: bool,
    /// Optional third-party extension namespace.
    pub extension: Option<String>,
    /// Optional source event name from the extension vocabulary.
    pub source_event: Option<String>,
    /// Optional title hint extracted from the body.
    pub title_hint: Option<String>,
    /// Optional body excerpt extracted from the agent's raw payload.
    pub body_excerpt: Option<String>,
    /// The agent's raw JSON, kept for forensics.
    pub raw: serde_json::Value,
}

/// Keys by which agent harnesses tag a hook event as belonging to a SUBAGENT
/// (a nested/spawned agent session) rather than the top-level session. Grok
/// sets `subagentType` (on its tool-use events); Claude Code sets `agent_type`
/// and `agent_id` (on its `SubagentStart`/`SubagentStop` and subagent tool
/// events). The set is a union so one check covers every harness that signals
/// subagent-ness; a harness that does not signal it simply never matches.
const SUBAGENT_MARKER_KEYS: &[&str] = &["subagentType", "agent_type", "agent_id"];

/// True when the raw hook payload carries a non-empty subagent marker — i.e.
/// the event originates from a spawned subagent session. The ingest router
/// consults this to optionally drop subagent captures (the
/// `drop_subagent_captures` setting). Only top-level string keys are inspected.
pub(crate) fn body_is_subagent(raw: &serde_json::Value) -> bool {
    SUBAGENT_MARKER_KEYS.iter().any(|key| {
        raw.get(*key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

/// Truthy interpretation of a query-string flag (`1`/`true`/`yes`/`on`,
/// case-insensitive). Used for the per-project `drop_subagent` opt-in and
/// the `[briefing]` session-start opt-in the host-side hook forwards from a
/// project's `.engram.toml`.
pub(crate) fn query_flag_truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// How the hook router derives a project name when no explicit
/// `project` override is present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectStrategy {
    /// Preserve v1 behavior: `project = basename(cwd)`.
    #[default]
    Basename,
    /// Opt-in marker behavior: `project = basename(main git repo root)`.
    RepoRoot,
}

impl ProjectStrategy {
    /// Parse a query-string marker value. Unknown values are ignored so a
    /// typo cannot route sessions into surprising new buckets.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("repo-root" | "repo_root") => Self::RepoRoot,
            _ => Self::Basename,
        }
    }

    /// Stable cache-key representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Basename => "basename",
            Self::RepoRoot => "repo-root",
        }
    }
}

/// Discriminator for the lifecycle event that triggered the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    /// New session started (capture cwd + model).
    SessionStart,
    /// User submitted a prompt.
    UserPrompt,
    /// Agent is about to call a tool.
    PreToolUse,
    /// Agent finished a tool call.
    PostToolUse,
    /// Compaction event (context window pressure).
    PreCompact,
    /// Agent emitted a notification.
    Notification,
    /// Agent finished its turn (interactive `/stop` or natural end).
    Stop,
    /// Session ended (final).
    SessionEnd,
    /// A subagent (nested/spawned child session) started.
    SubagentStart,
    /// A subagent finished.
    SubagentStop,
    /// Anything else.
    Other,
}

impl HookEvent {
    /// Parse a kebab- or snake-case event identifier into [`HookEvent`].
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "session-start" | "session_start" | "SessionStart" | "sessionStart" => {
                Self::SessionStart
            }
            "user-prompt" | "user_prompt" | "user-prompt-submit" | "user_prompt_submit"
            | "UserPromptSubmit" | "beforeSubmitPrompt" => Self::UserPrompt,
            "pre-tool-use" | "pre_tool_use" | "PreToolUse" | "preToolUse" | "BeforeTool" => {
                Self::PreToolUse
            }
            "post-tool-use" | "post_tool_use" | "PostToolUse" | "postToolUse"
            | "postToolUseFailure" | "PostToolUseFailure" | "AfterTool" => Self::PostToolUse,
            "pre-compact" | "pre_compact" | "PreCompact" | "preCompact" | "PreCompress" => {
                Self::PreCompact
            }
            "notification" | "Notification" => Self::Notification,
            "stop" | "Stop" => Self::Stop,
            "session-end" | "session_end" | "SessionEnd" | "sessionEnd" => Self::SessionEnd,
            "subagent-start" | "subagent_start" | "SubagentStart" | "subagentStart" => {
                Self::SubagentStart
            }
            "subagent-stop" | "subagent_stop" | "SubagentStop" | "subagentStop"
            | "subagent-end" | "SubagentEnd" => Self::SubagentStop,
            _ => Self::Other,
        }
    }

    /// Map to the storage-level [`ObservationKind`].
    #[must_use]
    pub const fn to_observation_kind(self) -> ObservationKind {
        match self {
            Self::SessionStart => ObservationKind::SessionStart,
            Self::UserPrompt => ObservationKind::UserPrompt,
            Self::PreToolUse => ObservationKind::PreToolUse,
            Self::PostToolUse => ObservationKind::PostToolUse,
            Self::PreCompact => ObservationKind::PreCompact,
            Self::Notification => ObservationKind::Notification,
            Self::Stop => ObservationKind::Stop,
            Self::SessionEnd => ObservationKind::SessionEnd,
            // Subagent lifecycle events are normally dropped (drop_subagent_captures);
            // bucket as Other for the flag-off path rather than growing ObservationKind.
            Self::SubagentStart | Self::SubagentStop => ObservationKind::Other,
            Self::Other => ObservationKind::Other,
        }
    }
}

/// Parse an agent identifier into [`AgentKind`]. Unknown values map to
/// [`AgentKind::Other`].
#[must_use]
pub fn parse_agent(s: &str) -> AgentKind {
    AgentKind::from_wire(s)
}

impl HookEnvelope {
    /// Build an envelope from the parsed query + the body JSON. Performs
    /// best-effort extraction of `session_id` / `cwd` / a body excerpt
    /// from common shapes used by Claude Code, Codex, and OpenCode hook
    /// payloads.
    #[must_use]
    pub fn from_query_and_body(query: HookQuery, raw: serde_json::Value) -> Self {
        let event = HookEvent::parse(&query.event);
        let agent = query.agent.as_deref().map_or(AgentKind::Other, parse_agent);
        // OpenCode's plugin SDK sends `sessionID` (capital `ID`) on the
        // tool.execute.*/session.* events; Claude Code uses `session_id`,
        // Codex `sessionId`, and Antigravity CLI uses `conversationId`.
        // JSON keys are case-sensitive, so all spellings must be listed
        // or tool events fail the router's "missing session_id" check.
        let session_id = extract_string(
            &raw,
            &[
                "session_id",
                "sessionId",
                "sessionID",
                "session",
                "conversationId",
            ],
        )
        .or_else(|| {
            extract_string_path(
                &raw,
                &[
                    &["info", "id"],
                    &["properties", "sessionID"],
                    &["properties", "info", "id"],
                    &["event", "properties", "sessionID"],
                    &["event", "properties", "info", "id"],
                    &["payload", "info", "id"],
                    &["payload", "properties", "sessionID"],
                    &["payload", "properties", "info", "id"],
                ],
            )
        });
        let body_cwd = extract_string(&raw, &["cwd", "current_dir", "working_dir", "directory"])
            .or_else(|| extract_first_string_array_item(&raw, &["workspacePaths"]))
            .or_else(|| {
                extract_string_path(
                    &raw,
                    &[
                        &["path", "cwd"],
                        &["info", "directory"],
                        &["properties", "info", "directory"],
                        &["event", "properties", "info", "directory"],
                        &["payload", "path", "cwd"],
                        &["payload", "info", "directory"],
                        &["payload", "properties", "info", "directory"],
                    ],
                )
            });
        // Body cwd wins over the query-string fallback: the body is
        // what agent CLIs natively send, so any query-string `cwd` is
        // a bridge / test override that should defer to live data.
        let cwd = body_cwd.or_else(|| query.cwd.filter(|s| !s.is_empty()));
        let workspace_override = query.workspace.filter(|s| !s.is_empty());
        let project_override = query.project.filter(|s| !s.is_empty());
        let project_strategy = ProjectStrategy::parse(query.project_strategy.as_deref());
        let drop_subagent_requested = query_flag_truthy(query.drop_subagent.as_deref());
        let extension = normalize_extension_name(query.extension.as_deref());
        let source_event = extension.as_ref().and_then(|_| {
            let raw_source = query
                .source_event
                .as_deref()
                .or_else(|| (event == HookEvent::Other).then_some(query.event.as_str()))?;
            normalize_source_event(raw_source)
        });
        let extension = if source_event.is_some() {
            extension
        } else {
            None
        };
        let title_hint = best_title_hint(event, &raw).or_else(|| {
            source_event
                .as_deref()
                .map(|source| extension_title_hint(&raw, source))
        });
        let body_excerpt = best_body_excerpt(event, &raw).or_else(|| {
            source_event
                .as_deref()
                .and_then(|_| extension_body_excerpt(&raw))
        });
        Self {
            event,
            agent,
            session_id,
            cwd,
            workspace_override,
            project_override,
            project_strategy,
            drop_subagent_requested,
            extension,
            source_event,
            title_hint,
            body_excerpt,
            raw,
        }
    }
}

fn extract_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        for candidate in extraction_candidates(value) {
            if let Some(s) = candidate.get(*key).and_then(serde_json::Value::as_str)
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn extract_string_path(value: &serde_json::Value, paths: &[&[&str]]) -> Option<String> {
    for path in paths {
        if let Some(s) = value_at_path(value, path).and_then(serde_json::Value::as_str)
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

fn extract_scalar_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        for candidate in extraction_candidates(value) {
            if let Some(value) = candidate.get(*key) {
                if let Some(s) = value.as_str()
                    && !s.is_empty()
                {
                    return Some(s.to_string());
                }
                if let Some(n) = value.as_i64() {
                    return Some(n.to_string());
                }
                if let Some(n) = value.as_u64() {
                    return Some(n.to_string());
                }
            }
        }
    }
    None
}

fn extract_first_string_array_item(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        for candidate in extraction_candidates(value) {
            if let Some(items) = candidate.get(*key).and_then(serde_json::Value::as_array) {
                for item in items {
                    if let Some(s) = item.as_str()
                        && !s.is_empty()
                    {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    None
}

fn value_at_path<'a>(
    mut value: &'a serde_json::Value,
    path: &[&str],
) -> Option<&'a serde_json::Value> {
    for segment in path {
        value = value.get(*segment)?;
    }
    Some(value)
}

fn extraction_candidates(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    let mut out = Vec::new();
    push_candidates(&mut out, value);
    if let Some(payload) = value.get("payload") {
        push_candidates(&mut out, payload);
    }
    if let Some(event) = value.get("event") {
        push_candidates(&mut out, event);
    }
    out
}

fn push_candidates<'a>(out: &mut Vec<&'a serde_json::Value>, value: &'a serde_json::Value) {
    out.push(value);
    if let Some(properties) = value.get("properties") {
        out.push(properties);
        if let Some(info) = properties.get("info") {
            out.push(info);
        }
    }
    if let Some(info) = value.get("info") {
        out.push(info);
    }
    if let Some(path) = value.get("path") {
        out.push(path);
    }
}

fn best_title_hint(event: HookEvent, raw: &serde_json::Value) -> Option<String> {
    match event {
        HookEvent::SessionStart => extract_string(raw, &["model", "title"]),
        HookEvent::UserPrompt => {
            extract_string(raw, &["prompt", "message", "text"]).map(|s| truncate_for_title(&s))
        }
        HookEvent::PreToolUse | HookEvent::PostToolUse => {
            extract_string(raw, &["tool", "tool_name", "name"])
                .or_else(|| extract_string_path(raw, &[&["toolCall", "name"]]))
                .or_else(|| {
                    extract_scalar_string(raw, &["stepIdx"]).map(|step| format!("step {step}"))
                })
        }
        HookEvent::Notification => extract_string(raw, &["message", "text"]),
        _ => None,
    }
}

fn extension_title_hint(raw: &serde_json::Value, source_event: &str) -> String {
    extract_string(raw, &["title", "summary", "subject", "name"])
        .map(|s| truncate_for_title(&s))
        .unwrap_or_else(|| source_event.to_string())
}

fn extension_body_excerpt(raw: &serde_json::Value) -> Option<String> {
    extract_string(
        raw,
        &[
            "body",
            "message",
            "text",
            "description",
            "summary",
            "details",
        ],
    )
    .map(|s| truncate_excerpt(&s))
}

/// Extract human-readable text content for an observation body, accepting the
/// shapes agents actually send: a plain string, an **array of content blocks**
/// (`[{ "type": "text", "text": "…" }]` — the shape Claude Code uses for
/// `tool_response`), or a structured object (rendered as compact JSON). Unlike
/// [`extract_string`], which only matches a JSON string and silently drops
/// everything else, this keeps tool outputs / inputs that arrive as
/// arrays/objects.
fn extract_content(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        for candidate in extraction_candidates(value) {
            if let Some(found) = candidate.get(*key).and_then(value_to_text)
                && !found.is_empty()
            {
                return Some(found);
            }
        }
    }
    None
}

/// Flatten a JSON value into text. Strings pass through; arrays concatenate
/// their flattened items (one per line); objects prefer a `text` / `content`
/// field and otherwise fall back to compact JSON. `null` yields `None`.
fn value_to_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        serde_json::Value::Array(items) => {
            let joined = items
                .iter()
                .filter_map(value_to_text)
                .collect::<Vec<_>>()
                .join("\n");
            (!joined.is_empty()).then_some(joined)
        }
        serde_json::Value::Object(_) => value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .or_else(|| value.get("content").and_then(value_to_text))
            .or_else(|| {
                serde_json::to_string(value)
                    .ok()
                    .filter(|s| s != "{}" && s != "null")
            }),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => None,
    }
}

fn best_body_excerpt(event: HookEvent, raw: &serde_json::Value) -> Option<String> {
    match event {
        HookEvent::UserPrompt => extract_content(raw, &["prompt", "message", "text"]),
        HookEvent::PostToolUse => {
            let tool = extract_string(raw, &["tool", "tool_name", "name"])
                .or_else(|| extract_string_path(raw, &[&["toolCall", "name"]]))
                .or_else(|| {
                    extract_scalar_string(raw, &["stepIdx"]).map(|step| format!("step {step}"))
                })?;
            let result =
                extract_content(raw, &["tool_response", "tool_output", "output", "result"])
                    .or_else(|| extract_content(raw, &["error"]))
                    .unwrap_or_else(|| "(no output captured)".into());
            Some(format!("tool: {tool}\n---\n{}", truncate_excerpt(&result)))
        }
        HookEvent::Notification => extract_content(raw, &["message", "text"]),
        _ => None,
    }
}

fn truncate_for_title(s: &str) -> String {
    const MAX: usize = 80;
    let one_line: String = s.chars().take_while(|c| *c != '\n').collect();
    if one_line.chars().count() <= MAX {
        one_line
    } else {
        let mut buf: String = one_line.chars().take(MAX - 1).collect();
        buf.push('…');
        buf
    }
}

fn truncate_excerpt(s: &str) -> String {
    const MAX: usize = 2_000;
    if s.len() <= MAX {
        s.to_string()
    } else {
        let mut buf = String::with_capacity(MAX + '…'.len_utf8());
        let mut end = 0;
        for (idx, ch) in s.char_indices() {
            let next = idx + ch.len_utf8();
            if next > MAX {
                break;
            }
            end = next;
        }
        buf.push_str(&s[..end]);
        buf.push('…');
        buf
    }
}

fn normalize_extension_name(value: Option<&str>) -> Option<String> {
    normalize_token(value?, 64)
}

fn normalize_source_event(value: &str) -> Option<String> {
    normalize_token(value, 128)
}

fn normalize_token(value: &str, max_len: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > max_len {
        return None;
    }
    if trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_subagent_detects_harness_markers() {
        // grok tags subagent tool-use events with `subagentType`.
        assert!(body_is_subagent(
            &serde_json::json!({ "sessionId": "s", "subagentType": "general-purpose" })
        ));
        // Claude Code tags its subagent events with `agent_type` / `agent_id`.
        assert!(body_is_subagent(
            &serde_json::json!({ "session_id": "s", "agent_type": "workflow-subagent" })
        ));
        assert!(body_is_subagent(
            &serde_json::json!({ "agent_id": "agent-abc123" })
        ));
    }

    #[test]
    fn body_is_subagent_false_for_top_level_and_empty_markers() {
        // A normal top-level event carries no marker.
        assert!(!body_is_subagent(
            &serde_json::json!({ "session_id": "s", "tool_name": "Write" })
        ));
        // An empty / blank or non-string marker does not count as a subagent.
        assert!(!body_is_subagent(
            &serde_json::json!({ "subagentType": "" })
        ));
        assert!(!body_is_subagent(
            &serde_json::json!({ "subagentType": "   " })
        ));
        assert!(!body_is_subagent(
            &serde_json::json!({ "agent_type": null })
        ));
        assert!(!body_is_subagent(&serde_json::json!({})));
    }

    #[test]
    fn parses_known_events() {
        assert_eq!(HookEvent::parse("session-start"), HookEvent::SessionStart);
        assert_eq!(HookEvent::parse("PreToolUse"), HookEvent::PreToolUse);
        assert_eq!(HookEvent::parse("user_prompt"), HookEvent::UserPrompt);
        assert_eq!(HookEvent::parse("bogus"), HookEvent::Other);
    }

    #[test]
    fn extension_event_preserves_source_event_when_opted_in() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                extension: Some("fstech".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "fst-1",
                "title": "Lead contacted",
                "message": "Lead Maria requested a proposal"
            }),
        );

        assert_eq!(env.event, HookEvent::Other);
        assert_eq!(env.extension.as_deref(), Some("fstech"));
        assert_eq!(env.source_event.as_deref(), Some("lead.contact"));
        assert_eq!(env.title_hint.as_deref(), Some("Lead contacted"));
        assert_eq!(
            env.body_excerpt.as_deref(),
            Some("Lead Maria requested a proposal")
        );
    }

    #[test]
    fn unknown_event_without_extension_leaves_no_source_event() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "fst-1",
                "title": "Lead contacted",
                "message": "Lead Maria requested a proposal"
            }),
        );

        assert_eq!(env.event, HookEvent::Other);
        assert_eq!(env.extension, None);
        assert_eq!(env.source_event, None);
        assert_eq!(env.title_hint, None);
        assert_eq!(env.body_excerpt, None);
    }

    #[test]
    fn invalid_extension_tokens_are_not_preserved() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                extension: Some("bad extension".into()),
                ..Default::default()
            },
            serde_json::json!({ "session_id": "fst-1" }),
        );

        assert_eq!(env.extension, None);
        assert_eq!(env.source_event, None);
    }

    #[test]
    fn maps_to_observation_kind() {
        assert_eq!(
            HookEvent::SessionEnd.to_observation_kind(),
            ObservationKind::SessionEnd
        );
    }

    #[test]
    fn envelope_extracts_session_and_cwd() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "session_id": "abc-123",
            "cwd": "/tmp/x",
            "model": "claude-sonnet-4-6"
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.event, HookEvent::SessionStart);
        assert_eq!(env.session_id.as_deref(), Some("abc-123"));
        assert_eq!(env.cwd.as_deref(), Some("/tmp/x"));
        assert_eq!(env.title_hint.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn envelope_parses_project_strategy_query() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "session-start".into(),
                project_strategy: Some("repo-root".into()),
                ..Default::default()
            },
            serde_json::json!({}),
        );

        assert_eq!(env.project_strategy, ProjectStrategy::RepoRoot);
    }

    /// Antigravity CLI identifies the conversation as `conversationId`
    /// and reports cwd-like routing through `workspacePaths`.
    #[test]
    fn envelope_extracts_antigravity_conversation_and_workspace_path() {
        let q = HookQuery {
            event: "PreToolUse".into(),
            agent: Some("agy".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "workspacePaths": ["/workspace/project", "/workspace/other"],
            "toolCall": {
                "name": "run_command",
                "args": {"CommandLine": "cargo test"}
            },
            "stepIdx": 3
        });
        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.agent, AgentKind::AntigravityCli);
        assert_eq!(env.event, HookEvent::PreToolUse);
        assert_eq!(
            env.session_id.as_deref(),
            Some("ec33ebf9-0cba-4100-8142-c61503f6c587")
        );
        assert_eq!(env.cwd.as_deref(), Some("/workspace/project"));
        assert_eq!(env.title_hint.as_deref(), Some("run_command"));
    }

    #[test]
    fn envelope_uses_antigravity_step_idx_for_post_tool_title_fallback() {
        let q = HookQuery {
            event: "PostToolUse".into(),
            agent: Some("antigravity-cli".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "conversationId": "agy-conv",
            "workspacePaths": ["/workspace/project"],
            "stepIdx": 5,
            "error": "exit status 1"
        });
        let env = HookEnvelope::from_query_and_body(q, raw);

        assert_eq!(env.title_hint.as_deref(), Some("step 5"));
        assert!(
            env.body_excerpt
                .as_deref()
                .is_some_and(|body| body.contains("exit status 1"))
        );
    }

    /// OpenCode's plugin SDK sends `sessionID` (capital `ID`) on the
    /// tool.execute.* / session.* events. Regression for issue #1: this
    /// spelling must be extracted, otherwise non-session-start events
    /// fail the router's "missing session_id" check.
    #[test]
    fn envelope_extracts_opencode_camelcase_session_id() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("open-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "sessionID": "ses_abc123",
            "tool": "bash",
            "callID": "call_1"
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.session_id.as_deref(), Some("ses_abc123"));
    }

    /// Earlier OpenCode plugin generation wrapped the actual SDK hook
    /// input under `payload`. Keep accepting that shape so users with
    /// an old plugin don't silently lose project routing until they
    /// restart with the fixed plugin.
    #[test]
    fn envelope_extracts_legacy_opencode_nested_payload() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("open-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "hook_event_name": "post-tool-use",
            "agent": "open-code",
            "payload": {
                "sessionID": "ses_nested",
                "cwd": "/home/user/engram",
                "tool": "bash",
                "output": "tests passed"
            }
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.session_id.as_deref(), Some("ses_nested"));
        assert_eq!(env.cwd.as_deref(), Some("/home/user/engram"));
        assert_eq!(env.title_hint.as_deref(), Some("bash"));
        assert!(
            env.body_excerpt
                .as_deref()
                .is_some_and(|body| body.contains("tests passed")),
            "post-tool body should include nested output: {:?}",
            env.body_excerpt
        );
    }

    /// OpenCode's plugin `event` hook receives bus events shaped like
    /// `{ event: { type, properties } }`; session creation carries the
    /// cwd as `properties.info.directory`.
    #[test]
    fn envelope_extracts_opencode_bus_event_session_info() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("open-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({
            "event": {
                "type": "session.created",
                "properties": {
                    "sessionID": "ses_bus",
                    "info": {
                        "id": "ses_bus",
                        "directory": "/home/user/engram",
                        "title": "New session"
                    }
                }
            }
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.session_id.as_deref(), Some("ses_bus"));
        assert_eq!(env.cwd.as_deref(), Some("/home/user/engram"));
        assert_eq!(env.title_hint.as_deref(), Some("New session"));
    }

    /// Alternative agent-name spellings all map to the same canonical
    /// AgentKind. The hook scripts and the test e2e shim send slightly
    /// different strings for historical reasons; this asserts we
    /// remain forgiving.
    #[test]
    fn agent_name_aliases_all_map_correctly() {
        assert_eq!(parse_agent("claude-code"), AgentKind::ClaudeCode);
        assert_eq!(parse_agent("claude_code"), AgentKind::ClaudeCode);
        assert_eq!(parse_agent("claude"), AgentKind::ClaudeCode);
        assert_eq!(parse_agent("codex"), AgentKind::Codex);
        assert_eq!(parse_agent("opencode"), AgentKind::OpenCode);
        assert_eq!(parse_agent("open-code"), AgentKind::OpenCode);
        assert_eq!(parse_agent("cursor"), AgentKind::Cursor);
        assert_eq!(parse_agent("gemini-cli"), AgentKind::GeminiCli);
        assert_eq!(parse_agent("gemini"), AgentKind::GeminiCli);
        assert_eq!(parse_agent("claude-desktop"), AgentKind::ClaudeDesktop);
        assert_eq!(parse_agent("openclaw"), AgentKind::OpenClaw);
        assert_eq!(parse_agent("antigravity-cli"), AgentKind::AntigravityCli);
        assert_eq!(parse_agent("antigravity"), AgentKind::AntigravityCli);
        assert_eq!(parse_agent("agy"), AgentKind::AntigravityCli);
        assert_eq!(parse_agent("omp"), AgentKind::Omp);
        assert_eq!(parse_agent("pi"), AgentKind::Pi);
        assert_eq!(parse_agent("oh-my-pi"), AgentKind::Omp);
        // Anything else is `Other`. Critical for the hook router:
        // a typo in the query string must not crash, it just gets
        // attributed to the catch-all bucket.
        assert_eq!(parse_agent(""), AgentKind::Other);
        assert_eq!(parse_agent("CLAUDE-CODE"), AgentKind::Other); // case-sensitive on purpose
        assert_eq!(parse_agent("../../etc/passwd"), AgentKind::Other);
    }

    /// An empty body is legitimate (some hook events carry no
    /// payload). Envelope extraction must produce sane defaults
    /// rather than panicking.
    #[test]
    fn envelope_tolerates_empty_body() {
        let q = HookQuery {
            event: "stop".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(q, serde_json::json!({}));
        assert_eq!(env.event, HookEvent::Stop);
        assert!(env.session_id.is_none());
        assert!(env.cwd.is_none());
        assert!(env.title_hint.is_none());
        assert!(env.body_excerpt.is_none());
    }

    /// Body is well-formed JSON but the expected `session_id` /
    /// `cwd` keys are missing — extraction returns None per key.
    #[test]
    fn envelope_missing_expected_fields() {
        let q = HookQuery {
            event: "user-prompt".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let raw = serde_json::json!({ "garbage": 42 });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.event, HookEvent::UserPrompt);
        assert!(env.session_id.is_none());
        assert!(env.cwd.is_none());
    }

    /// Body is a JSON primitive (string / null / number) rather
    /// than an object. The extractors must short-circuit cleanly.
    /// This guards against an upstream that POSTs a stringified
    /// payload by mistake.
    #[test]
    fn envelope_accepts_non_object_body() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        for raw in [
            serde_json::json!(null),
            serde_json::json!("a stringy payload"),
            serde_json::json!(42),
            serde_json::json!([1, 2, 3]),
        ] {
            let env = HookEnvelope::from_query_and_body(q.clone(), raw);
            assert!(
                env.session_id.is_none(),
                "no session_id from non-object body"
            );
            assert!(env.cwd.is_none(), "no cwd from non-object body");
        }
    }

    /// Empty `agent` query param maps to Other (rather than panic
    /// or default to ClaudeCode). The hook router uses this for the
    /// attribution column, so we want it consistent.
    #[test]
    fn missing_agent_query_param_maps_to_other() {
        let q = HookQuery {
            event: "session-end".into(),
            agent: None,
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(q, serde_json::json!({}));
        assert_eq!(env.agent, AgentKind::Other);
    }

    /// Title-hint extraction must truncate at the first newline (the
    /// "first line" rule used everywhere in the wiki log + handoff
    /// surfaces) and cap at 80 chars to keep observation titles
    /// scannable in the log.md heading.
    #[test]
    fn user_prompt_title_truncates_at_newline_and_at_max_chars() {
        let q = HookQuery {
            event: "user-prompt".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        // Multi-line prompt → title is the first line only.
        let env = HookEnvelope::from_query_and_body(
            q.clone(),
            serde_json::json!({ "prompt": "first line\nsecond line should be lost" }),
        );
        assert_eq!(env.title_hint.as_deref(), Some("first line"));

        // Very long single line → truncated with ellipsis.
        let long = "x".repeat(200);
        let env = HookEnvelope::from_query_and_body(q, serde_json::json!({ "prompt": long }));
        let title = env.title_hint.unwrap();
        assert!(title.chars().count() <= 80);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn post_tool_excerpt_truncates_without_splitting_utf8() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let output = format!("{}é", "x".repeat(1_999));
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "tool": "bash",
                "result": output,
            }),
        );
        let excerpt = env.body_excerpt.unwrap();
        assert!(excerpt.ends_with('…'));
        assert!(excerpt.starts_with("tool: bash\n---\n"));
    }

    /// Regression: the native-binary hook command sends the script stem
    /// `user-prompt-submit` as the event token (rendered by `render_shared.rs`,
    /// forwarded verbatim by `engram hook`). The parser must map it to
    /// `UserPrompt`; otherwise native installs (the Windows / posix-native
    /// default) bucket every prompt as `Other` and drop its text.
    #[test]
    fn parses_native_user_prompt_submit_event_token() {
        assert_eq!(
            HookEvent::parse("user-prompt-submit"),
            HookEvent::UserPrompt
        );
        assert_eq!(
            HookEvent::parse("user_prompt_submit"),
            HookEvent::UserPrompt
        );
    }

    /// Claude Code sends `tool_response` as an array of content blocks
    /// (`[{ "type": "text", "text": "…" }]`). The body excerpt must capture
    /// that text instead of falling back to "(no output captured)".
    #[test]
    fn post_tool_excerpt_captures_array_tool_response() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "tool_name": "Bash",
                "tool_response": [{"type": "text", "text": "MARKER_OUTPUT_123"}],
            }),
        );
        let body = env.body_excerpt.expect("post-tool body");
        assert!(
            body.contains("MARKER_OUTPUT_123"),
            "array tool_response text should be captured: {body:?}"
        );
        assert!(
            !body.contains("(no output captured)"),
            "should not fall back when output is present: {body:?}"
        );
    }

    /// An object-shaped `tool_response` is serialized into the body rather than
    /// dropped.
    #[test]
    fn post_tool_excerpt_captures_object_tool_response() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "tool_name": "Read",
                "tool_response": {"stdout": "MARKER_OBJ_456"},
            }),
        );
        let body = env.body_excerpt.expect("post-tool body");
        assert!(
            body.contains("MARKER_OBJ_456"),
            "object tool_response should be serialized into the body: {body:?}"
        );
    }

    /// End-to-end: a native-hook user prompt (`event=user-prompt-submit`,
    /// string `prompt`) maps to `UserPrompt` and keeps its body text.
    #[test]
    fn native_user_prompt_submit_keeps_prompt_body() {
        let q = HookQuery {
            event: "user-prompt-submit".into(),
            agent: Some("claude-code".into()),
            ..Default::default()
        };
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({ "session_id": "s1", "prompt": "MARKER_PROMPT_789" }),
        );
        assert_eq!(env.event, HookEvent::UserPrompt);
        assert_eq!(env.body_excerpt.as_deref(), Some("MARKER_PROMPT_789"));
    }
}
