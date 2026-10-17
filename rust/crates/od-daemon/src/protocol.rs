//! Runtime stream protocol parsers — Rust parity port of the per-agent
//! stream handlers (`apps/daemon/src/copilot-stream.ts`, and later the
//! `runtimes/*-stream.ts` family).
//!
//! Parsers turn child stdout lines into UI-lane events. Unparseable lines
//! become bounded `raw` events (parity: `boundedRawAgentEvent` in
//! `runtimes/run-event-payload-budget.ts`, simplified: byte-budget
//! truncation ladder without the cursor-field omission pass).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

/// Parity: `RUN_EVENT_JSON_BUDGET_BYTES` / `RUN_EVENT_STRING_PAYLOAD_BUDGET_BYTES`.
pub const RAW_LINE_BUDGET_BYTES: usize = 64 * 1024;
const STRING_LADDER_TOP_BYTES: usize = 16 * 1024;
const STRING_LADDER_MIN_BYTES: usize = 256;

/// UI-lane events all parsers can emit (parity: the `type` vocabulary the
/// chat panel renders: `status`, `thinking_delta`, `text_delta`,
/// `tool_use`, `tool_result`, `usage`, `raw`).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Status {
        label: String,
        /// `None` omits the key; `Some(Value::Null)` emits an explicit null.
        /// TS is not uniform here: Claude's init writes `model: obj.model ??
        /// null` (key always present), while the Copilot/Qoder init statuses
        /// write the key only when the wire carried it.
        model: Option<Value>,
        /// Parity: the `sessionId` field on the TS `status` event.
        session_id: Option<Value>,
        /// Parity: the `qodercliVersion` field on the TS `status` event (only
        /// the Qoder parser sets it).
        qodercli_version: Option<Value>,
        /// Parity: the `ttftMs` field Claude's `streaming` status carries.
        ttft_ms: Option<i64>,
    },
    ThinkingDelta { delta: String },
    TextDelta { delta: String },
    ToolUse { id: Option<String>, name: Option<String>, input: Value },
    ToolResult { tool_use_id: Option<String>, content: String, is_error: bool },
    Usage {
        usage: Value,
        stop_reason: String,
        duration_ms: Option<i64>,
        /// Top-level `costUsd` (parity: `packages/contracts` `usage` event and
        /// the TS parsers that emit it beside `usage`).
        cost_usd: Option<Value>,
        /// Qoder's `modelUsage` map, passed through unchanged.
        model_usage: Option<Value>,
        /// Set only when the run finished in error, so the renderer can tell a
        /// failed usage frame from a clean one.
        is_error: Option<bool>,
    },
    Raw { line: String, truncated_original_bytes: Option<usize> },
    TurnEnd { stop_reason: String },
    ThinkingStart,
    ThinkingTokens { tokens: f64 },
    ToolInputDelta { id: String, name: String, delta: String },
    RequestUsage { request_id: String, usage: Value },
    Error { message: String, code: Option<String>, raw: Option<String> },
    /// OpenCode's local-plugin in-flight tool frame (parity:
    /// `tool_in_flight` in `runtimes/opencode-tool-events.ts`). Distinct from
    /// [`StreamEvent::ToolUse`], which is the settled native frame.
    ToolInFlight {
        id: String,
        name: String,
        input: Value,
        started_at: i64,
    },
    /// A UI-lane frame the parser reproduces verbatim from its TS source.
    ///
    /// `json-event-stream.ts` dispatches over several runtimes whose frames
    /// have per-kind key sets: an OpenCode `usage` frame carries `costUsd` but
    /// no `stopReason`/`durationMs`, while the typed [`StreamEvent::Usage`]
    /// shape (Claude/Copilot/Qoder) always carries those. Emitting the exact
    /// frame here keeps the multi-kind parser from adding or losing keys
    /// through a shared shape it does not own.
    Json(Value),
}

impl StreamEvent {
    pub fn to_json(&self) -> Value {
        match self {
            StreamEvent::Status {
                label,
                model,
                session_id,
                qodercli_version,
                ttft_ms,
            } => {
                let mut event = serde_json::Map::new();
                event.insert("type".to_string(), Value::String("status".to_string()));
                event.insert("label".to_string(), Value::String(label.clone()));
                // Each key is inserted only when the TS frame that produced it
                // carries it — see the field docs on the variant.
                if let Some(model) = model {
                    event.insert("model".to_string(), model.clone());
                }
                if let Some(session_id) = session_id {
                    event.insert("sessionId".to_string(), session_id.clone());
                }
                if let Some(qodercli_version) = qodercli_version {
                    event.insert(
                        "qodercliVersion".to_string(),
                        qodercli_version.clone(),
                    );
                }
                if let Some(ttft_ms) = ttft_ms {
                    event.insert("ttftMs".to_string(), Value::from(*ttft_ms));
                }
                Value::Object(event)
            }
            StreamEvent::ThinkingDelta { delta } => json!({
                "type": "thinking_delta",
                "delta": delta,
            }),
            StreamEvent::TextDelta { delta } => json!({
                "type": "text_delta",
                "delta": delta,
            }),
            StreamEvent::ToolUse { id, name, input } => json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }),
            StreamEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => json!({
                "type": "tool_result",
                "toolUseId": tool_use_id,
                "content": content,
                "isError": is_error,
            }),
            StreamEvent::Usage {
                usage,
                stop_reason,
                duration_ms,
                cost_usd,
                model_usage,
                is_error,
            } => {
                let mut event = serde_json::Map::new();
                event.insert("type".to_string(), Value::String("usage".to_string()));
                event.insert("usage".to_string(), usage.clone());
                event.insert(
                    "stopReason".to_string(),
                    Value::String(stop_reason.clone()),
                );
                event.insert(
                    "durationMs".to_string(),
                    duration_ms.map(Value::from).unwrap_or(Value::Null),
                );
                if let Some(cost) = cost_usd {
                    event.insert("costUsd".to_string(), cost.clone());
                }
                if let Some(model_usage) = model_usage {
                    event.insert("modelUsage".to_string(), model_usage.clone());
                }
                if let Some(is_error) = is_error {
                    event.insert("isError".to_string(), Value::Bool(*is_error));
                }
                Value::Object(event)
            }
            StreamEvent::TurnEnd { stop_reason } => json!({
                "type": "turn_end",
                "stopReason": stop_reason,
            }),
            StreamEvent::ThinkingStart => json!({ "type": "thinking_start" }),
            StreamEvent::ThinkingTokens { tokens } => json!({
                "type": "thinking_tokens",
                "tokens": tokens,
            }),
            StreamEvent::ToolInputDelta { id, name, delta } => json!({
                "type": "tool_input_delta",
                "id": id,
                "name": name,
                "delta": delta,
            }),
            StreamEvent::RequestUsage { request_id, usage } => json!({
                "type": "request_usage",
                "requestId": request_id,
                "usage": usage,
            }),
            StreamEvent::Error { message, code, raw } => {
                let mut event = serde_json::Map::new();
                event.insert("type".to_string(), Value::String("error".to_string()));
                event.insert("message".to_string(), Value::String(message.clone()));
                if let Some(code) = code {
                    event.insert("code".to_string(), Value::String(code.clone()));
                }
                if let Some(raw) = raw {
                    event.insert("raw".to_string(), Value::String(raw.clone()));
                }
                Value::Object(event)
            }
            StreamEvent::ToolInFlight {
                id,
                name,
                input,
                started_at,
            } => json!({
                "type": "tool_in_flight",
                "id": id,
                "name": name,
                "input": input,
                "startedAt": started_at,
            }),
            StreamEvent::Json(value) => value.clone(),
            StreamEvent::Raw {
                line,
                truncated_original_bytes,
            } => {
                let mut event = serde_json::Map::new();
                event.insert("type".to_string(), Value::String("raw".to_string()));
                event.insert("line".to_string(), Value::String(line.clone()));
                if let Some(bytes) = truncated_original_bytes {
                    event.insert(
                        "truncated".to_string(),
                        json!({ "originalBytes": bytes }),
                    );
                }
                Value::Object(event)
            }
        }
    }
}

/// Bound one raw line to the event JSON budget (parity:
/// `boundRawAgentLine`, simplified — see module docs).
pub fn bound_raw_line(line: &str) -> StreamEvent {
    if line.len() <= RAW_LINE_BUDGET_BYTES {
        return StreamEvent::Raw {
            line: line.to_string(),
            truncated_original_bytes: None,
        };
    }
    let original_bytes = line.len();
    let mut budget = STRING_LADDER_TOP_BYTES;
    loop {
        let cut = floor_char_boundary(line, budget.min(line.len()));
        let candidate = &line[..cut];
        if candidate.len() <= RAW_LINE_BUDGET_BYTES || budget <= STRING_LADDER_MIN_BYTES {
            return StreamEvent::Raw {
                line: candidate.to_string(),
                truncated_original_bytes: Some(original_bytes),
            };
        }
        budget /= 2;
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Copilot `--output-format json` JSONL parser (parity:
/// `createCopilotStreamHandler` in `apps/daemon/src/copilot-stream.ts`).
/// Feed chunks; each emitted event goes to `on_event`. Unknown shapes are
/// dropped unless they carry no `type` at all (then ignored like TS).
#[derive(Debug, Default)]
pub struct CopilotParser {
    buffer: String,
}

impl CopilotParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &str, on_event: &mut dyn FnMut(StreamEvent)) {
        self.buffer.push_str(chunk);
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].trim().to_string();
            self.buffer.drain(..pos + 1);
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(value) => Self::handle_object(&value, on_event),
                Err(_) => on_event(bound_raw_line(&line)),
            }
        }
    }

    pub fn flush(&mut self, on_event: &mut dyn FnMut(StreamEvent)) {
        let rest = std::mem::take(&mut self.buffer).trim().to_string();
        if rest.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(&rest) {
            Ok(value) => Self::handle_object(&value, on_event),
            Err(_) => on_event(bound_raw_line(&rest)),
        }
    }

    fn handle_object(value: &Value, on_event: &mut dyn FnMut(StreamEvent)) {
        let obj = match value.as_object() {
            Some(obj) => obj,
            None => return,
        };
        let event_type = match obj.get("type").and_then(Value::as_str) {
            Some(t) => t,
            None => return,
        };
        let data = obj
            .get("data")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let get = |key: &str| data.get(key);
        match event_type {
            "session.tools_updated" => {
                if let Some(model) = get("model").and_then(Value::as_str) {
                    on_event(StreamEvent::Status {
                        label: "initializing".to_string(),
                        model: Some(Value::String(model.to_string())),
                        session_id: None,
                        qodercli_version: None,
                        ttft_ms: None,
                    });
                }
            }
            "assistant.turn_start" => on_event(StreamEvent::Status {
                label: "streaming".to_string(),
                model: None,
                session_id: None,
                qodercli_version: None,
                ttft_ms: None,
            }),
            "assistant.reasoning_delta" => {
                if let Some(delta) = get("deltaContent").and_then(Value::as_str) {
                    on_event(StreamEvent::ThinkingDelta {
                        delta: delta.to_string(),
                    });
                }
            }
            "assistant.message_delta" => {
                if let Some(delta) = get("deltaContent").and_then(Value::as_str) {
                    on_event(StreamEvent::TextDelta {
                        delta: delta.to_string(),
                    });
                }
            }
            "tool.execution_start" => on_event(StreamEvent::ToolUse {
                id: get("toolCallId").and_then(Value::as_str).map(str::to_string),
                name: get("toolName").and_then(Value::as_str).map(str::to_string),
                input: get("arguments").cloned().unwrap_or(Value::Null),
            }),
            "tool.execution_complete" => on_event(StreamEvent::ToolResult {
                tool_use_id: get("toolCallId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                content: stringify_result(get("result")),
                is_error: get("success") == Some(&Value::Bool(false)),
            }),
            "result" => {
                // Usage/exit live at the top level, not under `data`.
                let usage = obj.get("usage").cloned().unwrap_or(Value::Null);
                let success = obj.get("success") == Some(&Value::Bool(true));
                let exit_zero = obj.get("exitCode") == Some(&json!(0));
                let duration_ms = usage
                    .get("sessionDurationMs")
                    .and_then(Value::as_i64);
                on_event(StreamEvent::Usage {
                    usage,
                    stop_reason: if success || exit_zero {
                        "completed".to_string()
                    } else {
                        "error".to_string()
                    },
                    duration_ms,
                    cost_usd: None,
                    model_usage: None,
                    is_error: None,
                });
            }
            _ => {}
        }
    }
}

