//! Run routes — parity port of `apps/daemon/src/routes/runs.ts` (the ten
//! `/api/runs*` + `/api/chat` handlers) plus the in-memory run registry they
//! drive (`apps/daemon/src/runtimes/runs.ts`).
//!
//! The TypeScript daemon runs an agent child process per run; this crate has no
//! agent registry yet, so a created run is immediately marked `running` and
//! stays there until it is canceled (its `start` SSE frame carries `null` for
//! the agent-runtime fields it cannot know). Everything else — request
//! validation, idempotency, assistant-message ownership claims, message
//! seeding, status projection, SSE replay/live fan-out, cancellation, steering
//! and the result package — follows the TypeScript routes.
//!
//! Documented gaps against the TypeScript daemon:
//!
//! * agent execution (`startChatRun`), retries, durable `events.jsonl`
//!   journals (`eventsLogPath` stays `null`) and `executionDiagnostics`;
//! * plugin snapshot resolution/authorization, OD Next strategy tasks and
//!   clarification continuations (`strategyTask` / `taskExecutionId` never
//!   appear), and `runCreated` analytics/telemetry outboxes;
//! * BYOK provider resolution and `validateRunToolBundleForAgent` (no agent
//!   defs exist here, so MCP bundles are accepted without an injection check);
//! * workspace authority gating (`authorizeProjectRequest` stays the fixture
//!   allow; headerless workspace filtering is ported);
//! * `ask-user` awaiting-input detection uses a simplified question-form
//!   scan (open + close marker) instead of the full renderer contract, and
//!   the AGUI route forwards native run events instead of running the
//!   `agui-adapter` encoder.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path as FsPath, PathBuf};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use tower_http::services::fs::AsyncReadBody;

use crate::project_dir::{self, ProjectDirError};
use crate::project_files;
use crate::routes::{api_error, internal_error, store_error_response, AppState};
use crate::storage::{self, ProjectRow, Store, StoreError};

/// Parity: `TERMINAL_RUN_STATUSES` (`runtimes/runs.ts`).
const TERMINAL_RUN_STATUSES: [&str; 3] = ["succeeded", "failed", "canceled"];

/// Parity: the daemon's global `express.json({ limit: '4mb' })`.
const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// Parity: `createChatRunService({ maxEvents })` — the in-memory ring size.
const MAX_EVENTS: usize = 2_000;

/// Parity: `SSE_KEEPALIVE_INTERVAL_MS`.
const SSE_KEEPALIVE_INTERVAL_MS: u64 = 25_000;

/// Parity: `OPEN_DESIGN_PLUGIN_ID`.
const OPEN_DESIGN_PLUGIN_ID: &str = "open-design";

/// Parity: `RUN_RESULT_PACKAGE_SCHEMA`.
const RUN_RESULT_PACKAGE_SCHEMA: &str = "open-design.run-result-package.v1";

/// Parity: `BYOK_OPENCODE_AGENT_ID`.
const BYOK_OPENCODE_AGENT_ID: &str = "byok-opencode";

/// Parity: `BYOK_OPENCODE_PROVIDER_REQUIRED_MESSAGE`.
const BYOK_OPENCODE_PROVIDER_REQUIRED_MESSAGE: &str =
    "byok-opencode runs require a complete BYOK provider configuration";

/// Agents whose `promptInputFormat` is `stream-json` in
/// `runtimes/defs/{claude,codebuddy}.ts` — the only runtimes that keep stdin
/// open past the opening prompt (parity: `agentSupportsMidTurnSteering`).
const STEERABLE_AGENTS: [&str; 2] = ["claude", "codebuddy"];

// ---- small utilities ------------------------------------------------------

/// Lock helper that survives a poisoned mutex (one panicked writer must not
/// make every later request a 500).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

/// Parity: `nowMs()`/`Date.now()`.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// UUID-v4 shaped id, matching `randomUUID()` without a `uuid` dependency.
fn random_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    for (chunk, seed) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(counter);
        hasher.write_u64(chunk as u64);
        hasher.write_u64(now_ms() as u64);
        hasher.write_u32(std::process::id());
        seed.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// ---- SHA-256 (parity: `createHash('sha256')`) ------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Hex SHA-256 of `data` — byte-parity with Node's `createHash('sha256')`.
fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (index, word) in w.iter_mut().enumerate().take(16) {
            let start = index * 4;
            *word = u32::from_be_bytes([
                block[start],
                block[start + 1],
                block[start + 2],
                block[start + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

/// Parity: `canonicalJsonValue` — recursively sorted object keys.
fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), canonical_json(&map[key]));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

// ---- error envelopes ------------------------------------------------------

/// `{ error: { code, message } }`.
fn api_err(status: StatusCode, code: &str, message: &str) -> Box<Response> {
    Box::new(api_error(status, code, message))
}

/// `sendApiError(res, status, code, message, init)` — `init` keys (`retryable`,
/// `details`, `requestId`, …) spread into the error object.
fn api_err_init(status: StatusCode, code: &str, message: &str, init: Value) -> Response {
    let mut error = Map::new();
    error.insert("code".to_string(), json!(code));
    error.insert("message".to_string(), json!(message));
    if let Value::Object(extra) = init {
        for (key, value) in extra {
            error.insert(key, value);
        }
    }
    (status, Json(json!({ "error": Value::Object(error) }))).into_response()
}

fn store_err(err: &StoreError) -> Box<Response> {
    Box::new(store_error_response(err))
}

/// Parity: `SandboxImportedProjectError` → 400, unresolved id → 500.
fn project_dir_error_response(err: ProjectDirError) -> Response {
    match err {
        ProjectDirError::Rejected(message) => {
            api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &message)
        }
        ProjectDirError::Unresolved(message) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "PROJECT_DIR_UNRESOLVED",
            &message,
        ),
    }
}

// ---- request plumbing -----------------------------------------------------

/// Express only parses bodies declared as JSON; anything else reaches the
/// handler with `req.body` unset, i.e. `{}` here.
async fn read_json_body(request: Request) -> Result<Value, Box<Response>> {
    if !is_json_content_type(request.headers()) {
        return Ok(Value::Object(Map::new()));
    }
    let bytes = match to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(api_err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "PAYLOAD_TOO_LARGE",
                "request entity too large",
            ));
        }
    };
    if bytes.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let parsed: Value = serde_json::from_slice(&bytes)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "invalid json body"))?;
    if !parsed.is_object() && !parsed.is_array() {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "invalid json body",
        ));
    }
    Ok(parsed)
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let essence = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    essence == "application/json" || essence.ends_with("+json")
}

