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
        desktop_auth_secret: None,
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

async fn http_post(addr: SocketAddr, path: &str, body: &str) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
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

/// POST with one extra header line (the desktop import token).
async fn http_post_with_header(
    addr: SocketAddr,
    path: &str,
    body: &str,
    name: &str,
    value: &str,
) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n{name}: {value}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
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

/// PATCH with a JSON body.
async fn http_patch(addr: SocketAddr, path: &str, body: &str) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "PATCH {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
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

/// A raw request with any number of extra header lines.
async fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if body.is_some() {
        request.push_str("Content-Type: application/json\r\n");
    }
    if let Some(body) = body {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("Connection: close\r\n\r\n");
    if let Some(body) = body {
        request.push_str(body);
    }
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
async fn project_create_and_import_match_the_web_contract() {
    let paths = temp_paths("project-create");
    let daemon = RunningDaemon::start(config_with(paths, None))
        .await
        .expect("start");
    let addr = daemon.addr();

    // Create a managed project. The response must be the camelCase shape the
    // web app reads, with metadata parsed out of the SQLite column.
    let (head, body) = http_post(
        addr,
        "/api/projects",
        r#"{"id":"p-create","name":"  Created  ","metadata":{"kind":"prototype"},"customInstructions":"be brief"}"#,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["project"]["id"], "p-create");
    assert_eq!(value["project"]["name"], "Created");
    assert_eq!(value["project"]["metadata"]["kind"], "prototype");
    assert_eq!(value["project"]["customInstructions"], "be brief");
    assert_eq!(value["project"]["workspaceId"], serde_json::Value::Null);
    assert!(value["conversationId"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(
        value["project"].get("metadata_json").is_none(),
        "raw snake_case columns must not leak: {body}"
    );
    // The managed project directory exists with the row, so a run can spawn
    // with it as its cwd.
    assert!(daemon
        .state
        .config
        .paths
        .projects_dir()
        .join("p-create")
        .is_dir());

    // The catalog and the detail route use the same shape.
    let (_, body) = http_get(addr, "/api/projects", None).await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["projects"][0]["id"], "p-create");
    assert_eq!(value["projects"][0]["metadata"]["kind"], "prototype");
    let (head, body) = http_get(addr, "/api/projects/p-create", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["project"]["skillId"], serde_json::Value::Null);
    assert!(value["resolvedDir"].as_str().is_some());

    // linkedDirs are validated and persisted canonical/de-duplicated.
    let linked = std::env::temp_dir().join(format!("od-linked-http-{}", std::process::id()));
    std::fs::create_dir_all(&linked).expect("linked dir");
    let payload = serde_json::json!({
        "id": "p-linked",
        "name": "Linked",
        "metadata": { "linkedDirs": [linked.to_string_lossy(), linked.to_string_lossy()] },
    })
    .to_string();
    let (head, body) = http_post(addr, "/api/projects", &payload).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let dirs = value["project"]["metadata"]["linkedDirs"]
        .as_array()
        .expect("linked dirs");
    assert_eq!(dirs.len(), 1, "duplicates collapse: {body}");
    assert_eq!(
        dirs[0],
        linked.canonicalize().expect("canonical").to_string_lossy().as_ref()
    );

    // baseDir is privileged: only the import route may set it.
    let (head, body) = http_post(
        addr,
        "/api/projects",
        r#"{"id":"p-smuggle","name":"Nope","metadata":{"baseDir":"/etc"}}"#,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(
        body.contains("baseDir can only be set via POST /api/import/folder"),
        "body: {body}"
    );

    // Import a real folder: canonical path persisted, entry file detected,
    // seed conversation + Design Files tab state created.
    let folder = std::env::temp_dir().join(format!("od-import-{}", std::process::id()));
    std::fs::create_dir_all(&folder).expect("folder");
    std::fs::write(folder.join("index.html"), b"<html>imported</html>").expect("write");
    let payload = serde_json::json!({ "baseDir": folder.to_string_lossy() }).to_string();
    let (head, body) = http_post(addr, "/api/import/folder", &payload).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let project_id = value["project"]["id"].as_str().expect("project id").to_string();
    assert_eq!(value["entryFile"], "index.html");
    assert_eq!(value["project"]["metadata"]["importedFrom"], "folder");
    assert_eq!(
        value["project"]["metadata"]["baseDir"],
        folder.canonicalize().expect("canonical").to_string_lossy().as_ref()
    );
    assert_eq!(value["project"]["name"], folder.file_name().unwrap().to_string_lossy().as_ref());
    let (_, files) = http_get(addr, &format!("/api/projects/{project_id}/files"), None).await;
    assert!(files.contains("index.html"), "files: {files}");

    // System roots and the data directory are refused.
    let (head, body) = http_post(addr, "/api/import/folder", r#"{"baseDir":"/etc"}"#).await;
    assert!(head.contains("400"), "status: {head}");
    assert!(body.contains("system or credential directory"), "body: {body}");
    let (head, body) = http_post(addr, "/api/import/folder", r#"{"baseDir":"relative/dir"}"#).await;
    assert!(head.contains("400"), "status: {head}");
    assert!(body.contains("baseDir must be absolute"), "body: {body}");
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
async fn media_task_list_matches_the_typescript_shape() {
    let paths = temp_paths("media-tasks");
    let project_dir = paths.projects_dir().join("p1");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    std::fs::write(project_dir.join("shot.png"), b"png-bytes").expect("write file");
    let daemon = RunningDaemon::start(config_with(paths, None))
        .await
        .expect("start");
    let addr = daemon.addr();

    daemon
        .state
        .store
        .execute(
            "INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'Demo', 1, 2)",
            (),
        )
        .expect("insert project");
    // A finished task with a file that still exists, plus a live one.
    daemon
        .state
        .store
        .execute(
            "INSERT INTO media_tasks (id, project_id, run_id, status, surface, model, \
             progress_json, file_json, error_json, started_at, ended_at, created_at, updated_at) \
             VALUES ('m1', 'p1', 'r1', 'done', 'image', 'img-1', \
             '[\"a\",\"b\",\"c\",\"d\"]', '{\"name\":\"shot.png\"}', NULL, 1000, 4500, 1000, 4500)",
            (),
        )
        .expect("insert done task");
    daemon
        .state
        .store
        .execute(
            "INSERT INTO media_tasks (id, project_id, run_id, status, surface, model, \
             progress_json, file_json, error_json, started_at, ended_at, created_at, updated_at) \
             VALUES ('m2', 'p1', 'r1', 'running', 'image', 'img-1', \
             '[]', NULL, NULL, 2000, NULL, 2000, 2000)",
            (),
        )
        .expect("insert running task");

    // Default: terminal rows are filtered out.
    let (head, body) = http_get(addr, "/api/projects/p1/media/tasks", None).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let tasks = value["tasks"].as_array().expect("tasks");
    assert_eq!(tasks.len(), 1, "only the live task: {body}");
    assert_eq!(tasks[0]["taskId"], "m2");

    // includeDone=1 keeps the done row, confirms its file, and reports the
    // last three progress lines + batch coordinates.
    let (head, body) = http_get(addr, "/api/projects/p1/media/tasks?includeDone=1", None).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let tasks = value["tasks"].as_array().expect("tasks");
    assert_eq!(tasks.len(), 2, "both tasks: {body}");
    assert_eq!(tasks[0]["taskId"], "m2");
    let done = &tasks[1];
    assert_eq!(done["taskId"], "m1");
    assert_eq!(done["status"], "done");
    assert_eq!(done["file"]["name"], "shot.png");
    assert_eq!(done["file"]["kind"], "image");
    assert_eq!(done["file"]["mime"], "image/png");
    assert_eq!(done["file"]["size"], 9);
    assert_eq!(done["progress"].as_array().expect("progress").len(), 3);
    assert_eq!(done["progress"][0], "b");
    assert_eq!(done["progressCount"], 4);
    assert_eq!(done["elapsed"], 3);
    // Same run + same surface + overlapping lifetimes → one batch of two.
    assert_eq!(done["batchSize"], 2);
    assert_eq!(done["batchIndex"], 1);
    assert_eq!(tasks[0]["batchIndex"], 2);
    assert!(done.get("error").is_none(), "done tasks carry no error");

    let (head, body) = http_get(addr, "/api/projects/nope/media/tasks", None).await;
    assert!(head.contains("404"), "status: {head}");
    assert!(body.contains("PROJECT_NOT_FOUND"), "body: {body}");
}

async fn http_put(addr: SocketAddr, path: &str, body: &str) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "PUT {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
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
async fn app_config_round_trips_the_onboarding_write() {
    let daemon = RunningDaemon::start(config_with(temp_paths("app-config"), None))
        .await
        .expect("start");
    let addr = daemon.addr();

    // Fresh install: telemetry defaults are on, so onboarding events emit.
    let (head, body) = http_get(addr, "/api/app-config", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["config"]["telemetry"]["metrics"], true);

    // The onboarding Continue write must land and survive a reread.
    let (head, body) = http_put(
        addr,
        "/api/app-config",
        r#"{"agentId":"qwen","mode":"daemon","onboardingCompleted":true}"#,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["config"]["agentId"], "qwen");
    assert_eq!(value["config"]["onboardingCompleted"], true);
    // `mode` is request state, not a persisted preference.
    assert!(value["config"].get("mode").is_none(), "body: {body}");

    let (_, body) = http_get(addr, "/api/app-config", None).await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["config"]["agentId"], "qwen");
    assert_eq!(value["config"]["onboardingCompleted"], true);
}

/// Raw request with an arbitrary extra header line (ETag revalidation, Origin).
async fn http_get_with_header(
    addr: SocketAddr,
    path: &str,
    name: &str,
    value: &str,
) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{name}: {value}\r\nConnection: close\r\n\r\n"
    );
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

/// Read the head of a streaming response without waiting for it to end.
async fn http_get_stream_head(addr: SocketAddr, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
        .await
        .expect("write");
    // Headers and the first frames can arrive in separate packets, so read
    // until the handshake shows up (or the deadline passes).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut collected = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let read = match tokio::time::timeout(
            remaining,
            tokio::io::AsyncReadExt::read(&mut stream, &mut buffer),
        )
        .await
        {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(read)) => read,
            Ok(Err(_)) => break,
        };
        collected.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&collected);
        if text.contains("event: ready") || text.contains("404") {
            break;
        }
    }
    String::from_utf8_lossy(&collected).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_catalog_serves_the_summary_and_detail_shapes() {
    let daemon = RunningDaemon::start(config_with(temp_paths("catalog"), None))
        .await
        .expect("start");
    let addr = daemon.addr();
    let data_dir = daemon.state.config.paths.data_dir().to_path_buf();
    let skill_dir = data_dir.join("skills").join("hero");
    std::fs::create_dir_all(skill_dir.join("examples")).expect("mkdir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: hero\ndescription: Build a hero section for a landing page.\nod:\n  mode: prototype\ntriggers:\n  - hero\n---\nbody text\n",
    )
    .expect("write");
    std::fs::write(skill_dir.join("examples").join("one.html"), "<html></html>").expect("write");

    // Listing: summaries only, with `hasBody` and the derived example card.
    let (head, body) = http_get(addr, "/api/skills", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let skills = value["skills"].as_array().expect("skills");
    let hero = skills
        .iter()
        .find(|skill| skill["id"] == "hero")
        .expect("hero entry");
    assert_eq!(hero["source"], "user");
    assert_eq!(hero["mode"], "prototype");
    assert_eq!(hero["hasBody"], true);
    assert_eq!(hero["aggregatesExamples"], true);
    assert!(hero.get("body").is_none(), "listing must not ship bodies");
    assert!(hero.get("dir").is_none(), "listing must not ship paths");
    assert!(
        skills.iter().any(|skill| skill["id"] == "hero:one"),
        "derived card: {body}"
    );

    // Detail: the composed body, still without the on-disk path.
    let (head, body) = http_get(addr, "/api/skills/hero", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["id"], "hero");
    assert!(value["body"].as_str().expect("body").contains("body text"));
    assert!(value.get("dir").is_none(), "detail must not ship paths");
    let (head, _) = http_get(addr, "/api/skills/missing", None).await;
    assert!(head.contains("404"), "status: {head}");

    // The template catalogue shares the shape and is empty here.
    let (head, body) = http_get(addr, "/api/design-templates", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["designTemplates"], serde_json::json!([]));
    let (head, _) = http_get(addr, "/api/design-templates/missing", None).await;
    assert!(head.contains("404"), "status: {head}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn design_system_catalog_serves_the_picker_and_detail_shapes() {
    let daemon = RunningDaemon::start(config_with(temp_paths("design-systems"), None))
        .await
        .expect("start");
    let addr = daemon.addr();
    let data_dir = daemon.state.config.paths.data_dir().to_path_buf();
    let system_dir = data_dir.join("design-systems").join("acme");
    std::fs::create_dir_all(&system_dir).expect("mkdir");
    std::fs::write(
        system_dir.join("DESIGN.md"),
        "# Design System Inspired by Acme\n\n> Category: Industrial\n> Surface: web\n\nA crisp industrial identity.\n\n- **Background:** `#F4F4F4`\n- **Heading:** `#101010`\n- **Primary:** `#B23A2E`\n- **Border:** `#D8D8D8`\n",
    )
    .expect("write");

    let (head, body) = http_get(addr, "/api/design-systems", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let systems = value["designSystems"].as_array().expect("designSystems");
    let acme = systems
        .iter()
        .find(|system| system["id"] == "user:acme")
        .expect("user system");
    assert_eq!(acme["title"], "Acme");
    assert_eq!(acme["category"], "Industrial");
    assert_eq!(acme["summary"], "A crisp industrial identity.");
    assert_eq!(acme["source"], "user");
    assert_eq!(acme["status"], "draft");
    assert_eq!(acme["isEditable"], true);
    assert_eq!(
        acme["swatches"],
        serde_json::json!(["#f4f4f4", "#d8d8d8", "#101010", "#b23a2e"])
    );
    assert!(acme.get("body").is_none(), "listing must not ship bodies");

    let (head, body) = http_get(addr, "/api/design-systems/user%3Aacme", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(value["body"].as_str().expect("body").contains("crisp industrial"));
    assert_eq!(value["canMutate"], true);
    assert_eq!(value["designSystem"]["id"], "user:acme");
    assert_eq!(value["designSystem"]["body"], value["body"]);

    let (head, _) = http_get(addr, "/api/design-systems/missing", None).await;
    assert!(head.contains("404"), "status: {head}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_reads_answer_the_shell_without_a_workspace_identity() {
    let daemon = RunningDaemon::start(config_with(temp_paths("bootstrap"), None))
        .await
        .expect("start");
    let addr = daemon.addr();

    // Workspace identity: this daemon has none, and says so instead of 404ing.
    let (head, body) = http_get(addr, "/api/workspace/context", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(value["context"].is_null(), "body: {body}");
    let (head, body) = http_get(addr, "/api/workspace/directory", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["items"], serde_json::json!([]));
    assert!(value["activeWorkspaceId"].is_null());

    // Directory probe: a real folder exists, a deleted one does not.
    let dir = std::env::temp_dir().join(format!("od-dir-exists-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let payload = serde_json::json!({ "path": dir }).to_string();
    let (head, body) = http_post(addr, "/api/dir-exists", &payload).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["exists"], true);
    let payload = serde_json::json!({ "path": dir.join("gone") }).to_string();
    let (_, body) = http_post(addr, "/api/dir-exists", &payload).await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["exists"], false);

    // Recent dirs are pruned to what still exists and the pruned list is
    // persisted back.
    let keep = std::env::temp_dir().join(format!("od-recent-keep-{}", std::process::id()));
    std::fs::create_dir_all(&keep).expect("keep");
    let gone = std::env::temp_dir().join(format!("od-recent-gone-{}", std::process::id()));
    let payload = serde_json::json!({ "recentLinkedDirs": [keep, gone] }).to_string();
    let (head, _) = http_put(addr, "/api/app-config", &payload).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let (head, body) = http_get(addr, "/api/recent-dirs", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["dirs"].as_array().expect("dirs").len(), 1, "body: {body}");
    let (_, body) = http_get(addr, "/api/app-config", None).await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        value["config"]["recentLinkedDirs"].as_array().expect("persisted").len(),
        1,
        "body: {body}"
    );

    // Tabs: the project create seeds the Design Files tab document.
    let payload = serde_json::json!({ "id": "p-tabs", "name": "Tabs" }).to_string();
    let (head, _) = http_post(addr, "/api/projects", &payload).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let (head, body) = http_get(addr, "/api/projects/p-tabs/tabs", None).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(value["tabs"].is_array(), "body: {body}");
    let (head, _) = http_get(addr, "/api/projects/missing/tabs", None).await;
    assert!(head.contains("404"), "status: {head}");

    // Project events: the client's reconnect handshake, not a 404 loop.
    let head = http_get_stream_head(addr, "/api/projects/p-tabs/events").await;
    assert!(head.contains("200 OK"), "status: {head}");
    assert!(head.contains("text/event-stream"), "headers: {head}");
    assert!(head.contains("event: ready"), "stream: {head}");
    assert!(head.contains("p-tabs"), "stream: {head}");
    let head = http_get_stream_head(addr, "/api/projects/missing/events").await;
    assert!(head.contains("404"), "status: {head}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_serves_artifact_bytes_with_revalidating_etag() {
    let daemon = RunningDaemon::start(config_with(temp_paths("raw"), None))
        .await
        .expect("start");
    let addr = daemon.addr();
    let payload = serde_json::json!({ "id": "p-raw", "name": "Raw" }).to_string();
    let (head, _) = http_post(addr, "/api/projects", &payload).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let dir = daemon.state.config.paths.projects_dir().join("p-raw");
    std::fs::write(dir.join("hero.html"), b"<html><body>raw</body></html>").expect("write");

    // The preview iframe's HEAD/GET pair: bytes, MIME, and a cache validator.
    let (head, body) = http_get(addr, "/api/projects/p-raw/raw/hero.html", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    assert!(head.contains("text/html"), "mime: {head}");
    assert!(head.contains("no-cache"), "cache: {head}");
    let etag = head
        .lines()
        .find_map(|line| line.strip_prefix("etag: ").or_else(|| line.strip_prefix("ETag: ")))
        .expect("etag header")
        .trim()
        .to_string();
    assert_eq!(body, "<html><body>raw</body></html>");

    // Unchanged bytes revalidate to 304 and carry no body.
    let (head, body) =
        http_get_with_header(addr, "/api/projects/p-raw/raw/hero.html", "If-None-Match", &etag)
            .await;
    assert!(head.contains("304"), "status: {head}");
    assert!(body.is_empty(), "304 body: {body:?}");

    // A sandboxed preview iframe sends `Origin: null`; it must be allowed.
    let (head, _) =
        http_get_with_header(addr, "/api/projects/p-raw/raw/hero.html", "Origin", "null").await;
    assert!(head.contains("access-control-allow-origin: *"), "cors: {head}");

    let (head, _) = http_get(addr, "/api/projects/p-raw/raw/missing.html", None).await;
    assert!(head.contains("404"), "status: {head}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_scope_reads_as_unbound_and_gates_artifact_previews() {
    let daemon = RunningDaemon::start(config_with(temp_paths("scope"), None))
        .await
        .expect("start");
    let addr = daemon.addr();
    let payload = serde_json::json!({ "id": "p-scope", "name": "Scoped" }).to_string();
    let (head, _) = http_post(addr, "/api/projects", &payload).await;
    assert!(head.contains("200 OK"), "status: {head}");

    // The web app maps `unbound` to local resource authority, which is what
    // lets the artifact preview render.
    let (head, body) = http_get(addr, "/api/projects/p-scope/workspace-scope", None).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["scope"]["kind"], "unbound");
    assert_eq!(value["scope"]["projectId"], "p-scope");
    assert!(value["scope"]["workspaceId"].is_null());
    assert!(value["scope"]["context"].is_null());

    let (head, _) = http_get(addr, "/api/projects/missing/workspace-scope", None).await;
    assert!(head.contains("404"), "status: {head}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_route_matches_the_web_contract() {
    let daemon = RunningDaemon::start(config_with(temp_paths("agents"), None))
        .await
        .expect("start");
    let addr = daemon.addr();

    let (head, body) = http_get(addr, "/api/agents", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let agents = value["agents"].as_array().expect("agents");
    assert!(agents.len() > 5, "expected the shipped registry: {body}");
    let claude = agents
        .iter()
        .find(|agent| agent["id"] == "claude")
        .expect("claude in registry");
    assert_eq!(claude["bin"], "claude");
    assert!(claude["models"].as_array().is_some_and(|models| !models.is_empty()));
    assert_eq!(claude["modelsSource"], "fallback");
    assert_eq!(claude["promptInputFormat"], "stream-json");
    assert!(claude["available"].is_boolean());

    // The cold-start path: one `agent` event per row, then `done`.
    let (head, body) = http_get(addr, "/api/agents?stream=1", None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    assert!(
        head.to_ascii_lowercase().contains("text/event-stream"),
        "status: {head}"
    );
    assert!(body.contains("event: agent\ndata: "), "body: {body}");
    assert!(body.contains("event: done\ndata: {}"), "body: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_working_dir_route_rebinds_a_project_at_a_picked_folder() {
    let paths = temp_paths("working-dir");
    let daemon = RunningDaemon::start(config_with(paths, None))
        .await
        .expect("start");
    let addr = daemon.addr();

    // A managed project with no folder yet.
    let (head, body) = http_post(
        addr,
        "/api/projects",
        r#"{"id":"p-rebind","name":"Rebind","metadata":{"kind":"prototype"}}"#,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");

    // A missing project is a 404, not a silent create.
    let (head, body) = http_post(
        addr,
        "/api/projects/p-absent/working-dir",
        r#"{"baseDir":"/tmp"}"#,
    )
    .await;
    assert!(head.contains("404"), "status: {head}");
    assert!(body.contains("PROJECT_NOT_FOUND"), "body: {body}");

    // baseDir is required.
    let (head, body) = http_post(addr, "/api/projects/p-rebind/working-dir", r#"{}"#).await;
    assert!(head.contains("400"), "status: {head}");
    assert!(body.contains("baseDir required"), "body: {body}");

    // The orchestrator workspace contract is enforced.
    let (head, body) = http_post(
        addr,
        "/api/projects/p-rebind/working-dir",
        r#"{"baseDir":"/tmp","orchestratorWorkspace":{"kind":"team"}}"#,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(
        body.contains("orchestratorWorkspace.kind must be"),
        "body: {body}"
    );

    // Rebind to a real folder: the canonical path, the entry file and the
    // import marker land in the metadata, and the saved tab state resets.
    let folder = std::env::temp_dir().join(format!("od-rebind-{}", std::process::id()));
    std::fs::create_dir_all(&folder).expect("folder");
    std::fs::write(folder.join("index.html"), b"<html>rebound</html>").expect("write");
    let canonical = folder.canonicalize().expect("canonical");
    let payload = serde_json::json!({
        "baseDir": folder.to_string_lossy(),
        "orchestratorWorkspace": { "kind": "scratch", "writeback": "external" },
    })
    .to_string();
    let (head, body) = http_post(addr, "/api/projects/p-rebind/working-dir", &payload).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["baseDir"], canonical.to_string_lossy().as_ref());
    assert_eq!(value["entryFile"], "index.html");
    assert_eq!(value["project"]["id"], "p-rebind");
    assert_eq!(value["project"]["metadata"]["importedFrom"], "folder");
    assert_eq!(
        value["project"]["metadata"]["baseDir"],
        canonical.to_string_lossy().as_ref()
    );
    assert_eq!(value["project"]["metadata"]["kind"], "prototype");
    assert_eq!(
        value["project"]["metadata"]["orchestratorWorkspace"]["kind"],
        "scratch"
    );
    assert_eq!(
        value["project"]["metadata"]["orchestratorWorkspace"]["writeback"],
        "external"
    );

    // The rebind is durable: the project read carries the new folder.
    let (_, body) = http_get(addr, "/api/projects/p-rebind", None).await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        value["project"]["metadata"]["baseDir"],
        canonical.to_string_lossy().as_ref()
    );

    // The system roots are refused with the working-dir wording.
    let (head, body) = http_post(
        addr,
        "/api/projects/p-rebind/working-dir",
        r#"{"baseDir":"/etc"}"#,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(
        body.contains("system or credential directory"),
        "body: {body}"
    );
    let (head, body) = http_post(
        addr,
        "/api/projects/p-rebind/working-dir",
        r#"{"baseDir":"relative/dir"}"#,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(body.contains("baseDir must be absolute"), "body: {body}");
}

/// The web app renames a project and edits its custom instructions through
/// `PATCH /api/projects/:id`; the daemon-owned metadata rules ride along.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_patch_route_renames_a_project_and_guards_the_daemon_owned_metadata() {
    let paths = temp_paths("project-patch");
    let daemon = RunningDaemon::start(config_with(paths, None))
        .await
        .expect("start");
    let addr = daemon.addr();

    let (head, body) = http_post(
        addr,
        "/api/projects",
        r#"{"id":"p-patch","name":"Deck","metadata":{"kind":"prototype","scenarioBinding":{"id":"s-1"}}}"#,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");

    // A missing project is a 404, not a silent create.
    let (head, body) = http_patch(addr, "/api/projects/p-absent", r#"{"name":"Nope"}"#).await;
    assert!(head.contains("404"), "status: {head}");
    assert!(body.contains("PROJECT_NOT_FOUND"), "body: {body}");

    // A rename keeps every other field, including the daemon-owned binding.
    let (head, body) = http_patch(addr, "/api/projects/p-patch", r#"{"name":"Deck 2"}"#).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["project"]["name"], "Deck 2");
    assert_eq!(value["project"]["metadata"]["kind"], "prototype");
    assert_eq!(value["project"]["metadata"]["scenarioBinding"]["id"], "s-1");

    // The daemon owns the scenario binding: a patch may not change it.
    let (head, body) = http_patch(
        addr,
        "/api/projects/p-patch",
        r#"{"metadata":{"scenarioBinding":{"id":"s-2"}}}"#,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(body.contains("scenarioBinding is daemon-owned"), "body: {body}");

    // Repeating the same binding is allowed.
    let (head, body) = http_patch(
        addr,
        "/api/projects/p-patch",
        r#"{"metadata":{"scenarioBinding":{"id":"s-1"}}}"#,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");

    // The custom instructions are bounded.
    let (head, body) = http_patch(addr, "/api/projects/p-patch", r#"{"customInstructions":42}"#).await;
    assert!(head.contains("400"), "status: {head}");
    assert!(
        body.contains("customInstructions must be a string or null"),
        "body: {body}"
    );
    let long = "x".repeat(5001);
    let payload = serde_json::json!({ "customInstructions": long }).to_string();
    let (head, body) = http_patch(addr, "/api/projects/p-patch", &payload).await;
    assert!(head.contains("400"), "status: {head}");
    assert!(
        body.contains("customInstructions exceeds 5 000 character limit"),
        "body: {body}"
    );
    let (head, body) = http_patch(
        addr,
        "/api/projects/p-patch",
        r#"{"customInstructions":"Be brief."}"#,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["project"]["customInstructions"], "Be brief.");

    // The catalogue ids are checked against the local catalogue.
    let (head, body) = http_patch(
        addr,
        "/api/projects/p-patch",
        r#"{"skillId":"no-such-skill"}"#,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(body.contains("SKILL_NOT_FOUND"), "body: {body}");
    let (head, body) = http_patch(
        addr,
        "/api/projects/p-patch",
        r#"{"designSystemId":"no-such-system"}"#,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(body.contains("DESIGN_SYSTEM_NOT_FOUND"), "body: {body}");

    // A project with no folder cannot acquire one through a patch.
    let (head, body) = http_patch(
        addr,
        "/api/projects/p-patch",
        r#"{"metadata":{"baseDir":"/tmp"}}"#,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(
        body.contains("baseDir can only be set via POST /api/import/folder"),
        "body: {body}"
    );
}

/// An imported project stays at the folder it was imported from: the patch
/// route re-stamps the import-owned metadata instead of letting a patch
/// detach it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_patch_route_keeps_an_imported_project_at_its_folder() {
    let paths = temp_paths("project-patch-import");
    let daemon = RunningDaemon::start(config_with(paths, None))
        .await
        .expect("start");
    let addr = daemon.addr();

    let (head, body) = http_post(addr, "/api/projects", r#"{"id":"p-imported","name":"Imported"}"#).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");

    let folder = std::env::temp_dir().join(format!("od-patch-import-{}", std::process::id()));
    std::fs::create_dir_all(&folder).expect("folder");
    std::fs::write(folder.join("index.html"), b"<html>imported</html>").expect("write");
    let canonical = folder.canonicalize().expect("canonical");
    let (head, body) = http_post(
        addr,
        "/api/projects/p-imported/working-dir",
        &serde_json::json!({ "baseDir": folder.to_string_lossy() }).to_string(),
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");

    // A metadata patch keeps the folder.
    let (head, body) = http_patch(
        addr,
        "/api/projects/p-imported",
        r#"{"metadata":{"kind":"deck"}}"#,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["project"]["metadata"]["kind"], "deck");
    assert_eq!(
        value["project"]["metadata"]["baseDir"],
        canonical.to_string_lossy().as_ref()
    );
    assert_eq!(value["project"]["metadata"]["importedFrom"], "folder");

    // Moving the folder through a patch is refused.
    let (head, body) = http_patch(
        addr,
        "/api/projects/p-imported",
        r#"{"metadata":{"baseDir":"/tmp/elsewhere"}}"#,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(
        body.contains("baseDir is immutable after import; use a new import to change it"),
        "body: {body}"
    );

    // Clearing the metadata of an imported project is refused.
    let (head, body) = http_patch(addr, "/api/projects/p-imported", r#"{"metadata":null}"#).await;
    assert!(head.contains("400"), "status: {head}");
    assert!(
        body.contains("metadata cannot be cleared for imported projects"),
        "body: {body}"
    );
}

/// The workspace-scoped project list needs a member identity, and a create
/// that names a workspace binds the project to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workspace_attributed_create_is_bound_and_scoped() {
    let paths = temp_paths("workspace-create");
    let daemon = RunningDaemon::start(config_with(paths, None))
        .await
        .expect("start");
    let addr = daemon.addr();
    let workspace_headers = [
        ("x-od-workspace-id", "w-1"),
        ("x-od-workspace-member-id", "m-1"),
    ];

    // Without a member identity there is no context to list against.
    let (head, body) = http_get(addr, "/api/workspaces/w-1/projects", None).await;
    assert!(head.contains("401"), "status: {head}");
    assert!(body.contains("WORKSPACE_CONTEXT_REQUIRED"), "body: {body}");

    // A create that names a workspace binds the project to it.
    let (head, body) = http_request(
        addr,
        "POST",
        "/api/projects",
        &workspace_headers,
        Some(r#"{"id":"p-bound","name":"Bound"}"#),
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["project"]["workspaceId"], "w-1");

    // The no-scope catalog hides a claimed project.
    let (_, body) = http_get(addr, "/api/projects", None).await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        value["projects"].as_array().map(|items| items.len()),
        Some(0),
        "body: {body}"
    );

    // The scoped list serves it with the summary shape.
    let (head, body) = http_request(
        addr,
        "GET",
        "/api/workspaces/w-1/projects",
        &workspace_headers,
        None,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let projects = value["projects"].as_array().expect("projects");
    assert_eq!(projects.len(), 1, "body: {body}");
    let project = &projects[0];
    assert_eq!(project["id"], "p-bound");
    assert_eq!(project["workspaceId"], "w-1");
    assert_eq!(project["visibility"], "personal");
    assert_eq!(project["createdByWorkspaceMemberId"], "m-1");
    assert_eq!(project["syncState"], "local_only");
    assert_eq!(project["currentUserAccess"]["canOpen"], true);
    assert_eq!(project["currentUserAccess"]["canRename"], true);
    assert_eq!(project["project"]["workspaceId"], "w-1");

    // The view filters.
    let (_, body) = http_request(
        addr,
        "GET",
        "/api/workspaces/w-1/projects?view=team",
        &workspace_headers,
        None,
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        value["projects"].as_array().map(|items| items.len()),
        Some(0),
        "body: {body}"
    );
    let (_, body) = http_request(
        addr,
        "GET",
        "/api/workspaces/w-1/projects?view=drafts",
        &workspace_headers,
        None,
    )
    .await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        value["projects"].as_array().map(|items| items.len()),
        Some(1),
        "body: {body}"
    );

    // An unknown view is a 400.
    let (head, body) = http_request(
        addr,
        "GET",
        "/api/workspaces/w-1/projects?view=bogus",
        &workspace_headers,
        None,
    )
    .await;
    assert!(head.contains("400"), "status: {head}");
    assert!(
        body.contains("view must be all, recent, drafts, or team"),
        "body: {body}"
    );
}

/// A personal workspace adopts the projects no workspace has claimed — the
/// pre-workspace projects, which would otherwise vanish from every list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_personal_workspace_adopts_the_unclaimed_projects() {
    let paths = temp_paths("workspace-adopt");
    let daemon = RunningDaemon::start(config_with(paths, None))
        .await
        .expect("start");
    let addr = daemon.addr();
    let workspace_headers = [
        ("x-od-workspace-id", "w-1"),
        ("x-od-workspace-member-id", "m-1"),
    ];

    // A headerless create stays in the no-scope catalog.
    let (head, body) = http_post(
        addr,
        "/api/projects",
        r#"{"id":"p-legacy","name":"Legacy"}"#,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(value["project"]["workspaceId"].is_null(), "body: {body}");
    let (_, body) = http_get(addr, "/api/projects", None).await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        value["projects"].as_array().map(|items| items.len()),
        Some(1),
        "body: {body}"
    );

    // The personal workspace adopts it.
    let (head, body) = http_request(
        addr,
        "GET",
        "/api/workspaces/w-1/projects",
        &workspace_headers,
        None,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let projects = value["projects"].as_array().expect("projects");
    assert_eq!(projects.len(), 1, "body: {body}");
    // An adopted project records no owner: reading it never hands over
    // ownership.
    assert!(
        projects[0]["createdByWorkspaceMemberId"].is_null(),
        "body: {body}"
    );
    // ... and it is not the caller's own, so it stays read-only.
    assert_eq!(projects[0]["currentUserAccess"]["canRename"], false);

    // The no-scope catalog no longer lists it: it belongs to a workspace now.
    let (_, body) = http_get(addr, "/api/projects", None).await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        value["projects"].as_array().map(|items| items.len()),
        Some(0),
        "body: {body}"
    );
}

/// The desktop import-token gate: once a desktop has registered a secret,
/// the folder-import routes only accept a token bound to the picked folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_desktop_import_gate_binds_a_token_to_one_folder() {
    let secret = vec![0x5au8; 32];
    let mut config = config_with(temp_paths("desktop-gate"), None);
    config.desktop_auth_secret = Some(secret.clone());
    let daemon = RunningDaemon::start(config).await.expect("start");
    let addr = daemon.addr();

    let folder = std::env::temp_dir().join(format!("od-gate-{}", std::process::id()));
    std::fs::create_dir_all(&folder).expect("folder");
    let other = std::env::temp_dir().join(format!("od-gate-other-{}", std::process::id()));
    std::fs::create_dir_all(&other).expect("folder");

    // No token at all: the gate refuses the import.
    let payload = serde_json::json!({ "baseDir": folder.to_string_lossy() }).to_string();
    let (head, body) = http_post_with_header(
        addr,
        "/api/import/folder",
        &payload,
        "x-od-desktop-import-token",
        "",
    )
    .await;
    assert!(head.contains("403"), "status: {head} body: {body}");
    assert!(body.contains("desktop import token rejected"), "body: {body}");
    assert!(body.contains("\"reason\""), "body: {body}");

    // A token minted for another folder does not open this one.
    let foreign = od_daemon::desktop_import::mint_import_token(
        &secret,
        &other.to_string_lossy(),
        od_daemon::desktop_import::now_ms(),
    );
    let (head, body) = http_post_with_header(
        addr,
        "/api/import/folder",
        &payload,
        "x-od-desktop-import-token",
        &foreign,
    )
    .await;
    assert!(head.contains("403"), "status: {head} body: {body}");

    // The token for this folder is accepted, and the import records that the
    // folder came from the trusted picker.
    let token = od_daemon::desktop_import::mint_import_token(
        &secret,
        &folder.to_string_lossy(),
        od_daemon::desktop_import::now_ms(),
    );
    let (head, body) = http_post_with_header(
        addr,
        "/api/import/folder",
        &payload,
        "x-od-desktop-import-token",
        &token,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["project"]["metadata"]["fromTrustedPicker"], true);

    // A token is single use: replaying it is refused.
    let (head, body) = http_post_with_header(
        addr,
        "/api/import/folder",
        &payload,
        "x-od-desktop-import-token",
        &token,
    )
    .await;
    assert!(head.contains("403"), "status: {head} body: {body}");

    // The working-dir route shares the gate.
    let (head, body) = http_post(
        addr,
        "/api/projects",
        r#"{"id":"p-gate","name":"Gate"}"#,
    )
    .await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let rebind = serde_json::json!({ "baseDir": folder.to_string_lossy() }).to_string();
    let (head, body) = http_post(addr, "/api/projects/p-gate/working-dir", &rebind).await;
    assert!(head.contains("403"), "status: {head} body: {body}");
    assert!(body.contains("desktop import token rejected"), "body: {body}");
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