fn stringify_result(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(obj) = value.as_object() {
        if let Some(text) = obj.get("content").and_then(Value::as_str) {
            return text.to_string();
        }
        if let Some(text) = obj.get("detailedContent").and_then(Value::as_str) {
            return text.to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(parser: &mut CopilotParser, chunk: &str) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        parser.feed(chunk, &mut |event| events.push(event));
        events
    }

    #[test]
    fn copilot_maps_status_and_deltas() {
        let mut parser = CopilotParser::new();
        let events = collect(
            &mut parser,
            "{\"type\":\"session.tools_updated\",\"data\":{\"model\":\"gpt-5.2\"}}\n{\"type\":\"assistant.turn_start\",\"data\":{}}\n{\"type\":\"assistant.reasoning_delta\",\"data\":{\"deltaContent\":\"hmm\"}}\n{\"type\":\"assistant.message_delta\",\"data\":{\"deltaContent\":\"hi\"}}\n",
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::Status {
                    label: "initializing".to_string(),
                    model: Some(json!("gpt-5.2")),
                    session_id: None,
                    qodercli_version: None,
                    ttft_ms: None,
                },
                StreamEvent::Status {
                    label: "streaming".to_string(),
                    model: None,
                    session_id: None,
                    qodercli_version: None,
                    ttft_ms: None,
                },
                StreamEvent::ThinkingDelta {
                    delta: "hmm".to_string()
                },
                StreamEvent::TextDelta {
                    delta: "hi".to_string()
                },
            ]
        );
    }

    #[test]
    fn copilot_maps_tool_cycle_and_usage() {
        let mut parser = CopilotParser::new();
        let events = collect(
            &mut parser,
            "{\"type\":\"tool.execution_start\",\"data\":{\"toolCallId\":\"1\",\"toolName\":\"bash\",\"arguments\":{\"cmd\":\"ls\"}}}\n{\"type\":\"tool.execution_complete\",\"data\":{\"toolCallId\":\"1\",\"result\":\"ok\",\"success\":true}}\n{\"type\":\"result\",\"success\":true,\"usage\":{\"sessionDurationMs\":12}}\n",
        );
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], StreamEvent::ToolUse { .. }));
        match &events[1] {
            StreamEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("1"));
                assert_eq!(content, "ok");
                assert!(!is_error);
            }
            other => panic!("unexpected {other:?}"),
        }
        match &events[2] {
            StreamEvent::Usage { stop_reason, duration_ms, .. } => {
                assert_eq!(stop_reason, "completed");
                assert_eq!(*duration_ms, Some(12));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn copilot_drops_unknown_lanes_and_bounds_raw() {
        let mut parser = CopilotParser::new();
        let events = collect(
            &mut parser,
            "{\"type\":\"session.mcp_server_status\",\"data\":{}}\nnot json\n",
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Raw { .. }));
        let big = "x".repeat(RAW_LINE_BUDGET_BYTES + 1);
        match bound_raw_line(&big) {
            StreamEvent::Raw {
                line,
                truncated_original_bytes,
            } => {
                assert!(line.len() <= RAW_LINE_BUDGET_BYTES);
                assert_eq!(truncated_original_bytes, Some(big.len()));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// Claude Code `--output-format stream-json --verbose` JSONL parser
/// (parity: `createClaudeStreamHandler` in
/// `apps/daemon/src/runtimes/claude-stream.ts`, core lane mapping).
///
/// Documented deviations (sub-layers not ported): the role-marker guard
/// (text passes through unfiltered), child-evidence collectors, the
/// tool-input path scanner (`tool_input_target`/`tool_input_progress`
/// frames are not emitted), task-form snapshots, question forms, and the
/// artifact-echo dedup (text passes through unfiltered). Event lanes and
/// emission order match TypeScript.
#[derive(Debug, Default)]
pub struct ClaudeParser {
    buffer: String,
    blocks: std::collections::HashMap<String, BlockState>,
    current_message_id: Option<String>,
    text_streamed: std::collections::HashSet<String>,
    thinking_streamed: std::collections::HashSet<String>,
    current_message_streamed_text: bool,
    current_message_streamed_thinking: bool,
    emitted_tool_use_ids: std::collections::HashSet<String>,
    turn_end_emitted_for: std::collections::HashSet<String>,
}

#[derive(Debug, Clone, Default)]
struct BlockState {
    block_type: Option<String>,
    name: Option<String>,
    id: Option<String>,
    input: String,
    input_value: Option<Value>,
}

impl ClaudeParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &str, on_event: &mut dyn FnMut(StreamEvent)) {
        self.buffer.push_str(chunk);
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].trim().to_string();
            self.buffer.drain(..pos + 1);
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(value) => self.handle_object(&value, on_event),
                Err(_) => on_event(bound_raw_line(&line)),
            }
        }
    }

    pub fn flush(&mut self, on_event: &mut dyn FnMut(StreamEvent)) {
        let rest = std::mem::take(&mut self.buffer).trim().to_string();
        if rest.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(&rest) {
            Ok(value) => self.handle_object(&value, on_event),
            Err(_) => on_event(bound_raw_line(&rest)),
        }
    }

    fn block_key(&self, index: &Value) -> String {
        let id = self.current_message_id.as_deref().unwrap_or("anon");
        match index {
            Value::Number(n) => format!("{id}:{n}"),
            Value::String(s) => format!("{id}:{s}"),
            _ => format!("{id}:?"),
        }
    }

    fn emit_turn_end_once(
        &mut self,
        message_id: Option<String>,
        stop_reason: &str,
        parent_tool_use_id: Option<&str>,
        on_event: &mut dyn FnMut(StreamEvent),
    ) {
        if parent_tool_use_id
            .map(|id| !id.trim().is_empty())
            .unwrap_or(false)
        {
            return;
        }
        if let Some(id) = message_id.clone() {
            if !self.turn_end_emitted_for.insert(id) {
                return;
            }
        }
        on_event(StreamEvent::TurnEnd {
            stop_reason: stop_reason.to_string(),
        });
    }

    fn emit_tool_use(
        &self,
        id: Option<&str>,
        name: Option<&str>,
        input: Value,
        on_event: &mut dyn FnMut(StreamEvent),
    ) {
        on_event(StreamEvent::ToolUse {
            id: id.map(str::to_string),
            name: name.map(str::to_string),
            input,
        });
    }

    fn wrapper_tool_use_input(&self, block: &serde_json::Map<String, Value>) -> Value {
        if let Some(id) = block.get("id").and_then(Value::as_str) {
            for state in self.blocks.values() {
                if state.block_type.as_deref() == Some("tool_use")
                    && state.id.as_deref() == Some(id)
                    && !state.input.trim().is_empty()
                {
                    if let Ok(parsed) = serde_json::from_str::<Value>(&state.input) {
                        return parsed;
                    }
                    break;
                }
            }
        }
        block.get("input").cloned().unwrap_or(Value::Null)
    }

    fn handle_object(&mut self, obj: &Value, on_event: &mut dyn FnMut(StreamEvent)) {
        let obj = match obj.as_object() {
            Some(obj) => obj,
            None => return,
        };
        let obj_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let parent_tool_use_id = obj
            .get("parent_tool_use_id")
            .and_then(Value::as_str);

        if obj_type == "system" {
            match obj.get("subtype").and_then(Value::as_str).unwrap_or("") {
                "init" => {
                    // TS: `model: obj.model ?? null, sessionId: obj.session_id ??
                    // null` — both keys are always present, null when absent.
                    on_event(StreamEvent::Status {
                        label: "initializing".to_string(),
                        model: Some(
                            obj.get("model")
                                .and_then(Value::as_str)
                                .map(|model| json!(model))
                                .unwrap_or(Value::Null),
                        ),
                        session_id: Some(
                            obj.get("session_id")
                                .and_then(Value::as_str)
                                .map(|session| json!(session))
                                .unwrap_or(Value::Null),
                        ),
                        qodercli_version: None,
                        ttft_ms: None,
                    });
                    return;
                }
                "status" => {
                    on_event(StreamEvent::Status {
                        label: obj
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("working")
                            .to_string(),
                        model: None,
                        session_id: None,
                        qodercli_version: None,
                        ttft_ms: None,
                    });
                    return;
                }
                "thinking_tokens" => {
                    if let Some(tokens) = obj.get("estimated_tokens").and_then(Value::as_f64) {
                        if tokens.is_finite() && tokens > 0.0 {
                            on_event(StreamEvent::ThinkingTokens { tokens });
                        }
                    }
                    return;
                }
                _ => return,
            }
        }

        if obj_type == "stream_event" {
            if let Some(event) = obj.get("event") {
                self.handle_stream_event(event, parent_tool_use_id, on_event);
            }
            return;
        }

        if obj_type == "assistant" {
            let Some(message) = obj.get("message").and_then(Value::as_object) else {
                return;
            };
            let empty = Vec::new();
            let content = message
                .get("content")
                .and_then(Value::as_array)
                .unwrap_or(&empty);
            let explicit_id = message
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let text_id = explicit_id.clone().or_else(|| {
                if self.current_message_streamed_text {
                    self.current_message_id.clone()
                } else {
                    None
                }
            });
            let thinking_id = explicit_id.clone().or_else(|| {
                if self.current_message_streamed_thinking {
                    self.current_message_id.clone()
                } else {
                    None
                }
            });
            if let Some(id) = explicit_id.clone() {
                self.current_message_id = Some(id);
            }
            if let Some(id) = explicit_id.clone() {
                if let Some(usage) = message.get("usage").and_then(Value::as_object) {
                    let mut record = serde_json::Map::new();
                    let mut seen = false;
                    for key in [
                        "input_tokens",
                        "output_tokens",
                        "cache_creation_input_tokens",
                        "cache_read_input_tokens",
                    ] {
                        if let Some(n) = usage.get(key).and_then(Value::as_f64) {
                            if n.is_finite() {
                                record.insert(key.to_string(), Value::from(n));
                                seen = true;
                            }
                        }
                    }
                    if seen {
                        on_event(StreamEvent::RequestUsage {
                            request_id: id,
                            usage: Value::Object(record),
                        });
                    }
                }
            }
            let text_done = text_id
                .as_ref()
                .map(|id| self.text_streamed.contains(id))
                .unwrap_or(false);
            let thinking_done = thinking_id
                .as_ref()
                .map(|id| self.thinking_streamed.contains(id))
                .unwrap_or(false);
            let stop_reason = message
                .get("stop_reason")
                .and_then(Value::as_str)
                .map(str::to_string);
            for block in content {
                let Some(block) = block.as_object() else {
                    continue;
                };
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                if block_type == "tool_use" {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        if self.emitted_tool_use_ids.contains(id) {
                            continue;
                        }
                        self.emitted_tool_use_ids.insert(id.to_string());
                    }
                    let input = self.wrapper_tool_use_input(block);
                    self.emit_tool_use(
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                        input,
                        on_event,
                    );
                } else if !text_done
                    && block_type == "text"
                    && block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|t| !t.is_empty())
                        .unwrap_or(false)
                {
                    on_event(StreamEvent::TextDelta {
                        delta: block["text"].as_str().unwrap_or_default().to_string(),
                    });
                } else if !thinking_done
                    && block_type == "thinking"
                    && block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .map(|t| !t.is_empty())
                        .unwrap_or(false)
                {
                    on_event(StreamEvent::ThinkingDelta {
                        delta: block["thinking"].as_str().unwrap_or_default().to_string(),
                    });
                }
            }
            if let Some(reason) = stop_reason {
                self.emit_turn_end_once(
                    explicit_id.or_else(|| self.current_message_id.clone()),
                    &reason,
                    parent_tool_use_id,
                    on_event,
                );
            }
            if let Some(error) = obj.get("error").and_then(Value::as_str) {
                if !error.trim().is_empty() && parent_tool_use_id.is_none() {
                    let mut text = String::new();
                    for block in content {
                        if let Some(block) = block.as_object() {
                            if block.get("type").and_then(Value::as_str) == Some("text") {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        text.push('\n');
                                    }
                                    text.push_str(t);
                                }
                            }
                        }
                    }
                    let text = text.trim().to_string();
                    on_event(StreamEvent::Error {
                        message: if text.is_empty() {
                            error.to_string()
                        } else {
                            text
                        },
                        code: Some(error.to_string()),
                        raw: None,
                    });
                }
            }
            self.current_message_streamed_text = false;
            self.current_message_streamed_thinking = false;
            return;
        }

        if obj_type == "user" {
            if let Some(content) = obj
                .get("message")
                .and_then(Value::as_object)
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for block in content {
                    let Some(block) = block.as_object() else {
                        continue;
                    };
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let content = match block.get("content") {
                        Some(Value::String(text)) => text.clone(),
                        Some(Value::Array(parts)) => parts
                            .iter()
                            .map(|part| {
                                part.as_object()
                                    .filter(|p| {
                                        p.get("type").and_then(Value::as_str) == Some("text")
                                    })
                                    .and_then(|p| p.get("text"))
                                    .map(|t| {
                                        t.as_str().unwrap_or(&t.to_string()).to_string()
                                    })
                                    .unwrap_or_else(|| part.to_string())
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    on_event(StreamEvent::ToolResult {
                        tool_use_id: block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        content,
                        is_error: block.get("is_error") == Some(&Value::Bool(true)),
                    });
                }
            }
            return;
        }

        if obj_type == "result" {
            let is_error = obj.get("is_error") == Some(&Value::Bool(true));
            let stop_reason = obj
                .get("stop_reason")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    obj.get("terminal_reason")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                });
            let mut usage = obj.get("usage").cloned().unwrap_or(Value::Null);
            if usage.is_null() {
                usage = Value::Object(serde_json::Map::new());
            }
            // Cost/duration ride on the usage lane like TS.
            if let Value::Object(ref mut map) = usage {
                if let Some(cost) = obj.get("total_cost_usd") {
                    map.insert("costUsd".to_string(), cost.clone());
                }
                if let Some(duration) = obj.get("duration_ms") {
                    map.insert("durationMs".to_string(), duration.clone());
                }
                if is_error {
                    map.insert("isError".to_string(), Value::Bool(true));
                }
            }
            on_event(StreamEvent::Usage {
                usage,
                stop_reason: stop_reason.clone().unwrap_or_default(),
                duration_ms: obj.get("duration_ms").and_then(Value::as_i64),
                cost_usd: None,
                model_usage: None,
                is_error: None,
            });
            if is_error {
                let message = obj
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|errors| {
                        errors
                            .iter()
                            .filter_map(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        obj.get("result")
                            .and_then(Value::as_str)
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                    })
                    .or_else(|| {
                        obj.get("subtype")
                            .and_then(Value::as_str)
                            .map(|s| format!("Claude run failed: {s}"))
                    })
                    .unwrap_or_else(|| "Claude run failed".to_string());
                let trimmed = message.trim();
                let code = if trimmed.eq_ignore_ascii_case("prompt is too long.")
                    || trimmed.eq_ignore_ascii_case("prompt is too long")
                    || (trimmed.to_lowercase().starts_with("api error:")
                        && trimmed.to_lowercase().contains("prompt is too long"))
                {
                    Some("AGENT_PROMPT_TOO_LARGE".to_string())
                } else {
                    obj.get("subtype")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(Some("result_error".to_string()))
                };
                on_event(StreamEvent::Error {
                    message,
                    code,
                    raw: None,
                });
            }
        }
    }

    fn handle_stream_event(
        &mut self,
        event: &Value,
        parent_tool_use_id: Option<&str>,
        on_event: &mut dyn FnMut(StreamEvent),
    ) {
        let Some(event) = event.as_object() else {
            return;
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");

        if event_type == "message_delta" {
            if let Some(reason) = event
                .get("delta")
                .and_then(Value::as_object)
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
            {
                self.emit_turn_end_once(
                    self.current_message_id.clone(),
                    reason,
                    parent_tool_use_id,
                    on_event,
                );
            }
            return;
        }

        if event_type == "message_start" {
            if let Some(id) = event
                .get("message")
                .and_then(Value::as_object)
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
            {
                self.current_message_id = Some(id.to_string());
            } else {
                self.current_message_id = None;
            }
            self.current_message_streamed_text = false;
            self.current_message_streamed_thinking = false;
            if let Some(ttft) = event.get("ttft_ms").and_then(Value::as_i64) {
                // Parity: `onEvent({ type: 'status', label: 'streaming',
                // ttftMs: ev.ttft_ms })` — a status frame, never a raw one.
                on_event(StreamEvent::Status {
                    label: "streaming".to_string(),
                    model: None,
                    session_id: None,
                    qodercli_version: None,
                    ttft_ms: Some(ttft),
                });
            }
            return;
        }

        if event_type == "content_block_start" {
            let Some(block) = event.get("content_block").and_then(Value::as_object) else {
                return;
            };
            let key = self.block_key(event.get("index").unwrap_or(&Value::Null));
            let mut state = BlockState {
                block_type: block
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                id: block.get("id").and_then(Value::as_str).map(str::to_string),
                input: String::new(),
                input_value: block.get("input").cloned(),
            };
            let _ = &mut state;
            if state.block_type.as_deref() == Some("thinking") {
                on_event(StreamEvent::ThinkingStart);
            }
            self.blocks.insert(key, state);
            return;
        }

        if event_type == "content_block_delta" {
            let Some(delta) = event.get("delta").and_then(Value::as_object) else {
                return;
            };
            let key = self.block_key(event.get("index").unwrap_or(&Value::Null));
            let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
            if delta_type == "text_delta" {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    if let Some(id) = self.current_message_id.clone() {
                        self.text_streamed.insert(id);
                    }
                    self.current_message_streamed_text = true;
                    on_event(StreamEvent::TextDelta {
                        delta: text.to_string(),
                    });
                }
                return;
            }
            if delta_type == "thinking_delta" {
                if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                    // Only a delta that carried characters counts as
                    // "reasoning already streamed" (parity comment in TS).
                    if !thinking.is_empty() {
                        if let Some(id) = self.current_message_id.clone() {
                            self.thinking_streamed.insert(id);
                        }
                        self.current_message_streamed_thinking = true;
                    }
                    on_event(StreamEvent::ThinkingDelta {
                        delta: thinking.to_string(),
                    });
                }
                return;
            }
            if delta_type == "input_json_delta" {
                if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                    let state = self.blocks.get_mut(&key);
                    if let Some(state) = state {
                        if state.block_type.as_deref() == Some("tool_use") {
                            state.input.push_str(partial);
                            if let (Some(id), Some(name)) =
                                (state.id.clone(), state.name.clone())
                            {
                                on_event(StreamEvent::ToolInputDelta {
                                    id,
                                    name,
                                    delta: partial.to_string(),
                                });
                            }
                        }
                    }
                }
                return;
            }
            return;
        }

        if event_type == "content_block_stop" {
            let key = self.block_key(event.get("index").unwrap_or(&Value::Null));
            let state = self.blocks.get(&key).cloned();
            let already = state.as_ref().is_some_and(|state| {
                state.block_type.as_deref() == Some("tool_use")
                    && state
                        .id
                        .as_deref()
                        .map(|id| self.emitted_tool_use_ids.contains(id))
                        .unwrap_or(false)
            });
            if already {
                self.blocks.remove(&key);
                return;
            }
            if let Some(state) = state {
                if state.block_type.as_deref() == Some("tool_use")
                    && state.id.is_some()
                    && !state.input.trim().is_empty()
                {
                    if let Ok(input) = serde_json::from_str::<Value>(&state.input) {
                        let id = state.id.clone();
                        let name = state.name.clone();
                        if let Some(id) = id.as_deref() {
                            self.emitted_tool_use_ids.insert(id.to_string());
                        }
                        self.emit_tool_use(id.as_deref(), name.as_deref(), input, on_event);
                    }
                } else if state.block_type.as_deref() == Some("tool_use")
                    && state.id.is_some()
                    && state.input_value.is_some()
                {
                    let id = state.id.clone();
                    let name = state.name.clone();
                    let input = state.input_value.clone().unwrap_or(Value::Null);
                    if let Some(id) = id.as_deref() {
                        self.emitted_tool_use_ids.insert(id.to_string());
                    }
                    self.emit_tool_use(id.as_deref(), name.as_deref(), input, on_event);
                }
                self.blocks.remove(&key);
            }
        }
    }
}

/// Qoder CLI `--output-format stream-json` JSONL parser (parity:
/// `createQoderStreamHandler` in `apps/daemon/src/runtimes/qoder-stream.ts`).
///
/// Qoder's top-level records are wrapper objects (`system`, `assistant`,
/// `result`) with its own fields, so this stays separate from the Claude and
/// Copilot parsers. Unrecognised records and unparseable lines become bounded
/// `raw` events.
#[derive(Debug, Default)]
pub struct QoderParser {
    buffer: String,
    emitted_thinking_start: bool,
}

