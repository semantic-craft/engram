//! Registered-user domain types + validation.
//!
//! The storage shape lives in `engram-store`; this module owns the
//! **pure types** + **input validation** that both the writer (insert
//! path) and the CLI / admin endpoint (caller path) share. Keeping the
//! rules in core means the same `MemoryError::InvalidUsername`
//! materialises whether a username is rejected by the CLI, the HTTP
//! handler, or the writer actor — one source of truth.
//!
//! ## Attribution, not RBAC
//!
//! See [`crate::actor`] for the broader rationale: engram's data is
//! single-tenant by design. A `User` row records *who* a write came
//! from; it does not gate *whether* the write was allowed.
//!
//! ## Token storage (referenced here, implemented in `engram-store`)
//!
//! Tokens are 32 bytes of CSPRNG (256 bits of entropy). The store keeps
//! a single `SHA-256(token || ":" || pepper)` digest per user; the
//! per-server pepper from `[auth].token_pepper` makes a DB-only theft
//! useless to an offline attacker. SHA-256 gives an O(1) UNIQUE-index
//! lookup the per-request auth hot path needs. See
//! `engram_store::users` for the actual hash + generate helpers.

use serde::{Deserialize, Serialize};

use crate::{MemoryError, UserId};

/// Maximum username length. Anything longer is a misconfiguration; the
/// engine, CLI, and web UI all assume a small username fits in a single
/// terminal column / table cell.
pub const MAX_USERNAME_LEN: usize = 64;

/// Maximum email length per RFC 5321 §4.5.3.1.3 (path length).
pub const MAX_EMAIL_LEN: usize = 254;

/// A registered user as stored. `id` is the UUIDv7 primary key; the
/// other fields mirror the `users` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Stable identity. Used by `audit_log.author_id` and
    /// `pages.author_id` foreign keys.
    pub id: UserId,
    /// Validated username (see [`validate_username`]).
    pub username: String,
    /// Optional display name for "Last edited by Alice Smith" style UIs.
    pub name: Option<String>,
    /// Optional email; validated by [`validate_email`] before insert.
    pub email: Option<String>,
    /// Microseconds since epoch — V01 convention.
    pub created_at: i64,
    /// Microseconds since epoch; `None` until the first authenticated
    /// request from this user. Updated fire-and-forget per request.
    pub last_seen_at: Option<i64>,
    /// Microseconds since epoch; `None` means the token is active.
    /// `engram user expire` stamps it, `revive` clears it,
    /// `rotate-token` issues a fresh hash AND implicitly clears this
    /// field (rotating a token only makes sense to make it usable again).
    pub token_expired_at: Option<i64>,
}

impl User {
    /// `true` if the user's current token is usable. Auth middleware
    /// uses this to short-circuit the request before hitting any
    /// further state.
    #[must_use]
    pub fn is_token_active(&self) -> bool {
        self.token_expired_at.is_none()
    }
}

/// Pre-insert shape for a new user. Carries the inputs the caller
/// (CLI / admin endpoint) collected; the actual token generation +
/// hashing lives in `engram-store::users` since it owns the
/// per-server pepper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewUser {
    /// Required. Will be trimmed + validated; rejected if empty after
    /// trim, longer than [`MAX_USERNAME_LEN`], or contains whitespace /
    /// control characters / common separator chars.
    pub username: String,
    /// Optional display name. Trimmed; empty string is normalised to
    /// `None` so the DB doesn't store distinct `""` and `NULL`.
    pub name: Option<String>,
    /// Optional email. Trimmed + lowercased + validated by
    /// [`validate_email`]; empty string is normalised to `None`.
    pub email: Option<String>,
}

impl NewUser {
    /// In-place validate + normalise. Returns the same error kinds the
    /// CLI and admin endpoint surface to the operator.
    ///
    /// # Errors
    /// - [`MemoryError::InvalidUsername`] when the username is empty
    ///   after trim, too long, or contains forbidden characters.
    /// - [`MemoryError::InvalidEmail`] when an email is supplied but
    ///   fails the basic format check.
    pub fn validate(&mut self) -> Result<(), MemoryError> {
        self.username = self.username.trim().to_string();
        validate_username(&self.username)?;

        if let Some(name) = self.name.as_mut() {
            *name = name.trim().to_string();
        }
        if self.name.as_deref() == Some("") {
            self.name = None;
        }

        if let Some(email) = self.email.as_mut() {
            *email = email.trim().to_lowercase();
        }
        match self.email.as_deref() {
            Some("") => self.email = None,
            Some(e) => validate_email(e)?,
            None => {}
        }

        Ok(())
    }
}