/// `toJsonRecord`: only plain objects survive; arrays/null become `{}`.
fn as_record(value: Option<&Value>) -> Option<&Map<String, Value>> {
    match value {
        Some(Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn string_field(map: Option<&Map<String, Value>>, key: &str) -> Option<String> {
    map?
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn nonempty_string(map: Option<&Map<String, Value>>, key: &str) -> Option<String> {
    string_field(map, key).filter(|value| !value.is_empty())
}

fn is_safe_id(value: &str) -> bool {
    storage::is_safe_id(value)
}

/// Parity: `normalizeConversationSessionMode` — `chat`/`plan` survive, every
/// other value collapses to `design`.
fn normalize_session_mode(value: Option<&str>) -> Option<String> {
    match value {
        Some("chat") | Some("plan") => Some(value.unwrap().to_string()),
        _ => Some("design".to_string()),
    }
}

// ---- media execution policy (parity: `media/policy.ts`) -------------------

fn default_media_execution_policy() -> Value {
    json!({ "mode": "enabled" })
}

enum MediaParse {
    Ok(Value),
    Err(String),
}

/// Parity: `parseMediaExecutionPolicyInput`.
fn parse_media_execution_policy_input(value: Option<&Value>) -> MediaParse {
    match value {
        None | Some(Value::Null) => MediaParse::Ok(default_media_execution_policy()),
        Some(Value::Object(input)) => {
            let raw_mode = input
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("enabled");
            if raw_mode != "enabled" && raw_mode != "disabled" {
                return MediaParse::Err("mediaExecution.mode must be enabled or disabled".to_string());
            }
            let mut policy = Map::new();
            policy.insert("mode".to_string(), json!(raw_mode));
            if let Some(surfaces) = input.get("allowedSurfaces") {
                let Value::Array(items) = surfaces else {
                    return MediaParse::Err(
                        "mediaExecution.allowedSurfaces must be an array".to_string(),
                    );
                };
                let mut out: Vec<Value> = Vec::new();
                for item in items {
                    let Some(surface) = item.as_str() else {
                        return MediaParse::Err(
                            "mediaExecution.allowedSurfaces may only include image, video, or audio"
                                .to_string(),
                        );
                    };
                    if surface != "image" && surface != "video" && surface != "audio" {
                        return MediaParse::Err(
                            "mediaExecution.allowedSurfaces may only include image, video, or audio"
                                .to_string(),
                        );
                    }
                    if !out.iter().any(|existing| existing.as_str() == Some(surface)) {
                        out.push(json!(surface));
                    }
                }
                policy.insert("allowedSurfaces".to_string(), Value::Array(out));
            }
            if let Some(models) = input.get("allowedModels") {
                let Value::Array(items) = models else {
                    return MediaParse::Err(
                        "mediaExecution.allowedModels must be an array".to_string(),
                    );
                };
                let mut out: Vec<Value> = Vec::new();
                for item in items {
                    let Some(model) = item.as_str() else {
                        return MediaParse::Err(
                            "mediaExecution.allowedModels must contain non-empty strings"
                                .to_string(),
                        );
                    };
                    let model = model.trim();
                    if model.is_empty() {
                        return MediaParse::Err(
                            "mediaExecution.allowedModels must contain non-empty strings"
                                .to_string(),
                        );
                    }
                    if !out.iter().any(|existing| existing.as_str() == Some(model)) {
                        out.push(json!(model));
                    }
                }
                policy.insert("allowedModels".to_string(), Value::Array(out));
            }
            MediaParse::Ok(Value::Object(policy))
        }
        Some(_) => MediaParse::Err("mediaExecution must be an object when provided".to_string()),
    }
}

// ---- run tool bundle (parity: `run-tool-bundle.ts`) -----------------------

enum ToolBundleParse {
    Ok(Value),
    Err(String),
}

/// Parity: `SERVER_ID_PATTERN` — case-insensitive, 1..=64 chars.
fn mcp_server_id_ok(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-'))
}

/// Parity: `parseRunToolBundleForRequest` (server-side sanitization reduced to
/// the fields this crate reports back; URL parsing is a scheme check).
fn parse_run_tool_bundle(value: Option<&Value>) -> ToolBundleParse {
    fn invalid(index: usize) -> ToolBundleParse {
        ToolBundleParse::Err(format!("toolBundle.mcpServers[{index}] is invalid"))
    }

    let empty = json!({ "mcpServers": [] });
    match value {
        None | Some(Value::Null) => ToolBundleParse::Ok(empty),
        Some(Value::Object(input)) => match input.get("mcpServers") {
            None | Some(Value::Null) => ToolBundleParse::Ok(empty),
            Some(Value::Array(items)) => {
                let mut seen: Vec<String> = Vec::new();
                let mut servers = Vec::new();
                for (index, entry) in items.iter().enumerate() {
                    let Some(server) = entry.as_object() else {
                        return invalid(index);
                    };
                    let id = server
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or("");
                    if !mcp_server_id_ok(id) {
                        return invalid(index);
                    }
                    let transport = match server.get("transport") {
                        None | Some(Value::Null) => "stdio",
                        Some(Value::String(text)) => match text.as_str() {
                            "stdio" | "sse" | "http" => text.as_str(),
                            _ => return invalid(index),
                        },
                        Some(_) => return invalid(index),
                    };
                    let mut sanitized = Map::new();
                    sanitized.insert("id".to_string(), json!(id));
                    sanitized.insert("transport".to_string(), json!(transport));
                    sanitized.insert(
                        "enabled".to_string(),
                        json!(server.get("enabled").map(|value| value != &json!(false)).unwrap_or(true)),
                    );
                    for key in ["label", "templateId"] {
                        if let Some(text) = server.get(key).and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                sanitized.insert(key.to_string(), json!(text.trim()));
                            }
                        }
                    }
                    if transport == "stdio" {
                        let command = server
                            .get("command")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .unwrap_or("");
                        if command.is_empty() {
                            return invalid(index);
                        }
                        sanitized.insert("command".to_string(), json!(command));
                        if let Some(Value::Array(args)) = server.get("args") {
                            let cleaned: Vec<Value> = args
                                .iter()
                                .filter(|value| value.as_str().is_some())
                                .cloned()
                                .collect();
                            if !cleaned.is_empty() {
                                sanitized.insert("args".to_string(), Value::Array(cleaned));
                            }
                        }
                    } else {
                        let url = server
                            .get("url")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .unwrap_or("");
                        if url.is_empty()
                            || !(url.starts_with("http://") || url.starts_with("https://"))
                        {
                            return invalid(index);
                        }
                        sanitized.insert("url".to_string(), json!(url));
                        // Parity: `sanitizeMcpAuthMode(raw) ?? inferMcpAuthModeForUrl`;
                        // URL-based inference is not ported, so only an explicit
                        // `none`/`oauth` survives (documented in the module doc).
                        if let Some(auth) = server.get("authMode").and_then(Value::as_str) {
                            if matches!(auth, "none" | "oauth") {
                                sanitized.insert("authMode".to_string(), json!(auth));
                            }
                        }
                    }
                    if seen.iter().any(|existing| existing == id) {
                        return ToolBundleParse::Err(format!(
                            "toolBundle.mcpServers[{index}] duplicates server id \"{id}\""
                        ));
                    }
                    seen.push(id.to_string());
                    servers.push(Value::Object(sanitized));
                }
                ToolBundleParse::Ok(json!({ "mcpServers": servers }))
            }
            Some(_) => ToolBundleParse::Err("toolBundle.mcpServers must be an array".to_string()),
        },
        Some(_) => ToolBundleParse::Err("toolBundle must be an object".to_string()),
    }
}

/// Parity: `summarizeRunToolBundle`.
fn summarize_tool_bundle(bundle: &Value) -> Value {
    let servers = bundle
        .get("mcpServers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let summarized: Vec<Value> = servers
        .iter()
        .filter_map(Value::as_object)
        .map(|server| {
            let mut out = Map::new();
            for key in ["id", "transport", "enabled"] {
                if let Some(value) = server.get(key) {
                    out.insert(key.to_string(), value.clone());
                }
            }
            for key in ["label", "templateId", "authMode"] {
                if let Some(value) = server.get(key) {
                    let present = js_truthy(value)
                        && value.as_str().is_none_or(|text| !text.trim().is_empty());
                    if present {
                        out.insert(key.to_string(), value.clone());
                    }
                }
            }
            Value::Object(out)
        })
        .collect();
    json!({ "mcpServers": summarized })
}

// ---- plugin workflow correlation (parity: `mcp-observability.ts`) ---------

fn is_uuid_or_ulid(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 {
        return false;
    }
    let bytes = value.as_bytes();
    if value.len() == 36 {
        let groups: [usize; 5] = [8, 4, 4, 4, 12];
        let mut offset = 0;
        for (index, width) in groups.iter().enumerate() {
            if index > 0 {
                if bytes.get(offset) != Some(&b'-') {
                    return false;
                }
                offset += 1;
            }
            let slice = &value[offset..offset + width];
            if !slice.chars().all(|c| c.is_ascii_hexdigit()) {
                return false;
            }
            offset += width;
        }
        // `WORKFLOW_ID_PATTERN` variant/version nibble + RFC-4122 variant.
        let version = bytes[14] as char;
        if !('1'..='8').contains(&version) {
            return false;
        }
        let variant = bytes[19] as char;
        return matches!(variant, '8' | '9' | 'a' | 'b' | 'A' | 'B');
    }
    if value.len() == 26 {
        return value.chars().all(|c| {
            matches!(c, '0'..='9' | 'A'..='H' | 'J'..='K' | 'M'..='N' | 'P'..='T' | 'V'..='Z')
        });
    }
    false
}

/// Parity: `validatePluginWorkflowId` → `Ok(id)` or the contract message.
fn validate_plugin_workflow_id(value: Option<&Value>) -> Result<String, String> {
    let text = value.and_then(Value::as_str).unwrap_or("");
    if text.len() > 64 || !is_uuid_or_ulid(text) {
        return Err("pluginWorkflowId must be a canonical UUID or ULID".to_string());
    }
    Ok(text.to_string())
}

const ALLOWED_DISTRIBUTION: [&str; 4] = ["git_marketplace", "local_repo", "manual", "unknown"];
const ALLOWED_PUBLISHER: [&str; 3] = ["open_design_first_party", "third_party", "unknown"];
const ALLOWED_HOST_PRODUCT: [&str; 5] = [
    "codex_desktop",
    "codex_cli",
    "codex_unknown",
    "claude_code",
    "unknown",
];

/// Parity: `normalizeExternalPluginRunAnalyticsHints` (analytics-context
/// correlation is always `false` here — no analytics context exists in Rust).
fn normalize_external_plugin_hints(
    hints: &Map<String, Value>,
    client_request_id: Option<&str>,
) -> Result<Value, String> {
    if hints.get("entrySurface").and_then(Value::as_str) != Some("external_mcp") {
        return Err("entrySurface must be external_mcp".to_string());
    }
    let host_product = hints.get("hostProduct").and_then(Value::as_str).unwrap_or("");
    if !ALLOWED_HOST_PRODUCT.contains(&host_product) {
        return Err("hostProduct is invalid".to_string());
    }
    if hints.get("externalPluginId").and_then(Value::as_str) != Some(OPEN_DESIGN_PLUGIN_ID) {
        return Err(format!("id must be {OPEN_DESIGN_PLUGIN_ID}"));
    }
    let version = hints
        .get("externalPluginVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version_ok = version.len() <= 64
        && !version.is_empty()
        && version
            .split('.')
            .count()
            .checked_sub(1)
            .map(|dots| dots >= 2)
            .unwrap_or(false);
    if !version_ok
        || !version.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')
        })
    {
        return Err("version must be a bounded semver string".to_string());
    }
    let distribution = hints
        .get("distributionMechanism")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !ALLOWED_DISTRIBUTION.contains(&distribution) {
        return Err("distributionMechanism is invalid".to_string());
    }
    let publisher = hints.get("publisherClass").and_then(Value::as_str).unwrap_or("");
    if !ALLOWED_PUBLISHER.contains(&publisher) {
        return Err("publisherClass is invalid".to_string());
    }
    if let Some(quality) = hints.get("attributionQuality").and_then(Value::as_str) {
        if quality != "self_reported" && quality != "session_correlated" {
            return Err("attributionQuality is invalid".to_string());
        }
    }
    let workflow_id = validate_plugin_workflow_id(hints.get("pluginWorkflowId"))?;
    if let Some(brief_state) = hints.get("briefState").and_then(Value::as_str) {
        if !matches!(brief_state, "confirmed" | "skipped" | "not_applicable") {
            return Err("briefState is invalid".to_string());
        }
    }
    let client_request_id = client_request_id.unwrap_or("");
    if client_request_id.is_empty() {
        return Err("clientRequestId is required for plugin runs".to_string());
    }
    // Parity: `logicalPluginRequestDigest`.
    let expected = sha256_hex(
        format!("od-plugin-logical-request:v1:{client_request_id}").as_bytes(),
    );
    if hints.get("logicalRequestDigestVersion").and_then(Value::as_i64) != Some(1)
        || hints.get("logicalRequestDigest").and_then(Value::as_str) != Some(expected.as_str())
    {
        return Err("logical request digest does not match clientRequestId".to_string());
    }
    let mut out = Map::new();
    out.insert("entrySurface".to_string(), json!("external_mcp"));
    out.insert("hostProduct".to_string(), json!(host_product));
    out.insert("externalPluginId".to_string(), json!(OPEN_DESIGN_PLUGIN_ID));
    out.insert("externalPluginVersion".to_string(), json!(version));
    out.insert("distributionMechanism".to_string(), json!(distribution));
    out.insert("publisherClass".to_string(), json!(publisher));
    out.insert("attributionQuality".to_string(), json!("self_reported"));
    out.insert("pluginWorkflowId".to_string(), json!(workflow_id));
    out.insert("logicalRequestDigest".to_string(), json!(expected));
    out.insert("logicalRequestDigestVersion".to_string(), json!(1));
    if let Some(brief_state) = hints.get("briefState") {
        out.insert("briefState".to_string(), brief_state.clone());
    }
    Ok(Value::Object(out))
}

/// Parity: `externalPluginAttributionMismatch`.
fn external_plugin_attribution_mismatch(existing: Option<&Value>, incoming: Option<&Value>) -> bool {
    const KEYS: [&str; 10] = [
        "entrySurface",
        "hostProduct",
        "externalPluginId",
        "externalPluginVersion",
        "distributionMechanism",
        "publisherClass",
        "attributionQuality",
        "pluginWorkflowId",
        "logicalRequestDigest",
        "logicalRequestDigestVersion",
    ];
    let existing = existing.and_then(Value::as_object);
    let incoming = incoming.and_then(Value::as_object);
    let existing_is_plugin = existing
        .and_then(|map| map.get("externalPluginId"))
        .and_then(Value::as_str)
        == Some(OPEN_DESIGN_PLUGIN_ID);
    let incoming_is_plugin = incoming
        .and_then(|map| map.get("externalPluginId"))
        .and_then(Value::as_str)
        == Some(OPEN_DESIGN_PLUGIN_ID);
    if !existing_is_plugin && !incoming_is_plugin {
        return false;
    }
    if existing_is_plugin != incoming_is_plugin {
        return true;
    }
    let (Some(existing), Some(incoming)) = (existing, incoming) else {
        return false;
    };
    KEYS.iter().any(|key| existing.get(*key) != incoming.get(*key))
}

// ---- SSE transport --------------------------------------------------------

#[derive(Debug, Default)]
struct FeedState {
    buffer: Vec<u8>,
    closed: bool,
    waker: Option<Waker>,
}

impl FeedState {
    fn push(&mut self, bytes: &[u8]) {
        if self.closed {
            return;
        }
        self.buffer.extend_from_slice(bytes);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    fn close(&mut self) {
        self.closed = true;
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

/// One live SSE connection: the shared feed the reader drains plus the
/// registry coordinates needed to unsubscribe when the client goes away.
///
/// Parity: `createSseResponse` — an `http_body::Body` fed by an `AsyncRead`
/// over a shared queue, so events can be pushed while the response streams.
struct FeedReader {
    feed: Arc<Mutex<FeedState>>,
    detach: Option<(Runs, String, u64)>,
}

impl tokio::io::AsyncRead for FeedReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut state = lock(&this.feed);
        if state.buffer.is_empty() {
            if state.closed {
                return Poll::Ready(Ok(()));
            }
            state.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let count = state.buffer.len().min(buf.remaining());
        buf.put_slice(&state.buffer[..count]);
        state.buffer.drain(..count);
        Poll::Ready(Ok(()))
    }
}

impl Drop for FeedReader {
    fn drop(&mut self) {
        lock(&self.feed).close();
        if let Some((runs, run_id, subscriber_id)) = self.detach.take() {
            runs.unsubscribe(&run_id, subscriber_id);
        }
    }
}

/// Parity: `createSseResponse` headers + frame writer.
fn sse_frame(id: Option<u64>, event: &str, data: &Value) -> String {
    let id_line = match id {
        Some(id) => format!("id: {id}\n"),
        None => String::new(),
    };
    format!("{id_line}event: {event}\ndata: {data}\n\n")
}

// ---- run registry ---------------------------------------------------------

#[derive(Debug, Clone)]
struct RunEvent {
    id: u64,
    event: String,
    data: Value,
    timestamp: i64,
}

#[derive(Debug, Clone)]
struct Run {
    id: String,
    project_id: Option<String>,
    conversation_id: Option<String>,
    assistant_message_id: Option<String>,
    client_request_id: Option<String>,
    request_fingerprint: Option<String>,
    agent_id: Option<String>,
    project_metadata: Option<Value>,
    applied_plugin_snapshot_id: Option<String>,
    plugin_id: Option<String>,
    media_execution: Value,
    tool_bundle: Value,
    browser_use: Option<Value>,
    session_mode: Option<String>,
    context: Option<Value>,
    external_plugin_analytics: Option<Value>,
    client_type: Option<String>,
    status: String,
    created_at: i64,
    updated_at: i64,
    terminal_at: Option<i64>,
    events: Vec<RunEvent>,
    next_event_id: u64,
    cancel_requested: bool,
    cancel_origin: Option<String>,
    terminal_trigger: Option<String>,
    exit_code: Option<i64>,
    signal: Option<String>,
    error: Option<String>,
    error_code: Option<String>,
    failure_category: Option<String>,
    failure_detail: Option<String>,
    failure_action: Option<String>,
    retryable: Option<bool>,
    ended_with_unfinished_work: bool,
    stdin_open: bool,
    artifact_count: Option<i64>,
    deliverable_valid: Option<bool>,
    deliverable_validation: Option<String>,
    deliverable_entry_file: Option<String>,
    deliverable_artifact_kind: Option<String>,
    subscribers: HashMap<u64, Arc<Mutex<FeedState>>>,
}

impl Run {
    fn is_terminal(&self) -> bool {
        TERMINAL_RUN_STATUSES.contains(&self.status.as_str())
    }

    fn new(meta: &Map<String, Value>) -> Self {
        let now = now_ms();
        let tool_bundle = meta
            .get("toolBundle")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| json!({ "mcpServers": [] }));
        Self {
            id: random_id(),
            project_id: nonempty_string(Some(meta), "projectId"),
            conversation_id: nonempty_string(Some(meta), "conversationId"),
            assistant_message_id: nonempty_string(Some(meta), "assistantMessageId"),
            client_request_id: nonempty_string(Some(meta), "clientRequestId"),
            request_fingerprint: nonempty_string(Some(meta), "requestFingerprint"),
            agent_id: nonempty_string(Some(meta), "agentId"),
            project_metadata: meta.get("projectMetadata").cloned(),
            applied_plugin_snapshot_id: nonempty_string(Some(meta), "appliedPluginSnapshotId"),
            plugin_id: nonempty_string(Some(meta), "pluginId"),
            media_execution: match parse_media_execution_policy_input(meta.get("mediaExecution")) {
                MediaParse::Ok(policy) => policy,
                MediaParse::Err(_) => default_media_execution_policy(),
            },
            tool_bundle,
            browser_use: match meta.get("browserUse") {
                Some(value @ Value::Object(_)) => Some(value.clone()),
                _ => None,
            },
            session_mode: nonempty_string(Some(meta), "sessionMode")
                .filter(|value| matches!(value.as_str(), "chat" | "design" | "plan")),
            context: match meta.get("context") {
                Some(value @ Value::Object(_)) => Some(value.clone()),
                _ => None,
            },
            external_plugin_analytics: None,
            client_type: None,
            status: "queued".to_string(),
            created_at: now,
            updated_at: now,
            terminal_at: None,
            events: Vec::new(),
            next_event_id: 1,
            cancel_requested: false,
            cancel_origin: None,
            terminal_trigger: None,
            exit_code: None,
            signal: None,
            error: None,
            error_code: None,
            failure_category: None,
            failure_detail: None,
            failure_action: None,
            retryable: None,
            ended_with_unfinished_work: false,
            stdin_open: false,
            artifact_count: None,
            deliverable_valid: None,
            deliverable_validation: None,
            deliverable_entry_file: None,
            deliverable_artifact_kind: None,
            subscribers: HashMap::new(),
        }
    }
}

#[derive(Default)]
struct Registry {
    runs: HashMap<String, Run>,
    order: Vec<String>,
    by_client_request: HashMap<String, String>,
    by_workflow: HashMap<String, String>,
    next_subscriber: u64,
}

/// The in-memory run registry shared by every route (parity:
/// `design.runs`). Runs are process-local: a daemon restart forgets them,
/// matching the TypeScript v1 in-memory registry minus its durable journal.
#[derive(Clone, Default)]
pub struct Runs {
    inner: Arc<Mutex<Registry>>,
}

#[derive(Debug)]
enum CreateOutcome {
    Created(String),
    Reused(String),
    Conflict(String),
}

#[derive(Debug, PartialEq, Eq)]
enum ClaimOutcome {
    Claimed,
    Rejected,
}

impl Runs {
    fn get(&self, id: &str) -> Option<Run> {
        lock(&self.inner).runs.get(id).cloned()
    }

    fn list(
        &self,
        project_id: Option<&str>,
        conversation_id: Option<&str>,
        status: Option<&str>,
    ) -> Vec<Run> {
        let registry = lock(&self.inner);
        let mut runs = Vec::new();
        for id in &registry.order {
            let Some(run) = registry.runs.get(id) else {
                continue;
            };
            if let Some(project_id) = project_id {
                if run.project_id.as_deref() != Some(project_id) {
                    continue;
                }
            }
            if let Some(conversation_id) = conversation_id {
                if run.conversation_id.as_deref() != Some(conversation_id) {
                    continue;
                }
            }
            match status {
                // Parity: `status === 'active'` is the "not terminal yet"
                // pseudo-status, not a literal run state to match.
                Some("active") => {
                    if run.is_terminal() {
                        continue;
                    }
                }
                Some(status) if run.status != status => continue,
                _ => {}
            }
            runs.push(run.clone());
        }
        runs
    }

    fn find_by_workflow(&self, plugin_workflow_id: &str) -> Option<Run> {
        let registry = lock(&self.inner);
        let id = registry.by_workflow.get(plugin_workflow_id)?;
        registry.runs.get(id).cloned()
    }

    /// Parity: `createOrReuse`.
    fn create_or_reuse(&self, meta: &Map<String, Value>) -> CreateOutcome {
        let mut registry = lock(&self.inner);
        if let Some(client_request_id) = nonempty_string(Some(meta), "clientRequestId") {
            if let Some(existing_id) = registry.by_client_request.get(&client_request_id).cloned() {
                if let Some(existing) = registry.runs.get(&existing_id) {
                    let fingerprint = nonempty_string(Some(meta), "requestFingerprint");
                    if let (Some(incoming), Some(known)) =
                        (fingerprint.as_deref(), existing.request_fingerprint.as_deref())
                    {
                        if incoming != known {
                            return CreateOutcome::Conflict(existing_id);
                        }
                    }
                    return CreateOutcome::Reused(existing_id);
                }
            }
        }
        let mut run = Run::new(meta);
        let id = run.id.clone();
        if run.external_plugin_analytics.is_none() {
            run.external_plugin_analytics = analytics_from_meta(meta);
        }
        registry.order.push(id.clone());
        if let Some(client_request_id) = run.client_request_id.clone() {
            registry
                .by_client_request
                .insert(client_request_id, id.clone());
        }
        if let Some(workflow_id) = run
            .external_plugin_analytics
            .as_ref()
            .and_then(|analytics| analytics.get("pluginWorkflowId"))
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            registry.by_workflow.insert(workflow_id, id.clone());
        }
        registry.runs.insert(id.clone(), run);
        CreateOutcome::Created(id)
    }

    /// Undo an optimistically-created run (parity: `drop`).
    fn drop_run(&self, id: &str) {
        let mut registry = lock(&self.inner);
        if let Some(run) = registry.runs.get(id) {
            if run.is_terminal() {
                return;
            }
        } else {
            return;
        }
        if let Some(run) = registry.runs.remove(id) {
            registry.order.retain(|entry| entry != id);
            if let Some(client_request_id) = run.client_request_id {
                registry.by_client_request.remove(&client_request_id);
            }
        }
    }

    /// Append to the ring buffer and stamp `updatedAt` (parity: `emit`).
    fn push_event(&self, run: &mut Run, event: &str, data: Value) -> RunEvent {
        let record = RunEvent {
            id: run.next_event_id,
            event: event.to_string(),
            data,
            timestamp: now_ms(),
        };
        run.next_event_id += 1;
        run.events.push(record.clone());
        if run.events.len() > MAX_EVENTS {
            let excess = run.events.len() - MAX_EVENTS;
            run.events.drain(..excess);
        }
        run.updated_at = record.timestamp;
        record
    }

    /// Parity: `finish` → `commitFinish` (single terminal choke point).
    fn finish(&self, run_id: &str, status: &str, code: Option<i64>, signal: Option<&str>) {
        let mut registry = lock(&self.inner);
        let Some(run) = registry.runs.get_mut(run_id) else {
            return;
        };
        if run.is_terminal() {
            return;
        }
        let terminal_at = now_ms();
        run.status = status.to_string();
        run.exit_code = code;
        run.signal = signal.map(str::to_string);
        run.updated_at = terminal_at;
        run.terminal_at = Some(terminal_at);
        let end_data = json!({
            "code": code,
            "signal": signal,
            "status": status,
            "terminalAt": terminal_at,
            "resumable": false,
            "endedWithUnfinishedWork": run.ended_with_unfinished_work,
            "failureCategory": run.failure_category,
            "failureDetail": run.failure_detail,
            "failureAction": run.failure_action,
            "retryable": run.retryable,
        });
        let record = self.push_event(run, "end", end_data);
        let subscribers = std::mem::take(&mut run.subscribers);
        for feed in subscribers.values() {
            let frame = sse_frame(Some(record.id), &record.event, &record.data);
            let mut state = lock(feed);
            state.push(frame.as_bytes());
            state.close();
        }
    }

    /// Parity: `cancel(run, origin)` for a run with no child process.
    fn cancel(&self, run_id: &str, origin: &str) {
        let mut registry = lock(&self.inner);
        let Some(run) = registry.runs.get_mut(run_id) else {
            return;
        };
        if run.is_terminal() {
            return;
        }
        run.cancel_requested = true;
        run.cancel_origin = Some(origin.to_string());
        run.updated_at = now_ms();
        run.stdin_open = false;
        drop(registry);
        // Parity: `commitFinish` sends `end` to every live client before
        // `sse.end()`, so `finish` (not this path) owns subscriber teardown.
        self.finish(run_id, "canceled", None, Some("SIGTERM"));
    }

    fn set_client_type(&self, run_id: &str, client_type: &str) {
        let mut registry = lock(&self.inner);
        if let Some(run) = registry.runs.get_mut(run_id) {
            run.client_type = Some(client_type.to_string());
        }
    }

    fn set_deliverable(
        &self,
        run_id: &str,
        valid: bool,
        validation: &str,
        entry_file: Option<String>,
        artifact_kind: Option<String>,
    ) {
        let mut registry = lock(&self.inner);
        if let Some(run) = registry.runs.get_mut(run_id) {
            run.deliverable_valid = Some(valid);
            run.deliverable_validation = Some(validation.to_string());
            run.deliverable_entry_file = entry_file;
            run.deliverable_artifact_kind = artifact_kind;
        }
    }

    fn unsubscribe(&self, run_id: &str, subscriber_id: u64) {
        let mut registry = lock(&self.inner);
        if let Some(run) = registry.runs.get_mut(run_id) {
            run.subscribers.remove(&subscriber_id);
        }
    }

    /// Parity: `isRunActive` in `createInternalRunCreationService`.
    fn is_run_active(&self, run_id: &str) -> bool {
        self.get(run_id)
            .map(|run| !run.is_terminal())
            .unwrap_or(false)
    }

    /// Parity: `pinAssistantMessageOnRunCreate` — the atomic ownership claim
    /// that decides whether this run may pin/finalize the assistant row.
    ///
    /// `Store` exposes no transaction handle, so the single
    /// `better-sqlite3` immediate transaction is reduced to one guarded
    /// `UPDATE` (plus an `INSERT ... ON CONFLICT DO NOTHING` for a fresh id):
    /// both statements re-assert the ownership predicate themselves, so a
    /// losing request still observes `changes == 0`.
    fn claim_assistant_message(
        &self,
        store: &Store,
        run: &Run,
        seed: Option<&Value>,
    ) -> Result<ClaimOutcome, StoreError> {
        let (Some(conversation_id), Some(assistant_message_id)) =
            (run.conversation_id.as_deref(), run.assistant_message_id.as_deref())
        else {
            return Ok(ClaimOutcome::Claimed);
        };
        let claim_status = run.status.as_str();
        let existing = store.query_one(
            "SELECT run_id AS runId, run_status AS runStatus, role, \
                    conversation_id AS conversationId \
               FROM messages WHERE id = ?1",
            [assistant_message_id],
            |row| {
                Ok::<_, rusqlite::Error>((
                    row.get::<_, Option<String>>("runId")?,
                    row.get::<_, Option<String>>("runStatus")?,
                    row.get::<_, Option<String>>("role")?,
                    row.get::<_, Option<String>>("conversationId")?,
                ))
            },
        )?;
        let Some((existing_run_id, existing_run_status, role, existing_conversation)) = existing
        else {
            // Fresh id: seed the user turn first so its position lands before
            // the assistant row this claim is about to insert.
            if let Some(seed) = seed {
                upsert_message(store, conversation_id, seed)?;
            }
            let agent_id = run.agent_id.as_deref();
            let session_mode = normalized_message_session_mode(
                run.session_mode.as_deref(),
            );
            let run_context = run.context.as_ref().map(Value::to_string);
            let inserted = store.execute(
                "INSERT INTO messages \
                   (id, conversation_id, role, content, agent_id, run_id, run_status, \
                    session_mode, run_context_json, started_at, position, created_at) \
                 VALUES (?1, ?2, 'assistant', '', ?3, ?4, ?5, ?6, ?7, ?8, \
                    (SELECT COALESCE(MAX(position), -1) + 1 FROM messages WHERE conversation_id = ?2), \
                    ?9) \
                 ON CONFLICT(id) DO NOTHING",
                rusqlite::params![
                    assistant_message_id,
                    conversation_id,
                    agent_id,
                    run.id,
                    claim_status,
                    session_mode,
                    run_context,
                    run.created_at,
                    now_ms()
                ],
            )?;
            return Ok(if inserted > 0 {
                ClaimOutcome::Claimed
            } else {
                ClaimOutcome::Rejected
            });
        };
        // Scope guard (the route pre-filters these cases; this re-asserts).
        if role.as_deref() != Some("assistant") || existing_conversation.as_deref() != Some(conversation_id)
        {
            return Ok(ClaimOutcome::Rejected);
        }
        let is_same_run = existing_run_id.as_deref() == Some(run.id.as_str());
        let active_looking = existing_run_id.is_some()
            && !is_same_run
            && matches!(existing_run_status.as_deref(), Some("queued") | Some("running"));
        let existing_still_active = active_looking
            && existing_run_id
                .as_deref()
                .is_some_and(|other| self.is_run_active(other));
        if existing_still_active {
            return Ok(ClaimOutcome::Rejected);
        }
        let allow_stale_rebind = active_looking && !existing_still_active;
        let run_context = run.context.as_ref().map(Value::to_string);
        let session_mode = normalized_message_session_mode(
            run.session_mode.as_deref(),
        );
        let claimed = store.execute(
            "UPDATE messages \
                SET run_id = ?1, run_status = ?2, session_mode = ?3, run_context_json = ?4, \
                    events_json = CASE WHEN run_id = ?5 THEN events_json ELSE NULL END, \
                    content = CASE WHEN run_id = ?6 OR run_id IS NULL THEN content ELSE '' END, \
                    ended_at = NULL, \
                    last_run_event_id = CASE WHEN run_id = ?7 THEN last_run_event_id ELSE NULL END, \
                    started_at = CASE \
                        WHEN run_id = ?8 THEN started_at \
                        WHEN ?9 THEN COALESCE(started_at, ?10) \
                        ELSE ?11 \
                    END \
              WHERE id = ?12 AND conversation_id = ?13 AND role = 'assistant' \
                AND (run_id IS NULL OR run_id = ?14 \
                     OR run_status IN ('succeeded','failed','canceled') OR ?15)",
            rusqlite::params![
                run.id,
                claim_status,
                session_mode,
                run_context,
                run.id,
                run.id,
                run.id,
                run.id,
                if existing_run_id.is_some() { 0 } else { 1 },
                run.created_at,
                run.created_at,
                assistant_message_id,
                conversation_id,
                run.id,
                if allow_stale_rebind { 1 } else { 0 }
            ],
        )?;
        if claimed == 0 {
            return Ok(ClaimOutcome::Rejected);
        }
        if !is_same_run {
            store.execute(
                "DELETE FROM message_event_batches WHERE message_id = ?1",
                [assistant_message_id],
            )?;
        }
        if let Some(seed) = seed {
            upsert_message(store, conversation_id, seed)?;
        }
        Ok(ClaimOutcome::Claimed)
    }

    /// Parity: `steer` — the verdict lives in `classifyRunSteering`.
    fn steer(&self, run_id: &str, text: &str, runtime_accepts: bool) -> Result<(), &'static str> {
        let run = self.get(run_id).ok_or("run_missing")?;
        if !runtime_accepts {
            return Err("runtime_unsupported");
        }
        if run.is_terminal() {
            return Err("run_terminal");
        }
        if !run.stdin_open {
            return Err("stdin_closed");
        }
        let now = now_ms();
        {
            let mut registry = lock(&self.inner);
            if let Some(run) = registry.runs.get_mut(run_id) {
                run.updated_at = now;
                let record = self.push_event(
                    run,
                    "steering_message",
                    json!({ "runId": run_id, "length": text.chars().count(), "at": now }),
                );
                for feed in run.subscribers.values() {
                    let frame = sse_frame(Some(record.id), &record.event, &record.data);
                    lock(feed).push(frame.as_bytes());
                }
            }
        }
        Ok(())
    }

    /// Parity: `startChatRun`'s observable outcome without an agent runtime:
    /// the run goes `running` and publishes its `start` frame.
    fn start(&self, run_id: &str, cwd: Option<String>) {
        {
            let mut registry = lock(&self.inner);
            let Some(run) = registry.runs.get_mut(run_id) else {
                return;
            };
            if run.is_terminal() || run.cancel_requested {
                return;
            }
            run.status = "running".to_string();
            run.stdin_open = true;
            run.updated_at = now_ms();
            let data = json!({
                "runId": run_id,
                "agentId": run.agent_id,
                "bin": Value::Null,
                "streamFormat": Value::Null,
                "projectId": run.project_id,
                "cwd": cwd,
                "model": Value::Null,
                "reasoning": Value::Null,
                "serviceTier": Value::Null,
                "toolTokenExpiresAt": Value::Null,
            });
            let record = self.push_event(run, "start", data);
            let frames: Vec<(Arc<Mutex<FeedState>>, String)> = run
                .subscribers
                .values()
                .map(|feed| {
                    let frame = sse_frame(Some(record.id), &record.event, &record.data);
                    (Arc::clone(feed), frame)
                })
                .collect();
            for (feed, frame) in frames {
                lock(&feed).push(frame.as_bytes());
            }
        }
    }

    /// Replay events after `cursor`, then keep streaming live frames until the
    /// run reaches a terminal state (parity: `stream(run, req, res)`).
    fn stream(&self, run_id: &str, cursor: u64, map_events: bool) -> Option<Response> {
        let feed = Arc::new(Mutex::new(FeedState::default()));
        let mut registry = lock(&self.inner);
        if !registry.runs.contains_key(run_id) {
            return None;
        }
        let (is_terminal, frames, sent) = {
            let run = registry.runs.get(run_id)?;
            let mut frames = Vec::new();
            let mut sent = 0_usize;
            for record in run.events.iter().filter(|record| record.id > cursor) {
                let frame = if map_events {
                    agui_frame(&run.id, record)
                } else {
                    Some(sse_frame(Some(record.id), &record.event, &record.data))
                };
                if let Some(frame) = frame {
                    frames.push(frame);
                    sent += 1;
                }
            }
            (run.is_terminal(), frames, sent)
        };
        if !is_terminal {
            let subscriber_id = registry.next_subscriber;
            registry.next_subscriber += 1;
            if let Some(run) = registry.runs.get_mut(run_id) {
                run.subscribers.insert(subscriber_id, Arc::clone(&feed));
            }
            for frame in frames {
                lock(&feed).push(frame.as_bytes());
            }
            let subscription = (self.clone(), run_id.to_string(), subscriber_id);
            drop(registry);
            return Some(sse_response_with_subscription(feed, subscription));
        }
        let mut frames = frames;
        // Terminal: reattached clients still need their terminal signal.
        if sent == 0 {
            let tail = registry
                .runs
                .get(run_id)
                .and_then(|run| run.events.last().cloned());
            if let Some(last) = tail {
                let frame = if map_events {
                    agui_frame(run_id, &last)
                } else {
                    Some(sse_frame(Some(last.id), &last.event, &last.data))
                };
                if let Some(frame) = frame {
                    frames.push(frame);
                }
            }
        }
        for frame in frames {
            lock(&feed).push(frame.as_bytes());
        }
        lock(&feed).close();
        drop(registry);
        Some(sse_response_with_feed(feed))
    }
}

/// Parity: `meta.analyticsHints` → `run.externalPluginAnalytics` for the
/// first-party plugin only.
fn analytics_from_meta(meta: &Map<String, Value>) -> Option<Value> {
    let hints = as_record(meta.get("analyticsHints"))?;
    if hints.get("externalPluginId").and_then(Value::as_str) != Some(OPEN_DESIGN_PLUGIN_ID) {
        return None;
    }
    let mut out = Map::new();
    for key in [
        "entrySurface",
        "hostProduct",
        "externalPluginId",
        "externalPluginVersion",
        "distributionMechanism",
        "publisherClass",
        "attributionQuality",
        "pluginWorkflowId",
        "logicalRequestDigest",
        "logicalRequestDigestVersion",
        "briefState",
        "generationSloWindowMs",
    ] {
        if let Some(value) = hints.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    out.insert(
        "externalPluginId".to_string(),
        json!(OPEN_DESIGN_PLUGIN_ID),
    );
    Some(Value::Object(out))
}

/// Parity: `toOdNativeEvent` + `encodeOdEventForAgui` reduced to the native
/// event kinds the AGUI route admits (the adapter itself is not ported).
fn agui_frame(run_id: &str, record: &RunEvent) -> Option<String> {
    const AGUI_NATIVE_EVENT_KINDS: [&str; 11] = [
        "message_chunk",
        "tool_call",
        "state_update",
        "end",
        "run_started",
        "pipeline_stage_started",
        "pipeline_stage_completed",
        "genui_surface_request",
        "genui_surface_response",
        "genui_surface_timeout",
        "genui_state_synced",
    ];
    if !AGUI_NATIVE_EVENT_KINDS.contains(&record.event.as_str()) {
        return None;
    }
    let mut data = match &record.data {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    data.insert("runId".to_string(), json!(run_id));
    data.insert("seq".to_string(), json!(record.id));
    Some(sse_frame(
        Some(record.id),
        &record.event,
        &Value::Object(data),
    ))
}

fn sse_response_with_feed(feed: Arc<Mutex<FeedState>>) -> Response {
    start_keepalive(Arc::clone(&feed));
    let body = Body::new(AsyncReadBody::with_capacity(
        FeedReader {
            feed,
            detach: None,
        },
        8 * 1024,
    ));
    build_sse_response(body)
}

fn sse_response_with_subscription(
    feed: Arc<Mutex<FeedState>>,
    subscription: (Runs, String, u64),
) -> Response {
    start_keepalive(Arc::clone(&feed));
    let body = Body::new(AsyncReadBody::with_capacity(
        FeedReader {
            feed,
            detach: Some(subscription),
        },
        8 * 1024,
    ));
    build_sse_response(body)
}

fn start_keepalive(feed: Arc<Mutex<FeedState>>) {
    tokio::spawn(async move {
        // Parity: `setInterval(writeKeepAlive, 25_000)` — the first heartbeat
        // lands a full interval after the stream opens, not immediately.
        let period = Duration::from_millis(SSE_KEEPALIVE_INTERVAL_MS);
        loop {
            tokio::time::sleep(period).await;
            let mut state = lock(&feed);
            if state.closed {
                break;
            }
            state.push(b": keepalive\n\n");
        }
    });
}

fn build_sse_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header(header::CONNECTION, "keep-alive")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("static response")
}

// ---- status projection (parity: `statusBody`) -----------------------------

/// Parity: JavaScript truthiness for the JSON values this crate inspects.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(true),
        _ => true,
    }
}

