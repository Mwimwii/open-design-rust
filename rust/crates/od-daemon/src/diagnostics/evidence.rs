//! Live diagnostic snapshots that ride inside the export zip (parity:
//! `apps/daemon/src/services/diagnostics-evidence.ts`,
//! `diagnostics-environment.ts`, `diagnostics-client-evidence.ts` and
//! `http/api-failure-journal.ts`).
//!
//! Two kinds of fact live here rather than on disk:
//!
//! * **Environment evidence** — proxy configuration, failure groups and the
//!   workspace context the daemon last saw, sampled on demand.
//! * **Renderer evidence** — the chat-scroll capture the web app posts just
//!   before asking for a bundle. Both export paths (browser download and the
//!   desktop native save dialog) funnel through the same `GET
//!   /api/diagnostics/export`, so a rendezvous is the only shape that works for
//!   both; the renderer posts it here first and the export drains it.
//!
//! This port keeps the evidence in memory only. The TypeScript daemon
//! additionally checkpoints to `<dataRoot>/diagnostics/environment-evidence.json`
//! when launched with a writer; without one it reports `memory-only`, which is
//! exactly what this store reports.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::time::now_iso_millis;

pub const MAX_FAILURES: usize = 100;
pub const MAX_BYTES: usize = 256 * 1024;
pub const SAMPLE_INTERVAL_MS: u64 = 60_000;
pub const FLUSH_INTERVAL_MS: u64 = 60_000;
pub const MAX_WORKSPACES: usize = 50;
const MAX_CONFIG_LENGTH: usize = 4096;

/// Where the renderer posts a chat-scroll capture.
pub const CHAT_SCROLL_FORENSICS_PATH: &str = "/api/diagnostics/chat-scroll-forensics";
/// The name the capture lands under inside the zip.
pub const CHAT_SCROLL_FORENSICS_SUMMARY_FILE: &str = "chat-scroll-forensics.json";

pub const NO_RENDERER_CAPTURE_NOTE: &str =
    "No chat-scroll capture was posted by a renderer in this daemon session. The \
     capture is pushed by the web app when someone clicks Export logs, so a bundle \
     produced from the desktop Help menu, from `od diagnostics export`, or from a \
     browser that never had the app open will not contain one. Ask for a re-export \
     from the in-app Export logs button while the stuck chat is still on screen.";

pub const RECENT_API_FAILURES_LIMIT: usize = 100;
pub const RECENT_API_FAILURES_PRIVACY: &str = "Request bodies, query strings, messages, credentials, and resource identifiers are not recorded.";

/// `<dataRoot>/diagnostics/environment-evidence[.previous].json`.
pub fn diagnostics_evidence_paths(data_root: &Path) -> (PathBuf, PathBuf) {
    let directory = data_root.join("diagnostics");
    (
        directory.join("environment-evidence.json"),
        directory.join("environment-evidence.previous.json"),
    )
}

/// `<dataRoot>/diagnostics/daemon-health[.previous].json`.
pub fn daemon_health_paths(data_root: &Path) -> (PathBuf, PathBuf) {
    let directory = data_root.join("diagnostics");
    (
        directory.join("daemon-health.json"),
        directory.join("daemon-health.previous.json"),
    )
}

fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex_prefix(&digest, 16)
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    let mut out = String::with_capacity(len);
    for byte in bytes {
        if out.len() >= len {
            break;
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out.truncate(len);
    out
}

/// Keep endpoint correlation without exporting credentials, hostnames, paths
/// or PAC URLs.
fn proxy_endpoint(value: Option<&str>) -> Value {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return json!({ "configured": false });
    };
    if value.len() > MAX_CONFIG_LENGTH {
        return json!({ "configured": true, "valid": false });
    }
    let candidate = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let Ok(url) = url::Url::parse(&candidate) else {
        return json!({ "configured": true, "valid": false });
    };
    let scheme = url.scheme().to_string();
    if !["http", "https", "socks", "socks5", "socks5h"].contains(&scheme.as_str()) {
        return json!({ "configured": true, "valid": false });
    }
    let host = url.host_str().unwrap_or("").to_string();
    let mut map = Map::new();
    map.insert("configured".into(), Value::Bool(true));
    map.insert("valid".into(), Value::Bool(true));
    map.insert("protocol".into(), Value::String(scheme.clone()));
    map.insert(
        "port".into(),
        match url.port() {
            Some(port) => Value::Number(port.into()),
            None => Value::Null,
        },
    );
    map.insert(
        "endpointFingerprint".into(),
        Value::String(fingerprint(&format!("{scheme}://{host}"))),
    );
    map.insert(
        "loopback".into(),
        Value::Bool(matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")),
    );
    map.insert(
        "credentialsPresent".into(),
        Value::Bool(!url.username().is_empty() || url.password().is_some()),
    );
    Value::Object(map)
}

fn env_get<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str)
}

