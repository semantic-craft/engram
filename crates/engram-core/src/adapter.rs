//! Agent Adapter contract for lifecycle-driven task continuity.
//!
//! An **Agent Adapter** is the thin, harness-specific layer that translates one
//! coding agent's lifecycle events into engram's shared continuity protocol. The
//! adapter's *only* jobs are (a) naming the harness, (b) reporting the identity
//! dimensions the harness actually knows, and (c) declaring whether that harness
//! delivers a SessionStart hook's output into the resuming model context.
//!
//! Everything else — which Handoff is eligible, whether the claim succeeds, what
//! the ContextPackage contains, which scope and actor apply — is shared server
//! semantics and lives outside the adapter. Adapter-specific code therefore
//! cannot change claim, lease, revision, acknowledgement, or ContextPackage
//! selection semantics; the worst an adapter can do is decline to participate.
//!
//! ## Why delivery capability is part of the contract
//!
//! Automatic SessionStart recovery *claims* a Handoff (the same compare-and-set
//! path as the on-demand MCP claim). A claim is a mutation. Performing it for a
//! harness that then throws the rendered continuation away would consume a
//! transfer nobody read — the receiving Run would never see the claim id it
//! needs to acknowledge, and the work would sit under a live lease until it
//! expired. So a harness that ignores SessionStart output, or whose behaviour is
//! not established, must perform **no** automatic Handoff read or mutation at
//! all and leave the transfer open for an explicit on-demand claim.
//!
//! ## Identity dimensions stay distinct
//!
//! [`AdapterRequest`] keeps the authenticated `actor` separate from the
//! execution `agent`, the receiving `run`, and every *hint* ([`AdapterRequest::work_item`],
//! [`AdapterRequest::cwd`], [`AdapterPeer`]). Hints narrow selection inside an
//! already-resolved scope. They never grant authorization and never substitute
//! for actor identity.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentKind, SessionId, WorkItemId};
use crate::observation::ObservationKind;

/// What a harness does with a SessionStart hook's output.
///
/// This is the single capability that decides whether automatic recovery may
/// claim. It is deliberately three-valued: "we know it works", "we know it does
/// not", and "we have not established it" are different facts, and only the
/// first one is safe to mutate on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionStartDelivery {
    /// The harness injects SessionStart hook output into the resuming session's
    /// model context. Verified against the harness's documented hook contract.
    Injected,
    /// The harness runs SessionStart hooks but discards their output.
    Ignored,
    /// Delivery is not established for this harness — it has no SessionStart
    /// lifecycle surface, or its output semantics are unknown. Treated exactly
    /// like [`Self::Ignored`] for every destructive decision.
    Unknown,
}

impl SessionStartDelivery {
    /// Stable wire/audit string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Injected => "injected",
            Self::Ignored => "ignored",
            Self::Unknown => "unknown",
        }
    }

    /// Whether rendered SessionStart output actually reaches the model.
    #[must_use]
    pub const fn delivers_output(self) -> bool {
        matches!(self, Self::Injected)
    }
}

/// The shared, server-side Adapter capability table.
///
/// Promoted here from per-harness shell-script comments so one typed source
/// decides what every delivery path (POSIX hook, PowerShell hook, native
/// `engram hook`, generated TypeScript plugin) is allowed to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterContract {
    agent: AgentKind,
    session_start: SessionStartDelivery,
}

impl AdapterContract {
    /// Capability table. Every classification below is drawn from the harness's
    /// own hook documentation:
    ///
    /// | Adapter | SessionStart output | Automatic claim |
    /// |---|---|---|
    /// | `claude-code` | injected (`hookSpecificOutput.additionalContext`) | yes |
    /// | `codex` | injected (session-start stdout prepended) | yes |
    /// | `cursor` | injected (session-start stdout prepended) | yes |
    /// | `gemini-cli` | injected (session-start stdout prepended) | yes |
    /// | `open-code` | injected (`experimental.chat.system.transform`) | yes |
    /// | `antigravity-cli` | injected (`injectSteps[].ephemeralMessage`) | yes |
    /// | `openclaw` | injected (`prependContext`) | yes |
    /// | `omp` / `pi` | injected (`before_agent_start` message) | yes |
    /// | `grok` | **ignored** — "For events like SessionStart or PostToolUse, stdout is ignored" | no |
    /// | `claude-desktop` | **unknown** — no lifecycle hook surface | no |
    /// | anything else | **unknown** | no |
    #[must_use]
    pub const fn for_agent(agent: AgentKind) -> Self {
        let session_start = match agent {
            AgentKind::ClaudeCode
            | AgentKind::Codex
            | AgentKind::Cursor
            | AgentKind::GeminiCli
            | AgentKind::OpenCode
            | AgentKind::AntigravityCli
            | AgentKind::OpenClaw
            | AgentKind::Omp
            | AgentKind::Pi => SessionStartDelivery::Injected,
            // Grok's hook docs state SessionStart stdout is ignored.
            AgentKind::Grok => SessionStartDelivery::Ignored,
            // Claude Desktop is an MCP client with no lifecycle hooks, and
            // `Other` is by definition an unestablished harness.
            AgentKind::ClaudeDesktop | AgentKind::Other => SessionStartDelivery::Unknown,
        };
        Self {
            agent,
            session_start,
        }
    }

    /// Normalize a wire agent string into a contract. Unknown strings collapse
    /// to [`AgentKind::Other`], whose contract is non-destructive.
    #[must_use]
    pub fn from_wire(agent: Option<&str>) -> Self {
        Self::for_agent(agent.map_or(AgentKind::Other, AgentKind::from_wire))
    }

