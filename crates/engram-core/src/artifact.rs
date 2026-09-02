//! Typed, verifiable artifact references for WorkItem continuity.
//!
//! An [`ArtifactRef`] identifies a file, Git repository revision, worktree, or
//! external object without treating an absolute cwd as identity. Delivery facts
//! such as changed, verified, committed, and pushed are stored independently
//! and are never inferred from one another. Engram records observed status; it
//! never checks out, commits, pushes, merges, releases, deploys, submits, or
//! approves anything in Git or an external system.

#![allow(missing_docs)]

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
use crate::ids::{ArtifactId, ProjectId, SessionId};

/// Kind of verifiable object a Handoff or Checkpoint can point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    File,
    Git,
    Worktree,
    External,
}

impl ArtifactKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Git => "git",
            Self::Worktree => "worktree",
            Self::External => "external",
        }
    }
}

impl std::str::FromStr for ArtifactKind {
    type Err = MemoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "file" => Ok(Self::File),
            "git" => Ok(Self::Git),
            "worktree" => Ok(Self::Worktree),
            "external" => Ok(Self::External),
            other => Err(MemoryError::MalformedRecord(format!(
                "unknown artifact kind: {other}"
            ))),
        }
    }
}

/// Independent delivery facts. Each flag is an explicit observation; no flag
/// is derived from any other flag.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DeliveryFacts {
    pub changed: bool,
    pub verified: bool,
    pub committed: bool,
    pub pushed: bool,
    pub reviewed: bool,
    pub merged: bool,
    pub released: bool,
    pub deployed: bool,
    pub submitted: bool,
    pub approved: bool,
}

impl DeliveryFacts {
    /// Wire names in the order tests and docs enumerate independent facts.
    pub const FLAG_NAMES: [&'static str; 10] = [
        "changed",
        "verified",
        "committed",
        "pushed",
        "reviewed",
        "merged",
        "released",
        "deployed",
        "submitted",
        "approved",
    ];

    #[must_use]
    pub fn only(flag: &str) -> Option<Self> {
        let mut facts = Self::default();
        match flag {
            "changed" => facts.changed = true,
            "verified" => facts.verified = true,
            "committed" => facts.committed = true,
            "pushed" => facts.pushed = true,
            "reviewed" => facts.reviewed = true,
            "merged" => facts.merged = true,
            "released" => facts.released = true,
            "deployed" => facts.deployed = true,
            "submitted" => facts.submitted = true,
            "approved" => facts.approved = true,
            _ => return None,
        }
        Some(facts)
    }

    #[must_use]
    pub fn asserted_flags(&self) -> Vec<&'static str> {
        let mut flags = Vec::new();
        if self.changed {
            flags.push("changed");
        }
        if self.verified {
            flags.push("verified");
        }
        if self.committed {
            flags.push("committed");
        }
        if self.pushed {
            flags.push("pushed");
        }
        if self.reviewed {
            flags.push("reviewed");
        }
        if self.merged {
            flags.push("merged");
        }
        if self.released {
            flags.push("released");
        }
        if self.deployed {
            flags.push("deployed");
        }
        if self.submitted {
            flags.push("submitted");
        }
        if self.approved {
            flags.push("approved");
        }
        flags
    }
}

/// Caller-supplied verification evidence. Timestamp and source Run are filled
/// by the server from the enclosing Handoff or Checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VerificationEvidenceInput {
    pub check: String,
    pub result: String,
    #[serde(default)]
    pub applies_to_revision: Option<String>,
}

/// Recorded verification evidence for one artifact attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationEvidence {
    pub check: String,
    pub result: String,
    pub observed_at: Timestamp,
    pub source_run_id: SessionId,
    pub applies_to_revision: String,
    /// True when `applies_to_revision` does not match the artifact's observed
    /// revision. Stale evidence is retained; it is never treated as current.
    pub stale: bool,
}