/// Only this allowlist may enter diagnostics; never serialize an environment
/// object.
pub fn summarize_proxy_environment(env: &HashMap<String, String>) -> Value {
    let bypass_full = env_get(env, "NO_PROXY")
        .or_else(|| env_get(env, "no_proxy"))
        .unwrap_or("");
    let bypass: String = bypass_full.chars().take(MAX_CONFIG_LENGTH).collect();
    let configured = !bypass.is_empty();
    let mut no_proxy = Map::new();
    no_proxy.insert("configured".into(), Value::Bool(configured));
    no_proxy.insert("wildcard".into(), Value::Bool(bypass.trim() == "*"));
    no_proxy.insert(
        "fingerprint".into(),
        if configured {
            Value::String(fingerprint(&bypass))
        } else {
            Value::Null
        },
    );
    json!({
        "http": proxy_endpoint(env_get(env, "HTTP_PROXY").or_else(|| env_get(env, "http_proxy"))),
        "https": proxy_endpoint(env_get(env, "HTTPS_PROXY").or_else(|| env_get(env, "https_proxy"))),
        "all": proxy_endpoint(env_get(env, "ALL_PROXY").or_else(|| env_get(env, "all_proxy"))),
        "noProxy": Value::Object(no_proxy),
        "nodeUseEnvProxy": env_get(env, "NODE_USE_ENV_PROXY") == Some("1"),
    })
}

