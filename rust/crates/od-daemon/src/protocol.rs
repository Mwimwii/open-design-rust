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
        model: Option<String>,
        /// Claude/Qoder `session_id` as the wire carries it (parity: the
        /// `sessionId` field on the TS `status` event).
        session_id: Option<String>,
        /// Qoder's `qodercli_version` (parity: the `qodercliVersion` field on
        /// the TS `status` event; only the Qoder parser sets it).
        qodercli_version: Option<String>,
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
            } => {
                let mut event = serde_json::Map::new();
                event.insert("type".to_string(), Value::String("status".to_string()));
                event.insert("label".to_string(), Value::String(label.clone()));
                // `model` stays present-but-null for parity with the TS
                // parsers that always emit the key.
                event.insert(
                    "model".to_string(),
                    model.clone().map(Value::String).unwrap_or(Value::Null),
                );
                if let Some(session_id) = session_id {
                    event.insert("sessionId".to_string(), Value::String(session_id.clone()));
                }
                if let Some(qodercli_version) = qodercli_version {
                    event.insert(
                        "qodercliVersion".to_string(),
                        Value::String(qodercli_version.clone()),
                    );
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
                        model: Some(model.to_string()),
                        session_id: None,
                        qodercli_version: None,
                    });
                }
            }
            "assistant.turn_start" => on_event(StreamEvent::Status {
                label: "streaming".to_string(),
                model: None,
                session_id: None,
                qodercli_version: None,
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
                    model: Some("gpt-5.2".to_string()),
                    session_id: None,
                    qodercli_version: None,
                },
                StreamEvent::Status {
                    label: "streaming".to_string(),
                    model: None,
                    session_id: None,
                    qodercli_version: None,
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
                    on_event(StreamEvent::Status {
                        label: "initializing".to_string(),
                        model: obj
                            .get("model")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        session_id: obj
                            .get("session_id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        qodercli_version: None,
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
                let mut data = serde_json::Map::new();
                data.insert("type".to_string(), Value::String("status".to_string()));
                data.insert("label".to_string(), Value::String("streaming".to_string()));
                data.insert("ttftMs".to_string(), Value::from(ttft));
                on_event(StreamEvent::Raw {
                    line: Value::Object(data).to_string(),
                    truncated_original_bytes: None,
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
            on_event(StreamEvent::Status {
                label: "initializing".to_string(),
                model: obj.get("model").and_then(Value::as_str).map(str::to_string),
                session_id: obj
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                qodercli_version: obj
                    .get("qodercli_version")
                    .and_then(Value::as_str)
                    .map(str::to_string),
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
    }

    fn handle_line(&mut self, line: &str, on_event: &mut dyn FnMut(StreamEvent)) {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            on_event(bound_raw_line(line));
            return;
        };
        if self.kind == "opencode" && self.handle_opencode(&obj, line, on_event) {
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
        let mut probe = Probe::new("gemini");
        probe.feed("{\"type\":\"init\",\"session_id\":\"gm-1\"}\n");
        assert_eq!(
            probe.events,
            vec![json!({"type": "raw", "line": "{\"type\":\"init\",\"session_id\":\"gm-1\"}"})]
        );
    }
}
