//! Integration tests for the Streamable HTTP transport's stateless vs.
//! stateful behaviour (issue #3).
//!
//! Stateless clients (OpenCode `type: "remote"`, curl) send `initialize`
//! and `tools/call` as independent requests without echoing an
//! `Mcp-Session-Id`. In rmcp's default *stateful* mode the server demands
//! that header and rejects the second request with 422 "Unexpected
//! message, expect initialize request". `engram serve --transport http`
//! now defaults to *stateless* mode (`legacy_session_mode=false` +
//! `json_response=true`), so those clients work with no `mcp-remote` shim.
//! `--http-stateful` restores the session behaviour, and only for clients
//! on a pre-2026-07-28 protocol version — that revision removed sessions
//! from the protocol outright.
//!
//! These tests drive the exact `StreamableHttpService` wiring from
//! `serve.rs` through an axum router, so they catch a regression in either
//! direction. They also pin protocol-version negotiation, which the server
//! previously short-circuited by hard-pinning 2024-11-05.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use engram_mcp::EngramServer;
use engram_store::Store;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tempfile::TempDir;
use tower::ServiceExt;

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;
const TOOLS_CALL_STATUS: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_status","arguments":{}}}"#;
const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#;

/// Build a `/mcp` router exactly like `serve.rs` does, toggling stateful
/// mode. Returns the `Store` too so the writer actor stays alive for the
/// duration of the test.
async fn make_router(tmp: &TempDir, stateful: bool) -> (Router, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch".to_string(), None)
        .await
        .unwrap();
    let server = EngramServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
    let svc = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(stateful)
            .with_json_response(!stateful),
    );
    let router = Router::new().nest_service("/mcp", svc);
    (router, store)
}

/// POST a JSON-RPC body to `/mcp` with the Accept header every compliant
/// Streamable HTTP client sends (both JSON and event-stream), and no
/// session id.
fn post(body: impl Into<Body>) -> Request<Body> {
    post_with_headers(body, &[])
}

/// [`post`] plus extra request headers, for the SEP-2243 standard headers
/// (`MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`) that a 2026-07-28
/// client must send.
fn post_with_headers(body: impl Into<Body>, extra: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        // rmcp's DNS-rebinding guard rejects a missing/disallowed Host with
        // 400; `localhost` is in the default allowlist. Real HTTP clients
        // always send Host — oneshot does not, so set it explicitly.
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    builder.body(body.into()).unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 2_000_000)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The fix: in the default stateless mode, a `tools/call` arriving with no
/// prior session and no `Mcp-Session-Id` header is serviced and returns a
/// JSON-RPC result — not a 422 / "Session not found".
#[tokio::test]
async fn stateless_tools_call_without_session_succeeds() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router
        .clone()
        .oneshot(post(TOOLS_CALL_STATUS))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "stateless tools/call must succeed without a session id"
    );
    let body = body_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("stateless response must be JSON, got: {body}\nerr: {e}"));
    assert!(
        json.get("error").is_none(),
        "expected a JSON-RPC result, got an error: {body}"
    );
    assert!(json.get("result").is_some(), "missing result: {body}");
    // memory_status serialises StatusCounts, whose fields include
    // `pages_latest` — proves the tool actually ran, not just an empty ack.
    assert!(
        body.contains("pages_latest"),
        "result should carry status counts: {body}"
    );
}

/// `initialize` in stateless mode also returns a plain JSON-RPC result
/// (no session handshake required).
#[tokio::test]
async fn stateless_initialize_returns_json_result() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router.clone().oneshot(post(INITIALIZE)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("initialize returns JSON");
    assert!(
        json.get("result").is_some(),
        "missing initialize result: {body}"
    );
    assert!(
        body.contains("serverInfo") || body.contains("protocolVersion"),
        "initialize result should carry server info: {body}"
    );
}

/// Project-instruction application remains a local CLI capability. A remote
/// server may expose proposal storage through authenticated admin routes, but
/// its MCP tool list must stay at the existing 16 tools and carry no repository
/// apply authority.
#[tokio::test]
async fn stateless_remote_mcp_has_no_repository_instruction_write_tool() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router.clone().oneshot(post(TOOLS_LIST)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("tools/list must return JSON");
    let tools = json["result"]["tools"]
        .as_array()
        .expect("tools/list result must contain tools");
    assert_eq!(
        tools.len(),
        16,
        "the MCP tool surface must remain unchanged"
    );

    let names: Vec<_> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        names.contains(&"memory_install_self_routing"),
        "the existing read-only routing installer must remain available"
    );
    assert!(
        names.iter().all(|name| !name.contains("instruction_apply")
            && !name.contains("project_instruction")
            && *name != "instructions"),
        "remote MCP must not gain repository instruction write authority: {names:?}"
    );
}

