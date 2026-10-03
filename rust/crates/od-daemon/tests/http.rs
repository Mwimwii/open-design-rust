//! Integration tests: real socket binding + auth-layer behavior parity.

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use od_core::RuntimePaths;
use od_daemon::{AppState, DaemonConfig, RunningDaemon, Store};
use tower::ServiceExt;

const TOKEN: &str = "test-token-123";

fn temp_paths(tag: &str) -> RuntimePaths {
    let dir = std::env::temp_dir().join(format!("od-daemon-http-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    RuntimePaths::resolve(dir).expect("resolve data dir")
}

fn config_with(paths: RuntimePaths, token: Option<&str>) -> DaemonConfig {
    DaemonConfig {
        paths,
        bind_host: "127.0.0.1".to_string(),
        port: 0,
        api_token: token.map(str::to_string),
        api_auth_disabled: false,
        web_dist: None,
    }
}

async fn http_get(addr: SocketAddr, path: &str, authorization: Option<&str>) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let auth_header = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth_header}Connection: close\r\n\r\n");
    tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
        .await
        .expect("write");
    let mut response = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
        .await
        .expect("read");
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or(("", ""));
    (head.to_string(), body.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_ready_projects_over_real_socket() {
    let config = config_with(temp_paths("socket"), None);
    let daemon = RunningDaemon::start(config).await.expect("start");
    let addr = daemon.addr();

    let (head, body) = http_get(addr, "/api/health", None).await;
    assert!(head.contains("200 OK"), "health status: {head}");
    assert!(body.contains("\"ok\":true"), "health body: {body}");
    assert!(body.contains("amrTerminalReporter"), "health parity block: {body}");

    let (head, body) = http_get(addr, "/api/ready", None).await;
    assert!(head.contains("200 OK"), "ready status: {head}");
    assert!(body.contains("\"ready\":true"), "ready body: {body}");

    let (head, body) = http_get(addr, "/api/projects", None).await;
    assert!(head.contains("200 OK"), "projects status: {head}");
    assert!(body.contains("\"projects\":[]"), "projects body: {body}");

    let (head, _) = http_get(addr, "/api/nope", None).await;
    assert!(head.contains("404 Not Found"), "unknown api route: {head}");

    // A seeded project shows up in the no-scope catalog.
    daemon
        .state
        .store
        .execute(
            "INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'Demo', 1, 2)",
            (),
        )
        .expect("insert");
    let (_, body) = http_get(addr, "/api/projects", None).await;
    assert!(body.contains("\"p1\"") && body.contains("Demo"), "list body: {body}");

    let (_, body) = http_get(addr, "/api/projects/p1", None).await;
    assert!(body.contains("\"resolvedDir\""), "detail body: {body}");
    assert!(body.contains("\"workspaceId\":null"), "detail binding: {body}");

    let (head, _) = http_get(addr, "/api/projects/missing", None).await;
    assert!(head.contains("404 Not Found"), "missing project: {head}");

    daemon.shutdown().await;
    // After shutdown the port no longer accepts connections.
    let still_up = tokio::net::TcpStream::connect(addr).await.is_ok();
    assert!(!still_up, "listener should be closed after shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_layer_matches_typescript_rules() {
    let config = config_with(temp_paths("auth"), Some(TOKEN));
    let store = Store::open(&config.paths.db_file()).expect("store");
    let state = AppState::new(config, store);
    let router = od_daemon::routes::build_router(state);

    let remote: SocketAddr = "203.0.113.5:40000".parse().unwrap();
    let local: SocketAddr = "127.0.0.1:40000".parse().unwrap();

    // Remote without credentials → 401 + Basic challenge.
    let response = router
        .clone()
        .oneshot(
            api_request("/api/projects", remote, None)
                .await,
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = response
        .headers()
        .get(axum::http::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(challenge.contains("Basic"), "challenge: {challenge}");

    // Remote with exact bearer → 200.
    let response = router
        .clone()
        .oneshot(api_request("/api/projects", remote, Some(&format!("Bearer {TOKEN}"))).await)
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    // Remote with wrong bearer → 401.
    let response = router
        .clone()
        .oneshot(api_request("/api/projects", remote, Some("Bearer nope")).await)
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Loopback peer never carries credentials (desktop UI parity).
    let response = router
        .clone()
        .oneshot(api_request("/api/projects", local, None).await)
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    // Probe paths stay open to remote callers.
    let response = router
        .clone()
        .oneshot(api_request("/api/health", remote, None).await)
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    // Basic auth with the expected username works.
    let basic = format!(
        "Basic {}",
        base64_encode(format!("open-design:{TOKEN}"))
    );
    let response = router
        .clone()
        .oneshot(api_request("/api/projects", remote, Some(&basic)).await)
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
}

async fn api_request(path: &str, peer: SocketAddr, authorization: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request");
    if let Some(value) = authorization {
        request
            .headers_mut()
            .insert(
                axum::http::header::AUTHORIZATION,
                value.parse().expect("auth header"),
            );
    }
    request.extensions_mut().insert(axum::extract::ConnectInfo(peer));
    request
}

fn base64_encode(value: String) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(value)
}

/// Server startup must be quick; guard against accidental hangs in CI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_latency_budget() {
    let started = std::time::Instant::now();
    let config = config_with(temp_paths("latency"), None);
    let daemon = RunningDaemon::start(config).await.expect("start");
    assert!(started.elapsed() < Duration::from_secs(5), "daemon startup too slow");
    daemon.shutdown().await;
}