impl QoderParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &str, on_event: &mut dyn FnMut(StreamEvent)) {
        self.buffer.push_str(chunk);
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].trim().to_string();
            self.buffer.drain(..pos + 1);
            if line.is_empty() {
                continue;
            }
            self.handle_line(&line, on_event);
        }
    }

    pub fn flush(&mut self, on_event: &mut dyn FnMut(StreamEvent)) {
        let rest = std::mem::take(&mut self.buffer).trim().to_string();
        if rest.is_empty() {
            return;
        }
        self.handle_line(&rest, on_event);
    }

    fn handle_line(&mut self, line: &str, on_event: &mut dyn FnMut(StreamEvent)) {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => self.handle_object(&value, line, on_event),
            Err(_) => on_event(bound_raw_line(line)),
        }
    }

    fn handle_object(
        &mut self,
        value: &Value,
        raw_line: &str,
        on_event: &mut dyn FnMut(StreamEvent),
    ) {
        let obj = match value.as_object() {
            Some(obj) => obj,
            None => return,
        };
        let obj_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

        if obj_type == "system" && obj.get("subtype").and_then(Value::as_str) == Some("init") {
            // TS: `model: typeof obj.model === 'string' ? obj.model : undefined`
            // (and the same for sessionId / qodercliVersion) — the key is
            // omitted, not null, when the wire did not carry a string.
            on_event(StreamEvent::Status {
                label: "initializing".to_string(),
                model: obj
                    .get("model")
                    .and_then(Value::as_str)
                    .map(|model| json!(model)),
                session_id: obj
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(|session| json!(session)),
                qodercli_version: obj
                    .get("qodercli_version")
                    .and_then(Value::as_str)
                    .map(|version| json!(version)),
                ttft_ms: None,
            });
            return;
        }

        if obj_type == "assistant" {
            // `qoder-stream.ts` requires `obj.message` to be a record; anything
            // else falls through to the bounded raw event below.
            let Some(message) = obj.get("message").and_then(Value::as_object) else {
                on_event(bound_raw_line(raw_line));
                return;
            };
            let content = message.get("content");
            let mut emitted_text = false;
            if let Some(blocks) = content.and_then(Value::as_array) {
                for block in blocks {
                    let text = qoder_text_from_content_block(block);
                    if !text.is_empty() {
                        emitted_text = true;
                        on_event(StreamEvent::TextDelta { delta: text });
                        continue;
                    }
                    if block.get("type").and_then(Value::as_str) == Some("thinking") {
                        if let Some(thinking) = block.get("thinking").and_then(Value::as_str) {
                            if !thinking.is_empty() {
                                if !self.emitted_thinking_start {
                                    self.emitted_thinking_start = true;
                                    on_event(StreamEvent::ThinkingStart);
                                }
                                on_event(StreamEvent::ThinkingDelta {
                                    delta: thinking.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            if !emitted_text {
                if let Some(text) = content.and_then(Value::as_str) {
                    on_event(StreamEvent::TextDelta {
                        delta: text.to_string(),
                    });
                    emitted_text = true;
                }
            }
            if let Some(error) = obj.get("error") {
                if !error.is_null() && !emitted_text {
                    on_event(StreamEvent::Error {
                        message: qoder_message_from_error(error),
                        code: None,
                        raw: Some(raw_line.to_string()),
                    });
                }
            }
            return;
        }

        if obj_type == "result" {
            let is_error = obj.get("is_error") == Some(&Value::Bool(true));
            let usage = obj.get("usage").cloned().unwrap_or(Value::Null);
            on_event(StreamEvent::Usage {
                usage,
                stop_reason: obj
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                duration_ms: obj.get("duration_ms").and_then(Value::as_i64),
                cost_usd: obj.get("total_cost_usd").cloned(),
                model_usage: obj.get("modelUsage").cloned(),
                is_error: Some(is_error),
            });
            if is_error {
                on_event(StreamEvent::Error {
                    message: qoder_message_from_result(obj),
                    code: None,
                    raw: Some(raw_line.to_string()),
                });
            }
            return;
        }

        // An unrecognised record (e.g. the `user` echo carrying tool results):
        // bounded at the source, like the other parsers.
        on_event(bound_raw_line(raw_line));
    }
}

/// `textFromContentBlock` in `qoder-stream.ts`: a block's own `text` when it is
/// a `text` block (or carries a `text` field), otherwise nothing.
fn qoder_text_from_content_block(block: &Value) -> String {
    let Some(obj) = block.as_object() else {
        return String::new();
    };
    if obj.get("type").and_then(Value::as_str) == Some("text") {
        if let Some(text) = obj.get("text").and_then(Value::as_str) {
            return text.to_string();
        }
    }
    if let Some(text) = obj.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    String::new()
}

/// `messageFromError` in `qoder-stream.ts`.
fn qoder_message_from_error(error: &Value) -> String {
    if let Some(message) = error
        .as_object()
        .and_then(|obj| obj.get("message"))
        .and_then(Value::as_str)
    {
        return message.to_string();
    }
    if let Some(text) = error.as_str() {
        if !text.is_empty() {
            return text.to_string();
        }
    }
    "Unknown Qoder error".to_string()
}

/// `messageFromResult` in `qoder-stream.ts`.
fn qoder_message_from_result(obj: &serde_json::Map<String, Value>) -> String {
    if let Some(error) = obj.get("error") {
        if let Some(text) = error.as_str() {
            if !text.is_empty() {
                return text.to_string();
            }
        }
        if let Some(message) = error
            .as_object()
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            if !message.is_empty() {
                return message.to_string();
            }
        }
    }
    if let Some(message) = obj.get("message").and_then(Value::as_str) {
        if !message.is_empty() {
            return message.to_string();
        }
    }
    if let Some(stop_reason) = obj.get("stop_reason").and_then(Value::as_str) {
        if !stop_reason.is_empty() {
            return format!("Qoder run failed: {stop_reason}");
        }
    }
    "Qoder run failed".to_string()
}

/// Per-run state for OpenCode's optional local plugin (parity:
/// `OpenCodeToolEvents` in `runtimes/opencode-tool-events.ts`). The plugin can
/// announce a tool call before the native `tool_use` frame arrives, so pending
/// calls are held and reconciled once the native frame completes them.
#[derive(Debug, Default)]
pub struct OpenCodeToolEvents {
    session_id: Option<String>,
    pending: HashMap<String, PendingOpenCodeTool>,
    settled: HashSet<String>,
    closed: bool,
    before_session: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
struct PendingOpenCodeTool {
    name: String,
    input: Value,
    started_at: i64,
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

impl OpenCodeToolEvents {
    /// Parity: `startSession`. Replays frames buffered before the session id
    /// was known, then drops the buffer.
    pub fn start_session(&mut self, id: Option<&str>, on_event: &mut dyn FnMut(StreamEvent)) {
        let Some(id) = id else { return };
        if self.closed {
            return;
        }
        self.session_id = Some(id.to_string());
        let buffered: Vec<Value> = self.before_session.drain().map(|(_, frame)| frame).collect();
        for frame in buffered {
            if let Value::Object(map) = &frame {
                self.handle(map, on_event);
            }
        }
    }

    /// Parity: `handle`. Returns whether the frame was an `od_opencode_tool`
    /// frame (whether or not it produced an event).
    pub fn handle(
        &mut self,
        frame: &serde_json::Map<String, Value>,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> bool {
        if frame.get("type").and_then(Value::as_str) != Some("od_opencode_tool") {
            return false;
        }
        let session_id = frame.get("sessionID").and_then(Value::as_str);
        let call_id = frame.get("callID").and_then(Value::as_str);
        let tool = frame.get("tool").and_then(Value::as_str);
        let version_ok = frame.get("version").and_then(Value::as_i64) == Some(1);
        let shaped = version_ok
            && session_id.is_some_and(|id| !id.is_empty())
            && call_id.is_some_and(|id| !id.is_empty())
            && tool.is_some_and(|name| !name.is_empty());
        if self.closed || !shaped {
            return true;
        }
        let (session_id, call_id, tool) = (
            session_id.unwrap_or_default(),
            call_id.unwrap_or_default(),
            tool.unwrap_or_default(),
        );
        // The plugin callback can beat the asynchronous step_start writer.
        // Buffer that small window; only the native root session reaches the UI.
        let Some(root_session) = self.session_id.clone() else {
            if self.before_session.len() < 128 {
                self.before_session
                    .insert(format!("{session_id}:{call_id}"), Value::Object(frame.clone()));
            }
            return true;
        };
        if session_id != root_session || self.settled.contains(call_id) {
            return true;
        }
        let mut input = serde_json::Map::new();
        if let Some(path) = frame.get("path").and_then(Value::as_str) {
            if !path.is_empty() && path.len() < 8192 {
                input.insert("file_path".to_string(), Value::String(path.to_string()));
            }
        }
        let previous = self.pending.get(call_id).cloned();
        if let Some(previous) = &previous {
            if previous.name == tool && previous.input == Value::Object(input.clone()) {
                return true;
            }
        }
        let started_at = previous.as_ref().map(|tool| tool.started_at).unwrap_or_else(now_millis);
        let state = PendingOpenCodeTool {
            name: tool.to_string(),
            input: Value::Object(input),
            started_at,
        };
        self.pending.insert(call_id.to_string(), state.clone());
        on_event(StreamEvent::ToolInFlight {
            id: call_id.to_string(),
            name: state.name,
            input: state.input,
            started_at: state.started_at,
        });
        true
    }

    /// Parity: `complete` — the native frame settled this call.
    pub fn complete(&mut self, id: &str) {
        self.pending.remove(id);
        self.settled.insert(id.to_string());
    }

    /// Parity: `flush` — a stream that ends with calls still pending reports
    /// each as a failed result rather than leaving them rendered forever.
    pub fn flush(&mut self, on_event: &mut dyn FnMut(StreamEvent)) {
        self.closed = true;
        self.before_session.clear();
        let pending: Vec<(String, PendingOpenCodeTool)> = self.pending.drain().collect();
        for (id, state) in pending {
            on_event(StreamEvent::ToolUse {
                id: Some(id.clone()),
                name: Some(state.name),
                input: state.input,
            });
            on_event(StreamEvent::ToolResult {
                tool_use_id: Some(id.clone()),
                content: "Tool stream ended before completion was reported.".to_string(),
                is_error: true,
            });
            self.settled.insert(id);
        }
    }
}

/// Multi-runtime JSONL parser (parity: `createJsonEventStreamHandler` in
/// `runtimes/json-event-stream.ts`).
///
/// One parser per `event_parser` kind. The `feed`/`flush` line buffering is
/// shared; the per-kind handler decides what a parsed line means. Only the
/// `opencode` kind is ported so far — an unported kind consumes nothing and
/// each line falls through to a bounded `raw` event, matching what TS does for
/// a frame its handler does not recognise.
#[derive(Debug)]
pub struct JsonEventParser {
    kind: &'static str,
    buffer: String,
    state: JsonParserState,
}

#[derive(Debug, Default)]
struct JsonParserState {
    open_code_tool_events: OpenCodeToolEvents,
    open_code_tool_uses: HashSet<String>,
    open_code_tool_results: HashSet<String>,
    // Artifact-suppression state (parity: the four `artifact*`/
    // `suppress*ArtifactText` fields of `ParserState`). A file-writing tool
    // call is followed by the CLI echoing the written content back; the echo
    // is dropped so the artifact is not shown twice.
    suppress_next_artifact_text: bool,
    suppress_duplicate_artifact_text: bool,
    artifact_open_candidate: String,
    pending_artifact_text: String,
    antigravity_tool_uses: HashSet<String>,
    antigravity_text_emitted: bool,
    // cursor-agent turn bookkeeping (parity: `cursorTextSoFar` /
    // `cursorTurnStart`). A terminal replay carries only the current turn, so
    // recovery is measured from `cursorTurnStart`, not from the buffer start.
    cursor_text_so_far: String,
    cursor_turn_start: usize,
    // Codex bookkeeping (parity: the `codex*` fields of `ParserState`).
    codex_reasoning_emitted_by_item: HashMap<String, usize>,
    codex_reasoning_emitted_any: bool,
    codex_tool_uses: HashSet<String>,
    codex_error_emitted: bool,
    codex_previous_event_was_agent_message: bool,
    codex_last_agent_message_ended_with_newline: bool,
}

/// Whether `json-event-stream.ts` has a ported handler for `kind` (parity:
/// `createJsonEventStreamHandler(kind, …)`). An unported kind is routed to the
/// plain stdout lane by the pump rather than losing every line to a bare raw
/// event.
pub fn json_event_kind_supported(kind: &str) -> bool {
    matches!(
        kind,
        "opencode" | "gemini" | "antigravity" | "kimi" | "cursor-agent" | "codex"
    )
}

impl JsonEventParser {
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            buffer: String::new(),
            state: JsonParserState::default(),
        }
    }

    pub fn feed(&mut self, chunk: &str, on_event: &mut dyn FnMut(StreamEvent)) {
        self.buffer.push_str(chunk);
        while let Some(nl) = self.buffer.find('\n') {
            let line = self.buffer[..nl].trim().to_string();
            self.buffer.drain(..=nl);
            if line.is_empty() {
                continue;
            }
            self.handle_line(&line, on_event);
        }
    }

    pub fn flush(&mut self, on_event: &mut dyn FnMut(StreamEvent)) {
        let rem = self.buffer.trim().to_string();
        self.buffer.clear();
        if !rem.is_empty() {
            self.handle_line(&rem, on_event);
        }
        if self.kind == "opencode" {
            self.state.open_code_tool_events.flush(on_event);
        }
        flush_pending_artifact_text(&mut self.state, on_event);
    }

    fn handle_line(&mut self, line: &str, on_event: &mut dyn FnMut(StreamEvent)) {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            on_event(bound_raw_line(line));
            return;
        };
        let handled = match self.kind {
            "opencode" => self.handle_opencode(&obj, line, on_event),
            "gemini" => handle_gemini_event(&mut self.state, &obj, line, on_event),
            "antigravity" => handle_antigravity_event(&mut self.state, &obj, line, on_event),
            "kimi" => handle_kimi_event(&obj, on_event),
            "cursor-agent" => handle_cursor_event(&mut self.state, &obj, on_event),
            "codex" => handle_codex_event(&mut self.state, &obj, on_event),
            _ => false,
        };
        if handled {
            return;
        }
        on_event(bound_raw_line(line));
    }

    /// Parity: `handleOpenCodeEvent` (`json-event-stream.ts:221`).
    fn handle_opencode(
        &mut self,
        obj: &Value,
        line: &str,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> bool {
        let Some(obj) = obj.as_object() else {
            return false;
        };
        if self.state.open_code_tool_events.handle(obj, on_event) {
            return true;
        }
        let part = obj.get("part").and_then(Value::as_object);
        let obj_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

        if obj_type == "step_start" {
            // `sessionID` is OpenCode's own session handle (capture-style
            // resume). Surface it on the step-start status so the daemon can
            // persist it and replay it as `run -s <id>` next turn.
            let session_id = obj
                .get("sessionID")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            self.state
                .open_code_tool_events
                .start_session(session_id.as_deref(), on_event);
            on_event(StreamEvent::Json(json!({
                "type": "status",
                "label": "running",
                "sessionId": session_id,
            })));
            return true;
        }

        if obj_type == "text" {
            if let Some(text) = part
                .and_then(|part| part.get("text"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                on_event(StreamEvent::Json(json!({
                    "type": "text_delta",
                    "delta": text,
                })));
                return true;
            }
        }

        if obj_type == "tool_use" {
            let tool = part.and_then(|part| part.get("tool")).and_then(Value::as_str);
            let call_id = part.and_then(|part| part.get("callID")).and_then(Value::as_str);
            if let (Some(tool), Some(call_id)) = (tool, call_id) {
                self.state.open_code_tool_events.complete(call_id);
                let state_part = part
                    .and_then(|part| part.get("state"))
                    .and_then(Value::as_object);
                let key = format!(
                    "{}:{call_id}",
                    obj.get("sessionID").and_then(Value::as_str).unwrap_or("session")
                );
                if !self.state.open_code_tool_uses.contains(&key) {
                    self.state.open_code_tool_uses.insert(key.clone());
                    let raw_input = state_part.and_then(|state| state.get("input"));
                    let input = raw_input
                        .and_then(safe_parse_json)
                        .or_else(|| raw_input.cloned())
                        .unwrap_or(Value::Null);
                    on_event(StreamEvent::ToolUse {
                        id: Some(call_id.to_string()),
                        name: Some(tool.to_string()),
                        input,
                    });
                }
                let result = state_part.and_then(|state| opencode_tool_result(tool, state));
                if let Some((content, is_error)) = result {
                    if !self.state.open_code_tool_results.contains(&key) {
                        self.state.open_code_tool_results.insert(key);
                        on_event(StreamEvent::ToolResult {
                            tool_use_id: Some(call_id.to_string()),
                            content,
                            is_error,
                        });
                    }
                }
                return true;
            }
        }

        if obj_type == "step_finish" {
            let usage = part.and_then(|part| part.get("tokens")).and_then(format_opencode_usage);
            if let Some(usage) = usage {
                let mut frame = serde_json::Map::new();
                frame.insert("type".to_string(), json!("usage"));
                frame.insert("usage".to_string(), usage);
                if let Some(cost) = part
                    .and_then(|part| part.get("cost"))
                    .filter(|cost| cost.is_number())
                {
                    frame.insert("costUsd".to_string(), cost.clone());
                }
                on_event(StreamEvent::Json(Value::Object(frame)));
            }
            return true;
        }

        if obj_type == "error" {
            // OpenCode emits structured error frames on stdout (provider auth
            // failures, network errors, schema mismatches) and still exits 0.
            // Surface them as proper `error` events — not `raw`, which the chat
            // UI does not render — so the run fails visibly instead of looking
            // like a fast clean success. See issue #691.
            let fallback = obj.get("error").or_else(|| obj.get("message"));
            let message = match fallback {
                Some(value) => extract_error_message(value, "OpenCode error"),
                None => "OpenCode error".to_string(),
            };
            on_event(StreamEvent::Error {
                message,
                code: None,
                raw: Some(line.to_string()),
            });
            return true;
        }

        false
    }
}

/// Parity: `safeParseJson` (`json-event-stream.ts:52`). `None` is the TS
/// `null` result (absent, non-string, unparseable, or JSON `null`).
fn safe_parse_json(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::Object(_) | Value::Array(_) => Some(value.clone()),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(|parsed| !parsed.is_null()),
        _ => None,
    }
}

/// Parity: `stringifyContent` (`json-event-stream.ts:63`).
fn stringify_content(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Parity: `extractErrorMessage` (`json-event-stream.ts:121`).
fn extract_error_message(value: &Value, fallback: &str) -> String {
    if let Value::String(text) = value {
        if let Some(parsed) = safe_parse_json(value) {
            if parsed.is_object() {
                return extract_error_message(&parsed, text);
            }
        }
        return text.clone();
    }
    if let Value::Object(map) = value {
        if let Some(detail) = map.get("detail").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return detail.to_string();
        }
        if let Some(message) = map.get("message").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return extract_error_message(&Value::String(message.to_string()), message);
        }
        if let Some(error) = map.get("error").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return error.to_string();
        }
        if let Some(error) = map.get("error").filter(|error| error.is_object()) {
            return extract_error_message(error, fallback);
        }
        if let Some(data) = map.get("data").filter(|data| data.is_object()) {
            let data_message = extract_error_message(data, "");
            if !data_message.is_empty() {
                return data_message;
            }
        }
        if let Some(name) = map.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return name.to_string();
        }
    }
    fallback.to_string()
}

/// Parity: `formatOpenCodeUsage` (`json-event-stream.ts:166`).
fn format_opencode_usage(tokens: &Value) -> Option<Value> {
    let tokens = tokens.as_object()?;
    let mut usage = serde_json::Map::new();
    for (source, target) in [("input", "input_tokens"), ("output", "output_tokens"), ("reasoning", "thought_tokens")] {
        if let Some(value) = tokens.get(source).filter(|value| value.is_number()) {
            usage.insert(target.to_string(), value.clone());
        }
    }
    if let Some(cache) = tokens.get("cache").and_then(Value::as_object) {
        if let Some(read) = cache.get("read").filter(|value| value.is_number()) {
            usage.insert("cached_read_tokens".to_string(), read.clone());
        }
        if let Some(write) = cache.get("write").filter(|value| value.is_number()) {
            usage.insert("cached_write_tokens".to_string(), write.clone());
        }
    }
    if usage.is_empty() {
        return None;
    }
    Some(Value::Object(usage))
}

/// Parity: `isPowerShellErrorRecord` (`json-event-stream.ts:179`). A
/// non-terminating PowerShell error can leave the shell at exit 0, so both
/// canonical `ErrorRecord` fields must be present as their own `+`-prefixed
/// lines before the output counts as an error.
fn is_powershell_error_record(tool_name: &str, output: Option<&Value>) -> bool {
    let normalized = tool_name.to_lowercase();
    if normalized != "bash" && normalized != "shell" {
        return false;
    }
    let Some(output) = output.and_then(Value::as_str) else {
        return false;
    };
    static CATEGORY: OnceLock<Regex> = OnceLock::new();
    static FULLY_QUALIFIED: OnceLock<Regex> = OnceLock::new();
    let category =
        CATEGORY.get_or_init(|| Regex::new(r"(?:\A|\r?\n)\s*\+\s*CategoryInfo\s*:").expect("PowerShell CategoryInfo regex"));
    let fully_qualified = FULLY_QUALIFIED.get_or_init(|| {
        Regex::new(r"(?:\A|\r?\n)\s*\+\s*FullyQualifiedErrorId\s*:").expect("PowerShell FullyQualifiedErrorId regex")
    });
    category.is_match(output) && fully_qualified.is_match(output)
}

/// Parity: `openCodeToolResult` (`json-event-stream.ts:193`).
fn opencode_tool_result(
    tool_name: &str,
    state_part: &serde_json::Map<String, Value>,
) -> Option<(String, bool)> {
    let status = state_part
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    if status != "completed" && status != "error" && status != "failed" {
        return None;
    }
    let metadata = state_part.get("metadata").filter(|value| value.is_object());
    let exit_codes = [
        state_part.get("exit"),
        state_part.get("exitCode"),
        metadata.and_then(|metadata| metadata.get("exit")),
    ];
    let has_non_zero_exit = exit_codes
        .iter()
        .flatten()
        .any(|exit| exit.is_number() && exit.as_f64().is_some_and(|code| code != 0.0));
    let explicit_error = match state_part.get("error") {
        Some(Value::String(text)) if !text.trim().is_empty() => state_part.get("error").cloned(),
        Some(Value::Object(map)) if !map.is_empty() => state_part.get("error").cloned(),
        _ => None,
    };
    let is_error = status == "error"
        || status == "failed"
        || explicit_error.is_some()
        || has_non_zero_exit
        || is_powershell_error_record(tool_name, state_part.get("output"));
    let content = stringify_content(explicit_error.as_ref().or_else(|| state_part.get("output")));
    Some((content, is_error))
}

#[cfg(test)]
mod qoder_tests {
    use super::*;

    fn collect(feed: &str) -> Vec<StreamEvent> {
        let mut parser = QoderParser::new();
        let mut events = Vec::new();
        parser.feed(feed, &mut |event| events.push(event));
        parser.flush(&mut |event| events.push(event));
        events
    }

    fn lane(events: &[StreamEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                StreamEvent::Status { .. } => "status",
                StreamEvent::ThinkingDelta { .. } => "thinking_delta",
                StreamEvent::TextDelta { .. } => "text_delta",
                StreamEvent::ToolUse { .. } => "tool_use",
                StreamEvent::ToolResult { .. } => "tool_result",
                StreamEvent::Usage { .. } => "usage",
                StreamEvent::TurnEnd { .. } => "turn_end",
                StreamEvent::ThinkingStart => "thinking_start",
                StreamEvent::ThinkingTokens { .. } => "thinking_tokens",
                StreamEvent::ToolInputDelta { .. } => "tool_input_delta",
                StreamEvent::RequestUsage { .. } => "request_usage",
                StreamEvent::Error { .. } => "error",
                StreamEvent::Raw { .. } => "raw",
                StreamEvent::ToolInFlight { .. } => "tool_in_flight",
                StreamEvent::Json(_) => "json",
            })
            .collect()
    }

    #[test]
    fn system_init_maps_to_status_with_session_and_cli_version() {
        let events = collect(
            "{\"type\":\"system\",\"subtype\":\"init\",\"qodercli_version\":\"0.2.6\",\"model\":\"auto\",\"session_id\":\"session-1\"}\n",
        );
        assert_eq!(lane(&events), vec!["status"]);
        let json = events[0].to_json();
        assert_eq!(json["label"], "initializing");
        assert_eq!(json["model"], "auto");
        assert_eq!(json["sessionId"], "session-1");
        assert_eq!(json["qodercliVersion"], "0.2.6");
    }

    #[test]
    fn system_init_omits_absent_keys_instead_of_nulling_them() {
        // TS: `model: typeof obj.model === 'string' ? obj.model : undefined` —
        // an absent field drops the key, it does not become null.
        let events = collect("{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"session-1\"}\n");
        let json = events[0].to_json();
        assert_eq!(json["label"], "initializing");
        assert_eq!(json["sessionId"], "session-1");
        assert!(json.get("model").is_none(), "json: {json}");
        assert!(json.get("qodercliVersion").is_none(), "json: {json}");
    }

    #[test]
    fn assistant_content_blocks_map_to_text_and_thinking() {
        let events = collect(concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Hello\"},{\"type\":\"text\",\"text\":\" world\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"Considering.\"}]}}\n",
        ));
        assert_eq!(
            lane(&events),
            vec![
                "text_delta",
                "text_delta",
                "thinking_start",
                "thinking_delta"
            ]
        );
        assert_eq!(events[0].to_json()["delta"], "Hello");
        assert_eq!(events[1].to_json()["delta"], " world");
        assert_eq!(events[3].to_json()["delta"], "Considering.");
    }

    #[test]
    fn assistant_thinking_start_is_emitted_once_per_stream() {
        let events = collect(concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"one\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"two\"}]}}\n",
        ));
        assert_eq!(
            lane(&events),
            vec!["thinking_start", "thinking_delta", "thinking_delta"]
        );
    }

    #[test]
    fn assistant_string_content_maps_to_text_delta() {
        let events = collect("{\"type\":\"assistant\",\"message\":{\"content\":\"plain\"}}\n");
        assert_eq!(lane(&events), vec!["text_delta"]);
        assert_eq!(events[0].to_json()["delta"], "plain");
    }

    #[test]
    fn assistant_error_without_text_maps_to_error_with_raw() {
        let line = "{\"type\":\"assistant\",\"message\":{\"content\":[]},\"error\":{\"message\":\"Qoder authentication expired\"}}";
        let events = collect(&format!("{line}\n"));
        assert_eq!(lane(&events), vec!["error"]);
        let json = events[0].to_json();
        assert_eq!(json["message"], "Qoder authentication expired");
        assert_eq!(json["raw"], line);
    }

    #[test]
    fn assistant_error_without_detail_uses_fallback_message() {
        let events = collect(
            "{\"type\":\"assistant\",\"message\":{\"content\":[]},\"error\":{\"code\":\"E_QODER\"}}\n",
        );
        assert_eq!(lane(&events), vec!["error"]);
        assert_eq!(events[0].to_json()["message"], "Unknown Qoder error");
    }

    #[test]
    fn assistant_text_wins_over_a_trailing_error() {
        let events = collect(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Partial answer\"}]},\"error\":{\"message\":\"Trailing Qoder warning\"}}\n",
        );
        assert_eq!(lane(&events), vec!["text_delta"]);
    }

    #[test]
    fn result_maps_usage_fields_and_preserves_model_usage() {
        let events = collect(
            "{\"type\":\"result\",\"subtype\":\"success\",\"duration_ms\":10864,\"is_error\":false,\"stop_reason\":\"end_turn\",\"total_cost_usd\":0,\"usage\":{\"input_tokens\":10,\"output_tokens\":2},\"modelUsage\":{\"auto\":{\"costUSD\":0}}}\n",
        );
        assert_eq!(lane(&events), vec!["usage"]);
        let json = events[0].to_json();
        assert_eq!(json["stopReason"], "end_turn");
        assert_eq!(json["durationMs"], 10864);
        assert_eq!(json["costUsd"], 0);
        assert_eq!(json["isError"], false);
        assert_eq!(json["modelUsage"]["auto"]["costUSD"], 0);
        assert_eq!(json["usage"]["input_tokens"], 10);
    }

    #[test]
    fn result_is_error_maps_usage_then_error() {
        let line = "{\"type\":\"result\",\"subtype\":\"error\",\"duration_ms\":10864,\"is_error\":true,\"stop_reason\":\"tool_use_failed\",\"total_cost_usd\":0,\"usage\":{\"input_tokens\":10}}";
        let events = collect(&format!("{line}\n"));
        assert_eq!(lane(&events), vec!["usage", "error"]);
        assert_eq!(events[0].to_json()["isError"], true);
        let json = events[1].to_json();
        assert_eq!(json["message"], "Qoder run failed: tool_use_failed");
        assert_eq!(json["raw"], line);
    }

    #[test]
    fn unknown_and_malformed_lines_become_raw_events() {
        let events = collect("{\"type\":\"unknown\",\"value\":1}\nnot json\n");
        assert_eq!(lane(&events), vec!["raw", "raw"]);
        assert_eq!(events[0].to_json()["line"], "{\"type\":\"unknown\",\"value\":1}");
        assert_eq!(events[1].to_json()["line"], "not json");
    }

    #[test]
    fn trailing_line_without_newline_is_flushed() {
        let mut parser = QoderParser::new();
        let mut events = Vec::new();
        parser.feed(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"tail\"}]}}",
            &mut |event| events.push(event),
        );
        assert!(events.is_empty(), "no newline yet");
        parser.flush(&mut |event| events.push(event));
        assert_eq!(lane(&events), vec!["text_delta"]);
    }

    #[test]
    fn multibyte_text_survives_chunk_boundaries() {
        // The bytes are split mid-character across two chunks; the Rust string
        // API only sees valid UTF-8 boundaries, so the assembled line must
        // still decode the full text.
        let mut parser = QoderParser::new();
        let mut events = Vec::new();
        let line = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"\u{4f60}\u{597d}\"}]}}\n";
        let (a, b) = line.split_at(line.find('\u{4f60}').unwrap() + 3);
        parser.feed(a, &mut |event| events.push(event));
        parser.feed(b, &mut |event| events.push(event));
        assert_eq!(lane(&events), vec!["text_delta"]);
        assert_eq!(events[0].to_json()["delta"], "\u{4f60}\u{597d}");
    }
}

#[cfg(test)]
mod claude_tests {
    use super::*;