/// `^[a-zA-Z0-9_-]{1,128}$`.
pub fn diagnostic_id(value: Option<&str>) -> Option<String> {
    let value = value?;
    if value.is_empty() || value.len() > 128 {
        return None;
    }
    value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        .then(|| value.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticFailureCode {
    Timeout,
    Aborted,
    Dns,
    ConnectionRefused,
    Tls,
    Proxy,
    Http,
    Other,
}

#[derive(Debug, Clone, Default)]
pub struct FailureError {
    pub code: Option<String>,
    pub name: Option<String>,
    pub cause_code: Option<String>,
    pub stderr: Option<String>,
}

/// Error text, argv, stdout and stderr can contain secrets. Never retain them.
pub fn diagnostic_failure_code(error: Option<&FailureError>) -> DiagnosticFailureCode {
    let Some(error) = error else {
        return DiagnosticFailureCode::Other;
    };
    let code = error.code.as_deref().or(error.cause_code.as_deref());
    if matches!(
        code,
        Some("ETIMEDOUT") | Some("UND_ERR_CONNECT_TIMEOUT") | Some("UND_ERR_HEADERS_TIMEOUT") | Some("UND_ERR_BODY_TIMEOUT")
    ) || error.name.as_deref() == Some("TimeoutError")
    {
        return DiagnosticFailureCode::Timeout;
    }
    if code == Some("ABORT_ERR") || error.name.as_deref() == Some("AbortError") {
        return DiagnosticFailureCode::Aborted;
    }
    if matches!(code, Some("ENOTFOUND") | Some("EAI_AGAIN")) {
        return DiagnosticFailureCode::Dns;
    }
    if code == Some("ECONNREFUSED") {
        return DiagnosticFailureCode::ConnectionRefused;
    }
    if matches!(
        code,
        Some("CERT_HAS_EXPIRED") | Some("DEPTH_ZERO_SELF_SIGNED_CERT") | Some("UNABLE_TO_VERIFY_LEAF_SIGNATURE")
    ) {
        return DiagnosticFailureCode::Tls;
    }
    let tail: String = error
        .stderr
        .as_deref()
        .map(|stderr| {
            let chars: Vec<char> = stderr.chars().collect();
            chars[chars.len().saturating_sub(4096)..].iter().collect()
        })
        .unwrap_or_default();
    let lower = tail.to_lowercase();
    if lower.contains("proxyconnect") || lower.contains("proxy authentication required") {
        return DiagnosticFailureCode::Proxy;
    }
    if lower.contains("context deadline exceeded") || lower.contains("client.timeout") {
        return DiagnosticFailureCode::Timeout;
    }
    if lower.contains("x509:") || lower.contains("certificate verify failed") {
        return DiagnosticFailureCode::Tls;
    }
    if lower.contains("no such host") {
        return DiagnosticFailureCode::Dns;
    }
    DiagnosticFailureCode::Other
}

#[derive(Debug, Clone, Default)]
pub struct FailureInput {
    pub source: String,
    pub operation: Option<String>,
    pub request_id: Option<String>,
    pub status: Option<i64>,
    pub timed_out: bool,
    pub error: Option<FailureError>,
    pub workspace_id: Option<String>,
    pub member_id: Option<String>,
    pub workspace_type: Option<String>,
    /// Already-used child env, only summarized on a new group or once a minute.
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FailureRecord {
    workspace_id: Option<String>,
    member_id: Option<String>,
    workspace_type: Option<String>,
    source: String,
    code: DiagnosticFailureCode,
    status: Option<i64>,
    operation: Option<String>,
    request_id: Option<String>,
    proxy_sampled_at: Option<String>,
    first_at: String,
    last_at: String,
    count: u64,
    first_environment_at: Option<String>,
    last_environment_at: Option<String>,
    proxy_configuration: Option<Value>,
}

#[derive(Debug, Clone, Default)]
struct EvidenceState {
    failures: Vec<FailureRecord>,
    environments: Vec<Value>,
    directory: Option<Value>,
    context: Option<Value>,
    last_sample_ms: i64,
    dropped_groups: u64,
    environment_collection: &'static str,
}

/// In-memory evidence store. One store belongs to one running daemon, exactly
/// like the module-level singleton in TypeScript.
#[derive(Debug)]
pub struct DiagnosticsEvidence {
    inner: Mutex<EvidenceState>,
}

fn now_millis() -> i64 {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as i64,
        Err(err) => -(err.duration().as_millis() as i64),
    }
}

impl DiagnosticsEvidence {
    pub fn new() -> Self {
        Self::default()
    }

    fn workspace(value_id: Option<&str>, member: Option<&str>, kind: Option<&str>) -> (Option<String>, Option<String>, Option<String>) {
        let workspace_type = match kind {
            Some("team") => Some("team".to_string()),
            Some("personal") => Some("personal".to_string()),
            _ => None,
        };
        (
            diagnostic_id(value_id),
            diagnostic_id(member),
            workspace_type,
        )
    }

    /// Sample the environment when the previous sample is older than the
    /// interval. Collection failure cannot change a business result and never
    /// invalidates the bundle.
    pub fn refresh(&self, env: &HashMap<String, String>) {
        let now = now_millis();
        let mut state = self.inner.lock().expect("evidence lock");
        if now - state.last_sample_ms < SAMPLE_INTERVAL_MS as i64 {
            return;
        }
        state.last_sample_ms = now;
        let snapshot = json!({
            "sampledAt": now_iso_millis(),
            // The Rust daemon does not shell out to a platform proxy query,
            // which is one of the statuses the TypeScript collector itself
            // reports on platforms it cannot probe.
            "status": "unsupported",
            "systemProxy": Value::Null,
            "pacConfigured": Value::Null,
            "daemonProxy": summarize_proxy_environment(env),
            "interfaces": Value::Null,
        });
        state.environment_collection = "settled";
        state.environments.push(snapshot);
        if state.environments.len() > 4 {
            state.environments.remove(0);
        }
    }

    pub fn record(&self, input: &FailureInput) {
        let (workspace_id, member_id, workspace_type) =
            Self::workspace(input.workspace_id.as_deref(), input.member_id.as_deref(), input.workspace_type.as_deref());
        let mut code = diagnostic_failure_code(input.error.as_ref());
        if input.status.is_some() {
            code = DiagnosticFailureCode::Http;
        }
        if input.timed_out {
            code = DiagnosticFailureCode::Timeout;
        }
        let status = input
            .status
            .filter(|status| (400..=599).contains(status));
        let operation = input
            .operation
            .as_deref()
            .filter(|operation| {
                !operation.is_empty()
                    && operation.len() <= 160
                    && operation
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '_' | '.' | ':' | '-'))
            })
            .map(str::to_string);
        let identity_id = workspace_id.clone().unwrap_or_default();
        let member = member_id.clone().unwrap_or_default();
        let kind = workspace_type.clone().unwrap_or_default();
        let key = format!(
            "{}:{}:{:?}:{}:{}:{}:{}",
            operation.clone().unwrap_or_default(),
            input.source,
            code,
            status.map(|value| value.to_string()).unwrap_or_default(),
            identity_id,
            member,
            kind
        );

        let timestamp = now_millis();
        let time = now_iso_millis();
        let mut state = self.inner.lock().expect("evidence lock");
        let environment_at = state
            .environments
            .last()
            .and_then(|snapshot| snapshot.get("sampledAt"))
            .and_then(Value::as_str)
            .map(str::to_string);

        if let Some(index) = state
            .failures
            .iter()
            .position(|record| format!(
                "{}:{}:{:?}:{}:{}:{}:{}",
                record.operation.clone().unwrap_or_default(),
                record.source,
                record.code,
                record.status.map(|value| value.to_string()).unwrap_or_default(),
                record.workspace_id.clone().unwrap_or_default(),
                record.member_id.clone().unwrap_or_default(),
                record.workspace_type.clone().unwrap_or_default()
            ) == key)
        {
            let existing = state.failures.remove(index);
            let mut existing = existing;
            existing.count = existing.count.saturating_add(1);
            if let Some(env) = &input.env {
                let last = existing
                    .proxy_sampled_at
                    .clone()
                    .unwrap_or_else(|| existing.first_at.clone());
                if proxied_sample_due(&last, timestamp) {
                    existing.proxy_configuration = Some(summarize_proxy_environment(env));
                    existing.proxy_sampled_at = Some(time.clone());
                }
            }
            existing.last_at = time;
            existing.request_id = diagnostic_id(input.request_id.as_deref());
            existing.last_environment_at = environment_at;
            state.failures.push(existing);
        } else {
            state.failures.push(FailureRecord {
                workspace_id,
                member_id,
                workspace_type,
                source: input.source.clone(),
                code,
                status,
                operation,
                request_id: diagnostic_id(input.request_id.as_deref()),
                proxy_sampled_at: input.env.as_ref().map(|_| time.clone()),
                first_at: time.clone(),
                last_at: time,
                count: 1,
                first_environment_at: environment_at.clone(),
                last_environment_at: environment_at,
                proxy_configuration: input.env.as_ref().map(summarize_proxy_environment),
            });
            if state.failures.len() > MAX_FAILURES {
                state.failures.remove(0);
                state.dropped_groups += 1;
            }
        }
    }

    pub fn observe_directory(&self, items: &[Value]) {
        let mut state = self.inner.lock().expect("evidence lock");
        let truncated = items.len() > MAX_WORKSPACES;
        let mapped: Vec<Value> = items
            .iter()
            .take(MAX_WORKSPACES)
            .map(|item| {
                let member = item
                    .get("memberId")
                    .or_else(|| item.get("workspaceMemberId"))
                    .and_then(Value::as_str);
                let (workspace_id, member_id, workspace_type) = Self::workspace(
                    item.get("workspaceId").and_then(Value::as_str),
                    member,
                    item.get("workspaceType").and_then(Value::as_str),
                );
                json!({
                    "workspaceId": workspace_id,
                    "memberId": member_id,
                    "workspaceType": workspace_type,
                })
            })
            .collect();
        state.directory = Some(json!({
            "observedAt": now_iso_millis(),
            "truncated": truncated,
            "items": mapped,
        }));
    }

    pub fn observe_context(&self, value: Option<&Value>) {
        let mut state = self.inner.lock().expect("evidence lock");
        state.context = value.map(|value| {
            let (workspace_id, member_id, workspace_type) = Self::workspace(
                value.get("workspaceId").and_then(Value::as_str),
                value.get("memberId").and_then(Value::as_str),
                value.get("workspaceType").and_then(Value::as_str),
            );
            json!({
                "workspaceId": workspace_id,
                "memberId": member_id,
                "workspaceType": workspace_type,
                "observedAt": now_iso_millis(),
            })
        });
    }

    /// The bundle's `environment-evidence.json` document.
    pub fn snapshot(&self) -> Value {
        let state = self.inner.lock().expect("evidence lock");
        json!({
            "schemaVersion": 1,
            "exportedAt": now_iso_millis(),
            "limits": {
                "failures": MAX_FAILURES,
                "bytes": MAX_BYTES,
                "sampleIntervalMs": SAMPLE_INTERVAL_MS,
                "flushIntervalMs": FLUSH_INTERVAL_MS,
            },
            "coverage": {
                "actualNetworkRoute": "not-observed",
                "vpn": "not-determined",
                "workspaceContext": "last-daemon-response-not-browser-selection",
                "failureEnvironment": "cached-sample-at-or-before-failure; null-means-not-yet-sampled",
                "persistence": "memory-only",
                "droppedGroups": state.dropped_groups,
                "environmentCollection": state.environment_collection_or_default(),
            },
            "directory": state.directory.clone().unwrap_or(Value::Null),
            "context": state.context.clone().unwrap_or(Value::Null),
            "environments": state.environments.clone(),
            "failures": state.failures.clone(),
        })
    }
}

