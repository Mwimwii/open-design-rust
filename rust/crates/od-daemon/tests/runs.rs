//! Integration tests for the ten `/api/runs*` routes plus `POST /api/chat`
//! (parity with `apps/daemon/src/routes/runs.ts` and `runtimes/runs.ts`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use od_core::RuntimePaths;
use od_daemon::routes::build_router;
use od_daemon::{AppState, DaemonConfig, Store};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Fixture {
    router: Router,
    store: Store,
    data_root: PathBuf,
}

fn setup_state(tag: &str, shutting_down: bool) -> (Router, Store, PathBuf) {
    let data_root =
        std::env::temp_dir().join(format!("od-daemon-runs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_root);
    let paths = RuntimePaths::resolve(&data_root).expect("data dir");
    let config = DaemonConfig {
        paths,
        bind_host: "127.0.0.1".to_string(),
        port: 0,
        api_token: None,
        api_auth_disabled: false,
        web_dist: None,
    };
    let store = Store::open(&config.paths.db_file()).expect("store");
    store
        .execute(
            "INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'Demo', 1, 2)",
            (),
        )
        .expect("insert p1");
    let state = AppState::new(config, store.clone());
    state.shutting_down.store(shutting_down, Ordering::SeqCst);
    (build_router(state), store, data_root)
}

fn setup(tag: &str) -> Fixture {
    let (router, store, data_root) = setup_state(tag, false);
    Fixture {
        router,
        store,
        data_root,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data_root);
    }
}

/// Issue one request without consuming the response body — SSE routes stream
/// until the run finishes, so the caller decides when to read.
async fn request(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&'static str, &'static str)],
) -> (StatusCode, HeaderMap, Body) {
    let peer = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .extension(axum::extract::ConnectInfo(peer));
    let bytes = match &body {
        Some(body) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            serde_json::to_vec(body).expect("serialize")
        }
        None => Vec::new(),
    };
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(bytes)).expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    (response.status(), response.headers().clone(), response.into_body())
}

async fn send(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&'static str, &'static str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let (status, response_headers, response_body) =
        request(router, method, uri, body, headers).await;
    let payload = to_bytes(response_body, usize::MAX).await.expect("body");
    (status, response_headers, payload.to_vec())
}

/// JSON routes: assert the envelope content type and decode the payload.
async fn call(router: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, headers, payload) = send(router, method, uri, body, &[]).await;
    assert_eq!(
        headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "{uri}"
    );
    if payload.is_empty() {
        (status, Value::Null)
    } else {
        (status, serde_json::from_slice(&payload).expect("json body"))
    }
}

async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
    call(router, "GET", uri, None).await
}

async fn post(router: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    call(router, "POST", uri, Some(body)).await
}

/// A terminal run's SSE body, read to completion (parity: `to_bytes` on the
/// `AsyncRead`-backed stream returns as soon as the feed closes).
async fn sse(router: &Router, uri: &str) -> (StatusCode, String) {
    let (status, headers, payload) = send(router, "GET", uri, None, &[]).await;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("text/event-stream"),
        "{uri} answered {content_type}"
    );
    (
        status,
        String::from_utf8_lossy(&payload).into_owned(),
    )
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

fn message_of(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or("")
}

fn seed_conversation(fixture: &Fixture, id: &str, project_id: &str, created_at: i64) {
    fixture
        .store
        .execute(
            "INSERT INTO conversations (id, project_id, title, session_mode, created_at, updated_at)
             VALUES (?1, ?2, 'Seed', 'design', ?3, ?3)",
            rusqlite::params![id, project_id, created_at],
        )
        .expect("conversation");
}

fn seed_message(
    fixture: &Fixture,
    id: &str,
    conversation_id: &str,
    role: &str,
    content: &str,
    created_at: i64,
    position: i64,
) {
    fixture
        .store
        .execute(
            "INSERT INTO messages (id, conversation_id, role, content, created_at, position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, conversation_id, role, content, created_at, position],
        )
        .expect("message");
}