/// `get_info()` used to hard-pin `ProtocolVersion::V_2024_11_05`, so every
/// client — Claude Code asks for 2025-11-25, OpenCode and Codex for their
/// own revisions — was answered with the launch revision and lost every
/// feature added since. rmcp now negotiates against
/// `ServerHandler::supported_protocol_versions()`, so `initialize` must
/// echo whatever known revision the client asked for.
#[tokio::test]
async fn initialize_echoes_every_known_protocol_version() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    for version in [
        "2024-11-05",
        "2025-03-26",
        "2025-06-18",
        "2025-11-25",
        "2026-07-28",
    ] {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{version}","capabilities":{{}},"clientInfo":{{"name":"test","version":"1.0"}}}}}}"#
        );
        let resp = router.clone().oneshot(post(body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "initialize {version}");
        let text = body_string(resp).await;
        let json: serde_json::Value = serde_json::from_str(&text).expect("initialize returns JSON");
        assert_eq!(
            json["result"]["protocolVersion"].as_str(),
            Some(version),
            "server must negotiate {version}, not force its own default: {text}"
        );
    }
}

/// A client asking for a revision rmcp does not know falls back to the
/// server's advertised default instead of erroring — and that default is
/// now the SDK's latest, not 2024-11-05.
#[tokio::test]
async fn initialize_falls_back_to_latest_for_unknown_version() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;
    let resp = router.clone().oneshot(post(body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = body_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).expect("initialize returns JSON");
    assert_eq!(
        json["result"]["protocolVersion"].as_str(),
        Some(rmcp::model::ProtocolVersion::LATEST.as_str()),
        "unknown-version fallback must be the SDK's latest: {text}"
    );
}

/// MCP 2026-07-28 replaces the `initialize` handshake with `server/discover`
/// (SEP-2575). The server must answer it and advertise 2026-07-28 among its
/// supported revisions, so a stateless client can pick a version up front.
#[tokio::test]
async fn server_discover_advertises_2026_07_28() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let body = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let resp = router
        .clone()
        .oneshot(post_with_headers(
            body,
            &[
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", "server/discover"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = body_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).expect("discover returns JSON");
    let versions = json["result"]["supportedVersions"]
        .as_array()
        .unwrap_or_else(|| panic!("discover must advertise supportedVersions: {text}"));
    assert!(
        versions.iter().any(|v| v == "2026-07-28"),
        "server must advertise the current spec revision: {text}"
    );
}

/// The 2026-07-28 inline lifecycle: no `initialize`, no session id — the
/// protocol version and client capabilities ride in each request's `_meta`,
/// with the SEP-2243 standard headers alongside. A `tools/call` sent that
/// way must run the tool.
#[tokio::test]
async fn stateless_2026_07_28_tools_call_without_initialize_succeeds() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_status","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let resp = router
        .clone()
        .oneshot(post_with_headers(
            body,
            &[
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", "tools/call"),
                ("mcp-name", "memory_status"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = body_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).expect("tools/call returns JSON");
    assert!(
        json.get("error").is_none(),
        "expected a JSON-RPC result, got an error: {text}"
    );
    assert!(
        text.contains("pages_latest"),
        "the tool must actually have run: {text}"
    );
}

/// Contrast / guard: with `--http-stateful` (session mode), the same
/// session-less `tools/call` is rejected with 422 "Unexpected message,
/// expect initialize request" — the exact symptom from issue #3. This
/// proves the default flip is what resolves it, and pins the opt-in
/// behaviour so a future change to the default can't silently regress it.
#[tokio::test]
async fn stateful_tools_call_without_session_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, true).await;

    let resp = router
        .clone()
        .oneshot(post(TOOLS_CALL_STATUS))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "stateful mode must reject a session-less tools/call"
    );
    let body = body_string(resp).await;
    assert!(
        body.contains("initialize"),
        "stateful rejection should mention the missing initialize: {body}"
    );
}