    fn collect(feed: &str) -> Vec<StreamEvent> {
        let mut parser = ClaudeParser::new();
        let mut events = Vec::new();
        parser.feed(feed, &mut |event| events.push(event));
        parser.flush(&mut |_| {});
        events
    }

    fn lane(events: &[StreamEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                StreamEvent::Status { .. } => "status",
                StreamEvent::ThinkingDelta { .. } => "thinking_delta",
                StreamEvent::TextDelta { .. } => "text_delta",
                StreamEvent::ToolUse { .. } => "tool_use",
                StreamEvent::ToolResult { .. } => "tool_result",
                StreamEvent::Usage { .. } => "usage",
                StreamEvent::TurnEnd { .. } => "turn_end",
                StreamEvent::ThinkingStart => "thinking_start",
                StreamEvent::ThinkingTokens { .. } => "thinking_tokens",
                StreamEvent::ToolInputDelta { .. } => "tool_input_delta",
                StreamEvent::RequestUsage { .. } => "request_usage",
                StreamEvent::Error { .. } => "error",
                StreamEvent::Raw { .. } => "raw",
                StreamEvent::ToolInFlight { .. } => "tool_in_flight",
                StreamEvent::Json(_) => "json",
            })
            .collect()
    }

    #[test]
    fn status_frames_carry_exactly_the_keys_typescript_writes() {
        // TS `system/init` writes `model: obj.model ?? null` and
        // `sessionId: obj.session_id ?? null`: both keys present, null when the
        // wire had nothing.
        let events = collect("{\"type\":\"system\",\"subtype\":\"init\"}\n");
        assert_eq!(
            events[0].to_json(),
            json!({"type": "status", "label": "initializing", "model": null, "sessionId": null})
        );
        // A `system/status` frame has neither key.
        let events = collect("{\"type\":\"system\",\"subtype\":\"status\",\"status\":\"working\"}\n");
        assert_eq!(
            events[0].to_json(),
            json!({"type": "status", "label": "working"})
        );
        // The TTFT status is a status frame, not a raw line.
        let events = collect(
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"id\":\"m1\"},\"ttft_ms\":1234}}\n",
        );
        assert_eq!(
            events[0].to_json(),
            json!({"type": "status", "label": "streaming", "ttftMs": 1234})
        );
    }

    #[test]
    fn streamed_turn_maps_deltas_tool_use_and_turn_end() {
        let feed = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"sonnet\",\"session_id\":\"s1\"}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_stop\",\"index\":0}}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{}}}}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\": \\\"\"}}}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"ls\\\"}\"}}}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_stop\",\"index\":1}}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}}\n",
        );
        let events = collect(feed);
        assert_eq!(
            lane(&events),
            vec![
                "status",
                "text_delta",
                "tool_input_delta",
                "tool_input_delta",
                "tool_use",
                "turn_end",
            ]
        );
        match &events[4] {
            StreamEvent::ToolUse { id, name, input } => {
                assert_eq!(id.as_deref(), Some("t1"));
                assert_eq!(name.as_deref(), Some("Bash"));
                assert_eq!(input["command"], serde_json::json!("ls"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn legacy_wrapper_covers_text_and_result_without_deltas() {
        let feed = concat!(
            "{\"type\":\"assistant\",\"message\":{\"id\":\"m9\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}],\"usage\":{\"input_tokens\":3,\"output_tokens\":5}}}\n",
            "{\"type\":\"result\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":3},\"total_cost_usd\":0.01,\"duration_ms\":7}\n",
        );
        let events = collect(feed);
        assert_eq!(
            lane(&events),
            vec!["request_usage", "text_delta", "usage"]
        );
        match &events[2] {
            StreamEvent::Usage { stop_reason, .. } => assert_eq!(stop_reason, "end_turn"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn error_result_maps_to_error_event() {
        let events = collect("{\"type\":\"result\",\"is_error\":true,\"subtype\":\"error_during_execution\",\"errors\":[\"boom\"]}\n");
        assert_eq!(lane(&events), vec!["usage", "error"]);
        match &events[1] {
            StreamEvent::Error { message, code, .. } => {
                assert_eq!(message, "boom");
                assert_eq!(code.as_deref(), Some("error_during_execution"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// `String(value)` for the JSON scalars a frame carries (parity: the
/// `String(x ?? '')` casts in `json-event-stream.ts`).
fn stringify_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Parity: `normalizeTodoStatus` (`json-event-stream.ts:587`).
fn normalize_todo_status(value: Option<&Value>) -> String {
    let status = match value.and_then(Value::as_str) {
        Some(text) => {
            static SEPARATORS: OnceLock<Regex> = OnceLock::new();
            let separators = SEPARATORS.get_or_init(|| Regex::new(r"[-\s]+").expect("todo status regex"));
            separators
                .replace_all(&text.trim().to_lowercase(), "_")
                .to_string()
        }
        None => String::new(),
    };
    if matches!(status.as_str(), "completed" | "complete" | "done") || status.starts_with("completed") {
        return "completed".to_string();
    }
    if matches!(status.as_str(), "in_progress" | "doing" | "active") || status.starts_with("in_progress") {
        return "in_progress".to_string();
    }
    if matches!(
        status.as_str(),
        "stopped" | "failed" | "blocked" | "canceled" | "cancelled"
    ) || status.starts_with("stopped")
        || status.starts_with("failed")
        || status.starts_with("blocked")
        || status.starts_with("canceled")
        || status.starts_with("cancelled")
    {
        return "stopped".to_string();
    }
    "pending".to_string()
}

/// Parity: `todoWriteInputFromItems` (`json-event-stream.ts:614`).
fn todo_write_input_from_items(items: &Value) -> Option<Value> {
    let items = items.as_array()?;
    let mut todos = Vec::new();
    for raw in items {
        let Some(raw) = raw.as_object() else { continue };
        let content = raw
            .get("content")
            .and_then(Value::as_str)
            .or_else(|| raw.get("label").and_then(Value::as_str))
            .or_else(|| raw.get("description").and_then(Value::as_str))
            .or_else(|| raw.get("text").and_then(Value::as_str))
            .unwrap_or("");
        if content.is_empty() {
            continue;
        }
        let status = if raw.get("completed").and_then(Value::as_bool) == Some(true) {
            "completed".to_string()
        } else {
            normalize_todo_status(raw.get("status"))
        };
        todos.push(json!({ "content": content, "status": status }));
    }
    if todos.is_empty() {
        return None;
    }
    Some(json!({ "todos": todos }))
}

/// Parity: `todoWriteInputFromParsedValue` (`json-event-stream.ts:640`).
fn todo_write_input_from_parsed_value(value: &Value) -> Option<Value> {
    if value.is_array() {
        return todo_write_input_from_items(value);
    }
    let value = value.as_object()?;
    for key in ["todos", "todo"] {
        if let Some(items) = value.get(key).filter(|items| items.is_array()) {
            return todo_write_input_from_items(items);
        }
    }
    None
}

/// Parity: `artifactOpenCandidateLength` (`json-event-stream.ts:686`) — how
/// many trailing bytes could be the start of the artifact open tag.
fn artifact_open_candidate_length(text: &str, open_tag: &str) -> usize {
    let max = (open_tag.len() - 1).min(text.len());
    let mut len = max;
    while len > 0 {
        if text.is_char_boundary(text.len() - len) && open_tag.starts_with(&text[text.len() - len..]) {
            return len;
        }
        len -= 1;
    }
    0
}

/// Parity: `stripDuplicateArtifactText` (`json-event-stream.ts:648`) — drop the
/// text a file-writing tool already wrote to disk.
fn strip_duplicate_artifact_text(text: &str, state: &mut JsonParserState) -> String {
    const OPEN_TAG: &str = "<artifact";
    const CLOSE_TAG: &str = "</artifact>";
    if !state.suppress_next_artifact_text
        && !state.suppress_duplicate_artifact_text
        && state.artifact_open_candidate.is_empty()
    {
        return text.to_string();
    }
    let current = format!("{}{}", state.artifact_open_candidate, text);
    state.artifact_open_candidate = String::new();
    if state.suppress_duplicate_artifact_text {
        let Some(close_index) = current.find(CLOSE_TAG) else {
            return String::new();
        };
        state.suppress_duplicate_artifact_text = false;
        state.suppress_next_artifact_text = false;
        return strip_duplicate_artifact_text(&current[close_index + CLOSE_TAG.len()..], state);
    }
    let Some(open_index) = current.find(OPEN_TAG) else {
        let candidate_length = artifact_open_candidate_length(&current, OPEN_TAG);
        if state.suppress_next_artifact_text && candidate_length > 0 {
            state.artifact_open_candidate = current[current.len() - candidate_length..].to_string();
            return current[..current.len() - candidate_length].to_string();
        }
        if state.suppress_next_artifact_text {
            state.suppress_next_artifact_text = false;
        }
        return current;
    };
    state.suppress_duplicate_artifact_text = true;
    state.suppress_next_artifact_text = false;
    let prefix = format!("{}{}", state.pending_artifact_text, &current[..open_index]);
    state.pending_artifact_text = String::new();
    format!(
        "{}{}",
        prefix,
        strip_duplicate_artifact_text(&current[open_index..], state)
    )
}

/// Parity: `flushPendingArtifactText` (`json-event-stream.ts:694`).
fn flush_pending_artifact_text(state: &mut JsonParserState, on_event: &mut dyn FnMut(StreamEvent)) {
    let delta = format!("{}{}", state.pending_artifact_text, state.artifact_open_candidate);
    if delta.is_empty() {
        return;
    }
    state.pending_artifact_text = String::new();
    state.artifact_open_candidate = String::new();
    state.suppress_next_artifact_text = false;
    on_event(StreamEvent::Json(json!({ "type": "text_delta", "delta": delta })));
}

/// Parity: `isFileWriteToolUse` (`json-event-stream.ts:703`).
fn is_file_write_tool_use(tool_name: &str, input: Option<&Value>) -> bool {
    let Some(input) = input.and_then(Value::as_object) else {
        return false;
    };
    let path = input
        .get("file_path")
        .and_then(Value::as_str)
        .or_else(|| input.get("path").and_then(Value::as_str))
        .unwrap_or("");
    if !matches!(tool_name, "write_file" | "write" | "replace" | "edit") {
        return false;
    }
    static SOURCE_FILE: OnceLock<Regex> = OnceLock::new();
    let source_file = SOURCE_FILE.get_or_init(|| {
        Regex::new(r"(?i)\.(html|htm|css|js|jsx|ts|tsx|md)$").expect("file-write extension regex")
    });
    if source_file.is_match(path) {
        return true;
    }
    input.get("content").and_then(Value::as_str).is_some()
        || input.get("new_string").and_then(Value::as_str).is_some()
}

/// Parity: the frame text TS re-serializes for a `raw` field
/// (`stringifyContent(obj)`). We echo the original line instead: for the
/// compact frames these wires emit the two are byte-identical, and echoing
/// cannot reorder keys the way a map round-trip can.
fn frame_raw(line: &str) -> Option<String> {
    Some(line.to_string())
}

/// Parity: `handleGeminiEvent` (`json-event-stream.ts:307`).
fn handle_gemini_event(
    state: &mut JsonParserState,
    obj: &Value,
    line: &str,
    on_event: &mut dyn FnMut(StreamEvent),
) -> bool {
    let Some(obj) = obj.as_object() else {
        return false;
    };
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");
    let role = obj.get("role").and_then(Value::as_str).unwrap_or("");
    let content = obj.get("content").and_then(Value::as_str);
    let is_assistant_text =
        ty == "message" && role == "assistant" && content.is_some_and(|text| !text.is_empty());
    if !is_assistant_text {
        flush_pending_artifact_text(state, on_event);
    }

    if ty == "init" {
        let mut frame = serde_json::Map::new();
        frame.insert("type".to_string(), json!("status"));
        frame.insert("label".to_string(), json!("initializing"));
        if let Some(model) = obj.get("model").and_then(Value::as_str) {
            frame.insert("model".to_string(), json!(model));
        }
        on_event(StreamEvent::Json(Value::Object(frame)));
        return true;
    }

    if ty == "message" && role == "user" {
        return true;
    }

    if is_assistant_text {
        let delta = strip_duplicate_artifact_text(content.unwrap_or(""), state);
        if !delta.is_empty() {
            on_event(StreamEvent::Json(json!({ "type": "text_delta", "delta": delta })));
        }
        return true;
    }

    if ty == "tool_use" {
        let tool_id = obj.get("tool_id").and_then(Value::as_str);
        let tool_name = obj.get("tool_name").and_then(Value::as_str);
        if let (Some(tool_id), Some(tool_name)) = (tool_id, tool_name) {
            let parameters = obj.get("parameters");
            let input = parameters
                .and_then(safe_parse_json)
                .or_else(|| parameters.cloned())
                .unwrap_or(Value::Null);
            if tool_name == "write_todos" {
                if let Some(todo_input) = todo_write_input_from_parsed_value(&input) {
                    on_event(StreamEvent::Json(json!({
                        "type": "tool_use",
                        "id": format!("{tool_id}:todo-native"),
                        "name": "TodoWrite",
                        "input": todo_input,
                    })));
                    return true;
                }
            }
            if is_file_write_tool_use(tool_name, Some(&input)) {
                state.suppress_next_artifact_text = true;
            }
            on_event(StreamEvent::Json(json!({
                "type": "tool_use",
                "id": tool_id,
                "name": tool_name,
                "input": input,
            })));
            return true;
        }
    }

    if ty == "tool_result" {
        if let Some(tool_id) = obj.get("tool_id").and_then(Value::as_str) {
            let error = obj.get("error").filter(|error| error.is_object());
            let error_message = error.map(|error| extract_error_message(error, "")).unwrap_or_default();
            let output = match obj.get("output") {
                Some(Value::String(text)) => text.clone(),
                _ if !error_message.is_empty() => error_message,
                other => stringify_content(other),
            };
            on_event(StreamEvent::Json(json!({
                "type": "tool_result",
                "toolUseId": tool_id,
                "content": output,
                "isError": obj.get("status").and_then(Value::as_str) == Some("error") || error.is_some(),
            })));
            return true;
        }
    }

    if ty == "error" {
        let severity = obj
            .get("severity")
            .and_then(Value::as_str)
            .map(str::to_lowercase)
            .unwrap_or_default();
        let fallback = if severity == "warning" {
            "Gemini CLI warning"
        } else {
            "Gemini CLI error"
        };
        let source = obj.get("message").or_else(|| obj.get("error")).unwrap_or(&Value::Null);
        let message = extract_error_message(source, fallback);
        if severity == "warning" {
            on_event(StreamEvent::Json(
                json!({ "type": "status", "label": "warning", "detail": message }),
            ));
        } else {
            on_event(StreamEvent::Error {
                message,
                code: None,
                raw: frame_raw(line),
            });
        }
        return true;
    }

    if ty == "result" {
        let failed = obj.get("status").and_then(Value::as_str) == Some("error")
            || obj.get("error").is_some_and(|error| error.is_object());
        if failed {
            on_event(StreamEvent::Error {
                message: extract_error_message(obj.get("error").unwrap_or(&Value::Null), "Gemini CLI error"),
                code: None,
                raw: frame_raw(line),
            });
            return true;
        }
        let Some(stats) = obj.get("stats").filter(|stats| stats.is_object()) else {
            return true;
        };
        let mut usage = serde_json::Map::new();
        for (source, target) in [
            ("input_tokens", "input_tokens"),
            ("output_tokens", "output_tokens"),
            ("cached", "cached_read_tokens"),
        ] {
            if let Some(value) = stats.get(source).filter(|value| value.is_number()) {
                usage.insert(target.to_string(), value.clone());
            }
        }
        let mut frame = serde_json::Map::new();
        frame.insert("type".to_string(), json!("usage"));
        frame.insert("usage".to_string(), Value::Object(usage));
        if let Some(duration) = stats.get("duration_ms").filter(|value| value.is_number()) {
            frame.insert("durationMs".to_string(), duration.clone());
        }
        on_event(StreamEvent::Json(Value::Object(frame)));
        return true;
    }

    false
}

/// Parity: `handleAntigravityEvent` (`json-event-stream.ts:435`). Frames
/// without an `event` discriminator fall back to the Gemini-CLI JSONL branch,
/// which some agy-compatible builds print.
fn handle_antigravity_event(
    state: &mut JsonParserState,
    obj: &Value,
    line: &str,
    on_event: &mut dyn FnMut(StreamEvent),
) -> bool {
    let Some(obj) = obj.as_object() else {
        return false;
    };
    let Some(event) = obj.get("event").and_then(Value::as_str) else {
        return handle_gemini_event(state, &Value::Object(obj.clone()), line, on_event);
    };

    if event == "init" {
        on_event(StreamEvent::Json(json!({
            "type": "status",
            "label": "initializing",
            "sessionId": obj.get("conversation_id").and_then(Value::as_str),
        })));
        return true;
    }

    if event == "step_update" {
        let Some(step) = obj.get("step_update").and_then(Value::as_object) else {
            return false;
        };
        let step_type = step.get("step_type").and_then(Value::as_str).unwrap_or("");
        if step_type == "agent_response" {
            if let Some(delta) = step
                .get("text_delta")
                .and_then(Value::as_str)
                .filter(|delta| !delta.is_empty())
            {
                state.antigravity_text_emitted = true;
                on_event(StreamEvent::Json(json!({ "type": "text_delta", "delta": delta })));
            }
            return true;
        }
        if step_type == "tool" {
            let empty = serde_json::Map::new();
            let tool_info = step
                .get("tool_info")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            let name = tool_info
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| step.get("tool_name").and_then(Value::as_str))
                .unwrap_or("tool");
            let conversation = step
                .get("conversation_id")
                .filter(|value| !value.is_null())
                .or_else(|| obj.get("conversation_id").filter(|value| !value.is_null()))
                .map(stringify_scalar)
                .unwrap_or_default();
            let step_index = step
                .get("step_index")
                .filter(|value| !value.is_null())
                .map(stringify_scalar)
                .unwrap_or_default();
            let id = format!("{conversation}:{step_index}");
            if !state.antigravity_tool_uses.contains(&id) {
                state.antigravity_tool_uses.insert(id.clone());
                on_event(StreamEvent::Json(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": tool_info.get("parameters").cloned().unwrap_or(Value::Null),
                })));
            }
            let step_state = step.get("state").and_then(Value::as_str).unwrap_or("");
            if step_state == "DONE" || step_state == "ERROR" {
                let content = match tool_info.get("output") {
                    Some(Value::String(text)) => text.clone(),
                    other => stringify_content(other),
                };
                on_event(StreamEvent::Json(json!({
                    "type": "tool_result",
                    "toolUseId": id,
                    "content": content,
                    "isError": step_state == "ERROR",
                })));
            }
            return true;
        }
        // user_input echoes the prompt; checkpoint / subagent / error_message
        // carry no user-visible payload (an error's message arrives on the
        // terminal result).
        return true;
    }

    if event == "result" {
        let Some(result) = obj.get("result").and_then(Value::as_object) else {
            return false;
        };
        if result.get("status").and_then(Value::as_str) != Some("SUCCESS") {
            let message = result
                .get("error")
                .and_then(Value::as_str)
                .filter(|error| !error.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "Antigravity run ended with status {}",
                        result
                            .get("status")
                            .filter(|status| !status.is_null())
                            .map(stringify_scalar)
                            .unwrap_or_else(|| "unknown".to_string())
                    )
                });
            on_event(StreamEvent::Error {
                message,
                code: None,
                raw: frame_raw(line),
            });
            return true;
        }
        if !state.antigravity_text_emitted {
            if let Some(response) = result
                .get("response")
                .and_then(Value::as_str)
                .filter(|response| !response.is_empty())
            {
                state.antigravity_text_emitted = true;
                on_event(StreamEvent::Json(
                    json!({ "type": "text_delta", "delta": response }),
                ));
            }
        }
        let empty = serde_json::Map::new();
        let raw_usage = result.get("usage").and_then(Value::as_object).unwrap_or(&empty);
        let mut usage = serde_json::Map::new();
        for (source, target) in [
            ("input_tokens", "input_tokens"),
            ("output_tokens", "output_tokens"),
            ("thinking_tokens", "thought_tokens"),
            ("cache_read_tokens", "cached_read_tokens"),
        ] {
            if let Some(value) = raw_usage.get(source).filter(|value| value.is_number()) {
                usage.insert(target.to_string(), value.clone());
            }
        }
        let mut frame = serde_json::Map::new();
        frame.insert("type".to_string(), json!("usage"));
        frame.insert("usage".to_string(), Value::Object(usage));
        if let Some(seconds) = result
            .get("duration_seconds")
            .filter(|value| value.is_number())
            .and_then(Value::as_f64)
        {
            frame.insert("durationMs".to_string(), json!((seconds * 1000.0).round() as i64));
        }
        on_event(StreamEvent::Json(Value::Object(frame)));
        return true;
    }

    false
}

/// Parity: `handleKimiEvent` (`json-event-stream.ts:528`).
fn handle_kimi_event(obj: &Value, on_event: &mut dyn FnMut(StreamEvent)) -> bool {
    let Some(obj) = obj.as_object() else {
        return false;
    };
    let role = obj.get("role").and_then(Value::as_str).unwrap_or("");

    if role == "assistant" {
        if let Some(tool_calls) = obj.get("tool_calls").filter(|calls| calls.is_array()) {
            for raw_call in tool_calls.as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let Some(call) = raw_call.as_object() else { continue };
                let function = call.get("function").and_then(Value::as_object);
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                let name = function
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty());
                let (Some(id), Some(name)) = (id, name) else { continue };
                let arguments = function.and_then(|function| function.get("arguments"));
                let input = arguments
                    .and_then(safe_parse_json)
                    .or_else(|| arguments.cloned())
                    .unwrap_or(Value::Null);
                on_event(StreamEvent::Json(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                })));
            }
            return true;
        }
    }

    if role == "tool" {
        if let Some(call_id) = obj
            .get("tool_call_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|call_id| !call_id.is_empty())
        {
            on_event(StreamEvent::Json(json!({
                "type": "tool_result",
                "toolUseId": call_id,
                "content": stringify_content(obj.get("content")),
                "isError": false,
            })));
            return true;
        }
    }

    if role == "assistant" {
        if let Some(content) = obj.get("content").and_then(Value::as_str).filter(|text| !text.is_empty()) {
            on_event(StreamEvent::Json(json!({ "type": "text_delta", "delta": content })));
            return true;
        }
    }

    if role == "meta" && obj.get("type").and_then(Value::as_str) == Some("session.resume_hint") {
        return true;
    }

    false
}

/// JavaScript truthiness for a JSON value, for the handful of TS guards that
/// test a field directly rather than by type (`if (obj.message)`).
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|number| number != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        // Objects and arrays are always truthy in JS.
        _ => true,
    }
}

/// Parity: `extractCursorText` (`json-event-stream.ts:578`) — concatenate the
/// `text` blocks of an assistant message.
fn extract_cursor_text(message: &Value) -> String {
    let Some(blocks) = message
        .as_object()
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return String::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            let block = block.as_object()?;
            if block.get("type").and_then(Value::as_str) != Some("text") {
                return None;
            }
            block.get("text").and_then(Value::as_str)
        })
        .collect()
}

