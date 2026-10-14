//! Integration tests: the Critique Theater routes over a real socket —
//! `POST /api/projects/:projectId/critique/:runId/interrupt` and
//! `GET /api/projects/:projectId/critique/:runId/artifact` (parity for
//! `apps/daemon/src/critique/interrupt-handler.ts` and `artifact-handler.ts`).

use std::net::SocketAddr;
use std::path::PathBuf;

use od_core::RuntimePaths;
use od_daemon::{DaemonConfig, RunningDaemon};

fn temp_paths(tag: &str) -> RuntimePaths {
    let dir = std::env::temp_dir().join(format!("od-daemon-critique-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    RuntimePaths::resolve(dir).expect("resolve data dir")
}

fn config(paths: RuntimePaths) -> DaemonConfig {
    DaemonConfig {
        paths,
        bind_host: "127.0.0.1".to_string(),
        port: 0,
        api_token: None,
        api_auth_disabled: false,
        web_dist: None,
    }
}

async fn send(addr: SocketAddr, request: &str) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
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

fn seed_project(daemon: &RunningDaemon, id: &str) {
    daemon
        .state
        .store
        .execute(
            "INSERT INTO projects (id, name, created_at, updated_at) VALUES (?1, ?2, 0, 0)",
            [id, "Test Project"],
        )
        .expect("seed project");
}

fn seed_run(
    daemon: &RunningDaemon,
    id: &str,
    project_id: &str,
    status: &str,
    artifact_path: Option<&str>,
) {
    daemon
        .state
        .store
        .execute(
            "INSERT INTO critique_runs (id, project_id, artifact_path, status, rounds_json, protocol_version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, '[]', 1, 0, 0)",
            [id, project_id, artifact_path.unwrap_or(""), status],
        )
        .expect("seed run");
    // Normalize the empty-string artifact sentinel to NULL, matching the
    // TypeScript row shape where absent means null.
    if artifact_path.is_none() {
        daemon
            .state
            .store
            .execute(
                "UPDATE critique_runs SET artifact_path = NULL WHERE id = ?1",
                [id],
            )
            .expect("null artifact");
    }
}

#[tokio::test]
async fn interrupt_unknown_run_is_404() {
    let daemon = RunningDaemon::start(config(temp_paths("unknown"))).await.expect("start");
    let addr = daemon.addr();
    let (head, body) = send(
        addr,
        &format!("POST /api/projects/p1/critique/ghost/interrupt HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(head.contains("404"), "head: {head} body: {body}");
    assert!(body.contains("critique run not found"), "body: {body}");
}

#[tokio::test]
async fn interrupt_cross_project_run_is_404() {
    let daemon = RunningDaemon::start(config(temp_paths("cross"))).await.expect("start");
    seed_project(&daemon, "p1");
    seed_project(&daemon, "p2");
    seed_run(&daemon, "r1", "p2", "running", None);
    let addr = daemon.addr();
    let (head, body) = send(
        addr,
        &format!("POST /api/projects/p1/critique/r1/interrupt HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(head.contains("404"), "head: {head} body: {body}");
    assert!(body.contains("critique run not found"), "body: {body}");
}

#[tokio::test]
async fn interrupt_terminal_run_is_409() {
    let daemon = RunningDaemon::start(config(temp_paths("terminal"))).await.expect("start");
    seed_project(&daemon, "p1");
    seed_run(&daemon, "r1", "p1", "shipped", None);
    let addr = daemon.addr();
    let (head, body) = send(
        addr,
        &format!("POST /api/projects/p1/critique/r1/interrupt HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(head.contains("409"), "head: {head} body: {body}");
    assert!(body.contains("terminal status: shipped"), "body: {body}");
}

#[tokio::test]
async fn interrupt_running_run_recovers_to_interrupted() {
    let daemon = RunningDaemon::start(config(temp_paths("recover"))).await.expect("start");
    seed_project(&daemon, "p1");
    seed_run(&daemon, "r1", "p1", "running", None);
    let addr = daemon.addr();
    let (head, body) = send(
        addr,
        &format!("POST /api/projects/p1/critique/r1/interrupt HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(head.contains("202"), "head: {head} body: {body}");
    assert!(body.contains("\"recovered\":true"), "body: {body}");
    // Idempotent retry: already interrupted now.
    let (head, body) = send(
        addr,
        &format!("POST /api/projects/p1/critique/r1/interrupt HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(head.contains("202"), "head: {head} body: {body}");
    assert!(body.contains("\"prevStatus\":\"interrupted\""), "body: {body}");
}

#[tokio::test]
async fn artifact_without_persisted_bytes_is_404_with_status() {
    let daemon = RunningDaemon::start(config(temp_paths("noartifact"))).await.expect("start");
    seed_project(&daemon, "p1");
    seed_run(&daemon, "r1", "p1", "running", None);
    let addr = daemon.addr();
    let (head, body) = send(
        addr,
        &format!("GET /api/projects/p1/critique/r1/artifact HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(head.contains("404"), "head: {head} body: {body}");
    assert!(body.contains("no persisted artifact"), "body: {body}");
}

#[tokio::test]
async fn artifact_serves_bytes_with_mime() {
    let daemon = RunningDaemon::start(config(temp_paths("bytes"))).await.expect("start");
    seed_project(&daemon, "p1");
    let artifacts: PathBuf = daemon.state.config.paths.data_dir().join("critique-artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let file = artifacts.join("r1.html");
    std::fs::write(&file, "<h1>ship</h1>").expect("write artifact");
    seed_run(&daemon, "r1", "p1", "shipped", Some(file.to_str().unwrap()));
    let addr = daemon.addr();
    let (head, body) = send(
        addr,
        &format!("GET /api/projects/p1/critique/r1/artifact HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(head.contains("200"), "head: {head} body: {body}");
    assert!(head.to_lowercase().contains("content-type: text/html"), "head: {head}");
    assert!(body.contains("<h1>ship</h1>"), "body: {body}");
}
