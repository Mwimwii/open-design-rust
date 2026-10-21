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
    // Emits one plain-lane artifact tag, then exits: proves the finalizer
    // persists stdout artifacts into the project with their manifest.
    write_shim(
        &dir,
        "aider",
        concat!(
            "#!/bin/sh\n",
            "case \"$1\" in --version) echo 'aider 0.0.0'; exit 0;; esac\n",
            "printf '%s\\n' '<artifact identifier=\"hero\" type=\"text/html\" title=\"Hero\">'\n",
            "printf '%s\\n' '<html><body>artifact-body</body></html>'\n",
            "printf '%s\\n' '</artifact>'\n",
            "exit 0\n",
        ),
    );
    // Emits a fabricated role marker, then stays alive so the guard's kill
    // lands: proves the run-scoped role-marker guard terminates the child.
    write_shim(
        &dir,
        "deepseek",
        "#!/bin/sh\nprintf 'hello\\n## user\\nleak\\n'\nsleep 30\n",
    );
    // Emits four identical failing Edit calls on the Claude stream-json lane:
    // proves the tool-loop guard warns on a repeated failure signature.
    write_shim(
        &dir,
        "amp",
        r#"#!/bin/sh
cat > /dev/null
echo '{"type":"system","subtype":"init","model":"m","session_id":"s1"}'
i=1
while [ $i -le 4 ]; do
  printf '{"type":"assistant","message":{"id":"m%s","content":[{"type":"tool_use","id":"t%s","name":"Edit","input":{"file_path":"a.ts","old_string":"x"}}]}}\n' "$i" "$i"
  printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t%s","is_error":true,"content":"no match"}]}}\n' "$i"
  i=$((i+1))
done
echo '{"type":"result","subtype":"success","stop_reason":"end_turn"}'
exit 0
"#,
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
    // Emits one OpenCode JSONL turn, then exits: proves the multi-kind
    // `json-event-stream` lane maps to parsed events, not raw stdout.
    write_shim(
        &dir,
        "opencode",
        concat!(
            "#!/bin/sh\n",
            "cat > /dev/null\n",
            "echo '{\"type\":\"step_start\",\"sessionID\":\"ses-1\",\"part\":{\"type\":\"step-start\"}}'\n",
            "echo '{\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"hey\"}}'\n",
            "echo '{\"type\":\"step_finish\",\"part\":{\"tokens\":{\"input\":11,\"output\":7},\"cost\":0}}'\n",
            "exit 0\n",
        ),
    );
    // Emits one agy JSONL turn, then exits: proves the `antigravity` kind is
    // dispatched by the pump rather than falling through to the plain lane.
    // (`agy` is the Antigravity bin name.)
    write_shim(
        &dir,
        "agy",
        concat!(
            "#!/bin/sh\n",
            "cat > /dev/null\n",
            "echo '{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"agent_response\",\"text_delta\":\"hey\"}}'\n",
            "echo '{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"thinking_tokens\":1,\"cache_read_tokens\":0},\"duration_seconds\":0.5}}'\n",
            "exit 0\n",
        ),
    );
    // Emits one `codex exec --json` turn, then exits: proves the `codex` kind
    // reaches the parsed lane.
    write_shim(
        &dir,
        "codex",
        concat!(
            "#!/bin/sh\n",
            "cat > /dev/null\n",
            "echo '{\"type\":\"thread.started\",\"thread_id\":\"thr-1\"}'\n",
            "echo '{\"type\":\"item.completed\",\"item\":{\"id\":\"item-1\",\"type\":\"agent_message\",\"text\":\"hey\"}}'\n",
            "echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}'\n",
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
    // Parity: the plain lane's stdout payload is `{ chunk }`
    // (packages/contracts `ChatSseChunkPayload`). The web client reads
    // `event.data.chunk`, so naming it `text` silently drops every plain chunk.
    assert!(
        events.contains("\"chunk\""),
        "stdout payload lacks the contract's chunk field: {events}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_lane_persists_stdout_artifacts_into_the_project() {
    ensure_shim_on_path();
    let paths = temp_paths("artifacts");
    let data_dir = paths.data_dir().to_path_buf();
    let daemon = RunningDaemon::start(config(paths)).await.expect("start");
    daemon
        .state
        .store
        .execute(
            "INSERT INTO projects (id, name, created_at, updated_at) VALUES ('p1', 'Demo', 1, 2)",
            (),
        )
        .expect("insert p1");
    // Parity: `ensureProject` creates the managed project directory before a
    // run can spawn with it as cwd.
    std::fs::create_dir_all(data_dir.join("projects").join("p1")).expect("project dir");
    let addr = daemon.addr();
    let (head, body) = post_run(
        addr,
        r#"{"agentId":"aider","prompt":"hi","model":"default","projectId":"p1"}"#,
    )
    .await;
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
    let (_, final_body) = get_run(addr, &run_id).await;
    assert_eq!(status, "succeeded", "run did not succeed: {final_body}");

    let project_dir = data_dir.join("projects").join("p1");
    let artifact = project_dir.join("hero.html");
    assert!(
        artifact.exists(),
        "artifact missing at {}: {:?}",
        artifact.display(),
        std::fs::read_dir(&project_dir).map(|entries| entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>())
    );
    assert!(std::fs::read_to_string(&artifact)
        .expect("artifact body")
        .contains("artifact-body"));
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project_dir.join("hero.html.artifact.json")).expect("manifest"),
    )
    .expect("manifest json");
    assert_eq!(manifest["version"], 1);
    assert_eq!(manifest["kind"], "html");
    assert_eq!(manifest["entry"], "hero.html");

    let (_, events) = get_events(addr, &run_id).await;
    assert!(
        events.contains(r#""type":"artifact""#),
        "missing artifact frame: {events}"
    );
    assert!(
        events.contains("plain_stream_artifacts_persisted"),
        "missing persistence diagnostic: {events}"
    );
}

/// Poll `/api/runs/:id` until the run is terminal, returning its status and
/// the last body.
async fn wait_terminal(addr: SocketAddr, run_id: &str) -> (String, String) {
    let mut status = String::new();
    let mut body = String::new();
    for _ in 0..100 {
        let (_, current) = get_run(addr, run_id).await;
        body = current;
        status = serde_json::from_str::<serde_json::Value>(&body)
            .expect("json")
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        if matches!(status.as_str(), "succeeded" | "failed" | "canceled") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    (status, body)
}

fn run_id_of(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .expect("json")
        .get("runId")
        .and_then(|value| value.as_str())
        .expect("runId")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_marker_guard_terminates_a_plain_run() {
    ensure_shim_on_path();
    let daemon = RunningDaemon::start(config(temp_paths("role-marker")))
        .await
        .expect("start");
    let addr = daemon.addr();
    let (head, body) = post_run(addr, r#"{"agentId":"deepseek","prompt":"hi"}"#).await;
    assert!(head.contains("202"), "head: {head} body: {body}");
    let run_id = run_id_of(&body);
    let (status, final_body) = wait_terminal(addr, &run_id).await;
    assert_eq!(status, "failed", "run should be terminated: {final_body}");

    let (_, events) = get_events(addr, &run_id).await;
    assert!(
        events.contains(r#""type":"fabricated_role_marker""#),
        "missing warning event: {events}"
    );
    assert!(
        events.contains("ROLE_MARKER_HALLUCINATION"),
        "missing abort error: {events}"
    );
    assert!(
        !events.contains(r###""chunk":"## user"###),
        "the marker leaked to the renderer: {events}"
    );
    assert!(
        events.contains(r#""chunk":"hello"#),
        "safe text before the marker should still stream: {events}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_loop_guard_warns_on_a_repeated_failing_call() {
    ensure_shim_on_path();
    let daemon = RunningDaemon::start(config(temp_paths("tool-loop")))
        .await
        .expect("start");
    let addr = daemon.addr();
    let (head, body) = post_run(addr, r#"{"agentId":"amp","prompt":"hi"}"#).await;
    assert!(head.contains("202"), "head: {head} body: {body}");
    let run_id = run_id_of(&body);
    let (status, final_body) = wait_terminal(addr, &run_id).await;
    assert_eq!(status, "succeeded", "warn mode must not halt: {final_body}");

    let (_, events) = get_events(addr, &run_id).await;
    assert!(
        events.contains(r#""type":"tool_loop""#),
        "missing tool_loop event: {events}"
    );
    assert!(
        events.contains(r#""reason":"repeated-failure""#),
        "expected the repeat trigger: {events}"
    );
    assert!(
        events.contains(r#""action":"warn""#),
        "expected a warn verdict: {events}"
    );
    assert!(
        events.contains(r#""count":4"#),
        "expected four repeats: {events}"
    );
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
    // Parity: parser events ride the `agent` SSE lane with their `type` in the
    // payload (apps/web switches on `event.event === 'agent'`). Naming the frame
    // after the type (`event: text_delta`) is dropped by the renderer.
    assert!(
        events.contains("event: agent"),
        "lane events must use the agent SSE name: {events}"
    );
}

/// The `json-event-stream` lane must parse an OpenCode run into UI-lane events
/// instead of streaming its stdout verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_jsonl_maps_to_lane_events() {
    ensure_shim_on_path();
    let daemon = RunningDaemon::start(config(temp_paths("opencode")))
        .await
        .expect("start");
    let addr = daemon.addr();
    let (head, body) = post_run(addr, r#"{"agentId":"opencode","prompt":"hi"}"#).await;
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
    assert!(events.contains("event: agent"), "events: {events}");
    assert!(events.contains("text_delta"), "events: {events}");
    assert!(events.contains("hey"), "events: {events}");
    assert!(events.contains("\"usage\""), "events: {events}");
    assert!(
        !events.contains("event: stdout"),
        "an opencode run must not fall through to the plain lane: {events}"
    );
}

/// The agy (`antigravity`) kind of the same `json-event-stream` lane must map
/// to parsed events too: the pump dispatches on the resolved kind, not on a
/// fixed runtime name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn antigravity_jsonl_maps_to_lane_events() {
    ensure_shim_on_path();
    let daemon = RunningDaemon::start(config(temp_paths("antigravity")))
        .await
        .expect("start");
    let addr = daemon.addr();
    let (head, body) = post_run(addr, r#"{"agentId":"antigravity","prompt":"hi"}"#).await;
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
    assert!(events.contains("event: agent"), "events: {events}");
    assert!(events.contains("text_delta"), "events: {events}");
    assert!(events.contains("hey"), "events: {events}");
    assert!(events.contains("\"usage\""), "events: {events}");
    assert!(
        !events.contains("event: stdout"),
        "an antigravity run must not fall through to the plain lane: {events}"
    );
}

/// The `codex` kind of the same lane must map to parsed events too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_jsonl_maps_to_lane_events() {
    ensure_shim_on_path();
    let daemon = RunningDaemon::start(config(temp_paths("codex-jsonl")))
        .await
        .expect("start");
    let addr = daemon.addr();
    let (head, body) = post_run(addr, r#"{"agentId":"codex","prompt":"hi"}"#).await;
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
    assert!(events.contains("event: agent"), "events: {events}");
    assert!(events.contains("\"sessionId\":\"thr-1\""), "events: {events}");
    assert!(events.contains("hey"), "events: {events}");
    assert!(events.contains("\"usage\""), "events: {events}");
    assert!(
        !events.contains("event: stdout"),
        "a codex run must not fall through to the plain lane: {events}"
    );
}