/// Parity: `emitCursorTextDelta` (`json-event-stream.ts:737`). Timestamped
/// assistant events without a `model_call_id` are cursor-agent's real-time
/// incremental deltas; each is emitted verbatim, never deduped by content,
/// because a legitimately repeated fragment is real output.
fn emit_cursor_text_delta(
    text: &str,
    state: &mut JsonParserState,
    on_event: &mut dyn FnMut(StreamEvent),
) {
    if text.is_empty() {
        return;
    }
    state.cursor_text_so_far.push_str(text);
    on_event(StreamEvent::Json(json!({ "type": "text_delta", "delta": text })));
}

/// Parity: `reconcileCursorTurnReplay` (`json-event-stream.ts:767`). A terminal
/// replay carries the full text for the CURRENT turn, so it is compared against
/// `cursorTextSoFar[cursorTurnStart..]`; only a verified prefix permits suffix
/// recovery, and the turn boundary always advances.
fn reconcile_cursor_turn_replay(
    text: &str,
    state: &mut JsonParserState,
    on_event: &mut dyn FnMut(StreamEvent),
) {
    let suffix = {
        let emitted_turn = &state.cursor_text_so_far[state.cursor_turn_start..];
        if !text.is_empty() && text != emitted_turn && text.starts_with(emitted_turn) {
            Some(text[emitted_turn.len()..].to_string())
        } else {
            None
        }
    };
    if let Some(suffix) = suffix {
        if !suffix.is_empty() {
            on_event(StreamEvent::Json(json!({ "type": "text_delta", "delta": suffix })));
        }
        state.cursor_text_so_far.push_str(&suffix);
    }
    state.cursor_turn_start = state.cursor_text_so_far.len();
}

/// Parity: `handleCursorEvent` (`json-event-stream.ts:777`).
fn handle_cursor_event(
    state: &mut JsonParserState,
    obj: &Value,
    on_event: &mut dyn FnMut(StreamEvent),
) -> bool {
    let Some(obj) = obj.as_object() else {
        return false;
    };
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");

    if ty == "system" && obj.get("subtype").and_then(Value::as_str) == Some("init") {
        let mut frame = serde_json::Map::new();
        frame.insert("type".to_string(), json!("status"));
        frame.insert("label".to_string(), json!("initializing"));
        if let Some(model) = obj.get("model").and_then(Value::as_str) {
            frame.insert("model".to_string(), json!(model));
        }
        on_event(StreamEvent::Json(Value::Object(frame)));
        return true;
    }

    if ty == "assistant" && obj.get("message").is_some_and(js_truthy) {
        let message = obj.get("message").unwrap_or(&Value::Null);
        // Cursor sends a final assistant message that replays the full text for
        // the current turn — either tagged with `model_call_id`, or (fallback)
        // as a non-timestamped terminal assistant message. Both are reconciled
        // against the current turn's emitted text.
        if obj.get("model_call_id").is_some_and(Value::is_string) {
            let text = extract_cursor_text(message);
            reconcile_cursor_turn_replay(&text, state, on_event);
            return true;
        }
        let text = extract_cursor_text(message);
        if text.is_empty() {
            return false;
        }
        if obj.get("timestamp_ms").is_some_and(Value::is_number) {
            // Incremental streaming chunk within a turn.
            emit_cursor_text_delta(&text, state, on_event);
            return true;
        }
        reconcile_cursor_turn_replay(&text, state, on_event);
        return true;
    }

    if ty == "result" && obj.get("usage").is_some_and(Value::is_object) {
        let empty = serde_json::Map::new();
        let raw_usage = obj.get("usage").and_then(Value::as_object).unwrap_or(&empty);
        let mut usage = serde_json::Map::new();
        for (source, target) in [
            ("inputTokens", "input_tokens"),
            ("outputTokens", "output_tokens"),
            ("cacheReadTokens", "cached_read_tokens"),
            ("cacheWriteTokens", "cached_write_tokens"),
        ] {
            if let Some(value) = raw_usage.get(source).filter(|value| value.is_number()) {
                usage.insert(target.to_string(), value.clone());
            }
        }
        let mut frame = serde_json::Map::new();
        frame.insert("type".to_string(), json!("usage"));
        frame.insert("usage".to_string(), Value::Object(usage));
        if let Some(duration) = obj.get("duration_ms").filter(|value| value.is_number()) {
            frame.insert("durationMs".to_string(), duration.clone());
        }
        on_event(StreamEvent::Json(Value::Object(frame)));
        return true;
    }

    false
}

/// Parity: `parseJsonObjectsFromContent` (`json-event-stream.ts:73`).
fn parse_json_objects_from_content(value: &str) -> Vec<Value> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let direct = safe_parse_json(&Value::String(trimmed.to_string()));
    if let Some(direct) = direct.filter(|direct| direct.is_object()) {
        return vec![direct];
    }
    let mut objects = Vec::new();
    for line in trimmed.lines() {
        let parsed = safe_parse_json(&Value::String(line.trim().to_string()));
        if let Some(parsed) = parsed.filter(|parsed| parsed.is_object()) {
            objects.push(parsed);
        }
    }
    objects
}

/// Parity: `extractConnectorApiError` (`json-event-stream.ts:86`).
fn extract_connector_api_error(value: &serde_json::Map<String, Value>) -> Option<Value> {
    let error = value.get("error")?.as_object()?;
    if error.get("code").and_then(Value::as_str).is_some() {
        return Some(Value::Object(error.clone()));
    }
    let wrapped = error
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get("error"))
        .and_then(Value::as_object)?;
    if wrapped.get("code").and_then(Value::as_str).is_some() {
        return Some(Value::Object(wrapped.clone()));
    }
    None
}

/// Parity: `connectorToolSelectionErrorMessage` (`json-event-stream.ts:97`).
fn connector_tool_selection_error_message(content: &str) -> Option<String> {
    if !content.contains("CONNECTOR_TOOL_NOT_FOUND") {
        return None;
    }
    let mut error = None;
    for parsed in parse_json_objects_from_content(content) {
        let Some(parsed_error) = parsed.as_object().and_then(extract_connector_api_error) else {
            continue;
        };
        if parsed_error.get("code").and_then(Value::as_str) == Some("CONNECTOR_TOOL_NOT_FOUND") {
            error = Some(parsed_error);
            break;
        }
    }
    let error = error?;
    let empty = serde_json::Map::new();
    let details = error
        .get("details")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let connector_id = details
        .get("connectorId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let tool_name = details
        .get("toolName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("the requested connector tool");
    let target = match connector_id {
        Some(connector_id) => format!(
            "Connector tool {tool_name} is not allowed for connector {connector_id}."
        ),
        None => format!("Connector tool {tool_name} is not allowed."),
    };
    Some(format!(
        "{target} Re-list the connector catalog and choose one of the currently allowed read-only tools."
    ))
}

/// Parity: `recoverableCodexReconnectProgress` (`json-event-stream.ts:147`) —
/// codex reports its own upstream reconnect loop as error-shaped JSONL.
fn recoverable_codex_reconnect_progress(message: &str) -> Option<(i64, i64)> {
    if !message.contains("timeout waiting for child process to exit")
        && !message.contains("stream disconnected before completion")
    {
        return None;
    }
    static PROGRESS: OnceLock<Regex> = OnceLock::new();
    let progress = PROGRESS.get_or_init(|| {
        Regex::new(r"^Reconnecting\.\.\.\s+(\d+)/(\d+)\b").expect("codex reconnect regex")
    });
    let captures = progress.captures(message)?;
    let attempt: i64 = captures.get(1)?.as_str().parse().ok()?;
    let max: i64 = captures.get(2)?.as_str().parse().ok()?;
    if attempt <= 0 || max <= 0 {
        return None;
    }
    Some((attempt, max))
}

/// Parity: `codexTodoListInput` (`json-event-stream.ts:719`).
fn codex_todo_list_input(item: &serde_json::Map<String, Value>) -> Option<Value> {
    if item.get("type").and_then(Value::as_str) != Some("todo_list") {
        return None;
    }
    let items = item.get("items").filter(|items| items.is_array())?;
    todo_write_input_from_items(items)
}

/// Parity: `emitCodexTodoList` (`json-event-stream.ts:724`).
fn emit_codex_todo_list(
    item: &serde_json::Map<String, Value>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> bool {
    let Some(id) = item.get("id").and_then(Value::as_str) else {
        return false;
    };
    let Some(input) = codex_todo_list_input(item) else {
        return false;
    };
    on_event(StreamEvent::Json(
        json!({ "type": "tool_use", "id": id, "name": "TodoWrite", "input": input }),
    ));
    true
}

/// Parity: `codexChangeDiffStat` (`json-event-stream.ts:968`). Counts a codex
/// change the way `diffStat` counts the equivalent Claude call, so the two
/// agents cannot disagree about the same edit.
fn codex_change_diff_stat(kind: &str, diff: Option<&Value>) -> Option<Value> {
    let diff = diff.and_then(Value::as_str).filter(|diff| !diff.is_empty())?;
    if kind == "add" {
        return Some(json!({ "added": diff.split('\n').count(), "removed": 0 }));
    }
    if kind != "update" {
        return None;
    }
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.split('\n') {
        // `+++`/`---` are file headers, not content.
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    if added == 0 && removed == 0 {
        None
    } else {
        Some(json!({ "added": added, "removed": removed }))
    }
}

/// One file a codex `file_change` item changed (parity: `CodexFileChange`).
struct CodexFileChange {
    id: String,
    name: String,
    path: String,
    stat: Option<Value>,
}

/// Parity: `codexFileChanges` (`json-event-stream.ts:996`). Returns `None` for
/// a partially recognised item on purpose: emitting the recognised half would
/// silently drop the rest, whereas `None` keeps the whole line visible as
/// `raw`. The patch text is read and not kept — only its two line counts.
fn codex_file_changes(item: &serde_json::Map<String, Value>) -> Option<Vec<CodexFileChange>> {
    if item.get("type").and_then(Value::as_str) != Some("file_change") {
        return None;
    }
    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
    if item_id.is_empty() {
        return None;
    }
    let changes = item.get("changes").and_then(Value::as_array)?;
    if changes.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        let change = change.as_object()?;
        let file_path = change.get("path").and_then(Value::as_str).unwrap_or("");
        let kind = change.get("kind").and_then(Value::as_str).unwrap_or("");
        let name = match kind {
            "add" => "Write",
            "update" => "Edit",
            // A deletion (or any unknown kind) stays `raw`: the record has no
            // delete verb, and calling it 「新建」/「改写」 would be a lie.
            _ => return None,
        };
        if file_path.is_empty() {
            return None;
        }
        out.push(CodexFileChange {
            id: format!("{item_id}#{index}"),
            name: name.to_string(),
            path: file_path.to_string(),
            stat: codex_change_diff_stat(kind, change.get("diff")),
        });
    }
    Some(out)
}

/// Parity: `emitCodexFileChangeToolUses` (`json-event-stream.ts:1029`). Each
/// change is its own call under `<item id>#<index>`, because the chat row model
/// is one file per row and the web dedupes `tool_use` by id.
fn emit_codex_file_change_tool_uses(
    changes: &[CodexFileChange],
    state: &mut JsonParserState,
    on_event: &mut dyn FnMut(StreamEvent),
) {
    for change in changes {
        if state.codex_tool_uses.contains(&change.id) {
            continue;
        }
        state.codex_tool_uses.insert(change.id.clone());
        // `od_diff_stat` is absent, not null, when there is nothing to report:
        // an input that keeps its single-key shape is byte-identical to what
        // `exec --json` produced before this existed.
        let input = match &change.stat {
            Some(stat) => json!({ "file_path": change.path, "od_diff_stat": stat }),
            None => json!({ "file_path": change.path }),
        };
        on_event(StreamEvent::Json(
            json!({ "type": "tool_use", "id": change.id, "name": change.name, "input": input }),
        ));
    }
}

/// Parity: `codexMcpToolName` (`json-event-stream.ts:1074`).
fn codex_mcp_tool_name(item: &serde_json::Map<String, Value>) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("mcp_tool_call") {
        return None;
    }
    let server = item.get("server").and_then(Value::as_str).unwrap_or("");
    let tool = item.get("tool").and_then(Value::as_str).unwrap_or("");
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some(format!("mcp__{server}__{tool}"))
}

/// Parity: `emitCodexMcpToolUse` (`json-event-stream.ts:1092`).
fn emit_codex_mcp_tool_use(
    item: &serde_json::Map<String, Value>,
    id: &str,
    name: &str,
    state: &mut JsonParserState,
    on_event: &mut dyn FnMut(StreamEvent),
) {
    if state.codex_tool_uses.contains(id) {
        return;
    }
    state.codex_tool_uses.insert(id.to_string());
    let input = item
        .get("arguments")
        .and_then(Value::as_object)
        .map(|arguments| Value::Object(arguments.clone()))
        .unwrap_or_else(|| json!({}));
    on_event(StreamEvent::Json(
        json!({ "type": "tool_use", "id": id, "name": name, "input": input }),
    ));
}

/// Parity: `codexMcpToolResult` (`json-event-stream.ts:1120`).
fn codex_mcp_tool_result(item: &serde_json::Map<String, Value>) -> (String, bool) {
    let error = item.get("error").and_then(Value::as_object);
    let is_error = item.get("status").and_then(Value::as_str) == Some("failed") || error.is_some();
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let content = if message.is_empty() {
        stringify_content(item.get("result"))
    } else {
        message.to_string()
    };
    (content, is_error)
}

/// Parity: `codexWebSearchQuery` (`json-event-stream.ts:1153`). Only
/// `action.type === "search"` is recognised; calling a page fetch a 「搜索」
/// would be the same class of lie as calling a deletion 「新建」.
fn codex_web_search_query(item: &serde_json::Map<String, Value>) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("web_search") {
        return None;
    }
    let action = item.get("action").and_then(Value::as_object)?;
    if action.get("type").and_then(Value::as_str) != Some("search") {
        return None;
    }
    let query = item.get("query").and_then(Value::as_str).unwrap_or("");
    if query.is_empty() {
        None
    } else {
        Some(query.to_string())
    }
}

/// Parity: `emitCodexReasoningItem` (`json-event-stream.ts:844`). Codex streams
/// reasoning summary text accumulated on `item.text`; emit the unseen suffix so
/// the web's collapsible thinking block has real content, joining distinct
/// items with a blank line.
fn emit_codex_reasoning_item(
    obj: &serde_json::Map<String, Value>,
    state: &mut JsonParserState,
    on_event: &mut dyn FnMut(StreamEvent),
) -> bool {
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");
    if !matches!(ty, "item.started" | "item.updated" | "item.completed") {
        return false;
    }
    let Some(item) = obj.get("item").and_then(Value::as_object) else {
        return false;
    };
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return false;
    }
    let key = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
    let emitted = state
        .codex_reasoning_emitted_by_item
        .get(&key)
        .copied()
        .unwrap_or(0);
    if text.len() > emitted {
        // `emitted` indexes an earlier revision of this same item's text, so
        // the offset is normally a stable prefix boundary; a boundary that no
        // longer lands on a char edge falls back to the whole text rather than
        // panicking on the slice.
        let suffix = text.get(emitted..).unwrap_or(text);
        let delta = if emitted == 0 && state.codex_reasoning_emitted_any {
            format!("\n\n{suffix}")
        } else {
            suffix.to_string()
        };
        on_event(StreamEvent::Json(json!({ "type": "thinking_delta", "delta": delta })));
        state
            .codex_reasoning_emitted_by_item
            .insert(key, text.len());
        state.codex_reasoning_emitted_any = true;
    }
    true
}

