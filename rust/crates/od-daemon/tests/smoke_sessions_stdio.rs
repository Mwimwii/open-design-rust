//! End-to-end stdio coverage for the connection-test session lanes.
//!
//! The unit tests in `smoke_sessions` drive the state machines over an
//! in-memory duplex pipe. This test spawns a *real* child process that speaks
//! the ACP / dsh-profile / pi-rpc handshake so the same session objects are
//! exercised across a real stdin/stdout boundary — the exact shape
//! `SpawnAgentProbe` uses.
//!
//! The fake agent is this test binary re-entered with `OD_FAKE_AGENT` set: the
//! `fake_agent_child` case then speaks the protocol and exits. That keeps the
//! test dependency-free (no node/python required).

use std::io::{BufRead, Write};

use od_daemon::smoke_sessions::SmokeSession;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Re-entrant fake agent. Runs only when the parent test spawns this binary
/// with `OD_FAKE_AGENT` set.
#[test]
fn fake_agent_child() {
    let Ok(format) = std::env::var("OD_FAKE_AGENT") else {
        return;
    };
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut out = std::io::stdout();
    let mut send = |value: serde_json::Value| {
        writeln!(out, "{value}").expect("write frame");
        out.flush().expect("flush frame");
    };
    match format.as_str() {
        "acp" => {
            for line in lines.by_ref() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                match value.get("method").and_then(|method| method.as_str()) {
                    Some("initialize") => send(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": value["id"],
                        "result": { "agentInfo": { "version": "fake-1.0.0" } }
                    })),
                    Some("session/new") => send(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": value["id"],
                        "result": { "sessionId": "fake-session" }
                    })),
                    Some("session/prompt") => {
                        send(serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": "fake-session",
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": "ok" }
                                }
                            }
                        }));
                        send(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": value["id"],
                            "result": { "stopReason": "end_turn" }
                        }));
                        return;
                    }
                    _ => {}
                }
            }
        }
        "dsh" => {
            send(serde_json::json!({ "v": 1, "type": "ready" }));
            for line in lines.by_ref() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if value.get("type").and_then(|kind| kind.as_str()) != Some("execute") {
                    continue;
                }
                let request_id = value["request_id"].clone();
                send(serde_json::json!({
                    "v": 1, "type": "session", "request_id": request_id, "session_id": "fake-session"
                }));
                send(serde_json::json!({
                    "v": 1, "type": "text", "request_id": value["request_id"], "content": "ok"
                }));
                send(serde_json::json!({
                    "v": 1,
                    "type": "result",
                    "request_id": value["request_id"],
                    "session_id": "fake-session",
                    "status": "completed"
                }));
                return;
            }
        }
        "pi" => {
            for line in lines.by_ref() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if value.get("type").and_then(|kind| kind.as_str()) != Some("prompt") {
                    continue;
                }
                send(serde_json::json!({
                    "type": "message_update",
                    "assistantMessageEvent": { "type": "text_delta", "delta": "ok" }
                }));
                send(serde_json::json!({ "type": "agent_end" }));
                return;
            }
        }
        other => panic!("unknown OD_FAKE_AGENT {other}"),
    }
}

async fn drive(format: &str, fake: &str) {
    let mut child = tokio::process::Command::new(std::env::current_exe().expect("test exe"))
        .arg("--exact")
        .arg("fake_agent_child")
        .arg("--nocapture")
        .env("OD_FAKE_AGENT", fake)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn fake agent");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut session = SmokeSession::start(format, Box::new(stdin), "/tmp", "Reply with only: ok")
        .await
        .expect("session lane");

    let mut lines = BufReader::new(stdout).lines();
    let mut text = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let update = session.feed_line(&line).await;
        text.push_str(&update.text);
        if update.completed || update.fatal {
            break;
        }
    }
    let _ = child.kill().await;
    assert_eq!(text, "ok", "{format} assistant text");
    assert!(session.completed_successfully(), "{format} completed");
    assert!(!session.has_fatal_error(), "{format} no fatal error");
}

#[tokio::test]
async fn acp_session_completes_over_a_real_pipe() {
    drive("acp-json-rpc", "acp").await;
}

#[tokio::test]
async fn dsh_profile_session_completes_over_a_real_pipe() {
    drive("dsh-profile-jsonl", "dsh").await;
}

#[tokio::test]
async fn pi_rpc_session_completes_over_a_real_pipe() {
    drive("pi-rpc", "pi").await;
}
