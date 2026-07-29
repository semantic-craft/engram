//! Filesystem-mutation primitives shared by the `--apply` modes on
//! `install-mcp`, `install-hooks`, `setup-agent`, and the new
//! `install-instructions`.
//!
//! Every write goes through [`apply_atomic`], which:
//!
//! 1. Reads the existing file (or empty string if absent).
//! 2. Runs the caller-supplied mutator to compute the new content.
//! 3. If the new content equals the old, returns `NoOp` — never
//!    touches the disk on a redundant call.
//! 4. Otherwise moves the existing file into an unpredictable, private sibling
//!    recovery directory so no pre-created path or symlink can be overwritten.
//! 5. Preserves existing permissions and rechecks content/permissions for a
//!    concurrent change immediately before replacement.
//! 6. Writes the new content to a sibling tempfile, fsyncs, then renames over
//!    the original (POSIX atomic).
//!
//! Every `--apply` mode (install-mcp, install-hooks, install-instructions, …)
//! routes through this function. The mutator decides the format (JSON /
//! TOML / markdown) and the idempotency rule; the I/O atomics live here.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// What the mutation did to the target file. Surfaced to the user
/// so they can tell a meaningful change from a redundant re-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// File didn't exist; we created it.
    Created,
    /// File existed; our mutation changed it. A private sibling recovery copy
    /// records the prior content.
    Updated,
    /// File existed and our mutation produced the same content.
    /// No write happened. No backup written.
    NoOp,
}

/// Complete report for one atomic filesystem mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    /// Whether the target was created, updated, or already matched.
    pub outcome: ApplyOutcome,
    /// Unpredictably named recovery copy retained after an update.
    pub backup_path: Option<PathBuf>,
}

impl ApplyOutcome {
    /// Short verb for the CLI report line.
    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::NoOp => "no-op",
        }
    }
}

/// Apply an idempotent mutation to `path`.
///
/// `mutator` receives the existing file content (`""` if absent) and
/// returns the desired new content. The atomicity, backup, and
/// no-op detection happen here.
///
/// # Errors
/// Propagates IO + mutator failures.
pub fn apply_atomic<F>(path: &Path, mutator: F) -> Result<ApplyOutcome>
where
    F: FnOnce(&str) -> Result<String>,
{
    Ok(apply_atomic_report(path, mutator)?.outcome)
}

/// Apply an idempotent mutation and return its recovery metadata.
///
/// The caller's mutator runs against the content read immediately before the
/// backup/write sequence. Callers that need compare-and-swap safety must
/// perform their final expected-content check inside this closure.
///
/// # Errors
/// Propagates IO + mutator failures.
pub fn apply_atomic_report<F>(path: &Path, mutator: F) -> Result<ApplyReport>
where
    F: FnOnce(&str) -> Result<String>,
{
    let existed = path.exists();
    let (original, original_permissions) = if existed {
        let metadata = fs::metadata(path)
            .with_context(|| format!("reading metadata for {}", path.display()))?;
        (
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
            Some(metadata.permissions()),
        )
    } else {
        (String::new(), None)
    };

    let new_content = mutator(&original)?;

    if existed && new_content == original {
        return Ok(ApplyReport {
            outcome: ApplyOutcome::NoOp,
            backup_path: None,
        });
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("ensuring parent directory {}", parent.display()))?;
    }

    let backup_path = write_atomic(
        path,
        &new_content,
        original_permissions.as_ref(),
        existed.then_some(original.as_bytes()),
    )?;
    Ok(ApplyReport {
        outcome: if existed {
            ApplyOutcome::Updated
        } else {
            ApplyOutcome::Created
        },
        backup_path,
    })
}

/// Roll back a completed apply without ever overwriting a concurrently created
/// path. The just-applied bytes are retained in a private recovery directory;
/// updates restore the original by an atomic no-clobber hard link.
pub fn rollback_atomic_report(
    path: &Path,
    expected_applied: &[u8],
    expected_original: &[u8],
    report: &ApplyReport,
) -> Result<Option<PathBuf>> {
    if report.outcome == ApplyOutcome::NoOp {
        return Ok(None);
    }
    let metadata = fs::metadata(path)
        .with_context(|| format!("reading applied target metadata for {}", path.display()))?;
    let displaced = displace_expected_target(path, expected_applied, &metadata.permissions())?;
    if report.outcome == ApplyOutcome::Updated {
        let backup = report.backup_path.as_ref().ok_or_else(|| {
            anyhow::anyhow!("updated apply cannot roll back without its recovery backup")
        })?;
        let backup_metadata = fs::symlink_metadata(backup)
            .with_context(|| format!("inspecting recovery backup {}", backup.display()))?;
        if backup_metadata.file_type().is_symlink()
            || !backup_metadata.is_file()
            || fs::read(backup)? != expected_original
        {
            anyhow::bail!("recovery backup is not the exact original regular file");
        }
        fs::hard_link(backup, path).with_context(|| {
            format!(
                "restoring {} without overwriting a concurrent target; original remains at {}",
                path.display(),
                backup.display()
            )
        })?;
    }
    Ok(Some(displaced))
}

