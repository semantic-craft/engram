//! `GET /` — project list cards.

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;

use crate::state::WebState;
use crate::templates::{ProjectCard, ProjectsView, humanize, project_href};

/// Handler for `GET /`.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
) -> Result<Html<String>, StatusCode> {
    let summaries = state
        .reader
        .list_projects_with_stats()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let projects = summaries
        .into_iter()
        .map(|s| {
            let last_updated_relative = s.last_updated.as_deref().map(humanize).unwrap_or_default();
            let href = project_href(&s.workspace_name, &s.project_name);
            ProjectCard {
                workspace: s.workspace_name,
                project: s.project_name,
                page_count: s.page_count,
                last_updated_relative,
                href,
            }
        })
        .collect();

    let html = ProjectsView { projects }
        .render()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Html(html))
}