/// Parity: `stringField` — a trimmed, non-empty string or nothing.
fn trimmed_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// Parity: `normalizeOrchestratorWorkspace` — a valid record, else `null`.
fn normalize_orchestrator_workspace(value: Option<&Value>) -> Option<Map<String, Value>> {
    const KEYS: [&str; 5] = ["kind", "sourceLabel", "sourceRef", "baseRevision", "writeback"];
    let value = value?;
    if value.is_null() {
        return None;
    }
    let record = value.as_object()?;
    if record.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return None;
    }
    if trimmed_string(Some(record.get("kind")?))? != "scratch" {
        return None;
    }
    match record.get("writeback") {
        None | Some(Value::Null) => {}
        Some(other) => {
            if trimmed_string(Some(other))? != "external" {
                return None;
            }
        }
    }
    let mut out = Map::new();
    out.insert("kind".to_string(), json!("scratch"));
    out.insert("writeback".to_string(), json!("external"));
    for key in ["sourceLabel", "sourceRef", "baseRevision"] {
        match record.get(key) {
            None | Some(Value::Null) => {}
            Some(other) => {
                out.insert(key.to_string(), json!(trimmed_string(Some(other))?));
            }
        }
    }
    Some(out)
}

/// Parity: `projectWorkspaceProvenance`.
fn project_workspace_provenance(metadata: Option<&Value>) -> Value {
    let record = metadata.and_then(Value::as_object);
    let base_dir = trimmed_string(record.and_then(|map| map.get("baseDir"))).map(str::to_string);
    if let Some(mut provenance) =
        normalize_orchestrator_workspace(record.and_then(|map| map.get("orchestratorWorkspace")))
    {
        provenance.insert("kind".to_string(), json!("orchestrator-scratch"));
        provenance.insert("writeback".to_string(), json!("external"));
        return json!({
            "storage": { "kind": "folder-backed", "baseDir": base_dir },
            "provenance": provenance,
        });
    }
    if let Some(base_dir) = base_dir {
        return json!({
            "storage": { "kind": "folder-backed", "baseDir": base_dir },
            "provenance": { "kind": "user-local", "writeback": "in-place" },
        });
    }
    json!({ "storage": { "kind": "od-owned", "baseDir": null }, "provenance": null })
}

