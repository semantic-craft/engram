//! Strongly-typed identifiers for the domain.
//!
//! The 3-tuple ([`WorkspaceId`], [`ProjectId`], [`PagePath`]) is the universal
//! identity coordinate for any memory. It is baked in from M0 even though v1
//! ships single-workspace, so we never inherit basic-memory's v0.20 retrofit
//! pain (issues #783, #834, #802 and friends — see
//! `docs/issues-basic-memory.md`).

use std::fmt;
use std::path::Component;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::MemoryError;

macro_rules! id_newtype {
    ($vis:vis $name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        $vis struct $name(pub Uuid);

        impl $name {
            /// Generate a fresh v7 (time-ordered) identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Borrow the raw 16-byte big-endian representation.
            #[must_use]
            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }

            /// Reconstruct from a 16-byte slice (typically read from
            /// SQLite as a `BLOB`).
            ///
            /// # Errors
            /// Returns [`MemoryError::MalformedRecord`] if the slice is
            /// not exactly 16 bytes.
            pub fn from_slice(bytes: &[u8]) -> Result<Self, MemoryError> {
                Uuid::from_slice(bytes)
                    .map(Self)
                    .map_err(|e| MemoryError::MalformedRecord(format!("invalid uuid bytes: {e}")))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = MemoryError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::from_str(s)
                    .map(Self)
                    .map_err(|e| MemoryError::MalformedRecord(format!("invalid uuid: {e}")))
            }
        }
    };
}

id_newtype!(pub WorkspaceId, "Workspace identifier (top of the 3-tuple).");
id_newtype!(pub ProjectId, "Project identifier (middle of the 3-tuple).");
id_newtype!(pub SessionId, "Identifier for a single agent run.");
id_newtype!(pub ObservationId, "Identifier for a single observation captured during a session.");
id_newtype!(pub PageId, "Identifier for a single wiki page version.");
id_newtype!(pub HandoffId, "Identifier for a cross-agent handoff record.");
id_newtype!(pub UserId, "Identifier for a registered user (multi-user attribution; see [`crate::actor`]).");
id_newtype!(pub AutoImproveRunId, "Identifier for one auto-improvement review run.");
id_newtype!(pub AutoImproveProposalId, "Identifier for one staged auto-improvement proposal.");

/// Relative path of a page within the wiki tree.
///
/// Always uses `/` as the separator (POSIX-style), normalised on construction.
/// Never starts with a slash; never contains `..` or `.` components. This
/// invariant lets the store treat paths as flat keys without re-validating.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PagePath(String);

impl PagePath {
    /// Construct from a raw string. Rejects empty, absolute, Windows-prefix,
    /// backslash-separated, and dot-segment paths.
    ///
    /// # Errors
    /// Returns [`MemoryError::InvalidPagePath`] when the input is empty or
    /// contains a path component that would escape or alias the wiki root.
    pub fn new(raw: impl Into<String>) -> Result<Self, MemoryError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(MemoryError::InvalidPagePath("empty path".into()));
        }
        if raw.starts_with('/') {
            return Err(MemoryError::InvalidPagePath(format!(
                "leading slash: {raw}"
            )));
        }
        if raw.len() >= 3 {
            let bytes = raw.as_bytes();
            if bytes[1] == b':' && bytes[2] == b'/' && bytes[0].is_ascii_alphabetic() {
                return Err(MemoryError::InvalidPagePath(format!(
                    "windows drive prefix in {raw}"
                )));
            }
        }
        if raw.contains('\\') {
            return Err(MemoryError::InvalidPagePath(format!(
                "backslash separator in {raw}"
            )));
        }
        for component in std::path::Path::new(&raw).components() {
            match component {
                Component::Normal(_) => {}
                Component::CurDir | Component::ParentDir => {
                    return Err(MemoryError::InvalidPagePath(format!(
                        "dot segment in {raw}"
                    )));
                }
                Component::Prefix(_) | Component::RootDir => {
                    return Err(MemoryError::InvalidPagePath(format!(
                        "absolute path in {raw}"
                    )));
                }
            }
        }
        for segment in raw.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(MemoryError::InvalidPagePath(format!(
                    "invalid segment in {raw}"
                )));
            }
        }
        Ok(Self(raw))
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PagePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PagePath").field(&self.0).finish()
    }
}

impl fmt::Display for PagePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Discriminator for the agent CLI that captured an observation or handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    /// Anthropic Claude Code CLI.
    ClaudeCode,
    /// OpenAI Codex CLI.
    Codex,
    /// OpenCode (open-source coding agent).
    OpenCode,
    /// Cursor IDE agent.
    Cursor,
    /// Google Gemini CLI.
    GeminiCli,
    /// Anthropic Claude Desktop.
    ClaudeDesktop,
    /// OpenClaw personal AI gateway.
    #[serde(rename = "openclaw", alias = "open-claw")]
    OpenClaw,
    /// Google Antigravity CLI (`agy`).
    AntigravityCli,
    /// Oh My Pi (`omp`) coding agent.
    Omp,
    /// Pi coding agent.
    #[serde(rename = "pi")]
    Pi,
    /// xAI Grok Build CLI (`grok`).
    Grok,
    /// Anything else (manual capture, future agents).
    Other,
}