/// Parity: `handleCodexEvent` (`json-event-stream.ts:1161`).
fn handle_codex_event(
    state: &mut JsonParserState,
    obj: &Value,
    on_event: &mut dyn FnMut(StreamEvent),
) -> bool {
    let Some(obj) = obj.as_object() else {
        return false;
    };
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");

    if ty == "error" {
        let message = extract_error_message(
            obj.get("message").or_else(|| obj.get("error")).unwrap_or(&Value::Null),
            "Codex error",
        );
        // Normalize codex's own reconnect loop into an ephemeral machine status
        // rather than letting the raw SDK sentence reach assistant history.
        if let Some((attempt, max)) = recoverable_codex_reconnect_progress(&message) {
            on_event(StreamEvent::Json(json!({
                "type": "status",
                "label": "agent_reconnecting",
                "detail": format!("{attempt}/{max}"),
            })));
            return true;
        }
        if !state.codex_error_emitted {
            state.codex_error_emitted = true;
            on_event(StreamEvent::Error {
                message,
                code: None,
                raw: None,
            });
        }
        return true;
    }

    if ty == "turn.failed" {
        if !state.codex_error_emitted {
            state.codex_error_emitted = true;
            on_event(StreamEvent::Error {
                message: extract_error_message(
                    obj.get("error").or_else(|| obj.get("message")).unwrap_or(&Value::Null),
                    "Codex turn failed",
                ),
                code: None,
                raw: None,
            });
        }
        return true;
    }

    if ty == "thread.started" {
        // `thread_id` is codex's own session handle, surfaced on the same
        // `sessionId` status channel claude uses.
        let thread_id = obj
            .get("thread_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        on_event(StreamEvent::Json(json!({
            "type": "status",
            "label": "initializing",
            "sessionId": thread_id,
        })));
        return true;
    }

    if ty == "turn.started" {
        state.codex_previous_event_was_agent_message = false;
        state.codex_last_agent_message_ended_with_newline = false;
        on_event(StreamEvent::Json(json!({ "type": "status", "label": "thinking" })));
        return true;
    }

    if emit_codex_reasoning_item(obj, state, on_event) {
        return true;
    }

    if ty == "item.started" {
        if let Some(item) = obj.get("item").and_then(Value::as_object) {
            if emit_codex_todo_list(item, on_event) {
                state.codex_previous_event_was_agent_message = false;
                state.codex_last_agent_message_ended_with_newline = false;
                return true;
            }
            if item.get("type").and_then(Value::as_str) == Some("command_execution") {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    state.codex_previous_event_was_agent_message = false;
                    state.codex_last_agent_message_ended_with_newline = false;
                    if !state.codex_tool_uses.contains(id) {
                        state.codex_tool_uses.insert(id.to_string());
                        on_event(StreamEvent::Json(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": "Bash",
                            "input": { "command": item.get("command").and_then(Value::as_str).unwrap_or("") },
                        })));
                    }
                    return true;
                }
            }
            // Called from `item.started` so the row gets a real duration.
            if let Some(changes) = codex_file_changes(item) {
                state.codex_previous_event_was_agent_message = false;
                state.codex_last_agent_message_ended_with_newline = false;
                emit_codex_file_change_tool_uses(&changes, state, on_event);
                return true;
            }
            if let Some(name) = codex_mcp_tool_name(item) {
                if let Some(id) = item.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                    state.codex_previous_event_was_agent_message = false;
                    state.codex_last_agent_message_ended_with_newline = false;
                    emit_codex_mcp_tool_use(item, id, &name, state, on_event);
                    return true;
                }
            }
            if item.get("type").and_then(Value::as_str) == Some("web_search") {
                // The started frame's `query` really is always empty; `id` is
                // the id AFTER `JSON.parse` deduplicates codex's twice-
                // serialised `id` key, and the completed frame resolves to the
                // same one, so this early row retires into the settled one.
                if let Some(id) = item.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                    state.codex_previous_event_was_agent_message = false;
                    state.codex_last_agent_message_ended_with_newline = false;
                    on_event(StreamEvent::Json(json!({
                        "type": "tool_in_flight",
                        "id": id,
                        "name": "web_search",
                        "input": {},
                    })));
                }
                return true;
            }
        }
    }

    if ty == "item.updated" {
        if let Some(item) = obj.get("item").and_then(Value::as_object) {
            if emit_codex_todo_list(item, on_event) {
                state.codex_previous_event_was_agent_message = false;
                state.codex_last_agent_message_ended_with_newline = false;
                return true;
            }
        }
    }

    if ty == "item.completed" {
        if let Some(item) = obj.get("item").and_then(Value::as_object) {
            if emit_codex_todo_list(item, on_event) {
                state.codex_previous_event_was_agent_message = false;
                state.codex_last_agent_message_ended_with_newline = false;
                return true;
            }
            // Non-fatal in-stream notices (e.g. the skills context-budget
            // warning) arrive as `error` ITEMS while the turn keeps running;
            // surface them as a warning pill instead of raw noise.
            if item.get("type").and_then(Value::as_str) == Some("error") {
                if let Some(message) = item
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                {
                    on_event(StreamEvent::Json(
                        json!({ "type": "status", "label": "warning", "detail": message }),
                    ));
                    return true;
                }
            }
            if item.get("type").and_then(Value::as_str) == Some("command_execution") {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    state.codex_previous_event_was_agent_message = false;
                    state.codex_last_agent_message_ended_with_newline = false;
                    if !state.codex_tool_uses.contains(id) {
                        state.codex_tool_uses.insert(id.to_string());
                        on_event(StreamEvent::Json(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": "Bash",
                            "input": { "command": item.get("command").and_then(Value::as_str).unwrap_or("") },
                        })));
                    }
                    let content = stringify_content(item.get("aggregated_output"));
                    on_event(StreamEvent::Json(json!({
                        "type": "tool_result",
                        "toolUseId": id,
                        "content": content,
                        "isError": match item.get("exit_code").and_then(Value::as_i64) {
                            Some(exit_code) => exit_code != 0,
                            None => item.get("status").and_then(Value::as_str) == Some("failed"),
                        },
                    })));
                    if let Some(connector_error) = connector_tool_selection_error_message(&content) {
                        if !state.codex_error_emitted {
                            state.codex_error_emitted = true;
                            on_event(StreamEvent::Error {
                                message: connector_error,
                                code: None,
                                raw: None,
                            });
                        }
                    }
                    return true;
                }
            }
            if let Some(changes) = codex_file_changes(item) {
                state.codex_previous_event_was_agent_message = false;
                state.codex_last_agent_message_ended_with_newline = false;
                emit_codex_file_change_tool_uses(&changes, state, on_event);
                // The result carries no content: a patch belongs on the row as
                // `+N −M`, not in the terminal panel a `tool_result` opens.
                let is_error = item.get("status").and_then(Value::as_str) == Some("failed");
                for change in &changes {
                    on_event(StreamEvent::Json(json!({
                        "type": "tool_result",
                        "toolUseId": change.id,
                        "content": "",
                        "isError": is_error,
                    })));
                }
                return true;
            }
            if let Some(name) = codex_mcp_tool_name(item) {
                if let Some(id) = item.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                    state.codex_previous_event_was_agent_message = false;
                    state.codex_last_agent_message_ended_with_newline = false;
                    emit_codex_mcp_tool_use(item, id, &name, state, on_event);
                    let (content, is_error) = codex_mcp_tool_result(item);
                    on_event(StreamEvent::Json(json!({
                        "type": "tool_result",
                        "toolUseId": id,
                        "content": content,
                        "isError": is_error,
                    })));
                    return true;
                }
            }
            if let Some(query) = codex_web_search_query(item) {
                if let Some(id) = item.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                    state.codex_previous_event_was_agent_message = false;
                    state.codex_last_agent_message_ended_with_newline = false;
                    if !state.codex_tool_uses.contains(id) {
                        state.codex_tool_uses.insert(id.to_string());
                        on_event(StreamEvent::Json(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": "web_search",
                            "input": { "query": query },
                        })));
                    }
                    // No content: the row shows 「搜索 <query>」 without a
                    // fabricated 「N 处」. A hit count would be invented, not
                    // read — measure the wire before changing this.
                    on_event(StreamEvent::Json(json!({
                        "type": "tool_result",
                        "toolUseId": id,
                        "content": "",
                        "isError": false,
                    })));
                    return true;
                }
            }
            if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                if let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    let needs_boundary = state.codex_previous_event_was_agent_message
                        && !state.codex_last_agent_message_ended_with_newline
                        && !text.starts_with('\n');
                    let delta = if needs_boundary {
                        format!("\n{text}")
                    } else {
                        text.to_string()
                    };
                    on_event(StreamEvent::Json(json!({ "type": "text_delta", "delta": delta })));
                    state.codex_previous_event_was_agent_message = true;
                    state.codex_last_agent_message_ended_with_newline = text.ends_with('\n');
                    return true;
                }
            }
        }
    }

    if ty == "turn.completed" && obj.get("usage").is_some_and(Value::is_object) {
        let empty = serde_json::Map::new();
        let raw_usage = obj.get("usage").and_then(Value::as_object).unwrap_or(&empty);
        let mut usage = serde_json::Map::new();
        for (source, target) in [
            ("input_tokens", "input_tokens"),
            ("output_tokens", "output_tokens"),
            ("reasoning_output_tokens", "thought_tokens"),
            ("cached_input_tokens", "cached_read_tokens"),
        ] {
            if let Some(value) = raw_usage.get(source).filter(|value| value.is_number()) {
                usage.insert(target.to_string(), value.clone());
            }
        }
        on_event(StreamEvent::Json(
            json!({ "type": "usage", "usage": Value::Object(usage) }),
        ));
        return true;
    }

    false
}

#[cfg(test)]
mod json_event_tests {
    use super::*;

    /// Feeds a parser and records each emitted frame as its wire JSON.
    struct Probe {
        parser: JsonEventParser,
        events: Vec<Value>,
    }

    impl Probe {
        fn new(kind: &'static str) -> Self {
            Self {
                parser: JsonEventParser::new(kind),
                events: Vec::new(),
            }
        }

        fn feed(&mut self, chunk: &str) {
            let events = &mut self.events;
            self.parser.feed(chunk, &mut |event| events.push(event.to_json()));
        }

        fn flush(&mut self) {
            let events = &mut self.events;
            self.parser.flush(&mut |event| events.push(event.to_json()));
        }
    }