/// Parity: `statusBody` (`runtimes/runs.ts`) for a run this crate owns. The
/// agent-runtime diagnostics block (`executionDiagnostics`), durable journal
/// path, artifact paths and OD Next strategy fields are documented gaps.
fn status_body(run: &Run) -> Value {
    let mut out = Map::new();
    out.insert("id".to_string(), json!(run.id));
    out.insert("projectId".to_string(), json!(run.project_id));
    out.insert("conversationId".to_string(), json!(run.conversation_id));
    out.insert("assistantMessageId".to_string(), json!(run.assistant_message_id));
    out.insert("clientRequestId".to_string(), json!(run.client_request_id));
    out.insert("agentId".to_string(), json!(run.agent_id));
    out.insert("designSystemId".to_string(), Value::Null);
    out.insert("designSystemRequestedId".to_string(), Value::Null);
    out.insert("designSystemSelectionSource".to_string(), Value::Null);
    out.insert("designSystemDigest".to_string(), Value::Null);
    out.insert(
        "appliedPluginSnapshotId".to_string(),
        json!(run.applied_plugin_snapshot_id),
    );
    out.insert("pluginId".to_string(), json!(run.plugin_id));
    out.insert("strategyRolloutDecision".to_string(), Value::Null);
    out.insert("status".to_string(), json!(run.status));
    out.insert("createdAt".to_string(), json!(run.created_at));
    out.insert("updatedAt".to_string(), json!(run.updated_at));
    out.insert("terminalAt".to_string(), json!(run.terminal_at));
    out.insert("cancelRequested".to_string(), json!(run.cancel_requested));
    out.insert("cancelOrigin".to_string(), json!(run.cancel_origin));
    out.insert("terminalTrigger".to_string(), json!(run.terminal_trigger));
    out.insert("childPid".to_string(), Value::Null);
    out.insert("processGroupId".to_string(), Value::Null);
    // No child process is ever spawned here, so the exit is always "observed".
    out.insert("childExited".to_string(), json!(true));
    out.insert("childExitObservedAt".to_string(), Value::Null);
    out.insert("exitCode".to_string(), json!(run.exit_code));
    out.insert("signal".to_string(), json!(run.signal));
    out.insert("error".to_string(), json!(run.error));
    out.insert("errorCode".to_string(), json!(run.error_code));
    out.insert("failureCategory".to_string(), json!(run.failure_category));
    out.insert("failureDetail".to_string(), json!(run.failure_detail));
    out.insert("failureAction".to_string(), json!(run.failure_action));
    out.insert("retryable".to_string(), json!(run.retryable));
    out.insert("resumable".to_string(), json!(false));
    out.insert(
        "endedWithUnfinishedWork".to_string(),
        json!(run.ended_with_unfinished_work),
    );
    if let Some(count) = run.artifact_count {
        out.insert("artifactCount".to_string(), json!(count));
    }
    out.insert("eventsLogPath".to_string(), Value::Null);
    out.insert(
        "workspace".to_string(),
        project_workspace_provenance(run.project_metadata.as_ref()),
    );
    out.insert("mediaExecution".to_string(), run.media_execution.clone());
    out.insert(
        "toolBundle".to_string(),
        summarize_tool_bundle(&run.tool_bundle),
    );
    if let Some(browser_use) = &run.browser_use {
        out.insert("browserUse".to_string(), browser_use.clone());
    }
    if let Some(client_type) = &run.client_type {
        out.insert("clientType".to_string(), json!(client_type));
    }
    if let Some(analytics) = &run.external_plugin_analytics {
        out.insert("externalPluginAnalytics".to_string(), analytics.clone());
    }
    out.insert("manualResumeAttemptCount".to_string(), json!(0));
    out.insert("rechargeWaitDurationMs".to_string(), json!(0));
    if let Some(valid) = run.deliverable_valid {
        out.insert("deliverableValid".to_string(), json!(valid));
    }
    if let Some(validation) = &run.deliverable_validation {
        out.insert("deliverableValidation".to_string(), json!(validation));
    }
    if let Some(entry) = &run.deliverable_entry_file {
        out.insert("deliverableEntryFile".to_string(), json!(entry));
    }
    if let Some(kind) = &run.deliverable_artifact_kind {
        out.insert("deliverableArtifactKind".to_string(), json!(kind));
    }
    Value::Object(out)
}

