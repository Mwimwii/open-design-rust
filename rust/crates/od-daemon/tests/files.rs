//! Integration tests for the project file routes: inventory, raw reads, and
//! text previews — including the path-confinement guarantees that keep a
//! `path` parameter inside the project directory.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use od_core::RuntimePaths;
use od_daemon::routes::build_router;
use od_daemon::{AppState, DaemonConfig, Store};
use serde_json::Value;
use tower::ServiceExt;

const OLD_MTIME_MS: u64 = 1_700_000_100_000;
const NEW_MTIME_MS: u64 = 1_700_000_200_000;

struct Fixture {
    router: Router,
    store: Store,
    projects_root: PathBuf,
    data_root: PathBuf,
}

fn setup(tag: &str) -> Fixture {
    let data_root =
        std::env::temp_dir().join(format!("od-daemon-files-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_root);
    let paths = RuntimePaths::resolve(&data_root).expect("data dir");
    let projects_root = paths.projects_dir();
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
        projects_root,
        data_root,
    }
}

fn seed_managed(root: &Path) {
    let project = root.join("p1");
    std::fs::create_dir_all(project.join("sub")).unwrap();
    std::fs::create_dir_all(project.join("node_modules")).unwrap();
    let mut html = String::from("const sab = new SharedArrayBuffer(8);\n");
    html.push_str(&"<p>x</p>\n".repeat(500));
    std::fs::write(project.join("index.html"), &html).unwrap();
    std::fs::write(project.join("sub/note.md"), "# note").unwrap();
    std::fs::write(project.join(".env"), "SECRET=1").unwrap();
    std::fs::write(project.join("node_modules/skip.js"), "x").unwrap();
    std::fs::write(project.join("index.html.artifact.json"), "{}").unwrap();
    set_mtime(&project.join("sub/note.md"), OLD_MTIME_MS);
    set_mtime(&project.join("index.html"), NEW_MTIME_MS);
}

fn set_mtime(path: &Path, millis: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis))
        .unwrap();
}

async fn get(router: &Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let peer = SocketAddr::from(([127, 0, 0, 1], 0));
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .extension(axum::extract::ConnectInfo(peer))
        .body(Body::empty())
        .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, headers, body.to_vec())
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|err| panic!("json body {err}: {body:?}"))
}

