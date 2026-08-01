//! Local instruction-file access (CLAUDE.md / AGENTS.md).
//!
//! The daemon never reads the agent host's filesystem; this module is the
//! desktop-side counterpart that inspects instruction files on *this*
//! machine. Read-only by design — edits go through the user's editor.

use std::path::{Path, PathBuf};

use crate::types::InstructionFile;

/// Cap returned content so a pathological file can't flood the webview.
const MAX_CONTENT_BYTES: usize = 256 * 1024;

/// Per-project discovery candidates, in precedence order. Mirrors the
/// engram-cli instruction steward's source list.
pub const PROJECT_CANDIDATES: [&str; 3] = ["CLAUDE.md", ".claude/CLAUDE.md", "AGENTS.md"];

/// Expand `~/` against `$HOME`; require absolute paths otherwise.
pub fn expand_home(path: &str) -> Result<PathBuf, String> {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        Ok(Path::new(&home).join(rest))
    } else if path.starts_with('/') {
        Ok(PathBuf::from(path))
    } else {
        Err(format!("instruction path must be absolute or ~/…: {path}"))
    }
}

/// Stat + read one instruction file. Missing files come back with
/// `exists: false` rather than an error so the UI can render placeholders.
pub fn inspect(display: &str, abs: &Path) -> InstructionFile {
    let missing = || InstructionFile {
        path: display.to_owned(),
        abs_path: abs.display().to_string(),
        exists: false,
        size: None,
        modified_ms: None,
        content: None,
        truncated: false,
    };
    let Ok(meta) = std::fs::metadata(abs) else {
        return missing();
    };
    if !meta.is_file() {
        return missing();
    }
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok());
    let raw = std::fs::read(abs).unwrap_or_default();
    let truncated = raw.len() > MAX_CONTENT_BYTES;
    let slice = if truncated {
        &raw[..MAX_CONTENT_BYTES]
    } else {
        &raw[..]
    };
    InstructionFile {
        path: display.to_owned(),
        abs_path: abs.display().to_string(),
        exists: true,
        size: Some(meta.len()),
        modified_ms,
        content: Some(String::from_utf8_lossy(slice).into_owned()),
        truncated,
    }
}

/// Inspect the project-level candidates under a repository root.
pub fn discover(root: &Path) -> Vec<InstructionFile> {
    PROJECT_CANDIDATES
        .iter()
        .map(|rel| inspect(rel, &root.join(rel)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("engram-desktop-instr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn expand_home_accepts_absolute_rejects_relative() {
        assert_eq!(
            expand_home("/etc/hosts").unwrap(),
            PathBuf::from("/etc/hosts")
        );
        assert!(expand_home("relative/CLAUDE.md").is_err());
        assert!(expand_home("").is_err());
    }

    #[test]
    fn discover_reports_present_and_missing_candidates() {
        let root = scratch_dir("discover");
        std::fs::write(root.join("CLAUDE.md"), "@AGENTS.md\n").unwrap();
        std::fs::write(root.join("AGENTS.md"), "# Rules\nbody\n").unwrap();

        let files = discover(&root);
        assert_eq!(files.len(), 3);
        let claude = &files[0];
        assert_eq!(claude.path, "CLAUDE.md");
        assert!(claude.exists);
        assert_eq!(claude.content.as_deref(), Some("@AGENTS.md\n"));
        assert!(!claude.truncated);
        assert!(claude.modified_ms.is_some());

        let nested = &files[1];
        assert_eq!(nested.path, ".claude/CLAUDE.md");
        assert!(!nested.exists);
        assert!(nested.content.is_none());

        let agents = &files[2];
        assert!(agents.exists);
        assert_eq!(agents.size, Some("# Rules\nbody\n".len() as u64));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn inspect_truncates_oversized_files() {
        let root = scratch_dir("truncate");
        let big = root.join("AGENTS.md");
        std::fs::write(&big, "x".repeat(MAX_CONTENT_BYTES + 10)).unwrap();

        let file = inspect("AGENTS.md", &big);
        assert!(file.exists);
        assert!(file.truncated);
        assert_eq!(
            file.content.as_ref().map(String::len),
            Some(MAX_CONTENT_BYTES)
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
