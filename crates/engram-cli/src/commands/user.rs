//! `engram user` — manage registered users for multi-user attribution.
//!
//! Thin HTTP client over the admin endpoints in
//! `engram-mcp::admin::handle_{create,list,expire,revive,
//! rotate_token}_user`. Per invariant #16 (CLI is always a thin HTTP
//! client) the CLI never opens the store; everything routes through
//! `/admin/users/*`. The caller's bearer token must authenticate as
//! root or the server returns 403 (User tier) / 401 (Anonymous).
//!
//! Backward compatibility: every install that predates v0.8 lacks the
//! `[auth].token_pepper` field. The server's user-management routes
//! 503 in that case with `multi-user not enabled (set
//! [auth].token_pepper in config or run `engram init`)`. Existing
//! single-user installs see that error only if they actively try to
//! use these subcommands — the rest of the CLI keeps working.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{
    UserAddArgs, UserArgs, UserCommand, UserExpireArgs, UserListArgs, UserReviveArgs,
    UserRotateTokenArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json};

/// Mirrors `engram_core::User` on the server side. Repeated here
/// rather than imported to keep the CLI <-> server contract explicit
/// at the deserialisation boundary (the CLI tolerates a future server
/// that adds fields without a recompile).
#[derive(Debug, Deserialize, Serialize)]
struct UserRow {
    id: String,
    username: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    created_at: i64,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    token_expired_at: Option<i64>,
}

impl UserRow {
    fn is_token_active(&self) -> bool {
        self.token_expired_at.is_none()
    }
}

#[derive(Debug, Deserialize)]
struct UserWithToken {
    user: UserRow,
    token: String,
}

#[derive(Debug, Deserialize)]
struct UserList {
    users: Vec<UserRow>,
}

/// Dispatch entry point for `engram user <subcommand>`.
///
/// # Errors
/// Returns an error if the HTTP call fails, the server returns non-2xx,
/// or the response body can't be deserialised.
pub async fn run(config: &Config, args: UserArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    match args.command {
        UserCommand::Add(args) => add(&ep, args).await,
        UserCommand::List(args) => list(&ep, args).await,
        UserCommand::Expire(args) => expire(&ep, args).await,
        UserCommand::Revive(args) => revive(&ep, args).await,
        UserCommand::RotateToken(args) => rotate_token(&ep, args).await,
    }
}

#[derive(Debug, Serialize)]
struct CreateUserBody<'a> {
    username: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<&'a str>,
}

async fn add(ep: &ServerEndpoint, args: UserAddArgs) -> Result<()> {
    let body = CreateUserBody {
        username: args.username.trim(),
        name: args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        email: args
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    };
    let resp: UserWithToken = post_json(ep, "/admin/users", &body)
        .await
        .context("creating user")?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "user": &resp.user,
                "token": &resp.token
            }))?
        );
    } else {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "✓ created user '{}'", resp.user.username);
        if let Some(name) = &resp.user.name {
            let _ = writeln!(stderr, "  name:  {name}");
        }
        if let Some(email) = &resp.user.email {
            let _ = writeln!(stderr, "  email: {email}");
        }
        let _ = writeln!(
            stderr,
            "  id:    {}\n\n\
             Store this token now — it will NOT be shown again. \
             Only its SHA-256 digest is kept in the DB.",
            resp.user.id
        );
        // Token on stdout so it can be piped (`> ~/.config/...`) without
        // the surrounding human chrome.
        println!("{}", resp.token);
    }
    Ok(())
}

async fn list(ep: &ServerEndpoint, args: UserListArgs) -> Result<()> {
    let resp: UserList = get_json(ep, "/admin/users", &[])
        .await
        .context("listing users")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp.users)?);
        return Ok(());
    }
    if resp.users.is_empty() {
        println!("(no registered users)");
        return Ok(());
    }
    // Fixed-width table — usernames are validated ≤ 64 chars in core,
    // but most are short, so right-pad to the longest in this batch.
    let user_w = resp
        .users
        .iter()
        .map(|u| u.username.len())
        .max()
        .unwrap_or(8)
        .max(8);
    let name_w = resp
        .users
        .iter()
        .filter_map(|u| u.name.as_ref().map(String::len))
        .max()
        .unwrap_or(4)
        .max(4);
    let email_w = resp
        .users
        .iter()
        .filter_map(|u| u.email.as_ref().map(String::len))
        .max()
        .unwrap_or(5)
        .max(5);

    println!(
        "{:<user_w$}  {:<name_w$}  {:<email_w$}  {:<8}",
        "USERNAME",
        "NAME",
        "EMAIL",
        "STATUS",
        user_w = user_w,
        name_w = name_w,
        email_w = email_w,
    );
    for u in &resp.users {
        let status = if u.is_token_active() {
            "active"
        } else {
            "expired"
        };
        println!(
            "{:<user_w$}  {:<name_w$}  {:<email_w$}  {:<8}",
            u.username,
            u.name.as_deref().unwrap_or("-"),
            u.email.as_deref().unwrap_or("-"),
            status,
            user_w = user_w,
            name_w = name_w,
            email_w = email_w,
        );
    }
    Ok(())
}