impl AgentKind {
    /// Kebab-case wire string matching the serde representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::OpenCode => "open-code",
            Self::Cursor => "cursor",
            Self::GeminiCli => "gemini-cli",
            Self::ClaudeDesktop => "claude-desktop",
            Self::OpenClaw => "openclaw",
            Self::AntigravityCli => "antigravity-cli",
            Self::Omp => "omp",
            Self::Pi => "pi",
            Self::Grok => "grok",
            Self::Other => "other",
        }
    }

    /// Parse a known wire string or common alias. Unknown values map
    /// to [`AgentKind::Other`], which keeps hook ingestion tolerant of
    /// typos or future agents.
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            "claude-code" | "claude_code" | "claude" => Self::ClaudeCode,
            "codex" => Self::Codex,
            "open-code" | "opencode" => Self::OpenCode,
            "cursor" => Self::Cursor,
            "gemini-cli" | "gemini" => Self::GeminiCli,
            "claude-desktop" | "claude_desktop" => Self::ClaudeDesktop,
            "openclaw" | "open-claw" => Self::OpenClaw,
            "antigravity-cli" | "antigravity" | "agy" => Self::AntigravityCli,
            "pi" => Self::Pi,
            "omp" | "oh-my-pi" => Self::Omp,
            "grok" => Self::Grok,
            _ => Self::Other,
        }
    }

    /// Whether this agent injects the native `session-start` hook's stdout
    /// into the resuming session as context. Agents that consume it return
    /// `true` (Claude Code reads `hookSpecificOutput.additionalContext`).
    ///
    /// Grok ignores hook stdout on `SessionStart` (per Grok's hooks docs:
    /// "For events like SessionStart or PostToolUse, stdout is ignored"), so
    /// the native hook must NOT fetch the handoff for it: the fetch is
    /// **destructive** (the server marks the handoff accepted) and the result
    /// would be discarded — silently losing the handoff. For such agents the
    /// handoff stays available on demand via the MCP `memory_handoff_accept`
    /// tool. Unknown future agents return `false` until we know their
    /// SessionStart stdout semantics; accepting a handoff is single-use and
    /// should fail safe.
    #[must_use]
    pub fn session_start_injects_handoff(self) -> bool {
        !matches!(self, Self::Grok | Self::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_path_accepts_simple() {
        assert_eq!(PagePath::new("foo/bar.md").unwrap().as_str(), "foo/bar.md");
    }

    #[test]
    fn page_path_rejects_empty() {
        assert!(PagePath::new("").is_err());
    }

    #[test]
    fn page_path_rejects_leading_slash() {
        assert!(PagePath::new("/foo").is_err());
    }

    #[test]
    fn page_path_rejects_dot_segments() {
        assert!(PagePath::new("a/./b").is_err());
        assert!(PagePath::new("a/../b").is_err());
    }

    #[test]
    fn agent_kind_grok_round_trips() {
        assert_eq!(AgentKind::Grok.as_str(), "grok");
        assert_eq!(AgentKind::from_wire("grok"), AgentKind::Grok);
        // serde uses rename_all = "kebab-case" → "grok".
        assert_eq!(serde_json::to_string(&AgentKind::Grok).unwrap(), "\"grok\"");
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"grok\"").unwrap(),
            AgentKind::Grok
        );
        // Unknown tags still degrade to Other.
        assert_eq!(AgentKind::from_wire("grok-2"), AgentKind::Other);
        // Grok cannot inject the session-start handoff (ignores hook stdout);
        // every other agent can.
        assert!(!AgentKind::Grok.session_start_injects_handoff());
        assert!(AgentKind::ClaudeCode.session_start_injects_handoff());
        assert!(AgentKind::Codex.session_start_injects_handoff());
        assert!(!AgentKind::Other.session_start_injects_handoff());
    }

    #[test]
    fn page_path_rejects_backslashes_and_windows_prefixes() {
        assert!(PagePath::new(r"notes\secret.md").is_err());
        assert!(PagePath::new(r"C:\Users\me\secret.md").is_err());
        assert!(PagePath::new("C:/Users/me/secret.md").is_err());
    }

    #[test]
    fn ids_are_unique() {
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn id_round_trips_through_string() {
        let id = SessionId::new();
        let s = id.to_string();
        let parsed: SessionId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn id_round_trips_through_bytes() {
        let id = ProjectId::new();
        assert_eq!(ProjectId::from_slice(id.as_bytes()).unwrap(), id);
    }

    #[test]
    fn id_rejects_malformed_byte_lengths() {
        assert!(PageId::from_slice(&[0_u8; 15]).is_err());
        assert!(PageId::from_slice(&[0_u8; 17]).is_err());
    }

    #[test]
    fn agent_kind_serde() {
        let k = AgentKind::ClaudeCode;
        let s = serde_json::to_string(&k).unwrap();
        assert_eq!(s, "\"claude-code\"");
        let back: AgentKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, k);

        let omp = serde_json::to_string(&AgentKind::Omp).unwrap();
        assert_eq!(omp, "\"omp\"");
        assert_eq!(AgentKind::from_wire("omp"), AgentKind::Omp);
        assert_eq!(AgentKind::from_wire("oh-my-pi"), AgentKind::Omp);

        let pi = serde_json::to_string(&AgentKind::Pi).unwrap();
        assert_eq!(pi, "\"pi\"");
        assert_eq!(AgentKind::Pi.as_str(), "pi");
        assert_eq!(AgentKind::from_wire("pi"), AgentKind::Pi);
        assert_eq!(
            serde_json::from_str::<AgentKind>(&pi).unwrap(),
            AgentKind::Pi
        );

        let openclaw = serde_json::to_string(&AgentKind::OpenClaw).unwrap();
        assert_eq!(openclaw, "\"openclaw\"");
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"open-claw\"").unwrap(),
            AgentKind::OpenClaw
        );

        let antigravity = serde_json::to_string(&AgentKind::AntigravityCli).unwrap();
        assert_eq!(antigravity, "\"antigravity-cli\"");
    }
}