/// Publish/checkpoint input for one typed artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArtifactInput {
    pub kind: ArtifactKind,
    /// Stable locator. For files this is repository-relative. Absolute cwd
    /// values belong in `local_path_hint` and are never identity.
    pub locator: String,
    #[serde(default)]
    pub observed_revision: Option<String>,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub repository_identity: Option<String>,
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
    #[serde(default)]
    pub tree_hash: Option<String>,
    #[serde(default)]
    pub dirty: Option<bool>,
    #[serde(default)]
    pub local_path_hint: Option<String>,
    #[serde(default)]
    pub provenance: String,
    #[serde(default)]
    pub delivery: DeliveryFacts,
    #[serde(default)]
    pub verification: Vec<VerificationEvidenceInput>,
}

impl ArtifactInput {
    /// Canonical identity key. Absolute paths and local-path hints are
    /// excluded so two machines with different cwds resolve the same object.
    pub fn identity_key(&self) -> Result<String, MemoryError> {
        let normalized = self.normalized()?;
        Ok(normalized.identity_key)
    }

    pub fn normalized(&self) -> Result<NormalizedArtifact, MemoryError> {
        let locator = normalize_locator(&self.locator)?;
        if locator.is_empty() {
            return Err(MemoryError::MalformedRecord(
                "artifact locator must not be empty".into(),
            ));
        }
        let repository_identity = self
            .repository_identity
            .as_deref()
            .map(normalize_repository_identity)
            .transpose()?;
        let commit_id = nonempty_opt(self.commit_id.as_deref());
        let observed_revision = nonempty_opt(self.observed_revision.as_deref())
            .or_else(|| commit_id.clone())
            .or_else(|| nonempty_opt(self.content_hash.as_deref()));
        let content_hash = nonempty_opt(self.content_hash.as_deref());
        let tree_hash = nonempty_opt(self.tree_hash.as_deref());
        let git_ref = nonempty_opt(self.git_ref.as_deref());
        let local_path_hint = nonempty_opt(self.local_path_hint.as_deref());
        let provenance = self.provenance.trim().to_string();

        match self.kind {
            ArtifactKind::File => {
                if is_absolute_path(&locator) {
                    return Err(MemoryError::MalformedRecord(
                        "file artifact locator must be repository-relative; put absolute paths in local_path_hint".into(),
                    ));
                }
            }
            ArtifactKind::Worktree if is_absolute_path(&locator) => {
                return Err(MemoryError::MalformedRecord(
                    "worktree artifact locator must not be an absolute filesystem path; put absolute paths in local_path_hint".into(),
                ));
            }
            ArtifactKind::Git | ArtifactKind::Worktree => {
                if repository_identity
                    .as_ref()
                    .is_none_or(|value| value.is_empty())
                {
                    return Err(MemoryError::MalformedRecord(format!(
                        "{} artifact requires repository_identity; absolute cwd is not identity",
                        self.kind.as_str()
                    )));
                }
                if observed_revision
                    .as_ref()
                    .is_none_or(|value| value.is_empty())
                {
                    return Err(MemoryError::MalformedRecord(format!(
                        "{} artifact requires observed_revision or commit_id",
                        self.kind.as_str()
                    )));
                }
            }
            ArtifactKind::External => {}
        }

        let identity_key = match self.kind {
            ArtifactKind::File => format!(
                "file|{}|{locator}",
                repository_identity.as_deref().unwrap_or("")
            ),
            ArtifactKind::Git => format!(
                "git|{}|{}",
                repository_identity.as_deref().unwrap_or(""),
                observed_revision.as_deref().unwrap_or("")
            ),
            ArtifactKind::Worktree => format!(
                "worktree|{}|{}|{locator}|{}",
                repository_identity.as_deref().unwrap_or(""),
                observed_revision.as_deref().unwrap_or(""),
                worktree_identity_fingerprint(
                    self.dirty,
                    tree_hash.as_deref(),
                    content_hash.as_deref()
                )
            ),
            ArtifactKind::External => format!(
                "external|{locator}|{}",
                observed_revision.as_deref().unwrap_or("")
            ),
        };

        Ok(NormalizedArtifact {
            kind: self.kind,
            locator,
            observed_revision,
            content_hash,
            repository_identity,
            git_ref,
            commit_id,
            tree_hash,
            dirty: self.dirty,
            local_path_hint,
            provenance,
            delivery: self.delivery.clone(),
            verification: self.verification.clone(),
            identity_key,
        })
    }
}