// ---- deliverable validation (parity: `run-deliverable-validation.ts") ------

/// The one canonical file a run can deliver, or why there is none.
#[derive(Debug, Clone)]
struct DeliverableOutcome {
    valid: bool,
    validation: &'static str,
    entry_file: Option<String>,
    artifact_kind: Option<String>,
}

impl DeliverableOutcome {
    fn failed(validation: &'static str) -> Self {
        Self {
            valid: false,
            validation,
            entry_file: None,
            artifact_kind: None,
        }
    }

    fn into_status_fields(self, out: &mut Map<String, Value>) {
        out.insert("deliverableValid".to_string(), json!(self.valid));
        out.insert("deliverableValidation".to_string(), json!(self.validation));
        if let Some(entry) = self.entry_file {
            out.insert("deliverableEntryFile".to_string(), json!(entry));
        }
        if let Some(kind) = self.artifact_kind {
            out.insert("deliverableArtifactKind".to_string(), json!(kind));
        }
    }
}

/// Parity: `PROJECT_KIND_FILE_KINDS`.
fn accepted_deliverable_kinds(metadata: Option<&Value>) -> Option<Vec<&'static str>> {
    let kind = metadata
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)?;
    let declared: &[&'static str] = match kind {
        "prototype" | "template" => &["html"],
        "deck" => &["html", "presentation", "pdf"],
        "brand" => &["html", "document", "pdf"],
        "image" => &["image"],
        "video" => &["video"],
        "audio" => &["audio"],
        _ => return None,
    };
    if kind == "video" && is_hyperframes_project(metadata) {
        return Some(vec!["html", "presentation", "pdf", "video"]);
    }
    Some(declared.to_vec())
}

/// Parity: `isHyperFramesProject`.
fn is_hyperframes_project(metadata: Option<&Value>) -> bool {
    let record = match metadata.and_then(Value::as_object) {
        Some(record) => record,
        None => return false,
    };
    record.get("videoModel").and_then(Value::as_str) == Some("hyperframes-html")
        || record.get("intent").and_then(Value::as_str) == Some("hyperframes")
}

/// Parity: `safeRelativeFile` — a portable, rooted-relative path or `None`.
fn safe_relative_file(value: Option<&Value>) -> Option<String> {
    let raw = value?.as_str()?;
    let normalized = raw.trim().replace('\\', "/");
    if normalized.is_empty() || normalized.starts_with('/') {
        return None;
    }
    if normalized
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return None;
    }
    Some(normalized)
}