/// `POST /api/runs` → 202 + the run id, then the status projection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_run_answers_202_and_projects_the_running_status() {
    let fixture = setup("create-shape");
    let (status, headers, payload) = send(
        &fixture.router,
        "POST",
        "/api/runs",
        Some(json!({})),
        &[("x-od-client", "desktop")],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "POST /api/runs answers 202");
    assert_eq!(
        headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let body: Value = serde_json::from_slice(&payload).expect("json body");
    let run_id = body["runId"].as_str().expect("runId").to_string();
    assert_eq!(
        body,
        json!({
            "runId": run_id,
            "conversationId": null,
            "assistantMessageId": null,
            "clientRequestId": null,
            "reused": false,
            "resumed": false,
        })
    );

    let (status, run) = get(&fixture.router, &format!("/api/runs/{run_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["id"], json!(run_id));
    assert_eq!(run["status"], json!("running"));
    assert_eq!(run["cancelRequested"], json!(false));
    assert_eq!(run["terminalAt"], Value::Null);
    assert_eq!(run["projectId"], Value::Null);
    assert_eq!(run["conversationId"], Value::Null);
    assert_eq!(run["agentId"], Value::Null);
    assert_eq!(run["childExited"], json!(true));
    assert_eq!(run["childPid"], Value::Null);
    assert_eq!(run["eventsLogPath"], Value::Null);
    assert_eq!(run["manualResumeAttemptCount"], json!(0));
    assert_eq!(run["clientType"], json!("desktop"));
    assert_eq!(run["mediaExecution"], json!({"mode": "enabled"}));
    assert_eq!(run["toolBundle"], json!({"mcpServers": []}));
    assert_eq!(
        run["workspace"],
        json!({"storage": {"kind": "od-owned", "baseDir": null}, "provenance": null})
    );
    // A live run has no verdict yet.
    assert_eq!(run.get("deliverableValid"), None);

    let (status, list) = get(&fixture.router, "/api/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["runs"].as_array().expect("array").len(), 1);
    assert_eq!(list["awaitingInputProjectIds"], json!([]));

    // Express parses only JSON bodies; anything else reaches the handler as `{}`.
    let (status, body) = post(&fixture.router, "/api/runs", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["conversationId"], Value::Null);

    let peer = SocketAddr::from(([127, 0, 0, 1], 0));
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/runs")
        .extension(axum::extract::ConnectInfo(peer))
        .body(Body::empty())
        .expect("request");
    let response = fixture.router.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_run_reuses_by_client_request_id_and_conflicts_on_a_new_payload() {
    let fixture = setup("idempotent");
    let first = json!({"clientRequestId": "cr-1", "message": "hello"});

    let (status, body) = post(&fixture.router, "/api/runs", first.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["runId"].as_str().expect("runId").to_string();
    assert_eq!(body["reused"], json!(false));

    let (status, body) = post(&fixture.router, "/api/runs", first.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["runId"], json!(run_id));
    assert_eq!(body["reused"], json!(true));
    assert_eq!(body["resumed"], json!(false));

    // The fingerprint excludes `clientRequestId`, so a changed payload is a
    // different logical request on the same key.
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"clientRequestId": "cr-1", "message": "different"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code_of(&body), "IDEMPOTENCY_CONFLICT");

    let (status, list) = get(&fixture.router, "/api/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["runs"].as_array().expect("array").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_run_rejects_invalid_media_tool_bundle_and_byok_input() {
    let fixture = setup("invalid-create");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"mediaExecution": {"mode": "sometimes"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "BAD_REQUEST");
    assert_eq!(
        message_of(&body),
        "mediaExecution.mode must be enabled or disabled"
    );

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"toolBundle": {"mcpServers": "nope"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "BAD_REQUEST");
    assert_eq!(message_of(&body), "toolBundle.mcpServers must be an array");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"toolBundle": {"mcpServers": [{"id": "1bad", "transport": "stdio"}]}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        message_of(&body),
        "toolBundle.mcpServers[0] is invalid"
    );

    let (status, body) = post(&fixture.router, "/api/runs", json!({"agentId": "byok-opencode"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "VALIDATION_FAILED");
    assert_eq!(
        message_of(&body),
        "byok-opencode runs require a complete BYOK provider configuration"
    );

    // A present-but-default model still fails the completeness check.
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({
            "agentId": "byok-opencode",
            "byokProvider": {"apiKey": "sk-test"},
            "model": "default",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "VALIDATION_FAILED");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({
            "agentId": "byok-opencode",
            "byokProvider": {"apiKey": "sk-test"},
            "model": "gpt-5",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    // A non-object JSON document is a parse failure, not `{}`.
    let (status, body) = post(&fixture.router, "/api/runs", json!("scalar")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "BAD_REQUEST");
    assert_eq!(message_of(&body), "invalid json body");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_run_validates_conversation_ownership_and_message_pins() {
    let fixture = setup("pin-checks");
    seed_conversation(&fixture, "c1", "p1", 1);
    seed_conversation(&fixture, "c2", "p1", 2);
    seed_message(&fixture, "mu1", "c1", "user", "first", 10, 0);
    seed_message(&fixture, "ma1", "c1", "assistant", "answer", 20, 1);
    seed_message(&fixture, "mu2", "c2", "user", "other", 30, 0);

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"conversationId": "c1", "message": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "CONVERSATION_NOT_FOUND");
    assert_eq!(message_of(&body), "conversation not found for project");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "conversationId": "absent"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "CONVERSATION_NOT_FOUND");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "conversationId": "c1", "userMessageId": "bad/id"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(message_of(&body), "userMessageId is invalid");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"assistantMessageId": "ma1"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(message_of(&body), "assistantMessageId requires a conversation");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "conversationId": "c1",
               "userMessageId": "x1", "assistantMessageId": "x1"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        message_of(&body),
        "userMessageId and assistantMessageId must be distinct"
    );

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "conversationId": "c1", "userMessageId": "ma1"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code_of(&body), "INVALID_USER_MESSAGE");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "conversationId": "c1", "assistantMessageId": "mu1"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code_of(&body), "INVALID_ASSISTANT_MESSAGE");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "conversationId": "c1", "userMessageId": "mu2"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code_of(&body), "IDEMPOTENCY_CONFLICT");
    assert_eq!(
        message_of(&body),
        "userMessageId belongs to a different conversation"
    );

    // Happy path: the run binds the conversation and claims a fresh assistant pin.
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "conversationId": "c1", "message": "hello"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["conversationId"], json!("c1"));
    assert!(body["assistantMessageId"].as_str().is_some());
    assert!(body["clientRequestId"].is_null());

    let messages = fixture
        .store
        .query(
            "SELECT role, content FROM messages WHERE conversation_id = 'c1' ORDER BY position",
            (),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("messages");
    assert!(messages.contains(&("user".to_string(), "hello".to_string())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_runs_filters_projects_status_and_awaiting_input() {
    let fixture = setup("list-filters");
    fixture
        .store
        .execute(
            "INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p2', 'Two', 1, 2)",
            (),
        )
        .expect("insert p2");
    fixture
        .store
        .execute(
            "INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p3', 'Three', 1, 2)",
            (),
        )
        .expect("insert p3");
    seed_conversation(&fixture, "c1", "p1", 1);
    seed_conversation(&fixture, "c4", "p3", 2);

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "conversationId": "c1", "agentId": "claude", "message": "one"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_a = body["runId"].as_str().expect("runId").to_string();
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p2"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_b = body["runId"].as_str().expect("runId").to_string();

    let (status, list) = get(&fixture.router, "/api/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["runs"].as_array().expect("array").len(), 2);
    assert_eq!(list["awaitingInputProjectIds"], json!([]));

    let (status, list) = get(&fixture.router, "/api/runs?projectId=p1").await;
    assert_eq!(status, StatusCode::OK);
    let runs = list["runs"].as_array().expect("array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"], json!(run_a));

    let (status, list) = get(&fixture.router, "/api/runs?conversationId=c1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["runs"].as_array().expect("array").len(), 1);

    let (status, list) = get(&fixture.router, "/api/runs?status=running").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["runs"].as_array().expect("array").len(), 2);

    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{run_a}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, list) = get(&fixture.router, "/api/runs?status=canceled").await;
    assert_eq!(status, StatusCode::OK);
    let runs = list["runs"].as_array().expect("array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"], json!(run_a));

    // Parity: `status=active` is the "not terminal yet" pseudo-status.
    let (status, list) = get(&fixture.router, "/api/runs?status=active").await;
    assert_eq!(status, StatusCode::OK);
    let runs = list["runs"].as_array().expect("array");
    assert_eq!(runs.len(), 1, "{list}");
    assert_eq!(runs[0]["id"], json!(run_b));

    let (status, list) = get(&fixture.router, "/api/runs?status=queued").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["runs"], json!([]));

    // A project with no run at all never surfaces, even with an open question.
    seed_message(
        &fixture,
        "q-form",
        "c4",
        "assistant",
        "<question-form>pick one</question-form>",
        9_000_000_000_000,
        0,
    );
    seed_message(
        &fixture,
        "q-form-p1",
        "c1",
        "assistant",
        "<question-form>pick one</question-form>",
        4_000_000_000_000,
        999,
    );
    let (status, list) = get(&fixture.router, "/api/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["awaitingInputProjectIds"], json!(["p1"]));

    let (status, list) = get(&fixture.router, "/api/runs?projectId=p3").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["runs"], json!([]));
    assert_eq!(list["awaitingInputProjectIds"], json!([]));

    // A later user row answers the question.
    seed_message(&fixture, "reply", "c1", "user", "the second one", 5_000_000_000_000, 1_000);
    let (status, list) = get(&fixture.router, "/api/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["awaitingInputProjectIds"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_runs_requires_project_scope_for_workspace_bound_projects() {
    let fixture = setup("workspace-gate");
    fixture
        .store
        .execute(
            "INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p2', 'Two', 1, 2)",
            (),
        )
        .expect("insert p2");
    fixture
        .store
        .execute(
            "INSERT INTO workspace_projects
               (project_id, workspace_id, visibility, resource_state, created_at, updated_at)
             VALUES ('p1', 'ws1', 'team', 'active', 1, 1)",
            (),
        )
        .expect("workspace binding");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "agentId": "claude"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let kept = body["runId"].as_str().expect("runId").to_string();
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "agentId": "amr"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let amr = body["runId"].as_str().expect("runId").to_string();
    let (status, body) = post(&fixture.router, "/api/runs", json!({"projectId": "p1"})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let unknown = body["runId"].as_str().expect("runId").to_string();
    let (status, body) = post(&fixture.router, "/api/runs", json!({"projectId": "p2"})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let unbound = body["runId"].as_str().expect("runId").to_string();

    // No project scope: the workspace-bound run is visible → refuse the feed.
    let (status, body) = get(&fixture.router, "/api/runs").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "PROJECT_SCOPE_REQUIRED");
    assert_eq!(
        message_of(&body),
        "projectId is required when listing Workspace-bound runs"
    );

    // Headerless callers keep only runs whose runtime is known not to bill
    // through AMR's Workspace plane.
    let (status, list) = get(&fixture.router, "/api/runs?projectId=p1").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let runs = list["runs"].as_array().expect("array");
    assert_eq!(runs.len(), 1, "{list}");
    assert_eq!(runs[0]["id"], json!(kept));
    assert_ne!(runs[0]["id"], json!(amr));
    assert_ne!(runs[0]["id"], json!(unknown));

    let (status, list) = get(&fixture.router, "/api/runs?projectId=p2").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let runs = list["runs"].as_array().expect("array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"], json!(unbound));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_run_projects_status_and_the_result_package() {
    let fixture = setup("result-package");

    let (status, body) = get(&fixture.router, "/api/runs/absent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({"error": {"code": "NOT_FOUND", "message": "run not found"}})
    );

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "message": "build it"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["runId"].as_str().expect("runId").to_string();

    let (status, package) = get(
        &fixture.router,
        &format!("/api/runs/{run_id}/result-package"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{package}");
    assert_eq!(
        package["schema"],
        json!("open-design.run-result-package.v1")
    );
    assert_eq!(package["run"]["id"], json!(run_id));
    assert_eq!(package["run"]["status"], json!("running"));
    assert_eq!(package["run"]["projectId"], json!("p1"));
    assert_eq!(package["run"]["cancelRequested"], json!(false));
    for key in [
        "conversationId",
        "assistantMessageId",
        "agentId",
        "createdAt",
        "updatedAt",
        "exitCode",
        "signal",
        "error",
        "errorCode",
    ] {
        assert!(package["run"].get(key).is_some(), "run.{key} missing: {package}");
    }
    assert_eq!(
        package["workspace"],
        json!({"storage": {"kind": "od-owned", "baseDir": null}, "provenance": null})
    );
    assert_eq!(package["events"], json!({"logPath": null}));
    assert_eq!(
        package["project"],
        json!({"id": "p1", "name": "Demo", "fileCount": 0})
    );
    assert_eq!(package["artifacts"], json!([]));

    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{run_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, run) = get(&fixture.router, &format!("/api/runs/{run_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["status"], json!("canceled"));
    assert_eq!(run["cancelRequested"], json!(true));
    assert_eq!(run["signal"], json!("SIGTERM"));
    assert!(run["terminalAt"].as_i64().is_some());
    assert_eq!(run["deliverableValid"], json!(false));
    assert_eq!(run["deliverableValidation"], json!("not_succeeded"));
    assert_eq!(run.get("deliverableEntryFile"), None);

    // A project-less run has no folder to enumerate.
    let (status, body) = post(&fixture.router, "/api/runs", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let orphan = body["runId"].as_str().expect("runId").to_string();
    let (status, package) = get(
        &fixture.router,
        &format!("/api/runs/{orphan}/result-package"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{package}");
    assert_eq!(package["project"], Value::Null);
    assert_eq!(package["artifacts"], json!([]));

    let (status, body) = get(&fixture.router, "/api/runs/absent/result-package").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "NOT_FOUND");
    assert_eq!(message_of(&body), "run not found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_package_enumerates_a_folder_backed_project() {
    let fixture = setup("folder-backed");
    let base = fixture.data_root.join("external");
    std::fs::create_dir_all(&base).expect("external root");
    std::fs::write(base.join("index.html"), "<!doctype html><title>x</title>").expect("write");
    let base = base.canonicalize().expect("canonical");

    fixture
        .store
        .execute(
            "INSERT INTO projects (id, name, metadata_json, created_at, updated_at)
             VALUES ('p2', 'External', ?1, 1, 1)",
            rusqlite::params![json!({"baseDir": base.to_string_lossy()}).to_string()],
        )
        .expect("insert p2");

    let (status, body) = post(&fixture.router, "/api/runs", json!({"projectId": "p2"})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["runId"].as_str().expect("runId").to_string();

    let (status, run) = get(&fixture.router, &format!("/api/runs/{run_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["workspace"]["storage"]["kind"], json!("folder-backed"));
    assert_eq!(run["workspace"]["provenance"]["kind"], json!("user-local"));
    assert_eq!(run["workspace"]["provenance"]["writeback"], json!("in-place"));

    let (status, package) = get(
        &fixture.router,
        &format!("/api/runs/{run_id}/result-package"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{package}");
    assert_eq!(package["project"]["fileCount"], json!(1));
    let artifacts = package["artifacts"].as_array().expect("artifacts");
    assert_eq!(artifacts.len(), 1, "{package}");
    assert_eq!(artifacts[0]["file"], json!("index.html"));
    assert_eq!(artifacts[0]["kind"], json!("html"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_run_is_terminal_and_idempotent() {
    let fixture = setup("cancel");
    let (status, body) = post(&fixture.router, "/api/runs", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["runId"].as_str().expect("runId").to_string();

    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{run_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true));
    let run = &body["run"];
    assert_eq!(run["status"], json!("canceled"));
    assert_eq!(run["cancelRequested"], json!(true));
    assert_eq!(run["cancelOrigin"], json!("user_stop"));
    assert_eq!(run["signal"], json!("SIGTERM"));
    assert_eq!(run["exitCode"], Value::Null);
    assert_eq!(run["resumable"], json!(false));
    let terminal_at = run["terminalAt"].as_i64().expect("terminalAt");

    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{run_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["run"]["status"], json!("canceled"));
    assert_eq!(body["run"]["terminalAt"], json!(terminal_at));

    let (status, body) = post(&fixture.router, "/api/runs/absent/cancel", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "NOT_FOUND");
    assert_eq!(message_of(&body), "run not found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_run_lifecycle_verdicts_and_message_persistence() {
    let fixture = setup("steer");
    seed_conversation(&fixture, "c1", "p1", 1);

    let (status, body) = post(&fixture.router, "/api/runs/absent/steer", json!({"text": "hi"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "NOT_FOUND");

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "conversationId": "c1", "agentId": "claude", "message": "one"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let claude = body["runId"].as_str().expect("runId").to_string();

    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{claude}/steer"),
        json!({"text": "   "}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        message_of(&body),
        "text is required and must be a non-empty string"
    );

    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{claude}/steer"),
        json!({"text": "  keep going  "}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["delivered"], json!(true));
    assert_eq!(body["run"]["status"], json!("running"));

    let texts: Vec<String> = fixture
        .store
        .query(
            "SELECT content FROM messages WHERE conversation_id = 'c1' AND role = 'user'",
            (),
            |row| row.get(0),
        )
        .expect("messages");
    assert!(texts.iter().any(|text| text == "keep going"), "{texts:?}");

    // An agent that never keeps stdin open is refused before the turn state.
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "agentId": "codex"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let codex = body["runId"].as_str().expect("runId").to_string();
    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{codex}/steer"),
        json!({"text": "hello"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code_of(&body), "RUN_STEERING_UNSUPPORTED");
    assert_eq!(body["error"]["retryable"], json!(false));
    assert_eq!(
        body["error"]["details"]["refusal"],
        json!("runtime_unsupported")
    );
    assert_eq!(
        message_of(&body),
        "agent codex cannot take a mid-turn message: its stdin is closed together with the opening prompt"
    );

    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{claude}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{claude}/steer"),
        json!({"text": "too late"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code_of(&body), "RUN_STEERING_CLOSED");
    assert_eq!(body["error"]["retryable"], json!(false));
    assert_eq!(body["error"]["details"]["refusal"], json!("run_terminal"));
    assert_eq!(
        message_of(&body),
        "run already finished; send the message as a new turn"
    );

    // A refused steer leaves no trace, a delivered one is journaled.
    let texts: Vec<String> = fixture
        .store
        .query(
            "SELECT content FROM messages WHERE conversation_id = 'c1' AND role = 'user'",
            (),
            |row| row.get(0),
        )
        .expect("messages");
    assert!(!texts.iter().any(|text| text == "too late"), "{texts:?}");
    assert!(!texts.iter().any(|text| text == "hello"), "{texts:?}");

    let (status, events) = sse(&fixture.router, &format!("/api/runs/{claude}/events")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.contains("event: steering_message"), "{events}");
    assert!(events.contains("event: end"), "{events}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_stream_replays_then_closes() {
    let fixture = setup("events");
    let (status, body) = post(&fixture.router, "/api/runs", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["runId"].as_str().expect("runId").to_string();

    let (status, body) = post(
        &fixture.router,
        &format!("/api/runs/{run_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, events) = sse(&fixture.router, &format!("/api/runs/{run_id}/events")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.contains("event: start"), "{events}");
    assert!(events.contains("event: end"), "{events}");
    assert!(events.starts_with("id: 1\nevent: start\n"), "{events}");

    // `?after=` resumes past the start frame.
    let (status, events) = sse(
        &fixture.router,
        &format!("/api/runs/{run_id}/events?after=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!events.contains("event: start"), "{events}");
    assert!(events.contains("event: end"), "{events}");

    // Reattaching past the tail still receives the terminal signal.
    let (status, events) = sse(
        &fixture.router,
        &format!("/api/runs/{run_id}/events?after=99"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.contains("event: end"), "{events}");

    let (status, body) = get(&fixture.router, "/api/runs/absent/events").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "NOT_FOUND");
    assert_eq!(message_of(&body), "run not found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agui_stream_forwards_only_native_events() {
    let fixture = setup("agui");
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({"projectId": "p1", "agentId": "claude"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["runId"].as_str().expect("runId").to_string();
    post(
        &fixture.router,
        &format!("/api/runs/{run_id}/cancel"),
        json!({}),
    )
    .await;

    let (status, events) = sse(&fixture.router, &format!("/api/runs/{run_id}/agui")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.contains("event: end"), "{events}");
    assert!(events.contains(&format!("\"runId\":\"{run_id}\"")), "{events}");
    assert!(events.contains("\"seq\":"), "{events}");
    // `start` is not part of the AGUI native kind set.
    assert!(!events.contains("event: start"), "{events}");

    let (status, body) = get(&fixture.router, "/api/runs/absent/agui").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "NOT_FOUND");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_route_streams_the_run_instead_of_202() {
    let fixture = setup("chat-stream");
    let (status, headers, body) = request(
        &fixture.router,
        "POST",
        "/api/chat",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the chat route never answers 202");
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );

    let (status, list) = get(&fixture.router, "/api/runs").await;
    assert_eq!(status, StatusCode::OK);
    let runs = list["runs"].as_array().expect("array");
    assert_eq!(runs.len(), 1, "{list}");
    let run_id = runs[0]["id"].as_str().expect("runId").to_string();
    assert_eq!(runs[0]["status"], json!("running"));

    let (status, cancel) = post(
        &fixture.router,
        &format!("/api/runs/{run_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cancel}");

    let payload = to_bytes(body, usize::MAX).await.expect("stream");
    let text = String::from_utf8_lossy(&payload);
    assert!(text.contains("event: start"), "{text}");
    assert!(text.contains("event: end"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn by_plugin_workflow_lookup_conflicts_and_rejects() {
    let fixture = setup("plugin-workflow");
    const WORKFLOW: &str = "123e4567-e89b-42d3-a456-426614174000";
    const DIGEST_1: &str = "7ff1651d54903b988ae343172b5bce0efdffbaf08dbe072e701f306b1280815b";
    const DIGEST_2: &str = "0ea84c434e4d314df9f1a79732c38048359e8b3f0a117a1fe5a82008f76ab7d1";

    let (status, body) = get(&fixture.router, "/api/runs/by-plugin-workflow/not-a-uuid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "PLUGIN_CONTRACT_REJECTED");
    assert_eq!(
        message_of(&body),
        "pluginWorkflowId must be a canonical UUID or ULID"
    );

    let (status, body) = get(&fixture.router, &format!("/api/runs/by-plugin-workflow/{WORKFLOW}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "NOT_FOUND");
    assert_eq!(message_of(&body), "plugin workflow run not found");

    // The digest is `sha256("od-plugin-logical-request:v1:" + clientRequestId)`.
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({
            "clientRequestId": "plugin-request-1",
            "message": "build the brief",
            "analyticsHints": {
                "entrySurface": "external_mcp",
                "hostProduct": "codex_cli",
                "externalPluginId": "open-design",
                "externalPluginVersion": "1.0.0",
                "distributionMechanism": "local_repo",
                "publisherClass": "open_design_first_party",
                "pluginWorkflowId": WORKFLOW,
                "logicalRequestDigest": DIGEST_1,
                "logicalRequestDigestVersion": 1,
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["runId"].as_str().expect("runId").to_string();

    let (status, found) = get(
        &fixture.router,
        &format!("/api/runs/by-plugin-workflow/{WORKFLOW}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["runId"], json!(run_id));
    assert_eq!(found["pluginWorkflowId"], json!(WORKFLOW));
    assert_eq!(found["logicalRequestDigest"], json!(DIGEST_1));
    assert_eq!(found["logicalRequestDigestVersion"], json!(1));
    assert_eq!(found["projectId"], Value::Null);
    assert_eq!(
        found["externalPluginContext"],
        json!({
            "id": "open-design",
            "version": "1.0.0",
            "distributionMechanism": "local_repo",
            "publisherClass": "open_design_first_party",
        })
    );

    let (status, run) = get(&fixture.router, &format!("/api/runs/{run_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        run["externalPluginAnalytics"]["externalPluginId"],
        json!("open-design")
    );

    // The same workflow id with a different logical request is a conflict.
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({
            "clientRequestId": "plugin-request-2",
            "message": "a different brief",
            "analyticsHints": {
                "entrySurface": "external_mcp",
                "hostProduct": "codex_cli",
                "externalPluginId": "open-design",
                "externalPluginVersion": "1.0.0",
                "distributionMechanism": "local_repo",
                "publisherClass": "open_design_first_party",
                "pluginWorkflowId": WORKFLOW,
                "logicalRequestDigest": DIGEST_2,
                "logicalRequestDigestVersion": 1,
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code_of(&body), "PLUGIN_WORKFLOW_CONFLICT");

    // A digest that does not match the request key is a contract rejection.
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({
            "clientRequestId": "plugin-request-3",
            "message": "build the brief",
            "analyticsHints": {
                "entrySurface": "external_mcp",
                "hostProduct": "codex_cli",
                "externalPluginId": "open-design",
                "externalPluginVersion": "1.0.0",
                "distributionMechanism": "local_repo",
                "publisherClass": "open_design_first_party",
                "pluginWorkflowId": "7b2b3f8c-2f6e-4a53-9b3e-6c9d4a1e5f70",
                "logicalRequestDigest": DIGEST_1,
                "logicalRequestDigestVersion": 1,
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(code_of(&body), "PLUGIN_CONTRACT_REJECTED");
    assert_eq!(
        message_of(&body),
        "logical request digest does not match clientRequestId"
    );

    let (status, list) = get(&fixture.router, "/api/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["runs"].as_array().expect("array").len(), 1, "{list}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assistant_pin_claim_conflict_returns_run_in_progress() {
    let fixture = setup("claim-conflict");
    seed_conversation(&fixture, "c1", "p1", 1);

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({
            "projectId": "p1",
            "conversationId": "c1",
            "assistantMessageId": "am-pin",
            "clientRequestId": "cr-pin",
            "message": "first",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let first = body["runId"].as_str().expect("runId").to_string();
    assert_eq!(body["assistantMessageId"], json!("am-pin"));

    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({
            "projectId": "p1",
            "conversationId": "c1",
            "assistantMessageId": "am-pin",
            "clientRequestId": "cr-pin-2",
            "message": "second",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code_of(&body), "RUN_IN_PROGRESS");
    assert_eq!(
        message_of(&body),
        "assistantMessageId is already bound to an active run"
    );

    let (status, list) = get(&fixture.router, "/api/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["runs"].as_array().expect("array").len(), 1, "{list}");

    post(
        &fixture.router,
        &format!("/api/runs/{first}/cancel"),
        json!({}),
    )
    .await;

    // Once the owning run is terminal the pin may be rebound.
    let (status, body) = post(
        &fixture.router,
        "/api/runs",
        json!({
            "projectId": "p1",
            "conversationId": "c1",
            "assistantMessageId": "am-pin",
            "clientRequestId": "cr-pin-3",
            "message": "third",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let rebound = body["runId"].as_str().expect("runId").to_string();
    assert_ne!(rebound, first);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_creation_is_refused_while_the_daemon_shuts_down() {
    let (router, _store, data_root) = setup_state("shutdown", true);
    let _cleanup = data_root;

    let (status, body) = post(&router, "/api/runs", json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code_of(&body), "UPSTREAM_UNAVAILABLE");
    assert_eq!(message_of(&body), "daemon is shutting down");

    let (status, body) = post(&router, "/api/chat", json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code_of(&body), "UPSTREAM_UNAVAILABLE");

    let (status, list) = get(&router, "/api/runs").await;
    assert_eq!(status, StatusCode::OK, "reads stay open during shutdown");
    assert_eq!(list["runs"], json!([]));
}
