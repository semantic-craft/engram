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
    ActiveProject, ActiveProjectMode, ActorContext, ActorKey, AuthLevel, ProjectId, WorkItemId,
    WorkItemState, WorkspaceId,
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
/// remote MCP surface still carries no repository apply authority; it does
/// expose the read-only exact context reader.
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
        20,
        "the MCP tool surface must contain WorkItem continuity and context reading"
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
        names.contains(&"memory_context_read"),
        "the budgeted query package reader must be available"
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
            "context_budget": 4096,
            "lease_seconds": 30
        }),
    )
    .await;
    assert_eq!(claimed["work_item_id"], work_item_id);
    assert_eq!(claimed["handoff_id"], handoff_id);
    assert_eq!(claimed["revision"], 2);
    assert_eq!(claimed["handoff"]["state"], "claimed");
    assert!(
        claimed.get("package").is_some() && claimed.get("trace").is_some(),
        "claim must return the shared ContextPackage contract: {claimed}"
    );
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
        "context_budget": 4096,
        "lease_seconds": 30
    });
    let rival_claim_args = serde_json::json!({
        "handoff_id": handoff_id,
        "expected_revision": 1,
        "run_id": rival_run,
        "attempt_id": rival_attempt,
        "context_budget": 4096,
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
    assert!(
        first_claim.get("package").is_some(),
        "the winning claim must return a ContextPackage: {first_claim}"
    );
    assert!(
        !conflict.contains("\"package\""),
        "the losing claim must not return a ContextPackage: {conflict}"
    );

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
            "context_budget": 4096,
            "lease_seconds": 31
        }),
    )
    .await;
    assert!(
        mismatch.contains("different continuity request"),
        "{mismatch}"
    );

    // The claim returns an assembled package, so the assembly options are part
    // of the Attempt identity too: changing the budget is a changed request,
    // not a lost-response retry that may replay under the same id.
    let changed_budget = call_tool_failure(
        winning_router,
        251,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": handoff_id,
            "expected_revision": 1,
            "run_id": winning_run,
            "attempt_id": winning_attempt,
            "context_budget": 8192,
            "lease_seconds": 30
        }),
    )
    .await;
    assert!(
        changed_budget.contains("different continuity request"),
        "{changed_budget}"
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
            "context_budget": 4096,
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
            "context_budget": 4096,
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

const DELIVERY_FLAGS: [&str; 10] = [
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

fn delivery_only(flag: &str) -> serde_json::Value {
    let mut facts = serde_json::Map::new();
    for name in DELIVERY_FLAGS {
        facts.insert(name.to_string(), serde_json::json!(name == flag));
    }
    serde_json::Value::Object(facts)
}

fn asserted_flags(value: &serde_json::Value) -> Vec<&str> {
    DELIVERY_FLAGS
        .into_iter()
        .filter(|flag| value["delivery"][flag] == true)
        .collect()
}

/// Issue #42 tracer: publish/checkpoint carry typed ArtifactRefs and never
/// infer delivery facts from one another.
#[tokio::test]
async fn typed_artifact_refs_carry_independent_delivery_facts() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;
    let run_id = "019f0042-0000-7000-8000-000000000001";
    let mut artifacts = Vec::new();
    for flag in DELIVERY_FLAGS {
        artifacts.push(serde_json::json!({
            "kind": "file",
            "locator": format!("src/{flag}.rs"),
            "content_hash": format!("hash-{flag}"),
            "provenance": "source agent observation",
            "delivery": delivery_only(flag)
        }));
    }
    artifacts.push(serde_json::json!({
        "kind": "worktree",
        "locator": "main-worktree",
        "repository_identity": "github.com/semantic-craft/engram",
        "observed_revision": "abc123deadbeef",
        "dirty": true,
        "local_path_hint": "/tmp/machine-a/engram",
        "provenance": "dirty checkout",
        "delivery": delivery_only("changed")
    }));
    artifacts.push(serde_json::json!({
        "kind": "git",
        "locator": "origin",
        "repository_identity": "github.com/semantic-craft/engram",
        "observed_revision": "abc123deadbeef",
        "commit_id": "abc123deadbeef",
        "local_path_hint": "/tmp/machine-a/engram",
        "provenance": "stale tests",
        "verification": [{
            "check": "cargo test --workspace",
            "result": "ok",
            "applies_to_revision": "old-revision"
        }]
    }));

    let published = call_tool(
        &router,
        42,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": run_id,
            "objective": "Carry typed artifact evidence",
            "acceptance_criteria": ["facts stay independent"],
            "summary": "Published independent delivery facts.",
            "artifacts": artifacts
        }),
    )
    .await;
    let returned = published["artifacts"]
        .as_array()
        .expect("begin returns artifacts");
    assert_eq!(returned.len(), DELIVERY_FLAGS.len() + 2);
    for (flag, artifact) in DELIVERY_FLAGS.iter().zip(returned.iter()) {
        assert_eq!(artifact["kind"], "file");
        assert_eq!(asserted_flags(artifact), vec![*flag], "{artifact}");
        assert_eq!(artifact["locator"], format!("src/{flag}.rs"));
    }
    let worktree = &returned[DELIVERY_FLAGS.len()];
    assert_eq!(worktree["kind"], "worktree");
    assert_eq!(worktree["dirty"], true);
    assert_eq!(asserted_flags(worktree), vec!["changed"]);
    assert_ne!(worktree["delivery"]["committed"], true);
    assert_ne!(worktree["delivery"]["pushed"], true);
    let git = &returned[DELIVERY_FLAGS.len() + 1];
    assert_eq!(git["verification"][0]["stale"], true);
    assert_eq!(
        git["verification"][0]["applies_to_revision"],
        "old-revision"
    );
    assert_eq!(git["observed_revision"], "abc123deadbeef");

    let checkpoint = call_tool(
        &router,
        43,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": published["work_item_id"],
            "run_id": run_id,
            "attempt_id": "019f0042-0000-7000-8000-000000000002",
            "expected_work_item_revision": 1,
            "summary": "Checkpointed the same independent facts.",
            "work_item_state": "active",
            "acceptance_criteria": [
                {"criterion": "facts stay independent", "satisfied": false}
            ],
            "artifacts": [{
                "kind": "external",
                "locator": "https://github.com/semantic-craft/engram/issues/42",
                "observed_revision": "open",
                "provenance": "issue tracker",
                "delivery": delivery_only("submitted")
            }]
        }),
    )
    .await;
    assert_eq!(checkpoint["artifacts"][0]["kind"], "external");
    assert_eq!(
        asserted_flags(&checkpoint["artifacts"][0]),
        vec!["submitted"]
    );
    assert_ne!(checkpoint["artifacts"][0]["delivery"]["approved"], true);

    let mismatch = call_tool_failure(
        &router,
        44,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000003",
            "objective": "Conflicting hash",
            "summary": "Same file locator, different content hash.",
            "artifacts": [{
                "kind": "file",
                "locator": "src/changed.rs",
                "content_hash": "hash-changed-other",
                "provenance": "second observer"
            }]
        }),
    )
    .await;
    assert!(mismatch.contains("content-hash mismatch"), "{mismatch}");
}

/// Issue #42 tracer: two absolute cwds with the same repository identity and
/// revision resolve to one artifact.
#[tokio::test]
async fn git_artifact_identity_ignores_absolute_cwd() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;
    let machine_a = call_tool(
        &router,
        50,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000010",
            "objective": "Machine A continuation",
            "summary": "First absolute cwd.",
            "cwd": "/tmp/machine-a/engram",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "https://github.com/semantic-craft/engram.git",
                "observed_revision": "def456",
                "commit_id": "def456",
                "local_path_hint": "/tmp/machine-a/engram"
            }]
        }),
    )
    .await;
    let machine_b = call_tool(
        &router,
        51,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000011",
            "objective": "Machine B continuation",
            "summary": "Second absolute cwd.",
            "cwd": "/Users/other/machine-b/engram",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "https://github.com/semantic-craft/engram",
                "observed_revision": "def456",
                "commit_id": "def456",
                "local_path_hint": "/Users/other/machine-b/engram"
            }]
        }),
    )
    .await;
    assert_ne!(machine_a["work_item_id"], machine_b["work_item_id"]);
    assert_eq!(
        machine_a["artifacts"][0]["id"],
        machine_b["artifacts"][0]["id"]
    );
    assert_eq!(machine_a["artifacts"][0]["observed_revision"], "def456");
    assert_eq!(machine_b["artifacts"][0]["observed_revision"], "def456");
    assert_eq!(
        machine_a["artifacts"][0]["source_run_id"],
        "019f0042-0000-7000-8000-000000000010"
    );
    assert_eq!(
        machine_b["artifacts"][0]["source_run_id"],
        "019f0042-0000-7000-8000-000000000011"
    );
    assert_eq!(
        machine_a["artifacts"][0]["local_path_hint"],
        "/tmp/machine-a/engram"
    );
    assert_eq!(
        machine_b["artifacts"][0]["local_path_hint"],
        "/Users/other/machine-b/engram"
    );
}

