//! Connection-test session lanes (parity: the ACP / dsh-profile / pi-rpc
//! session controllers under `apps/daemon/src/agent-protocol/`).
//!
//! The chat-run path drives the full session controllers (tool execution,
//! artifacts, resume, usage). A connection test only needs the smoke prompt:
//! perform the protocol handshake, ask for a reply, and report whether the
//! prompt completed cleanly or the session failed. That subset is what lives
//! here, so `SpawnAgentProbe` can feed `acpCleanCompletion` / `acpFatal` (and
//! the dsh/pi terminal signals) into the same decision tree the TS daemon uses
//! instead of treating every JSON line as raw stdout.
//!
//! Deliberate gaps, all benign for a connectivity check:
//! * no model selection (`session/set_model` / `configOptions`) — the agent
//!   runs its configured default model; the smoke prompt does not care which;
//! * no MCP servers, tool execution, session resume, image forwarding, or
//!   artifact extraction;
//! * server→client requests other than `session/request_permission` are
//!   answered with JSON-RPC "method not found" so the agent cannot hang
//!   waiting on the daemon during a smoke test.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// Parity: `ACP_PROTOCOL_VERSION`.
pub const ACP_PROTOCOL_VERSION: i64 = 1;

/// Writes to the agent's stdin. Boxed so tests can drive a duplex pipe.
pub type SessionWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// What one stdout line produced.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SmokeSessionUpdate {
    /// Assistant text recovered from this frame.
    pub text: String,
    /// A protocol/transport error the decision tree must surface.
    pub error: Option<String>,
    /// Parity: `acpSession.hasFatalError()`.
    pub fatal: bool,
    /// Parity: `acpSession.completedSuccessfully()`.
    pub completed: bool,
}

impl SmokeSessionUpdate {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    fn fail(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            error: Some(message),
            fatal: true,
            ..Default::default()
        }
    }
}

pub enum SmokeSession {
    Acp(AcpSmoke),
    Dsh(DshSmoke),
    Pi(PiSmoke),
}

impl SmokeSession {
    /// `None` when `stream_format` has no session lane.
    pub async fn start(
        stream_format: &str,
        writer: SessionWriter,
        cwd: &str,
        prompt: &str,
    ) -> Option<Self> {
        match stream_format {
            "acp-json-rpc" => Some(Self::Acp(AcpSmoke::start(writer, cwd, prompt).await)),
            "dsh-profile-jsonl" => Some(Self::Dsh(DshSmoke::start(writer, cwd, prompt))),
            "pi-rpc" => Some(Self::Pi(PiSmoke::start(writer, prompt).await)),
            _ => None,
        }
    }

    pub async fn feed_line(&mut self, line: &str) -> SmokeSessionUpdate {
        match self {
            Self::Acp(session) => session.feed_line(line).await,
            Self::Dsh(session) => session.feed_line(line).await,
            Self::Pi(session) => session.feed_line(line).await,
        }
    }

    pub fn completed_successfully(&self) -> bool {
        match self {
            Self::Acp(session) => session.completed,
            Self::Dsh(session) => session.completed,
            Self::Pi(session) => session.completed,
        }
    }

    pub fn has_fatal_error(&self) -> bool {
        match self {
            Self::Acp(session) => session.fatal,
            Self::Dsh(session) => session.fatal,
            Self::Pi(session) => session.fatal,
        }
    }
}