fn displace_expected_target(
    path: &Path,
    expected: &[u8],
    expected_permissions: &fs::Permissions,
) -> Result<PathBuf> {
    ensure_unchanged_since_read(path, expected, Some(expected_permissions))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let recovery_dir = tempfile::Builder::new()
        .prefix(".engram-apply-rollback.")
        .tempdir_in(parent)
        .with_context(|| format!("creating rollback recovery beside {}", path.display()))?;
    let recovery_path = recovery_dir.path().join(
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("rollback target has no file name"))?,
    );
    fs::rename(path, &recovery_path)
        .with_context(|| format!("moving applied target {} into recovery", path.display()))?;
    let kept_dir = recovery_dir.keep();
    if let Err(error) =
        ensure_unchanged_since_read(&recovery_path, expected, Some(expected_permissions))
    {
        restore_without_overwrite(&recovery_path, path);
        return Err(error.context(format!(
            "applied target changed during rollback; bytes remain in {}",
            kept_dir.display()
        )));
    }
    Ok(recovery_path)
}

/// Tempfile + rename atomic write. The tempfile MUST land in the
/// same directory as the target so `rename(2)` stays intra-filesystem
/// — otherwise we get EXDEV ("Invalid cross-device link").
///
/// This used to fall back to `tempfile()` (i.e. `$TMPDIR`, typically
/// `/tmp` on tmpfs) when the target had no parent component, but
/// that breaks any relative path like `CLAUDE.md` whose parent is
/// `""` (empty) — the project lives on a different filesystem than
/// `/tmp` in just about every realistic setup. Treat empty parent
/// as `.` (current directory) instead.
fn write_atomic(
    path: &Path,
    content: &str,
    permissions: Option<&fs::Permissions>,
    expected_original: Option<&[u8]>,
) -> Result<Option<PathBuf>> {
    write_atomic_with_pre_handoff(path, content, permissions, expected_original, || {})
}

fn write_atomic_with_pre_handoff<F>(
    path: &Path,
    content: &str,
    permissions: Option<&fs::Permissions>,
    expected_original: Option<&[u8]>,
    before_handoff: F,
) -> Result<Option<PathBuf>>
where
    F: FnOnce(),
{
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::Builder::new()
        .prefix(".engram-apply-tmp.")
        .tempfile_in(parent)
        .with_context(|| format!("creating tempfile next to {}", path.display()))?;
    tmp.write_all(content.as_bytes())
        .context("writing tempfile content")?;
    if let Some(permissions) = permissions {
        tmp.as_file()
            .set_permissions(permissions.clone())
            .context("preserving target permissions on tempfile")?;
    }
    tmp.as_file().sync_data().context("fsync tempfile")?;
    let Some(expected_original) = expected_original else {
        tmp.persist_noclobber(path).with_context(|| {
            format!(
                "creating {} without overwriting a concurrently created target",
                path.display()
            )
        })?;
        return Ok(None);
    };

    ensure_unchanged_since_read(path, expected_original, permissions)?;
    before_handoff();
    let backup_dir = tempfile::Builder::new()
        .prefix(".engram-apply-backup.")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "creating private recovery directory next to {}",
                path.display()
            )
        })?;
    let backup_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("apply target has no file name: {}", path.display()))?;
    let backup_path = backup_dir.path().join(backup_name);
    fs::rename(path, &backup_path)
        .with_context(|| format!("moving {} into private recovery storage", path.display()))?;
    let kept_backup_dir = backup_dir.keep();

    if let Err(error) = ensure_unchanged_since_read(&backup_path, expected_original, permissions) {
        restore_without_overwrite(&backup_path, path);
        return Err(error.context(format!(
            "target changed during atomic handoff; recovery bytes remain in {}",
            kept_backup_dir.display()
        )));
    }

    if let Err(error) = tmp.persist_noclobber(path) {
        restore_without_overwrite(&backup_path, path);
        return Err(anyhow::Error::new(error.error).context(format!(
            "installing {} without overwriting a concurrent target; recovery bytes remain in {}",
            path.display(),
            kept_backup_dir.display()
        )));
    }
    Ok(Some(backup_path))
}

/// Restore a displaced target only when the path is still absent. `hard_link`
/// is an atomic no-clobber operation on the same filesystem; if another writer
/// has already recreated the path, both its bytes and our recovery copy remain.
fn restore_without_overwrite(backup: &Path, target: &Path) {
    let _ = fs::hard_link(backup, target);
}