/// Issue #42 tracer: related WorkItems use explicit relationships, do not
/// inherit claims/blockers, and fail closed on child authority, missing
/// scope, self-links, and cycles.
#[tokio::test]
async fn work_item_relationships_fail_closed_and_do_not_inherit_claims() {
    let tmp = TempDir::new().unwrap();
    let parent_actor = ActorContext {
        agent: Some("claude-code".into()),
        user: Some("parent-user".into()),
        ..ActorContext::default()
    };
    let child_actor = ActorContext {
        agent: Some("codex".into()),
        user: Some("child-user".into()),
        ..ActorContext::default()
    };
    let (parent_router, store) = make_router_for_actor(&tmp, false, parent_actor).await;
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
    let sibling = store
        .writer
        .get_or_create_project(ws, "sibling", None)
        .await
        .unwrap();
    let _ = sibling;
    let child_server = EngramServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
    let child_svc = StreamableHttpService::new(
        move || Ok(child_server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let child_router = Router::new()
        .nest_service("/mcp", child_svc)
        .layer(Extension(AuthLevel::User))
        .layer(Extension(child_actor));

    let parent_run = "019f0042-0000-7000-8000-000000000020";
    let parent = call_tool(
        &parent_router,
        60,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": parent_run,
            "objective": "Parent work",
            "acceptance_criteria": ["parent remains parent"],
            "summary": "Parent is blocked and claimed independently."
        }),
    )
    .await;
    let parent_id = parent["work_item_id"].as_str().unwrap().to_string();
    let parent_handoff = parent["handoff_id"].as_str().unwrap().to_string();

    call_tool(
        &parent_router,
        61,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": parent_id,
            "run_id": parent_run,
            "attempt_id": "019f0042-0000-7000-8000-000000000021",
            "expected_work_item_revision": 1,
            "summary": "Parent is blocked on review.",
            "work_item_state": "blocked",
            "acceptance_criteria": [
                {"criterion": "parent remains parent", "satisfied": false}
            ]
        }),
    )
    .await;

    let claimed = call_tool(
        &parent_router,
        62,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": parent_handoff,
            "expected_revision": 1,
            "run_id": parent_run,
            "attempt_id": "019f0042-0000-7000-8000-000000000022",
            "context_budget": 4096
        }),
    )
    .await;
    assert_eq!(claimed["handoff"]["state"], "claimed");

    let child = call_tool(
        &child_router,
        63,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000023",
            "objective": "Child investigation",
            "summary": "New WorkItem derived from the parent.",
            "relationships": [
                {"kind": "child_of", "target_work_item_id": parent_id},
                {"kind": "derived_from", "target_work_item_id": parent_id},
                {"kind": "depends_on", "target_work_item_id": parent_id}
            ]
        }),
    )
    .await;
    let child_id = child["work_item_id"].as_str().unwrap().to_string();
    assert_ne!(child_id, parent_id);
    let kinds: Vec<_> = child["relationships"]
        .as_array()
        .unwrap()
        .iter()
        .map(|rel| rel["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"child_of"));
    assert!(kinds.contains(&"derived_from"));
    assert!(kinds.contains(&"depends_on"));

    let discovered_child = call_tool(
        &child_router,
        64,
        "memory_handoff_discover",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(discovered_child["work_item"]["id"], child_id);
    assert_eq!(discovered_child["work_item"]["state"], "active");
    assert_eq!(discovered_child["handoff"]["state"], "open");

    let parent_item = store
        .reader
        .work_item_by_id(parent_id.parse::<WorkItemId>().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parent_item.state, WorkItemState::Blocked);

    let child_claim = call_tool_failure(
        &child_router,
        66,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": parent_handoff,
            "expected_revision": claimed["revision"],
            "run_id": "019f0042-0000-7000-8000-000000000023",
            "attempt_id": "019f0042-0000-7000-8000-000000000024",
            "context_budget": 4096
        }),
    )
    .await;
    assert!(
        child_claim.contains("child WorkItem cannot"),
        "{child_claim}"
    );

    let child_complete = call_tool_failure(
        &child_router,
        67,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": parent_id,
            "run_id": "019f0042-0000-7000-8000-000000000023",
            "attempt_id": "019f0042-0000-7000-8000-000000000025",
            "expected_work_item_revision": 2,
            "summary": "Child tried to complete the parent.",
            "work_item_state": "completed",
            "acceptance_criteria": [
                {"criterion": "parent remains parent", "satisfied": true}
            ]
        }),
    )
    .await;
    assert!(
        child_complete.contains("child WorkItem cannot"),
        "{child_complete}"
    );

    let child_abandon = call_tool_failure(
        &child_router,
        65,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": parent_id,
            "run_id": "019f0042-0000-7000-8000-000000000023",
            "attempt_id": "019f0042-0000-7000-8000-000000000032",
            "expected_work_item_revision": 2,
            "summary": "Child tried to abandon the parent.",
            "work_item_state": "abandoned",
            "acceptance_criteria": [
                {"criterion": "parent remains parent", "satisfied": false}
            ]
        }),
    )
    .await;
    assert!(
        child_abandon.contains("child WorkItem cannot"),
        "{child_abandon}"
    );

    let child_supersede = call_tool_failure(
        &child_router,
        69,
        "memory_handoff_begin",
        serde_json::json!({
            "work_item_id": parent_id,
            "run_id": "019f0042-0000-7000-8000-000000000023",
            "summary": "Child tried to supersede the parent by publishing a continuation."
        }),
    )
    .await;
    assert!(
        child_supersede.contains("child WorkItem cannot"),
        "{child_supersede}"
    );

    let parent_result = call_tool(
        &child_router,
        68,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": child_id,
            "run_id": "019f0042-0000-7000-8000-000000000023",
            "attempt_id": "019f0042-0000-7000-8000-000000000026",
            "expected_work_item_revision": 1,
            "summary": "Child recorded evidence for the parent.",
            "work_item_state": "active",
            "parent_result": {
                "summary": "investigation notes",
                "artifacts": [{
                    "kind": "file",
                    "locator": "notes/child.md",
                    "provenance": "child evidence"
                }]
            }
        }),
    )
    .await;
    assert_eq!(
        parent_result["parent_result"]["parent_work_item_id"],
        parent_id
    );
    let parent_after = store
        .reader
        .work_item_by_id(parent_id.parse::<WorkItemId>().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parent_after.state, WorkItemState::Blocked);
    assert_eq!(parent_after.child_results[0].summary, "investigation notes");

    let self_link = call_tool_failure(
        &parent_router,
        70,
        "memory_handoff_begin",
        serde_json::json!({
            "work_item_id": parent_id,
            "run_id": parent_run,
            "expected_work_item_revision": parent_after.revision,
            "expected_checkpoint_revision": parent_after.revision,
            "summary": "Self-link should fail.",
            "relationships": [{"kind": "depends_on", "target_work_item_id": parent_id}]
        }),
    )
    .await;
    assert!(self_link.contains("self-link"), "{self_link}");

    let partial_scope = call_tool_failure(
        &child_router,
        71,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000027",
            "objective": "Partial scope",
            "summary": "Missing project on the target.",
            "relationships": [{
                "kind": "depends_on",
                "target_work_item_id": parent_id,
                "target_workspace": "default"
            }]
        }),
    )
    .await;
    assert!(
        partial_scope.contains("workspace") && partial_scope.contains("project"),
        "{partial_scope}"
    );

    let missing_scope = call_tool_failure(
        &child_router,
        72,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000028",
            "objective": "Missing scope",
            "summary": "Target project does not exist.",
            "relationships": [{
                "kind": "depends_on",
                "target_work_item_id": parent_id,
                "target_workspace": "default",
                "target_project": "does-not-exist"
            }]
        }),
    )
    .await;
    assert!(
        missing_scope.to_lowercase().contains("not found"),
        "{missing_scope}"
    );

    let foreign = call_tool(
        &parent_router,
        73,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000029",
            "objective": "Sibling project work",
            "summary": "Lives in sibling.",
            "workspace": "default",
            "project": "sibling"
        }),
    )
    .await;
    let linked = call_tool(
        &parent_router,
        74,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": parent_run,
            "objective": "Cross-project link",
            "summary": "Depends on sibling work.",
            "relationships": [{
                "kind": "depends_on",
                "target_work_item_id": foreign["work_item_id"],
                "target_workspace": "default",
                "target_project": "sibling"
            }]
        }),
    )
    .await;
    assert_eq!(
        linked["relationships"][0]["to_work_item_id"],
        foreign["work_item_id"]
    );

    let cycle_a = call_tool(
        &parent_router,
        75,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000030",
            "objective": "Cycle A",
            "summary": "First node."
        }),
    )
    .await;
    let cycle_b = call_tool(
        &parent_router,
        76,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000031",
            "objective": "Cycle B",
            "summary": "Depends on A.",
            "relationships": [{
                "kind": "depends_on",
                "target_work_item_id": cycle_a["work_item_id"]
            }]
        }),
    )
    .await;
    let cycle = call_tool_failure(
        &parent_router,
        77,
        "memory_handoff_begin",
        serde_json::json!({
            "work_item_id": cycle_a["work_item_id"],
            "run_id": "019f0042-0000-7000-8000-000000000030",
            "expected_work_item_revision": cycle_a["work_item_revision"],
            "summary": "A depends on B would cycle.",
            "relationships": [{
                "kind": "depends_on",
                "target_work_item_id": cycle_b["work_item_id"]
            }]
        }),
    )
    .await;
    assert!(cycle.contains("acyclic"), "{cycle}");
}