fn names(listing: &Value) -> Vec<String> {
    listing["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|file| file["name"].as_str().expect("name").to_string())
        .collect()
}

fn no_store(headers: &HeaderMap) -> bool {
    headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()) == Some("no-store")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_read_and_preview_happy_path() {
    let fixture = setup("happy");
    seed_managed(&fixture.projects_root);

    let (status, headers, body) = get(&fixture.router, "/api/projects/p1/files").await;
    assert_eq!(status, StatusCode::OK);
    assert!(no_store(&headers), "listing must not be cacheable");
    let listing = json(&body);
    assert_eq!(names(&listing), vec!["index.html", "sub/note.md"]);
    let first = &listing["files"][0];
    assert_eq!(first["type"], "file");
    assert_eq!(first["path"], "index.html");
    assert_eq!(first["kind"], "html");
    assert_eq!(first["mime"], "text/html; charset=utf-8");
    assert!(first["size"].as_u64().unwrap() > 1000);
    let local_path = first["localPath"].as_str().expect("localPath");
    assert!(
        PathBuf::from(local_path).is_absolute(),
        "localPath must be absolute: {local_path}"
    );
    assert!(local_path.ends_with("index.html"));
    assert!(first["artifactManifest"].is_object());
    assert_eq!(first["artifactKind"], "html");

    let (status, headers, body) = get(&fixture.router, "/api/projects/p1/files/index.html").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    assert!(body.starts_with(b"const sab = new SharedArrayBuffer"));

    // Percent-encoded separators resolve the same way Express does.
    let (status, _, body) = get(&fixture.router, "/api/projects/p1/files/sub%2Fnote.md").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_slice(), b"# note");

    let (status, headers, body) =
        get(&fixture.router, "/api/projects/p1/text-preview/index.html?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(no_store(&headers), "preview must not be cacheable");
    let preview = json(&body);
    // `limit=1` floors to 1, then clamps up to the 1 KiB floor.
    assert_eq!(preview["limit"], 1024);
    assert_eq!(preview["truncated"], true);
    assert_eq!(preview["text"].as_str().unwrap().len(), 1024);
    assert_eq!(preview["mime"], "text/html; charset=utf-8");
    assert_eq!(preview["kind"], "html");
    assert_eq!(preview["poweredPreview"]["required"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_project_dir_lists_empty() {
    let fixture = setup("empty");
    let (status, _, body) = get(&fixture.router, "/api/projects/p1/files").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(names(&json(&body)), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn since_and_limit_parsing_match_typescript() {
    let fixture = setup("query");
    seed_managed(&fixture.projects_root);

    let (_, _, body) = get(&fixture.router, "/api/projects/p1/files").await;
    assert_eq!(names(&json(&body)).len(), 2);

    let cutoff = NEW_MTIME_MS - 50_000;
    let encoded: String = cutoff.to_string().bytes().map(|b| format!("%{b:02X}")).collect();
    for since in [cutoff.to_string(), "1700000150000".to_string(), encoded] {
        let (_, _, body) = get(&fixture.router, &format!("/api/projects/p1/files?since={since}"))
            .await;
        assert_eq!(names(&json(&body)), vec!["index.html"], "since={since}");
    }

    // Non-numeric / repeated / non-positive `since` all degrade to "no cutoff".
    for uri in [
        "/api/projects/p1/files?since=abc",
        "/api/projects/p1/files?since=",
        "/api/projects/p1/files?since=1&since=2",
        "/api/projects/p1/files?since=0",
        "/api/projects/p1/files?since=-5",
        "/api/projects/p1/files?since=Infinity",
    ] {
        let (_, _, body) = get(&fixture.router, uri).await;
        assert_eq!(names(&json(&body)).len(), 2, "since handling for {uri}");
    }

    for (query, expected) in [
        ("", 96 * 1024),
        ("?limit=", 1024),
        ("?limit=abc", 96 * 1024),
        ("?limit=2048.9", 2048),
        ("?limit=100000000", 512 * 1024),
        ("?limit=-100", 1024),
        ("?limit=Infinity", 96 * 1024),
        ("?limit=1&limit=2", 96 * 1024),
        ("?limit=%32%30%34%38", 2048),
    ] {
        let (_, _, body) = get(
            &fixture.router,
            &format!("/api/projects/p1/text-preview/index.html{query}"),
        )
        .await;
        assert_eq!(
            json(&body)["limit"],
            expected,
            "limit handling for {query:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_statuses_match_typescript() {
    let fixture = setup("errors");
    seed_managed(&fixture.projects_root);

    let (status, _, body) = get(&fixture.router, "/api/projects/bad%21id/files").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&body)["error"]["code"], "BAD_REQUEST");

    let (status, _, body) = get(&fixture.router, "/api/projects/../files").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&body)["error"]["message"], "invalid project id");

    let (status, _, body) = get(&fixture.router, "/api/projects/nope/files").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["code"], "PROJECT_NOT_FOUND");

    let (status, _, body) = get(&fixture.router, "/api/projects/nope/files/index.html").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["code"], "PROJECT_NOT_FOUND");

    let (status, _, body) = get(&fixture.router, "/api/projects/p1/files/missing.html").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["code"], "FILE_NOT_FOUND");

    // The internal version store is unreachable ahead of the project lookup.
    for uri in [
        "/api/projects/nope/files/.file-versions/index.html",
        "/api/projects/p1/text-preview/sub/.file-versions/index.html",
    ] {
        let (status, _, body) = get(&fixture.router, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(json(&body)["error"]["message"], "file not found", "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traversal_attempts_never_leave_the_project_dir() {
    let fixture = setup("traversal");
    seed_managed(&fixture.projects_root);
    std::fs::write(fixture.data_root.join("secret.txt"), "top secret").unwrap();

    for uri in [
        "/api/projects/p1/files/../../secret.txt",
        "/api/projects/p1/files/%2e%2e/%2e%2e/secret.txt",
        "/api/projects/p1/files/..%2F..%2Fsecret.txt",
        "/api/projects/p1/files/%2Fetc%2Fpasswd",
        "/api/projects/p1/files/C%3A%5Cwindows%5Csystem32",
        "/api/projects/p1/files/a/../../secret.txt",
        "/api/projects/p1/text-preview/..%2F..%2Fsecret.txt",
        "/api/projects/p1/text-preview/%2Fetc%2Fpasswd",
    ] {
        let (status, _, body) = get(&fixture.router, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body:?}");
        assert_eq!(json(&body)["error"]["code"], "BAD_REQUEST", "{uri}");
    }

    let (status, _, _) = get(&fixture.router, "/api/projects/p1/files/.live-artifacts/x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Directories are never served as files.
    let (status, _, _) = get(&fixture.router, "/api/projects/p1/files/sub").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The outside file itself is never readable through any encoding.
    let (status, _, body) = get(&fixture.router, "/api/projects/p1/files/secret.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symlink_escape_is_rejected() {
    let fixture = setup("symlink");
    seed_managed(&fixture.projects_root);
    let project = fixture.projects_root.join("p1");
    std::fs::write(fixture.data_root.join("outside.txt"), "out").unwrap();
    std::os::unix::fs::symlink(
        fixture.data_root.join("outside.txt"),
        project.join("escape.txt"),
    )
    .unwrap();

    let (status, _, body) = get(&fixture.router, "/api/projects/p1/files/escape.txt").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json(&body)["error"]["message"],
        "path escapes project dir via symlink"
    );

    let (status, _, body) = get(&fixture.router, "/api/projects/p1/text-preview/escape.txt").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json(&body)["error"]["message"],
        "path escapes project dir via symlink"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imported_projects_refuse_hidden_segments() {
    let fixture = setup("imported");
    let imported_root = fixture.data_root.join("imported");
    std::fs::create_dir_all(&imported_root).unwrap();
    std::fs::write(imported_root.join("index.html"), "<html>ok</html>").unwrap();
    std::fs::write(imported_root.join(".env"), "SECRET=1").unwrap();

    let metadata = serde_json::json!({ "baseDir": imported_root.to_string_lossy() }).to_string();
    fixture
        .store
        .execute(
            "INSERT INTO projects (id, name, metadata_json, created_at, updated_at) \
             VALUES ('p2', 'Imported', ?1, 1, 2)",
            [&metadata],
        )
        .expect("insert p2");

    let (status, _, body) = get(&fixture.router, "/api/projects/p2/files/index.html").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    for uri in [
        "/api/projects/p2/files/.env",
        "/api/projects/p2/files/sub/.env",
        "/api/projects/p2/text-preview/.env",
    ] {
        let (status, _, body) = get(&fixture.router, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(
            json(&body)["error"]["message"],
            "hidden path segments are not accessible in imported folders",
            "{uri}"
        );
    }

    // Managed projects are unaffected by the hidden-segment rule.
    seed_managed(&fixture.projects_root);
    let (status, _, _) = get(&fixture.router, "/api/projects/p1/files/.env").await;
    assert_eq!(status, StatusCode::OK);
}