async fn expire(ep: &ServerEndpoint, args: UserExpireArgs) -> Result<()> {
    if !args.yes {
        confirm(&format!(
            "Expire token for user '{}'? Their token stops authenticating immediately. (y/N) ",
            args.username
        ))?;
    }
    let path = format!("/admin/users/{}/expire", url_encode(&args.username));
    // The server returns `{ user: UserRow }` but the CLI only needs to
    // confirm a 2xx; ignore the payload.
    let _: serde_json::Value = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("expiring user")?;
    println!("✓ expired token for user '{}'", args.username);
    Ok(())
}

async fn revive(ep: &ServerEndpoint, args: UserReviveArgs) -> Result<()> {
    let path = format!("/admin/users/{}/revive", url_encode(&args.username));
    let _: serde_json::Value = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("reviving user")?;
    println!("✓ revived token for user '{}'", args.username);
    Ok(())
}

async fn rotate_token(ep: &ServerEndpoint, args: UserRotateTokenArgs) -> Result<()> {
    if !args.yes {
        confirm(&format!(
            "Rotate token for user '{}'? Any existing client using the old token will \
             start getting 401 immediately. (y/N) ",
            args.username
        ))?;
    }
    let path = format!("/admin/users/{}/rotate-token", url_encode(&args.username));
    let resp: UserWithToken = post_json(ep, &path, &serde_json::json!({}))
        .await
        .context("rotating user token")?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "user": &resp.user,
                "token": &resp.token
            }))?
        );
    } else {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "✓ rotated token for user '{}'\n\n\
             Store this token now — it will NOT be shown again.",
            resp.user.username
        );
        println!("{}", resp.token);
    }
    Ok(())
}

/// Lightweight interactive y/N prompt. Reads from stdin; an empty
/// reply, EOF, or anything starting with `n`/`N` aborts.
fn confirm(prompt: &str) -> Result<()> {
    let mut stderr = io::stderr().lock();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();
    drop(stderr);

    let mut buf = String::new();
    let n = io::stdin().lock().read_line(&mut buf)?;
    if n == 0 {
        bail!("aborted (no input)");
    }
    let trimmed = buf.trim();
    if !trimmed.eq_ignore_ascii_case("y") && !trimmed.eq_ignore_ascii_case("yes") {
        bail!("aborted");
    }
    Ok(())
}

/// Percent-encode a username for URL-path use. The validation in
/// `core::user::validate_username` already excludes whitespace and the
/// common path separators (`/ \ : ; ,`), and emails-as-usernames
/// (`alice@home`) carry `@` which is reserved in paths. Conservative
/// encode: anything that isn't alphanumeric / `-_.` becomes `%XX`.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_passes_safe_chars_through() {
        assert_eq!(url_encode("alice"), "alice");
        assert_eq!(url_encode("user_1"), "user_1");
        assert_eq!(url_encode("a.b-c"), "a.b-c");
    }

    #[test]
    fn url_encode_percent_encodes_at_and_other_specials() {
        assert_eq!(url_encode("alice@home"), "alice%40home");
        // Validation forbids these but the encoder must still be safe
        // if a future relaxation lets them through.
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("a b"), "a%20b");
    }

    #[test]
    fn user_row_active_status_reflects_token_expired_at() {
        let active = UserRow {
            id: "x".into(),
            username: "alice".into(),
            name: None,
            email: None,
            created_at: 0,
            last_seen_at: None,
            token_expired_at: None,
        };
        assert!(active.is_token_active());

        let expired = UserRow {
            token_expired_at: Some(123),
            ..active
        };
        assert!(!expired.is_token_active());
    }
}