/// Issue #42 tracer: artifact text is privacy-scrubbed and audit detail never
/// records opaque claim secrets.
#[tokio::test]
async fn artifact_text_is_scrubbed_and_audit_omits_claim_secrets() {
    let tmp = TempDir::new().unwrap();
    let (router, store) = make_router(&tmp, false).await;
    let published = call_tool(
        &router,
        80,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000040",
            "objective": "Scrub secrets",
            "acceptance_criteria": ["no leaked claim material"],
            "summary": "Artifact provenance contains a key.",
            "artifacts": [{
                "kind": "file",
                "locator": "src/secret.rs",
                "provenance": "token sk-or-v1-deadbeefcafebabe1234567890abcdef"
            }]
        }),
    )
    .await;
    let provenance = published["artifacts"][0]["provenance"].as_str().unwrap();
    assert!(provenance.contains("[REDACTED]"), "{provenance}");
    assert!(!provenance.contains("deadbeef"), "{provenance}");

    let claimed = call_tool(
        &router,
        81,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": published["handoff_id"],
            "expected_revision": 1,
            "run_id": "019f0042-0000-7000-8000-000000000041",
            "attempt_id": "019f0042-0000-7000-8000-000000000042",
            "context_budget": 4096
        }),
    )
    .await;
    let claim_id = claimed["claim_id"].as_str().unwrap();

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let mut stmt = conn
        .prepare("SELECT detail FROM audit_log WHERE op LIKE 'handoff%' OR op LIKE 'checkpoint%'")
        .unwrap();
    let details: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    assert!(!details.is_empty(), "expected continuity audit rows");
    for detail in &details {
        assert!(
            !detail.contains(claim_id),
            "audit leaked claim secret: {detail}"
        );
        assert!(
            !detail.contains("deadbeef"),
            "audit leaked scrubbed secret: {detail}"
        );
    }
}

/// Issue #42 tracer: checkpoint write and response expose ArtifactRefs and
/// relationships with stable identities.
#[tokio::test]
async fn checkpoint_write_returns_artifacts_and_relationships() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;
    let run_id = "019f0042-0000-7000-8000-000000000050";
    let current = call_tool(
        &router,
        90,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": run_id,
            "objective": "Checkpoint artifacts and relationships",
            "acceptance_criteria": ["checkpoint returns both"],
            "summary": "Work item that will checkpoint evidence."
        }),
    )
    .await;
    let dependency = call_tool(
        &router,
        91,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000051",
            "objective": "Upstream dependency",
            "summary": "Target of an explicit depends_on link."
        }),
    )
    .await;

    let checkpoint = call_tool(
        &router,
        92,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": current["work_item_id"],
            "run_id": run_id,
            "attempt_id": "019f0042-0000-7000-8000-000000000052",
            "expected_work_item_revision": 1,
            "summary": "Attached an artifact and a depends_on relationship.",
            "work_item_state": "active",
            "acceptance_criteria": [
                {"criterion": "checkpoint returns both", "satisfied": false}
            ],
            "artifacts": [{
                "kind": "file",
                "locator": "src/checkpoint.rs",
                "repository_identity": "github.com/semantic-craft/engram",
                "content_hash": "checkpoint-hash",
                "provenance": "checkpoint observation",
                "local_path_hint": "/tmp/machine-a/src/checkpoint.rs"
            }],
            "relationships": [{
                "kind": "depends_on",
                "target_work_item_id": dependency["work_item_id"]
            }]
        }),
    )
    .await;

    assert_eq!(checkpoint["artifacts"][0]["kind"], "file");
    assert_eq!(checkpoint["artifacts"][0]["locator"], "src/checkpoint.rs");
    assert_eq!(
        checkpoint["artifacts"][0]["repository_identity"],
        "github.com/semantic-craft/engram"
    );
    assert_eq!(checkpoint["artifacts"][0]["source_run_id"], run_id);
    assert!(checkpoint["artifacts"][0]["id"].as_str().is_some());
    assert_eq!(checkpoint["relationships"][0]["kind"], "depends_on");
    assert_eq!(
        checkpoint["relationships"][0]["from_work_item_id"],
        current["work_item_id"]
    );
    assert_eq!(
        checkpoint["relationships"][0]["to_work_item_id"],
        dependency["work_item_id"]
    );
    assert!(checkpoint["relationships"][0]["id"].as_str().is_some());
}

/// Issue #42 tracer: creating depends_on/derived_from/child_of rejects an
/// unauthorized actor and does not mutate relationships.
#[tokio::test]
async fn relationship_creation_rejects_unauthorized_actor() {
    let tmp = TempDir::new().unwrap();
    let owner = ActorContext {
        agent: Some("claude-code".into()),
        user: Some("owner-user".into()),
        ..ActorContext::default()
    };
    let stranger = ActorContext {
        agent: Some("codex".into()),
        user: Some("stranger-user".into()),
        ..ActorContext::default()
    };
    let (owner_router, store) = make_router_for_actor(&tmp, false, owner).await;
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
    let stranger_server = EngramServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
    let stranger_svc = StreamableHttpService::new(
        move || Ok(stranger_server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let stranger_router = Router::new()
        .nest_service("/mcp", stranger_svc)
        .layer(Extension(AuthLevel::User))
        .layer(Extension(stranger));

    let from = call_tool(
        &owner_router,
        100,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000060",
            "objective": "Owned source work",
            "summary": "Owner publishes the FROM work item."
        }),
    )
    .await;
    let target = call_tool(
        &owner_router,
        101,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000061",
            "objective": "Owned target work",
            "summary": "Owner publishes the TO work item."
        }),
    )
    .await;
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let count_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM work_item_relationships", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count_before, 0);

    for (idx, kind) in ["depends_on", "derived_from", "child_of"]
        .into_iter()
        .enumerate()
    {
        let error = call_tool_failure(
            &stranger_router,
            102 + idx as u64,
            "memory_handoff_begin",
            serde_json::json!({
                "work_item_id": from["work_item_id"],
                "run_id": "019f0042-0000-7000-8000-000000000062",
                "summary": format!("Stranger tried to attach {kind}."),
                "relationships": [{
                    "kind": kind,
                    "target_work_item_id": target["work_item_id"]
                }]
            }),
        )
        .await;
        assert!(
            error.to_lowercase().contains("unauthorized"),
            "{kind}: {error}"
        );
    }

    let count_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM work_item_relationships", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count_after, 0);
}

