//! Privacy strip — redacts secrets before they reach durable storage.
//!
//! Two surfaces live here:
//!
//! - [`Sanitizer`] is a stateful, config-driven scrubber. Build it once
//!   at startup from `SanitizeConfig` and pass `Arc<Sanitizer>` to every
//!   subsystem that writes user-facing text to disk (hook ingress,
//!   consolidator, wiki writer, MCP `memory_handoff_begin`).
//! - [`Sanitized<T>`] is the typed boundary: the *only* way to construct
//!   one is through [`Sanitized::new`], which forces the caller to hand
//!   over an already-scrubbed value. Without this, you can't accidentally
//!   persist raw text by skipping the sanitizer.
//!
//! ## What we redact
//!
//! Built-in patterns cover bearer tokens, vendor-prefixed API keys
//! (Anthropic / OpenAI / OpenRouter sk-…, Stripe sk_live_…, GitHub PATs,
//! Google AIza…, Slack xoxb/xoxp…, AWS AKIA…), PEM-bracketed private
//! keys, URL-embedded credentials (`postgres://user:pass@host`), and
//! anything matching the generic `*_(KEY|TOKEN|SECRET|PASSWORD|
//! CREDENTIAL)=value` shape. Operators can extend the list via
//! `[sanitize].extra_patterns` and exempt substrings via
//! `[sanitize].allowlist` — the allowlist is checked *per match*, so a
//! pattern still runs but an allowlisted span survives unchanged.
//!
//! ## What we deliberately do not catch
//!
//! Standalone high-entropy strings (e.g. a 32-char random hex) cannot
//! be safely redacted without knowing their structure — too many false
//! positives. Operators who care about that level of paranoia should
//! add a custom pattern via `extra_patterns`.

use std::sync::Arc;

use regex::Regex;
use tracing::debug;

use crate::NewObservation;

/// Compile-time list of redaction patterns. Order is intentional:
/// more-specific patterns first. False positives are acceptable —
/// better to redact a stray hash than to leak a credential.
const BUILTIN_PATTERN_STRS: &[&str] = &[
    // Bearer-style tokens.
    r#"(?i)bearer\s+[A-Za-z0-9._\-+/=]{16,}"#,
    // Vendor-prefixed API keys.
    r"sk-[A-Za-z0-9_\-]{16,}",
    r"sk_live_[A-Za-z0-9_\-]{16,}",
    r"ghp_[A-Za-z0-9]{20,}",
    r"github_pat_[A-Za-z0-9_]{20,}",
    r"AKIA[0-9A-Z]{12,}",
    // Naked Google / Gemini API keys.
    r"AIza[A-Za-z0-9_\-]{30,}",
    // Slack tokens (bot/user/admin/app-level/refresh).
    r"xox[abprs]-[A-Za-z0-9\-]{10,}",
    r"xapp-[A-Za-z0-9\-]{10,}",
    // JWTs (three base64url segments separated by dots).
    r"eyJ[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}",
    // PEM private key blocks — multi-line, lazy match.
    r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    // URL-embedded credentials: scheme://user:pass@host.
    r"[a-zA-Z][a-zA-Z0-9+\-.]*://[^:/\s]+:[^@\s]+@[^\s]+",
    // Provider-specific env-var assignments (kept explicit for clarity
    // and so that bare `OPENAI_API_KEY=anything-at-all` still triggers
    // even without `sk-` shape).
    r#"(?i)(ANTHROPIC_API_KEY|OPENAI_API_KEY|OPENROUTER_API_KEY|VOYAGE_API_KEY|MISTRAL_API_KEY|GROQ_API_KEY|HF_TOKEN|HUGGINGFACE_TOKEN|AWS_(SECRET_)?ACCESS_KEY[A-Z_]*|GITHUB_TOKEN|GH_TOKEN|GITLAB_TOKEN|GOOGLE_API_KEY|GEMINI_API_KEY|OLLAMA_API_KEY)\s*[=:]\s*\S+"#,
    // Generic env-var catch-all: any *_KEY / *_TOKEN / *_SECRET /
    // *_PASSWORD / *_CREDENTIAL[S] / *_PRIVATE_KEY assignment.
    r#"(?i)\b[A-Z][A-Z0-9_]*_(KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|CREDENTIALS|PRIVATE_KEY)\s*[=:]\s*\S+"#,
    // Filesystem paths that commonly contain credentials.
    r"(?:/[^/\s]+)*/\.ssh(?:/[^\s]+)?",
    r"(?:/[^/\s]+)*/\.aws(?:/[^\s]+)?",
    r"(?:/[^/\s]+)*/\.kube(?:/[^\s]+)?",
    r"(?:/[^/\s]+)*/\.config/gcloud(?:/[^\s]+)?",
    r"(?:/[^/\s]+)*/\.gnupg(?:/[^\s]+)?",
];

