mod api_client;
mod commands;
mod daemon_manager;
mod types;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::list_pages,
            commands::read_page,
            commands::semantic_search,
            commands::daemon_status,
            commands::write_page,
            commands::delete_page,
            commands::admin_status,
            commands::memory_health,
            commands::run_embed,
            commands::run_sweep,
            commands::run_backup,
            commands::daemon_start,
            commands::daemon_stop,
            commands::pending_queue,
            commands::pending_detail,
            commands::pending_diff,
            commands::pending_approve,
            commands::pending_reject,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