/// Issue #42 tracer: observation metadata is per attachment, file identity
/// includes repository coordinates, and dirty worktrees do not collapse.
#[tokio::test]
async fn artifact_observations_are_per_attachment_and_do_not_collide() {
    let tmp = TempDir::new().unwrap();
    let (router, store) = make_router(&tmp, false).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let _sibling = store
        .writer
        .get_or_create_project(ws, "sibling", None)
        .await
        .unwrap();

    let first = call_tool(
        &router,
        110,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000070",
            "objective": "First observer",
            "summary": "First machine attaches the git object.",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "abc123",
                "commit_id": "abc123",
                "dirty": false,
                "local_path_hint": "/tmp/machine-a/engram",
                "provenance": "first observer"
            }]
        }),
    )
    .await;
    let second = call_tool(
        &router,
        111,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000071",
            "objective": "Second observer",
            "summary": "Second project attaches the same git object.",
            "workspace": "default",
            "project": "sibling",
            "cwd": "/tmp/project-b/engram",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "abc123",
                "commit_id": "abc123",
                "dirty": true,
                "local_path_hint": "/Users/other/machine-b/engram",
                "provenance": "second observer"
            }]
        }),
    )
    .await;
    assert_eq!(
        first["artifacts"][0]["id"], second["artifacts"][0]["id"],
        "one repository at one revision is one identity, whatever absolute \
         checkout observed it"
    );
    assert_eq!(
        second["artifacts"][0]["source_run_id"],
        "019f0042-0000-7000-8000-000000000071"
    );
    assert_eq!(second["artifacts"][0]["provenance"], "second observer");
    assert_eq!(second["artifacts"][0]["dirty"], true);
    assert_eq!(
        second["artifacts"][0]["local_path_hint"],
        "/Users/other/machine-b/engram"
    );
    assert_eq!(
        first["artifacts"][0]["source_run_id"],
        "019f0042-0000-7000-8000-000000000070"
    );
    assert_eq!(first["artifacts"][0]["provenance"], "first observer");
    assert_eq!(first["artifacts"][0]["dirty"], false);

    let rediscovered = call_tool(
        &router,
        112,
        "memory_handoff_discover",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["provenance"],
        "first observer"
    );
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["local_path_hint"],
        "/tmp/machine-a/engram"
    );

    let repo_a = call_tool(
        &router,
        113,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000072",
            "objective": "Repo A file",
            "summary": "Same relative path in repository A.",
            "artifacts": [{
                "kind": "file",
                "locator": "src/lib.rs",
                "repository_identity": "github.com/org/repo-a",
                "content_hash": "hash-a",
                "provenance": "repo a"
            }]
        }),
    )
    .await;
    let repo_b = call_tool(
        &router,
        114,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000073",
            "objective": "Repo B file",
            "summary": "Same relative path in repository B.",
            "artifacts": [{
                "kind": "file",
                "locator": "src/lib.rs",
                "repository_identity": "github.com/org/repo-b",
                "content_hash": "hash-b",
                "provenance": "repo b"
            }]
        }),
    )
    .await;
    assert_ne!(
        repo_a["artifacts"][0]["id"], repo_b["artifacts"][0]["id"],
        "same relative path in two repositories must not share identity"
    );

    let dirty_a = call_tool(
        &router,
        115,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000074",
            "objective": "Dirty worktree A",
            "summary": "First dirty checkout at the same commit.",
            "artifacts": [{
                "kind": "worktree",
                "locator": "wt-a",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "abc123",
                "tree_hash": "dirty-tree-a",
                "dirty": true,
                "local_path_hint": "/tmp/wt-a",
                "provenance": "dirty a"
            }]
        }),
    )
    .await;
    let dirty_b = call_tool(
        &router,
        116,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000075",
            "objective": "Dirty worktree B",
            "summary": "Second dirty checkout at the same commit.",
            "artifacts": [{
                "kind": "worktree",
                "locator": "wt-b",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "abc123",
                "tree_hash": "dirty-tree-b",
                "dirty": true,
                "local_path_hint": "/tmp/wt-b",
                "provenance": "dirty b"
            }]
        }),
    )
    .await;
    assert_ne!(
        dirty_a["artifacts"][0]["id"], dirty_b["artifacts"][0]["id"],
        "dirty worktrees at the same commit must not collapse"
    );
    assert_eq!(dirty_a["artifacts"][0]["provenance"], "dirty a");
    assert_eq!(dirty_b["artifacts"][0]["provenance"], "dirty b");
    assert_eq!(dirty_a["artifacts"][0]["local_path_hint"], "/tmp/wt-a");
    assert_eq!(dirty_b["artifacts"][0]["local_path_hint"], "/tmp/wt-b");
    assert_eq!(dirty_a["artifacts"][0]["dirty"], true);
    assert_eq!(dirty_b["artifacts"][0]["dirty"], true);
}

/// Tracer for the claim-side ordering contract (#44): a publisher-selected
/// `context_ref` is assembled before any retrieval candidate.
///
/// The server has no embedder, so the claim's retrieval leg is pure FTS5.
/// Six wiki pages match the handoff text strongly enough that their bm25
/// ranks fall well below -1 — which is exactly why a synthetic "-1.0 score"
/// cannot express precedence. The explicit reference points at a seventh page
/// whose vocabulary retrieval never surfaces, and the budget is tight enough
/// that only the first couple of candidates fit.
#[tokio::test]
async fn explicit_handoff_context_refs_precede_retrieval_candidates_at_claim() {
    use engram_core::{NewPage, PagePath, Tier};

    let tmp = TempDir::new().unwrap();
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

    let seed = |path: String, title: String, body: String| NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title,
        body,
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
    };

    // Filler corpus: keeps the shared terms selective, so FTS5 bm25 gives the
    // six matching pages ranks far beyond the old synthetic -1.0.
    let mut pages = Vec::new();
    for index in 0..24 {
        pages.push(seed(
            format!("notes/filler-{index:02}.md"),
            format!("Filler {index:02}"),
            format!("Unrelated maintenance chatter number {index} about packaging and icons."),
        ));
    }
    for index in 0..6 {
        pages.push(seed(
            format!("notes/peregrine-{index:02}.md"),
            format!("Peregrine Ledger Rehearsal {index:02}"),
            format!(
                "peregrine ledger rehearsal notes {index}. Peregrine ledger rehearsal keeps \
                 peregrine ledger rehearsal ordering stable across every peregrine ledger \
                 rehearsal replay, variant {index}."
            ),
        ));
    }
    pages.push(seed(
        "notes/quicksilver.md".to_string(),
        "Quicksilver Decision".to_string(),
        "Quicksilver decision, carried forward verbatim by publisher choice.".to_string(),
    ));
    store.writer.upsert_pages_batch(pages).await.unwrap();

    let router = router_for_store(&store, ws, proj, ActorContext::anonymous(), false);

    // Acquire the reference through the public query seam, not by hand.
    let queried = call_tool(
        &router,
        200,
        "memory_query",
        serde_json::json!({ "query": "quicksilver", "context_budget": 4096 }),
    )
    .await;
    assert_eq!(
        queried["package"]["entries"][0]["page_path"], "notes/quicksilver.md",
        "fixture must isolate the explicitly referenced page: {queried}"
    );
    let explicit_ref = queried["package"]["entries"][0]["context_ref"].clone();

    let published = call_tool(
        &router,
        201,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0044-0000-7000-8000-000000000001",
            "objective": "Carry the quicksilver decision forward",
            "summary": "peregrine ledger rehearsal mid-flight",
            "next_steps": ["finish peregrine ledger rehearsal"],
            "context_refs": [explicit_ref],
        }),
    )
    .await;

    let claimed = call_tool(
        &router,
        202,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": published["handoff_id"],
            "expected_revision": 1,
            "run_id": "019f0044-0000-7000-8000-000000000002",
            "attempt_id": "019f0044-0000-7000-8000-000000000003",
            "context_budget": 260,
            "lease_seconds": 30
        }),
    )
    .await;

    let entries = claimed["package"]["entries"].as_array().unwrap();
    assert_eq!(
        entries[0]["context_ref"], explicit_ref,
        "the publisher-selected reference must be entry zero: {claimed}"
    );
    assert_eq!(
        entries[0]["selection_reason"], "handoff_explicit_ref",
        "entry zero must be the explicit reference, not a retrieval hit: {claimed}"
    );
    assert!(
        claimed["trace"]["candidate_count"].as_u64().unwrap() >= 7,
        "retrieval must actually have competed with the explicit reference: {claimed}"
    );
    assert_eq!(
        claimed["trace"]["deduplicated_count"], 0,
        "the six retrieval candidates must be distinct competitors: {claimed}"
    );
    assert!(
        entries
            .iter()
            .skip(1)
            .any(|entry| entry["score"].as_f64().unwrap() < -1.0),
        "a retrieval candidate must outrank -1.0, so only an explicit priority \
         dimension can order the reference first: {claimed}"
    );
    assert!(
        claimed["package"]["estimated_consumption"]
            .as_u64()
            .unwrap()
            <= 260,
        "the tight budget must still be respected: {claimed}"
    );

    // What an identical retry replays is the claim transition. The package is
    // assembled again, against current evidence — freezing it at first claim
    // would hand a retrying agent a view of a corpus that has moved on. The
    // publisher's reference is pinned to an exact revision, so it stays put
    // while the retrieval leg picks up the new page.
    store
        .writer
        .upsert_pages_batch(vec![seed(
            "notes/peregrine-06.md".to_string(),
            "Peregrine Ledger Rehearsal 06".to_string(),
            "peregrine ledger rehearsal notes 6, written after the first claim \
             response was lost. Peregrine ledger rehearsal, variant 6."
                .to_string(),
        )])
        .await
        .unwrap();
    let retried = call_tool(
        &router,
        203,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": published["handoff_id"],
            "expected_revision": 1,
            "run_id": "019f0044-0000-7000-8000-000000000002",
            "attempt_id": "019f0044-0000-7000-8000-000000000003",
            "context_budget": 260,
            "lease_seconds": 30
        }),
    )
    .await;
    for field in ["claim_id", "lease_expires_at", "revision", "handoff"] {
        assert_eq!(
            retried[field], claimed[field],
            "an identical retry must replay the claim transition ({field}): {retried}"
        );
    }
    assert_eq!(
        retried["package"]["entries"][0]["context_ref"], explicit_ref,
        "a revisioned reference is pinned, so it survives the re-assembly: {retried}"
    );
    assert!(
        retried["trace"]["candidate_count"].as_u64().unwrap()
            > claimed["trace"]["candidate_count"].as_u64().unwrap(),
        "the retry must assemble against current evidence, not a frozen package: \
         {retried}"
    );
}