fn file_path_of(file: &Value) -> String {
    file.get("path")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .or_else(|| file.get("name").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

/// Parity: `inferredEntry`.
fn inferred_entry(files: &[Value], accepted: Option<&[&'static str]>) -> Option<Value> {
    if let Some(root_index) = files
        .iter()
        .find(|file| file_path_of(file) == "index.html")
    {
        return Some(root_index.clone());
    }
    let root_html: Vec<&Value> = files
        .iter()
        .filter(|file| !file_path_of(file).contains('/') && file_kind(file) == "html")
        .collect();
    if root_html.len() == 1 {
        return Some(root_html[0].clone());
    }
    if let Some(accepted) = accepted {
        let compatible: Vec<&Value> = files
            .iter()
            .filter(|file| accepted.contains(&file_kind(file)))
            .collect();
        if compatible.len() == 1 {
            return Some(compatible[0].clone());
        }
    }
    if files.len() == 1 {
        return Some(files[0].clone());
    }
    None
}

fn file_kind(file: &Value) -> &str {
    file.get("kind").and_then(Value::as_str).unwrap_or("")
}

/// Parity: `resolveDeliverable` (run scope). The linked-page fallback behind
/// `entry_not_touched` is unreachable in this crate — a run never records
/// `artifactOutcome.touchedPaths` — so it is reduced to that one verdict.
fn resolve_deliverable(
    project_root: &FsPath,
    files: &[Value],
    metadata: Option<&Value>,
    run_status: &str,
    artifact_count: Option<i64>,
    touched_paths: Option<&[String]>,
) -> DeliverableOutcome {
    if run_status != "succeeded" {
        return DeliverableOutcome::failed("not_succeeded");
    }
    if artifact_count.unwrap_or(0) <= 0 {
        return DeliverableOutcome::failed("no_artifact");
    }
    let accepted = accepted_deliverable_kinds(metadata);
    let declared = safe_relative_file(metadata.and_then(|value| value.get("entryFile")));
    let selected = declared
        .and_then(|declared| {
            files
                .iter()
                .find(|file| file_path_of(file) == declared)
                .cloned()
        })
        .or_else(|| inferred_entry(files, accepted.as_deref()));
    let Some(selected) = selected else {
        return DeliverableOutcome::failed("entry_missing");
    };
    let entry_file = file_path_of(&selected);
    let artifact_kind = file_kind(&selected).to_string();
    let facts = || DeliverableOutcome {
        valid: false,
        validation: "",
        entry_file: Some(entry_file.clone()),
        artifact_kind: Some(artifact_kind.clone()),
    };
    if let Some(touched) = touched_paths {
        let touched: Vec<String> = touched
            .iter()
            .filter_map(|candidate| {
                let absolute = if FsPath::new(candidate).is_absolute() {
                    PathBuf::from(candidate)
                } else {
                    project_root.join(candidate)
                };
                let relative = absolute.strip_prefix(project_root).ok()?;
                Some(relative.to_string_lossy().replace('\\', "/"))
            })
            .collect();
        if !touched.contains(&entry_file) {
            let mut outcome = facts();
            outcome.validation = "entry_not_touched";
            return outcome;
        }
    }
    if let Some(accepted) = accepted.as_deref() {
        if !accepted.contains(&artifact_kind.as_str()) {
            let mut outcome = facts();
            outcome.validation = "type_mismatch";
            return outcome;
        }
    }
    let target = project_root.join(&entry_file);
    let readable = target
        .strip_prefix(project_root)
        .map(|relative| {
            relative
                .components()
                .all(|component| component != std::path::Component::ParentDir)
        })
        .unwrap_or(false)
        && std::fs::metadata(&target).map(|stat| stat.is_file()).unwrap_or(false)
        && std::fs::File::open(&target).is_ok();
    if !readable {
        let mut outcome = facts();
        outcome.validation = "entry_unreadable";
        return outcome;
    }
    DeliverableOutcome {
        valid: true,
        validation: "valid",
        entry_file: Some(entry_file),
        artifact_kind: Some(artifact_kind),
    }
}

/// Parity: `validateChatRunDeliverable` — status gates first, then the
/// filesystem scan.
fn deliverable_for_run(
    state: &AppState,
    run: &Run,
    artifact_count: Option<i64>,
) -> DeliverableOutcome {
    if run.status != "succeeded" {
        return DeliverableOutcome::failed("not_succeeded");
    }
    if artifact_count.unwrap_or(0) <= 0 {
        return DeliverableOutcome::failed("no_artifact");
    }
    let Some(project_id) = run.project_id.as_deref().filter(|id| !id.is_empty()) else {
        return DeliverableOutcome::failed("project_missing");
    };
    let project = match state.store.get_project(project_id) {
        Ok(project) => project,
        Err(err) => return failed_store_deliverable(&err),
    };
    let Some(project) = project else {
        return DeliverableOutcome::failed("project_missing");
    };
    let metadata: Option<Value> = project
        .metadata_json
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .or_else(|| run.project_metadata.clone());
    let base = match project_dir::project_fs_base(&state.config, &project) {
        Ok(base) => base,
        Err(_) => return DeliverableOutcome::failed("project_missing"),
    };
    let files = match project_files::list_files(&base, None) {
        Ok(files) => files,
        Err(_) => return DeliverableOutcome::failed("project_missing"),
    };
    resolve_deliverable(&base, &files, metadata.as_ref(), &run.status, artifact_count, None)
}

fn failed_store_deliverable(_err: &StoreError) -> DeliverableOutcome {
    DeliverableOutcome::failed("project_missing")
}

// ---- SQLite helpers (parity: `db.ts`) -------------------------------------

/// The conversation columns the run routes read.
struct ConversationRow {
    id: String,
    project_id: Option<String>,
    session_mode: Option<String>,
}

fn get_conversation(store: &Store, id: &str) -> Result<Option<ConversationRow>, StoreError> {
    store.query_one(
        "SELECT id, project_id AS projectId, session_mode AS sessionMode \
         FROM conversations WHERE id = ?1",
        [id],
        |row| {
            Ok(ConversationRow {
                id: row.get("id")?,
                project_id: row.get("projectId")?,
                session_mode: row.get("sessionMode")?,
            })
        },
    )
}

/// Parity: `getFirstProjectConversation`.
fn first_project_conversation(
    store: &Store,
    project_id: &str,
) -> Result<Option<ConversationRow>, StoreError> {
    let id = store.query_one(
        "SELECT id FROM conversations WHERE project_id = ?1 \
         ORDER BY created_at ASC, rowid ASC LIMIT 1",
        [project_id],
        |row| row.get::<_, String>(0),
    )?;
    match id {
        Some(id) => get_conversation(store, &id),
        None => Ok(None),
    }
}

/// Parity: `updateProject(db, id, {})` — the activity bump the message
/// writers perform so `listProjects` reorders.
fn bump_project_updated_at(store: &Store, project_id: &str) -> Result<(), StoreError> {
    let Some(existing) = store.get_project(project_id)? else {
        return Ok(());
    };
    let metadata_json = match existing.metadata_json.as_deref() {
        Some(text) if !text.is_empty() => match serde_json::from_str::<Value>(text) {
            Ok(parsed) if js_truthy(&parsed) => Some(parsed.to_string()),
            _ => None,
        },
        _ => None,
    };
    store.execute(
        "UPDATE projects
            SET name = ?1, skill_id = ?2, design_system_id = ?3, pending_prompt = ?4,
                metadata_json = ?5, custom_instructions = ?6, updated_at = ?7
          WHERE id = ?8",
        rusqlite::params![
            existing.name,
            existing.skill_id,
            existing.design_system_id,
            existing.pending_prompt,
            metadata_json,
            existing.custom_instructions,
            now_ms(),
            project_id
        ],
    )?;
    Ok(())
}

fn json_text_if_truthy(map: &Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(value) if js_truthy(value) => Some(value.to_string()),
        _ => None,
    }
}

fn is_terminal_message_run_status(status: Option<&str>) -> bool {
    matches!(status, Some("succeeded") | Some("failed") | Some("canceled"))
}

/// Parity: `upsertMessage` for the columns a run-create seed, a steer message
/// and an assistant claim write. The daemon's append-only event batches are
/// not ported, so `events_json` is preserved exactly when TS would keep it.
fn upsert_message(store: &Store, conversation_id: &str, message: &Value) -> Result<(), StoreError> {
    let map = as_record(Some(message)).expect("seed message is an object");
    let id = string_field(Some(map), "id").unwrap_or_default();
    let now = now_ms();
    let existing = store.query_one(
        "SELECT run_id AS runId, run_status AS runStatus FROM messages WHERE id = ?1",
        [&id],
        |row| {
            Ok::<_, rusqlite::Error>((
                row.get::<_, Option<String>>("runId")?,
                row.get::<_, Option<String>>("runStatus")?,
            ))
        },
    )?;
    match existing {
        Some((existing_run_id, existing_run_status)) => {
            let incoming_terminal = is_terminal_message_run_status(
                map.get("runStatus").and_then(Value::as_str),
            );
            let preserve = existing_run_id.is_some()
                && matches!(
                    existing_run_status.as_deref(),
                    Some("queued") | Some("running")
                )
                && !incoming_terminal;
            let next_content = if preserve {
                store
                    .query_one(
                        "SELECT content FROM messages WHERE id = ?1",
                        [&id],
                        |row| row.get::<_, Option<String>>(0),
                    )?
                    .map(|content| Some(content.unwrap_or_default()))
                    .unwrap_or(None)
            } else {
                string_field(Some(map), "content")
            };
            let next_events_json = if preserve {
                store.query_one(
                    "SELECT events_json FROM messages WHERE id = ?1",
                    [&id],
                    |row| row.get::<_, Option<String>>(0),
                )?
                .unwrap_or(None)
            } else {
                json_text_if_truthy(map, "events")
            };
            let attachments = json_text_if_truthy(map, "attachments");
            let comment_attachments = json_text_if_truthy(map, "commentAttachments");
            let run_context = json_text_if_truthy(map, "runContext");
            let session_mode =
                normalized_message_session_mode(map.get("sessionMode").and_then(Value::as_str));
            let started_at = map.get("startedAt").and_then(Value::as_i64);
            let ended_at = map.get("endedAt").and_then(Value::as_i64);
            store.execute(
                "UPDATE messages
                    SET role = ?1, content = ?2, run_id = ?3, run_status = ?4,
                        events_json = ?5, attachments_json = ?6,
                        comment_attachments_json = ?7, session_mode = ?8,
                        run_context_json = ?9, started_at = ?10, ended_at = ?11
                  WHERE id = ?12",
                rusqlite::params![
                    string_field(Some(map), "role").unwrap_or_default(),
                    next_content,
                    map.get("runId").and_then(Value::as_str),
                    map.get("runStatus").and_then(Value::as_str),
                    next_events_json,
                    attachments,
                    comment_attachments,
                    session_mode,
                    run_context,
                    started_at,
                    ended_at,
                    id
                ],
            )?;
        }
        None => {
            let position = store
                .query_one(
                    "SELECT COALESCE(MAX(position), -1) FROM messages WHERE conversation_id = ?1",
                    [conversation_id],
                    |row| row.get::<_, i64>(0),
                )?
                .unwrap_or(-1)
                + 1;
            let created_at = map
                .get("createdAt")
                .and_then(Value::as_i64)
                .unwrap_or(now);
            store.execute(
                "INSERT INTO messages
                   (id, conversation_id, role, content, run_id, run_status,
                    events_json, attachments_json, comment_attachments_json,
                    session_mode, run_context_json, started_at, ended_at,
                    position, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                rusqlite::params![
                    id,
                    conversation_id,
                    string_field(Some(map), "role").unwrap_or_default(),
                    string_field(Some(map), "content").unwrap_or_default(),
                    map.get("runId").and_then(Value::as_str),
                    map.get("runStatus").and_then(Value::as_str),
                    json_text_if_truthy(map, "events"),
                    json_text_if_truthy(map, "attachments"),
                    json_text_if_truthy(map, "commentAttachments"),
                    normalized_message_session_mode(
                        map.get("sessionMode").and_then(Value::as_str),
                    ),
                    json_text_if_truthy(map, "runContext"),
                    map.get("startedAt").and_then(Value::as_i64),
                    map.get("endedAt").and_then(Value::as_i64),
                    position,
                    created_at
                ],
            )?;
        }
    }
    store.execute(
        "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![now, conversation_id],
    )?;
    Ok(())
}

fn normalized_message_session_mode(mode: Option<&str>) -> Option<String> {
    let mode = mode?;
    matches!(mode, "chat" | "design" | "plan").then(|| mode.to_string())
}

/// Parity: `emittedRenderableQuestionForm`, reduced to the open + close
/// marker pair (the JSON-body grammar lives in `@open-design/contracts`, which
/// this crate does not depend on).
fn emitted_renderable_question_form(content: &str) -> bool {
    let lowered = content.to_ascii_lowercase();
    for tag in ["question-form", "ask-question"] {
        let open = format!("<{tag}");
        let Some(start) = lowered.find(&open) else {
            continue;
        };
        let close = format!("</{tag}>");
        if let Some(end) = lowered[start..].find(&close) {
            let inner = &content[start + open.len()..start + end];
            if inner.trim_end().ends_with('>') {
                // `<question-form/>`-style self closing markup has no body.
                continue;
            }
            if !inner.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

/// Parity: `listProjectsAwaitingInput` — newest form-bearing assistant message
/// per project, reported only when its own conversation has no later user row.
fn list_projects_awaiting_input(store: &Store) -> Result<Vec<String>, StoreError> {
    struct Candidate {
        partition_key: String,
        conversation_id: String,
        created_at: i64,
        position: i64,
        content: String,
    }
    let candidates = store.query(
        "SELECT c.project_id AS partitionKey, m.conversation_id AS conversationId, \
                m.created_at AS createdAt, m.position AS position, m.content AS content \
           FROM messages m \
           JOIN conversations c ON c.id = m.conversation_id \
          WHERE m.role = 'assistant' \
            AND (LOWER(m.content) LIKE '%<question-form%' \
              OR LOWER(m.content) LIKE '%<ask-question%') \
          ORDER BY m.created_at DESC, m.position DESC",
        [],
        |row| {
            Ok(Candidate {
                partition_key: row.get("partitionKey")?,
                conversation_id: row.get("conversationId")?,
                created_at: row.get("createdAt")?,
                position: row.get("position")?,
                content: row.get("content")?,
            })
        },
    )?;
    let mut winners: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        if winners
            .iter()
            .any(|winner| winner.partition_key == candidate.partition_key)
        {
            continue;
        }
        if !emitted_renderable_question_form(&candidate.content) {
            continue;
        }
        winners.push(candidate);
    }
    let mut awaiting = Vec::new();
    for winner in &winners {
        let answered = store.query_one(
            "SELECT 1 FROM messages reply \
              WHERE reply.conversation_id = ?1 \
                AND (reply.created_at > ?2 OR (reply.created_at = ?2 AND reply.position > ?3)) \
              LIMIT 1",
            rusqlite::params![
                winner.conversation_id,
                winner.created_at,
                winner.position
            ],
            |row| row.get::<_, i64>(0),
        )?;
        if answered.is_none() {
            awaiting.push(winner.partition_key.clone());
        }
    }
    Ok(awaiting)
}

// ---- query/header helpers --------------------------------------------------

/// Parity: Express' query parser for a single scalar key (no array collapse).
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        if raw_key != key {
            continue;
        }
        return Some(raw_value.to_string());
    }
    None
}

/// Parity: `Number(Last-Event-ID || after || 0)` clamped to a finite cursor.
fn parse_cursor(headers: &HeaderMap, query: Option<&str>) -> u64 {
    let raw = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| query_param(query, "after"));
    let requested = raw
        .as_deref()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0);
    if requested.is_finite() {
        requested.max(0.0) as u64
    } else {
        0
    }
}

/// Parity: `x-od-client` / `user-agent` client classification.
fn client_type_from_headers(headers: &HeaderMap) -> String {
    let declared = headers
        .get("x-od-client")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if declared == "desktop" || declared == "web" {
        return declared;
    }
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if user_agent.contains("Electron/") {
        "desktop".to_string()
    } else {
        "web".to_string()
    }
}

/// Parity: `workspaceResourceContextFromRequest` — headerless callers carry
/// neither workspace header.
fn is_headerless_workspace_request(headers: &HeaderMap) -> bool {
    headers.get("x-od-workspace-id").is_none()
        && headers.get("x-od-workspace-member-id").is_none()
}

// ---- run creation (parity: `handleRunCreate` / `POST /api/chat`) ----------

/// Parity: `withoutSensitiveRunInput`.
fn without_sensitive_run_input(body: &Map<String, Value>) -> Map<String, Value> {
    let mut sanitized = body.clone();
    for key in [
        "byokProvider",
        "byokProfileId",
        "apiKey",
        "rechargeResumeCapability",
        "workspaceScope",
        "odNextTaskInputSnapshot",
    ] {
        sanitized.remove(key);
    }
    sanitized
}

/// Parity: `runRequestFingerprint` — canonical JSON of the execution-shaping
/// request, transport/recovery metadata excluded.
fn run_request_fingerprint(meta: &Map<String, Value>) -> String {
    let mut logical = meta.clone();
    for key in [
        "clientRequestId",
        "requestFingerprint",
        "resume",
        "analyticsHints",
        "userMessageId",
        "assistantMessageId",
        "projectMetadata",
        "appliedPluginSnapshotId",
    ] {
        logical.remove(key);
    }
    logical.insert("appliedPluginSnapshot".to_string(), Value::Null);
    let canonical = canonical_json(&Value::Object(logical));
    sha256_hex(canonical.to_string().as_bytes())
}

/// Parity: `hasCompleteByokOpenCodeConfig`. The provider config builder's
/// protocol/base-URL normalization is not ported, so this crate requires the
/// fields that builder itself refuses to do without (documented gap).
fn has_complete_byok_opencode_config(meta: &Map<String, Value>) -> bool {
    if string_field(Some(meta), "agentId").as_deref() != Some(BYOK_OPENCODE_AGENT_ID) {
        return true;
    }
    let Some(provider) = meta.get("byokProvider").and_then(Value::as_object) else {
        return false;
    };
    let api_key_ok = provider
        .get("apiKey")
        .and_then(Value::as_str)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let model = meta.get("model").and_then(Value::as_str).map(str::trim).unwrap_or("");
    api_key_ok && !model.is_empty() && !model.eq_ignore_ascii_case("default")
}

/// Parity: `resolvePluginGenerationSloWindowMs` for an installation with no
/// agent runtime definitions (inactivity timeout = the 10-minute default).
fn generation_slo_window_ms() -> i64 {
    const MIN: i64 = 5 * 60 * 1000;
    const DEFAULT: i64 = 45 * 60 * 1000;
    const MAX: i64 = 24 * 60 * 60 * 1000;
    const TERMINAL_BUFFER: i64 = 60 * 1000;
    const DEFAULT_INACTIVITY: i64 = 10 * 60 * 1000;

    let requested = std::env::var("OD_PLUGIN_GENERATION_SLO_WINDOW_MS")
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .map(|value| value.floor() as i64)
        .unwrap_or(DEFAULT)
        .clamp(MIN, MAX);
    let runtime_floor = (DEFAULT_INACTIVITY + TERMINAL_BUFFER).clamp(MIN, MAX);
    requested.max(runtime_floor)
}

/// A conversation may only carry a run for the project that owns it.
fn check_conversation_ownership(
    store: &Store,
    conversation_id: Option<&str>,
    project_id: Option<&str>,
) -> Result<(), Box<Response>> {
    let Some(conversation_id) = conversation_id.filter(|id| !id.is_empty()) else {
        return Ok(());
    };
    let rejected = || {
        api_err(
            StatusCode::NOT_FOUND,
            "CONVERSATION_NOT_FOUND",
            "conversation not found for project",
        )
    };
    let Some(project_id) = project_id.filter(|id| !id.is_empty()) else {
        return Err(rejected());
    };
    let conversation = get_conversation(store, conversation_id).map_err(|err| store_err(&err))?;
    match conversation {
        Some(conversation) if conversation.project_id.as_deref() == Some(project_id) => Ok(()),
        _ => Err(rejected()),
    }
}

/// The `(role, conversation_id)` pair the assistant/user pin-ownership checks
/// read from `messages`.
type MessageOwnership = (Option<String>, Option<String>);

fn message_role_and_conversation(
    store: &Store,
    id: &str,
) -> Result<Option<MessageOwnership>, StoreError> {
    store.query_one(
        "SELECT role, conversation_id AS conversationId FROM messages WHERE id = ?1",
        [id],
        |row| {
            Ok::<_, rusqlite::Error>((
                row.get::<_, Option<String>>("role")?,
                row.get::<_, Option<String>>("conversationId")?,
            ))
        },
    )
}

/// The 202 payload `POST /api/runs` and `POST /api/chat` build off a prepared
/// run (chat never sends it — it answers with the SSE stream).
struct PreparedRunCreate {
    run_id: String,
    created: bool,
    body: Value,
}

async fn prepare_run_create(
    state: &AppState,
    headers: &HeaderMap,
    request: Request,
) -> Result<PreparedRunCreate, Box<Response>> {
    if state.shutting_down.load(Ordering::SeqCst) {
        return Err(api_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "UPSTREAM_UNAVAILABLE",
            "daemon is shutting down",
        ));
    }
    let body = read_json_body(request).await?;
    let record = as_record(Some(&body)).cloned().unwrap_or_default();

    let media_policy = match parse_media_execution_policy_input(record.get("mediaExecution")) {
        MediaParse::Ok(policy) => policy,
        MediaParse::Err(message) => {
            return Err(api_err(StatusCode::BAD_REQUEST, "BAD_REQUEST", &message));
        }
    };
    let tool_bundle = match parse_run_tool_bundle(record.get("toolBundle")) {
        ToolBundleParse::Ok(bundle) => bundle,
        ToolBundleParse::Err(message) => {
            return Err(api_err(StatusCode::BAD_REQUEST, "BAD_REQUEST", &message));
        }
    };
    if !has_complete_byok_opencode_config(&record) {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "VALIDATION_FAILED",
            BYOK_OPENCODE_PROVIDER_REQUIRED_MESSAGE,
        ));
    }

    let request_project_id = nonempty_string(Some(&record), "projectId");
    let request_conversation_id = nonempty_string(Some(&record), "conversationId");
    check_conversation_ownership(
        &state.store,
        request_conversation_id.as_deref(),
        request_project_id.as_deref(),
    )?;

    // Parity: a missing project row is not an error here — the run simply
    // carries no project metadata (and no sandbox verdict to enforce).
    let mut project: Option<ProjectRow> = None;
    if let Some(project_id) = request_project_id.as_deref() {
        project = state
            .store
            .get_project(project_id)
            .map_err(|err| store_error_response(&err))?;
        if let Some(row) = &project {
            project_dir::assert_sandbox_project_root_available(row)
                .map_err(project_dir_error_response)?;
        }
    }

    let mut meta = without_sensitive_run_input(&record);
    meta.insert("mediaExecution".to_string(), media_policy);
    meta.insert("toolBundle".to_string(), tool_bundle);
    if let Some(agent_id) = nonempty_string(Some(&record), "agentId") {
        meta.insert("agentId".to_string(), json!(agent_id));
    }
    // Always replace any untrusted request field, including with null.
    meta.insert("workspaceScope".to_string(), Value::Null);
    if let Some(row) = &project {
        if let Some(text) = row.metadata_json.as_deref().filter(|text| !text.is_empty()) {
            if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                meta.insert("projectMetadata".to_string(), parsed);
            }
        }
    }

    // External plugin analytics hints: normalize, then bind the workflow id.
    const EXTERNAL_KEYS: [&str; 5] = [
        "externalPluginId",
        "externalPluginVersion",
        "pluginWorkflowId",
        "logicalRequestDigest",
        "logicalRequestDigestVersion",
    ];
    if let Some(hints) = as_record(meta.get("analyticsHints")) {
        let has_external = EXTERNAL_KEYS.iter().any(|key| hints.contains_key(*key));
        if has_external {
            let normalized = normalize_external_plugin_hints(
                hints,
                nonempty_string(Some(&meta), "clientRequestId").as_deref(),
            )
            .map_err(|message| {
                api_error(StatusCode::BAD_REQUEST, "PLUGIN_CONTRACT_REJECTED", &message)
            })?;
            let mut merged = hints.clone();
            if let Value::Object(normalized) = normalized {
                for (key, value) in normalized {
                    merged.insert(key, value);
                }
            }
            merged.insert(
                "generationSloWindowMs".to_string(),
                json!(generation_slo_window_ms()),
            );
            meta.insert("analyticsHints".to_string(), Value::Object(merged));
            if let Some(workflow_id) = meta
                .get("analyticsHints")
                .and_then(|value| value.get("pluginWorkflowId"))
                .and_then(Value::as_str)
            {
                if let Some(existing) = state.runs.find_by_workflow(workflow_id) {
                    let same_request = existing.client_request_id
                        == nonempty_string(Some(&meta), "clientRequestId");
                    if !same_request {
                        return Err(api_err(
                            StatusCode::CONFLICT,
                            "PLUGIN_WORKFLOW_CONFLICT",
                            "pluginWorkflowId is already bound to a different logical run request",
                        ));
                    }
                }
            }
        }
    }

    // Headless clients omit conversationId; bind the project's first one.
    let mut conversation_fallback_bound = false;
    if request_project_id.is_some() && request_conversation_id.is_none() {
        if let Some(project_id) = request_project_id.as_deref() {
            let fallback = first_project_conversation(&state.store, project_id)
                .map_err(|err| store_error_response(&err))?;
            if let Some(conversation) = fallback {
                meta.insert("conversationId".to_string(), json!(conversation.id));
                conversation_fallback_bound = true;
            }
        }
    }
    let conversation_id = nonempty_string(Some(&meta), "conversationId");
    let project_id = nonempty_string(Some(&meta), "projectId");
    check_conversation_ownership(
        &state.store,
        conversation_id.as_deref(),
        project_id.as_deref(),
    )?;

    let conversation_session = match conversation_id.as_deref() {
        Some(id) => get_conversation(&state.store, id).map_err(|err| store_error_response(&err))?,
        None => None,
    };
    let requested_mode = meta.get("sessionMode").and_then(Value::as_str);
    let session_mode = match requested_mode {
        Some(mode @ ("chat" | "design" | "plan")) => normalize_session_mode(Some(mode)),
        _ => normalize_session_mode(
            conversation_session
                .as_ref()
                .and_then(|conversation| conversation.session_mode.as_deref()),
        ),
    };
    let session_mode = session_mode.unwrap_or_else(|| "design".to_string());
    meta.insert("sessionMode".to_string(), json!(session_mode));

    // Pin ownership: a client-supplied message id must reference a row this
    // run is allowed to mutate (#6418).
    let missing_client_pin = nonempty_string(Some(&meta), "assistantMessageId").is_none();
    let client_user_message_id = nonempty_string(Some(&meta), "userMessageId");
    if let Some(id) = client_user_message_id.as_deref() {
        if !is_safe_id(id) {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "userMessageId is invalid",
            ));
        }
        if conversation_id.is_some() {
            if let Some((role, owner)) =
                message_role_and_conversation(&state.store, id).map_err(|err| store_error_response(&err))?
            {
                if role.as_deref() != Some("user") {
                    return Err(api_err(
                        StatusCode::CONFLICT,
                        "INVALID_USER_MESSAGE",
                        "userMessageId must reference a user message",
                    ));
                }
                if owner.as_deref() != conversation_id.as_deref() {
                    return Err(api_err(
                        StatusCode::CONFLICT,
                        "IDEMPOTENCY_CONFLICT",
                        "userMessageId belongs to a different conversation",
                    ));
                }
            }
        }
    }
    let client_assistant_message_id = nonempty_string(Some(&meta), "assistantMessageId");
    if let Some(id) = client_assistant_message_id.as_deref() {
        if !is_safe_id(id) {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "assistantMessageId is invalid",
            ));
        }
    }
    if let (Some(user_id), Some(assistant_id)) =
        (client_user_message_id.as_deref(), client_assistant_message_id.as_deref())
    {
        if user_id == assistant_id {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "userMessageId and assistantMessageId must be distinct",
            ));
        }
    }
    if let Some(assistant_id) = client_assistant_message_id.as_deref() {
        if conversation_id.is_none() {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "assistantMessageId requires a conversation",
            ));
        }
        if let Some((role, owner)) =
            message_role_and_conversation(&state.store, assistant_id)
                .map_err(|err| store_error_response(&err))?
        {
            if role.as_deref() != Some("assistant") {
                return Err(api_err(
                    StatusCode::CONFLICT,
                    "INVALID_ASSISTANT_MESSAGE",
                    "assistantMessageId must reference an assistant message",
                ));
            }
            if owner.as_deref() != conversation_id.as_deref() {
                return Err(api_err(
                    StatusCode::CONFLICT,
                    "IDEMPOTENCY_CONFLICT",
                    "assistantMessageId belongs to a different conversation",
                ));
            }
        }
    }

    // Prepare the seed payload; it is only persisted when this run is new.
    let mut seed: Option<Value> = None;
    if conversation_id.is_some()
        && (client_user_message_id.is_some() || missing_client_pin || conversation_fallback_bound)
    {
        if missing_client_pin {
            meta.insert("assistantMessageId".to_string(), json!(random_id()));
        }
        let prompt = string_field(Some(&record), "currentPrompt").or_else(|| {
            string_field(Some(&record), "message")
                .filter(|message| !message.trim().is_empty())
        });
        if let Some(content) = prompt {
            let id = client_user_message_id.clone().unwrap_or_else(random_id);
            let now = now_ms();
            seed = Some(json!({
                "id": id,
                "role": "user",
                "content": content,
                "startedAt": now,
                "endedAt": now,
            }));
        }
    }

    meta.insert(
        "requestFingerprint".to_string(),
        json!(run_request_fingerprint(&meta)),
    );

    let creation = state.runs.create_or_reuse(&meta);
    let (run_id, created) = match creation {
        CreateOutcome::Conflict(existing_id) => {
            tracing::debug!(%existing_id, "idempotency conflict");
            return Err(api_err(
                StatusCode::CONFLICT,
                "IDEMPOTENCY_CONFLICT",
                "clientRequestId is already associated with a different logical run request",
            ));
        }
        CreateOutcome::Reused(run_id) => (run_id, false),
        CreateOutcome::Created(run_id) => {
            // Parity: `activeRunBlockingDesignSystemEnrichment`.
            let is_enrichment = as_record(meta.get("analyticsHints"))
                .and_then(|hints| hints.get("dsEnrichment"))
                .is_some_and(|value| value == &json!(true));
            if is_enrichment {
                if let Some(conversation_id) = conversation_id.as_deref() {
                    let blocking = state
                        .runs
                        .list(None, Some(conversation_id), Some("active"))
                        .into_iter()
                        .find(|candidate| candidate.id != run_id);
                    if let Some(blocking) = blocking {
                        state.runs.drop_run(&run_id);
                        return Err(Box::new(api_err_init(
                            StatusCode::CONFLICT,
                            "DESIGN_SYSTEM_ENRICHMENT_IN_PROGRESS",
                            "a design-system enrichment run is already active for this conversation",
                            json!({
                                "details": {
                                    "kind": "design_system_enrichment_in_progress",
                                    "runId": blocking.id,
                                    "conversationId": blocking.conversation_id.unwrap_or_default(),
                                }
                            }),
                        )));
                    }
                }
            }
            let run = state
                .runs
                .get(&run_id)
                .ok_or_else(|| api_err(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found"))?;
            let claim = state
                .runs
                .claim_assistant_message(&state.store, &run, seed.as_ref())
                .map_err(|err| store_error_response(&err))?;
            if claim == ClaimOutcome::Rejected {
                state.runs.drop_run(&run_id);
                return Err(api_err(
                    StatusCode::CONFLICT,
                    "RUN_IN_PROGRESS",
                    "assistantMessageId is already bound to an active run",
                ));
            }
            (run_id, true)
        }
    };

    let run = state
        .runs
        .get(&run_id)
        .ok_or_else(|| api_err(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found"))?;
    state
        .runs
        .set_client_type(&run_id, &client_type_from_headers(headers));
    let analytics_mismatch = !created
        && external_plugin_attribution_mismatch(
            run.external_plugin_analytics.as_ref(),
            meta.get("analyticsHints"),
        );

    let mut payload = Map::new();
    payload.insert("runId".to_string(), json!(run.id));
    payload.insert("conversationId".to_string(), json!(run.conversation_id));
    payload.insert("assistantMessageId".to_string(), json!(run.assistant_message_id));
    payload.insert("clientRequestId".to_string(), json!(run.client_request_id));
    payload.insert("reused".to_string(), json!(!created));
    payload.insert("resumed".to_string(), json!(false));
    if analytics_mismatch {
        payload.insert("analyticsAttributionMismatch".to_string(), json!(true));
    }
    if let Some(snapshot_id) = &run.applied_plugin_snapshot_id {
        payload.insert("appliedPluginSnapshotId".to_string(), json!(snapshot_id));
    }
    if let Some(plugin_id) = &run.plugin_id {
        payload.insert("pluginId".to_string(), json!(plugin_id));
    }
    Ok(PreparedRunCreate {
        run_id,
        created,
        body: Value::Object(payload),
    })
}

/// Parity: the `cwd` a starter would hand the child (`resolveProjectDir`).
fn project_cwd(state: &AppState, run: &Run) -> Option<String> {
    let project_id = run.project_id.as_deref()?;
    let project = state.store.get_project(project_id).ok()??;
    project_dir::project_fs_base(&state.config, &project)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

// ---- handlers -------------------------------------------------------------

/// `POST /api/runs` — validate, claim, start, and answer 202 with the run id.
async fn create_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let prepared = match prepare_run_create(&state, &headers, request).await {
        Ok(prepared) => prepared,
        Err(response) => return *response,
    };
    if prepared.created {
        let run = match state.runs.get(&prepared.run_id) {
            Some(run) => run,
            None => return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found"),
        };
        state
            .runs
            .start(&prepared.run_id, project_cwd(&state, &run));
    }
    (StatusCode::ACCEPTED, Json(prepared.body)).into_response()
}

/// `POST /api/chat` — the same preparation, answered with the run's SSE
/// stream instead of the 202 payload (parity: the chat route never 202s).
async fn create_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    let prepared = match prepare_run_create(&state, &headers, request).await {
        Ok(prepared) => prepared,
        Err(response) => return *response,
    };
    let cursor = parse_cursor(&headers, query.as_deref());
    let response = match state.runs.stream(&prepared.run_id, cursor, false) {
        Some(response) => response,
        None => return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found"),
    };
    if prepared.created {
        let run = state.runs.get(&prepared.run_id).unwrap_or_else(|| {
            unreachable!("a created run is present until the daemon drops it")
        });
        state
            .runs
            .start(&prepared.run_id, project_cwd(&state, &run));
    }
    response
}

/// `GET /api/runs` — the run feed plus the awaiting-input project set.
async fn list_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let project_id = query_param(query.as_deref(), "projectId");
    let conversation_id = query_param(query.as_deref(), "conversationId");
    let status = query_param(query.as_deref(), "status");
    let mut visible = state.runs.list(
        project_id.as_deref(),
        conversation_id.as_deref(),
        status.as_deref(),
    );
    let project_id = project_id.filter(|value| !value.is_empty());
    match project_id.as_deref() {
        Some(project_id) => {
            let bound = match state.store.workspace_id_for_project(project_id) {
                Ok(binding) => binding.is_some(),
                Err(err) => return store_error_response(&err),
            };
            if bound && is_headerless_workspace_request(&headers) {
                // Headerless local callers may list only runs whose runtime is
                // known not to use AMR's Workspace billing plane.
                visible.retain(|run| {
                    run.agent_id
                        .as_deref()
                        .is_some_and(|agent| !agent.is_empty() && agent != "amr")
                });
            }
        }
        None => {
            for run in &visible {
                let Some(run_project) = run.project_id.as_deref() else {
                    continue;
                };
                let bound = match state.store.workspace_id_for_project(run_project) {
                    Ok(binding) => binding.is_some(),
                    Err(err) => return store_error_response(&err),
                };
                if bound {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "PROJECT_SCOPE_REQUIRED",
                        "projectId is required when listing Workspace-bound runs",
                    );
                }
            }
        }
    }
    let visible_project_ids: HashSet<String> = visible
        .iter()
        .filter_map(|run| run.project_id.clone())
        .filter(|id| !id.is_empty())
        .collect();
    let awaiting = if visible_project_ids.is_empty() {
        Vec::new()
    } else {
        match list_projects_awaiting_input(&state.store) {
            Ok(awaiting) => awaiting
                .into_iter()
                .filter(|id| visible_project_ids.contains(id))
                .collect(),
            Err(err) => return store_error_response(&err),
        }
    };
    let payload = json!({
        "runs": visible.iter().map(status_body).collect::<Vec<Value>>(),
        "awaitingInputProjectIds": awaiting,
    });
    Json(payload).into_response()
}