    /// Parity: `json-event-stream.test.ts:15` (opencode).
    #[test]
    fn opencode_emits_text_and_usage() {
        let mut probe = Probe::new("opencode");
        probe.feed(concat!(
            "{\"type\":\"step_start\",\"sessionID\":\"ses-1\",\"part\":{\"type\":\"step-start\"}}\n",
            "{\"type\":\"text\",\"sessionID\":\"ses-1\",\"part\":{\"type\":\"text\",\"text\":\"hello\"}}\n",
            "{\"type\":\"step_finish\",\"sessionID\":\"ses-1\",\"part\":{\"type\":\"step-finish\",\"tokens\":{\"input\":11,\"output\":7,\"reasoning\":3,\"cache\":{\"read\":5,\"write\":2}},\"cost\":0}}\n",
        ));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "running", "sessionId": "ses-1"}),
                json!({"type": "text_delta", "delta": "hello"}),
                json!({
                    "type": "usage",
                    "usage": {
                        "input_tokens": 11,
                        "output_tokens": 7,
                        "thought_tokens": 3,
                        "cached_read_tokens": 5,
                        "cached_write_tokens": 2,
                    },
                    "costUsd": 0,
                }),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:41`.
    #[test]
    fn opencode_step_start_surfaces_the_session_id() {
        let mut probe = Probe::new("opencode");
        probe.feed("{\"type\":\"step_start\",\"sessionID\":\"ses_4af9c2\",\"part\":{\"type\":\"step-start\"}}\n");
        assert_eq!(
            probe.events,
            vec![json!({"type": "status", "label": "running", "sessionId": "ses_4af9c2"})]
        );
    }

    /// Parity: `json-event-stream.test.ts:49` — a missing session id reports
    /// `null` rather than dropping the key (no spurious capture).
    #[test]
    fn opencode_step_start_without_session_id_reports_null() {
        let mut probe = Probe::new("opencode");
        probe.feed("{\"type\":\"step_start\",\"part\":{\"type\":\"step-start\"}}\n");
        assert_eq!(
            probe.events,
            vec![json!({"type": "status", "label": "running", "sessionId": null})]
        );
    }

    /// Parity: `json-event-stream.test.ts:57`.
    #[test]
    fn opencode_emits_tool_use_and_result() {
        let mut probe = Probe::new("opencode");
        probe.feed(&format!(
            "{}\n",
            json!({
                "type": "tool_use",
                "part": {
                    "tool": "read",
                    "callID": "call-1",
                    "state": {
                        "input": "{\"file\":\"foo.txt\"}",
                        "output": "done",
                        "status": "completed",
                    },
                },
            })
        ));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "call-1", "name": "read", "input": {"file": "foo.txt"}}),
                json!({"type": "tool_result", "toolUseId": "call-1", "content": "done", "isError": false}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:81` — structured failures.
    #[test]
    fn opencode_marks_structured_tool_failures_as_errors() {
        let cases: Vec<(Value, &str)> = vec![
            (
                json!({"status": "completed", "output": "command failed", "metadata": {"exit": 1}}),
                "command failed",
            ),
            (
                json!({"status": "completed", "output": "command failed", "exitCode": 2}),
                "command failed",
            ),
            (
                json!({"status": "completed", "output": "command failed", "exit": 3}),
                "command failed",
            ),
            (
                json!({"status": "completed", "output": "partial output", "error": "tool failed"}),
                "tool failed",
            ),
            (
                json!({"status": "error", "output": "partial output", "error": "permission denied"}),
                "permission denied",
            ),
            (json!({"status": "failed", "output": "process failed"}), "process failed"),
            (
                json!({
                    "status": "completed",
                    "output": concat!(
                        "Get-Content : Cannot find path 'missing.txt' because it does not exist.\r\n",
                        "    + CategoryInfo          : ObjectNotFound: (missing.txt:String) [Get-Content], ItemNotFoundException\r\n",
                        "    + FullyQualifiedErrorId : PathNotFound,Microsoft.PowerShell.Commands.GetContentCommand",
                    ),
                    "metadata": {"exit": 0},
                }),
                concat!(
                    "Get-Content : Cannot find path 'missing.txt' because it does not exist.\r\n",
                    "    + CategoryInfo          : ObjectNotFound: (missing.txt:String) [Get-Content], ItemNotFoundException\r\n",
                    "    + FullyQualifiedErrorId : PathNotFound,Microsoft.PowerShell.Commands.GetContentCommand",
                ),
            ),
        ];
        for (index, (state, content)) in cases.iter().enumerate() {
            let mut probe = Probe::new("opencode");
            probe.feed(&format!(
                "{}\n",
                json!({
                    "type": "tool_use",
                    "part": {
                        "tool": "bash",
                        "callID": format!("call-{index}"),
                        "state": state,
                    },
                })
            ));
            assert_eq!(
                probe.events.last(),
                Some(&json!({
                    "type": "tool_result",
                    "toolUseId": format!("call-{index}"),
                    "content": content,
                    "isError": true,
                })),
                "case {index}"
            );
        }
    }

    /// Parity: `json-event-stream.test.ts:159`.
    #[test]
    fn opencode_preserves_successful_and_non_terminal_tool_states() {
        let mut probe = Probe::new("opencode");
        for (call_id, state) in [
            ("success", json!({"status": "completed", "output": "done", "metadata": {"exit": 0}})),
            ("false-error", json!({"status": "completed", "output": "done", "error": false})),
            ("zero-error", json!({"status": "completed", "output": "done", "error": 0})),
            ("pending", json!({"status": "pending", "input": {"command": "pwd"}})),
            ("running", json!({"status": "running", "input": {"command": "pwd"}})),
        ] {
            probe.feed(&format!(
                "{}\n",
                json!({"type": "tool_use", "part": {"tool": "bash", "callID": call_id, "state": state}})
            ));
        }
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "success", "name": "bash", "input": null}),
                json!({"type": "tool_result", "toolUseId": "success", "content": "done", "isError": false}),
                json!({"type": "tool_use", "id": "false-error", "name": "bash", "input": null}),
                json!({"type": "tool_result", "toolUseId": "false-error", "content": "done", "isError": false}),
                json!({"type": "tool_use", "id": "zero-error", "name": "bash", "input": null}),
                json!({"type": "tool_result", "toolUseId": "zero-error", "content": "done", "isError": false}),
                json!({"type": "tool_use", "id": "pending", "name": "bash", "input": {"command": "pwd"}}),
                json!({"type": "tool_use", "id": "running", "name": "bash", "input": {"command": "pwd"}}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:186` — a terminal result is emitted
    /// once per call even when OpenCode replays the frame.
    #[test]
    fn opencode_emits_a_terminal_result_once_per_call() {
        let mut probe = Probe::new("opencode");
        let terminal = format!(
            "{}\n",
            json!({
                "type": "tool_use",
                "sessionID": "session-1",
                "part": {
                    "tool": "bash",
                    "callID": "call-1",
                    "state": {"status": "completed", "output": "failed", "metadata": {"exit": 1}},
                },
            })
        );
        probe.feed(&format!("{terminal}{terminal}"));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "call-1", "name": "bash", "input": null}),
                json!({"type": "tool_result", "toolUseId": "call-1", "content": "failed", "isError": true}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:206` — both canonical `+`-prefixed
    /// ErrorRecord fields are required, and only for a shell tool.
    #[test]
    fn opencode_powershell_signatures_stay_narrow() {
        let error_record = concat!(
            "+ CategoryInfo : ObjectNotFound: (missing.txt:String) [Get-Content], ItemNotFoundException\n",
            "+ FullyQualifiedErrorId : PathNotFound,Microsoft.PowerShell.Commands.GetContentCommand",
        );
        let documentation = concat!(
            "CategoryInfo : copied from a troubleshooting document\n",
            "FullyQualifiedErrorId : copied-example",
        );
        let mut probe = Probe::new("opencode");
        for (tool, call_id, output) in [
            ("read", "read-error-record", error_record),
            ("bash", "bash-documentation", documentation),
        ] {
            probe.feed(&format!(
                "{}\n",
                json!({
                    "type": "tool_use",
                    "part": {
                        "tool": tool,
                        "callID": call_id,
                        "state": {"status": "completed", "output": output, "metadata": {"exit": 0}},
                    },
                })
            ));
        }
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "read-error-record", "name": "read", "input": null}),
                json!({"type": "tool_result", "toolUseId": "read-error-record", "content": error_record, "isError": false}),
                json!({"type": "tool_use", "id": "bash-documentation", "name": "bash", "input": null}),
                json!({"type": "tool_result", "toolUseId": "bash-documentation", "content": documentation, "isError": false}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:296` — a structured error frame whose
    /// detail lives under `error.data.message`.
    #[test]
    fn opencode_emits_structured_errors_as_error_events() {
        let mut probe = Probe::new("opencode");
        let line = json!({
            "type": "error",
            "error": {"data": {"message": "OpenCode auth failed: login required"}},
        });
        probe.feed(&format!("{line}\n"));
        assert_eq!(
            probe.events,
            vec![json!({
                "type": "error",
                "message": "OpenCode auth failed: login required",
                "raw": line.to_string(),
            })]
        );
    }

    /// Parity: `json-event-stream.test.ts:310`.
    #[test]
    fn opencode_preserves_nested_error_messages() {
        let mut probe = Probe::new("opencode");
        let line = json!({"type": "error", "error": {"message": "model not found: openai/nope"}});
        probe.feed(&format!("{line}\n"));
        assert_eq!(
            probe.events,
            vec![json!({
                "type": "error",
                "message": "model not found: openai/nope",
                "raw": line.to_string(),
            })]
        );
    }

    /// Parity: `json-event-stream.test.ts:324` / `:375` — `error.name` is the
    /// fallback when no message is present.
    #[test]
    fn opencode_falls_back_to_error_name() {
        for error in [json!({"name": "AuthError", "data": {}}), json!({"name": "NetworkError"})] {
            let mut probe = Probe::new("opencode");
            let line = json!({"type": "error", "error": error});
            probe.feed(&format!("{line}\n"));
            assert_eq!(
                probe.events,
                vec![json!({
                    "type": "error",
                    "message": error["name"],
                    "raw": line.to_string(),
                })]
            );
        }
    }

    /// Parity: `json-event-stream.test.ts:393` — a generic message when the
    /// error frame carries no usable detail.
    #[test]
    fn opencode_falls_back_to_a_generic_error_message() {
        let mut probe = Probe::new("opencode");
        let line = json!({"type": "error", "error": {}});
        probe.feed(&format!("{line}\n"));
        assert_eq!(
            probe.events,
            vec![json!({"type": "error", "message": "OpenCode error", "raw": line.to_string()})]
        );
    }

    /// Parity: `json-event-stream.test.ts:338` — an unparseable line becomes a
    /// bounded raw event (only on flush, since the trailing line has no newline).
    #[test]
    fn unknown_lines_become_raw_events() {
        let mut probe = Probe::new("opencode");
        probe.feed("not-json\n");
        probe.flush();
        assert_eq!(probe.events, vec![json!({"type": "raw", "line": "not-json"})]);
    }

    /// Parity: `runtimes/opencode-tool-events.ts` — the local plugin's frame
    /// surfaces an in-flight tool before the native frame settles it.
    #[test]
    fn opencode_plugin_frame_surfaces_tool_in_flight() {
        let mut probe = Probe::new("opencode");
        probe.feed(concat!(
            "{\"type\":\"step_start\",\"sessionID\":\"ses-1\",\"part\":{\"type\":\"step-start\"}}\n",
            "{\"type\":\"od_opencode_tool\",\"version\":1,\"sessionID\":\"ses-1\",\"callID\":\"c1\",\"tool\":\"read\",\"path\":\"/tmp/x.txt\"}\n",
        ));
        assert_eq!(probe.events.len(), 2);
        let frame = &probe.events[1];
        assert_eq!(frame["type"], "tool_in_flight");
        assert_eq!(frame["id"], "c1");
        assert_eq!(frame["name"], "read");
        assert_eq!(frame["input"], json!({"file_path": "/tmp/x.txt"}));
        assert!(frame["startedAt"].is_i64() || frame["startedAt"].is_u64());
    }

    /// A pending plugin call is failed on flush rather than left rendered.
    #[test]
    fn opencode_flush_fails_a_pending_plugin_call() {
        let mut probe = Probe::new("opencode");
        probe.feed(concat!(
            "{\"type\":\"step_start\",\"sessionID\":\"ses-1\",\"part\":{\"type\":\"step-start\"}}\n",
            "{\"type\":\"od_opencode_tool\",\"version\":1,\"sessionID\":\"ses-1\",\"callID\":\"c1\",\"tool\":\"read\"}\n",
        ));
        probe.flush();
        assert_eq!(
            probe.events.last(),
            Some(&json!({
                "type": "tool_result",
                "toolUseId": "c1",
                "content": "Tool stream ended before completion was reported.",
                "isError": true,
            }))
        );
    }

    /// An unported kind consumes nothing, so each line falls through to raw —
    /// the same shape TS produces for a frame its handler does not recognise.
    #[test]
    fn unported_kinds_fall_through_to_raw() {
        let mut probe = Probe::new("unknown-kind");
        probe.feed("{\"type\":\"init\",\"session_id\":\"gm-1\"}\n");
        assert_eq!(
            probe.events,
            vec![json!({"type": "raw", "line": "{\"type\":\"init\",\"session_id\":\"gm-1\"}"})]
        );
    }

    /// Serializes a frame the way the wire carries it, so `raw` expectations
    /// are exactly the fed text.
    fn frame_line(frame: &Value) -> String {
        format!("{}\n", serde_json::to_string(frame).expect("frame json"))
    }

    fn gemini_write_file(file_path: &str, content: &str) -> Value {
        json!({
            "type": "tool_use",
            "tool_name": "write_file",
            "tool_id": "write_file__1",
            "parameters": { "file_path": file_path, "content": content },
        })
    }

    /// Parity: `json-event-stream.test.ts:408`.
    #[test]
    fn gemini_emits_init_text_and_usage() {
        let mut probe = Probe::new("gemini");
        probe.feed(&frame_line(
            &json!({"type": "init", "session_id": "gm-1", "model": "gemini-3-flash-preview"}),
        ));
        probe.feed(&frame_line(
            &json!({"type": "message", "role": "assistant", "content": "hello", "delta": true}),
        ));
        probe.feed(&frame_line(&json!({
            "type": "result",
            "status": "success",
            "stats": {"input_tokens": 9, "output_tokens": 4, "cached": 2, "duration_ms": 321},
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "initializing", "model": "gemini-3-flash-preview"}),
                json!({"type": "text_delta", "delta": "hello"}),
                json!({
                    "type": "usage",
                    "usage": {"input_tokens": 9, "output_tokens": 4, "cached_read_tokens": 2},
                    "durationMs": 321,
                }),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:494`.
    #[test]
    fn gemini_handles_real_user_tool_and_error_frames() {
        let mut probe = Probe::new("gemini");
        let fatal = json!({
            "type": "result",
            "status": "error",
            "error": {"type": "FatalAuthenticationError", "message": "Authentication failed"},
            "stats": {"input_tokens": 11, "output_tokens": 0, "cached": 0, "duration_ms": 42},
        });
        probe.feed(&frame_line(
            &json!({"type": "message", "role": "user", "content": "make a video"}),
        ));
        probe.feed(&frame_line(&json!({
            "type": "tool_use",
            "tool_name": "write_file",
            "tool_id": "tool-1",
            "parameters": {"path": "timeline.json"},
        })));
        probe.feed(&frame_line(&json!({
            "type": "tool_result",
            "tool_id": "tool-1",
            "status": "success",
            "output": "wrote timeline.json",
        })));
        probe.feed(&frame_line(&json!({
            "type": "error",
            "severity": "warning",
            "message": "Agent execution blocked: retrying without shell",
        })));
        probe.feed(&frame_line(&fatal));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "tool-1", "name": "write_file", "input": {"path": "timeline.json"}}),
                json!({"type": "tool_result", "toolUseId": "tool-1", "content": "wrote timeline.json", "isError": false}),
                json!({"type": "status", "label": "warning", "detail": "Agent execution blocked: retrying without shell"}),
                json!({"type": "error", "message": "Authentication failed", "raw": fatal.to_string()}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:606` — a file write suppresses the
    /// echoed artifact the CLI prints next.
    #[test]
    fn gemini_suppresses_duplicate_artifact_text_after_a_file_write() {
        let mut probe = Probe::new("gemini");
        probe.feed(&frame_line(&gemini_write_file(
            "index.html",
            "<!doctype html><html></html>",
        )));
        probe.feed(&frame_line(&json!({
            "type": "message",
            "role": "assistant",
            "content": "Done.\n\n<artifact identifier=\"page\" type=\"text/html\">\n<!doctype html><html></html>\n</artifact>",
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({
                    "type": "tool_use",
                    "id": "write_file__1",
                    "name": "write_file",
                    "input": {"file_path": "index.html", "content": "<!doctype html><html></html>"},
                }),
                json!({"type": "text_delta", "delta": "Done.\n\n"}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:645` — the open tag is split across
    /// two frames, so its prefix must be held until the rest arrives.
    #[test]
    fn gemini_suppresses_artifact_text_split_across_frames() {
        let mut probe = Probe::new("gemini");
        probe.feed(&frame_line(&gemini_write_file(
            "index.html",
            "<!doctype html><html></html>",
        )));
        probe.feed(&frame_line(&json!({
            "type": "message",
            "role": "assistant",
            "content": "Done.\n\n<",
        })));
        probe.feed(&frame_line(&json!({
            "type": "message",
            "role": "assistant",
            "content": "artifact identifier=\"page\" type=\"text/html\">\n<!doctype html><html></html>\n</artifact>Tail",
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({
                    "type": "tool_use",
                    "id": "write_file__1",
                    "name": "write_file",
                    "input": {"file_path": "index.html", "content": "<!doctype html><html></html>"},
                }),
                json!({"type": "text_delta", "delta": "Done.\n\n"}),
                json!({"type": "text_delta", "delta": "Tail"}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:826` — only the first (echoed)
    /// artifact is dropped; a later one is real output.
    #[test]
    fn gemini_preserves_a_later_artifact_after_suppressing_the_echo() {
        let mut probe = Probe::new("gemini");
        probe.feed(&frame_line(&gemini_write_file(
            "helper.html",
            "<!doctype html><html>helper</html>",
        )));
        probe.feed(&frame_line(&json!({
            "type": "message",
            "role": "assistant",
            "content": "Helper written.\n\n<artifact identifier=\"helper\" type=\"text/html\">\n<!doctype html><html>helper</html>\n</artifact>",
        })));
        let final_text = "Final artifact:\n\n<artifact identifier=\"final\" type=\"text/html\">\n<!doctype html><html>final</html>\n</artifact>";
        probe.feed(&frame_line(&json!({
            "type": "message",
            "role": "assistant",
            "content": final_text,
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({
                    "type": "tool_use",
                    "id": "write_file__1",
                    "name": "write_file",
                    "input": {"file_path": "helper.html", "content": "<!doctype html><html>helper</html>"},
                }),
                json!({"type": "text_delta", "delta": "Helper written.\n\n"}),
                json!({"type": "text_delta", "delta": final_text}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:743` — prose after a file write with
    /// no artifact following is emitted unchanged.
    #[test]
    fn gemini_keeps_prose_after_a_file_write_with_no_artifact() {
        let mut probe = Probe::new("gemini");
        probe.feed(&frame_line(&gemini_write_file("index.html", "<html></html>")));
        probe.feed(&frame_line(&json!({
            "type": "message",
            "role": "assistant",
            "content": "Prose only.",
        })));
        assert_eq!(
            probe.events.last(),
            Some(&json!({"type": "text_delta", "delta": "Prose only."}))
        );
    }

    /// Parity: `json-event-stream.test.ts:782` — a trailing partial open tag is
    /// held, then flushed as prose when the stream ends.
    #[test]
    fn gemini_flushes_a_trailing_partial_artifact_opener_as_prose() {
        let mut probe = Probe::new("gemini");
        probe.feed(&frame_line(&gemini_write_file("index.html", "<html></html>")));
        probe.feed(&frame_line(&json!({
            "type": "message",
            "role": "assistant",
            "content": "Tail <art",
        })));
        probe.flush();
        let deltas: Vec<&Value> = probe
            .events
            .iter()
            .filter(|event| event["type"] == "text_delta")
            .collect();
        assert_eq!(
            deltas,
            vec![
                &json!({"type": "text_delta", "delta": "Tail "}),
                &json!({"type": "text_delta", "delta": "<art"}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:875` — a non-warning `error` frame is
    /// fatal regardless of severity vocabulary.
    #[test]
    fn gemini_treats_terminal_error_frames_as_errors() {
        let terminal = json!({"type": "error", "severity": "error", "message": "Maximum session turns exceeded"});
        let unknown = json!({"type": "error", "severity": "critical", "error": {"message": "Invalid stream: malformed tool call"}});
        let mut probe = Probe::new("gemini");
        probe.feed(&frame_line(&terminal));
        probe.feed(&frame_line(&unknown));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "error", "message": "Maximum session turns exceeded", "raw": terminal.to_string()}),
                json!({"type": "error", "message": "Invalid stream: malformed tool call", "raw": unknown.to_string()}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:433`.
    #[test]
    fn kimi_emits_tool_calls_results_and_text() {
        let mut probe = Probe::new("kimi");
        probe.feed(&frame_line(&json!({
            "role": "assistant",
            "tool_calls": [{
                "type": "function",
                "id": "tool-1",
                "function": {"name": "Write", "arguments": "{\"path\":\"index.html\",\"content\":\"<html></html>\"}"},
            }],
        })));
        probe.feed(&frame_line(&json!({
            "role": "tool",
            "tool_call_id": "tool-1",
            "content": "Wrote 13 bytes to index.html",
        })));
        probe.feed(&frame_line(&json!({"role": "assistant", "content": "Done."})));
        probe.feed(&frame_line(&json!({
            "role": "meta",
            "type": "session.resume_hint",
            "session_id": "session-1",
            "content": "To resume this session: kimi -r session-1",
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "tool-1", "name": "Write", "input": {"path": "index.html", "content": "<html></html>"}}),
                json!({"type": "tool_result", "toolUseId": "tool-1", "content": "Wrote 13 bytes to index.html", "isError": false}),
                json!({"type": "text_delta", "delta": "Done."}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:2391`.
    #[test]
    fn antigravity_maps_text_tool_steps_and_usage() {
        let mut probe = Probe::new("antigravity");
        probe.feed(&frame_line(&json!({
            "event": "init",
            "conversation_id": "c3b66b04-872b-4fbe-a3a4-058a026ef20a",
        })));
        probe.feed(&frame_line(&json!({
            "event": "step_update",
            "step_update": {
                "step_type": "tool",
                "conversation_id": "edb1c8c1-50ba-4f3f-87eb-412d0e9d47c3",
                "step_index": 4,
                "tool_name": "run_command",
                "state": "DONE",
                "tool_info": {
                    "name": "run_command",
                    "parameters": {"CommandLine": "echo hello_headless_demo"},
                    "output": "hello_headless_demo\r\n",
                },
            },
        })));
        probe.feed(&frame_line(&json!({
            "event": "step_update",
            "step_update": {"step_type": "agent_response", "text_delta": "Git rebase..."},
        })));
        probe.feed(&frame_line(&json!({
            "event": "result",
            "result": {
                "status": "SUCCESS",
                "duration_seconds": 6.88,
                "usage": {
                    "input_tokens": 10418,
                    "output_tokens": 589,
                    "thinking_tokens": 551,
                    "cache_read_tokens": 8113,
                },
            },
        })));
        probe.flush();
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "initializing", "sessionId": "c3b66b04-872b-4fbe-a3a4-058a026ef20a"}),
                json!({
                    "type": "tool_use",
                    "id": "edb1c8c1-50ba-4f3f-87eb-412d0e9d47c3:4",
                    "name": "run_command",
                    "input": {"CommandLine": "echo hello_headless_demo"},
                }),
                json!({
                    "type": "tool_result",
                    "toolUseId": "edb1c8c1-50ba-4f3f-87eb-412d0e9d47c3:4",
                    "content": "hello_headless_demo\r\n",
                    "isError": false,
                }),
                json!({"type": "text_delta", "delta": "Git rebase..."}),
                json!({
                    "type": "usage",
                    "usage": {
                        "input_tokens": 10418,
                        "output_tokens": 589,
                        "thought_tokens": 551,
                        "cached_read_tokens": 8113,
                    },
                    "durationMs": 6880,
                }),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:2429` — a non-SUCCESS result is one
    /// error event, and no unrecognised frame leaks through as `raw`.
    #[test]
    fn antigravity_surfaces_an_error_result() {
        let mut probe = Probe::new("antigravity");
        probe.feed(&frame_line(&json!({
            "event": "result",
            "result": {"status": "ERROR", "error": "Individual quota reached for this model"},
        })));
        probe.flush();
        let errors: Vec<&Value> = probe
            .events
            .iter()
            .filter(|event| event["type"] == "error")
            .collect();
        assert_eq!(errors.len(), 1);
        assert!(errors[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Individual quota reached")));
        assert!(!probe.events.iter().any(|event| event["type"] == "text_delta"));
        assert!(!probe.events.iter().any(|event| event["type"] == "raw"));
    }

    /// Parity: `json-event-stream.test.ts:2443`.
    #[test]
    fn antigravity_falls_back_to_the_result_response() {
        let mut probe = Probe::new("antigravity");
        probe.feed(&frame_line(&json!({
            "event": "result",
            "result": {
                "status": "SUCCESS",
                "response": "Only in the result.",
                "duration_seconds": 1,
                "usage": {
                    "input_tokens": 3,
                    "output_tokens": 4,
                    "thinking_tokens": 0,
                    "cache_read_tokens": 0,
                },
            },
        })));
        probe.flush();
        let deltas: Vec<&Value> = probe
            .events
            .iter()
            .filter(|event| event["type"] == "text_delta")
            .collect();
        assert_eq!(deltas, vec![&json!({"type": "text_delta", "delta": "Only in the result."})]);
    }

    /// Replays the recorded agy success transcript the TS test reads
    /// (`apps/daemon/tests/fixtures/antigravity-stream-json/docs-success.stdout.ndjson`),
    /// so the Rust parser is exercised against the same bytes as TS rather
    /// than a hand-built approximation.
    #[test]
    fn antigravity_replays_the_recorded_success_transcript() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../../apps/daemon/tests/fixtures/antigravity-stream-json/docs-success.stdout.ndjson",
        );
        let transcript = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let mut probe = Probe::new("antigravity");
        probe.feed(&transcript);
        probe.flush();
        // The `user_input` echo carries nothing, and the terminal `result`
        // response is not re-emitted once text has streamed.
        let types: Vec<&str> = probe.events.iter().filter_map(|event| event["type"].as_str()).collect();
        assert_eq!(
            types,
            vec!["status", "tool_use", "tool_result", "text_delta", "usage"]
        );
        assert_eq!(probe.events[1]["id"], "edb1c8c1-50ba-4f3f-87eb-412d0e9d47c3:4");
        assert_eq!(probe.events[2]["content"], "hello_headless_demo\r\n");
        assert_eq!(probe.events[3]["delta"], "Git rebase...");
        assert_eq!(probe.events[4]["usage"]["input_tokens"], 10418);
        assert_eq!(probe.events[4]["usage"]["thought_tokens"], 551);
        assert_eq!(probe.events[4]["durationMs"], 6880);
    }

    /// Parity: `json-event-stream.test.ts:2456` — frames without an `event`
    /// discriminator take the Gemini-CLI branch.
    #[test]
    fn antigravity_falls_back_to_the_gemini_branch() {
        let mut probe = Probe::new("antigravity");
        probe.feed(&frame_line(&json!({"type": "init", "session_id": "agy-1", "model": "gemini-3.5-flash"})));
        probe.feed(&frame_line(&json!({
            "type": "message",
            "role": "assistant",
            "content": "Hello from Antigravity.",
            "delta": true,
        })));
        probe.flush();
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "initializing", "model": "gemini-3.5-flash"}),
                json!({"type": "text_delta", "delta": "Hello from Antigravity."}),
            ]
        );
    }

    /// A cursor-agent assistant frame. `timestamp_ms` marks a real-time
    /// incremental delta; `model_call_id` marks a terminal full-turn replay.
    fn cursor_assistant(text: &str, timestamp_ms: Option<i64>, model_call_id: Option<&str>) -> Value {
        let mut frame = serde_json::Map::new();
        frame.insert("type".to_string(), json!("assistant"));
        if let Some(timestamp) = timestamp_ms {
            frame.insert("timestamp_ms".to_string(), json!(timestamp));
        }
        if let Some(call) = model_call_id {
            frame.insert("model_call_id".to_string(), json!(call));
        }
        frame.insert(
            "message".to_string(),
            json!({"role": "assistant", "content": [{"type": "text", "text": text}]}),
        );
        Value::Object(frame)
    }

    /// Parity: `json-event-stream.test.ts:908`.
    #[test]
    fn cursor_emits_partial_text_once_and_usage() {
        let mut probe = Probe::new("cursor-agent");
        probe.feed(&frame_line(&json!({"type": "system", "subtype": "init", "model": "GPT-5 Mini"})));
        probe.feed(&frame_line(&cursor_assistant("OD", Some(1), None)));
        probe.feed(&frame_line(&cursor_assistant("_OK", Some(2), None)));
        probe.feed(&frame_line(&cursor_assistant("OD_OK", None, None)));
        probe.feed(&frame_line(&json!({
            "type": "result",
            "duration_ms": 120,
            "usage": {"inputTokens": 5, "outputTokens": 2, "cacheReadTokens": 1, "cacheWriteTokens": 0},
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "initializing", "model": "GPT-5 Mini"}),
                json!({"type": "text_delta", "delta": "OD"}),
                json!({"type": "text_delta", "delta": "_OK"}),
                json!({
                    "type": "usage",
                    "usage": {
                        "input_tokens": 5,
                        "output_tokens": 2,
                        "cached_read_tokens": 1,
                        "cached_write_tokens": 0,
                    },
                    "durationMs": 120,
                }),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:950`.
    #[test]
    fn cursor_emits_suffix_when_the_final_assistant_extends_partial_text() {
        let mut probe = Probe::new("cursor-agent");
        probe.feed(&frame_line(&cursor_assistant("hello", Some(1), None)));
        probe.feed(&frame_line(&cursor_assistant("hello world", None, None)));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "text_delta", "delta": "hello"}),
                json!({"type": "text_delta", "delta": " world"}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:973` — a repeated fragment is real
    /// output and must not be deduped by content.
    #[test]
    fn cursor_concatenates_independent_timestamped_fragments_verbatim() {
        let mut probe = Probe::new("cursor-agent");
        probe.feed(&frame_line(&cursor_assistant("hello", Some(1), None)));
        probe.feed(&frame_line(&cursor_assistant(" world", Some(2), None)));
        probe.feed(&frame_line(&cursor_assistant(" world", Some(3), None)));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "text_delta", "delta": "hello"}),
                json!({"type": "text_delta", "delta": " world"}),
                json!({"type": "text_delta", "delta": " world"}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1011`.
    #[test]
    fn cursor_skips_a_model_call_id_replay_to_avoid_duplicate_content() {
        let mut probe = Probe::new("cursor-agent");
        probe.feed(&frame_line(&cursor_assistant("hello", Some(1), None)));
        probe.feed(&frame_line(&cursor_assistant(" world", Some(2), None)));
        probe.feed(&frame_line(&cursor_assistant("hello world", Some(3), Some("call-1"))));
        probe.feed(&frame_line(&cursor_assistant("second", Some(4), None)));
        probe.feed(&frame_line(&cursor_assistant(" turn", Some(5), None)));
        probe.feed(&frame_line(&cursor_assistant("second turn", Some(6), Some("call-2"))));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "text_delta", "delta": "hello"}),
                json!({"type": "text_delta", "delta": " world"}),
                json!({"type": "text_delta", "delta": "second"}),
                json!({"type": "text_delta", "delta": " turn"}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1097` — a dropped chunk is recovered
    /// from the replay of the SECOND turn, because the replay is reconciled
    /// against that turn's text, not the whole buffer.
    #[test]
    fn cursor_emits_the_missing_suffix_from_a_second_turn_replay() {
        let mut probe = Probe::new("cursor-agent");
        probe.feed(&frame_line(&cursor_assistant("hello world", Some(1), None)));
        probe.feed(&frame_line(&cursor_assistant("hello world", Some(2), Some("call-1"))));
        probe.feed(&frame_line(&cursor_assistant("second", Some(3), None)));
        probe.feed(&frame_line(&cursor_assistant("second turn", Some(4), Some("call-2"))));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "text_delta", "delta": "hello world"}),
                json!({"type": "text_delta", "delta": "second"}),
                json!({"type": "text_delta", "delta": " turn"}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1190`.
    #[test]
    fn cursor_does_not_duplicate_output_across_two_fallback_terminated_turns() {
        let mut probe = Probe::new("cursor-agent");
        probe.feed(&frame_line(&cursor_assistant("hello", Some(1), None)));
        probe.feed(&frame_line(&cursor_assistant("hello world", None, None)));
        probe.feed(&frame_line(&cursor_assistant("second", Some(2), None)));
        probe.feed(&frame_line(&cursor_assistant("second turn", None, None)));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "text_delta", "delta": "hello"}),
                json!({"type": "text_delta", "delta": " world"}),
                json!({"type": "text_delta", "delta": "second"}),
                json!({"type": "text_delta", "delta": " turn"}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1287` — when a middle fragment was
    /// dropped the emitted text is not a replay prefix, so nothing is appended
    /// rather than re-showing an already-visible suffix.
    #[test]
    fn cursor_leaves_an_unverifiable_replay_alone() {
        let mut probe = Probe::new("cursor-agent");
        probe.feed(&frame_line(&cursor_assistant("hello", Some(1), None)));
        probe.feed(&frame_line(&cursor_assistant(" world", Some(2), None)));
        probe.feed(&frame_line(&cursor_assistant("hello brave world", None, None)));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "text_delta", "delta": "hello"}),
                json!({"type": "text_delta", "delta": " world"}),
            ]
        );
    }

    // ── codex (`codex exec --json` / the app-server's normalised frames) ──

    /// Concatenates the `text_delta` frames, the way the chat panel does.
    fn joined_text(events: &[Value]) -> String {
        events
            .iter()
            .filter(|event| event["type"] == "text_delta")
            .filter_map(|event| event["delta"].as_str())
            .collect()
    }

    /// Parity: `json-event-stream.test.ts:1325`.
    #[test]
    fn codex_emits_status_text_and_usage() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({"type": "thread.started", "thread_id": "thr-1"})));
        probe.feed(&frame_line(&json!({"type": "turn.started"})));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {"id": "item-1", "type": "agent_message", "text": "hello"},
        })));
        probe.feed(&frame_line(&json!({
            "type": "turn.completed",
            "usage": {"input_tokens": 12, "cached_input_tokens": 4, "output_tokens": 3},
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "initializing", "sessionId": "thr-1"}),
                json!({"type": "status", "label": "thinking"}),
                json!({"type": "text_delta", "delta": "hello"}),
                json!({"type": "usage", "usage": {"input_tokens": 12, "output_tokens": 3, "cached_read_tokens": 4}}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1351` and `:1361` — the thread id is
    /// the capture-style resume handle, and its absence must stay `null` rather
    /// than becoming a spurious capture.
    #[test]
    fn codex_thread_started_surfaces_the_thread_id_or_null() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "thread.started",
            "thread_id": "019eef4f-7409-7c82-bebe-30504eed3959",
        })));
        probe.feed(&frame_line(&json!({"type": "thread.started"})));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "initializing", "sessionId": "019eef4f-7409-7c82-bebe-30504eed3959"}),
                json!({"type": "status", "label": "initializing", "sessionId": null}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1369`.
    #[test]
    fn codex_emits_thinking_status_and_reasoning_token_usage() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({"type": "turn.started"})));
        probe.feed(&frame_line(&json!({
            "type": "turn.completed",
            "usage": {
                "input_tokens": 12,
                "cached_input_tokens": 4,
                "output_tokens": 3,
                "reasoning_output_tokens": 8,
            },
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "thinking"}),
                json!({
                    "type": "usage",
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 3,
                        "thought_tokens": 8,
                        "cached_read_tokens": 4,
                    },
                }),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1400` — a distinct reasoning item
    /// starts a new summary paragraph, which the parser owns.
    #[test]
    fn codex_emits_thinking_deltas_from_reasoning_items() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({"type": "turn.started"})));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {"id": "item_0", "type": "reasoning", "text": "**Scoping the deck**\nPick 10 slides."},
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {"id": "item_2", "type": "reasoning", "text": "**Choosing a palette**"},
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "thinking"}),
                json!({"type": "thinking_delta", "delta": "**Scoping the deck**\nPick 10 slides."}),
                json!({"type": "thinking_delta", "delta": "\n\n**Choosing a palette**"}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1424` — repeated lifecycle events for
    /// one item emit only the unseen suffix.
    #[test]
    fn codex_emits_only_the_unseen_reasoning_suffix() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.started",
            "item": {"id": "item_0", "type": "reasoning", "text": ""},
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.updated",
            "item": {"id": "item_0", "type": "reasoning", "text": "Reading the brief"},
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {"id": "item_0", "type": "reasoning", "text": "Reading the brief, then drafting."},
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "thinking_delta", "delta": "Reading the brief"}),
                json!({"type": "thinking_delta", "delta": ", then drafting."}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1439` — a non-fatal error ITEM is a
    /// visible warning, not a fatal error: two sibling runs in the incident
    /// bundle carried it and completed successfully.
    #[test]
    fn codex_surfaces_non_fatal_error_items_as_a_warning() {
        let message = "Skill descriptions were shortened to fit the 2% skills context budget. \
                       Codex can still see every skill, but some descriptions are shorter.";
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {"id": "item_0", "type": "error", "message": message},
        })));
        assert_eq!(
            probe.events,
            vec![json!({"type": "status", "label": "warning", "detail": message})]
        );
    }

    /// Parity: `json-event-stream.test.ts:1457` and `:1487`.
    #[test]
    fn codex_preserves_line_boundaries_between_agent_messages() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({"type": "turn.started"})));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {"id": "item-1", "type": "agent_message", "text": "English: one"},
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {"id": "item-2", "type": "agent_message", "text": "Chinese: 一"},
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {"id": "item-3", "type": "agent_message", "text": "English: two"},
        })));
        assert_eq!(joined_text(&probe.events), "English: one\nChinese: 一\nEnglish: two");

        // An existing trailing newline is not doubled.
        let mut probe = Probe::new("codex");
        for (id, text) in [
            ("item-1", "English: one\n"),
            ("item-2", "Chinese: 一"),
            ("item-3", "\nEnglish: two"),
        ] {
            probe.feed(&frame_line(&json!({
                "type": "item.completed",
                "item": {"id": id, "type": "agent_message", "text": text},
            })));
        }
        assert_eq!(joined_text(&probe.events), "English: one\nChinese: 一\nEnglish: two");
    }

    /// Parity: `json-event-stream.test.ts:1516` — the first fatal error wins.
    #[test]
    fn codex_emits_structured_errors_once() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "error",
            "message": json!({"detail": "The 'gpt-5.5' model requires a newer version of Codex."}).to_string(),
        })));
        probe.feed(&frame_line(&json!({
            "type": "turn.failed",
            "error": {"message": "plain failure"},
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "error", "message": "The 'gpt-5.5' model requires a newer version of Codex."}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1542`.
    #[test]
    fn codex_emits_command_execution_tool_events() {
        let command = "/bin/zsh -lc 'echo hello-from-codex'";
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.started",
            "item": {
                "id": "item-1",
                "type": "command_execution",
                "command": command,
                "aggregated_output": "",
                "exit_code": null,
                "status": "in_progress",
            },
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {
                "id": "item-1",
                "type": "command_execution",
                "command": command,
                "aggregated_output": "hello-from-codex\n",
                "exit_code": 0,
                "status": "completed",
            },
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "item-1", "name": "Bash", "input": {"command": command}}),
                json!({"type": "tool_result", "toolUseId": "item-1", "content": "hello-from-codex\n", "isError": false}),
            ]
        );
    }

    const ADDED_HTML: &str = "/project/ai-design-judgment.html";
    const ADDED_MD: &str = "/project/brand-spec.md";
    const UPDATED_HTML: &str = "/project/trustworthy-design-one-pager.html";

    /// Parity: `json-event-stream.test.ts:1609` and `:1622` — one row per
    /// changed file, because the chat row model is one file per row and the web
    /// dedupes `tool_use` by id.
    #[test]
    fn codex_emits_a_tool_row_per_changed_file() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.started",
            "item": {
                "id": "item_3",
                "type": "file_change",
                "changes": [
                    {"path": ADDED_HTML, "kind": "add"},
                    {"path": ADDED_MD, "kind": "add"},
                ],
                "status": "in_progress",
            },
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {
                "id": "item_3",
                "type": "file_change",
                "changes": [
                    {"path": ADDED_HTML, "kind": "add"},
                    {"path": ADDED_MD, "kind": "add"},
                ],
                "status": "completed",
            },
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "item_3#0", "name": "Write", "input": {"file_path": ADDED_HTML}}),
                json!({"type": "tool_use", "id": "item_3#1", "name": "Write", "input": {"file_path": ADDED_MD}}),
                json!({"type": "tool_result", "toolUseId": "item_3#0", "content": "", "isError": false}),
                json!({"type": "tool_result", "toolUseId": "item_3#1", "content": "", "isError": false}),
            ]
        );

        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.started",
            "item": {
                "id": "item_9",
                "type": "file_change",
                "changes": [{"path": UPDATED_HTML, "kind": "update"}],
                "status": "in_progress",
            },
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {
                "id": "item_9",
                "type": "file_change",
                "changes": [{"path": UPDATED_HTML, "kind": "update"}],
                "status": "completed",
            },
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "item_9#0", "name": "Edit", "input": {"file_path": UPDATED_HTML}}),
                json!({"type": "tool_result", "toolUseId": "item_9#0", "content": "", "isError": false}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:1641` — the `tool_use` leaves the
    /// parser at `item.started` so the row has a real duration.
    #[test]
    fn codex_emits_the_file_change_tool_use_at_item_started() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.started",
            "item": {
                "id": "item_9",
                "type": "file_change",
                "changes": [{"path": UPDATED_HTML, "kind": "update"}],
                "status": "in_progress",
            },
        })));
        assert_eq!(
            probe.events,
            vec![json!({"type": "tool_use", "id": "item_9#0", "name": "Edit", "input": {"file_path": UPDATED_HTML}})]
        );
    }

    /// Parity: `json-event-stream.test.ts:1677`, `:1688` and `:1709`.
    #[test]
    fn codex_file_change_completed_only_failed_and_unknown_kinds() {
        // Completed-only still emits the pair.
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {
                "id": "item_9",
                "type": "file_change",
                "changes": [{"path": UPDATED_HTML, "kind": "update"}],
                "status": "completed",
            },
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "item_9#0", "name": "Edit", "input": {"file_path": UPDATED_HTML}}),
                json!({"type": "tool_result", "toolUseId": "item_9#0", "content": "", "isError": false}),
            ]
        );

        // `status: 'failed'` is the only failure signal on the item.
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {
                "id": "item_9",
                "type": "file_change",
                "changes": [{"path": UPDATED_HTML, "kind": "update"}],
                "status": "failed",
            },
        })));
        assert_eq!(
            probe.events.last(),
            Some(&json!({"type": "tool_result", "toolUseId": "item_9#0", "content": "", "isError": true}))
        );

        // A deletion stays raw: the record has no delete verb.
        let unknown = json!({
            "type": "item.completed",
            "item": {
                "id": "item_9",
                "type": "file_change",
                "changes": [{"path": UPDATED_HTML, "kind": "delete"}],
                "status": "completed",
            },
        });
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&unknown));
        assert_eq!(
            probe.events,
            vec![json!({"type": "raw", "line": unknown.to_string()})]
        );
    }

    /// Parity: `json-event-stream.test.ts:968` — a patch's two line counts ride
    /// along under `od_diff_stat` and the patch itself is dropped.
    #[test]
    fn codex_file_change_carries_a_diff_stat_instead_of_the_patch() {
        let patch = "@@ -2,3 +2,4 @@\n beta\n-gamma\n+GAMMA-1\n+GAMMA-2\n delta\n";
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {
                "id": "item_7",
                "type": "file_change",
                "changes": [{"path": "note.md", "kind": {"type": "update", "move_path": null}, "diff": patch}],
                "status": "completed",
            },
        })));
        // A structured `kind` object is not a string, so the change is not
        // recognised and the whole line stays raw.
        assert_eq!(probe.events.len(), 1);
        assert_eq!(probe.events[0]["type"], "raw");

        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {
                "id": "item_7",
                "type": "file_change",
                "changes": [{"path": "note.md", "kind": "update", "diff": patch}],
                "status": "completed",
            },
        })));
        assert_eq!(
            probe.events[0],
            json!({
                "type": "tool_use",
                "id": "item_7#0",
                "name": "Edit",
                "input": {"file_path": "note.md", "od_diff_stat": {"added": 2, "removed": 1}},
            })
        );
    }

    /// Parity: `json-event-stream.test.ts:1758`.
    #[test]
    fn codex_emits_todo_write_from_todo_list_items() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.started",
            "item": {
                "id": "item-0",
                "type": "todo_list",
                "items": [
                    {"text": "Inspect workspace", "completed": false},
                    {"text": "Write prototype", "completed": false},
                ],
            },
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.updated",
            "item": {
                "id": "item-0",
                "type": "todo_list",
                "items": [
                    {"text": "Inspect workspace", "completed": true},
                    {"text": "Write prototype", "completed": false},
                ],
            },
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({
                    "type": "tool_use",
                    "id": "item-0",
                    "name": "TodoWrite",
                    "input": {"todos": [
                        {"content": "Inspect workspace", "status": "pending"},
                        {"content": "Write prototype", "status": "pending"},
                    ]},
                }),
                json!({
                    "type": "tool_use",
                    "id": "item-0",
                    "name": "TodoWrite",
                    "input": {"todos": [
                        {"content": "Inspect workspace", "status": "completed"},
                        {"content": "Write prototype", "status": "pending"},
                    ]},
                }),
            ]
        );
    }

    fn codex_connector_command() -> &'static str {
        "od tools connectors execute --connector github --tool github.github_list_notifications --input .daily-digest-tmp/notifications.json"
    }

    /// Feeds a `command_execution` pair whose completed output is `output`.
    fn feed_connector_command(probe: &mut Probe, id: &str, output: &str) {
        probe.feed(&frame_line(&json!({
            "type": "item.started",
            "item": {
                "id": id,
                "type": "command_execution",
                "command": codex_connector_command(),
                "aggregated_output": "",
                "exit_code": null,
                "status": "in_progress",
            },
        })));
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {
                "id": id,
                "type": "command_execution",
                "command": codex_connector_command(),
                "aggregated_output": output,
                "exit_code": 1,
                "status": "failed",
            },
        })));
    }

    const CONNECTOR_ERROR_MESSAGE: &str = "Connector tool github.github_list_notifications is not allowed \
         for connector github. Re-list the connector catalog and choose one of the currently allowed \
         read-only tools.";

    /// Parity: `json-event-stream.test.ts:1814` and `:1878` — a disallowed
    /// connector tool selection becomes terminal, even after noise output.
    #[test]
    fn codex_surfaces_a_disallowed_connector_tool_selection() {
        let direct = json!({
            "ok": false,
            "status": 404,
            "error": {
                "code": "CONNECTOR_TOOL_NOT_FOUND",
                "message": "connector tool is not allowed",
                "details": {
                    "connectorId": "github",
                    "toolName": "github.github_list_notifications",
                },
            },
        })
        .to_string();
        let mut probe = Probe::new("codex");
        feed_connector_command(&mut probe, "item-connector", &format!("{direct}\n"));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "item-connector", "name": "Bash", "input": {"command": codex_connector_command()}}),
                json!({"type": "tool_result", "toolUseId": "item-connector", "content": format!("{direct}\n"), "isError": true}),
                json!({"type": "error", "message": CONNECTOR_ERROR_MESSAGE}),
            ]
        );

        let noise = json!({"event": "running", "message": "starting connector call"}).to_string();
        let mut probe = Probe::new("codex");
        feed_connector_command(
            &mut probe,
            "item-connector",
            &format!("{noise}\n{direct}\n"),
        );
        assert_eq!(
            probe.events.last(),
            Some(&json!({"type": "error", "message": CONNECTOR_ERROR_MESSAGE}))
        );
    }

    /// Parity: `json-event-stream.test.ts:1946` — the API's wrapped error shape.
    #[test]
    fn codex_surfaces_a_wrapped_connector_tool_selection() {
        let wrapped = json!({
            "error": {
                "data": {
                    "error": {
                        "code": "CONNECTOR_TOOL_NOT_FOUND",
                        "message": "connector tool is not allowed",
                        "details": {
                            "connectorId": "github",
                            "toolName": "github.github_list_notifications",
                        },
                    },
                },
            },
        })
        .to_string();
        let mut probe = Probe::new("codex");
        feed_connector_command(&mut probe, "item-connector-wrapped", &format!("{wrapped}\n"));
        assert_eq!(
            probe.events.last(),
            Some(&json!({"type": "error", "message": CONNECTOR_ERROR_MESSAGE}))
        );
    }

    /// Parity: `json-event-stream.test.ts:2019`, `:2039`, `:2062` and `:2076` —
    /// codex's own reconnect loop is a status, while a real error stays fatal
    /// even after one, and reconnect text inside a real error changes nothing.
    #[test]
    fn codex_normalizes_reconnect_errors_but_keeps_real_ones_fatal() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "error",
            "message": "Reconnecting... 2/5 (timeout waiting for child process to exit)",
        })));
        probe.feed(&frame_line(&json!({
            "type": "error",
            "message": "Reconnecting... 2/5 (stream disconnected before completion: Connection reset by peer (os error 54))",
        })));
        probe.feed(&frame_line(&json!({
            "type": "error",
            "message": "Authentication failed: invalid API key",
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "status", "label": "agent_reconnecting", "detail": "2/5"}),
                json!({"type": "status", "label": "agent_reconnecting", "detail": "2/5"}),
                json!({"type": "error", "message": "Authentication failed: invalid API key"}),
            ]
        );

        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "error",
            "message": "Authentication failed after Reconnecting... stream disconnected before completion",
        })));
        assert_eq!(
            probe.events,
            vec![json!({
                "type": "error",
                "message": "Authentication failed after Reconnecting... stream disconnected before completion",
            })]
        );
    }

    const CODEX_MCP_TOOL_CALL_STARTED: &str = "{\"type\":\"item.started\",\"item\":{\"id\":\"item_2\",\"type\":\"mcp_tool_call\",\"server\":\"echofacts\",\"tool\":\"echo_fact\",\"arguments\":{\"topic\":\"orbital-mechanics\"},\"result\":null,\"error\":null,\"status\":\"in_progress\"}}";
    const CODEX_MCP_TOOL_CALL_FAILED: &str = "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_2\",\"type\":\"mcp_tool_call\",\"server\":\"echofacts\",\"tool\":\"echo_fact\",\"arguments\":{\"topic\":\"orbital-mechanics\"},\"result\":null,\"error\":{\"message\":\"MCP tool call requires approval, but approval policy is never\"},\"status\":\"failed\"}}";
    const CODEX_WEB_SEARCH_STARTED: &str = "{\"type\":\"item.started\",\"item\":{\"id\":\"item_2\",\"type\":\"web_search\",\"id\":\"exec-9fb8985e-4163-4af2-82a2-d499ab71d18b\",\"query\":\"\",\"action\":{\"type\":\"other\"}}}";
    const CODEX_WEB_SEARCH_COMPLETED: &str = "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_2\",\"type\":\"web_search\",\"id\":\"exec-9fb8985e-4163-4af2-82a2-d499ab71d18b\",\"query\":\"OpenAI Codex CLI release notes\",\"action\":{\"type\":\"search\",\"query\":\"OpenAI Codex CLI release notes\"}}}";
    /// Duplicate `id` key: a JSON parse keeps the last, so this is the tool id.
    const CODEX_WEB_SEARCH_ID: &str = "exec-9fb8985e-4163-4af2-82a2-d499ab71d18b";
    const CODEX_MCP_TOOL_NAME: &str = "mcp__echofacts__echo_fact";

    /// Parity: `json-event-stream.test.ts:2127`, `:2156` and `:2171` — the row
    /// leaves at `item.started` (so it has a duration), and the pair is
    /// self-sufficient when only `item.completed` arrives.
    #[test]
    fn codex_emits_a_tool_row_for_an_mcp_tool_call() {
        let expected_pair = vec![
            json!({"type": "tool_use", "id": "item_2", "name": CODEX_MCP_TOOL_NAME, "input": {"topic": "orbital-mechanics"}}),
            json!({"type": "tool_result", "toolUseId": "item_2", "content": "MCP tool call requires approval, but approval policy is never", "isError": true}),
        ];

        let mut probe = Probe::new("codex");
        probe.feed(&format!("{CODEX_MCP_TOOL_CALL_STARTED}\n"));
        assert_eq!(probe.events, vec![expected_pair[0].clone()]);

        let mut probe = Probe::new("codex");
        probe.feed(&format!("{CODEX_MCP_TOOL_CALL_STARTED}\n{CODEX_MCP_TOOL_CALL_FAILED}\n"));
        assert_eq!(probe.events, expected_pair);

        let mut probe = Probe::new("codex");
        probe.feed(&format!("{CODEX_MCP_TOOL_CALL_FAILED}\n"));
        assert_eq!(probe.events, expected_pair);
    }

    /// Parity: `json-event-stream.test.ts:2322` — a half-named call must not
    /// become a row labelled `mcp____`.
    #[test]
    fn codex_leaves_a_half_named_mcp_tool_call_as_raw() {
        let line = json!({
            "type": "item.completed",
            "item": {
                "id": "item_2",
                "type": "mcp_tool_call",
                "server": "echofacts",
                "arguments": {"topic": "orbital-mechanics"},
                "result": null,
                "error": null,
                "status": "completed",
            },
        });
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&line));
        assert_eq!(probe.events, vec![json!({"type": "raw", "line": line.to_string()})]);
    }

    /// Parity: `json-event-stream.test.ts:2352` — a non-failed call is not an
    /// error, and the success payload shape is deliberately not pinned.
    #[test]
    fn codex_does_not_mark_a_non_failed_mcp_tool_call_as_an_error() {
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&json!({
            "type": "item.completed",
            "item": {
                "id": "item_2",
                "type": "mcp_tool_call",
                "server": "echofacts",
                "tool": "echo_fact",
                "arguments": {"topic": "orbital-mechanics"},
                "result": null,
                "error": null,
                "status": "completed",
            },
        })));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_use", "id": "item_2", "name": CODEX_MCP_TOOL_NAME, "input": {"topic": "orbital-mechanics"}}),
                json!({"type": "tool_result", "toolUseId": "item_2", "content": "", "isError": false}),
            ]
        );
    }

    /// Parity: `json-event-stream.test.ts:2234` and `:2271` — the started frame
    /// starts the clock through the generic early form, and the settled pair
    /// still carries the term; the client retires one into the other by id.
    #[test]
    fn codex_emits_a_search_row_for_a_completed_web_search() {
        let mut probe = Probe::new("codex");
        probe.feed(&format!("{CODEX_WEB_SEARCH_STARTED}\n{CODEX_WEB_SEARCH_COMPLETED}\n"));
        assert_eq!(
            probe.events,
            vec![
                json!({"type": "tool_in_flight", "id": CODEX_WEB_SEARCH_ID, "name": "web_search", "input": {}}),
                json!({"type": "tool_use", "id": CODEX_WEB_SEARCH_ID, "name": "web_search", "input": {"query": "OpenAI Codex CLI release notes"}}),
                json!({"type": "tool_result", "toolUseId": CODEX_WEB_SEARCH_ID, "content": "", "isError": false}),
            ]
        );

        let mut probe = Probe::new("codex");
        probe.feed(&format!("{CODEX_WEB_SEARCH_STARTED}\n"));
        assert_eq!(
            probe.events,
            vec![json!({"type": "tool_in_flight", "id": CODEX_WEB_SEARCH_ID, "name": "web_search", "input": {}})]
        );
    }

    /// Parity: `json-event-stream.test.ts:2288` — codex's action taxonomy has
    /// more members than `search`, so anything else stays raw.
    #[test]
    fn codex_leaves_a_non_search_web_search_as_raw() {
        let line = json!({
            "type": "item.completed",
            "item": {
                "id": CODEX_WEB_SEARCH_ID,
                "type": "web_search",
                "query": "OpenAI Codex CLI release notes",
                "action": {"type": "other"},
            },
        });
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&line));
        assert_eq!(probe.events, vec![json!({"type": "raw", "line": line.to_string()})]);
    }

    /// An unhandled codex frame falls back to raw (`json-event-stream.test.ts:2012`).
    #[test]
    fn codex_leaves_unhandled_events_as_raw() {
        let line = json!({"type": "unhandled.event", "foo": "bar"});
        let mut probe = Probe::new("codex");
        probe.feed(&frame_line(&line));
        assert_eq!(
            probe.events,
            vec![json!({"type": "raw", "line": line.to_string()})]
        );
    }
}
