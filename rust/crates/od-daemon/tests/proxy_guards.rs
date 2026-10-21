//! HTTP-level parity for the BYOK proxy guards.
//!
//! Mirrors `apps/daemon/tests/reasoning-egress-policy.test.ts` and the
//! base-URL guard exercised by the connection-test suite: the route must
//! answer with the TS status/code/wording and never reach an upstream fetch.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use od_core::RuntimePaths;
use od_daemon::{AppState, DaemonConfig, Store};
use serde_json::{json, Value};
use tower::ServiceExt;

fn router() -> axum::Router {
    // Each call gets its own data root: tests run in parallel, and sharing one
    // SQLite file races the migration against the reader.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "od-proxy-guards-{}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let paths = RuntimePaths::resolve(dir).expect("resolve data dir");
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

async fn post(uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = router().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn body_with(reasoning: Value) -> Value {
    json!({
        "baseUrl": "https://api.openai.com/v1",
        "apiKey": "sk-secret-openai-key",
        "model": "gpt-test",
        "messages": [{ "role": "user", "content": "hello" }],
        "reasoningExecution": reasoning
    })
}

#[tokio::test]
async fn malformed_reasoning_policy_is_a_400_before_any_upstream_call() {
    for reasoning in [json!({}), json!({ "mode": "off" }), Value::Null, json!([])] {
        let (status, payload) = post("/api/proxy/openai/stream", body_with(reasoning)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            payload["error"]["code"],
            json!("reasoning_execution_invalid_policy")
        );
        assert_eq!(
            payload["error"]["message"],
            json!("reasoningExecution.mode must be one of enabled, disabled, or allowlist.")
        );
        assert_eq!(payload["error"]["data"]["routeKind"], json!("proxy"));
        assert_eq!(payload["error"]["data"]["provider"], json!("openai"));
    }
}

#[tokio::test]
async fn disabled_policy_answers_403_on_every_proxy_provider() {
    let disabled = json!({ "mode": "disabled" });
    let cases = [
        (
            "/api/proxy/openai/stream",
            json!({
                "baseUrl": "https://api.openai.com/v1",
                "apiKey": "sk-openai",
                "model": "gpt-test",
                "reasoningExecution": disabled,
            }),
            "openai",
        ),
        (
            "/api/proxy/anthropic/stream",
            json!({
                "baseUrl": "https://api.anthropic.com",
                "apiKey": "sk-ant",
                "model": "claude-test",
                "reasoningExecution": disabled,
            }),
            "anthropic",
        ),
        (
            "/api/proxy/azure/stream",
            json!({
                "baseUrl": "https://resource.openai.azure.com",
                "apiKey": "azure-key",
                "model": "deployment-one",
                "reasoningExecution": disabled,
            }),
            "azure",
        ),
        (
            "/api/proxy/google/stream",
            json!({
                "apiKey": "google-key",
                "model": "gemini-2.5-pro",
                "reasoningExecution": disabled,
            }),
            "google",
        ),
        (
            "/api/proxy/ollama/stream",
            json!({
                "apiKey": "ollama-key",
                "model": "llama-test",
                "reasoningExecution": disabled,
            }),
            "ollama",
        ),
    ];
    for (uri, body, provider) in cases {
        let (status, payload) = post(uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(
            payload["error"]["code"],
            json!("reasoning_execution_disabled"),
            "{uri}"
        );
        assert_eq!(
            payload["error"]["message"],
            json!("Reasoning provider egress is disabled for this run.")
        );
        assert_eq!(payload["error"]["data"]["routeKind"], json!("proxy"));
        assert_eq!(payload["error"]["data"]["provider"], json!(provider));
        assert_eq!(payload["error"]["data"]["policyMode"], json!("disabled"));
        let serialized = payload.to_string();
        assert!(!serialized.contains("secret"), "{uri}: {serialized}");
        assert!(!serialized.contains("ollama-key"), "{uri}: {serialized}");
    }
}

#[tokio::test]
async fn allowlist_denies_unlisted_base_urls_and_models() {
    let body = json!({
        "baseUrl": "https://api.openai.com/v1",
        "apiKey": "sk-openai",
        "model": "gpt-test",
        "reasoningExecution": {
            "mode": "allowlist",
            "allowedBaseUrls": ["https://api.anthropic.com"],
            "allowedModels": ["gpt-test"]
        }
    });
    let (status, payload) = post("/api/proxy/openai/stream", body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        payload["error"]["code"],
        json!("reasoning_execution_not_allowlisted")
    );
    assert_eq!(
        payload["error"]["message"],
        json!("Reasoning provider egress is not allowlisted for this run.")
    );
    assert_eq!(payload["error"]["data"]["policyMode"], json!("allowlist"));
}

#[tokio::test]
async fn plugin_context_is_refused_with_the_daemon_wording() {
    for key in ["pluginId", "appliedPluginSnapshotId"] {
        let mut body = json!({
            "baseUrl": "https://api.openai.com/v1",
            "apiKey": "sk-openai",
            "model": "gpt-test"
        });
        body[key] = json!("plugin-1");
        let (status, payload) = post("/api/proxy/openai/stream", body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{key}");
        assert_eq!(payload["error"]["code"], json!("PLUGIN_REQUIRES_DAEMON"));
        assert_eq!(
            payload["error"]["message"],
            json!("Plugin runs must go through POST /api/runs so the daemon can resolve and pin the applied plugin snapshot.")
        );
    }
}

#[tokio::test]
async fn internal_and_malformed_base_urls_fail_closed() {
    let internal = json!({
        "baseUrl": "http://10.0.0.5/v1",
        "apiKey": "sk-openai",
        "model": "gpt-test"
    });
    let (status, payload) = post("/api/proxy/openai/stream", internal).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(payload["error"]["code"], json!("FORBIDDEN"));
    assert_eq!(payload["error"]["message"], json!("Internal IPs blocked"));

    let malformed = json!({
        "baseUrl": "ftp://example.com",
        "apiKey": "sk-openai",
        "model": "gpt-test"
    });
    let (status, payload) = post("/api/proxy/openai/stream", malformed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["error"]["code"], json!("BAD_REQUEST"));
    assert_eq!(payload["error"]["message"], json!("Only http/https allowed"));
}

#[tokio::test]
async fn required_fields_still_answer_400_with_the_ts_message() {
    let (status, payload) = post("/api/proxy/openai/stream", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["error"]["code"], json!("BAD_REQUEST"));
    assert_eq!(
        payload["error"]["message"],
        json!("baseUrl, apiKey, and model are required")
    );

    let (status, payload) = post("/api/proxy/google/stream", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(payload["error"]["message"], json!("apiKey and model are required"));
}