async fn write_json(writer: &mut SessionWriter, value: Value) {
    let mut frame = value.to_string();
    frame.push('\n');
    let _ = writer.write_all(frame.as_bytes()).await;
    let _ = writer.flush().await;
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ACP (JSON-RPC 2.0 over stdio)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpStage {
    Initialize,
    SessionNew,
    Prompt,
    Done,
}

pub struct AcpSmoke {
    writer: SessionWriter,
    cwd: String,
    prompt: String,
    stage: AcpStage,
    session_id: Option<String>,
    prompt_id: Option<u64>,
    completed: bool,
    fatal: bool,
}

impl AcpSmoke {
    async fn start(writer: SessionWriter, cwd: &str, prompt: &str) -> Self {
        let mut session = Self {
            writer,
            cwd: cwd.to_string(),
            prompt: prompt.to_string(),
            stage: AcpStage::Initialize,
            session_id: None,
            prompt_id: None,
            completed: false,
            fatal: false,
        };
        write_json(
            &mut session.writer,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "clientCapabilities": { "terminal": false },
                    "clientInfo": { "name": "open-design", "version": "connection-test" },
                }
            }),
        )
        .await;
        session
    }

    async fn feed_line(&mut self, line: &str) -> SmokeSessionUpdate {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return SmokeSessionUpdate::default();
        };
        let Some(object) = value.as_object() else {
            return SmokeSessionUpdate::default();
        };
        let id = object.get("id").and_then(Value::as_u64);

        if let Some(method) = object.get("method").and_then(Value::as_str) {
            if let Some(id) = id {
                // Server→client request: keep the agent unblocked.
                if method == "session/request_permission" {
                    write_json(
                        &mut self.writer,
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "outcome": { "outcome": "cancelled" } }
                        }),
                    )
                    .await;
                } else {
                    write_json(
                        &mut self.writer,
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": "Method not found" }
                        }),
                    )
                    .await;
                }
                return SmokeSessionUpdate::default();
            }
            if method == "session/update" {
                return acp_text_from_update(object.get("params"));
            }
            return SmokeSessionUpdate::default();
        }

        let Some(id) = id else {
            return SmokeSessionUpdate::default();
        };
        if let Some(error) = object.get("error") {
            self.fatal = true;
            self.stage = AcpStage::Done;
            return SmokeSessionUpdate::fail(acp_error_message(error));
        }
        let result = object.get("result");
        match self.stage {
            AcpStage::Initialize if id == 1 => {
                self.stage = AcpStage::SessionNew;
                write_json(
                    &mut self.writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "session/new",
                        "params": { "cwd": self.cwd, "mcpServers": [] }
                    }),
                )
                .await;
                SmokeSessionUpdate::default()
            }
            AcpStage::SessionNew if id == 2 => {
                let session_id = result
                    .and_then(|value| value.get("sessionId"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let Some(session_id) = session_id else {
                    self.fatal = true;
                    self.stage = AcpStage::Done;
                    return SmokeSessionUpdate::fail("invalid session/new response");
                };
                self.session_id = Some(session_id.clone());
                self.stage = AcpStage::Prompt;
                self.prompt_id = Some(3);
                write_json(
                    &mut self.writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 3,
                        "method": "session/prompt",
                        "params": {
                            "sessionId": session_id,
                            "prompt": [{ "type": "text", "text": self.prompt }]
                        }
                    }),
                )
                .await;
                SmokeSessionUpdate::default()
            }
            AcpStage::Prompt if Some(id) == self.prompt_id => {
                self.completed = true;
                self.stage = AcpStage::Done;
                SmokeSessionUpdate {
                    completed: true,
                    ..Default::default()
                }
            }
            _ => SmokeSessionUpdate::default(),
        }
    }
}

fn acp_error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or("ACP session error")
        .to_string()
}

/// `session/update` → assistant text. Only `agent_message_chunk` text blocks
/// are assistant-visible; thoughts, tool calls, and plans are ignored.
fn acp_text_from_update(params: Option<&Value>) -> SmokeSessionUpdate {
    let Some(update) = params.and_then(|params| params.get("update")) else {
        return SmokeSessionUpdate::default();
    };
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("agent_message_chunk") {
        return SmokeSessionUpdate::default();
    }
    let Some(content) = update.get("content") else {
        return SmokeSessionUpdate::default();
    };
    if content.get("type").and_then(Value::as_str) != Some("text") {
        return SmokeSessionUpdate::default();
    }
    match content.get("text").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => SmokeSessionUpdate::text(text),
        _ => SmokeSessionUpdate::default(),
    }
}

// ---------------------------------------------------------------------------
// dsh profile (`dsh --profile open-design --stdio`, JSONL frames)
// ---------------------------------------------------------------------------

pub struct DshSmoke {
    writer: SessionWriter,
    request_id: String,
    cwd: String,
    prompt: String,
    ready: bool,
    session_id: Option<String>,
    completed: bool,
    fatal: bool,
}

impl DshSmoke {
    fn start(writer: SessionWriter, cwd: &str, prompt: &str) -> Self {
        Self {
            writer,
            request_id: format!("connection-test-{}", now_millis()),
            cwd: cwd.to_string(),
            prompt: prompt.to_string(),
            ready: false,
            session_id: None,
            completed: false,
            fatal: false,
        }
    }

