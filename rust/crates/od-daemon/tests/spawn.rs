//! Integration tests: agent child spawning over a real socket — a `qwen`
//! shim on PATH proves the runs engine spawns the resolved binary,
//! streams stdout lines as events, and finishes the run from the exit
//! status (parity for the spawn layer in `runtimes/agent-process.ts`).

use std::net::SocketAddr;
use std::sync::Once;

use od_core::RuntimePaths;
use od_daemon::{DaemonConfig, RunningDaemon};

static INIT_PATH: Once = Once::new();

fn write_shim(dir: &std::path::Path, name: &str, script: &str) {
    let shim = dir.join(name);
    std::fs::write(&shim, script).expect("shim script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&shim).expect("shim meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).expect("shim perms");
    }
}

fn shim_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("od-spawn-shim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("shim dir");
    // Exits at once after one stdout line: proves spawn → stream → finish.
    write_shim(&dir, "qwen", "#!/bin/sh\ncat > /dev/null\necho hello-from-shim\nexit 0\n");
    // Sleeps so the run stays alive for steer/cancel coverage. Reads and
    // discards stdin first so the pump's prompt write never blocks.
    write_shim(
        &dir,
        "claude",
        "#!/bin/sh\ncat > /dev/null\nsleep 30\n",
    );
    // Emits one Copilot JSONL turn, then exits: proves protocol parsing.
    write_shim(
        &dir,
        "copilot",
        concat!(
            "#!/bin/sh\n",
            "cat > /dev/null\n",
            "echo '{\"type\":\"assistant.message_delta\",\"data\":{\"deltaContent\":\"hey\"}}'\n",
            "echo '{\"type\":\"result\",\"success\":true}'\n",
            "exit 0\n",
        ),
    );
    dir
}

fn ensure_shim_on_path() {
    INIT_PATH.call_once(|| {
        let dir = shim_dir();
        let mut paths = vec![dir];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        let joined = std::env::join_paths(paths).expect("join paths");
        std::env::set_var("PATH", joined);
    });
}