/// `GET /api/runs/by-plugin-workflow/:workflowId`.
async fn get_run_by_workflow(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> Response {
    let plugin_workflow_id = match validate_plugin_workflow_id(Some(&json!(workflow_id))) {
        Ok(plugin_workflow_id) => plugin_workflow_id,
        Err(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "PLUGIN_CONTRACT_REJECTED",
                "pluginWorkflowId must be a canonical UUID or ULID",
            );
        }
    };
    let run = state.runs.find_by_workflow(&plugin_workflow_id);
    let has_plugin_analytics = run
        .as_ref()
        .and_then(|run| run.external_plugin_analytics.as_ref())
        .is_some_and(|analytics| {
            analytics.get("externalPluginId").and_then(Value::as_str)
                == Some(OPEN_DESIGN_PLUGIN_ID)
        });
    let (Some(run), true) = (run, has_plugin_analytics) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "plugin workflow run not found",
        );
    };
    let analytics = run
        .external_plugin_analytics
        .as_ref()
        .expect("plugin analytics presence checked above");
    let payload = json!({
        "runId": run.id,
        "projectId": run.project_id,
        "pluginWorkflowId": plugin_workflow_id,
        "logicalRequestDigest": analytics.get("logicalRequestDigest").cloned(),
        "logicalRequestDigestVersion": analytics.get("logicalRequestDigestVersion").cloned(),
        "externalPluginContext": {
            "id": analytics.get("externalPluginId").cloned(),
            "version": analytics.get("externalPluginVersion").cloned(),
            "distributionMechanism": analytics.get("distributionMechanism").cloned(),
            "publisherClass": analytics.get("publisherClass").cloned(),
        },
    });
    Json(payload).into_response()
}