    async fn feed_line(&mut self, line: &str) -> SmokeSessionUpdate {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return SmokeSessionUpdate::default();
        };
        let Some(frame) = value.as_object() else {
            return SmokeSessionUpdate::default();
        };
        let frame_type = frame.get("type").and_then(Value::as_str).unwrap_or("");
        if !self.ready {
            if frame_type != "ready" {
                self.fatal = true;
                return SmokeSessionUpdate::fail(
                    "DeepSeek Harness profile did not begin with a ready frame.",
                );
            }
            self.ready = true;
            write_json(
                &mut self.writer,
                json!({
                    "v": 1,
                    "type": "execute",
                    "request_id": self.request_id,
                    "cwd": self.cwd,
                    "prompt": self.prompt,
                    "mcp_servers": []
                }),
            )
            .await;
            return SmokeSessionUpdate::default();
        }
        if frame_type == "ready" || frame_type == "probe" {
            self.fatal = true;
            return SmokeSessionUpdate::fail(
                "DeepSeek Harness profile emitted an unexpected identity frame.",
            );
        }
        if let Some(request_id) = frame.get("request_id").and_then(Value::as_str) {
            if request_id != self.request_id {
                return SmokeSessionUpdate::default();
            }
        }
        match frame_type {
            "session" => {
                if self.session_id.is_some() {
                    self.fatal = true;
                    return SmokeSessionUpdate::fail(
                        "DeepSeek Harness profile emitted more than one session frame.",
                    );
                }
                self.session_id = frame
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                SmokeSessionUpdate::default()
            }
            "text" => match frame.get("content").and_then(Value::as_str) {
                Some(content) if !content.is_empty() => SmokeSessionUpdate::text(content),
                _ => SmokeSessionUpdate::default(),
            },
            "protocol_error" => {
                self.fatal = true;
                let code = frame
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("DSH_PROFILE_PROTOCOL_ERROR");
                SmokeSessionUpdate::fail(format!(
                    "DeepSeek Harness profile reported a protocol error ({code})."
                ))
            }
            "result" => {
                if self.session_id.is_none() && frame.get("status").and_then(Value::as_str) == Some("failed") {
                    self.fatal = true;
                    return SmokeSessionUpdate::fail(dsh_error_message(frame));
                }
                if self.session_id.is_none() {
                    self.fatal = true;
                    return SmokeSessionUpdate::fail(
                        "DeepSeek Harness profile emitted run content before establishing a session.",
                    );
                }
                match frame.get("status").and_then(Value::as_str) {
                    Some("completed") => {
                        self.completed = true;
                        SmokeSessionUpdate {
                            completed: true,
                            ..Default::default()
                        }
                    }
                    Some("cancelled") => {
                        self.fatal = true;
                        SmokeSessionUpdate::fail(
                            "DeepSeek Harness profile cancelled a run that OpenDesign did not cancel.",
                        )
                    }
                    _ => {
                        self.fatal = true;
                        SmokeSessionUpdate::fail(dsh_error_message(frame))
                    }
                }
            }
            _ => SmokeSessionUpdate::default(),
        }
    }
}

fn dsh_error_message(frame: &serde_json::Map<String, Value>) -> String {
    frame
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or("DeepSeek Harness profile run failed.")
        .to_string()
}

// ---------------------------------------------------------------------------
// pi RPC (`pi --mode rpc`, JSONL commands + events)
// ---------------------------------------------------------------------------

pub struct PiSmoke {
    writer: SessionWriter,
    completed: bool,
    fatal: bool,
}

impl PiSmoke {
    async fn start(mut writer: SessionWriter, prompt: &str) -> Self {
        write_json(
            &mut writer,
            json!({ "id": 1, "type": "prompt", "message": prompt }),
        )
        .await;
        Self {
            writer,
            completed: false,
            fatal: false,
        }
    }