/// Tracer for the claim-side retrieval query (#44): handoff text reaches the
/// store as prose, never as an FTS5 expression.
///
/// Pre-quoting it would look like explicit FTS5 syntax to `route_fts_query`,
/// which then skips term routing — and a 1–2 character CJK term, the most
/// common shape of a Chinese query, has no unicode61 leg that can match it.
/// Generated prose can also carry those triggers by accident, and a parse
/// failure lands after the compare-and-set, on an already-claimed Handoff.
#[tokio::test]
async fn handoff_text_routes_as_prose_at_claim() {
    use engram_core::{NewPage, PagePath, Tier};

    let tmp = TempDir::new().unwrap();
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
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/continuity.md").unwrap(),
            title: "交接契约".to_string(),
            // One glued unicode61 token: only the LIKE leg can match a
            // two-character term inside it.
            body: "本次交接契约回归验收的结论与后续步骤。".to_string(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
        })
        .await
        .unwrap();

    let router = router_for_store(&store, ws, proj, ActorContext::anonymous(), false);
    let published = call_tool(
        &router,
        210,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0044-0000-7000-8000-000000000011",
            "objective": "继续交接契约回归",
            "summary": "契约 回归",
        }),
    )
    .await;
    let claimed = call_tool(
        &router,
        211,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": published["handoff_id"],
            "expected_revision": 1,
            "run_id": "019f0044-0000-7000-8000-000000000012",
            "attempt_id": "019f0044-0000-7000-8000-000000000013",
            "context_budget": 4096,
            "lease_seconds": 30
        }),
    )
    .await;

    assert!(
        claimed["package"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["page_path"] == "notes/continuity.md"),
        "a multi-term CJK handoff must still find continuation candidates: {claimed}"
    );

    // Prose carrying FTS5 grammar: a bare `NOT` cannot open an FTS5
    // expression, and parentheses/quotes are query syntax. Claim assembly runs
    // after the compare-and-set, so a parse failure here would hand back an
    // error on a Handoff that stays claimed until release or lease expiry.
    let awkward = call_tool(
        &router,
        212,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0044-0000-7000-8000-000000000014",
            "objective": "Ship the continuity work",
            "summary": "NOT ready: the \"context package\" (#44) needs review AND a rerun",
            "next_steps": ["re-run the gate (all four)"],
        }),
    )
    .await;
    let awkward_claim = call_tool_outcome(
        &router,
        213,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": awkward["handoff_id"],
            "expected_revision": 1,
            "run_id": "019f0044-0000-7000-8000-000000000015",
            "attempt_id": "019f0044-0000-7000-8000-000000000016",
            "context_budget": 4096,
            "lease_seconds": 30
        }),
    )
    .await;
    let awkward_claim = awkward_claim
        .unwrap_or_else(|error| panic!("prose must not be parsed as an FTS5 expression: {error}"));
    assert_eq!(
        awkward_claim["handoff"]["state"], "claimed",
        "the claim must survive prose that looks like query syntax: {awkward_claim}"
    );
}

/// Issue #42: a live Claim's first receiving checkpoint may attach
/// ArtifactRefs and WorkItem relationships. A stranger without a claim is
/// still independently unauthorized and must not mutate the graph.
#[tokio::test]
async fn receiver_first_checkpoint_can_attach_artifacts_and_relationships() {
    let tmp = TempDir::new().unwrap();
    let owner = ActorContext {
        agent: Some("claude-code".into()),
        user: Some("owner-user".into()),
        ..ActorContext::default()
    };
    let receiver = ActorContext {
        agent: Some("codex".into()),
        user: Some("receiver-user".into()),
        ..ActorContext::default()
    };
    let stranger = ActorContext {
        agent: Some("opencode".into()),
        user: Some("stranger-user".into()),
        ..ActorContext::default()
    };
    let (owner_router, store) = make_router_for_actor(&tmp, false, owner).await;
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
    let receiver_router = router_for_store(&store, ws, proj, receiver, false);
    let stranger_router = router_for_store(&store, ws, proj, stranger, false);

    let owner_run = "019f0042-0000-7000-8000-000000000080";
    let receiving_run = "019f0042-0000-7000-8000-000000000081";
    let published = call_tool(
        &owner_router,
        200,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": owner_run,
            "objective": "Receiver ack with evidence",
            "acceptance_criteria": ["first checkpoint carries relationships"],
            "summary": "Owner published work for another agent to claim."
        }),
    )
    .await;
    let dependency = call_tool(
        &owner_router,
        201,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000082",
            "objective": "Upstream evidence",
            "summary": "Target of the receiver's depends_on link."
        }),
    )
    .await;
    let claimed = call_tool(
        &receiver_router,
        202,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": published["handoff_id"],
            "expected_revision": 1,
            "run_id": receiving_run,
            "attempt_id": "019f0042-0000-7000-8000-000000000083",
            "context_budget": 4096
        }),
    )
    .await;
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let count_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM work_item_relationships", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count_before, 0);

    let stranger_error = call_tool_failure(
        &stranger_router,
        203,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": published["work_item_id"],
            "run_id": "019f0042-0000-7000-8000-000000000084",
            "attempt_id": "019f0042-0000-7000-8000-000000000085",
            "expected_work_item_revision": 1,
            "summary": "Stranger tried to attach a relationship without a claim.",
            "work_item_state": "active",
            "acceptance_criteria": [
                {"criterion": "first checkpoint carries relationships", "satisfied": false}
            ],
            "relationships": [{
                "kind": "depends_on",
                "target_work_item_id": dependency["work_item_id"]
            }]
        }),
    )
    .await;
    assert!(
        stranger_error.to_lowercase().contains("unauthorized"),
        "{stranger_error}"
    );
    let count_after_stranger: i64 = conn
        .query_row("SELECT COUNT(*) FROM work_item_relationships", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count_after_stranger, count_before);

    let checkpoint = call_tool(
        &receiver_router,
        204,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": published["work_item_id"],
            "run_id": receiving_run,
            "attempt_id": "019f0042-0000-7000-8000-000000000086",
            "expected_work_item_revision": 1,
            "handoff_id": published["handoff_id"],
            "claim_id": claimed["claim_id"],
            "expected_handoff_revision": claimed["revision"],
            "summary": "Receiver acknowledged with artifact and relationship evidence.",
            "work_item_state": "active",
            "acceptance_criteria": [
                {"criterion": "first checkpoint carries relationships", "satisfied": false}
            ],
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "ack123",
                "commit_id": "ack123",
                "git_ref": "feature",
                "provenance": "receiver ack",
                "local_path_hint": "/tmp/receiver/engram"
            }],
            "relationships": [{
                "kind": "depends_on",
                "target_work_item_id": dependency["work_item_id"]
            }]
        }),
    )
    .await;
    assert_eq!(checkpoint["handoff_state"], "acknowledged");
    assert_eq!(checkpoint["artifacts"][0]["kind"], "git");
    assert_eq!(checkpoint["artifacts"][0]["git_ref"], "feature");
    assert_eq!(checkpoint["artifacts"][0]["provenance"], "receiver ack");
    assert_eq!(checkpoint["relationships"][0]["kind"], "depends_on");
    assert_eq!(
        checkpoint["relationships"][0]["from_work_item_id"],
        published["work_item_id"]
    );
    assert_eq!(
        checkpoint["relationships"][0]["to_work_item_id"],
        dependency["work_item_id"]
    );
    let count_after_receiver: i64 = conn
        .query_row("SELECT COUNT(*) FROM work_item_relationships", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count_after_receiver, 1);
}

