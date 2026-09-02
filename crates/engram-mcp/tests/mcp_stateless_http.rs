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

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Extension, Router};
use engram_core::{
    ActiveProject, ActiveProjectMode, ActorContext, ActorKey, AuthLevel, ProjectId, WorkspaceId,
};
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
    make_router_for_actor(tmp, stateful, ActorContext::anonymous()).await
}

async fn make_router_for_actor(
    tmp: &TempDir,
    stateful: bool,
    actor: ActorContext,
) -> (Router, Store) {
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
    let router = router_for_store(&store, ws, proj, actor, stateful);
    (router, store)
}

fn router_for_store(
    store: &Store,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    actor: ActorContext,
    stateful: bool,
) -> Router {
    router_for_store_with_active(
        store,
        workspace_id,
        project_id,
        actor,
        stateful,
        ActiveProject::new(),
    )
}

fn router_for_store_with_active(
    store: &Store,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    actor: ActorContext,
    stateful: bool,
    active_project: ActiveProject,
) -> Router {
    let server = EngramServer::new(
        store.reader.clone(),
        store.writer.clone(),
        workspace_id,
        project_id,
    )
    .with_active_project(active_project);
    let svc = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(stateful)
            .with_json_response(!stateful),
    );
    Router::new()
        .nest_service("/mcp", svc)
        .layer(Extension(AuthLevel::User))
        .layer(Extension(actor))
}

async fn call_tool(
    router: &Router,
    id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    call_tool_outcome(router, id, name, arguments)
        .await
        .unwrap_or_else(|error| panic!("tool {name} failed: {error}"))
}

async fn call_tool_outcome(
    router: &Router,
    id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let response = router
        .clone()
        .oneshot(post(body.to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    let rpc: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("tool {name} returned non-JSON: {text}\n{error}"));
    if let Some(error) = rpc.get("error") {
        return Err(error.to_string());
    }
    let tool_text = rpc["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool {name} returned no text content: {text}"));
    if rpc["result"]["isError"] == true {
        return Err(tool_text.to_string());
    }
    serde_json::from_str(tool_text)
        .map_err(|error| format!("tool {name} text was not JSON: {tool_text}\n{error}"))
}

async fn call_tool_failure(
    router: &Router,
    id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> String {
    call_tool_outcome(router, id, name, arguments)
        .await
        .expect_err("tool unexpectedly succeeded")
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

/// Project-instruction application remains a local CLI capability. Task
/// continuity replaces accept with discover/claim/release/checkpoint, but the
/// remote MCP surface still carries no repository apply authority.
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
        19,
        "the MCP tool surface must contain the complete WorkItem lifecycle"
    );

    let names: Vec<_> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        names.contains(&"memory_install_self_routing"),
        "the existing read-only routing installer must remain available"
    );
    for required in [
        "memory_handoff_begin",
        "memory_handoff_discover",
        "memory_handoff_claim",
        "memory_handoff_release",
        "memory_checkpoint_write",
        "memory_handoff_cancel",
    ] {
        assert!(names.contains(&required), "missing {required}: {names:?}");
    }
    assert!(
        !names.contains(&"memory_handoff_accept"),
        "the single-use accept contract must be removed: {names:?}"
    );
    assert!(
        names.iter().all(|name| !name.contains("instruction_apply")
            && !name.contains("project_instruction")
            && *name != "instructions"),
        "remote MCP must not gain repository instruction write authority: {names:?}"
    );
}