    async fn feed_line(&mut self, line: &str) -> SmokeSessionUpdate {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return SmokeSessionUpdate::default();
        };
        let Some(event) = value.as_object() else {
            return SmokeSessionUpdate::default();
        };
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "extension_ui_request" => {
                self.reply_extension_ui(event).await;
                SmokeSessionUpdate::default()
            }
            "agent_end" => {
                self.completed = true;
                SmokeSessionUpdate {
                    completed: true,
                    ..Default::default()
                }
            }
            "message_update" => {
                let Some(delta) = event.get("assistantMessageEvent") else {
                    return SmokeSessionUpdate::default();
                };
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => delta
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(SmokeSessionUpdate::text)
                        .unwrap_or_default(),
                    "error" => {
                        self.fatal = true;
                        let message = delta
                            .get("reason")
                            .and_then(Value::as_str)
                            .or_else(|| delta.get("delta").and_then(Value::as_str))
                            .unwrap_or("Agent error");
                        SmokeSessionUpdate::fail(message)
                    }
                    _ => SmokeSessionUpdate::default(),
                }
            }
            "turn_end" => {
                let stop_reason = event
                    .get("message")
                    .and_then(|message| message.get("stopReason"))
                    .and_then(Value::as_str);
                if stop_reason == Some("error") {
                    self.fatal = true;
                    let message = event
                        .get("message")
                        .and_then(|message| message.get("errorMessage"))
                        .and_then(Value::as_str)
                        .filter(|message| !message.is_empty())
                        .unwrap_or("Pi agent error");
                    return SmokeSessionUpdate::fail(message);
                }
                SmokeSessionUpdate::default()
            }
            _ => SmokeSessionUpdate::default(),
        }
    }

    /// Parity: `replyExtensionUi` — fire-and-forget methods are consumed
    /// silently; dialogs auto-resolve so pi cannot block on a user.
    async fn reply_extension_ui(&mut self, request: &serde_json::Map<String, Value>) {
        let Some(id) = request.get("id").cloned() else {
            return;
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        if matches!(
            method,
            "setStatus" | "setWidget" | "notify" | "setTitle" | "set_editor_text"
        ) {
            return;
        }
        let result = if method == "confirm" {
            json!({ "confirmed": true })
        } else {
            let options = request
                .get("params")
                .and_then(|params| params.get("options"))
                .or_else(|| request.get("options"))
                .and_then(Value::as_array);
            match options.and_then(|options| options.first()) {
                Some(Value::String(value)) => json!({ "value": value }),
                Some(Value::Object(object)) => json!({
                    "value": object
                        .get("label")
                        .or_else(|| object.get("value"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                }),
                _ => json!({ "cancelled": true }),
            }
        };
        let mut frame = json!({ "type": "extension_ui_response", "id": id });
        if let (Some(frame), Some(result)) = (frame.as_object_mut(), result.as_object()) {
            for (key, value) in result {
                frame.insert(key.clone(), value.clone());
            }
        }
        write_json(&mut self.writer, frame).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};

    fn session_pipe() -> (SessionWriter, BufReader<DuplexStream>) {
        let (writer, reader) = tokio::io::duplex(64 * 1024);
        (Box::new(writer), BufReader::new(reader))
    }

    async fn next_frame(reader: &mut BufReader<DuplexStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read frame");
        serde_json::from_str(&line).expect("json frame")
    }

    #[tokio::test]
    async fn acp_runs_initialize_session_new_prompt_and_reports_completion() {
        let (writer, mut reader) = session_pipe();
        let mut session = SmokeSession::start("acp-json-rpc", writer, "/tmp/work", "Reply with only: ok")
            .await
            .expect("session");

        let initialize = next_frame(&mut reader).await;
        assert_eq!(initialize["method"], json!("initialize"));
        assert_eq!(initialize["params"]["protocolVersion"], json!(1));
        assert_eq!(initialize["params"]["clientCapabilities"]["terminal"], json!(false));

        let update = session
            .feed_line(r#"{"jsonrpc":"2.0","id":1,"result":{"agentInfo":{"version":"1.0.0"}}}"#)
            .await;
        assert_eq!(update, SmokeSessionUpdate::default());

        let session_new = next_frame(&mut reader).await;
        assert_eq!(session_new["method"], json!("session/new"));
        assert_eq!(session_new["params"]["cwd"], json!("/tmp/work"));
        assert_eq!(session_new["params"]["mcpServers"], json!([]));

        session
            .feed_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}"#)
            .await;
        let prompt = next_frame(&mut reader).await;
        assert_eq!(prompt["method"], json!("session/prompt"));
        assert_eq!(prompt["params"]["sessionId"], json!("s-1"));
        assert_eq!(
            prompt["params"]["prompt"],
            json!([{ "type": "text", "text": "Reply with only: ok" }])
        );

        let text = session
            .feed_line(
                r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}}}"#,
            )
            .await;
        assert_eq!(text.text, "ok");
        assert!(!text.completed);

        let done = session
            .feed_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
            .await;
        assert!(done.completed);
        assert!(session.completed_successfully());
        assert!(!session.has_fatal_error());
    }

    #[tokio::test]
    async fn acp_answers_server_requests_and_surfaces_errors() {
        let (writer, mut reader) = session_pipe();
        let mut session = SmokeSession::start("acp-json-rpc", writer, "/tmp/work", "hi")
            .await
            .expect("session");
        let _ = next_frame(&mut reader).await;

        session
            .feed_line(
                r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{}}"#,
            )
            .await;
        let reply = next_frame(&mut reader).await;
        assert_eq!(reply["id"], json!(7));
        assert_eq!(reply["result"]["outcome"]["outcome"], json!("cancelled"));

        session
            .feed_line(r#"{"jsonrpc":"2.0","id":9,"method":"fs/read_text_file","params":{}}"#)
            .await;
        let reply = next_frame(&mut reader).await;
        assert_eq!(reply["error"]["code"], json!(-32601));

        let failure = session
            .feed_line(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#)
            .await;
        assert_eq!(failure.error.as_deref(), Some("boom"));
        assert!(failure.fatal);
        assert!(session.has_fatal_error());
        assert!(!session.completed_successfully());
    }

    #[tokio::test]
    async fn dsh_executes_after_ready_and_reports_the_terminal_result() {
        let (writer, mut reader) = session_pipe();
        let mut session = SmokeSession::start("dsh-profile-jsonl", writer, "/tmp/work", "Reply with only: ok")
            .await
            .expect("session");

        session.feed_line(r#"{"v":1,"type":"ready"}"#).await;
        let execute = next_frame(&mut reader).await;
        assert_eq!(execute["type"], json!("execute"));
        assert_eq!(execute["prompt"], json!("Reply with only: ok"));
        assert_eq!(execute["mcp_servers"], json!([]));
        let request_id = execute["request_id"].as_str().expect("request id").to_string();

        session
            .feed_line(&format!(
                r#"{{"v":1,"type":"session","request_id":"{request_id}","session_id":"d-1"}}"#
            ))
            .await;
        let text = session
            .feed_line(&format!(
                r#"{{"v":1,"type":"text","request_id":"{request_id}","content":"ok"}}"#
            ))
            .await;
        assert_eq!(text.text, "ok");

        let done = session
            .feed_line(&format!(
                r#"{{"v":1,"type":"result","request_id":"{request_id}","session_id":"d-1","status":"completed"}}"#
            ))
            .await;
        assert!(done.completed);
        assert!(session.completed_successfully());
    }

    #[tokio::test]
    async fn dsh_rejects_a_failed_result_and_foreign_requests() {
        let (writer, mut reader) = session_pipe();
        let mut session = SmokeSession::start("dsh-profile-jsonl", writer, "/tmp/work", "hi")
            .await
            .expect("session");
        session.feed_line(r#"{"v":1,"type":"ready"}"#).await;
        let execute = next_frame(&mut reader).await;
        let request_id = execute["request_id"].as_str().expect("request id").to_string();

        let ignored = session
            .feed_line(r#"{"v":1,"type":"text","request_id":"other","content":"not mine"}"#)
            .await;
        assert_eq!(ignored, SmokeSessionUpdate::default());

        session
            .feed_line(&format!(
                r#"{{"v":1,"type":"session","request_id":"{request_id}","session_id":"d-1"}}"#
            ))
            .await;
        let failed = session
            .feed_line(&format!(
                r#"{{"v":1,"type":"result","request_id":"{request_id}","session_id":"d-1","status":"failed","error":{{"message":"bad key","code":"AUTH"}}}}"#
            ))
            .await;
        assert_eq!(failed.error.as_deref(), Some("bad key"));
        assert!(failed.fatal);
        assert!(session.has_fatal_error());
    }

    #[tokio::test]
    async fn pi_sends_the_prompt_and_completes_on_agent_end() {
        let (writer, mut reader) = session_pipe();
        let mut session = SmokeSession::start("pi-rpc", writer, "/tmp/work", "Reply with only: ok")
            .await
            .expect("session");
        let prompt = next_frame(&mut reader).await;
        assert_eq!(prompt["type"], json!("prompt"));
        assert_eq!(prompt["message"], json!("Reply with only: ok"));

        let text = session
            .feed_line(
                r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"ok"}}"#,
            )
            .await;
        assert_eq!(text.text, "ok");

        let done = session.feed_line(r#"{"type":"agent_end"}"#).await;
        assert!(done.completed);
        assert!(session.completed_successfully());
    }

    #[tokio::test]
    async fn pi_resolves_extension_ui_and_reports_error_deltas() {
        let (writer, mut reader) = session_pipe();
        let mut session = SmokeSession::start("pi-rpc", writer, "/tmp/work", "hi")
            .await
            .expect("session");
        let _ = next_frame(&mut reader).await;

        session
            .feed_line(r#"{"type":"extension_ui_request","id":4,"method":"setStatus"}"#)
            .await;
        session
            .feed_line(
                r#"{"type":"extension_ui_request","id":5,"method":"confirm","params":{"options":["yes"]}}"#,
            )
            .await;
        let reply = next_frame(&mut reader).await;
        assert_eq!(reply["id"], json!(5));
        assert_eq!(reply["confirmed"], json!(true));

        let failure = session
            .feed_line(
                r#"{"type":"message_update","assistantMessageEvent":{"type":"error","reason":"rate limited"}}"#,
            )
            .await;
        assert_eq!(failure.error.as_deref(), Some("rate limited"));
        assert!(session.has_fatal_error());
    }
}