/// Stateful sanitizer. Cheap to clone — wraps an `Arc` of compiled
/// patterns. Construct once at startup, then pass everywhere by clone.
#[derive(Clone)]
pub struct Sanitizer {
    inner: Arc<SanitizerInner>,
}

struct SanitizerInner {
    patterns: Vec<Regex>,
    allowlist: Vec<String>,
}

impl std::fmt::Debug for Sanitizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sanitizer")
            .field("patterns", &self.inner.patterns.len())
            .field("allowlist", &self.inner.allowlist.len())
            .finish()
    }
}

/// User-tunable sanitizer settings. Mirrors the `[sanitize]` section
/// of `config.toml`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SanitizeConfig {
    /// Additional regex patterns to redact. Compiled once at startup;
    /// invalid regex aborts startup with a clear error.
    pub extra_patterns: Vec<String>,
    /// Substrings that should *never* be redacted, even if a pattern
    /// matches them. Useful when a project codename collides with the
    /// generic env-var catch-all (e.g. "PROJECT_TOKEN" the user wants
    /// to keep visible).
    pub allowlist: Vec<String>,
}

impl Sanitizer {
    /// Build a sanitizer from built-in patterns plus the operator's
    /// extras. Returns an error if any extra pattern fails to compile.
    ///
    /// # Errors
    /// Returns [`regex::Error`] when an entry in `extra_patterns` is
    /// not a valid regex.
    pub fn new(cfg: &SanitizeConfig) -> Result<Self, regex::Error> {
        let mut patterns =
            Vec::with_capacity(BUILTIN_PATTERN_STRS.len() + cfg.extra_patterns.len());
        for p in BUILTIN_PATTERN_STRS {
            patterns.push(Regex::new(p)?);
        }
        for p in &cfg.extra_patterns {
            patterns.push(Regex::new(p)?);
        }
        Ok(Self {
            inner: Arc::new(SanitizerInner {
                patterns,
                allowlist: cfg.allowlist.clone(),
            }),
        })
    }

    /// Built-in-only sanitizer (no operator extras, no allowlist).
    /// Convenient for tests and zero-config callers.
    #[must_use]
    pub fn builtin() -> Self {
        Self::new(&SanitizeConfig::default()).expect("built-in patterns compile")
    }

    /// Scrub a single string. Each match is replaced with `[REDACTED]`
    /// unless the matched substring contains an allowlist entry, in
    /// which case it is left alone.
    #[must_use]
    pub fn scrub(&self, input: &str) -> String {
        let mut out = input.to_string();
        for re in &self.inner.patterns {
            out = re
                .replace_all(&out, |caps: &regex::Captures<'_>| {
                    let m = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
                    if self.inner.allowlist.iter().any(|a| m.contains(a)) {
                        m.to_string()
                    } else {
                        debug!(pattern = re.as_str(), "sanitize: redacted match");
                        "[REDACTED]".to_string()
                    }
                })
                .into_owned();
        }
        out
    }
}