    /// Normalized harness/agent kind.
    #[must_use]
    pub const fn agent(&self) -> AgentKind {
        self.agent
    }

    /// SessionStart output-delivery capability.
    #[must_use]
    pub const fn session_start_delivery(&self) -> SessionStartDelivery {
        self.session_start
    }

    /// Whether automatic SessionStart recovery may read and claim a Handoff.
    ///
    /// False for every harness that discards SessionStart output and for every
    /// harness whose delivery is not established — those perform no automatic
    /// Handoff read or mutation and leave the transfer open for an explicit
    /// on-demand `memory_handoff_claim`.
    #[must_use]
    pub const fn may_claim_on_session_start(&self) -> bool {
        self.session_start.delivers_output()
    }
}

/// Audit label for a claim recorded through the on-demand MCP tool surface
/// rather than automatic SessionStart recovery.
#[must_use]
pub fn on_demand_delivery_path(agent: AgentKind) -> String {
    format!("{}:on-demand", agent.as_str())
}

/// The counterpart side of a transfer, as the receiving Adapter sees it.
///
/// Both fields are provenance/routing hints carried by the Handoff itself.
/// Neither grants authorization, and neither substitutes for
/// [`AdapterRequest::actor`]: a transfer addressed to one Adapter is still
/// claimable by another, and the claim is always recorded against the
/// authenticated actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterPeer {
    /// Adapter that published the transfer.
    pub source_agent: AgentKind,
    /// Advisory target selector the publisher attached, when any.
    pub target_agent: Option<AgentKind>,
}

/// One normalized lifecycle request from an Agent Adapter.
///
/// Constructed once per delivery path so no adapter re-derives identity on its
/// own. Every field is either an authenticated identity or an explicitly
/// advisory hint; the doc comments say which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterRequest {
    /// Harness capability table for this adapter.
    pub contract: AdapterContract,
    /// Authenticated actor key (`user:…` / `sub:…` / `client:…` / `anonymous`).
    /// The **only** authorization identity. Never derived from `contract`,
    /// `work_item`, `cwd`, or a peer target selector.
    pub actor: String,
    /// Actual receiving Run/Session identity, when the harness reports one.
    /// `None` means the adapter could not name the Run it is starting, which
    /// makes an automatic claim impossible to bind correctly.
    pub run: Option<SessionId>,
    /// Optional WorkItem selection hint (e.g. a checkout pinned to one task via
    /// its `.engram.toml`). Narrows selection *inside* the already-resolved
    /// workspace/project; it can never widen scope or authorize anything.
    pub work_item: Option<WorkItemId>,
    /// Canonical lifecycle event that produced this request.
    pub event: ObservationKind,
    /// Local routing hint. Never the identity of the task.
    pub cwd: Option<String>,
}

impl AdapterRequest {
    /// Whether this request may perform an automatic claim.
    ///
    /// Requires both a delivery-capable harness *and* a real receiving Run: a
    /// claim that cannot be bound to the Run that will write the acknowledging
    /// Checkpoint is worse than no claim at all.
    #[must_use]
    pub fn may_claim(&self) -> bool {
        self.contract.may_claim_on_session_start() && self.run.is_some()
    }

    /// Audit-safe delivery-path label: which adapter, and what it is allowed to
    /// do with SessionStart output. Contains no claim secrets.
    #[must_use]
    pub fn delivery_path(&self) -> String {
        format!(
            "{}:{}",
            self.contract.agent().as_str(),
            self.contract.session_start_delivery().as_str()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability matrix itself. Getting one row wrong is silent and
    /// expensive — a claim for a harness that discards the render consumes a
    /// transfer nobody reads — so pin every row, and pin that a Run is still
    /// required before any of them may claim.
    #[test]
    fn capability_matrix_gates_automatic_claiming() {
        for agent in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::Cursor,
            AgentKind::GeminiCli,
            AgentKind::OpenCode,
            AgentKind::AntigravityCli,
            AgentKind::OpenClaw,
            AgentKind::Omp,
            AgentKind::Pi,
        ] {
            let contract = AdapterContract::for_agent(agent);
            assert_eq!(
                contract.session_start_delivery(),
                SessionStartDelivery::Injected,
                "{} delivers session-start output",
                agent.as_str()
            );
            assert!(contract.may_claim_on_session_start());
        }
        assert_eq!(
            AdapterContract::for_agent(AgentKind::Grok).session_start_delivery(),
            SessionStartDelivery::Ignored
        );
        for contract in [
            AdapterContract::for_agent(AgentKind::Grok),
            AdapterContract::for_agent(AgentKind::ClaudeDesktop),
            AdapterContract::from_wire(Some("some-future-cli")),
            AdapterContract::from_wire(None),
        ] {
            assert!(
                !contract.may_claim_on_session_start(),
                "{:?} must default to non-destructive",
                contract.agent()
            );
        }

        // Delivery capability alone is not enough: an adapter that cannot name
        // the Run it is starting would create a lease nobody can acknowledge.
        let request = AdapterRequest {
            contract: AdapterContract::for_agent(AgentKind::ClaudeCode),
            actor: "user:alice".into(),
            run: None,
            work_item: None,
            event: ObservationKind::SessionStart,
            cwd: Some("/repo".into()),
        };
        assert!(!request.may_claim());
        assert_eq!(request.delivery_path(), "claude-code:injected");
        assert!(
            AdapterRequest {
                run: Some(SessionId::new()),
                ..request
            }
            .may_claim()
        );
    }
}
