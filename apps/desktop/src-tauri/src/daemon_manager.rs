//! Local daemon process control (Phase 2: macOS launchd only).
//!
//! HTTP-based admin operations (embed/sweep/backup/status) live on
//! `ApiClient`; this module only starts/stops the daemon itself.

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;
    use std::process::Command;

    /// Both the engram-branded label and the pre-rename ai-memory label:
    /// machines migrate at their own pace.
    const PLIST_CANDIDATES: [&str; 2] = [
        "com.semantic-craft.engram.plist",
        "com.semantic-craft.ai-memory.plist",
    ];

    fn launch_agent() -> Result<PathBuf, String> {
        let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
        let dir = PathBuf::from(home).join("Library/LaunchAgents");
        PLIST_CANDIDATES
            .iter()
            .map(|name| dir.join(name))
            .find(|p| p.exists())
            .ok_or_else(|| format!("no engram launch agent found in {}", dir.display()))
    }

    fn launchctl(verb: &str) -> Result<String, String> {
        let plist = launch_agent()?;
        let out = Command::new("launchctl")
            .arg(verb)
            .arg(&plist)
            .output()
            .map_err(|e| format!("launchctl {verb}: {e}"))?;
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if out.status.success() {
            Ok(format!(
                "launchctl {verb} {}",
                plist.file_name().unwrap_or_default().to_string_lossy()
            ))
        } else if stderr.is_empty() {
            Err(format!("launchctl {verb} failed"))
        } else {
            Err(stderr)
        }
    }

    pub fn daemon_start() -> Result<String, String> {
        launchctl("load")
    }

    pub fn daemon_stop() -> Result<String, String> {
        launchctl("unload")
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn daemon_start() -> Result<String, String> {
        Err("daemon start/stop is macOS-only for now (Phase 3 adds other platforms)".to_string())
    }

    pub fn daemon_stop() -> Result<String, String> {
        Err("daemon start/stop is macOS-only for now (Phase 3 adds other platforms)".to_string())
    }
}

pub use imp::{daemon_start, daemon_stop};
