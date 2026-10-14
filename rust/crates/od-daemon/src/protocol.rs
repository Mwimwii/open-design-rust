//! Runtime stream protocol parsers — Rust parity port of the per-agent
//! stream handlers (`apps/daemon/src/copilot-stream.ts`, and later the
//! `runtimes/*-stream.ts` family).
//!
//! Parsers turn child stdout lines into UI-lane events. Unparseable lines
//! become bounded `raw` events (parity: `boundedRawAgentEvent` in
//! `runtimes/run-event-payload-budget.ts`, simplified: byte-budget
//! truncation ladder without the cursor-field omission pass).

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
    Status { label: String, model: Option<String> },
    ThinkingDelta { delta: String },
    TextDelta { delta: String },
    ToolUse { id: Option<String>, name: Option<String>, input: Value },
    ToolResult { tool_use_id: Option<String>, content: String, is_error: bool },
    Usage { usage: Value, stop_reason: String, duration_ms: Option<i64> },
    Raw { line: String, truncated_original_bytes: Option<usize> },
}

impl StreamEvent {
    pub fn to_json(&self) -> Value {
        match self {
            StreamEvent::Status { label, model } => json!({
                "type": "status",
                "label": label,
                "model": model,
            }),
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
            } => json!({
                "type": "usage",
                "usage": usage,
                "stopReason": stop_reason,
                "durationMs": duration_ms,
            }),
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
                    });
                }
            }
            "assistant.turn_start" => on_event(StreamEvent::Status {
                label: "streaming".to_string(),
                model: None,
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
                },
                StreamEvent::Status {
                    label: "streaming".to_string(),
                    model: None,
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