/// Issue #42: shared artifact identity survives purging the first writer's
/// project; the remaining project's attachment and observation stay intact.
///
/// Identity is repository plus revision, so the two projects observe the same
/// object from two different absolute checkouts and must land on one id. That
/// shared row is what made the project-level CASCADE dangerous: deleting the
/// first writer's project would have taken the second project's evidence with
/// it.
#[tokio::test]
async fn purging_first_writer_project_keeps_shared_artifact_for_other_scope() {
    let tmp = TempDir::new().unwrap();
    let (router, store) = make_router(&tmp, false).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let project_a = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let project_b = store
        .writer
        .get_or_create_project(ws, "sibling", None)
        .await
        .unwrap();
    // A lifecycle observation in the surviving project: the purge must not
    // reach it through the shared identity row either.
    let session_b = engram_core::SessionId::new();
    store
        .writer
        .begin_session(engram_core::NewSession {
            id: session_b,
            workspace_id: ws,
            project_id: project_b,
            agent_kind: engram_core::AgentKind::Codex,
            cwd: Some("/tmp/project-b/engram".into()),
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(engram_core::NewObservation {
            session_id: session_b,
            workspace_id: ws,
            project_id: project_b,
            kind: engram_core::ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "sibling ledger".into(),
            body: "sibling ledger evidence recorded before the purge".into(),
            importance: 5,
        })
        .await
        .unwrap();

    let first = call_tool(
        &router,
        210,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000090",
            "objective": "Project A writer",
            "summary": "First project attaches the shared git object.",
            "cwd": "/tmp/project-a/engram",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "share123",
                "commit_id": "share123",
                "git_ref": "main",
                "tree_hash": "tree-a",
                "dirty": false,
                "local_path_hint": "/tmp/project-a/engram",
                "provenance": "project a"
            }]
        }),
    )
    .await;
    let second = call_tool(
        &router,
        211,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-000000000091",
            "objective": "Project B writer",
            "summary": "Second project attaches the same git object.",
            "workspace": "default",
            "project": "sibling",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "share123",
                "commit_id": "share123",
                "git_ref": "feature",
                "tree_hash": "tree-b",
                "dirty": true,
                "local_path_hint": "/tmp/project-b/engram",
                "provenance": "project b"
            }]
        }),
    )
    .await;
    assert_eq!(first["artifacts"][0]["id"], second["artifacts"][0]["id"]);

    store
        .writer
        .purge_project(ws, project_a, "default/scratch")
        .await
        .unwrap();

    let rediscovered = call_tool(
        &router,
        212,
        "memory_handoff_discover",
        serde_json::json!({
            "workspace": "default",
            "project": "sibling"
        }),
    )
    .await;
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["id"],
        second["artifacts"][0]["id"]
    );
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["provenance"],
        "project b"
    );
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["git_ref"],
        "feature"
    );
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["tree_hash"],
        "tree-b"
    );
    assert_eq!(rediscovered["handoff"]["artifacts"][0]["dirty"], true);
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["local_path_hint"],
        "/tmp/project-b/engram"
    );
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["source_run_id"],
        "019f0042-0000-7000-8000-000000000091"
    );

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let attachments: i64 = conn
        .query_row("SELECT COUNT(*) FROM artifact_attachments", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(attachments, 1);
    let identities: i64 = conn
        .query_row("SELECT COUNT(*) FROM artifacts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(identities, 1);

    let recalled = call_tool(
        &router,
        213,
        "memory_query",
        serde_json::json!({
            "query": "sibling ledger",
            "workspace": "default",
            "project": "sibling",
            "context_budget": 4096
        }),
    )
    .await;
    assert!(
        recalled["package"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["title"] == "sibling ledger"),
        "the surviving project's observation must still be readable: {recalled}"
    );
}

/// Issue #42: git_ref / tree_hash / content_hash are per observation. A later
/// observer must not inherit the first writer's branch or hashes.
#[tokio::test]
async fn later_git_observer_does_not_inherit_first_writer_ref_or_hash() {
    let tmp = TempDir::new().unwrap();
    let (router, store) = make_router(&tmp, false).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let _sibling = store
        .writer
        .get_or_create_project(ws, "sibling", None)
        .await
        .unwrap();

    let first = call_tool(
        &router,
        220,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-0000000000a0",
            "objective": "First git observer",
            "summary": "Observed main at tree A.",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "obs123",
                "commit_id": "obs123",
                "git_ref": "main",
                "tree_hash": "tree-a",
                "local_path_hint": "/tmp/machine-a/engram",
                "provenance": "first observer"
            }]
        }),
    )
    .await;
    let second = call_tool(
        &router,
        221,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-0000000000a1",
            "objective": "Second git observer",
            "summary": "Observed feature at tree B.",
            "workspace": "default",
            "project": "sibling",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "obs123",
                "commit_id": "obs123",
                "git_ref": "feature",
                "tree_hash": "tree-b",
                "local_path_hint": "/Users/other/machine-b/engram",
                "provenance": "second observer"
            }]
        }),
    )
    .await;
    assert_eq!(first["artifacts"][0]["id"], second["artifacts"][0]["id"]);
    assert_eq!(second["artifacts"][0]["git_ref"], "feature");
    assert_eq!(second["artifacts"][0]["tree_hash"], "tree-b");
    assert_eq!(
        second["artifacts"][0]["local_path_hint"],
        "/Users/other/machine-b/engram"
    );
    assert_eq!(first["artifacts"][0]["git_ref"], "main");
    assert_eq!(first["artifacts"][0]["tree_hash"], "tree-a");

    let rediscovered = call_tool(
        &router,
        222,
        "memory_handoff_discover",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(rediscovered["handoff"]["artifacts"][0]["git_ref"], "main");
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["tree_hash"],
        "tree-a"
    );
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["local_path_hint"],
        "/tmp/machine-a/engram"
    );
    assert_eq!(
        rediscovered["handoff"]["artifacts"][0]["provenance"],
        "first observer"
    );
}

/// Issue #42: worktree locators reject absolute filesystem paths; git identity
/// still ignores differing cwd hints.
#[tokio::test]
async fn worktree_locator_rejects_absolute_path_git_identity_ignores_cwd_hint() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let absolute = call_tool_failure(
        &router,
        230,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-0000000000b0",
            "objective": "Absolute worktree locator",
            "summary": "Absolute cwd must not become worktree identity.",
            "artifacts": [{
                "kind": "worktree",
                "locator": "/tmp/machine-a/engram",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "abc123",
                "local_path_hint": "/tmp/machine-a/engram"
            }]
        }),
    )
    .await;
    assert!(
        absolute.to_lowercase().contains("absolute")
            || absolute.to_lowercase().contains("local_path_hint"),
        "{absolute}"
    );

    let machine_a = call_tool(
        &router,
        231,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-0000000000b1",
            "objective": "Git hint A",
            "summary": "First cwd hint.",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "cwd123",
                "commit_id": "cwd123",
                "local_path_hint": "/tmp/machine-a/engram"
            }]
        }),
    )
    .await;
    let machine_b = call_tool(
        &router,
        232,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": "019f0042-0000-7000-8000-0000000000b2",
            "objective": "Git hint B",
            "summary": "Second cwd hint.",
            "artifacts": [{
                "kind": "git",
                "locator": "origin",
                "repository_identity": "github.com/semantic-craft/engram",
                "observed_revision": "cwd123",
                "commit_id": "cwd123",
                "local_path_hint": "/Users/other/machine-b/engram"
            }]
        }),
    )
    .await;
    assert_eq!(
        machine_a["artifacts"][0]["id"],
        machine_b["artifacts"][0]["id"]
    );
    assert_eq!(
        machine_a["artifacts"][0]["local_path_hint"],
        "/tmp/machine-a/engram"
    );
    assert_eq!(
        machine_b["artifacts"][0]["local_path_hint"],
        "/Users/other/machine-b/engram"
    );
}

