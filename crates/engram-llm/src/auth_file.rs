//! Shared JSON auth-file helpers for token-backed providers.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Serialize, de::DeserializeOwned};

use crate::error::{LlmError, LlmResult};

pub(crate) fn load_entry<T>(path: &Path, key: &str) -> LlmResult<Option<T>>
where
    T: DeserializeOwned,
{
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .map_err(|e| LlmError::Auth(format!("read auth file {}: {e}", path.display())))?;
    let mut value = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|e| LlmError::Auth(format!("parse auth file {}: {e}", path.display())))?;
    let Some(entry) = value.as_object_mut().and_then(|obj| obj.remove(key)) else {
        return Ok(None);
    };
    if entry.get("type").and_then(serde_json::Value::as_str) != Some("oauth") {
        return Ok(None);
    }
    serde_json::from_value::<T>(entry)
        .map(Some)
        .map_err(|e| LlmError::Auth(format!("parse {key} auth entry: {e}")))
}

pub(crate) fn save_entry<T>(path: &Path, key: &str, entry: Option<T>) -> LlmResult<()>
where
    T: Serialize,
{
    let mut root = if path.exists() {
        let bytes = std::fs::read(path)
            .map_err(|e| LlmError::Auth(format!("read auth file {}: {e}", path.display())))?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| LlmError::Auth(format!("parse auth file {}: {e}", path.display())))?
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        return Err(LlmError::Auth(format!(
            "auth file {} must contain a JSON object",
            path.display()
        )));
    }
    let Some(obj) = root.as_object_mut() else {
        return Err(LlmError::Auth(format!(
            "auth file {} must contain a JSON object",
            path.display()
        )));
    };
    match entry {
        Some(entry) => {
            obj.insert(key.to_string(), serde_json::to_value(entry)?);
        }
        None => {
            obj.remove(key);
        }
    }
    if obj.is_empty() {
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|e| LlmError::Auth(format!("remove auth file {}: {e}", path.display())))?;
        }
        return Ok(());
    }
    write_auth_file(path, &root)
}

fn write_auth_file(path: &Path, value: &serde_json::Value) -> LlmResult<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| LlmError::Auth(format!("create auth dir {}: {e}", parent.display())))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| LlmError::Auth(format!("open auth tmp {}: {e}", tmp.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| LlmError::Auth(format!("chmod auth tmp {}: {e}", tmp.display())))?;
        }
        file.write_all(&bytes)
            .map_err(|e| LlmError::Auth(format!("write auth tmp {}: {e}", tmp.display())))?;
        file.sync_all()
            .map_err(|e| LlmError::Auth(format!("fsync auth tmp {}: {e}", tmp.display())))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| LlmError::Auth(format!("rename auth file {}: {e}", path.display())))?;
    Ok(())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