/// Public tracer bullet for issue #40. Two independently attributed MCP
/// clients transfer one stable WorkItem, and acknowledgement occurs only when
/// the receiver writes its first durable checkpoint.
#[tokio::test]
async fn work_item_handoff_claim_checkpoint_and_completion_round_trip() {
    let tmp = TempDir::new().unwrap();
    let source = ActorContext {
        agent: Some("claude-code".into()),
        user: Some("source-user".into()),
        ..ActorContext::default()
    };
    let receiver = ActorContext {
        agent: Some("codex".into()),
        user: Some("receiver-user".into()),
        ..ActorContext::default()
    };
    let (source_router, store) = make_router_for_actor(&tmp, false, source).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let receiver_server = EngramServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
    let receiver_svc = StreamableHttpService::new(
        move || Ok(receiver_server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let receiver_router = Router::new()
        .nest_service("/mcp", receiver_svc)
        .layer(Extension(AuthLevel::User))
        .layer(Extension(receiver));

    let source_run = "019f0000-0000-7000-8000-000000000001";
    let receiving_run = "019f0000-0000-7000-8000-000000000002";
    let published = call_tool(
        &source_router,
        10,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": source_run,
            "objective": "Implement issue 40",
            "acceptance_criteria": ["public lifecycle works", "terminal state is explicit"],
            "summary": "The source agent prepared a recoverable continuation.",
            "open_questions": ["Does the receiving regression pass?"],
            "next_steps": ["Claim and checkpoint the WorkItem"],
            "files_touched": ["crates/engram-mcp/tests/mcp_stateless_http.rs"]
        }),
    )
    .await;
    let work_item_id = published["work_item_id"].as_str().unwrap();
    let handoff_id = published["handoff_id"].as_str().unwrap();
    assert_eq!(published["work_item_revision"], 1);
    assert_eq!(published["handoff_revision"], 1);

    let discovered = call_tool(
        &receiver_router,
        11,
        "memory_handoff_discover",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(discovered["handoff"]["id"], handoff_id);
    assert_eq!(discovered["handoff"]["state"], "open");
    assert_eq!(discovered["work_item"]["id"], work_item_id);
    assert_eq!(discovered["work_item"]["objective"], "Implement issue 40");
    assert_eq!(
        discovered["work_item"]["acceptance_criteria"],
        serde_json::json!(["public lifecycle works", "terminal state is explicit"])
    );

    let claimed = call_tool(
        &receiver_router,
        12,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": handoff_id,
            "expected_revision": 1,
            "run_id": receiving_run,
            "attempt_id": "019f0000-0000-7000-8000-000000000003",
            "lease_seconds": 30
        }),
    )
    .await;
    assert_eq!(claimed["work_item_id"], work_item_id);
    assert_eq!(claimed["handoff_id"], handoff_id);
    assert_eq!(claimed["revision"], 2);
    assert_eq!(claimed["handoff"]["state"], "claimed");
    let claim_id = claimed["claim_id"].as_str().unwrap();

    let still_claimed = call_tool(
        &receiver_router,
        13,
        "memory_handoff_discover",
        serde_json::json!({ "include_claimed": true }),
    )
    .await;
    assert_eq!(still_claimed["handoff"]["state"], "claimed");

    let first_checkpoint = call_tool(
        &receiver_router,
        14,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": receiving_run,
            "attempt_id": "019f0000-0000-7000-8000-000000000004",
            "expected_work_item_revision": 1,
            "handoff_id": handoff_id,
            "claim_id": claim_id,
            "expected_handoff_revision": 2,
            "summary": "Receiver durably accepted the continuation.",
            "work_item_state": "active",
            "acceptance_criteria": [
                {"criterion": "public lifecycle works", "satisfied": true},
                {"criterion": "terminal state is explicit", "satisfied": false}
            ]
        }),
    )
    .await;
    assert_eq!(first_checkpoint["sequence"], 1);
    assert_eq!(first_checkpoint["work_item_revision"], 2);
    assert_eq!(first_checkpoint["handoff_state"], "acknowledged");

    let premature_completion = call_tool_failure(
        &receiver_router,
        15,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": receiving_run,
            "attempt_id": "019f0000-0000-7000-8000-000000000005",
            "expected_work_item_revision": 2,
            "summary": "One criterion is still unsatisfied.",
            "work_item_state": "completed",
            "acceptance_criteria": [
                {"criterion": "public lifecycle works", "satisfied": true},
                {"criterion": "terminal state is explicit", "satisfied": false}
            ]
        }),
    )
    .await;
    assert!(
        premature_completion.contains("every acceptance criterion"),
        "{premature_completion}"
    );

    let completed = call_tool(
        &receiver_router,
        16,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": receiving_run,
            "attempt_id": "019f0000-0000-7000-8000-000000000006",
            "expected_work_item_revision": 2,
            "summary": "The receiver completed the WorkItem.",
            "work_item_state": "completed",
            "acceptance_criteria": [
                {"criterion": "public lifecycle works", "satisfied": true},
                {"criterion": "terminal state is explicit", "satisfied": true}
            ]
        }),
    )
    .await;
    assert_eq!(completed["sequence"], 2);
    assert_eq!(completed["work_item_revision"], 3);
    assert_eq!(completed["work_item_state"], "completed");
}

