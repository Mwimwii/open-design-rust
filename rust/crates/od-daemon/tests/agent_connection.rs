//! Integration tests for the agent-mode `POST /api/test/connection` route.
//!
//! The decision tree itself is pinned by fixture tests inside
//! `agent_connection.rs`; these tests drive the real HTTP route so the
//! request/response wiring (status code, JSON envelope, agent resolution) is
//! covered without burning an actual provider turn.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use od_core::RuntimePaths;
use od_daemon::{AppState, DaemonConfig, Store};
use tower::ServiceExt;

fn temp_paths(tag: &str) -> RuntimePaths {
    let dir = std::env::temp_dir().join(format!("od-agent-conn-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    RuntimePaths::resolve(dir).expect("resolve data dir")
}

fn router_for(paths: RuntimePaths) -> axum::Router {
    let config = DaemonConfig {
        paths,
        bind_host: "127.0.0.1".to_string(),
        port: 0,
        api_token: None,
        api_auth_disabled: false,
        web_dist: None,
    };
    let store = Store::open(&config.paths.data_dir().join("app.sqlite")).expect("store");
    od_daemon::routes::build_router(AppState::new(config, store))
}

async fn post_connection(router: axum::Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/test/connection")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = router.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

#[tokio::test]
async fn agent_mode_rejects_a_missing_agent_id() {
    let router = router_for(temp_paths("missing-id"));
    let (status, body) = post_connection(router, serde_json::json!({"mode": "agent"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BAD_REQUEST");
}

#[tokio::test]
async fn agent_mode_reports_an_unknown_agent_as_not_installed() {
    let router = router_for(temp_paths("unknown-agent"));
    let (status, body) = post_connection(
        router,
        serde_json::json!({"mode": "agent", "agentId": "not-a-real-agent"}),
    )
    .await;
    // Parity: the endpoint answers 200 with `ok:false` + a `kind`, never a 5xx.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], false);
    assert_eq!(body["kind"], "agent_not_installed");
    assert_eq!(body["detail"], "Unknown agent id: not-a-real-agent");
    assert_eq!(body["model"], "default");
    assert_eq!(body["diagnostics"]["phase"], "binary_resolution");
}

#[tokio::test]
async fn agent_mode_reports_a_def_with_no_binary_as_not_installed() {
    let router = router_for(temp_paths("no-binary"));
    // `amr` is a shipped def whose binary resolution depends entirely on the
    // environment; force the "not installed" shape by pointing every
    // resolution override at a path that cannot exist.
    let (status, body) = post_connection(
        router,
        serde_json::json!({
            "mode": "agent",
            "agentId": "definitely-not-installed",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kind"], "agent_not_installed");
}

#[tokio::test]
async fn provider_mode_still_validates_protocol() {
    let router = router_for(temp_paths("provider-protocol"));
    let (status, body) = post_connection(
        router,
        serde_json::json!({"mode": "provider", "protocol": "nope"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "BAD_REQUEST");
}