/// Issue #43 tracer: one WorkItem crosses three Runs. Each hop publishes a
/// successor from the state it actually received, the transfer it replaces
/// becomes readable history instead of being overwritten, discovery only ever
/// offers the newest non-superseded transfer, and a terminal WorkItem refuses
/// further transfers.
#[tokio::test]
async fn successor_handoffs_chain_across_three_runs_without_overwriting_history() {
    let tmp = TempDir::new().unwrap();
    let alice = ActorContext {
        agent: Some("claude-code".into()),
        user: Some("alice".into()),
        ..ActorContext::default()
    };
    let (router_a, store) = make_router_for_actor(&tmp, false, alice).await;
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
    let router_b = router_for_store(
        &store,
        ws,
        proj,
        ActorContext {
            agent: Some("codex".into()),
            user: Some("bob".into()),
            ..ActorContext::default()
        },
        false,
    );
    let router_c = router_for_store(
        &store,
        ws,
        proj,
        ActorContext {
            agent: Some("cursor".into()),
            user: Some("cara".into()),
            ..ActorContext::default()
        },
        false,
    );

    let run_a = "019f0043-0000-7000-8000-000000000001";
    let run_b = "019f0043-0000-7000-8000-000000000002";
    let run_c = "019f0043-0000-7000-8000-000000000003";
    let first_objective = "Chain successors without losing history";

    let first = call_tool(
        &router_a,
        100,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": run_a,
            "objective": first_objective,
            "acceptance_criteria": ["chain is readable", "history is immutable"],
            "summary": "Alice opened the work."
        }),
    )
    .await;
    let work_item_id = first["work_item_id"].as_str().unwrap().to_string();
    let h1 = first["handoff_id"].as_str().unwrap().to_string();
    assert_eq!(first["work_item_revision"], 1);
    assert!(first["predecessor_handoff_id"].is_null());
    assert_eq!(first["superseded_handoff_ids"], serde_json::json!([]));

    // Alice replaces her own still-unclaimed offer. The successor names its
    // predecessor and supersedes exactly that one transfer.
    let second = call_tool(
        &router_a,
        101,
        "memory_handoff_begin",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": run_a,
            "expected_work_item_revision": 1,
            "summary": "Alice re-scoped before anyone claimed."
        }),
    )
    .await;
    let h2 = second["handoff_id"].as_str().unwrap().to_string();
    assert_eq!(second["work_item_revision"], 2);
    assert_eq!(second["predecessor_handoff_id"], serde_json::json!(h1));
    assert!(
        second["source_checkpoint_id"].is_null(),
        "no checkpoint yet"
    );
    assert_eq!(
        second["superseded_handoff_ids"],
        serde_json::json!([h1]),
        "the unclaimed predecessor is superseded, not overwritten"
    );

    let bob_sees = call_tool(
        &router_b,
        102,
        "memory_handoff_discover",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        bob_sees["handoff"]["id"], h2,
        "discovery never offers a superseded transfer"
    );
    let chain = bob_sees["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 2, "{chain:?}");
    assert_eq!(chain[0]["handoff_id"], serde_json::json!(h1));
    assert_eq!(chain[0]["state"], "superseded");
    assert_eq!(chain[0]["superseded_by_handoff_id"], serde_json::json!(h2));
    assert_eq!(chain[1]["state"], "open");

    let bob_claim = call_tool(
        &router_b,
        103,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": h2,
            "expected_revision": 1,
            "run_id": run_b,
            "attempt_id": "019f0043-0000-7000-8000-000000000010",
            "lease_seconds": 60,
            "context_budget": 4096
        }),
    )
    .await;
    let bob_checkpoint = call_tool(
        &router_b,
        104,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": run_b,
            "attempt_id": "019f0043-0000-7000-8000-000000000011",
            "expected_work_item_revision": 2,
            "handoff_id": h2,
            "claim_id": bob_claim["claim_id"],
            "expected_handoff_revision": 2,
            "summary": "Bob durably received and advanced the work.",
            "work_item_state": "active",
            "acceptance_criteria": [
                {"criterion": "chain is readable", "satisfied": true},
                {"criterion": "history is immutable", "satisfied": false}
            ]
        }),
    )
    .await;
    assert_eq!(bob_checkpoint["handoff_state"], "acknowledged");
    assert_eq!(bob_checkpoint["work_item_revision"], 3);

    let third = call_tool(
        &router_b,
        105,
        "memory_handoff_begin",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": run_b,
            "expected_work_item_revision": 3,
            "expected_checkpoint_revision": 3,
            "summary": "Bob handed the rest to Cara."
        }),
    )
    .await;
    let h3 = third["handoff_id"].as_str().unwrap().to_string();
    assert_eq!(third["predecessor_handoff_id"], serde_json::json!(h2));
    assert_eq!(
        third["source_checkpoint_id"],
        bob_checkpoint["checkpoint_id"]
    );
    assert_eq!(third["source_checkpoint_revision"], 3);
    assert_eq!(
        third["superseded_handoff_ids"],
        serde_json::json!([]),
        "an acknowledged predecessor is history, never superseded"
    );

    // A stale Checkpoint revision must not mutate anything.
    let stale = call_tool_failure(
        &router_b,
        106,
        "memory_handoff_begin",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": run_b,
            "expected_work_item_revision": 4,
            "expected_checkpoint_revision": 2,
            "summary": "Constructed from a Checkpoint that is no longer latest."
        }),
    )
    .await;
    assert!(stale.contains("stale checkpoint revision"), "{stale}");

    // Retention runs; a predecessor referenced by a successor survives it.
    call_tool(
        &router_c,
        107,
        "memory_forget_sweep",
        serde_json::json!({ "dry_run": false }),
    )
    .await;

    let cara_sees = call_tool(
        &router_c,
        108,
        "memory_handoff_discover",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        cara_sees["handoff"]["id"], h3,
        "stale publish changed nothing"
    );
    // The envelope carries what is still outstanding and the exact revision a
    // further successor must assert, without copying canonical bodies.
    assert_eq!(cara_sees["work_item"]["objective"], first_objective);
    assert_eq!(cara_sees["latest_checkpoint"]["work_item_revision"], 3);
    assert_eq!(
        cara_sees["latest_checkpoint"]["acceptance_criteria"],
        serde_json::json!([
            {"criterion": "chain is readable", "satisfied": true},
            {"criterion": "history is immutable", "satisfied": false}
        ])
    );
    let chain = cara_sees["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 3, "predecessors survive retention: {chain:?}");
    let ids: Vec<&str> = chain
        .iter()
        .map(|e| e["handoff_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![h1.as_str(), h2.as_str(), h3.as_str()]);
    assert_eq!(chain[2]["predecessor_handoff_id"], serde_json::json!(h2));
    // Target selector, authenticated actor, Run, and execution agent stay
    // separate dimensions on every hop.
    assert_eq!(chain[1]["source_actor"], "user:alice");
    assert_eq!(chain[1]["source_run_id"], run_a);
    assert_eq!(chain[1]["from_agent"], "claude-code");
    assert!(chain[1]["to_agent"].is_null());
    assert_eq!(chain[1]["receiving_actor"], "user:bob");
    assert_eq!(chain[1]["receiving_run_id"], run_b);
    assert_eq!(chain[1]["receiving_claim_state"], "acknowledged");
    assert!(chain[2]["receiving_actor"].is_null(), "h3 is unclaimed");

    let cara_claim = call_tool(
        &router_c,
        109,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": h3,
            "expected_revision": 1,
            "run_id": run_c,
            "attempt_id": "019f0043-0000-7000-8000-000000000012",
            "lease_seconds": 60,
            "context_budget": 4096
        }),
    )
    .await;
    let completed = call_tool(
        &router_c,
        110,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": run_c,
            "attempt_id": "019f0043-0000-7000-8000-000000000013",
            "expected_work_item_revision": 4,
            "handoff_id": h3,
            "claim_id": cara_claim["claim_id"],
            "expected_handoff_revision": 2,
            "summary": "Cara finished the work.",
            "work_item_state": "completed",
            "acceptance_criteria": [
                {"criterion": "chain is readable", "satisfied": true},
                {"criterion": "history is immutable", "satisfied": true}
            ]
        }),
    )
    .await;
    assert_eq!(completed["work_item_state"], "completed");

    // Terminal is terminal: the WorkItem cannot be checkpointed back to
    // `active` to route around the publish-side rejection below.
    let reopen = call_tool_failure(
        &router_c,
        113,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": run_c,
            "attempt_id": "019f0043-0000-7000-8000-000000000014",
            "expected_work_item_revision": completed["work_item_revision"],
            "summary": "Reopening finished work.",
            "work_item_state": "active",
            "acceptance_criteria": [
                {"criterion": "chain is readable", "satisfied": true},
                {"criterion": "history is immutable", "satisfied": true}
            ]
        }),
    )
    .await;
    assert!(
        reopen.contains("terminal work item state"),
        "a completed WorkItem must not be reopened: {reopen}"
    );

    let terminal = call_tool_failure(
        &router_c,
        111,
        "memory_handoff_begin",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": run_c,
            "expected_work_item_revision": completed["work_item_revision"],
            "expected_checkpoint_revision": completed["work_item_revision"],
            "summary": "Follow-up work on a finished WorkItem."
        }),
    )
    .await;
    assert!(
        terminal.contains("terminal work item state"),
        "follow-up must create a related WorkItem instead: {terminal}"
    );

    // The explicit relationship contract is the supported follow-up path.
    let follow_up = call_tool(
        &router_c,
        112,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": run_c,
            "objective": "Follow-up derived from the finished work",
            "summary": "New WorkItem, explicit relationship.",
            "relationships": [
                {"kind": "derived_from", "target_work_item_id": work_item_id}
            ]
        }),
    )
    .await;
    assert_ne!(follow_up["work_item_id"], serde_json::json!(work_item_id));
    assert_eq!(follow_up["relationships"][0]["kind"], "derived_from");
}