/// One recovery journey covers the retry, true same-revision concurrency,
/// ownership, scope, and recovery edges at the public MCP seam.
#[tokio::test]
async fn handoff_attempts_are_replay_safe_and_expired_leases_are_recoverable() {
    let tmp = TempDir::new().unwrap();
    let source = ActorContext {
        agent: Some("claude-code".into()),
        user: Some("source-user".into()),
        ..ActorContext::default()
    };
    let receiver = ActorContext {
        agent: Some("codex".into()),
        user: Some("receiver-user".into()),
        ..ActorContext::default()
    };
    let rival = ActorContext {
        agent: Some("opencode".into()),
        user: Some("rival-user".into()),
        ..ActorContext::default()
    };
    let (source_router, store) = make_router_for_actor(&tmp, false, source).await;
    let workspace_id = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let project_id = store
        .writer
        .get_or_create_project(workspace_id, "scratch", None)
        .await
        .unwrap();
    let fallback_project_id = store
        .writer
        .get_or_create_project(workspace_id, "static-fallback", None)
        .await
        .unwrap();
    let active_project = ActiveProject::with_mode(ActiveProjectMode::PerActor);
    for user in ["receiver-user", "rival-user"] {
        active_project.set_for(
            &ActorKey {
                user: Some(user.into()),
                session_id: None,
            },
            workspace_id,
            project_id,
        );
    }
    let receiver_router = router_for_store_with_active(
        &store,
        workspace_id,
        fallback_project_id,
        receiver,
        false,
        active_project.clone(),
    );
    let rival_router = router_for_store_with_active(
        &store,
        workspace_id,
        fallback_project_id,
        rival,
        false,
        active_project,
    );

    let published = call_tool(
        &source_router,
        20,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0000-0000-7000-8000-000000000020",
            "objective": "Survive receiver loss",
            "acceptance_criteria": ["a replacement can continue"],
            "summary": "The first receiver may disappear."
        }),
    )
    .await;
    let work_item_id = published["work_item_id"].as_str().unwrap();
    let handoff_id = published["handoff_id"].as_str().unwrap();
    let receiver_run = "019f0000-0000-7000-8000-000000000021";
    let rival_run = "019f0000-0000-7000-8000-000000000023";
    let receiver_attempt = "019f0000-0000-7000-8000-000000000022";
    let rival_attempt = "019f0000-0000-7000-8000-000000000024";
    let receiver_claim_args = serde_json::json!({
        "handoff_id": handoff_id,
        "expected_revision": 1,
        "run_id": receiver_run,
        "attempt_id": receiver_attempt,
        "lease_seconds": 30
    });
    let rival_claim_args = serde_json::json!({
        "handoff_id": handoff_id,
        "expected_revision": 1,
        "run_id": rival_run,
        "attempt_id": rival_attempt,
        "lease_seconds": 30
    });
    let (receiver_outcome, rival_outcome) = tokio::join!(
        call_tool_outcome(
            &receiver_router,
            21,
            "memory_handoff_claim",
            receiver_claim_args.clone(),
        ),
        call_tool_outcome(
            &rival_router,
            22,
            "memory_handoff_claim",
            rival_claim_args.clone(),
        )
    );
    let (
        winning_router,
        replacement_router,
        winning_run,
        replacement_run,
        winning_attempt,
        winning_claim_args,
        first_claim,
        conflict,
    ) = match (receiver_outcome, rival_outcome) {
        (Ok(claim), Err(conflict)) => (
            &receiver_router,
            &rival_router,
            receiver_run,
            rival_run,
            receiver_attempt,
            receiver_claim_args,
            claim,
            conflict,
        ),
        (Err(conflict), Ok(claim)) => (
            &rival_router,
            &receiver_router,
            rival_run,
            receiver_run,
            rival_attempt,
            rival_claim_args,
            claim,
            conflict,
        ),
        outcomes => {
            panic!("same-revision claims must yield one owner and one conflict: {outcomes:?}")
        }
    };
    assert!(conflict.contains("stale handoff revision"), "{conflict}");

    let replayed_claim = call_tool(
        winning_router,
        23,
        "memory_handoff_claim",
        winning_claim_args.clone(),
    )
    .await;
    assert_eq!(
        replayed_claim, first_claim,
        "lost claim response must replay exactly"
    );

    let actor_mismatch = call_tool_failure(
        replacement_router,
        24,
        "memory_handoff_claim",
        winning_claim_args,
    )
    .await;
    assert!(
        actor_mismatch.contains("different continuity request"),
        "{actor_mismatch}"
    );

    let mismatch = call_tool_failure(
        winning_router,
        25,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": handoff_id,
            "expected_revision": 1,
            "run_id": winning_run,
            "attempt_id": winning_attempt,
            "lease_seconds": 31
        }),
    )
    .await;
    assert!(
        mismatch.contains("different continuity request"),
        "{mismatch}"
    );

    let claim_id = first_claim["claim_id"].as_str().unwrap();
    let wrong_owner = call_tool_failure(
        replacement_router,
        26,
        "memory_handoff_release",
        serde_json::json!({
            "handoff_id": handoff_id,
            "claim_id": claim_id,
            "expected_revision": 2,
            "run_id": replacement_run,
            "attempt_id": "019f0000-0000-7000-8000-000000000025"
        }),
    )
    .await;
    assert!(
        wrong_owner.contains("different actor or Run"),
        "{wrong_owner}"
    );

    let release_args = serde_json::json!({
        "handoff_id": handoff_id,
        "claim_id": claim_id,
        "expected_revision": 2,
        "run_id": winning_run,
        "attempt_id": "019f0000-0000-7000-8000-000000000026"
    });
    let released = call_tool(
        winning_router,
        27,
        "memory_handoff_release",
        release_args.clone(),
    )
    .await;
    let replayed_release =
        call_tool(winning_router, 28, "memory_handoff_release", release_args).await;
    assert_eq!(released, replayed_release);
    assert_eq!(released["revision"], 3);
    assert_eq!(released["state"], "open");

    call_tool(
        winning_router,
        29,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": handoff_id,
            "expected_revision": 3,
            "run_id": winning_run,
            "attempt_id": "019f0000-0000-7000-8000-000000000027",
            "lease_seconds": 1
        }),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    let recovered = call_tool(
        replacement_router,
        30,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": handoff_id,
            "expected_revision": 4,
            "run_id": replacement_run,
            "attempt_id": "019f0000-0000-7000-8000-000000000028",
            "lease_seconds": 30
        }),
    )
    .await;
    assert_eq!(recovered["revision"], 5);

    let checkpoint_args = serde_json::json!({
        "work_item_id": work_item_id,
        "run_id": replacement_run,
        "attempt_id": "019f0000-0000-7000-8000-000000000029",
        "expected_work_item_revision": 1,
        "handoff_id": handoff_id,
        "claim_id": recovered["claim_id"],
        "expected_handoff_revision": 5,
        "summary": "Replacement receiver checkpointed the task.",
        "work_item_state": "active",
        "acceptance_criteria": [
            {"criterion": "a replacement can continue", "satisfied": true}
        ]
    });
    let checkpoint = call_tool(
        replacement_router,
        31,
        "memory_checkpoint_write",
        checkpoint_args.clone(),
    )
    .await;
    let replayed_checkpoint = call_tool(
        replacement_router,
        32,
        "memory_checkpoint_write",
        checkpoint_args,
    )
    .await;
    assert_eq!(checkpoint, replayed_checkpoint);
    assert_eq!(checkpoint["sequence"], 1);
    assert_eq!(checkpoint["handoff_state"], "acknowledged");

    let partial_scope = call_tool_failure(
        replacement_router,
        33,
        "memory_handoff_discover",
        serde_json::json!({"workspace": "default"}),
    )
    .await;
    assert!(partial_scope.contains("workspace") && partial_scope.contains("project"));

    let missing_scope = call_tool_failure(
        replacement_router,
        34,
        "memory_handoff_discover",
        serde_json::json!({"workspace": "missing-workspace", "project": "scratch"}),
    )
    .await;
    assert!(missing_scope.contains("not found"), "{missing_scope}");

    let foreign = call_tool(
        &source_router,
        35,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0000-0000-7000-8000-000000000030",
            "objective": "Remain isolated in another workspace",
            "summary": "Cross-workspace sentinel handoff.",
            "workspace": "other-workspace",
            "project": "scratch"
        }),
    )
    .await;
    let local = call_tool(
        replacement_router,
        36,
        "memory_handoff_discover",
        serde_json::json!({}),
    )
    .await;
    assert!(local["handoff"].is_null(), "foreign handoff leaked locally");
    let explicit_foreign = call_tool(
        replacement_router,
        37,
        "memory_handoff_discover",
        serde_json::json!({"workspace": "other-workspace", "project": "scratch"}),
    )
    .await;
    assert_eq!(explicit_foreign["handoff"]["id"], foreign["handoff_id"]);
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