fn temp_paths(tag: &str) -> RuntimePaths {
    let dir = std::env::temp_dir().join(format!("od-daemon-spawn-{tag}-{}", std::process::id()));
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

async fn post_run(addr: SocketAddr, body: &str) -> (String, String) {
    send(
        addr,
        &format!(
            "POST /api/runs HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

async fn get_run(addr: SocketAddr, id: &str) -> (String, String) {
    send(
        addr,
        &format!("GET /api/runs/{id} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    )
    .await
}

async fn get_events(addr: SocketAddr, id: &str) -> (String, String) {
    send(
        addr,
        &format!("GET /api/runs/{id}/events?cursor=0 HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_shim_runs_to_succeeded_with_stdout_events() {
    ensure_shim_on_path();
    let daemon = RunningDaemon::start(config(temp_paths("ok"))).await.expect("start");
    let addr = daemon.addr();
    let (head, body) =
        post_run(addr, r#"{"agentId":"qwen","prompt":"hi","model":"default"}"#).await;
    assert!(head.contains("202"), "head: {head} body: {body}");
    let run_id: String = serde_json::from_str::<serde_json::Value>(&body)
        .expect("json")
        .get("runId")
        .and_then(|v| v.as_str())
        .expect("runId")
        .to_string();
    let mut status = String::new();
    for _ in 0..100 {
        let (_, body) = get_run(addr, &run_id).await;
        status = serde_json::from_str::<serde_json::Value>(&body)
            .expect("json")
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if matches!(status.as_str(), "succeeded" | "failed" | "canceled") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(status, "succeeded", "run did not succeed");
    let (_, events) = get_events(addr, &run_id).await;
    assert!(events.contains("hello-from-shim"), "events: {events}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_agent_keeps_no_executor_behavior() {
    ensure_shim_on_path();
    let daemon = RunningDaemon::start(config(temp_paths("noop"))).await.expect("start");
    let addr = daemon.addr();
    let (head, body) = post_run(addr, r#"{"agentId":"local-profile-custom","prompt":"hi"}"#).await;
    assert!(head.contains("202"), "head: {head} body: {body}");
    let run_id: String = serde_json::from_str::<serde_json::Value>(&body)
        .expect("json")
        .get("runId")
        .and_then(|v| v.as_str())
        .expect("runId")
        .to_string();
    let (_, body) = get_run(addr, &run_id).await;
    let status = serde_json::from_str::<serde_json::Value>(&body)
        .expect("json")
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert_eq!(status, "running", "body: {body}");
}

async fn post_steer(addr: SocketAddr, id: &str, text: &str) -> (String, String) {
    let body = serde_json::json!({"text": text}).to_string();
    send(
        addr,
        &format!(
            "POST /api/runs/{id}/steer HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

async fn post_cancel(addr: SocketAddr, id: &str) -> (String, String) {
    send(
        addr,
        &format!("POST /api/runs/{id}/cancel HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    )
    .await
}

/// A live child with stdin open accepts steering (delivered + journaled),
/// and cancel kills the child: proves the full spawn → steer → cancel
/// lifecycle against a real process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_child_accepts_steer_and_cancel_kills_it() {
    ensure_shim_on_path();
    let daemon = RunningDaemon::start(config(temp_paths("live"))).await.expect("start");
    let addr = daemon.addr();
    let (head, body) = post_run(addr, r#"{"agentId":"claude","prompt":"hi"}"#).await;
    assert!(head.contains("202"), "head: {head} body: {body}");
    let run_id: String = serde_json::from_str::<serde_json::Value>(&body)
        .expect("json")
        .get("runId")
        .and_then(|v| v.as_str())
        .expect("runId")
        .to_string();
    // The shim sleeps, so the run must still be running here.
    let (_, body) = get_run(addr, &run_id).await;
    let status = serde_json::from_str::<serde_json::Value>(&body)
        .expect("json")
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert_eq!(status, "running", "body: {body}");

    let (head, body) = post_steer(addr, &run_id, "keep going").await;
    assert!(head.contains("200"), "head: {head} body: {body}");
    assert!(body.contains("\"delivered\":true"), "body: {body}");

    let (head, body) = post_cancel(addr, &run_id).await;
    assert!(head.contains("200"), "head: {head} body: {body}");
    assert!(body.contains("\"canceled\""), "body: {body}");

    // The kill lands and the pump finishes the run as canceled.
    let mut status = String::new();
    for _ in 0..100 {
        let (_, body) = get_run(addr, &run_id).await;
        status = serde_json::from_str::<serde_json::Value>(&body)
            .expect("json")
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if status == "canceled" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(status, "canceled", "child kill did not cancel the run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copilot_wire_maps_to_lane_events() {
    ensure_shim_on_path();
    let daemon = RunningDaemon::start(config(temp_paths("copilot"))).await.expect("start");
    let addr = daemon.addr();
    let (head, body) = post_run(addr, r#"{"agentId":"copilot","prompt":"hi"}"#).await;
    assert!(head.contains("202"), "head: {head} body: {body}");
    let run_id: String = serde_json::from_str::<serde_json::Value>(&body)
        .expect("json")
        .get("runId")
        .and_then(|v| v.as_str())
        .expect("runId")
        .to_string();
    let mut status = String::new();
    for _ in 0..100 {
        let (_, body) = get_run(addr, &run_id).await;
        status = serde_json::from_str::<serde_json::Value>(&body)
            .expect("json")
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if matches!(status.as_str(), "succeeded" | "failed" | "canceled") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(status, "succeeded", "run did not succeed");
    let (_, events) = get_events(addr, &run_id).await;
    assert!(events.contains("text_delta"), "events: {events}");
    assert!(events.contains("hey"), "events: {events}");
}