/// Validated artifact fields ready for persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedArtifact {
    pub kind: ArtifactKind,
    pub locator: String,
    pub observed_revision: Option<String>,
    pub content_hash: Option<String>,
    pub repository_identity: Option<String>,
    pub git_ref: Option<String>,
    pub commit_id: Option<String>,
    pub tree_hash: Option<String>,
    pub dirty: Option<bool>,
    pub local_path_hint: Option<String>,
    pub provenance: String,
    pub delivery: DeliveryFacts,
    pub verification: Vec<VerificationEvidenceInput>,
    pub identity_key: String,
}

impl NormalizedArtifact {
    /// Stable identity including repository or project coordinates for files.
    #[must_use]
    pub fn identity_key_for_scope(&self, project_id: ProjectId) -> String {
        if self.kind == ArtifactKind::File
            && self
                .repository_identity
                .as_deref()
                .is_none_or(str::is_empty)
        {
            format!("file|scope:{project_id}|{}", self.locator)
        } else {
            self.identity_key.clone()
        }
    }
}

/// Materialized artifact attached to a Handoff, Checkpoint, or parent result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    pub locator: String,
    pub observed_revision: Option<String>,
    pub content_hash: Option<String>,
    pub repository_identity: Option<String>,
    pub git_ref: Option<String>,
    pub commit_id: Option<String>,
    pub tree_hash: Option<String>,
    pub dirty: Option<bool>,
    /// Local absolute path, never part of identity.
    pub local_path_hint: Option<String>,
    pub provenance: String,
    pub source_run_id: SessionId,
    pub observed_at: Timestamp,
    pub delivery: DeliveryFacts,
    pub verification: Vec<VerificationEvidence>,
}

fn worktree_identity_fingerprint(
    dirty: Option<bool>,
    tree_hash: Option<&str>,
    content_hash: Option<&str>,
) -> String {
    if dirty == Some(true) {
        tree_hash
            .filter(|value| !value.is_empty())
            .or_else(|| content_hash.filter(|value| !value.is_empty()))
            .unwrap_or("dirty")
            .to_string()
    } else {
        "clean".to_string()
    }
}

fn nonempty_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Canonicalize a locator so identity does not depend on the reporting
/// machine's path separator.
///
/// `\` collapses to `/` before the caller derives an identity key: the same
/// repository file reported as `src\lib.rs` from Windows and `src/lib.rs`
/// from macOS is one object, so both must produce one key. Every locator
/// leaving this function therefore uses `/` exclusively.
fn normalize_locator(raw: &str) -> Result<String, MemoryError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.contains('\0') {
        return Err(MemoryError::MalformedRecord(
            "artifact locator must not contain NUL".into(),
        ));
    }
    let unified = trimmed.replace('\\', "/");
    for segment in unified.split('/') {
        if segment == ".." {
            return Err(MemoryError::MalformedRecord(
                "artifact locator must not contain parent segments".into(),
            ));
        }
    }
    Ok(unified.trim_end_matches('/').to_string())
}

fn normalize_repository_identity(raw: &str) -> Result<String, MemoryError> {
    let locator = normalize_locator(raw)?;
    Ok(locator.trim_end_matches(".git").to_string())
}

