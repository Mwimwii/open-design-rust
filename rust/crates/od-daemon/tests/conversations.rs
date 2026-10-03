//! Integration tests for the project conversation routes (parity with
//! `apps/daemon/src/routes/project/conversations.ts`) and the `db.ts` storage
//! helpers they call.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
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

fn setup(tag: &str) -> Fixture {
    let data_root =
        std::env::temp_dir().join(format!("od-daemon-convs-{tag}-{}", std::process::id()));
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
    let router = build_router(AppState::new(config, store.clone()));
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

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let peer = SocketAddr::from(([127, 0, 0, 1], 0));
    let (bytes, headers) = match body {
        Some(body) => (
            serde_json::to_vec(&body).expect("serialize"),
            vec![(header::CONTENT_TYPE, "application/json")],
        ),
        None => (Vec::new(), vec![]),
    };
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .extension(axum::extract::ConnectInfo(peer));
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(Body::from(bytes))
        .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let payload = to_bytes(response.into_body(), usize::MAX).await.expect("body");
    assert_eq!(
        headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "every conversation route answers application/json"
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

async fn patch(router: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    call(router, "PATCH", uri, Some(body)).await
}

async fn put(router: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    call(router, "PUT", uri, Some(body)).await
}

async fn delete(router: &Router, uri: &str) -> (StatusCode, Value) {
    call(router, "DELETE", uri, None).await
}

fn error_of(body: &Value) -> &Value {
    &body["error"]
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

fn message_of(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or("")
}

fn conversation_field<'a>(body: &'a Value, key: &str) -> &'a Value {
    &body["conversation"][key]
}

/// Create a conversation and return its id.
async fn create_conversation(router: &Router, body: Value) -> String {
    let (status, response) = post(router, "/api/projects/p1/conversations", body).await;
    assert_eq!(status, StatusCode::OK, "create failed: {response}");
    conversation_field(&response, "id")
        .as_str()
        .expect("conversation id")
        .to_string()
}

// ---- GET /api/projects/:id/conversations -----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_conversations_missing_project_is_project_not_found() {
    let fixture = setup("list-404");
    let (status, body) = get(&fixture.router, "/api/projects/nope/conversations").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "PROJECT_NOT_FOUND");
    assert_eq!(message_of(&body), "project not found");
    assert_eq!(
        body,
        json!({"error": {"code": "PROJECT_NOT_FOUND", "message": "project not found"}})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_conversations_starts_empty_and_orders_by_updated_at_desc() {
    let fixture = setup("list-shape");
    let (status, body) = get(&fixture.router, "/api/projects/p1/conversations").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"conversations": []}));

    let first = create_conversation(&fixture.router, json!({"title": "First"})).await;
    // Force a distinguishable ordering instead of racing on the clock.
    fixture
        .store
        .execute(
            "UPDATE conversations SET updated_at = 1000 WHERE id = ?1",
            [first.as_str()],
        )
        .expect("age first");
    let second = create_conversation(&fixture.router, json!({"title": "Second"})).await;
    fixture
        .store
        .execute(
            "UPDATE conversations SET updated_at = 2000 WHERE id = ?1",
            [second.as_str()],
        )
        .expect("age second");

    let (status, body) = get(&fixture.router, "/api/projects/p1/conversations").await;
    assert_eq!(status, StatusCode::OK);
    let conversations = body["conversations"].as_array().expect("array");
    assert_eq!(conversations.len(), 2);
    let ids: Vec<&str> = conversations
        .iter()
        .map(|c| c["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec![second.as_str(), first.as_str()]);

    let listed = &conversations[0];
    assert_eq!(listed["projectId"], json!("p1"));
    assert_eq!(listed["title"], json!("Second"));
    assert_eq!(listed["sessionMode"], json!("design"));
    assert_eq!(listed["messageCount"], json!(0));
    assert!(listed["createdAt"].as_i64().is_some());
    assert_eq!(listed["updatedAt"].as_i64(), Some(2000));
    // No run has ever touched the conversation, so both derived keys are absent.
    assert_eq!(listed.get("latestRun"), None);
    assert_eq!(listed.get("totalDurationMs"), None);
}

// ---- POST /api/projects/:id/conversations ----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_conversation_defaults_and_validation() {
    let fixture = setup("create-defaults");

    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({"title": "   padded   "}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(conversation_field(&body, "title"), &json!("padded"));
    assert_eq!(conversation_field(&body, "sessionMode"), &json!("design"));
    assert_eq!(conversation_field(&body, "projectId"), &json!("p1"));
    assert_eq!(conversation_field(&body, "messageCount"), &json!(0));

    for mode in ["design", "chat", "plan"] {
        let (status, _) = post(
            &fixture.router,
            "/api/projects/p1/conversations",
            json!({"sessionMode": mode}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "accepts {mode}");
    }

    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({"sessionMode": "banana"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "BAD_REQUEST");
    assert_eq!(
        message_of(&body),
        "sessionMode must be one of design, chat, or plan"
    );

    let (status, body) = post(
        &fixture.router,
        "/api/projects/nope/conversations",
        json!({"title": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "PROJECT_NOT_FOUND");
    assert_eq!(message_of(&body), "project not found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_conversation_without_json_content_type_reads_as_empty_body() {
    let fixture = setup("create-nojson");
    let peer = SocketAddr::from(([127, 0, 0, 1], 0));
    let request = Request::builder()
        .method("POST")
        .uri("/api/projects/p1/conversations")
        .header(header::CONTENT_TYPE, "text/plain")
        .extension(axum::extract::ConnectInfo(peer))
        .body(Body::from("not json"))
        .expect("request");
    let response = fixture.router.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let payload = to_bytes(response.into_body(), usize::MAX).await.expect("body");
    let body: Value = serde_json::from_slice(&payload).expect("json");
    assert_eq!(conversation_field(&body, "sessionMode"), &json!("design"));
    assert_eq!(conversation_field(&body, "title"), &Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_rejects_non_object_json_body() {
    let fixture = setup("create-scalar");
    let peer = SocketAddr::from(([127, 0, 0, 1], 0));
    let request = Request::builder()
        .method("POST")
        .uri("/api/projects/p1/conversations")
        .header(header::CONTENT_TYPE, "application/json")
        .extension(axum::extract::ConnectInfo(peer))
        .body(Body::from("\"scalar\""))
        .expect("request");
    let response = fixture.router.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload = to_bytes(response.into_body(), usize::MAX).await.expect("body");
    let body: Value = serde_json::from_slice(&payload).expect("json");
    assert_eq!(code_of(&body), "BAD_REQUEST");
    assert_eq!(message_of(&body), "invalid json body");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_copies_messages_and_marks_the_boundary() {
    let fixture = setup("fork");
    let source = create_conversation(
        &fixture.router,
        json!({"title": "Design review", "sessionMode": "chat"}),
    )
    .await;

    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{source}/messages/m1"),
        json!({"role": "user", "content": "first", "runId": "run-1",
               "runStatus": "succeeded", "lastRunEventId": "7"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{source}/messages/m2"),
        json!({"role": "assistant", "content": "second", "runId": "run-2",
               "runStatus": "running"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, response) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({"seedFromConversationId": source}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    // Derived title: source base + " (1)".
    assert_eq!(
        conversation_field(&response, "title"),
        &json!("Design review (1)")
    );
    // The fork inherits the source conversation's session mode.
    assert_eq!(conversation_field(&response, "sessionMode"), &json!("chat"));
    // `insertConversation` re-reads the row BEFORE the seed loop runs, so the
    // create response still reports the pre-seed count (TS parity).
    assert_eq!(conversation_field(&response, "messageCount"), &json!(0));
    let fork = conversation_field(&response, "id")
        .as_str()
        .expect("fork id")
        .to_string();

    let (status, messages) = get(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{fork}/messages"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let messages = messages["messages"].as_array().expect("array").clone();
    assert_eq!(messages.len(), 2);

    // Fresh ids, drop run pointers, keep the settled verdict.
    assert_ne!(messages[0]["id"], json!("m1"));
    assert_ne!(messages[1]["id"], json!("m2"));
    assert_eq!(messages[0].get("runId"), None);
    assert_eq!(messages[0].get("lastRunEventId"), None);
    assert_eq!(messages[0]["runStatus"], json!("succeeded"));
    // A non-terminal run status is dropped by `settledForkVerdict`.
    assert_eq!(messages[1].get("runStatus"), None);
    assert_eq!(messages[0]["content"], json!("first"));
    assert_eq!(messages[1]["content"], json!("second"));

    // The divider lands only on the last copied message.
    assert_eq!(messages[0].get("forkedInto"), None);
    assert_eq!(
        messages[1]["forkedInto"],
        json!({"title": "Design review", "conversationId": source})
    );

    // The source conversation keeps its own rows untouched.
    let (status, original) = get(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{source}/messages"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let original = original["messages"].as_array().expect("array");
    assert_eq!(original.len(), 2);
    assert_eq!(original[0]["id"], json!("m1"));
    assert_eq!(original[1]["runStatus"], json!("running"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_requires_a_routable_source_conversation() {
    let fixture = setup("fork-source");

    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({"forkAfterMessageId": "m1"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "FORK_SOURCE_NOT_FOUND");
    assert_eq!(message_of(&body), "fork source conversation not found");

    let (status, body) = post(
        &fixture.router,
        "/api/projects/nope/conversations",
        json!({"forkAfterMessageId": "m1"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "PROJECT_NOT_FOUND");

    let other = create_conversation(&fixture.router, json!({"title": "Other"})).await;
    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({"seedFromConversationId": other}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A conversation owned by ANOTHER project is not routable: the fork
    // silently seeds nothing rather than leaking a foreign transcript.
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
            "INSERT INTO conversations (id, project_id, title, session_mode, created_at, updated_at)
             VALUES ('foreign', 'p2', 'Foreign', 'design', 1, 1)",
            (),
        )
        .expect("insert foreign conversation");
    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({"seedFromConversationId": "foreign"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // No routable source ⇒ no derived fork title and no copied rows.
    assert_eq!(conversation_field(&body, "title"), &Value::Null);
    assert_eq!(conversation_field(&body, "messageCount"), &json!(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_truncates_at_the_requested_message() {
    let fixture = setup("fork-truncate");
    let source = create_conversation(&fixture.router, json!({"title": "Trunc"})).await;
    for (id, text) in [("m1", "one"), ("m2", "two"), ("m3", "three")] {
        let (status, body) = put(
            &fixture.router,
            &format!("/api/projects/p1/conversations/{source}/messages/{id}"),
            json!({"role": "user", "content": text}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let (status, response) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({"seedFromConversationId": source, "forkAfterMessageId": "m2"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let fork = conversation_field(&response, "id")
        .as_str()
        .expect("fork id")
        .to_string();
    let (status, messages) = get(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{fork}/messages"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let messages = messages["messages"].as_array().expect("array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"], json!("one"));
    assert_eq!(messages[1]["content"], json!("two"));
    // The divider is the lower bound of the copied context, so it sits on the
    // LAST copied row — the fork point itself.
    assert_eq!(messages[0].get("forkedInto"), None);
    assert_eq!(
        messages[1]["forkedInto"],
        json!({"title": "Trunc", "conversationId": source})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_of_an_unknown_message_is_rejected() {
    let fixture = setup("fork-unknown");
    let source = create_conversation(&fixture.router, json!({"title": "Src"})).await;
    put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{source}/messages/m1"),
        json!({"role": "user", "content": "one"}),
    )
    .await;

    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({"seedFromConversationId": source, "forkAfterMessageId": "missing"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "FORK_MESSAGE_NOT_FOUND");
    assert_eq!(message_of(&body), "fork message not found");

    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({
            "seedFromConversationId": source,
            "forkAfterMessageId": "missing",
            "forkFallbackMessage": {"id": "missing", "role": "user", "content": "fallback"},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "BAD_REQUEST");
    assert_eq!(message_of(&body), "fork fallback predecessor is required");

    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({
            "seedFromConversationId": source,
            "forkAfterMessageId": "missing",
            "forkFallbackMessage": {"id": "missing", "role": "user", "content": "fallback"},
            "forkFallbackPredecessorMessageId": "also-missing",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "FORK_FALLBACK_PREDECESSOR_NOT_FOUND");
    assert_eq!(message_of(&body), "fork fallback predecessor not found");

    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({
            "seedFromConversationId": source,
            "forkAfterMessageId": "missing",
            "forkFallbackMessage": {"id": "missing", "role": "user", "content": "fallback"},
            "forkFallbackPredecessorMessageId": null,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let fork = conversation_field(&body, "id").as_str().expect("id").to_string();
    let (_, messages) = get(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{fork}/messages"),
    )
    .await;
    let messages = messages["messages"].as_array().expect("array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["content"], json!("fallback"));
    // `upsertMessage` assigns a fresh id to every copied row.
    assert_ne!(messages[0]["id"], json!("missing"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_seed_messages_override_the_source_transcript() {
    let fixture = setup("fork-client-seed");
    let source = create_conversation(&fixture.router, json!({"title": "Src"})).await;
    put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{source}/messages/m1"),
        json!({"role": "user", "content": "ignored"}),
    )
    .await;

    let (status, body) = post(
        &fixture.router,
        "/api/projects/p1/conversations",
        json!({
            "seedFromConversationId": source,
            "seedMessages": [
                {"id": "a", "role": "user", "content": "A"},
                {"id": "b", "role": "assistant", "content": "B"},
            ],
            "forkAfterMessageId": "b",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let fork = conversation_field(&body, "id").as_str().expect("id").to_string();
    let (_, messages) = get(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{fork}/messages"),
    )
    .await;
    let messages = messages["messages"].as_array().expect("array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"], json!("A"));
    assert_eq!(messages[1]["content"], json!("B"));
    // Only the last copied message carries the divider, and it needs a source
    // title (present here).
    assert_eq!(
        messages[1]["forkedInto"],
        json!({"title": "Src", "conversationId": source})
    );
    assert_eq!(messages[0].get("forkedInto"), None);
}

// ---- PATCH /api/projects/:id/conversations/:cid ----------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_updates_title_and_session_mode() {
    let fixture = setup("patch");
    let id = create_conversation(&fixture.router, json!({"title": "Before"})).await;

    let (status, body) = patch(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}"),
        json!({"title": "After", "sessionMode": "plan", "updatedAt": 5000}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(conversation_field(&body, "title"), &json!("After"));
    assert_eq!(conversation_field(&body, "sessionMode"), &json!("plan"));
    assert_eq!(conversation_field(&body, "updatedAt"), &json!(5000));

    // An invalid mode is rejected before anything is written.
    let (status, body) = patch(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}"),
        json!({"sessionMode": "nope"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "BAD_REQUEST");
    assert_eq!(
        message_of(&body),
        "sessionMode must be one of design, chat, or plan"
    );

    // `sessionMode: null` is present-but-invalid too (`hasOwnProperty`).
    let (status, body) = patch(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}"),
        json!({"sessionMode": null}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "BAD_REQUEST");

    // Omitting the field keeps the stored mode and refreshes updatedAt.
    let (status, body) = patch(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}"),
        json!({"title": "Renamed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(conversation_field(&body, "title"), &json!("Renamed"));
    assert_eq!(conversation_field(&body, "sessionMode"), &json!("plan"));
    assert!(conversation_field(&body, "updatedAt").as_i64().unwrap() > 5000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_missing_conversation_is_404_not_found() {
    let fixture = setup("patch-404");
    let (status, body) = patch(
        &fixture.router,
        "/api/projects/p1/conversations/absent",
        json!({"title": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "CONVERSATION_NOT_FOUND");
    assert_eq!(message_of(&body), "not found");

    // A conversation belonging to another project is not routable here.
    let other = create_conversation(&fixture.router, json!({"title": "Other"})).await;
    let (status, body) = patch(
        &fixture.router,
        &format!("/api/projects/other-project/conversations/{other}"),
        json!({"title": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "CONVERSATION_NOT_FOUND");

    // Comment-anchor conversations are never routable.
    let (status, body) = patch(
        &fixture.router,
        "/api/projects/p1/conversations/comment-anchor-abc",
        json!({"title": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "CONVERSATION_NOT_FOUND");
}

// ---- DELETE /api/projects/:id/conversations/:cid ---------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_removes_the_conversation() {
    let fixture = setup("delete");
    let id = create_conversation(&fixture.router, json!({"title": "Doomed"})).await;

    let (status, body) = delete(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ok": true}));

    let (status, list) = get(&fixture.router, "/api/projects/p1/conversations").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list, json!({"conversations": []}));

    let (status, body) = delete(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "CONVERSATION_NOT_FOUND");
    assert_eq!(message_of(&body), "not found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_repoints_team_comment_anchors_before_removing_the_row() {
    let fixture = setup("delete-team");
    fixture
        .store
        .execute(
            "INSERT INTO workspace_projects
               (project_id, workspace_id, visibility, resource_state, created_at, updated_at)
             VALUES ('p1', 'ws1', 'team', 'active', 1, 1)",
            (),
        )
        .expect("workspace binding");

    let anchor = create_conversation(&fixture.router, json!({"title": "Anchor"})).await;
    fixture
        .store
        .execute(
            "UPDATE conversations SET id = ?1 WHERE id = ?2",
            rusqlite::params![format!("comment-anchor-{anchor}"), anchor],
        )
        .expect("rename to anchor");
    let anchor = format!("comment-anchor-{anchor}");
    let doomed = create_conversation(&fixture.router, json!({"title": "Doomed"})).await;
    fixture
        .store
        .execute(
            "INSERT INTO preview_comments
               (id, project_id, conversation_id, file_path, element_id, selector, label,
                text, position_json, html_hint, slide_key, note, status,
                created_at, updated_at)
             VALUES ('c1', 'p1', ?1, 'index.html', 'e1', '#e1', 'L', 'T', '{}', '', -1,
                     '', 'open', 1, 1)",
            [doomed.as_str()],
        )
        .expect("comment");

    let (status, body) = delete(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{doomed}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"ok": true}));

    let deleted = fixture
        .store
        .query_one(
            "SELECT id FROM conversations WHERE id = ?1",
            [doomed.as_str()],
            |row| row.get::<_, String>(0),
        )
        .expect("query");
    assert_eq!(deleted, None);
    let anchor_row = fixture
        .store
        .query_one(
            "SELECT id FROM conversations WHERE id = ?1",
            [anchor.as_str()],
            |row| row.get::<_, String>(0),
        )
        .expect("query");
    assert_eq!(anchor_row.as_deref(), Some(anchor.as_str()));
    let comment_anchor = fixture
        .store
        .query_one(
            "SELECT conversation_id FROM preview_comments WHERE id = 'c1'",
            (),
            |row| row.get::<_, String>(0),
        )
        .expect("query");
    assert_eq!(comment_anchor.as_deref(), Some(anchor.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_leaves_personal_project_comments_alone() {
    let fixture = setup("delete-personal");
    let doomed = create_conversation(&fixture.router, json!({"title": "Doomed"})).await;
    let kept = create_conversation(&fixture.router, json!({"title": "Kept"})).await;
    fixture
        .store
        .execute(
            "INSERT INTO preview_comments
               (id, project_id, conversation_id, file_path, element_id, selector, label,
                text, position_json, html_hint, slide_key, note, status,
                created_at, updated_at)
             VALUES ('c1', 'p1', ?1, 'index.html', 'e1', '#e1', 'L', 'T', '{}', '', -1,
                     '', 'open', 1, 1)",
            [doomed.as_str()],
        )
        .expect("comment");

    let (status, body) = delete(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{doomed}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // No workspace binding → no repair pass; the ON DELETE CASCADE removes the
    // comment with its conversation.
    let remaining: Vec<String> = fixture
        .store
        .query("SELECT id FROM conversations", (), |row| row.get::<_, String>(0))
        .expect("query");
    assert_eq!(remaining, vec![kept.clone()]);
    let comments: Vec<String> = fixture
        .store
        .query("SELECT id FROM preview_comments", (), |row| row.get::<_, String>(0))
        .expect("query");
    assert!(comments.is_empty());
}

// ---- GET /api/projects/:id/conversations/:cid/messages ---------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_messages_requires_a_routable_conversation() {
    let fixture = setup("messages-404");
    let (status, body) = get(
        &fixture.router,
        "/api/projects/p1/conversations/absent/messages",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "CONVERSATION_NOT_FOUND");
    assert_eq!(message_of(&body), "conversation not found");

    let (status, body) = get(&fixture.router, "/api/projects/nope/conversations/x/messages").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "CONVERSATION_NOT_FOUND");

    let id = create_conversation(&fixture.router, json!({})).await;
    let (status, body) = get(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"messages": []}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_messages_returns_stored_message_shape() {
    let fixture = setup("messages-shape");
    let id = create_conversation(&fixture.router, json!({})).await;

    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages/m1"),
        json!({
            "role": "assistant",
            "content": "hello",
            "agentId": "gpt",
            "agentName": "GPT",
            "events": [{"kind": "text", "text": "hello"}],
            "startedAt": 10,
            "endedAt": 20,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let saved = &body["message"];
    assert_eq!(saved["id"], json!("m1"));
    assert_eq!(saved["role"], json!("assistant"));
    assert_eq!(saved["content"], json!("hello"));
    assert_eq!(saved["agentId"], json!("gpt"));
    assert_eq!(saved["events"], json!([{"kind": "text", "text": "hello"}]));
    assert_eq!(saved["startedAt"], json!(10));
    assert_eq!(saved["endedAt"], json!(20));
    assert_eq!(saved.get("runId"), None);
    assert_eq!(saved.get("runStatus"), None);
    assert_eq!(saved.get("events"), Some(&json!([{"kind": "text", "text": "hello"}])));

    let (status, body) = get(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["messages"], json!([saved]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_messages_decorates_strategy_task_turns() {
    let fixture = setup("messages-strategy");
    let id = create_conversation(&fixture.router, json!({})).await;
    fixture
        .store
        .execute(
            "INSERT INTO applied_plugin_snapshots
               (id, project_id, plugin_id, plugin_version, manifest_source_digest,
                task_kind, inputs_json, resolved_context_json, capabilities_granted,
                assets_staged_json, applied_at)
             VALUES ('snap1', 'p1', 'plg', '1.0.0', 'digest', 'task', '{}', '{}', '[]',
                     '{}', 1)",
            (),
        )
        .expect("snapshot");
    fixture
        .store
        .execute(
            "INSERT INTO strategy_task_executions
               (task_execution_id, project_id, conversation_id, snapshot_id, strategy_id,
                strategy_version, strategy_package_hash, selected_agent_id, input_stage,
                outcome, initial_run_id, latest_run_id, blocked_visible_text,
                created_at, updated_at)
             VALUES ('exec1', 'p1', ?1, 'snap1', 'st', '1', 'hash', 'agent', 'production',
                     'blocked', 'run-b', 'run-b', 'blocked text', 1, 1)",
            [id.as_str()],
        )
        .expect("execution");
    fixture
        .store
        .execute(
            "INSERT INTO strategy_task_runs
               (task_execution_id, run_id, input_stage, task_run_index, created_at)
             VALUES ('exec1', 'run-b', 'production', 3, 1)",
            (),
        )
        .expect("run");

    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages/m1"),
        json!({"role": "assistant", "content": "text", "runId": "run-b"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = get(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let message = &body["messages"][0];
    assert_eq!(message["strategyTaskExecutionId"], json!("exec1"));
    assert_eq!(message["strategyTaskRunIndex"], json!(3));
    assert_eq!(message["strategyTaskBlocked"], json!(true));
    assert_eq!(message["strategyTaskBlockedText"], json!("blocked text"));
    assert_eq!(message.get("strategyTaskDelivered"), None);
}

// ---- PUT /api/projects/:id/conversations/:cid/messages/:mid ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_message_rejects_a_mismatched_id() {
    let fixture = setup("put-mismatch");
    let id = create_conversation(&fixture.router, json!({})).await;
    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages/m1"),
        json!({"id": "other", "role": "user", "content": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&body), "BAD_REQUEST");
    assert_eq!(message_of(&body), "id mismatch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_message_rejects_a_row_owned_by_another_conversation() {
    let fixture = setup("put-cross");
    let first = create_conversation(&fixture.router, json!({})).await;
    let second = create_conversation(&fixture.router, json!({})).await;
    put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{first}/messages/m1"),
        json!({"role": "user", "content": "x"}),
    )
    .await;

    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{second}/messages/m1"),
        json!({"role": "user", "content": "hijack"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "MESSAGE_NOT_FOUND");
    assert_eq!(message_of(&body), "message not found");

    // The original row is untouched.
    let (_, list) = get(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{first}/messages"),
    )
    .await;
    assert_eq!(list["messages"][0]["content"], json!("x"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_message_create_only_returns_the_stored_row() {
    let fixture = setup("put-create-only");
    let id = create_conversation(&fixture.router, json!({})).await;
    let uri = format!("/api/projects/p1/conversations/{id}/messages/m1");
    put(&fixture.router, &uri, json!({"role": "user", "content": "first"})).await;

    let (status, body) = put(
        &fixture.router,
        &uri,
        json!({"role": "user", "content": "second", "createOnly": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["message"]["content"], json!("first"));

    let (_, list) = get(&fixture.router, &uri.replace("/messages/m1", "/messages")).await;
    assert_eq!(list["messages"].as_array().expect("array").len(), 1);

    // Without the flag the row is overwritten.
    let (status, body) = put(
        &fixture.router,
        &uri,
        json!({"role": "user", "content": "second"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["message"]["content"], json!("second"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_message_never_regresses_a_daemon_backed_row() {
    let fixture = setup("put-regress");
    let id = create_conversation(&fixture.router, json!({})).await;
    let uri = format!("/api/projects/p1/conversations/{id}/messages/m1");
    let (status, body) = put(
        &fixture.router,
        &uri,
        json!({
            "role": "assistant",
            "content": "stored text",
            "runId": "run-1",
            "runStatus": "succeeded",
            "lastRunEventId": "12",
            "startedAt": 100,
            "endedAt": 200,
            "events": [{"kind": "text", "text": "a"}, {"kind": "done_key", "key": "k1"}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A stale snapshot: fewer events, an older status, a shorter body.
    let (status, body) = put(
        &fixture.router,
        &uri,
        json!({
            "role": "assistant",
            "content": "short",
            "runStatus": "running",
            "events": [{"kind": "text", "text": "a"}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let message = &body["message"];
    assert_eq!(message["content"], json!("stored text"));
    assert_eq!(message["runStatus"], json!("succeeded"));
    assert_eq!(message["runId"], json!("run-1"));
    assert_eq!(
        message["events"],
        json!([{"kind": "text", "text": "a"}, {"kind": "done_key", "key": "k1"}])
    );
    assert_eq!(message["startedAt"], json!(100));
    assert_eq!(message["endedAt"], json!(200));
    assert_eq!(message["lastRunEventId"], json!("12"));

    // A differing runId never repopulates the row's run-owned fields.
    let (status, body) = put(
        &fixture.router,
        &uri,
        json!({
            "role": "assistant",
            "content": "from another run",
            "runId": "run-2",
            "runStatus": "failed",
            "events": [],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let message = &body["message"];
    assert_eq!(message["runId"], json!("run-1"));
    assert_eq!(message["runStatus"], json!("succeeded"));
    assert_eq!(message["content"], json!("stored text"));
    assert_eq!(
        message["events"],
        json!([{"kind": "text", "text": "a"}, {"kind": "done_key", "key": "k1"}])
    );
    // UI metadata still lands on the protected path.
    assert_eq!(message.get("feedback"), None);
    let (status, body) = put(
        &fixture.router,
        &uri,
        json!({"role": "assistant", "feedback": {"score": 1}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message"]["feedback"], json!({"score": 1}));
    assert_eq!(body["message"]["runId"], json!("run-1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_message_keeps_run_owned_fields_when_the_payload_is_a_sibling_fold() {
    let fixture = setup("put-sibling-fold");
    let id = create_conversation(&fixture.router, json!({})).await;
    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages/m1"),
        json!({
            "role": "assistant",
            "content": "first answer",
            "runId": "run-1",
            "runStatus": "succeeded",
            "events": [{"kind": "text", "text": "first answer"}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages/m2"),
        json!({
            "role": "assistant",
            "content": "second answer",
            "runId": "run-2",
            "runStatus": "succeeded",
            "events": [{"kind": "done_key", "key": "shared-key"}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A payload that repeats a SIBLING row's done_key is a client-side fold of
    // one logical turn; the stored run ownership wins.
    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages/m1"),
        json!({
            "role": "assistant",
            "content": "folded",
            "runId": "run-1",
            "runStatus": "failed",
            "events": [{"kind": "done_key", "key": "shared-key"}],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let message = &body["message"];
    assert_eq!(message["content"], json!("first answer"));
    assert_eq!(message["runStatus"], json!("succeeded"));
    assert_eq!(message["runId"], json!("run-1"));
    assert_eq!(
        message["events"],
        json!([{"kind": "text", "text": "first answer"}])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_message_compacts_adjacent_agent_events_and_bumps_the_project() {
    let fixture = setup("put-compact");
    let id = create_conversation(&fixture.router, json!({})).await;
    fixture
        .store
        .execute("UPDATE projects SET updated_at = 7 WHERE id = 'p1'", ())
        .expect("age project");

    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages/m1"),
        json!({
            "role": "assistant",
            "content": "x",
            "events": [
                {"kind": "text", "text": "a"},
                {"kind": "text", "text": "b"},
                {"kind": "text", "text": "c"},
                {"kind": "tool_use", "id": "t1", "name": "Read"},
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Adjacent text deltas merge; a non-delta event is never folded in
    // (parity: `compactAdjacentMessageAgentEvents`).
    assert_eq!(
        body["message"]["events"],
        json!([
            {"kind": "text", "text": "abc"},
            {"kind": "tool_use", "id": "t1", "name": "Read"},
        ])
    );

    let project_updated_at: Option<i64> = fixture
        .store
        .query_one(
            "SELECT updated_at FROM projects WHERE id = 'p1'",
            (),
            |row| row.get(0),
        )
        .expect("query");
    assert!(
        project_updated_at.unwrap_or(0) > 7,
        "PUT bumps the parent project's updatedAt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_message_without_a_role_is_a_storage_error() {
    let fixture = setup("put-no-role");
    let id = create_conversation(&fixture.router, json!({})).await;
    let (status, body) = put(
        &fixture.router,
        &format!("/api/projects/p1/conversations/{id}/messages/m1"),
        json!({"content": "no role"}),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(code_of(&body), "INTERNAL_ERROR");
    assert_eq!(message_of(&body), "storage error");
    assert_eq!(error_of(&body).as_object().expect("object").len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_message_treats_a_non_json_body_as_empty() {
    let fixture = setup("put-falsy");
    let id = create_conversation(&fixture.router, json!({})).await;
    let uri = format!("/api/projects/p1/conversations/{id}/messages/m1");

    // `const m = req.body || {}` — an unparsed body reaches the handler as
    // `{}`, which carries no role and so fails like TS's better-sqlite3
    // binding of `undefined`.
    let peer = SocketAddr::from(([127, 0, 0, 1], 0));
    let request = Request::builder()
        .method("PUT")
        .uri(&uri)
        .header(header::CONTENT_TYPE, "text/plain")
        .extension(axum::extract::ConnectInfo(peer))
        .body(Body::from("nope"))
        .expect("request");
    let response = fixture.router.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let payload = to_bytes(response.into_body(), usize::MAX).await.expect("body");
    let body: Value = serde_json::from_slice(&payload).expect("json");
    assert_eq!(code_of(&body), "INTERNAL_ERROR");

    // Express's `express.json({ strict: true })` rejects a non-object/array
    // top level, so a JSON `null` body is a parse failure, not `{}`.
    let (status, body) = put(&fixture.router, &uri, Value::Null).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(code_of(&body), "BAD_REQUEST");
    assert_eq!(message_of(&body), "invalid json body");
}