impl Default for Sanitizer {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Marker carried by every value that has passed through the privacy
/// strip. The wrapped value is private and reachable only via
/// [`Sanitized::inner`] / [`Sanitized::into_inner`].
#[derive(Debug, Clone)]
pub struct Sanitized<T>(T);

impl<T> Sanitized<T> {
    /// Borrow the inner sanitized value.
    pub fn inner(&self) -> &T {
        &self.0
    }
    /// Consume and return the inner sanitized value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl Sanitized<NewObservation> {
    /// Apply the privacy strip to an observation's title + body.
    #[must_use]
    pub fn new(mut obs: NewObservation, sanitizer: &Sanitizer) -> Self {
        obs.title = sanitizer.scrub(&obs.title);
        obs.body = sanitizer.scrub(&obs.body);
        Self(obs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObservationKind, ProjectId, SessionId, WorkspaceId};

    fn s() -> Sanitizer {
        Sanitizer::builtin()
    }

    #[test]
    fn scrubs_bearer_token() {
        let out = s().scrub("Authorization: Bearer abcdef0123456789ABCDEF0123456789");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("abcdef0123"));
    }

    #[test]
    fn scrubs_openrouter_key_via_sk_prefix() {
        let out = s().scrub("key=sk-or-v1-deadbeefcafebabe1234567890abcdef");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("deadbeef"));
    }

    #[test]
    fn scrubs_naked_google_api_key() {
        // Fixture is the AIzaSy… shape with random hex padding, NOT a
        // real key. A previous iteration of this file used a live
        // value as the fixture; do not do that — automated scanners
        // (GitGuardian, Google's own) will pick it up and you'll
        // spend an hour rotating credentials.
        let out = s().scrub("the key AIzaSy0123456789abcdefghijklmnopqrstuvwx is leaked");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("AIzaSy"));
    }

    #[test]
    fn scrubs_slack_bot_token() {
        let out = s().scrub("slack=xoxb-1234567890-abcdefghij");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("xoxb-1234"));
    }

    #[test]
    fn scrubs_pem_private_key_block() {
        let pem = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let out = s().scrub(pem);
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("b3BlbnNz"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn scrubs_url_embedded_credentials() {
        let out = s().scrub("connect to postgres://admin:hunter2@db.internal/prod");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn scrubs_generic_env_var_assignments() {
        let out = s().scrub("MY_INTERNAL_API_KEY=abcdef123456");
        assert!(out.contains("[REDACTED]"));
        let out2 = s().scrub("SOMETHING_SECRET=foo");
        assert!(out2.contains("[REDACTED]"));
        let out3 = s().scrub("DB_PASSWORD=hunter2");
        assert!(out3.contains("[REDACTED]"));
    }

    #[test]
    fn scrubs_cloud_credential_paths() {
        let out = s().scrub("read /home/user/.aws/credentials");
        assert!(out.contains("[REDACTED]"));
        let out2 = s().scrub("set KUBECONFIG=/home/user/.kube/config");
        assert!(out2.contains("[REDACTED]"));
    }

    #[test]
    fn observation_round_trip() {
        let raw = NewObservation {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "OPENAI_API_KEY=sk-leak-1234567890abcdef".into(),
            body: "see /home/user/.ssh/id_ed25519".into(),
            importance: 5,
        };
        let scrubbed = Sanitized::new(raw, &s()).into_inner();
        assert!(scrubbed.title.contains("[REDACTED]"));
        assert!(scrubbed.body.contains("[REDACTED]"));
    }

    #[test]
    fn extra_patterns_compile_and_apply() {
        let cfg = SanitizeConfig {
            extra_patterns: vec![r"CANARY-[0-9]+".to_string()],
            allowlist: vec![],
        };
        let sn = Sanitizer::new(&cfg).unwrap();
        let out = sn.scrub("found CANARY-42 here");
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn invalid_extra_pattern_errors() {
        let cfg = SanitizeConfig {
            extra_patterns: vec!["[unterminated".to_string()],
            allowlist: vec![],
        };
        assert!(Sanitizer::new(&cfg).is_err());
    }

    #[test]
    fn allowlist_substring_survives_redaction() {
        // The generic env-var pattern would match this, but the
        // allowlist exempts spans containing "PROJECT_TOKEN".
        let cfg = SanitizeConfig {
            extra_patterns: vec![],
            allowlist: vec!["PROJECT_TOKEN_PUBLIC".to_string()],
        };
        let sn = Sanitizer::new(&cfg).unwrap();
        let out = sn.scrub("we use PROJECT_TOKEN_PUBLIC=abc internally");
        assert!(
            out.contains("PROJECT_TOKEN_PUBLIC"),
            "allowlist span should survive; got: {out}"
        );
    }
}