impl EvidenceState {
    fn environment_collection_or_default(&self) -> &'static str {
        if self.environment_collection.is_empty() {
            "not-requested"
        } else {
            self.environment_collection
        }
    }
}

/// Whether a new proxy sample is due for an already-recorded failure group.
fn proxied_sample_due(last_sampled_iso: &str, now: i64) -> bool {
    let Ok(parsed) = parse_iso_millis(last_sampled_iso) else {
        return true;
    };
    now - parsed >= SAMPLE_INTERVAL_MS as i64
}

/// Parse the fixed-width ISO-8601 millisecond stamps this module writes.
fn parse_iso_millis(value: &str) -> Result<i64, ()> {
    let bytes = value.as_bytes();
    if bytes.len() != 24 || bytes[23] != b'Z' {
        return Err(());
    }
    let number = |range: std::ops::Range<usize>| -> Result<i64, ()> {
        value.get(range).ok_or(())?.parse::<i64>().map_err(|_| ())
    };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    let millis = number(20..23)?;
    let days = days_from_civil(year, month as u32, day as u32);
    Ok(((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000) + millis)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let m = m as i64;
    let d = d as i64;
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A capture the renderer posted for the current session.
#[derive(Debug, Clone)]
pub struct StoredChatScrollForensics {
    pub received_at_iso: String,
    /// Opaque here on purpose — the renderer owns the shape.
    pub payload: Value,
}

#[derive(Debug, Default)]
pub struct ChatScrollStore {
    inner: Mutex<Option<StoredChatScrollForensics>>,
}

impl ChatScrollStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn store(&self, payload: Value) -> String {
        let received_at_iso = now_iso_millis();
        let mut guard = self.inner.lock().expect("chat scroll lock");
        *guard = Some(StoredChatScrollForensics {
            received_at_iso: received_at_iso.clone(),
            payload,
        });
        received_at_iso
    }

    pub fn read(&self) -> Option<StoredChatScrollForensics> {
        self.inner.lock().expect("chat scroll lock").clone()
    }

    /// Absence is written out in words rather than left as a missing file: a
    /// reader must be able to tell "the renderer had nothing to say" apart
    /// from "this export path never asks the renderer".
    pub fn summary(&self) -> Value {
        match self.read() {
            None => json!({
                "captured": false,
                "receivedAtIso": Value::Null,
                "note": NO_RENDERER_CAPTURE_NOTE,
                "evidence": Value::Null,
            }),
            Some(record) => json!({
                "captured": true,
                "receivedAtIso": record.received_at_iso,
                "note": "Posted by the web app immediately before this bundle was requested.",
                "evidence": record.payload,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentApiFailure {
    pub at: String,
    pub method: String,
    pub path: String,
    pub status: i64,
    pub code: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct ApiFailureJournal {
    inner: Mutex<Vec<RecentApiFailure>>,
}

impl ApiFailureJournal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, failure: RecentApiFailure) {
        let mut guard = self.inner.lock().expect("api failure lock");
        guard.push(failure);
        let len = guard.len();
        if len > RECENT_API_FAILURES_LIMIT {
            guard.drain(0..len - RECENT_API_FAILURES_LIMIT);
        }
    }

    pub fn read(&self) -> Vec<RecentApiFailure> {
        self.inner.lock().expect("api failure lock").clone()
    }
}

/// The bundle's `recent-api-failures.json` document.
pub fn recent_api_failures_summary(journal: &ApiFailureJournal) -> Value {
    json!({
        "retainedLimit": RECENT_API_FAILURES_LIMIT,
        "privacy": RECENT_API_FAILURES_PRIVACY,
        "failures": journal.read(),
    })
}

/// Parity: `diagnosticRouteTemplate` — keep the declared route, never the
/// user-controlled URL a router may have mounted it under.
pub fn diagnostic_route_template(matched_path: Option<&str>) -> String {
    let Some(route_path) = matched_path.filter(|path| !path.is_empty()) else {
        return "/api/:unmatched".to_string();
    };
    let normalized = if route_path.starts_with('/') {
        route_path.to_string()
    } else {
        format!("/{route_path}")
    };
    if normalized == "/api" || normalized.starts_with("/api/") {
        return normalized;
    }
    format!("/api/:mounted{normalized}")
}

/// The store the daemon and the renderer rendezvous through.
#[derive(Debug, Default)]
pub struct DiagnosticsState {
    pub evidence: DiagnosticsEvidence,
    pub chat_scroll: ChatScrollStore,
    pub api_failures: ApiFailureJournal,
}

impl DiagnosticsState {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for DiagnosticsEvidence {
    fn default() -> Self {
        Self {
            inner: Mutex::new(EvidenceState {
                environment_collection: "not-requested",
                ..EvidenceState::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn proxy_summary_keeps_correlation_without_credentials() {
        let summary = summarize_proxy_environment(&env_of(&[
            ("HTTP_PROXY", "http://user:secret@proxy.example.com:8080"),
            ("NO_PROXY", "localhost,*.internal"),
            ("NODE_USE_ENV_PROXY", "1"),
        ]));
        assert_eq!(summary["http"]["configured"], json!(true));
        assert_eq!(summary["http"]["valid"], json!(true));
        assert_eq!(summary["http"]["protocol"], json!("http"));
        assert_eq!(summary["http"]["port"], json!(8080));
        assert_eq!(summary["http"]["credentialsPresent"], json!(true));
        assert_eq!(summary["http"]["loopback"], json!(false));
        assert!(summary["http"]["endpointFingerprint"].is_string());
        assert_eq!(summary["https"]["configured"], json!(false));
        assert_eq!(summary["noProxy"]["configured"], json!(true));
        assert_eq!(summary["noProxy"]["wildcard"], json!(false));
        assert!(summary["noProxy"]["fingerprint"].is_string());
        assert_eq!(summary["nodeUseEnvProxy"], json!(true));
        // The credential never leaves the summary.
        assert!(!summary.to_string().contains("secret"));
    }

    #[test]
    fn proxy_endpoints_reject_unknown_schemes_and_bare_hosts_parse() {
        let summary = summarize_proxy_environment(&env_of(&[
            ("HTTP_PROXY", "ftp://proxy.example.com"),
            ("HTTPS_PROXY", "proxy.example.com:3128"),
        ]));
        assert_eq!(summary["http"], json!({ "configured": true, "valid": false }));
        assert_eq!(summary["https"]["valid"], json!(true));
        assert_eq!(summary["https"]["port"], json!(3128));
    }

    #[test]
    fn failure_codes_map_like_the_typescript_classifier() {
        let code = |error: FailureError| diagnostic_failure_code(Some(&error));
        assert_eq!(code(FailureError { code: Some("ETIMEDOUT".into()), ..Default::default() }), DiagnosticFailureCode::Timeout);
        assert_eq!(code(FailureError { name: Some("TimeoutError".into()), ..Default::default() }), DiagnosticFailureCode::Timeout);
        assert_eq!(code(FailureError { code: Some("ABORT_ERR".into()), ..Default::default() }), DiagnosticFailureCode::Aborted);
        assert_eq!(code(FailureError { code: Some("ENOTFOUND".into()), ..Default::default() }), DiagnosticFailureCode::Dns);
        assert_eq!(code(FailureError { code: Some("ECONNREFUSED".into()), ..Default::default() }), DiagnosticFailureCode::ConnectionRefused);
        assert_eq!(code(FailureError { code: Some("CERT_HAS_EXPIRED".into()), ..Default::default() }), DiagnosticFailureCode::Tls);
        assert_eq!(code(FailureError { stderr: Some("proxyconnect tcp: proxy authentication required".into()), ..Default::default() }), DiagnosticFailureCode::Proxy);
        assert_eq!(code(FailureError { stderr: Some("context deadline exceeded".into()), ..Default::default() }), DiagnosticFailureCode::Timeout);
        assert_eq!(code(FailureError { stderr: Some("no such host".into()), ..Default::default() }), DiagnosticFailureCode::Dns);
        assert_eq!(code(FailureError::default()), DiagnosticFailureCode::Other);
        assert_eq!(diagnostic_failure_code(None), DiagnosticFailureCode::Other);
    }

    #[test]
    fn evidence_groups_failures_and_only_keeps_safe_identifiers() {
        let evidence = DiagnosticsEvidence::new();
        let input = FailureInput {
            source: "local-api".into(),
            operation: Some("/api/projects/:id".into()),
            request_id: Some("req-1".into()),
            status: Some(500),
            workspace_id: Some("bad id!".into()),
            ..Default::default()
        };
        evidence.record(&input);
        evidence.record(&input);
        let snapshot = evidence.snapshot();
        let failures = snapshot["failures"].as_array().unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["count"], json!(2));
        assert_eq!(failures[0]["code"], json!("http"));
        assert_eq!(failures[0]["status"], json!(500));
        // An unsafe workspace id is dropped rather than exported.
        assert_eq!(failures[0]["workspaceId"], json!(null));
        assert_eq!(snapshot["coverage"]["persistence"], json!("memory-only"));
        assert_eq!(snapshot["coverage"]["environmentCollection"], json!("not-requested"));
        assert_eq!(snapshot["limits"]["failures"], json!(100));
        assert_eq!(snapshot["schemaVersion"], json!(1));
    }

    #[test]
    fn a_status_outside_the_error_range_is_not_recorded_as_http() {
        let evidence = DiagnosticsEvidence::new();
        evidence.record(&FailureInput {
            source: "local-api".into(),
            operation: Some("/api/x".into()),
            status: Some(200),
            ..Default::default()
        });
        let snapshot = evidence.snapshot();
        assert_eq!(snapshot["failures"][0]["status"], json!(null));
        assert_eq!(snapshot["failures"][0]["code"], json!("http"));
    }

    #[test]
    fn refresh_samples_the_environment_once() {
        let evidence = DiagnosticsEvidence::new();
        let env = env_of(&[("HTTP_PROXY", "http://proxy.example.com:8080")]);
        evidence.refresh(&env);
        evidence.refresh(&env);
        let snapshot = evidence.snapshot();
        assert_eq!(snapshot["environments"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["environments"][0]["status"], json!("unsupported"));
        assert_eq!(snapshot["environments"][0]["daemonProxy"]["http"]["port"], json!(8080));
        assert_eq!(snapshot["coverage"]["environmentCollection"], json!("settled"));
    }

    #[test]
    fn directory_and_context_observations_are_redacted_ids() {
        let evidence = DiagnosticsEvidence::new();
        evidence.observe_directory(&[json!({ "workspaceId": "ws_1", "workspaceMemberId": "m_1", "workspaceType": "team" })]);
        evidence.observe_context(Some(&json!({ "workspaceId": "ws_2", "memberId": "m_2", "workspaceType": "nope" })));
        let snapshot = evidence.snapshot();
        assert_eq!(snapshot["directory"]["items"][0]["memberId"], json!("m_1"));
        assert_eq!(snapshot["directory"]["items"][0]["workspaceType"], json!("team"));
        assert_eq!(snapshot["directory"]["truncated"], json!(false));
        assert_eq!(snapshot["context"]["workspaceType"], json!(null));
        assert!(snapshot["directory"]["observedAt"].is_string());
    }

    #[test]
    fn chat_scroll_summary_states_absence_in_words() {
        let store = ChatScrollStore::new();
        let absent = store.summary();
        assert_eq!(absent["captured"], json!(false));
        assert_eq!(absent["receivedAtIso"], json!(null));
        assert_eq!(absent["evidence"], json!(null));
        assert!(absent["note"].as_str().unwrap().contains("No chat-scroll capture"));

        store.store(json!({ "chatLogHtml": "<div/>" }));
        let present = store.summary();
        assert_eq!(present["captured"], json!(true));
        assert!(present["receivedAtIso"].is_string());
        assert_eq!(present["evidence"]["chatLogHtml"], json!("<div/>"));
    }

    #[test]
    fn api_failures_are_bounded_and_summarized() {
        let journal = ApiFailureJournal::new();
        for index in 0..(RECENT_API_FAILURES_LIMIT + 5) {
            journal.record(RecentApiFailure {
                at: now_iso_millis(),
                method: "GET".into(),
                path: "/api/:unmatched".into(),
                status: 500,
                code: "INTERNAL".into(),
                retryable: false,
                request_id: Some(format!("req-{index}")),
            });
        }
        let summary = recent_api_failures_summary(&journal);
        assert_eq!(summary["retainedLimit"], json!(100));
        assert_eq!(summary["failures"].as_array().unwrap().len(), RECENT_API_FAILURES_LIMIT);
        assert_eq!(summary["failures"][0]["requestId"], json!("req-5"));
    }

    #[test]
    fn route_templates_never_keep_user_controlled_paths() {
        assert_eq!(diagnostic_route_template(None), "/api/:unmatched");
        assert_eq!(diagnostic_route_template(Some("")), "/api/:unmatched");
        assert_eq!(diagnostic_route_template(Some("/api/projects/{id}")), "/api/projects/{id}");
        assert_eq!(diagnostic_route_template(Some("api/x")), "/api/x");
        assert_eq!(diagnostic_route_template(Some("/other/x")), "/api/:mounted/other/x");
    }

    #[test]
    fn diagnostic_ids_reject_unsafe_values() {
        assert_eq!(diagnostic_id(Some("ok_1-A")), Some("ok_1-A".to_string()));
        assert_eq!(diagnostic_id(Some("bad id")), None);
        assert_eq!(diagnostic_id(Some("")), None);
        assert_eq!(diagnostic_id(Some(&"a".repeat(129))), None);
        assert_eq!(diagnostic_id(None), None);
    }

    #[test]
    fn iso_millis_parsing_round_trips() {
        let stamp = "2026-10-17T13:00:47.465Z";
        let millis = parse_iso_millis(stamp).expect("parse");
        let parts = super::super::time::UtcParts::from_system_time(
            std::time::UNIX_EPOCH + Duration::from_millis(millis as u64),
        );
        assert_eq!(parts.to_iso_millis(), stamp);
        assert!(parse_iso_millis("nonsense").is_err());
    }
}