/// Whether a normalized locator names a filesystem location rather than a
/// path inside a repository.
///
/// A bare drive prefix counts: [`normalize_locator`] strips trailing
/// separators, so a Windows checkout root arrives here as `C:` rather than
/// `C:\\`. Requiring three characters would let that root through as
/// cross-machine identity, which is exactly what `local_path_hint` is for.
/// `C:relative` is drive-relative and not a repository path either.
///
/// Input is always a [`normalize_locator`] result, so separators are already
/// `/`: a UNC root arrives as `//host/share` and needs no separate `\` case.
fn is_absolute_path(value: &str) -> bool {
    value.starts_with('/')
        || (value.len() >= 2
            && value.as_bytes()[1] == b':'
            && value.as_bytes()[0].is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_identity_ignores_absolute_cwd_hints() {
        let left = ArtifactInput {
            kind: ArtifactKind::Git,
            locator: "ignored-cwd".into(),
            observed_revision: Some("abc123".into()),
            repository_identity: Some("https://github.com/semantic-craft/engram.git/".into()),
            local_path_hint: Some("/tmp/machine-a/engram".into()),
            provenance: "source".into(),
            ..empty_input()
        };
        let right = ArtifactInput {
            kind: ArtifactKind::Git,
            locator: "other-cwd".into(),
            observed_revision: Some("abc123".into()),
            repository_identity: Some("https://github.com/semantic-craft/engram".into()),
            local_path_hint: Some("/var/machine-b/engram".into()),
            provenance: "source".into(),
            ..empty_input()
        };
        assert_eq!(left.identity_key().unwrap(), right.identity_key().unwrap());
        assert_ne!(
            left.local_path_hint.as_deref(),
            right.local_path_hint.as_deref()
        );
    }

    #[test]
    fn file_locator_rejects_absolute_paths() {
        let input = ArtifactInput {
            kind: ArtifactKind::File,
            locator: "/tmp/repo/src/lib.rs".into(),
            local_path_hint: Some("/tmp/repo/src/lib.rs".into()),
            ..empty_input()
        };
        assert!(input.normalized().is_err());
    }

    #[test]
    fn absolute_locators_are_rejected_including_drive_roots() {
        // `normalize_locator` strips the trailing separator, so a Windows
        // checkout root reaches the check as `C:`.
        for locator in [
            "/",
            "/tmp/machine-a/engram",
            "C:\\",
            "C:/",
            "C:",
            "\\\\host\\share",
        ] {
            for kind in [ArtifactKind::Worktree, ArtifactKind::File] {
                let input = ArtifactInput {
                    kind,
                    locator: locator.into(),
                    repository_identity: Some("github.com/semantic-craft/engram".into()),
                    observed_revision: Some("abc123".into()),
                    local_path_hint: Some(locator.into()),
                    ..empty_input()
                };
                let error = input
                    .normalized()
                    .expect_err(&format!("{kind:?} must reject {locator}"))
                    .to_string();
                assert!(
                    error.contains("local_path_hint") || error.contains("must not be empty"),
                    "{kind:?} {locator}: {error}"
                );
            }
        }

        // A repository-relative path that merely looks drive-ish is fine.
        let ok = ArtifactInput {
            kind: ArtifactKind::Worktree,
            locator: "wt/machine-a".into(),
            repository_identity: Some("github.com/semantic-craft/engram".into()),
            observed_revision: Some("abc123".into()),
            local_path_hint: Some("/tmp/machine-a/engram".into()),
            ..empty_input()
        };
        assert!(ok.normalized().is_ok());
    }

    #[test]
    fn worktree_locator_rejects_absolute_paths() {
        let input = ArtifactInput {
            kind: ArtifactKind::Worktree,
            locator: "/tmp/machine-a/engram".into(),
            repository_identity: Some("github.com/semantic-craft/engram".into()),
            observed_revision: Some("abc123".into()),
            local_path_hint: Some("/tmp/machine-a/engram".into()),
            ..empty_input()
        };
        let error = input.normalized().unwrap_err().to_string();
        assert!(error.contains("local_path_hint"), "{error}");
        assert!(!error.contains("https://"));
    }

    #[test]
    fn external_locator_allows_https_url() {
        let input = ArtifactInput {
            kind: ArtifactKind::External,
            locator: "https://github.com/semantic-craft/engram/issues/42".into(),
            observed_revision: Some("open".into()),
            ..empty_input()
        };
        assert!(input.normalized().is_ok());
    }

    #[test]
    fn file_identity_includes_repository_not_just_relative_path() {
        let left = ArtifactInput {
            kind: ArtifactKind::File,
            locator: "src/lib.rs".into(),
            repository_identity: Some("github.com/org/repo-a".into()),
            ..empty_input()
        };
        let right = ArtifactInput {
            kind: ArtifactKind::File,
            locator: "src/lib.rs".into(),
            repository_identity: Some("github.com/org/repo-b".into()),
            ..empty_input()
        };
        assert_ne!(left.identity_key().unwrap(), right.identity_key().unwrap());
        assert!(left.identity_key().unwrap().contains("repo-a"));
    }

    #[test]
    fn dirty_worktrees_at_the_same_commit_have_distinct_identities() {
        let left = ArtifactInput {
            kind: ArtifactKind::Worktree,
            locator: "wt-a".into(),
            repository_identity: Some("github.com/semantic-craft/engram".into()),
            observed_revision: Some("abc123".into()),
            tree_hash: Some("dirty-tree-a".into()),
            dirty: Some(true),
            local_path_hint: Some("/tmp/wt-a".into()),
            ..empty_input()
        };
        let right = ArtifactInput {
            kind: ArtifactKind::Worktree,
            locator: "wt-b".into(),
            repository_identity: Some("github.com/semantic-craft/engram".into()),
            observed_revision: Some("abc123".into()),
            tree_hash: Some("dirty-tree-b".into()),
            dirty: Some(true),
            local_path_hint: Some("/tmp/wt-b".into()),
            ..empty_input()
        };
        assert_ne!(left.identity_key().unwrap(), right.identity_key().unwrap());
        assert!(!left.identity_key().unwrap().contains("/tmp/wt-a"));
    }

    #[test]
    fn delivery_facts_are_independent() {
        for flag in DeliveryFacts::FLAG_NAMES {
            let facts = DeliveryFacts::only(flag).unwrap();
            assert_eq!(facts.asserted_flags(), vec![flag]);
        }
    }

    /// #54: one repository file reported from Windows (`src\lib.rs`) and from
    /// a POSIX machine (`src/lib.rs`) is one object under #42's cross-machine
    /// identity rule, so the separator the reporter happened to use must not
    /// survive into the identity key — scoped or unscoped.
    #[test]
    fn file_identity_is_separator_independent() {
        let project_id = ProjectId::new();
        let posix = ArtifactInput {
            kind: ArtifactKind::File,
            locator: "src/nested/lib.rs".into(),
            repository_identity: Some("github.com/semantic-craft/engram".into()),
            ..empty_input()
        };
        let windows = ArtifactInput {
            kind: ArtifactKind::File,
            locator: "src\\nested\\lib.rs".into(),
            repository_identity: Some("github.com/semantic-craft/engram".into()),
            ..empty_input()
        };
        assert_eq!(
            posix.identity_key().unwrap(),
            windows.identity_key().unwrap()
        );
        assert_eq!(
            posix
                .normalized()
                .unwrap()
                .identity_key_for_scope(project_id),
            windows
                .normalized()
                .unwrap()
                .identity_key_for_scope(project_id)
        );

        // The repository-less file falls back to the project-scoped key,
        // which embeds the locator too.
        let scoped_posix = ArtifactInput {
            kind: ArtifactKind::File,
            locator: "src/nested/lib.rs".into(),
            ..empty_input()
        };
        let scoped_windows = ArtifactInput {
            kind: ArtifactKind::File,
            locator: "src\\nested\\lib.rs".into(),
            ..empty_input()
        };
        assert_eq!(
            scoped_posix
                .normalized()
                .unwrap()
                .identity_key_for_scope(project_id),
            scoped_windows
                .normalized()
                .unwrap()
                .identity_key_for_scope(project_id)
        );
    }

    /// The worktree key embeds the locator as well, so it inherits the same
    /// separator hazard the File key had.
    #[test]
    fn worktree_identity_is_separator_independent() {
        let posix = ArtifactInput {
            kind: ArtifactKind::Worktree,
            locator: "wt/machine-a".into(),
            repository_identity: Some("github.com/semantic-craft/engram".into()),
            observed_revision: Some("abc123".into()),
            local_path_hint: Some("/tmp/machine-a/engram".into()),
            ..empty_input()
        };
        let windows = ArtifactInput {
            kind: ArtifactKind::Worktree,
            locator: "wt\\machine-a".into(),
            repository_identity: Some("github.com/semantic-craft/engram".into()),
            observed_revision: Some("abc123".into()),
            local_path_hint: Some("C:\\machine-a\\engram".into()),
            ..empty_input()
        };
        assert_eq!(
            posix.identity_key().unwrap(),
            windows.identity_key().unwrap()
        );
    }

    fn empty_input() -> ArtifactInput {
        ArtifactInput {
            kind: ArtifactKind::File,
            locator: "src/lib.rs".into(),
            observed_revision: None,
            content_hash: None,
            repository_identity: None,
            git_ref: None,
            commit_id: None,
            tree_hash: None,
            dirty: None,
            local_path_hint: None,
            provenance: String::new(),
            delivery: DeliveryFacts::default(),
            verification: Vec::new(),
        }
    }
}