/// Validate a username after trim. Rules — kept minimal per the v0.8
/// design call (no complex policies, just enough to keep weird
/// characters out):
///
/// - non-empty;
/// - at most [`MAX_USERNAME_LEN`] characters (code points, not bytes);
/// - no control characters (`is_control`);
/// - no whitespace anywhere (usernames are identifiers, not free text);
/// - no common separator / quoting characters that would make CLI
///   quoting + URL embedding painful: `/ \ : ; , " ' ` `.
///
/// UTF-8 letters / digits / `.` / `-` / `_` / `@` are allowed, so
/// emails-as-usernames (`alice@home`) work for operators who want them.
///
/// # Errors
/// Returns [`MemoryError::InvalidUsername`] on any rule violation.
pub fn validate_username(s: &str) -> Result<(), MemoryError> {
    if s.is_empty() {
        return Err(MemoryError::InvalidUsername("empty after trim".into()));
    }
    let len = s.chars().count();
    if len > MAX_USERNAME_LEN {
        return Err(MemoryError::InvalidUsername(format!(
            "{len} characters, exceeds max {MAX_USERNAME_LEN}"
        )));
    }
    for ch in s.chars() {
        if ch.is_control() {
            return Err(MemoryError::InvalidUsername(format!(
                "control character U+{:04X}",
                ch as u32
            )));
        }
        if ch.is_whitespace() {
            return Err(MemoryError::InvalidUsername(format!(
                "whitespace character {ch:?}"
            )));
        }
        if matches!(ch, '/' | '\\' | ':' | ';' | ',' | '"' | '\'' | '`') {
            return Err(MemoryError::InvalidUsername(format!(
                "separator character {ch:?}"
            )));
        }
    }
    Ok(())
}

