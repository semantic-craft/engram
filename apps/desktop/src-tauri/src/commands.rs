use tauri::Manager;
use tauri_plugin_opener::OpenerExt;

use crate::api_client::ApiClient;
use crate::types::{
    DaemonStatus, EmbedReport, Hit, InstructionFile, MemoryHealth, PageDetail, PageSummary,
    ProjectSummary, WritePageArgs, WritePageResult,
};

/// Scope a client to the requested project; `None`/empty falls back to
/// the `_global` default so pre-scope callers keep working.
fn client(project: Option<String>) -> ApiClient {
    match project.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => ApiClient::for_project(p),
        _ => ApiClient::new(),
    }
}

#[tauri::command]
pub async fn list_pages(project: Option<String>) -> Result<Vec<PageSummary>, String> {
    client(project).list_pages().await
}

#[tauri::command]
pub async fn read_page(path: String, project: Option<String>) -> Result<PageDetail, String> {
    client(project).read_page(&path).await
}

#[tauri::command]
pub async fn semantic_search(
    query: String,
    project: Option<String>,
    global: Option<bool>,
) -> Result<Vec<Hit>, String> {
    if global.unwrap_or(false) {
        ApiClient::new().semantic_search_global(&query).await
    } else {
        client(project).semantic_search(&query).await
    }
}

#[tauri::command]
pub async fn daemon_status(project: Option<String>) -> DaemonStatus {
    client(project).daemon_status().await
}

#[tauri::command]
pub async fn write_page(
    args: WritePageArgs,
    project: Option<String>,
) -> Result<WritePageResult, String> {
    client(project).write_page(&args).await
}

#[tauri::command]
pub async fn delete_page(path: String, project: Option<String>) -> Result<(), String> {
    client(project).delete_page(&path).await
}

#[tauri::command]
pub async fn admin_status() -> Result<serde_json::Value, String> {
    ApiClient::new().admin_status().await
}

#[tauri::command]
pub async fn memory_health(project: Option<String>) -> Result<MemoryHealth, String> {
    client(project).memory_health().await
}

#[tauri::command]
pub async fn run_embed(
    reembed: bool,
    dry_run: bool,
    project: Option<String>,
) -> Result<EmbedReport, String> {
    client(project).run_embed(reembed, dry_run).await
}

#[tauri::command]
pub async fn run_sweep(
    dry_run: bool,
    project: Option<String>,
) -> Result<serde_json::Value, String> {
    client(project).run_sweep(dry_run).await
}

/// Download a backup tarball into the user's download directory.
/// Returns the written file's full path.
#[tauri::command]
pub async fn run_backup(app: tauri::AppHandle, filename: String) -> Result<String, String> {
    if filename.trim().is_empty() || filename.contains('/') || filename.contains("..") {
        return Err(format!("invalid backup filename: {filename}"));
    }
    let bytes = ApiClient::new().backup().await?;
    let dir = app.path().download_dir().map_err(|e| e.to_string())?;
    let dest = dir.join(filename);
    tokio::fs::write(&dest, bytes)
        .await
        .map_err(|e| e.to_string())?;
    Ok(dest.display().to_string())
}

#[tauri::command]
pub fn daemon_start() -> Result<String, String> {
    crate::daemon_manager::daemon_start()
}

#[tauri::command]
pub fn daemon_stop() -> Result<String, String> {
    crate::daemon_manager::daemon_stop()
}

/// Project inventory with stats for the switcher and the overview grid.
#[tauri::command]
pub async fn list_projects_stats() -> Result<Vec<ProjectSummary>, String> {
    ApiClient::new().list_projects().await
}

/// `{handoff, briefing, health}` snapshot for one project's dashboard.
#[tauri::command]
pub async fn project_overview(project: String) -> Result<serde_json::Value, String> {
    ApiClient::for_project(&project).project_overview(50).await
}

/// Read machine-local instruction files (`~/…` or absolute paths).
#[tauri::command]
pub fn read_instruction_files(paths: Vec<String>) -> Result<Vec<InstructionFile>, String> {
    paths
        .iter()
        .map(|p| {
            let abs = crate::instructions::expand_home(p)?;
            Ok(crate::instructions::inspect(p, &abs))
        })
        .collect()
}

/// Inspect the CLAUDE.md / .claude/CLAUDE.md / AGENTS.md chain under a
/// project root on this machine.
#[tauri::command]
pub fn discover_project_instructions(root: String) -> Result<Vec<InstructionFile>, String> {
    let root = crate::instructions::expand_home(&root)?;
    Ok(crate::instructions::discover(&root))
}

/// Open a local file with the system default handler (read-only UI's
/// escape hatch for editing instruction files).
#[tauri::command]
pub fn open_in_editor(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let abs = crate::instructions::expand_home(&path)?;
    app.opener()
        .open_path(abs.display().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Aggregate pending proposals across every project (the admin surface is
/// per-project; the queue fans out and keeps only non-empty projects).
#[tauri::command]
pub async fn pending_queue() -> Result<serde_json::Value, String> {
    let c = ApiClient::new();
    let mut out = Vec::new();
    for p in c.list_projects().await? {
        let name = p.project_name;
        if name.is_empty() {
            continue;
        }
        match c.pending_list(&name).await {
            Ok(list) => {
                if list.as_array().is_some_and(|a| !a.is_empty()) {
                    out.push(serde_json::json!({ "project": name, "proposals": list }));
                }
            }
            Err(e) => return Err(format!("{name}: {e}")),
        }
    }
    Ok(serde_json::json!(out))
}

#[tauri::command]
pub async fn pending_detail(project: String, id: String) -> Result<serde_json::Value, String> {
    ApiClient::new().pending_detail(&project, &id).await
}

#[tauri::command]
pub async fn pending_diff(project: String, id: String) -> Result<serde_json::Value, String> {
    ApiClient::new().pending_diff(&project, &id).await
}

#[tauri::command]
pub async fn pending_approve(project: String, id: String) -> Result<serde_json::Value, String> {
    ApiClient::new().pending_approve(&project, &id).await
}

#[tauri::command]
pub async fn pending_reject(
    project: String,
    id: String,
    reason: String,
) -> Result<serde_json::Value, String> {
    ApiClient::new()
        .pending_reject(&project, &id, &reason)
        .await
}
