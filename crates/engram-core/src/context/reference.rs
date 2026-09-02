use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ObservationId, PageId, PagePath};

const CONTEXT_REF_PREFIX: &str = "engram-context-v1.";

/// Canonical context sources currently produced by memory queries.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    /// A normal canonical wiki page.
    WikiPage,
    /// A canonical summary page under `sessions/`.
    SessionPage,
    /// An immutable lifecycle observation.
    Observation,
}

/// Stable, scoped, revisioned source reference.
#[derive(Clone, Debug, PartialEq, Eq, Hash, schemars::JsonSchema)]
#[schemars(with = "String")]
pub struct ContextRef {
    workspace: String,
    project: String,
    locator: ContextLocator,
}

#[derive(Serialize, Deserialize)]
struct ContextRefPayload {
    workspace: String,
    project: String,
    #[serde(flatten)]
    locator: ContextLocator,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ContextLocator {
    WikiPage { path: PagePath, revision: PageId },
    SessionPage { path: PagePath, revision: PageId },
    Observation { id: ObservationId },
}

/// Invalid stable context reference.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContextRefError {
    /// A required coordinate was empty.
    #[error("context reference {0} cannot be empty")]
    Empty(&'static str),
    /// The wire prefix is not supported.
    #[error("unsupported context reference version")]
    UnsupportedVersion,
    /// The URL-safe payload could not be decoded.
    #[error("invalid context reference encoding")]
    InvalidEncoding,
    /// The decoded payload is malformed.
    #[error("invalid context reference payload")]
    InvalidPayload,
}

impl ContextRef {
    fn with_locator(
        workspace: impl Into<String>,
        project: impl Into<String>,
        locator: ContextLocator,
    ) -> Result<Self, ContextRefError> {
        match &locator {
            ContextLocator::WikiPage { path, .. } if path.as_str().starts_with("sessions/") => {
                return Err(ContextRefError::InvalidPayload);
            }
            ContextLocator::SessionPage { path, .. } if !path.as_str().starts_with("sessions/") => {
                return Err(ContextRefError::InvalidPayload);
            }
            _ => {}
        }
        let value = Self {
            workspace: workspace.into(),
            project: project.into(),
            locator,
        };
        for (label, coordinate) in [
            ("workspace", value.workspace.as_str()),
            ("project", value.project.as_str()),
        ] {
            if coordinate.trim().is_empty() {
                return Err(ContextRefError::Empty(label));
            }
        }
        Ok(value)
    }

    /// Construct a typed wiki or session-page reference.
    pub fn page(
        workspace: impl Into<String>,
        project: impl Into<String>,
        path: PagePath,
        revision: PageId,
    ) -> Result<Self, ContextRefError> {
        let locator = if path.as_str().starts_with("sessions/") {
            ContextLocator::SessionPage { path, revision }
        } else {
            ContextLocator::WikiPage { path, revision }
        };
        Self::with_locator(workspace, project, locator)
    }

    /// Construct a typed immutable-observation reference.
    pub fn observation(
        workspace: impl Into<String>,
        project: impl Into<String>,
        id: ObservationId,
    ) -> Result<Self, ContextRefError> {
        Self::with_locator(workspace, project, ContextLocator::Observation { id })
    }

    /// Source kind.
    #[must_use]
    pub const fn kind(&self) -> ContextKind {
        match &self.locator {
            ContextLocator::WikiPage { .. } => ContextKind::WikiPage,
            ContextLocator::SessionPage { .. } => ContextKind::SessionPage,
            ContextLocator::Observation { .. } => ContextKind::Observation,
        }
    }

    /// Workspace coordinate.
    #[must_use]
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// Project coordinate.
    #[must_use]
    pub fn project(&self) -> &str {
        &self.project
    }

    /// Relative page path, when this reference names a page.
    #[must_use]
    pub const fn page_path(&self) -> Option<&PagePath> {
        match &self.locator {
            ContextLocator::WikiPage { path, .. } | ContextLocator::SessionPage { path, .. } => {
                Some(path)
            }
            ContextLocator::Observation { .. } => None,
        }
    }

    /// Exact page revision, when this reference names a page.
    #[must_use]
    pub const fn page_revision(&self) -> Option<PageId> {
        match &self.locator {
            ContextLocator::WikiPage { revision, .. }
            | ContextLocator::SessionPage { revision, .. } => Some(*revision),
            ContextLocator::Observation { .. } => None,
        }
    }

    /// Immutable observation identity and revision.
    #[must_use]
    pub const fn observation_id(&self) -> Option<ObservationId> {
        match &self.locator {
            ContextLocator::Observation { id } => Some(*id),
            ContextLocator::WikiPage { .. } | ContextLocator::SessionPage { .. } => None,
        }
    }

    /// Exact source revision in public wire form.
    #[must_use]
    pub fn source_revision(&self) -> String {
        match &self.locator {
            ContextLocator::WikiPage { revision, .. }
            | ContextLocator::SessionPage { revision, .. } => revision.to_string(),
            ContextLocator::Observation { id } => id.to_string(),
        }
    }
}

impl fmt::Display for ContextRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let payload = ContextRefPayload {
            workspace: self.workspace.clone(),
            project: self.project.clone(),
            locator: self.locator.clone(),
        };
        let bytes = serde_json::to_vec(&payload).map_err(|_| fmt::Error)?;
        write!(f, "{CONTEXT_REF_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
    }
}

impl PartialOrd for ContextRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ContextRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.workspace
            .cmp(&other.workspace)
            .then_with(|| self.project.cmp(&other.project))
            .then_with(|| self.kind().cmp(&other.kind()))
            .then_with(|| self.page_path().cmp(&other.page_path()))
            .then_with(|| self.source_revision().cmp(&other.source_revision()))
    }
}

impl FromStr for ContextRef {
    type Err = ContextRefError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let encoded = raw
            .strip_prefix(CONTEXT_REF_PREFIX)
            .ok_or(ContextRefError::UnsupportedVersion)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| ContextRefError::InvalidEncoding)?;
        let payload: ContextRefPayload =
            serde_json::from_slice(&bytes).map_err(|_| ContextRefError::InvalidPayload)?;
        Self::with_locator(payload.workspace, payload.project, payload.locator)
    }
}

impl Serialize for ContextRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ContextRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_ref_round_trips_without_exposing_coordinates() {
        let original = ContextRef::page(
            "research workspace",
            "paper/one",
            PagePath::new("notes/a b.md").unwrap(),
            PageId::from_str("019e0000-0000-7000-8000-000000000001").unwrap(),
        )
        .unwrap();
        let wire = original.to_string();
        assert!(wire.starts_with(CONTEXT_REF_PREFIX));
        assert!(!wire.contains("notes/a b.md"));
        assert!(!wire.contains("/Users/"));
        assert_eq!(wire.parse::<ContextRef>().unwrap(), original);
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(serde_json::from_str::<ContextRef>(&json).unwrap(), original);
    }
}
