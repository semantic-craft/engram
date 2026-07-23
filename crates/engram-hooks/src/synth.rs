//! Rule-based session-page synthesis (no LLM).
//!
//! At `SessionEnd` we already have N observation rows for the session.
//! We turn them into a single markdown page under `wiki/sessions/<id>.md`
//! using only deterministic heuristics: first-prompt as title, files
//! touched, tool-call counts. Once the LLM provider lands in M6 we'll
//! add an opt-in path that re-narrates the page.

use std::collections::BTreeMap;

use engram_core::{
    NewPage, Observation, ObservationKind, PagePath, ProjectId, SessionId, Tier, WorkspaceId,
};
use jiff::tz::TimeZone;

const RAW_OBSERVATION_MAX_LINES: usize = 500;
const RAW_OBSERVATION_HEAD_LINES: usize = 250;
const RAW_OBSERVATION_TAIL_LINES: usize = RAW_OBSERVATION_MAX_LINES - RAW_OBSERVATION_HEAD_LINES;

/// Build a [`NewPage`] from the observations collected during a session.
///
/// The returned page is *always* under `sessions/<session-id>.md` so each
/// session has a stable URL the user can bookmark.
#[must_use]
pub fn synthesize_session_page(
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    observations: &[Observation],
) -> NewPage {
    let title = derive_title(observations);
    let body = render_body(session_id, observations, &title);
    let path = PagePath::new(format!("sessions/{session_id}.md"))
        .expect("hard-coded sessions/<uuid>.md is always valid");
    NewPage {
        workspace_id,
        project_id,
        path,
        title: title.clone(),
        body,
        tier: Tier::Episodic,
        frontmatter_json: serde_json::json!({
            "title": title,
            "session_id": session_id.to_string(),
            "tier": "episodic",
        }),
        pinned: false,
        links: Vec::new(),
        author_id: None,
    }
}

fn derive_title(observations: &[Observation]) -> String {
    for obs in observations {
        if obs.kind == ObservationKind::UserPrompt && !obs.title.is_empty() {
            return obs.title.clone();
        }
    }
    for obs in observations {
        if !obs.title.is_empty() {
            return obs.title.clone();
        }
    }
    "session".to_string()
}

fn render_body(session_id: SessionId, observations: &[Observation], title: &str) -> String {
    let mut tool_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut prompts: Vec<&Observation> = Vec::new();
    let mut start: Option<&Observation> = None;
    let mut end: Option<&Observation> = None;

    for obs in observations {
        match obs.kind {
            ObservationKind::SessionStart => start = Some(obs),
            ObservationKind::SessionEnd => end = Some(obs),
            ObservationKind::UserPrompt => prompts.push(obs),
            // Count only PostToolUse — each tool call produces both a
            // PreToolUse and a PostToolUse observation, so counting both
            // doubles every reported number ("Bash: 4" for two real calls).
            // PostToolUse is the "completed call" event; pre-only calls
            // that never produced a post (cancellations) are intentionally
            // excluded.
            ObservationKind::PostToolUse if !obs.title.is_empty() => {
                *tool_counts.entry(obs.title.as_str()).or_insert(0) += 1;
            }
            _ => {}
        }
    }

    let mut buf = String::with_capacity(2048);
    buf.push_str(&format!("# {title}\n\n"));

    buf.push_str("## Session metadata\n\n");
    buf.push_str(&format!("- **session_id:** `{session_id}`\n"));
    if let Some(s) = start {
        buf.push_str(&format!("- **started_at:** {}\n", human_ts(&s.created_at),));
    }
    if let Some(e) = end {
        buf.push_str(&format!("- **ended_at:** {}\n", human_ts(&e.created_at),));
    }
    buf.push_str(&format!("- **observations:** {}\n\n", observations.len()));

    if !prompts.is_empty() {
        buf.push_str("## Prompts\n\n");
        for (i, p) in prompts.iter().enumerate() {
            buf.push_str(&format!("{}. {}\n", i + 1, p.title));
        }
        buf.push('\n');
    }

    if !tool_counts.is_empty() {
        buf.push_str("## Tool calls\n\n");
        for (name, count) in &tool_counts {
            buf.push_str(&format!("- `{name}`: {count}\n"));
        }
        buf.push('\n');
    }

    buf.push_str("## Raw observations\n\n");
    render_raw_observations(&mut buf, observations);

    buf.push_str("\n_Synthesised by engram (M3, no-LLM heuristic)._\n");
    buf
}

fn render_raw_observations(buf: &mut String, observations: &[Observation]) {
    if observations.len() <= RAW_OBSERVATION_MAX_LINES {
        for obs in observations {
            render_raw_observation(buf, obs);
        }
        return;
    }

    for obs in &observations[..RAW_OBSERVATION_HEAD_LINES] {
        render_raw_observation(buf, obs);
    }
    let omitted = observations.len() - RAW_OBSERVATION_HEAD_LINES - RAW_OBSERVATION_TAIL_LINES;
    buf.push_str(&format!(
        "\n_... {omitted} raw observations omitted from the middle (showing first {RAW_OBSERVATION_HEAD_LINES} and last {RAW_OBSERVATION_TAIL_LINES})._\n\n",
    ));
    for obs in &observations[observations.len() - RAW_OBSERVATION_TAIL_LINES..] {
        render_raw_observation(buf, obs);
    }
}

