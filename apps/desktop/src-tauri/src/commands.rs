use tauri::Manager;

use crate::api_client::ApiClient;
use crate::types::{
    DaemonStatus, EmbedReport, Hit, MemoryHealth, PageDetail, PageSummary, WritePageArgs,
    WritePageResult,
};

#[tauri::command]
pub async fn list_pages() -> Result<Vec<PageSummary>, String> {
    ApiClient::new().list_pages().await
}

#[tauri::command]
pub async fn read_page(path: String) -> Result<PageDetail, String> {
    ApiClient::new().read_page(&path).await
}

#[tauri::command]
pub async fn semantic_search(query: String) -> Result<Vec<Hit>, String> {
    ApiClient::new().semantic_search(&query).await
}

#[tauri::command]
pub async fn daemon_status() -> DaemonStatus {
    ApiClient::new().daemon_status().await
}

#[tauri::command]
pub async fn write_page(args: WritePageArgs) -> Result<WritePageResult, String> {
    ApiClient::new().write_page(&args).await
}

#[tauri::command]
pub async fn delete_page(path: String) -> Result<(), String> {
    ApiClient::new().delete_page(&path).await
}

#[tauri::command]
pub async fn admin_status() -> Result<serde_json::Value, String> {
    ApiClient::new().admin_status().await
}

#[tauri::command]
pub async fn memory_health() -> Result<MemoryHealth, String> {
    ApiClient::new().memory_health().await
}

#[tauri::command]
pub async fn run_embed(reembed: bool, dry_run: bool) -> Result<EmbedReport, String> {
    ApiClient::new().run_embed(reembed, dry_run).await
}

#[tauri::command]
pub async fn run_sweep(dry_run: bool) -> Result<serde_json::Value, String> {
    ApiClient::new().run_sweep(dry_run).await
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

/// Aggregate pending proposals across every project (the admin surface is
/// per-project; the queue fans out and keeps only non-empty projects).
#[tauri::command]
pub async fn pending_queue() -> Result<serde_json::Value, String> {
    let c = ApiClient::new();
    let mut out = Vec::new();
    for p in c.list_projects().await? {
        let Some(name) = p["project_name"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        match c.pending_list(name).await {
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