/// Issue #43 tracer: two Runs publishing a successor at the same revision
/// produce exactly one successor and one explicit conflict, and the four ways
/// a transfer can end stay distinguishable in `audit_log`.
#[tokio::test]
async fn concurrent_successors_conflict_and_transfer_outcomes_stay_distinct() {
    let tmp = TempDir::new().unwrap();
    let owner = ActorContext {
        agent: Some("claude-code".into()),
        user: Some("owner".into()),
        ..ActorContext::default()
    };
    let (router, store) = make_router_for_actor(&tmp, false, owner).await;
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
    let receiver = router_for_store(
        &store,
        ws,
        proj,
        ActorContext {
            agent: Some("codex".into()),
            user: Some("receiver".into()),
            ..ActorContext::default()
        },
        false,
    );

    let owner_run = "019f0043-0000-7000-8000-000000000020";
    let receiver_run = "019f0043-0000-7000-8000-000000000021";
    let published = call_tool(
        &router,
        120,
        "memory_handoff_begin",
        serde_json::json!({
            "run_id": owner_run,
            "objective": "Race two successors",
            "acceptance_criteria": ["exactly one successor wins"],
            "summary": "Owner opened the work."
        }),
    )
    .await;
    let work_item_id = published["work_item_id"].as_str().unwrap().to_string();
    let first_handoff = published["handoff_id"].as_str().unwrap().to_string();

    call_tool(
        &router,
        121,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": owner_run,
            "attempt_id": "019f0043-0000-7000-8000-000000000022",
            "expected_work_item_revision": 1,
            "summary": "Owner recorded durable progress.",
            "work_item_state": "active",
            "acceptance_criteria": [
                {"criterion": "exactly one successor wins", "satisfied": false}
            ]
        }),
    )
    .await;

    let successor_args = |marker: &str| {
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": owner_run,
            "expected_work_item_revision": 2,
            "expected_checkpoint_revision": 2,
            "summary": format!("Successor attempt {marker}.")
        })
    };
    let (left, right) = tokio::join!(
        call_tool_outcome(&router, 122, "memory_handoff_begin", successor_args("left")),
        call_tool_outcome(
            &router,
            123,
            "memory_handoff_begin",
            successor_args("right")
        ),
    );
    let (winner, conflict) = match (left, right) {
        (Ok(winner), Err(conflict)) | (Err(conflict), Ok(winner)) => (winner, conflict),
        outcomes => panic!("same-revision successors must yield one of each: {outcomes:?}"),
    };
    assert!(conflict.contains("stale work item revision"), "{conflict}");
    assert_eq!(
        winner["superseded_handoff_ids"],
        serde_json::json!([first_handoff])
    );
    let successor = winner["handoff_id"].as_str().unwrap().to_string();

    let missing = call_tool_failure(
        &router,
        124,
        "memory_handoff_begin",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": owner_run,
            "summary": "No expected revisions at all."
        }),
    )
    .await;
    assert!(missing.contains("expected_work_item_revision"), "{missing}");

    // Claim, let the lease lapse, reclaim (lease expiry), then release.
    call_tool(
        &receiver,
        125,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": successor,
            "expected_revision": 1,
            "run_id": receiver_run,
            "attempt_id": "019f0043-0000-7000-8000-000000000023",
            "lease_seconds": 1,
            "context_budget": 4096
        }),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let reclaimed = call_tool(
        &receiver,
        126,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": successor,
            "expected_revision": 2,
            "run_id": receiver_run,
            "attempt_id": "019f0043-0000-7000-8000-000000000024",
            "lease_seconds": 60,
            "context_budget": 4096
        }),
    )
    .await;
    let released = call_tool(
        &receiver,
        127,
        "memory_handoff_release",
        serde_json::json!({
            "handoff_id": successor,
            "claim_id": reclaimed["claim_id"],
            "expected_revision": reclaimed["revision"],
            "run_id": receiver_run,
            "attempt_id": "019f0043-0000-7000-8000-000000000025"
        }),
    )
    .await;
    assert_eq!(released["state"], "open");

    let cancelled = call_tool(
        &router,
        128,
        "memory_handoff_cancel",
        serde_json::json!({
            "handoff_id": successor,
            "expected_revision": released["revision"],
            "run_id": owner_run
        }),
    )
    .await;
    assert_eq!(
        cancelled["state"], "cancelled",
        "cancellation is its own terminal state, not a lapsed offer"
    );

    // Completing the WorkItem retires whatever transfer is still open, so no
    // receiver can claim a lease against finished work.
    let final_open = call_tool(
        &router,
        129,
        "memory_handoff_begin",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": owner_run,
            "expected_work_item_revision": 3,
            "expected_checkpoint_revision": 2,
            "summary": "Still outstanding when the work finishes."
        }),
    )
    .await;
    call_tool(
        &router,
        130,
        "memory_checkpoint_write",
        serde_json::json!({
            "work_item_id": work_item_id,
            "run_id": owner_run,
            "attempt_id": "019f0043-0000-7000-8000-000000000026",
            "expected_work_item_revision": 4,
            "summary": "Owner finished the work.",
            "work_item_state": "completed",
            "acceptance_criteria": [
                {"criterion": "exactly one successor wins", "satisfied": true}
            ]
        }),
    )
    .await;
    let nothing_pending = call_tool(
        &router,
        131,
        "memory_handoff_discover",
        serde_json::json!({}),
    )
    .await;
    assert!(
        nothing_pending["handoff"].is_null(),
        "a finished WorkItem leaves nothing claimable: {nothing_pending}"
    );
    let claim_terminal = call_tool_failure(
        &receiver,
        132,
        "memory_handoff_claim",
        serde_json::json!({
            "handoff_id": final_open["handoff_id"],
            "expected_revision": 1,
            "run_id": receiver_run,
            "attempt_id": "019f0043-0000-7000-8000-000000000027",
            "lease_seconds": 60,
            "context_budget": 4096
        }),
    )
    .await;
    assert!(
        claim_terminal.contains("work item is completed"),
        "{claim_terminal}"
    );

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let mut stmt = conn
        .prepare("SELECT op, detail FROM audit_log WHERE op LIKE 'handoff%'")
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (op, outcome) in [
        ("handoff_supersede", "superseded"),
        ("handoff_cancel", "cancelled"),
        ("handoff_release", "released"),
        ("handoff_claim_expire", "expired"),
        ("handoff_expire_terminal", "expired"),
    ] {
        assert!(
            rows.iter().any(|(logged_op, detail)| logged_op == op
                && detail.contains(&format!("\"outcome\":\"{outcome}\""))),
            "audit must distinguish {op}/{outcome}: {rows:?}"
        );
    }
}