/// `GET /api/runs/:id` — status projection, computing (and caching) the
/// deliverable verdict once the run is terminal.
async fn get_run(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if id.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "run id missing");
    }
    let Some(run) = state.runs.get(&id) else {
        return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found");
    };
    let mut status = status_body(&run);
    if !run.is_terminal() {
        return Json(status).into_response();
    }
    if status.get("deliverableValid").is_some()
        && status.get("deliverableValidation").is_some()
    {
        return Json(status).into_response();
    }
    let artifact_count = run.artifact_count;
    let blocking_state = state.clone();
    let blocking_run = run.clone();
    let outcome = match tokio::task::spawn_blocking(move || {
        deliverable_for_run(&blocking_state, &blocking_run, artifact_count)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => return internal_error(&err.to_string()),
    };
    state.runs.set_deliverable(
        &id,
        outcome.valid,
        outcome.validation,
        outcome.entry_file.clone(),
        outcome.artifact_kind.clone(),
    );
    status = status_body(&run);
    let mut fields = Map::new();
    outcome.into_status_fields(&mut fields);
    if let Value::Object(existing) = status {
        status = Value::Object(existing.into_iter().chain(fields).collect());
    }
    Json(status).into_response()
}

/// `GET /api/runs/:id/result-package`.
async fn get_run_result_package(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if id.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "run id missing");
    }
    let Some(run) = state.runs.get(&id) else {
        return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found");
    };
    let status = status_body(&run);
    let project = match run.project_id.as_deref() {
        Some(project_id) => match state.store.get_project(project_id) {
            Ok(project) => project,
            Err(err) => return store_error_response(&err),
        },
        None => None,
    };
    let mut files: Vec<Value> = Vec::new();
    if let Some(project) = &project {
        let folder_backed = status
            .get("workspace")
            .and_then(|workspace| workspace.get("storage"))
            .and_then(|storage| storage.get("kind"))
            == Some(&json!("folder-backed"));
        let base = match project_dir::project_fs_base(&state.config, project) {
            Ok(base) => base,
            Err(err) => {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "WORKSPACE_ENUMERATION_FAILED",
                    &err.to_string(),
                );
            }
        };
        if folder_backed && !base.is_dir() {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "WORKSPACE_ENUMERATION_FAILED",
                "workspace root is not a directory",
            );
        }
        match tokio::task::spawn_blocking(move || project_files::list_files(&base, None)).await {
            Ok(Ok(listed)) => files = listed,
            Ok(Err(err)) => {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "WORKSPACE_ENUMERATION_FAILED",
                    &err.to_string(),
                );
            }
            Err(err) => return internal_error(&err.to_string()),
        }
    }
    let artifacts: Vec<Value> = files
        .iter()
        .filter(|file| {
            file.get("artifactManifest")
                .and_then(Value::as_object)
                .is_some()
        })
        .map(|file| {
            let manifest = file.get("artifactManifest").cloned().unwrap_or(Value::Null);
            let name = file.get("name").and_then(Value::as_str).unwrap_or("");
            let manifest_str = |key: &str| manifest.get(key).and_then(Value::as_str);
            json!({
                "file": name,
                "kind": manifest_str("kind")
                    .map(Value::from)
                    .or_else(|| file.get("artifactKind").cloned())
                    .unwrap_or(Value::Null),
                "renderer": manifest_str("renderer").map(Value::from).unwrap_or(Value::Null),
                "title": manifest_str("title").map(Value::from).unwrap_or_else(|| json!(name)),
                "status": manifest_str("status").map(Value::from).unwrap_or(Value::Null),
                "manifest": manifest,
            })
        })
        .collect();
    let mut run_subset = Map::new();
    for key in [
        "id",
        "status",
        "projectId",
        "conversationId",
        "assistantMessageId",
        "agentId",
        "createdAt",
        "updatedAt",
        "cancelRequested",
        "exitCode",
        "signal",
        "error",
        "errorCode",
    ] {
        if let Some(value) = status.get(key) {
            run_subset.insert(key.to_string(), value.clone());
        }
    }
    let project_payload = project.as_ref().map(|project| {
        json!({
            "id": project.id,
            "name": project.name,
            "fileCount": files.len(),
        })
    });
    let payload = json!({
        "schema": RUN_RESULT_PACKAGE_SCHEMA,
        "run": run_subset,
        "workspace": status.get("workspace").cloned().unwrap_or(json!({
            "storage": { "kind": "od-owned", "baseDir": null },
            "provenance": null,
        })),
        "events": { "logPath": Value::Null },
        "project": project_payload,
        "artifacts": artifacts,
    });
    Json(payload).into_response()
}

/// `GET /api/runs/:id/events` — replay then live SSE fan-out.
async fn stream_run_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    Path(id): Path<String>,
) -> Response {
    if id.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "run id missing");
    }
    let cursor = parse_cursor(&headers, query.as_deref());
    match state.runs.stream(&id, cursor, false) {
        Some(response) => response,
        None => api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found"),
    }
}

/// `GET /api/runs/:id/agui` — the same stream with native events forwarded.
async fn stream_run_agui(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    Path(id): Path<String>,
) -> Response {
    if id.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "run id missing");
    }
    let cursor = parse_cursor(&headers, query.as_deref());
    match state.runs.stream(&id, cursor, true) {
        Some(response) => response,
        None => api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found"),
    }
}

/// `POST /api/runs/:id/cancel`.
async fn cancel_run(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if id.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "run id missing");
    }
    if state.runs.get(&id).is_none() {
        return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found");
    }
    state.runs.cancel(&id, "user_stop");
    let Some(run) = state.runs.get(&id) else {
        return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found");
    };
    Json(json!({ "ok": true, "run": status_body(&run) })).into_response()
}

/// `POST /api/runs/:id/steer` — deliver one more user frame mid-turn.
async fn steer_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    if id.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "run id missing");
    }
    let Some(run) = state.runs.get(&id) else {
        return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found");
    };
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let record = as_record(Some(&body)).cloned().unwrap_or_default();
    let text = record
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "text is required and must be a non-empty string",
        );
    }
    let runtime_accepts = run
        .agent_id
        .as_deref()
        .is_some_and(|agent| STEERABLE_AGENTS.contains(&agent));
    if let Err(refusal) = state.runs.steer(&id, &text, runtime_accepts) {
        // Two distinct codes on purpose: `runtime_unsupported` is permanent
        // for this agent, the closed ones mean "send it as a new turn".
        if refusal == "runtime_unsupported" {
            return api_err_init(
                StatusCode::CONFLICT,
                "RUN_STEERING_UNSUPPORTED",
                &format!(
                    "agent {} cannot take a mid-turn message: its stdin is closed together with the opening prompt",
                    run.agent_id.as_deref().unwrap_or("unknown")
                ),
                json!({ "retryable": false, "details": { "refusal": refusal } }),
            );
        }
        let message = if refusal == "run_terminal" {
            "run already finished; send the message as a new turn"
        } else {
            "the turn already ended and stopped reading input; send the message as a new turn"
        };
        return api_err_init(
            StatusCode::CONFLICT,
            "RUN_STEERING_CLOSED",
            message,
            json!({ "retryable": false, "details": { "refusal": refusal } }),
        );
    }
    // Durability, written only after a successful delivery: a refused steer
    // must leave no trace.
    let message_id = random_id();
    if let Some(conversation_id) = run.conversation_id.as_deref() {
        let now = now_ms();
        let seed = json!({
            "id": message_id,
            "role": "user",
            "content": text,
            "startedAt": now,
            "endedAt": now,
        });
        if let Err(err) = upsert_message(&state.store, conversation_id, &seed) {
            return store_error_response(&err);
        }
        if let Some(project_id) = run.project_id.as_deref().filter(|id| !id.is_empty()) {
            if let Err(err) = bump_project_updated_at(&state.store, project_id) {
                return store_error_response(&err);
            }
        }
    }
    let updated = state.runs.get(&id).unwrap_or(run);
    Json(json!({
        "ok": true,
        "delivered": true,
        "messageId": message_id,
        "run": status_body(&updated),
    }))
    .into_response()
}

/// The ten `/api/runs*` + `/api/chat` routes (parity: `routes/runs.ts`),
/// registered before the `/api/{*rest}` JSON-404 wildcard.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/runs", post(create_run).get(list_runs))
        .route(
            "/api/runs/by-plugin-workflow/{workflowId}",
            get(get_run_by_workflow),
        )
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/result-package", get(get_run_result_package))
        .route("/api/runs/{id}/events", get(stream_run_events))
        .route("/api/runs/{id}/agui", get(stream_run_agui))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/steer", post(steer_run))
        .route("/api/chat", post(create_chat))
}