fn render_raw_observation(buf: &mut String, obs: &Observation) {
    let kind = observation_kind_label(obs);
    buf.push_str(&format!(
        "- `{}` @ {} — {}\n",
        kind,
        human_ts(&obs.created_at),
        obs.title.chars().take(80).collect::<String>(),
    ));
}

fn observation_kind_label(obs: &Observation) -> String {
    match (&obs.extension, &obs.source_event) {
        (Some(extension), Some(source_event)) => {
            format!("{} [{}:{}]", obs.kind.as_str(), extension, source_event)
        }
        _ => obs.kind.as_str().to_string(),
    }
}

fn human_ts(ts: &jiff::Timestamp) -> String {
    ts.to_zoned(TimeZone::UTC)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::{ObservationId, SessionId};
    use jiff::Timestamp;

    fn obs(kind: ObservationKind, title: &str) -> Observation {
        Observation {
            id: ObservationId::new(),
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            kind,
            extension: None,
            source_event: None,
            title: title.into(),
            body: String::new(),
            importance: 5,
            created_at: Timestamp::now(),
        }
    }

    #[test]
    fn title_falls_back_through_kinds() {
        let no_prompt = vec![obs(ObservationKind::PostToolUse, "Edit")];
        assert_eq!(derive_title(&no_prompt), "Edit");

        let empty: Vec<Observation> = vec![];
        assert_eq!(derive_title(&empty), "session");

        let with_prompt = vec![
            obs(ObservationKind::PostToolUse, "Edit"),
            obs(ObservationKind::UserPrompt, "fix the auth bug"),
        ];
        assert_eq!(derive_title(&with_prompt), "fix the auth bug");
    }

    #[test]
    fn body_includes_tool_counts_and_prompts() {
        // Each real tool call produces a Pre+Post pair. The render must
        // report one entry per call (not one per observation), so two
        // Edit calls = 2 (not 4) and one Bash call = 1 (not 2).
        let observations = vec![
            obs(ObservationKind::SessionStart, "session"),
            obs(ObservationKind::UserPrompt, "build the thing"),
            obs(ObservationKind::PreToolUse, "Edit"),
            obs(ObservationKind::PostToolUse, "Edit"),
            obs(ObservationKind::PreToolUse, "Edit"),
            obs(ObservationKind::PostToolUse, "Edit"),
            obs(ObservationKind::PreToolUse, "Bash"),
            obs(ObservationKind::PostToolUse, "Bash"),
            obs(ObservationKind::SessionEnd, "session"),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            &observations,
        );
        assert!(page.title.contains("build the thing"));
        assert!(page.body.contains("`Edit`: 2"));
        assert!(page.body.contains("`Bash`: 1"));
        assert!(page.body.contains("build the thing"));
    }

    #[test]
    fn pre_only_tool_calls_are_not_counted() {
        // A PreToolUse without a matching PostToolUse (cancelled / crashed
        // mid-call) intentionally drops out of the count rather than
        // inflating it.
        let observations = vec![
            obs(ObservationKind::PreToolUse, "Bash"),
            obs(ObservationKind::PreToolUse, "Bash"),
            obs(ObservationKind::PostToolUse, "Bash"),
        ];
        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            &observations,
        );
        assert!(page.body.contains("`Bash`: 1"));
    }

    #[test]
    fn body_includes_opt_in_extension_source_event() {
        let mut custom = obs(ObservationKind::Other, "Lead contacted");
        custom.extension = Some("fstech".into());
        custom.source_event = Some("lead.contact".into());

        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            &[custom],
        );

        assert!(page.body.contains("`other [fstech:lead.contact]`"));
    }

    #[test]
    fn raw_observations_small_session_includes_all_entries() {
        let observations: Vec<Observation> = (0..5)
            .map(|i| obs(ObservationKind::Other, &format!("entry-{i}")))
            .collect();

        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            &observations,
        );

        for i in 0..5 {
            assert!(page.body.contains(&format!("entry-{i}")));
        }
        assert!(!page.body.contains("raw observations omitted"));
    }

    #[test]
    fn raw_observations_large_session_omits_middle_with_count() {
        let observations: Vec<Observation> = (0..600)
            .map(|i| obs(ObservationKind::Other, &format!("entry-{i}")))
            .collect();

        let page = synthesize_session_page(
            WorkspaceId::new(),
            ProjectId::new(),
            SessionId::new(),
            &observations,
        );

        assert!(page.body.contains("entry-0"));
        assert!(page.body.contains("entry-249"));
        assert!(!page.body.contains("entry-250"));
        assert!(!page.body.contains("entry-349"));
        assert!(page.body.contains("entry-350"));
        assert!(page.body.contains("entry-599"));
        assert!(
            page.body
                .contains("100 raw observations omitted from the middle")
        );
    }
}