/// Basic email-format check. Intentionally permissive — operators may
/// use intranet-style addresses (`alice@home`) without a public TLD, so
/// we don't require a dot in the domain. Rules:
///
/// - at most [`MAX_EMAIL_LEN`] characters;
/// - exactly one `@`;
/// - non-empty local + non-empty domain;
/// - no whitespace anywhere;
/// - no control characters.
///
/// This is the "doesn't look obviously wrong" check, not RFC 5322
/// compliance. Quoted local parts, IP-literal domains in brackets, and
/// other rare-but-valid forms are not supported.
///
/// # Errors
/// Returns [`MemoryError::InvalidEmail`] on any rule violation.
pub fn validate_email(s: &str) -> Result<(), MemoryError> {
    let len = s.chars().count();
    if len > MAX_EMAIL_LEN {
        return Err(MemoryError::InvalidEmail(format!(
            "{len} characters, exceeds max {MAX_EMAIL_LEN}"
        )));
    }
    for ch in s.chars() {
        if ch.is_control() {
            return Err(MemoryError::InvalidEmail(format!(
                "control character U+{:04X}",
                ch as u32
            )));
        }
        if ch.is_whitespace() {
            return Err(MemoryError::InvalidEmail(format!(
                "whitespace character {ch:?}"
            )));
        }
    }
    let mut parts = s.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return Err(MemoryError::InvalidEmail("multiple `@` characters".into()));
    }
    if local.is_empty() {
        return Err(MemoryError::InvalidEmail("empty local part".into()));
    }
    if domain.is_empty() {
        return Err(MemoryError::InvalidEmail("empty domain part".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── username validation ───────────────────────────────────────

    #[test]
    fn username_accepts_simple() {
        assert!(validate_username("alice").is_ok());
        assert!(validate_username("boss").is_ok());
        assert!(validate_username("user_1").is_ok());
        assert!(validate_username("a.b-c").is_ok());
    }

    #[test]
    fn username_accepts_email_style() {
        // Emails-as-usernames must work for operators who want them.
        assert!(validate_username("alice@home").is_ok());
        assert!(validate_username("boss@example.com").is_ok());
    }

    #[test]
    fn username_accepts_utf8_letters() {
        assert!(validate_username("bóss").is_ok());
        assert!(validate_username("ボス").is_ok());
    }

    #[test]
    fn username_rejects_empty() {
        assert!(validate_username("").is_err());
    }

    #[test]
    fn username_rejects_whitespace() {
        for s in ["boss man", "boss\tman", "boss\nman", " boss", "boss "] {
            assert!(
                validate_username(s).is_err(),
                "whitespace must be rejected: {s:?}"
            );
        }
    }

    #[test]
    fn username_rejects_control_chars() {
        assert!(validate_username("boss\x00").is_err());
        assert!(validate_username("boss\x07man").is_err());
        assert!(validate_username("boss\x7fman").is_err());
    }

    #[test]
    fn username_rejects_separator_chars() {
        for ch in ['/', '\\', ':', ';', ',', '"', '\'', '`'] {
            let s = format!("boss{ch}man");
            assert!(
                validate_username(&s).is_err(),
                "{ch:?} must be rejected: {s:?}"
            );
        }
    }

    #[test]
    fn username_rejects_over_length() {
        let too_long = "a".repeat(MAX_USERNAME_LEN + 1);
        assert!(validate_username(&too_long).is_err());
        let just_right = "a".repeat(MAX_USERNAME_LEN);
        assert!(validate_username(&just_right).is_ok());
    }

    // ── email validation ──────────────────────────────────────────

    #[test]
    fn email_accepts_typical() {
        assert!(validate_email("alice@example.com").is_ok());
        assert!(validate_email("a.b.c@subdomain.example.com").is_ok());
        assert!(validate_email("alice+filter@example.com").is_ok());
    }

    #[test]
    fn email_accepts_intranet_no_tld() {
        // Homelab users have `alice@home` setups; the basic check must
        // not require a dot in the domain.
        assert!(validate_email("alice@home").is_ok());
        assert!(validate_email("boss@localhost").is_ok());
    }

    #[test]
    fn email_rejects_missing_at() {
        assert!(validate_email("alice.example.com").is_err());
    }

    #[test]
    fn email_rejects_multiple_at() {
        assert!(validate_email("alice@@example.com").is_err());
        assert!(validate_email("a@b@c").is_err());
    }

    #[test]
    fn email_rejects_empty_parts() {
        assert!(validate_email("@example.com").is_err());
        assert!(validate_email("alice@").is_err());
        assert!(validate_email("@").is_err());
    }

    #[test]
    fn email_rejects_whitespace() {
        for s in ["alice @example.com", "alice@ example.com", " alice@x.y"] {
            assert!(
                validate_email(s).is_err(),
                "whitespace must be rejected: {s:?}"
            );
        }
    }

    #[test]
    fn email_rejects_control_chars() {
        assert!(validate_email("alice\x00@example.com").is_err());
    }

    #[test]
    fn email_rejects_over_length() {
        let local = "a".repeat(MAX_EMAIL_LEN);
        let too_long = format!("{local}@x");
        assert!(validate_email(&too_long).is_err());
    }

    // ── NewUser::validate normalisation ───────────────────────────

    #[test]
    fn new_user_trims_username() {
        let mut nu = NewUser {
            username: "  alice  ".into(),
            name: None,
            email: None,
        };
        nu.validate().unwrap();
        assert_eq!(nu.username, "alice");
    }

    #[test]
    fn new_user_normalises_empty_optionals_to_none() {
        let mut nu = NewUser {
            username: "alice".into(),
            name: Some("   ".into()),
            email: Some("   ".into()),
        };
        nu.validate().unwrap();
        assert_eq!(nu.name, None);
        assert_eq!(nu.email, None);
    }

    #[test]
    fn new_user_lowercases_email() {
        let mut nu = NewUser {
            username: "alice".into(),
            name: None,
            email: Some("Alice@Example.COM".into()),
        };
        nu.validate().unwrap();
        assert_eq!(nu.email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn new_user_trims_name() {
        let mut nu = NewUser {
            username: "alice".into(),
            name: Some("  Alice Smith  ".into()),
            email: None,
        };
        nu.validate().unwrap();
        // Internal whitespace is fine in a display name — only the
        // edges get trimmed.
        assert_eq!(nu.name.as_deref(), Some("Alice Smith"));
    }

    #[test]
    fn new_user_rejects_invalid_username() {
        let mut nu = NewUser {
            username: "boss man".into(),
            name: None,
            email: None,
        };
        assert!(matches!(
            nu.validate(),
            Err(MemoryError::InvalidUsername(_))
        ));
    }

    #[test]
    fn new_user_rejects_invalid_email() {
        let mut nu = NewUser {
            username: "alice".into(),
            name: None,
            email: Some("not-an-email".into()),
        };
        assert!(matches!(nu.validate(), Err(MemoryError::InvalidEmail(_))));
    }
}
