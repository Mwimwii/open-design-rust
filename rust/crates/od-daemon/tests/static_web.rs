//! Integration tests: serving the built web export (`web_dist`) plus the
//! `/artifacts` and `/frames` static trees.
//!
//! Parity reference: `apps/daemon/src/static-spa.ts` and the two
//! `express.static` mounts in `apps/daemon/src/server.ts`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use od_core::RuntimePaths;
use od_daemon::{AppState, DaemonConfig, Store};
use tower::ServiceExt;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("od-daemon-static-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(path, contents).expect("write file");
}

/// A `web_dist` shaped like a Next.js `output: 'export'` build: one shell at
/// the root, per-route directories with their own `index.html`, and
/// `_next/static` assets.
fn web_export(tag: &str) -> PathBuf {
    let dist = temp_dir(tag);
    write_file(&dist.join("index.html"), "<html>shell</html>");
    write_file(&dist.join("projects/index.html"), "<html>projects shell</html>");
    write_file(
        &dist.join("_next/static/chunks/main.js"),
        "console.log('chunk');",
    );
    dist
}

fn config(paths: RuntimePaths, web_dist: Option<PathBuf>) -> DaemonConfig {
    DaemonConfig {
        paths,
        bind_host: "127.0.0.1".to_string(),
        port: 0,
        api_token: None,
        api_auth_disabled: false,
        web_dist,
    }
}

fn router_for(config: DaemonConfig) -> axum::Router {
    let store = Store::open(&config.paths.db_file()).expect("store");
    od_daemon::routes::build_router(AppState::new(config, store))
}

fn request(method: &str, path: &str, accept: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(accept) = accept {
        builder = builder.header("accept", accept);
    }
    let mut request = builder.body(Body::empty()).expect("request");
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo("127.0.0.1:40000".parse::<SocketAddr>().unwrap()));
    request
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deep_links_get_the_shell_and_assets_stay_404() {
    let dist = web_export("shell");
    let router = router_for(config(temp_paths("shell"), Some(dist.clone())));

    // The shell itself, and every unknown non-API path, answers index.html.
    for path in ["/", "/projects/", "/deep/link", "/settings"] {
        let response = router
            .clone()
            .oneshot(request("GET", path, Some("text/html")))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert!(
            body_text(response).await.contains("shell"),
            "{path} should answer the shell"
        );
    }

    // Per-route directories keep their own index.html (trailingSlash export).
    let response = router
        .clone()
        .oneshot(request("GET", "/projects/index.html", Some("text/html")))
        .await
        .expect("response");
    assert!(body_text(response).await.contains("projects shell"));

    // Real assets are served as-is.
    let response = router
        .clone()
        .oneshot(request("GET", "/_next/static/chunks/main.js", None))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response).await.contains("chunk"));

    // A missing asset is a 404, NOT a 200 HTML shell: this is the whole point
    // of the fallback policy.
    let response = router
        .clone()
        .oneshot(request("GET", "/_next/static/chunks/gone.js", Some("*/*")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!body_text(response).await.contains("shell"));

    // An unknown path requested as JSON is a 404 too (no HTML for data calls).
    let response = router
        .clone()
        .oneshot(request("GET", "/aggregate", Some("application/json")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Non-GET never gets the shell.
    let response = router
        .oneshot(request("POST", "/anything", Some("text/html")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&dist);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_shell_directory_is_still_a_404() {
    let dist = temp_dir("no-shell");
    let router = router_for(config(temp_paths("no-shell"), Some(dist.clone())));

    let response = router
        .oneshot(request("GET", "/deep/link", Some("text/html")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&dist);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_web_dist_has_no_shell() {
    let router = router_for(config(temp_paths("no-dist"), None));
    let response = router
        .oneshot(request("GET", "/", Some("text/html")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifacts_tree_is_served_outside_api_auth() {
    let data = temp_paths("artifacts");
    write_file(
        &data.artifacts_dir().join("run-1/chart.txt"),
        "artifact-bytes",
    );
    let dist = web_export("artifacts");
    let router = router_for(config(data.clone(), Some(dist.clone())));

    let response = router
        .clone()
        .oneshot(request("GET", "/artifacts/run-1/chart.txt", None))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "artifact-bytes");

    // Missing artifacts are 404s, not the SPA shell.
    let response = router
        .clone()
        .oneshot(request("GET", "/artifacts/run-9/gone.txt", Some("text/html")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // The API surface is untouched by the static mount.
    let response = router
        .oneshot(request("GET", "/api/health", None))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let _ = std::fs::remove_dir_all(data.data_dir());
    let _ = std::fs::remove_dir_all(&dist);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_tree_comes_from_the_resource_root() {
    let resources = temp_dir("resources");
    write_file(
        &resources.join("frames/iphone-15-pro.html"),
        "<html>frame</html>",
    );
    let dist = web_export("frames");
    let data = temp_paths("frames");

    // `frames_dir()` reads OD_RESOURCE_ROOT at router-build time. Other tests
    // in this binary mount `/frames` from the checkout instead; none of them
    // request it, so the process-wide value is harmless here.
    let prior = std::env::var_os("OD_RESOURCE_ROOT");
    std::env::set_var("OD_RESOURCE_ROOT", &resources);
    let router = router_for(config(data.clone(), Some(dist.clone())));
    match prior {
        Some(value) => std::env::set_var("OD_RESOURCE_ROOT", value),
        None => std::env::remove_var("OD_RESOURCE_ROOT"),
    }

    let response = router
        .clone()
        .oneshot(request("GET", "/frames/iphone-15-pro.html", None))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response).await.contains("frame"));

    let response = router
        .oneshot(request("GET", "/frames/missing.html", Some("text/html")))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&resources);
    let _ = std::fs::remove_dir_all(data.data_dir());
    let _ = std::fs::remove_dir_all(&dist);
}

fn temp_paths(tag: &str) -> RuntimePaths {
    let dir = std::env::temp_dir().join(format!("od-daemon-static-data-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    RuntimePaths::resolve(dir).expect("resolve data dir")
}