fn ensure_unchanged_since_read(
    path: &Path,
    expected: &[u8],
    expected_permissions: Option<&fs::Permissions>,
) -> Result<()> {
    let current = fs::read(path)
        .with_context(|| format!("re-reading {} before atomic replace", path.display()))?;
    if current != expected {
        anyhow::bail!("target changed concurrently during atomic apply; refusing to overwrite it");
    }
    if let Some(expected_permissions) = expected_permissions {
        let current_permissions = fs::metadata(path)
            .with_context(|| format!("re-reading metadata for {}", path.display()))?
            .permissions();
        if !same_permissions(&current_permissions, expected_permissions) {
            anyhow::bail!(
                "target permissions changed concurrently during atomic apply; refusing to overwrite it"
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn same_permissions(left: &fs::Permissions, right: &fs::Permissions) -> bool {
    use std::os::unix::fs::PermissionsExt;
    left.mode() == right.mode()
}

#[cfg(not(unix))]
fn same_permissions(left: &fs::Permissions, right: &fs::Permissions) -> bool {
    left.readonly() == right.readonly()
}

// --------------------------------------------------------------------
// JSON mutation helpers
// --------------------------------------------------------------------

/// Parse `original` as JSON (or yield an empty object if blank),
/// hand the mutable object to `mutator`, and return the
/// pretty-printed result with a trailing newline.
///
/// Errors out with a clear "this file isn't JSON" message rather
/// than silently overwriting; the user gets a chance to investigate.
///
/// # Errors
/// Returns an error if the input is non-empty and not parseable as
/// a JSON object.
pub fn mutate_json<F>(original: &str, mutator: F) -> Result<String>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<()>,
{
    let mut root: serde_json::Map<String, serde_json::Value> = if original.trim().is_empty() {
        serde_json::Map::new()
    } else {
        let parsed: serde_json::Value = serde_json::from_str(original).with_context(|| {
            "existing file isn't valid JSON; refusing to overwrite. Inspect by hand, \
             rename it, or delete it before re-running --apply."
        })?;
        match parsed {
            serde_json::Value::Object(m) => m,
            _ => {
                anyhow::bail!(
                    "existing file is JSON but not an object at the root \
                     (top-level array / string / number). Refusing to overwrite."
                );
            }
        }
    };
    mutator(&mut root)?;
    let mut out = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .context("serialising merged JSON")?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Read-mutate-write for TOML files via `toml_edit` (preserves
/// comments + formatting from the original).
///
/// `mutator` receives the parsed `DocumentMut` and can use the full
/// `toml_edit` API to make changes. Returns the rendered TOML.
///
/// # Errors
/// Returns an error if the input is non-empty and not parseable.
pub fn mutate_toml<F>(original: &str, mutator: F) -> Result<String>
where
    F: FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
{
    let mut doc: toml_edit::DocumentMut = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original.parse().with_context(|| {
            "existing file isn't valid TOML; refusing to overwrite. Inspect by hand, \
             rename it, or delete it before re-running --apply."
        })?
    };
    mutator(&mut doc)?;
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn apply_to_missing_file_creates() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("nested/dir/foo.json");
        let outcome = apply_atomic(&p, |_| Ok("hello\n".into())).unwrap();
        assert_eq!(outcome, ApplyOutcome::Created);
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello\n");
    }

    #[test]
    fn apply_to_unchanged_file_is_noop_and_no_backup() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("foo.json");
        fs::write(&p, "same\n").unwrap();
        let outcome = apply_atomic(&p, |_| Ok("same\n".into())).unwrap();
        assert_eq!(outcome, ApplyOutcome::NoOp);
        // No recovery directory should appear.
        let backups: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".engram-apply-backup.")
            })
            .collect();
        assert!(backups.is_empty(), "no-op must not create a backup");
    }

    #[test]
    fn apply_to_changed_file_backs_up_then_writes() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("foo.json");
        fs::write(&p, "old\n").unwrap();
        let report = apply_atomic_report(&p, |_| Ok("new\n".into())).unwrap();
        assert_eq!(report.outcome, ApplyOutcome::Updated);
        assert_eq!(fs::read_to_string(&p).unwrap(), "new\n");
        let bak_content = fs::read_to_string(report.backup_path.unwrap()).unwrap();
        assert_eq!(bak_content, "old\n");
    }

    #[test]
    fn concurrent_change_during_mutator_is_never_overwritten() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("AGENTS.md");
        fs::write(&p, "approved base\n").unwrap();
        let raced = p.clone();
        let error = apply_atomic_report(&p, move |_| {
            fs::write(&raced, "concurrent owner bytes\n").unwrap();
            Ok("approved proposal\n".into())
        })
        .unwrap_err();
        assert!(error.to_string().contains("changed concurrently"));
        assert_eq!(fs::read_to_string(&p).unwrap(), "concurrent owner bytes\n");
    }

    #[test]
    fn concurrent_change_after_final_check_is_recovered_without_overwrite() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("AGENTS.md");
        fs::write(&target, "approved base\n").unwrap();
        let raced = target.clone();
        let error = write_atomic_with_pre_handoff(
            &target,
            "approved proposal\n",
            None,
            Some(b"approved base\n"),
            move || fs::write(&raced, "concurrent owner bytes\n").unwrap(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("target changed during atomic handoff")
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "concurrent owner bytes\n"
        );
    }

    #[test]
    fn precreated_backup_names_are_never_touched() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("AGENTS.md");
        let decoy = tmp.path().join("AGENTS.md.bak-0");
        fs::write(&target, "old\n").unwrap();
        fs::write(&decoy, "decoy\n").unwrap();
        let report = apply_atomic_report(&target, |_| Ok("new\n".into())).unwrap();
        assert_eq!(fs::read_to_string(&decoy).unwrap(), "decoy\n");
        assert_eq!(
            fs::read_to_string(report.backup_path.unwrap()).unwrap(),
            "old\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn precreated_external_backup_symlink_is_never_followed() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("AGENTS.md");
        let external = tmp.path().join("external.txt");
        let decoy = tmp.path().join("AGENTS.md.bak-0");
        fs::write(&target, "old\n").unwrap();
        fs::write(&external, "external\n").unwrap();
        symlink(&external, &decoy).unwrap();
        apply_atomic_report(&target, |_| Ok("new\n".into())).unwrap();
        assert_eq!(fs::read_to_string(&external).unwrap(), "external\n");
    }

    #[cfg(unix)]
    #[test]
    fn changed_file_preserves_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("AGENTS.md");
        fs::write(&p, "old\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        apply_atomic(&p, |_| Ok("new\n".into())).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn completed_update_can_roll_back_without_clobbering_recovery() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("AGENTS.md");
        fs::write(&target, "base\n").unwrap();
        let report = apply_atomic_report(&target, |_| Ok("proposed\n".into())).unwrap();
        let displaced = rollback_atomic_report(&target, b"proposed\n", b"base\n", &report)
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "base\n");
        assert_eq!(fs::read_to_string(displaced).unwrap(), "proposed\n");
        assert_eq!(
            fs::read_to_string(report.backup_path.unwrap()).unwrap(),
            "base\n"
        );
    }

    #[test]
    fn json_mutator_preserves_user_keys() {
        let original = r#"{"unrelated":"keep me","mcpServers":{"foo":{"url":"http://foo"}}}"#;
        let out = mutate_json(original, |m| {
            let servers = m
                .entry("mcpServers")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .unwrap();
            servers.insert(
                "engram".into(),
                serde_json::json!({"url": "http://homelab:49374/mcp"}),
            );
            Ok(())
        })
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        // Unrelated key survives.
        assert_eq!(parsed["unrelated"], "keep me");
        // Sibling MCP server survives.
        assert_eq!(parsed["mcpServers"]["foo"]["url"], "http://foo");
        // Ours is added.
        assert_eq!(
            parsed["mcpServers"]["engram"]["url"],
            "http://homelab:49374/mcp"
        );
    }

    #[test]
    fn json_mutator_rejects_non_object_root() {
        let err = mutate_json("[1,2,3]", |_| Ok(())).unwrap_err();
        assert!(format!("{err:?}").contains("not an object"));
    }

    #[test]
    fn json_mutator_rejects_invalid_json() {
        let err = mutate_json("{not valid", |_| Ok(())).unwrap_err();
        assert!(format!("{err:?}").contains("isn't valid JSON"));
    }

    #[test]
    fn toml_mutator_preserves_comments_and_other_tables() {
        let original = "# top comment kept\n\
                        [other]\n\
                        keep = \"this\"\n";
        let out = mutate_toml(original, |doc| {
            doc["mcp_servers"]["engram"]["url"] = toml_edit::value("http://homelab:49374/mcp");
            Ok(())
        })
        .unwrap();
        assert!(out.contains("# top comment kept"));
        assert!(out.contains("[other]"));
        assert!(out.contains("keep = \"this\""));
        assert!(out.contains("engram"));
        assert!(out.contains("http://homelab:49374/mcp"));
    }

    #[test]
    fn idempotent_double_apply_second_is_noop() {
        // The realistic flow: user runs --apply twice in a row,
        // second call should be a clean no-op.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("settings.json");

        let mutator = |s: &str| {
            mutate_json(s, |m| {
                m.insert("foo".into(), serde_json::json!("bar"));
                Ok(())
            })
        };
        let first = apply_atomic(&p, mutator).unwrap();
        assert_eq!(first, ApplyOutcome::Created);
        let second = apply_atomic(&p, mutator).unwrap();
        assert_eq!(second, ApplyOutcome::NoOp);
    }
}
