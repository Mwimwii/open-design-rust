//! Agent-mode connection tests — Rust parity port of the agent-mode half of
//! `apps/daemon/src/connectionTest.ts` plus `apps/daemon/src/claude-diagnostics.ts`.
//!
//! ## Shape of the port
//!
//! The endpoint always answers HTTP 200 with a JSON envelope; `ok` + `kind`
//! (a [`ConnectionTestKind`]) carry the outcome and `diagnostics.phase` says
//! how far the test got (issue #2248). [`resolve_agent_connection`] owns that
//! decision tree and is *pure* over a [`SmokeOutcome`] plus an
//! [`AgentResolution`], so the failure codes are pinned by fixture-backed tests
//! that never spawn a real agent. [`SpawnAgentProbe`] is the production
//! [`AgentProbe`] that actually resolves the binary and runs the CLI.
//!
//! Deferred to follow-up beads (documented rather than silently dropped):
//! the ACP / dsh-profile / pi-rpc session layers, so those stream formats run
//! through the plain stdout lane in [`SpawnAgentProbe`]; `resolveModelForAgent`
//! live-catalog substitution; and the redaction set of `redact.ts` (this module
//! uses the connection-test redactor for both, since the diagnostics it builds
//! are auth-header shaped).

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::SystemTime;

use regex::Regex;
use serde_json::{Map, Value};

use crate::agent_auth::{
    antigravity_auth_guidance, antigravity_quota_guidance, classify_agent_auth_failure,
    classify_agent_service_failure, cursor_auth_guidance, AgentServiceFailureCode,
    AuthProbeResult, AuthProbeStatus,
};

// ---- constants (parity: connectionTest.ts) ---------------------------------

/// Parity: `SMOKE_PROMPT`.
pub const SMOKE_PROMPT: &str = "Reply with only: ok";
/// Parity: `DEFAULT_AGENT_TIMEOUT_MS`.
const DEFAULT_AGENT_TIMEOUT_MS: u64 = 45_000;
/// Parity: `AGENT_STDOUT_DRAIN_MS`.
const AGENT_STDOUT_DRAIN_MS: u64 = 25;
/// Parity: `AGENT_COMPLETION_DEBOUNCE_MS`.
const AGENT_COMPLETION_DEBOUNCE_MS: u64 = 500;
/// Parity: `SAMPLE_MAX_CHARS`.
const SAMPLE_MAX_CHARS: usize = 120;
/// Parity: `MAX_CONNECTION_TEST_TIMEOUT_MS` — `setTimeout` clamps above this.
const MAX_CONNECTION_TEST_TIMEOUT_MS: u64 = 2_147_483_647;
/// Parity: `AGENT_KILL_GRACE_MS`.
const AGENT_KILL_GRACE_MS: u64 = 2_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parity: `resolveConnectionTestTimeoutMs` — an env override wins when it
/// parses to a positive integer at or below the clamp, otherwise the default.
pub fn resolve_connection_test_timeout_ms(
    key: &str,
    fallback: u64,
    env: &HashMap<String, String>,
) -> u64 {
    let Some(raw) = env.get(key) else {
        return fallback;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fallback;
    }
    match trimmed.parse::<u64>() {
        Ok(value) if (1..=MAX_CONNECTION_TEST_TIMEOUT_MS).contains(&value) => value,
        _ => fallback,
    }
}

/// Parity: `agentTimeoutMs()`.
pub fn agent_timeout_ms(env: &HashMap<String, String>) -> u64 {
    resolve_connection_test_timeout_ms(
        "OD_CONNECTION_TEST_AGENT_TIMEOUT_MS",
        DEFAULT_AGENT_TIMEOUT_MS,
        env,
    )
}

// ---- redaction / sampling ---------------------------------------------------

fn escape_regex(value: &str) -> String {
    regex::escape(value)
}

/// Parity: `redactSecrets` (connectionTest.ts:861) — `Bearer …`,
/// `x-api-key`/`api-key`/`x-goog-api-key` headers, `?key=…` query strings, and
/// any explicitly supplied secret.
pub fn redact_secrets(text: &str, exact_secrets: &[&str]) -> String {
    if text.is_empty() {
        return String::new();
    }
    static BEARER: OnceLock<Regex> = OnceLock::new();
    static HEADER: OnceLock<Regex> = OnceLock::new();
    static QUERY: OnceLock<Regex> = OnceLock::new();
    let bearer = BEARER.get_or_init(|| {
        Regex::new(r"(?i)Bearer\s+[A-Za-z0-9_\-.+/=]+").expect("bearer regex")
    });
    let header = HEADER.get_or_init(|| {
        Regex::new(r#"(?i)(x-api-key|api-key|x-goog-api-key)\s*[:=]\s*[^\s,;"']+"#)
            .expect("api key header regex")
    });
    let query =
        QUERY.get_or_init(|| Regex::new(r"(?i)([?&]key=)[^&\s]+").expect("query key regex"));
    let mut out = bearer.replace_all(text, "Bearer [REDACTED]").into_owned();
    out = header.replace_all(&out, "$1: [REDACTED]").into_owned();
    out = query.replace_all(&out, "$1[REDACTED]").into_owned();
    for secret in exact_secrets {
        if secret.is_empty() {
            continue;
        }
        let pattern = format!("(?-u){}", escape_regex(secret));
        if let Ok(re) = Regex::new(&pattern) {
            out = re.replace_all(&out, "[REDACTED]").into_owned();
        }
    }
    out
}

/// Parity: `truncateSample` (connectionTest.ts:889) — JS `\s+` collapsed to one
/// space, trimmed, then capped at [`SAMPLE_MAX_CHARS`] with an ellipsis.
pub fn truncate_sample(text: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for ch in text.chars() {
        if crate::plain_stream::js_whitespace(ch) {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    if out.chars().count() <= SAMPLE_MAX_CHARS {
        return out;
    }
    let mut capped: String = out.chars().take(SAMPLE_MAX_CHARS - 1).collect();
    capped.push('…');
    capped
}

/// Parity: `isSmokeOkReply`.
pub fn is_smoke_ok_reply(text: &str) -> bool {
    text.trim().to_lowercase() == "ok"
}

/// Parity: `isLikelyModelErrorText`.
pub fn is_likely_model_error_text(text: &str) -> bool {
    static NO_ENDPOINT: OnceLock<Regex> = OnceLock::new();
    static MODEL: OnceLock<Regex> = OnceLock::new();
    static FAILURE: OnceLock<Regex> = OnceLock::new();
    let no_endpoint = NO_ENDPOINT.get_or_init(|| {
        Regex::new(r"(?i)no (?:available )?endpoints? found for\b").expect("endpoint regex")
    });
    if no_endpoint.is_match(text) {
        return true;
    }
    let model = MODEL.get_or_init(|| Regex::new(r"(?i)model").expect("model regex"));
    let failure = FAILURE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)(not found|not exist|does not exist|unknown|invalid|unsupported|not supported",
            r"|not have access|no access|issue with the selected model)"
        ))
        .expect("model failure regex")
    });
    model.is_match(text) && failure.is_match(text)
}

/// Parity: `smokeFailureDetail`.
pub fn smoke_failure_detail(sample: &str) -> String {
    if sample.is_empty() {
        "Provider returned a 2xx response without assistant text".to_string()
    } else {
        format!("Expected smoke test reply \"ok\"; got \"{sample}\"")
    }
}

/// Parity: `isTerminalCompletionStopReason`.
pub fn is_terminal_completion_stop_reason(value: &Value) -> bool {
    value
        .as_str()
        .map(|text| !text.is_empty() && text != "tool_use")
        .unwrap_or(false)
}

// ---- Claude result frame ----------------------------------------------------

/// Parity: the object `parseClaudeResultFrame` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeResultFrame {
    pub result_text: String,
    pub stop_reason: Option<String>,
    pub is_error: bool,
    pub subtype: Option<String>,
}

/// Parity: `parseClaudeResultFrame` (connectionTest.ts:1900) — the *last*
/// `type:"result"` JSONL frame on stdout wins.
pub fn parse_claude_result_frame(stdout: &str) -> Option<ClaudeResultFrame> {
    let mut parsed: Option<ClaudeResultFrame> = None;
    for raw_line in stdout.split('\n') {
        let trimmed = raw_line.trim_end_matches('\r').trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("result") {
            continue;
        }
        let result_text = obj
            .get("result")
            .and_then(Value::as_str)
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
        let stop_reason = obj
            .get("stop_reason")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .or_else(|| {
                obj.get("terminal_reason")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            })
            .map(str::to_string);
        parsed = Some(ClaudeResultFrame {
            result_text,
            stop_reason,
            is_error: obj.get("is_error").map(truthy).unwrap_or(false),
            subtype: obj
                .get("subtype")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    parsed
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

// ---- OpenCode helpers -------------------------------------------------------

/// Parity: `extractOpenCodeTextFromRawStdout`.
pub fn extract_opencode_text_from_raw_stdout(stdout: &str) -> String {
    let mut text = Vec::new();
    for raw_line in stdout.split('\n') {
        let trimmed = raw_line.trim_end_matches('\r').trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(part) = obj.get("part").and_then(Value::as_object) {
            if let Some(value) = part.get("text").and_then(Value::as_str) {
                text.push(value.to_string());
            }
        }
    }
    text.join("")
}

const OPENCODE_OUTDATED_CLI_DETAIL: &str =
    "OpenCode CLI appears to be outdated or incompatible with this connection test. Update it with `npm i -g opencode-ai@latest`, then retry the OpenCode connection test.";
const OPENCODE_PROVIDER_CONNECTIVITY_DETAIL_MAX_LENGTH: usize = 240;

/// Parity: `openCodeOutdatedCliDetail`.
pub fn opencode_outdated_cli_detail(output: &str) -> Option<String> {
    let value = output.to_lowercase();
    static HELP: OnceLock<Regex> = OnceLock::new();
    static ARGS: OnceLock<Regex> = OnceLock::new();
    let help = HELP.get_or_init(|| {
        Regex::new(r"(?i)\bopencode\b").expect("opencode regex")
    });
    let commands = Regex::new(r"(?i)\b(?:usage|commands|options):").expect("commands regex");
    let looks_like_help = help.is_match(&value) && commands.is_match(&value);
    let args = ARGS.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)\b(?:unknown option|unknown argument|unexpected argument",
            r"|unrecognized option|invalid option|incompatible opencode args)\b"
        ))
        .expect("unsupported args regex")
    });
    if looks_like_help || args.is_match(&value) {
        Some(OPENCODE_OUTDATED_CLI_DETAIL.to_string())
    } else {
        None
    }
}

/// Parity: `openCodeProviderConnectivityDetail`.
pub fn opencode_provider_connectivity_detail(output: &str) -> Option<String> {
    static CONNECT: OnceLock<Regex> = OnceLock::new();
    static UNABLE: OnceLock<Regex> = OnceLock::new();
    static TYPO: OnceLock<Regex> = OnceLock::new();
    let connect = CONNECT.get_or_init(|| {
        Regex::new(r#"(?i)\bCannot connect to API:[^"'`\r\n]+"#).expect("connect regex")
    });
    let unable = UNABLE.get_or_init(|| {
        Regex::new(r#"(?i)\bUnable to connect[^"'`\r\n]+"#).expect("unable regex")
    });
    let typo = TYPO
        .get_or_init(|| Regex::new(r"(?i)\bWas there a typo in the url or port\?").expect("typo regex"));
    for raw_line in output.split('\n') {
        let line: String = raw_line
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if line.is_empty() {
            continue;
        }
        let Some(found) = connect
            .find(&line)
            .or_else(|| unable.find(&line))
            .or_else(|| typo.find(&line))
        else {
            continue;
        };
        let detail = found.as_str().trim();
        let bounded = if detail.chars().count() > OPENCODE_PROVIDER_CONNECTIVITY_DETAIL_MAX_LENGTH {
            let head: String = detail
                .chars()
                .take(OPENCODE_PROVIDER_CONNECTIVITY_DETAIL_MAX_LENGTH - 3)
                .collect();
            format!("{}...", head.trim_end())
        } else {
            detail.to_string()
        };
        return Some(format!(
            "OpenCode reported a provider connectivity failure before the connection test timed out: {bounded}"
        ));
    }
    None
}

// ---- Codex executable guidance ---------------------------------------------

/// Parity: `codexExecutableGuidance`.
pub fn codex_executable_guidance(
    agent_id: &str,
    configured_override_path: Option<&str>,
    path_resolved_path: Option<&str>,
) -> String {
    let (Some(configured), Some(resolved)) = (configured_override_path, path_resolved_path) else {
        return String::new();
    };
    if agent_id != "codex" || configured == resolved {
        return String::new();
    }
    format!(
        " Configured Codex path failed: {configured}. OpenDesign also detected a PATH Codex CLI at {resolved}. Update CODEX_BIN or clear the custom path to use the detected binary."
    )
}

/// Parity: `codexExecutableFallbackSuccessDetail`.
pub fn codex_executable_fallback_success_detail(
    configured_override_path: &str,
    path_resolved_path: &str,
) -> String {
    format!(
        "Configured Codex path failed: {configured_override_path}. This test succeeded with the PATH Codex CLI at {path_resolved_path}. Update CODEX_BIN or clear the custom path to use the detected binary."
    )
}

/// Parity: `codexConfiguredPathSuccessDetail`.
pub fn codex_configured_path_success_detail(configured_override_path: &str) -> String {
    format!("This test used the configured Codex path: {configured_override_path}.")
}

/// Parity: `codexInvalidConfiguredPathFallbackDetail`.
pub fn codex_invalid_configured_path_fallback_detail(
    configured_value: &str,
    path_resolved_path: &str,
) -> String {
    format!(
        "Configured Codex path is invalid or not executable: {configured_value}. This test used the PATH Codex CLI at {path_resolved_path}. Update CODEX_BIN or clear the custom path to use the detected binary."
    )
}

// ---- agent CLI env prefs ----------------------------------------------------

/// Parity: `AGENT_CLI_ENV_KEYS` (app-config.ts:205) — which env vars a given
/// agent may be configured with. Kept beside its only Rust consumer for now;
/// the app-config bead moves it when prefs persistence lands.
pub fn agent_cli_env_keys(agent_id: &str) -> Option<&'static [&'static str]> {
    Some(match agent_id {
        "amr" => &[
            "VELA_BIN",
            "VELA_API_URL",
            "VELA_LINK_URL",
            "VELA_RUNTIME_KEY",
            "VELA_OPENCODE_BIN",
            "OPEN_DESIGN_AMR_PROFILE",
            "OPENCODE_TEST_HOME",
        ],
        "aider" => &["AIDER_BIN"],
        "claude" => &[
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_BIN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "MMD_MODEL_ROUTES_FILE",
        ],
        "codex" => &[
            "CODEX_HOME",
            "CODEX_BIN",
            "OPENAI_BASE_URL",
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
        ],
        "copilot" => &["COPILOT_BIN"],
        "cursor-agent" => &["CURSOR_AGENT_BIN"],
        "deepseek" => &["DEEPSEEK_BIN"],
        "devin" => &["DEVIN_BIN"],
        "mimo" => &["MIMO_BIN"],
        "hermes" => &["HERMES_BIN"],
        "kimi" => &["KIMI_BIN"],
        "kiro" => &["KIRO_BIN"],
        "kilo" => &["KILO_BIN"],
        "opencode" => &["OPENCODE_BIN"],
        "pi" => &["PI_BIN", "PI_CODING_AGENT_DIR"],
        "qoder" => &["QODER_BIN"],
        "qwen" => &["QWEN_BIN"],
        "trae-cli" => &["TRAE_CLI_BIN"],
        "vibe" => &["VIBE_BIN"],
        _ => return None,
    })
}

/// Parity: `validateAgentCliEnv` — drops unknown agents and non-string values.
pub fn validate_agent_cli_env(raw: Option<&Value>) -> Option<Map<String, Value>> {
    let obj = raw?.as_object()?;
    let mut result = Map::new();
    for (agent_id, value) in obj {
        if agent_id == "__proto__" || agent_id == "constructor" {
            continue;
        }
        let Some(allowed) = agent_cli_env_keys(agent_id) else {
            continue;
        };
        let Some(entry) = value.as_object() else {
            continue;
        };
        let mut env = Map::new();
        for (env_key, env_value) in entry {
            if !allowed.contains(&env_key.as_str()) {
                continue;
            }
            let Some(text) = env_value.as_str() else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            env.insert(env_key.clone(), Value::String(text.to_string()));
        }
        if !env.is_empty() {
            result.insert(agent_id.clone(), Value::Object(env));
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Parity: `agentCliEnvForAgent` — `byok-opencode` reads the `opencode` prefs.
pub fn agent_cli_env_for_agent(
    prefs: Option<&Map<String, Value>>,
    agent_id: &str,
) -> HashMap<String, String> {
    let Some(prefs) = prefs else {
        return HashMap::new();
    };
    let key = if agent_id == "byok-opencode" {
        "opencode"
    } else {
        agent_id
    };
    let Some(Value::Object(env)) = prefs.get(key) else {
        return HashMap::new();
    };
    env.iter()
        .filter_map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
        .collect()
}

/// Parity: `stripCodexBinOverride`.
pub fn strip_codex_bin_override(prefs: Option<Map<String, Value>>) -> Option<Map<String, Value>> {
    let mut prefs = prefs?;
    let has_codex_bin = prefs
        .get("codex")
        .and_then(Value::as_object)
        .and_then(|codex| codex.get("CODEX_BIN"))
        .and_then(Value::as_str)
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    if !has_codex_bin {
        return Some(prefs);
    }
    let mut next_codex = prefs
        .get("codex")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    next_codex.remove("CODEX_BIN");
    if next_codex.is_empty() {
        prefs.remove("codex");
    } else {
        prefs.insert("codex".to_string(), Value::Object(next_codex));
    }
    if prefs.is_empty() {
        None
    } else {
        Some(prefs)
    }
}

// ---- Claude CLI diagnostics ------------------------------------------------

/// Parity: `ClaudeCliDiagnosticInput`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeCliDiagnosticInput {
    pub agent_id: String,
    pub exit_code: Option<i64>,
    pub signal: Option<String>,
    pub stderr_tail: Option<String>,
    pub stdout_tail: Option<String>,
    pub env: HashMap<String, String>,
    pub resolved_bin: Option<String>,
}

/// Parity: `ClaudeCliDiagnostic`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeCliDiagnostic {
    pub message: String,
    pub detail: String,
    pub retryable: bool,
    pub code: Option<String>,
}

fn env_value(env: &HashMap<String, String>, key: &str) -> Option<String> {
    let found = env.keys().find(|candidate| candidate.to_uppercase() == key)?;
    let value = env.get(found)?;
    if value.trim().is_empty() {
        None
    } else {
        Some(value.trim().to_string())
    }
}

/// Parity: `selectedClaudeCompatibleRuntime`.
fn selected_claude_compatible_runtime(input: &ClaudeCliDiagnosticInput) -> &'static str {
    let Some(bin) = input.resolved_bin.as_deref().map(str::trim).filter(|s| !s.is_empty())
    else {
        return "claude";
    };
    let normalized = bin.replace('\\', "/");
    let base = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    let base = if base.len() > 4 {
        let lower = base.to_lowercase();
        if lower.ends_with(".exe") || lower.ends_with(".cmd") || lower.ends_with(".bat") {
            &base[..base.len() - 4]
        } else {
            base
        }
    } else {
        base
    };
    if base.eq_ignore_ascii_case("openclaude") {
        "openclaude"
    } else {
        "claude"
    }
}

/// Parity: `diagnoseClaudeCliFailure` (claude-diagnostics.ts:78).
pub fn diagnose_claude_cli_failure(
    input: &ClaudeCliDiagnosticInput,
) -> Option<ClaudeCliDiagnostic> {
    if input.agent_id != "claude" {
        return None;
    }
    if input.exit_code == Some(0) && input.signal.is_none() {
        return None;
    }
    let body = [input.stderr_tail.as_deref(), input.stdout_tail.as_deref()]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let normalized = body.to_lowercase();
    let has_custom_base_url = env_value(&input.env, "ANTHROPIC_BASE_URL").is_some();
    let has_config_dir = env_value(&input.env, "CLAUDE_CONFIG_DIR").is_some();
    let runtime = selected_claude_compatible_runtime(input);
    let is_openclaude = runtime == "openclaude";
    let runtime_label = if is_openclaude { "OpenClaude" } else { "Claude Code" };
    let default_endpoint_label = if is_openclaude {
        "its configured endpoint"
    } else {
        "the Anthropic API"
    };

    let connection_refused = has_custom_base_url
        && (normalized.contains("connectionrefused")
            || normalized.contains("connection refused")
            || normalized.contains("econnrefused"));
    if connection_refused {
        return Some(with_context(
            "Claude Code could not reach the configured custom Anthropic endpoint.",
            "ANTHROPIC_BASE_URL appears to point at a local or proxy endpoint that refused the connection. Start or fix that proxy, clear the stale endpoint, or remove the custom endpoint to retry with standard Claude Code auth.",
            input,
            None,
        ));
    }

    let connection_dropped = [
        "socket connection was closed",
        "closed unexpectedly",
        "socket hang up",
        "econnreset",
        "etimedout",
        "epipe",
        "und_err_socket",
        "premature close",
        "other side closed",
        "fetch failed",
        "connection error",
        "connection reset",
        "connection closed",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
        || Regex::new(r"(?i)unable to connect to api \((econnreset|etimedout)\)")
            .map(|re| re.is_match(&normalized))
            .unwrap_or(false);
    if connection_dropped {
        if has_custom_base_url {
            let fallback = if is_openclaude {
                "check the OpenClaude endpoint configuration."
            } else {
                "remove ANTHROPIC_BASE_URL to retry with standard Claude Code auth."
            };
            return Some(with_context(
                &format!("{runtime_label} lost its connection to the configured custom Anthropic endpoint before the response finished."),
                &format!("The connection to ANTHROPIC_BASE_URL was closed mid-stream — often a proxy or relay that drops long-lived streaming requests, and most likely on large generations. Retry; if it keeps happening, raise the proxy idle/stream timeout, or {fallback}"),
                input,
                Some("AGENT_CONNECTION_DROPPED"),
            ));
        }
        return Some(with_context(
            &format!("{runtime_label} lost its connection to {default_endpoint_label} before the response finished."),
            "The network connection was closed mid-response — common on unstable networks, VPNs, or proxies that drop long-lived streaming requests, and most likely on large generations. Retry the request.",
            input,
            Some("AGENT_CONNECTION_DROPPED"),
        ));
    }

    static AUTH: OnceLock<Regex> = OnceLock::new();
    let auth_re = AUTH.get_or_init(|| {
        Regex::new(concat!(
            r#"(?i)\b401\b|apikeysource["'\s:]+none|not logged in|please run /login"#,
            r"|(auth|oauth|credential|token).*(fail|invalid|missing|expired|not found|none|unauthorized)",
            r"|(unauthorized|invalid api key|missing api key|could not authenticate|authentication failed)"
        ))
        .expect("claude auth regex")
    });
    if auth_re.is_match(&body) && has_custom_base_url {
        return Some(with_context(
            "Claude Code could not authenticate with the configured custom Anthropic endpoint.",
            "Check ANTHROPIC_BASE_URL, proxy credentials, endpoint authentication environment, and model access. Remove the custom endpoint only if you want to retry with standard Claude Code auth.",
            input,
            None,
        ));
    }
    if auth_re.is_match(&body) {
        if is_openclaude {
            return Some(with_context(
                "OpenClaude could not authenticate with its configured endpoint.",
                "The spawned OpenClaude process exited before producing a response. Check the OpenClaude API key, endpoint, and local configuration, then retry.",
                input,
                None,
            ));
        }
        let config_hint = if has_config_dir {
            "The configured Claude config directory may contain stale or expired auth state."
        } else {
            "If you use multiple Claude profiles, set CLAUDE_CONFIG_DIR in Settings so OpenDesign spawns the same profile that works in your terminal."
        };
        return Some(with_context(
            "Claude Code could not authenticate. Run `claude`, use `/login`, then retry the OpenDesign request.",
            &format!("The spawned Claude Code process exited before producing a response. {config_hint}"),
            input,
            None,
        ));
    }

    static MODEL_UNAVAILABLE: OnceLock<Regex> = OnceLock::new();
    let model_re = MODEL_UNAVAILABLE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)selected model is not available|current plan or region",
            r"|(model).*(not available|not supported|unsupported|not found|not have access|no access)"
        ))
        .expect("claude model regex")
    });
    if model_re.is_match(&body) && has_custom_base_url {
        return Some(with_context(
            "Claude Code could not access the selected model through the configured custom endpoint.",
            "The custom ANTHROPIC_BASE_URL or proxy may not expose the model Claude Code selected. Change the model, fix the endpoint/proxy, or remove ANTHROPIC_BASE_URL and retry with standard Claude Code auth.",
            input,
            None,
        ));
    }

    let windows_mismatch = ["credential manager", "wsl", "powershell", "native windows"]
        .iter()
        .any(|needle| normalized.contains(needle));
    if windows_mismatch {
        return Some(with_context(
            "Claude Code appears to be using credentials from a different local environment.",
            "Re-authenticate Claude Code in the same Windows, WSL, or shell environment that OpenDesign uses. On native Windows, check Windows Credential Manager if `/login` does not repair the session.",
            input,
            None,
        ));
    }

    let config_state = Regex::new(r"(?i)(config|profile|session|credential|oauth)")
        .expect("config state regex")
        .is_match(&body)
        && Regex::new(r"(?i)(stale|corrupt|expired|different|missing|not found|invalid)")
            .expect("config state tail regex")
            .is_match(&body);
    if config_state {
        let (message, detail) = if has_config_dir {
            (
                "Claude Code failed while using the configured Claude profile.",
                "Re-run `claude` and `/login` for that profile, then retry OpenDesign.",
            )
        } else {
            (
                "Claude Code may be using a different or stale local profile than your terminal.",
                "Run `claude` and `/login`, or set CLAUDE_CONFIG_DIR in Settings when you use multiple Claude profiles.",
            )
        };
        return Some(with_context(message, detail, input, None));
    }

    if body.trim().is_empty() && input.exit_code == Some(1) && has_custom_base_url {
        return Some(with_context(
            "Claude Code exited before producing diagnostics while using a custom Anthropic endpoint.",
            "Check ANTHROPIC_BASE_URL, proxy credentials, endpoint authentication environment, and model access. Remove the custom endpoint only if you want to retry with standard Claude Code auth.",
            input,
            None,
        ));
    }

    if body.trim().is_empty() && input.exit_code == Some(1) {
        if is_openclaude {
            return Some(with_context(
                "OpenClaude exited before producing diagnostics.",
                "Check the OpenClaude API key, endpoint, and local configuration, then retry.",
                input,
                None,
            ));
        }
        let message = if has_config_dir {
            "Claude Code exited before producing diagnostics while using the configured Claude profile."
        } else {
            "Claude Code exited before producing diagnostics."
        };
        let detail = if has_config_dir {
            "Re-run `claude` and `/login` for that profile, then retry OpenDesign."
        } else {
            "Run `claude`, use `/login`, and retry. If you use multiple Claude profiles, set CLAUDE_CONFIG_DIR in Settings so OpenDesign uses the same profile as your terminal."
        };
        return Some(with_context(message, detail, input, None));
    }

    if normalized.contains("anthropic_base_url") && has_custom_base_url {
        return Some(with_context(
            "Claude Code failed while using a custom Anthropic endpoint.",
            "Check the ANTHROPIC_BASE_URL endpoint, proxy, model access, and authentication settings, then retry.",
            input,
            None,
        ));
    }

    None
}

/// Parity: `withContext`.
fn with_context(
    message: &str,
    detail: &str,
    input: &ClaudeCliDiagnosticInput,
    code: Option<&str>,
) -> ClaudeCliDiagnostic {
    let has_custom_base_url = env_value(&input.env, "ANTHROPIC_BASE_URL");
    let config_dir = env_value(&input.env, "CLAUDE_CONFIG_DIR");
    let runtime_label = if selected_claude_compatible_runtime(input) == "openclaude" {
        "OpenClaude"
    } else {
        "Claude Code"
    };
    let body = [input.stderr_tail.as_deref(), input.stdout_tail.as_deref()]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let redacted_body = redact_secrets(&body, &[]);
    let collapsed = truncate_sample_hard(&redacted_body, 240);
    let mut context: Vec<String> = vec![message.to_string(), detail.to_string()];
    if !collapsed.is_empty() {
        context.push(format!("Claude output: {collapsed}"));
    }
    if let Some(config_dir) = &config_dir {
        context.push(format!("Effective CLAUDE_CONFIG_DIR: {config_dir}."));
    }
    if has_custom_base_url.is_some() {
        context.push(format!(
            "ANTHROPIC_BASE_URL is set for this {runtime_label} process."
        ));
    }
    let detail_text: Vec<String> = context.into_iter().filter(|part| !part.is_empty()).collect();
    ClaudeCliDiagnostic {
        message: redact_secrets(message, &[]),
        detail: redact_secrets(&detail_text.join(" "), &[]),
        retryable: true,
        code: code.map(str::to_string),
    }
}

/// `redactSecrets(body).replace(/\s+/g,' ').trim().slice(-240)`.
fn truncate_sample_hard(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    let chars: Vec<char> = out.chars().collect();
    if chars.len() <= max {
        return out;
    }
    chars[chars.len() - max..].iter().collect()
}

// ---- probe interface --------------------------------------------------------

pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Parity: `ConnectionTestKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionTestKind {
    Success,
    AuthFailed,
    Forbidden,
    NotFoundModel,
    InvalidModelId,
    InvalidBaseUrl,
    RateLimited,
    UpstreamUnavailable,
    Timeout,
    AgentNotInstalled,
    AgentAuthRequired,
    AgentSpawnFailed,
    Unknown,
}

impl ConnectionTestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionTestKind::Success => "success",
            ConnectionTestKind::AuthFailed => "auth_failed",
            ConnectionTestKind::Forbidden => "forbidden",
            ConnectionTestKind::NotFoundModel => "not_found_model",
            ConnectionTestKind::InvalidModelId => "invalid_model_id",
            ConnectionTestKind::InvalidBaseUrl => "invalid_base_url",
            ConnectionTestKind::RateLimited => "rate_limited",
            ConnectionTestKind::UpstreamUnavailable => "upstream_unavailable",
            ConnectionTestKind::Timeout => "timeout",
            ConnectionTestKind::AgentNotInstalled => "agent_not_installed",
            ConnectionTestKind::AgentAuthRequired => "agent_auth_required",
            ConnectionTestKind::AgentSpawnFailed => "agent_spawn_failed",
            ConnectionTestKind::Unknown => "unknown",
        }
    }
}

/// Parity: the `AgentTestRequest` fields the daemon reads.
#[derive(Debug, Clone, Default)]
pub struct AgentConnectionRequest {
    pub agent_id: String,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub service_tier: Option<String>,
    pub agent_cli_env: Option<Value>,
}

/// Parity: `executableResolution` combined with the resolved def's name.
#[derive(Debug, Clone, Default)]
pub struct AgentResolution {
    pub agent_name: String,
    pub selected_path: Option<String>,
    pub launch_path: Option<String>,
    pub path_resolved_path: Option<String>,
    pub configured_override_path: Option<String>,
    pub diagnostic: Option<String>,
    /// The launch environment, used by `diagnoseClaudeCliFailure`.
    pub env: HashMap<String, String>,
}

/// Parity: the early binary-resolution outcomes.
#[derive(Debug, Clone)]
pub enum ResolutionOutcome {
    /// `getAgentDef` returned nothing.
    UnknownAgent,
    /// The def exists but no executable resolved.
    NotInstalled,
    Ready(AgentResolution),
}

/// Parity: `AgentChildExit` (`{ kind:'exit', code, signal }`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChildExit {
    pub code: Option<i64>,
    pub signal: Option<String>,
}

/// Parity: `{ kind:'spawnError', error }`.
#[derive(Debug, Clone)]
pub struct SpawnFailure {
    pub message: String,
    /// `(error as NodeJS.ErrnoException).code` — `ENOENT` becomes
    /// `agent_not_installed`.
    pub code: Option<String>,
}

/// Parity: the `winner` / `completion` of the connection-test race.
#[derive(Debug, Clone, Default)]
pub enum Winner {
    #[default]
    Text,
    StreamError(String),
    ProviderConnectivity(String),
    Timeout,
    Aborted,
    Exit(Result<ChildExit, SpawnFailure>),
}

/// Everything the pure decision tree needs from the smoke run.
#[derive(Debug, Clone, Default)]
pub struct SmokeOutcome {
    pub winner: Winner,
    /// Set only when `winner` is [`Winner::Text`]; the completion of the race.
    pub completion: Option<Winner>,
    /// `sink.getText()`.
    pub text: String,
    /// `sink.getRawStdout()`.
    pub raw_stdout: String,
    /// `sink.getRawStdoutTail()` (bounded to 400 bytes in the sink).
    pub raw_stdout_tail: String,
    /// `sink.getStderrTail()` (bounded to 400 bytes in the sink).
    pub stderr_tail: String,
    /// From an `initializing` status frame.
    pub resolved_model: Option<String>,
    /// `sink.sawTerminalCompletion()`.
    pub saw_terminal_completion: bool,
    /// Parity: `acpSession.completedSuccessfully()`.
    pub acp_clean_completion: bool,
    /// Parity: `acpSession.hasFatalError()`.
    pub acp_fatal: bool,
    /// From the post-exit antigravity `--log-file` read.
    pub antigravity_log_tail: Option<String>,
}

/// What one probe run produced.
#[derive(Debug, Clone)]
pub enum ProbeOutcome {
    /// `def.buildArgs` threw — parity: the buildArgs catch block.
    BuildArgsFailed(String),
    /// The pre-spawn auth probe reported `missing`.
    AuthRequired(AuthProbeResult),
    Smoke(SmokeOutcome),
}

/// The seam that makes the decision tree fixture-testable.
pub trait AgentProbe: Send + Sync {
    /// Parity: `resolveAgentLaunch(def, configuredAgentEnv)`.
    fn resolve<'a>(
        &'a self,
        agent_id: &'a str,
        configured_env: &'a HashMap<String, String>,
    ) -> BoxFuture<'a, ResolutionOutcome>;
    /// Parity: buildArgs → pre-spawn auth probe → spawn → child race.
    fn run<'a>(
        &'a self,
        request: &'a AgentConnectionRequest,
        resolution: &'a AgentResolution,
    ) -> BoxFuture<'a, ProbeOutcome>;
}

// ---- response construction --------------------------------------------------

/// Parity: `ConnectionTestDiagnostics` / `ConnectionTestPhase`.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    pub phase: String,
    pub binary_path: Option<String>,
    pub stderr_tail: Option<String>,
    pub stdout_tail: Option<String>,
    /// `Some(None)` writes an explicit `null` (the contract distinguishes
    /// "exit unknown" from "no key at all").
    pub exit_code: Option<Option<i64>>,
    pub signal: Option<Option<String>>,
    /// Extra override fields applied in TS's spread order.
    pub overrides: Vec<(String, Value)>,
}

impl Diagnostics {
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("phase".to_string(), Value::String(self.phase.clone()));
        if let Some(path) = &self.binary_path {
            map.insert("binaryPath".to_string(), Value::String(path.clone()));
        }
        if let Some(tail) = &self.stderr_tail {
            map.insert("stderrTail".to_string(), Value::String(tail.clone()));
        }
        if let Some(tail) = &self.stdout_tail {
            map.insert("stdoutTail".to_string(), Value::String(tail.clone()));
        }
        if let Some(code) = &self.exit_code {
            map.insert(
                "exitCode".to_string(),
                code.map(Value::from).unwrap_or(Value::Null),
            );
        }
        if let Some(signal) = &self.signal {
            map.insert(
                "signal".to_string(),
                signal
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
        }
        for (key, value) in &self.overrides {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }
}

/// A response builder that preserves the TS key order.
#[derive(Debug, Clone)]
pub struct ResponseBuilder {
    map: Map<String, Value>,
}

impl ResponseBuilder {
    fn new(ok: bool, kind: ConnectionTestKind, latency_ms: i64) -> Self {
        let mut map = Map::new();
        map.insert("ok".to_string(), Value::Bool(ok));
        map.insert("kind".to_string(), Value::String(kind.as_str().to_string()));
        map.insert("latencyMs".to_string(), Value::from(latency_ms));
        Self { map }
    }

    fn set(mut self, key: &str, value: Option<Value>) -> Self {
        if let Some(value) = value {
            self.map.insert(key.to_string(), value);
        }
        self
    }

    pub fn model(self, model: &str) -> Self {
        self.set("model", Some(Value::String(model.to_string())))
    }
    pub fn resolved_model(self, model: Option<&str>) -> Self {
        self.set("resolvedModel", model.map(|m| Value::String(m.to_string())))
    }
    pub fn sample(self, sample: Option<&str>) -> Self {
        self.set("sample", sample.map(|s| Value::String(s.to_string())))
    }
    pub fn agent_name(self, name: &str) -> Self {
        self.set("agentName", Some(Value::String(name.to_string())))
    }
    pub fn detail(self, detail: Option<String>) -> Self {
        self.set("detail", detail.map(Value::String))
    }
    pub fn configured_executable_path(self, path: Option<&str>) -> Self {
        self.set(
            "configuredExecutablePath",
            path.map(|p| Value::String(p.to_string())),
        )
    }
    pub fn detected_executable_path(self, path: Option<&str>) -> Self {
        self.set(
            "detectedExecutablePath",
            path.map(|p| Value::String(p.to_string())),
        )
    }
    pub fn used_executable_path(self, path: Option<&str>) -> Self {
        self.set(
            "usedExecutablePath",
            path.map(|p| Value::String(p.to_string())),
        )
    }
    pub fn used_executable_source(self, source: Option<&str>) -> Self {
        self.set(
            "usedExecutableSource",
            source.map(|s| Value::String(s.to_string())),
        )
    }
    pub fn diagnostics(self, diagnostics: Diagnostics) -> Self {
        self.set("diagnostics", Some(diagnostics.to_json()))
    }
    pub fn build(self) -> Value {
        Value::Object(self.map)
    }
}

// ---- the decision tree ------------------------------------------------------

/// Immutable inputs the decision tree needs from the outer scope.
struct SmokeContext<'a> {
    agent_id: &'a str,
    agent_name: &'a str,
    model: &'a str,
    resolution: &'a AgentResolution,
    start_ms: i64,
}

impl SmokeContext<'_> {
    fn latency(&self) -> i64 {
        now_ms() - self.start_ms
    }

    /// Parity: `buildDiagnostics(overrides)` with `phase` fixed at `spawn`
    /// (the tracker is flipped to `spawn` before the child is created).
    fn diagnostics(&self, overrides: Diagnostics) -> Diagnostics {
        Diagnostics {
            phase: if overrides.phase.is_empty() {
                "spawn".to_string()
            } else {
                overrides.phase.clone()
            },
            binary_path: self.resolution.launch_path.clone(),
            stderr_tail: overrides.stderr_tail,
            stdout_tail: overrides.stdout_tail,
            exit_code: overrides.exit_code,
            signal: overrides.signal,
            overrides: overrides.overrides,
        }
    }
}

fn empty() -> Diagnostics {
    Diagnostics::default()
}

fn with_phase(phase: &str) -> Diagnostics {
    Diagnostics {
        phase: phase.to_string(),
        ..Diagnostics::default()
    }
}

fn with_phase_exit(phase: &str, exit: &ChildExit) -> Diagnostics {
    Diagnostics {
        phase: phase.to_string(),
        exit_code: Some(exit.code),
        signal: Some(exit.signal.clone()),
        ..Diagnostics::default()
    }
}

/// Parity: the whole `resultFrom*` family. Pure over the probe outcome.
pub fn resolve_agent_connection(
    agent_id: &str,
    agent_name: &str,
    model: &str,
    resolution: &AgentResolution,
    start_ms: i64,
    outcome: &SmokeOutcome,
) -> Value {
    let ctx = SmokeContext {
        agent_id,
        agent_name,
        model,
        resolution,
        start_ms,
    };
    let winner = if matches!(outcome.winner, Winner::Text) {
        outcome.completion.clone().unwrap_or(Winner::Text)
    } else {
        outcome.winner.clone()
    };
    match winner {
        Winner::Text => {
            // Text without a completion is unreachable in the real race; treat
            // it as the theoretical "text without exit" path.
            result_from_agent_text(&ctx, &outcome.text, outcome.resolved_model.as_deref(), None)
        }
        Winner::StreamError(message) => result_from_stream_error(&ctx, &message),
        Winner::ProviderConnectivity(detail) => {
            result_from_provider_connectivity(&ctx, &detail, None)
        }
        Winner::Timeout | Winner::Aborted => {
            let timed_out = matches!(winner, Winner::Timeout);
            result_from_cancellation(&ctx, outcome, timed_out)
        }
        Winner::Exit(result) => result_from_child_exit(&ctx, outcome, &result),    }
}

/// Parity: `resultFromStreamError`.
fn result_from_stream_error(ctx: &SmokeContext<'_>, error: &str) -> Value {
    let latency = ctx.latency();
    let detail = redact_secrets(error, &[]);
    if let Some(auth) = classify_agent_auth_failure(ctx.agent_id, &detail) {
        if auth.status == AuthProbeStatus::Missing {
            return ResponseBuilder::new(false, ConnectionTestKind::AgentAuthRequired, latency)
                .model(ctx.model)
                .agent_name(ctx.agent_name)
                .detail(Some(
                    auth.message
                        .clone()
                        .unwrap_or_else(|| cursor_auth_guidance().to_string()),
                ))
                .diagnostics(ctx.diagnostics(empty()))
                .build();
        }
    }
    if !detail.is_empty() && is_likely_model_error_text(&detail) {
        return ResponseBuilder::new(false, ConnectionTestKind::NotFoundModel, latency)
            .model(ctx.model)
            .agent_name(ctx.agent_name)
            .detail(Some(detail))
            .diagnostics(ctx.diagnostics(with_phase("output_parse")))
            .build();
    }
    ResponseBuilder::new(false, ConnectionTestKind::AgentSpawnFailed, latency)
        .model(ctx.model)
        .agent_name(ctx.agent_name)
        .detail(Some(detail))
        .diagnostics(ctx.diagnostics(empty()))
        .build()
}

/// Parity: `resultFromProviderConnectivity`.
fn result_from_provider_connectivity(
    ctx: &SmokeContext<'_>,
    provider_detail: &str,
    exit: Option<&ChildExit>,
) -> Value {
    let latency = ctx.latency();
    let detail = redact_secrets(provider_detail, &[]);
    let diagnostics = match exit {
        Some(exit) => ctx.diagnostics(with_phase_exit("connection_smoke_test", exit)),
        None => ctx.diagnostics(with_phase("connection_smoke_test")),
    };
    ResponseBuilder::new(false, ConnectionTestKind::UpstreamUnavailable, latency)
        .model(ctx.model)
        .agent_name(ctx.agent_name)
        .detail(Some(detail))
        .diagnostics(diagnostics)
        .build()
}

/// Parity: `resultFromCancellation`.
fn result_from_cancellation(
    ctx: &SmokeContext<'_>,
    outcome: &SmokeOutcome,
    timed_out: bool,
) -> Value {
    if timed_out && ctx.agent_id == "opencode" {
        let raw_detail = format!(
            "{}\n{}",
            outcome.stderr_tail.trim(),
            outcome.raw_stdout_tail.trim()
        );
        if let Some(detail) = opencode_provider_connectivity_detail(&raw_detail) {
            return result_from_provider_connectivity(ctx, &detail, None);
        }
    }
    let latency = ctx.latency();
    ResponseBuilder::new(false, ConnectionTestKind::Timeout, latency)
        .model(ctx.model)
        .agent_name(ctx.agent_name)
        .diagnostics(ctx.diagnostics(empty()))
        .build()
}

/// Parity: `resultFromAgentText`.
fn result_from_agent_text(
    ctx: &SmokeContext<'_>,
    text: &str,
    resolved_model: Option<&str>,
    exit: Option<&ChildExit>,
) -> Value {
    let latency = ctx.latency();
    let raw_sample = truncate_sample(text);
    let sample = redact_secrets(&raw_sample, &[]);
    let exit_info = exit.cloned().unwrap_or(ChildExit {
        code: Some(0),
        signal: None,
    });
    let diagnostics = ctx.diagnostics(with_phase_exit("connection_smoke_test", &exit_info));
    // Parity: the model-error check reads the *unredacted* sample, and the
    // failure detail is the smoke-failure sentence, not the raw sample.
    if !raw_sample.is_empty() && is_likely_model_error_text(&raw_sample) {
        let detail = redact_secrets(&smoke_failure_detail(&raw_sample), &[]);
        return ResponseBuilder::new(false, ConnectionTestKind::NotFoundModel, latency)
            .model(ctx.model)
            .agent_name(ctx.agent_name)
            .detail(Some(detail))
            .diagnostics(ctx.diagnostics(with_phase_exit("output_parse", &exit_info)))
            .build();
    }
    ResponseBuilder::new(true, ConnectionTestKind::Success, latency)
        .model(ctx.model)
        .resolved_model(resolved_model)
        .agent_name(ctx.agent_name)
        .sample(Some(&sample))
        .diagnostics(diagnostics)
        .build()
}

/// Parity: `resultFromChildExit` (connectionTest.ts:2860+).
fn result_from_child_exit(
    ctx: &SmokeContext<'_>,
    outcome: &SmokeOutcome,
    exit: &Result<ChildExit, SpawnFailure>,
) -> Value {
    let latency = ctx.latency();
    let exit = match exit {
        Ok(exit) => exit.clone(),
        Err(failure) => {
            let guidance = codex_executable_guidance(
                ctx.agent_id,
                ctx.resolution.configured_override_path.as_deref(),
                ctx.resolution.path_resolved_path.as_deref(),
            );
            let diagnostic_suffix = ctx
                .resolution
                .diagnostic
                .as_deref()
                .map(|diagnostic| format!(" {diagnostic}"))
                .unwrap_or_default();
            let detail = redact_secrets(
                &format!("{}{guidance}{diagnostic_suffix}", failure.message),
                &[],
            );
            let is_missing = failure.code.as_deref() == Some("ENOENT");
            let kind = if is_missing {
                ConnectionTestKind::AgentNotInstalled
            } else {
                ConnectionTestKind::AgentSpawnFailed
            };
            let phase = if is_missing {
                "binary_resolution"
            } else {
                "spawn"
            };
            return ResponseBuilder::new(false, kind, latency)
                .model(ctx.model)
                .agent_name(ctx.agent_name)
                .detail(Some(detail))
                .diagnostics(ctx.diagnostics(with_phase(phase)))
                .build();
        }
    };

    // On Windows, short-lived JSON-stream CLIs can deliver close before all
    // stdout has reached the parser. Parity: the drain delay is applied by the
    // probe before this runs.
    if ctx.agent_id == "opencode" {
        let provider_detail = opencode_provider_connectivity_detail(&format!(
            "{}\n{}",
            outcome.stderr_tail.trim(),
            outcome.raw_stdout_tail.trim()
        ));
        if let Some(detail) = provider_detail {
            return result_from_provider_connectivity(ctx, &detail, Some(&exit));
        }
    }
    let buffered = outcome.text.trim().to_string();
    let claude_result = if ctx.agent_id == "claude" {
        parse_claude_result_frame(&outcome.raw_stdout)
    } else {
        None
    };
    let claude_reported_success = claude_result
        .as_ref()
        .map(|frame| !frame.is_error && frame.subtype.as_deref() == Some("success"))
        .unwrap_or(false);
    let claude_late_exit_one =
        ctx.agent_id == "claude" && exit.code == Some(1) && exit.signal.is_none();
    let parsed_claude_result_text = if claude_reported_success {
        claude_result
            .as_ref()
            .map(|frame| frame.result_text.trim().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let visible_text = if buffered.is_empty() {
        parsed_claude_result_text.clone()
    } else {
        buffered.clone()
    };
    let acp_clean_completion = outcome.acp_clean_completion;
    let acp_forced_shutdown = acp_clean_completion
        && ((exit.code.is_none() && exit.signal.as_deref() == Some("SIGTERM"))
            || (exit.code == Some(130) && exit.signal.is_none()));
    let claude_completed_turn = claude_late_exit_one
        && claude_reported_success
        && (outcome.saw_terminal_completion
            || claude_result
                .as_ref()
                .and_then(|frame| frame.stop_reason.as_deref())
                .map(|stop_reason| is_terminal_completion_stop_reason(&Value::String(stop_reason.to_string())))
                .unwrap_or(false));
    let exited_cleanly =
        (exit.code == Some(0) && exit.signal.is_none()) || acp_forced_shutdown || claude_completed_turn;
    if !visible_text.is_empty() {
        let raw_sample = truncate_sample(&visible_text);
        if !raw_sample.is_empty() && is_likely_model_error_text(&raw_sample) {
            return result_from_agent_text(
                ctx,
                &visible_text,
                outcome.resolved_model.as_deref(),
                Some(&exit),
            );
        }
        if exited_cleanly {
            return result_from_agent_text(
                ctx,
                &visible_text,
                outcome.resolved_model.as_deref(),
                Some(&exit),
            );
        }
    }
    let stderr_tail = outcome.stderr_tail.trim().to_string();
    let raw_stdout_tail = outcome.raw_stdout_tail.trim().to_string();
    if (ctx.agent_id == "opencode" || ctx.agent_id == "mimo")
        && exited_cleanly
        && !raw_stdout_tail.is_empty()
    {
        let recovered = extract_opencode_text_from_raw_stdout(&raw_stdout_tail)
            .trim()
            .to_string();
        if !recovered.is_empty() {
            return result_from_agent_text(
                ctx,
                &recovered,
                outcome.resolved_model.as_deref(),
                Some(&exit),
            );
        }
    }
    // Antigravity print-mode empty-output guard (#4281).
    if ctx.agent_id == "antigravity" && visible_text.is_empty() {
        let mut combined = format!("{stderr_tail}\n{raw_stdout_tail}");
        if let Some(log_tail) = &outcome.antigravity_log_tail {
            combined = format!("{combined}\n{log_tail}");
        }
        let antigravity_auth = classify_agent_auth_failure("antigravity", &combined);
        let service_failure = classify_agent_service_failure(&combined);
        let is_auth = antigravity_auth
            .as_ref()
            .map(|auth| auth.status == AuthProbeStatus::Missing)
            .unwrap_or(false)
            || service_failure == Some(AgentServiceFailureCode::AgentAuthRequired)
            || (antigravity_auth.is_none() && service_failure.is_none() && exited_cleanly);
        if is_auth {
            return ResponseBuilder::new(
                false,
                ConnectionTestKind::AgentAuthRequired,
                latency,
            )
            .model(ctx.model)
            .agent_name(ctx.agent_name)
            .detail(Some(
                antigravity_auth
                    .and_then(|auth| auth.message)
                    .unwrap_or_else(|| antigravity_auth_guidance().to_string()),
            ))
            .diagnostics(ctx.diagnostics(with_phase_exit("connection_smoke_test", &exit)))
            .build();
        }
        if service_failure == Some(AgentServiceFailureCode::RateLimited) {
            return ResponseBuilder::new(false, ConnectionTestKind::RateLimited, latency)
                .model(ctx.model)
                .agent_name(ctx.agent_name)
                .detail(Some(antigravity_quota_guidance().to_string()))
                .diagnostics(ctx.diagnostics(with_phase_exit("connection_smoke_test", &exit)))
                .build();
        }
    }
    let acp_fatal = outcome.acp_fatal;
    let mut parts: Vec<String> = Vec::new();
    if let Some(code) = exit.code {
        parts.push(format!("exit {code}"));
    }
    if let Some(signal) = &exit.signal {
        parts.push(format!("signal {signal}"));
    }
    if !stderr_tail.is_empty() {
        parts.push(format!("stderr: {}", tail_chars(&stderr_tail, 200)));
    }
    let stdout_detail_source = if raw_stdout_tail.is_empty() {
        buffered.clone()
    } else {
        raw_stdout_tail.clone()
    };
    if !stdout_detail_source.is_empty() {
        parts.push(format!("stdout: {}", tail_chars(&stdout_detail_source, 200)));
    }
    let raw_detail = parts.join(" · ");
    if ctx.agent_id == "opencode" {
        if let Some(detail) = opencode_outdated_cli_detail(&raw_detail) {
            return ResponseBuilder::new(false, ConnectionTestKind::AgentSpawnFailed, latency)
                .model(ctx.model)
                .agent_name(ctx.agent_name)
                .detail(Some(detail))
                .diagnostics(ctx.diagnostics(with_phase_exit("spawn", &exit)))
                .build();
        }
    }
    if let Some(auth) = classify_agent_auth_failure(ctx.agent_id, &raw_detail) {
        if auth.status == AuthProbeStatus::Missing {
            return ResponseBuilder::new(false, ConnectionTestKind::AgentAuthRequired, latency)
                .model(ctx.model)
                .agent_name(ctx.agent_name)
                .detail(Some(
                    auth.message
                        .unwrap_or_else(|| cursor_auth_guidance().to_string()),
                ))
                .diagnostics(ctx.diagnostics(with_phase_exit("connection_smoke_test", &exit)))
                .build();
        }
    }
    let claude_diagnostic = diagnose_claude_cli_failure(&ClaudeCliDiagnosticInput {
        agent_id: ctx.agent_id.to_string(),
        exit_code: exit.code,
        signal: exit.signal.clone(),
        stderr_tail: Some(stderr_tail.clone()),
        stdout_tail: Some(if outcome.raw_stdout.is_empty() {
            if raw_stdout_tail.is_empty() {
                visible_text.clone()
            } else {
                raw_stdout_tail.clone()
            }
        } else {
            outcome.raw_stdout.clone()
        }),
        env: ctx.resolution.env.clone(),
        resolved_bin: ctx.resolution.selected_path.clone(),
    });
    if let Some(diagnostic) = claude_diagnostic {
        return ResponseBuilder::new(false, ConnectionTestKind::AgentSpawnFailed, latency)
            .model(ctx.model)
            .agent_name(ctx.agent_name)
            .detail(Some(diagnostic.detail))
            .diagnostics(ctx.diagnostics(with_phase_exit("spawn", &exit)))
            .build();
    }
    let detail = redact_secrets(&raw_detail, &[]);
    let guidance = redact_secrets(
        &format!(
            "{}{}",
            codex_executable_guidance(
                ctx.agent_id,
                ctx.resolution.configured_override_path.as_deref(),
                ctx.resolution.path_resolved_path.as_deref(),
            ),
            ctx.resolution
                .diagnostic
                .as_deref()
                .map(|diagnostic| format!(" {diagnostic}"))
                .unwrap_or_default()
        ),
        &[],
    );
    let kind = if acp_fatal || !exited_cleanly {
        ConnectionTestKind::AgentSpawnFailed
    } else {
        ConnectionTestKind::Unknown
    };
    let detail_text = if detail.is_empty() {
        format!("Agent exited without producing assistant text{guidance}")
    } else {
        format!("{detail}{guidance}")
    };
    let phase = if buffered.is_empty() {
        "spawn"
    } else {
        "output_parse"
    };
    ResponseBuilder::new(false, kind, latency)
        .model(ctx.model)
        .agent_name(ctx.agent_name)
        .detail(Some(detail_text))
        .diagnostics(ctx.diagnostics(with_phase_exit(phase, &exit)))
        .build()
}

fn tail_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

// ---- orchestration ----------------------------------------------------------

/// Parity: `testAgentConnectionInternal` (minus the spawn, which the probe owns).
pub fn test_agent_connection_internal<'a>(
    probe: &'a dyn AgentProbe,
    request: &'a AgentConnectionRequest,
    configured_env: &'a HashMap<String, String>,
) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let start_ms = now_ms();
        let model = request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .unwrap_or("default")
            .to_string();
        let resolution = match probe.resolve(&request.agent_id, configured_env).await {
            ResolutionOutcome::UnknownAgent => {
                return ResponseBuilder::new(
                    false,
                    ConnectionTestKind::AgentNotInstalled,
                    now_ms() - start_ms,
                )
                .model(&model)
                .agent_name(&request.agent_id)
                .detail(Some(format!("Unknown agent id: {}", request.agent_id)))
                .diagnostics(with_phase("binary_resolution"))
                .build();
            }
            ResolutionOutcome::NotInstalled => {
                let name = crate::agents::get_agent_def(&request.agent_id)
                    .map(|def| def.name.to_string())
                    .unwrap_or_else(|| request.agent_id.clone());
                return ResponseBuilder::new(
                    false,
                    ConnectionTestKind::AgentNotInstalled,
                    now_ms() - start_ms,
                )
                .model(&model)
                .agent_name(&name)
                .diagnostics(with_phase("binary_resolution"))
                .build();
            }
            ResolutionOutcome::Ready(resolution) => resolution,
        };
        match probe.run(request, &resolution).await {
            ProbeOutcome::BuildArgsFailed(detail) => {
                ResponseBuilder::new(
                    false,
                    ConnectionTestKind::AgentSpawnFailed,
                    now_ms() - start_ms,
                )
                .model(&model)
                .agent_name(&resolution.agent_name)
                .detail(Some(redact_secrets(&detail, &[])))
                .diagnostics(with_phase("binary_resolution"))
                .build()
            }
            ProbeOutcome::AuthRequired(auth) => {
                // The pre-spawn probe runs before the smoke child, so phase is
                // still `binary_resolution` and the sink is empty — the probe's
                // own output is the only source of stderr/stdout/exit context.
                let mut overrides: Vec<(String, Value)> = Vec::new();
                if let Some(tail) = &auth.stdout_tail {
                    overrides.push((
                        "stdoutTail".to_string(),
                        Value::String(redact_secrets(tail, &[])),
                    ));
                }
                if let Some(tail) = &auth.stderr_tail {
                    overrides.push((
                        "stderrTail".to_string(),
                        Value::String(redact_secrets(tail, &[])),
                    ));
                }
                if let Some(exit_code) = auth.exit_code {
                    overrides.push(("exitCode".to_string(), Value::from(exit_code)));
                }
                if let Some(signal) = &auth.signal {
                    overrides.push(("signal".to_string(), Value::String(signal.clone())));
                }
                let mut diagnostics =
                    with_phase("binary_resolution");
                diagnostics.binary_path = resolution.launch_path.clone();
                if let Some(exit_code) = auth.exit_code {
                    diagnostics.exit_code = Some(Some(i64::from(exit_code)));
                }
                if auth.signal.is_some() {
                    diagnostics.signal = Some(auth.signal.clone());
                }
                if let Some(tail) = &auth.stdout_tail {
                    diagnostics.stdout_tail = Some(redact_secrets(tail, &[]));
                }
                if let Some(tail) = &auth.stderr_tail {
                    diagnostics.stderr_tail = Some(redact_secrets(tail, &[]));
                }
                let _ = overrides;
                ResponseBuilder::new(
                    false,
                    ConnectionTestKind::AgentAuthRequired,
                    now_ms() - start_ms,
                )
                .model(&model)
                .agent_name(&resolution.agent_name)
                .detail(Some(
                    auth.message
                        .clone()
                        .unwrap_or_else(|| cursor_auth_guidance().to_string()),
                ))
                .diagnostics(diagnostics)
                .build()
            }
            ProbeOutcome::Smoke(outcome) => resolve_agent_connection(
                &request.agent_id,
                &resolution.agent_name,
                &model,
                &resolution,
                start_ms,
                &outcome,
            ),
        }
    })
}

/// Parity: `testAgentConnection` — the codex CODEX_BIN fallback wrapper.
pub fn test_agent_connection<'a>(
    probe: &'a dyn AgentProbe,
    request: &'a AgentConnectionRequest,
) -> BoxFuture<'a, Value> {
    Box::pin(async move {
        let validated = validate_agent_cli_env(request.agent_cli_env.as_ref());
        let configured_codex_bin = validated
            .as_ref()
            .and_then(|prefs| prefs.get("codex"))
            .and_then(Value::as_object)
            .and_then(|codex| codex.get("CODEX_BIN"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        let configured_env = agent_cli_env_for_agent(validated.as_ref(), &request.agent_id);
        let primary = test_agent_connection_internal(probe, request, &configured_env).await;
        let resolution = match probe.resolve(&request.agent_id, &configured_env).await {
            ResolutionOutcome::Ready(resolution) => resolution,
            _ => AgentResolution::default(),
        };
        let configured_override_path = resolution.configured_override_path.clone();
        let path_resolved_path = resolution.path_resolved_path.clone();
        if request.agent_id == "codex"
            && primary
                .get("ok")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && !configured_codex_bin.is_empty()
        {
            if let Some(configured) = &configured_override_path {
                let mut builder = ResponseBuilder::from_value(primary)
                    .configured_executable_path(Some(configured))
                    .used_executable_path(
                        resolution
                            .launch_path
                            .as_deref()
                            .or(Some(configured.as_str())),
                    )
                    .used_executable_source(Some("configured"))
                    .detail(Some(redact_secrets(
                        &codex_configured_path_success_detail(configured),
                        &[],
                    )));
                if let Some(detected) = &path_resolved_path {
                    builder = builder.detected_executable_path(Some(detected));
                }
                return builder.build();
            }
            if let Some(detected) = &path_resolved_path {
                return ResponseBuilder::from_value(primary)
                    .configured_executable_path(Some(&configured_codex_bin))
                    .detected_executable_path(Some(detected))
                    .used_executable_path(
                        resolution.launch_path.as_deref().or(Some(detected.as_str())),
                    )
                    .used_executable_source(Some("fallback_invalid"))
                    .detail(Some(redact_secrets(
                        &codex_invalid_configured_path_fallback_detail(
                            &configured_codex_bin,
                            detected,
                        ),
                        &[],
                    )))
                    .build();
            }
        }
        let primary_kind = primary
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let fallback_eligible_kinds = [
            "agent_spawn_failed",
            "agent_not_installed",
            "unknown",
        ];
        if request.agent_id != "codex"
            || primary
                .get("ok")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || !fallback_eligible_kinds.contains(&primary_kind.as_str())
            || configured_override_path.is_none()
            || path_resolved_path.is_none()
            || configured_override_path == path_resolved_path
        {
            return primary;
        }
        let fallback_request = AgentConnectionRequest {
            agent_id: request.agent_id.clone(),
            model: request.model.clone(),
            reasoning: request.reasoning.clone(),
            service_tier: request.service_tier.clone(),
            agent_cli_env: strip_codex_bin_override(validated).map(Value::Object),
        };
        let fallback_configured_env =
            agent_cli_env_for_agent(fallback_request.agent_cli_env.as_ref().and_then(Value::as_object), &request.agent_id);
        let fallback =
            test_agent_connection_internal(probe, &fallback_request, &fallback_configured_env).await;
        if !fallback
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return primary;
        }
        let configured = configured_override_path.unwrap_or_default();
        let detected = path_resolved_path.unwrap_or_default();
        ResponseBuilder::from_value(fallback)
            .configured_executable_path(Some(&configured))
            .detected_executable_path(Some(&detected))
            .used_executable_path(resolution.launch_path.as_deref().or(Some(detected.as_str())))
            .used_executable_source(Some("fallback_failed"))
            .detail(Some(redact_secrets(
                &codex_executable_fallback_success_detail(&configured, &detected),
                &[],
            )))
            .build()
    })
}

impl ResponseBuilder {
    /// Continue building from an existing response (parity: `{...primaryResult}`).
    fn from_value(value: Value) -> Self {
        let mut map = match value {
            Value::Object(map) => map,
            other => {
                let mut map = Map::new();
                map.insert("value".to_string(), other);
                map
            }
        };
        // Re-establish the canonical order: ok, kind, latencyMs, model, …
        let order = [
            "ok",
            "kind",
            "latencyMs",
            "model",
            "resolvedModel",
            "sample",
            "status",
            "agentName",
            "detail",
            "configuredExecutablePath",
            "detectedExecutablePath",
            "usedExecutablePath",
            "usedExecutableSource",
            "diagnostics",
        ];
        let mut ordered = Map::new();
        for key in order {
            if let Some(item) = map.remove(key) {
                ordered.insert(key.to_string(), item);
            }
        }
        for (key, item) in map {
            ordered.insert(key, item);
        }
        Self { map: ordered }
    }
}

// ---- production probe -------------------------------------------------------

/// The production [`AgentProbe`]: resolves the real executable and runs the
/// CLI. Tests use a fixture probe instead so they never burn provider budget.
pub struct SpawnAgentProbe {
    pub data_dir: PathBuf,
    pub base_env: HashMap<String, String>,
    pub timeout_ms: u64,
}

impl SpawnAgentProbe {
    pub fn new(data_dir: PathBuf, base_env: HashMap<String, String>) -> Self {
        let timeout_ms = agent_timeout_ms(&base_env);
        Self {
            data_dir,
            base_env,
            timeout_ms,
        }
    }

    /// Parity: the `{...process.env}` base every spawned agent inherits.
    pub fn from_process_env(data_dir: PathBuf) -> Self {
        Self::new(data_dir, std::env::vars().collect())
    }
}

/// One stdout line fed to the format-appropriate parser, accumulating the same
/// `sink` state the TS connection test collects. Parity:
/// `attachAgentStreamHandlers` + the `send` sink in `connectionTest.ts`.
struct SmokeLane {
    kind: SmokeLaneKind,
    browser: SmokeSink,
}

enum SmokeLaneKind {
    Claude(Box<crate::protocol::ClaudeParser>),
    Copilot(Box<crate::protocol::CopilotParser>),
    Qoder(Box<crate::protocol::QoderParser>),
    Json(Box<crate::protocol::JsonEventParser>),
    Plain,
}

#[derive(Default)]
struct SmokeSink {
    text: String,
    raw_stdout: String,
    raw_stdout_tail: String,
    stderr_tail: String,
    resolved_model: Option<String>,
    saw_terminal_completion: bool,
    stream_error: Option<String>,
}

impl SmokeSink {
    fn consume_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.text.push_str(text);
        }
    }

    fn append_raw_stdout(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.raw_stdout.push_str(chunk);
        if self.raw_stdout.len() > 16_384 {
            self.raw_stdout = tail_bytes(&self.raw_stdout, 16_384);
        }
        self.raw_stdout_tail.push_str(chunk);
        if self.raw_stdout_tail.len() > 400 {
            self.raw_stdout_tail = tail_bytes(&self.raw_stdout_tail, 400);
        }
    }

    fn append_stderr(&mut self, chunk: &str) {
        self.stderr_tail.push_str(chunk);
        if self.stderr_tail.len() > 400 {
            self.stderr_tail = tail_bytes(&self.stderr_tail, 400);
        }
    }

    /// Parity: the `send` sink's event handling.
    fn send_event(&mut self, event: &crate::protocol::StreamEvent) {
        use crate::protocol::StreamEvent;
        match event {
            StreamEvent::Error { message, .. } => {
                if self.stream_error.is_none() {
                    self.stream_error = Some(message.clone());
                }
            }
            StreamEvent::TextDelta { delta } => self.consume_text(delta),
            StreamEvent::Status { label, model, .. } => {
                if label == "initializing" {
                    if let Some(Value::String(model)) = model {
                        if !model.trim().is_empty() {
                            self.resolved_model = Some(model.trim().to_string());
                        }
                    }
                }
            }
            StreamEvent::TurnEnd { stop_reason } => {
                if is_terminal_completion_stop_reason(&Value::String(stop_reason.clone())) {
                    self.saw_terminal_completion = true;
                }
            }
            StreamEvent::Usage { stop_reason, .. } => {
                if is_terminal_completion_stop_reason(&Value::String(stop_reason.clone())) {
                    self.saw_terminal_completion = true;
                }
            }
            StreamEvent::Json(value) => {
                let Some(obj) = value.as_object() else { return };
                let event_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
                if event_type == "error" {
                    let message = obj
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("agent stream error");
                    if self.stream_error.is_none() {
                        self.stream_error = Some(message.to_string());
                    }
                    return;
                }
                if event_type == "status" {
                    let is_init = obj.get("label").and_then(Value::as_str) == Some("initializing");
                    if is_init {
                        if let Some(Value::String(model)) = obj.get("model") {
                            if !model.trim().is_empty() {
                                self.resolved_model = Some(model.trim().to_string());
                            }
                        }
                    }
                }
                if event_type == "text" {
                    if let Some(Value::String(text)) = obj.get("text") {
                        self.consume_text(text);
                    }
                }
                if event_type == "text_delta" {
                    if let Some(Value::String(delta)) = obj.get("delta") {
                        self.consume_text(delta);
                    }
                }
                if event_type == "turn_end" || event_type == "usage" {
                    if let Some(stop_reason) = obj.get("stopReason") {
                        if is_terminal_completion_stop_reason(stop_reason) {
                            self.saw_terminal_completion = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn tail_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

impl SmokeLane {
    fn new(stream_format: &str, json_kind: Option<&'static str>) -> Self {
        let kind = match stream_format {
            "claude-stream-json" => {
                SmokeLaneKind::Claude(Box::new(crate::protocol::ClaudeParser::new()))
            }
            "copilot-stream-json" => {
                SmokeLaneKind::Copilot(Box::new(crate::protocol::CopilotParser::new()))
            }
            "qoder-stream-json" => {
                SmokeLaneKind::Qoder(Box::new(crate::protocol::QoderParser::new()))
            }
            "json-event-stream" => match json_kind
                .filter(|kind| crate::protocol::json_event_kind_supported(kind))
            {
                Some(kind) => {
                    SmokeLaneKind::Json(Box::new(crate::protocol::JsonEventParser::new(kind)))
                }
                None => SmokeLaneKind::Plain,
            },
            // ACP / dsh-profile / pi-rpc session layers are not ported; the
            // plain lane still recovers their stdout text for the smoke test.
            _ => SmokeLaneKind::Plain,
        };
        Self {
            kind,
            browser: SmokeSink::default(),
        }
    }

    /// Feed one stdout line (no trailing newline) and collect sink state.
    fn feed_line(&mut self, line: &str) {
        if matches!(&self.kind, SmokeLaneKind::Plain) {
            let chunk = format!("{line}\n");
            self.browser.append_raw_stdout(&chunk);
            self.browser.consume_text(&chunk);
            return;
        }
        let raw_lane = matches!(
            &self.kind,
            SmokeLaneKind::Claude(_) | SmokeLaneKind::Json(_)
        );
        let mut events: Vec<crate::protocol::StreamEvent> = Vec::new();
        match &mut self.kind {
            SmokeLaneKind::Claude(parser) => {
                parser.feed(&format!("{line}\n"), &mut |event| events.push(event));
            }
            SmokeLaneKind::Copilot(parser) => {
                parser.feed(&format!("{line}\n"), &mut |event| events.push(event));
            }
            SmokeLaneKind::Qoder(parser) => {
                parser.feed(&format!("{line}\n"), &mut |event| events.push(event));
            }
            SmokeLaneKind::Json(parser) => {
                parser.feed(&format!("{line}\n"), &mut |event| events.push(event));
            }
            SmokeLaneKind::Plain => unreachable!("plain lane handled above"),
        }
        if raw_lane {
            self.browser.append_raw_stdout(&format!("{line}\n"));
        }
        for event in &events {
            self.browser.send_event(event);
        }
    }

    fn flush(&mut self) {
        let mut events: Vec<crate::protocol::StreamEvent> = Vec::new();
        match &mut self.kind {
            SmokeLaneKind::Claude(parser) => parser.flush(&mut |event| events.push(event)),
            SmokeLaneKind::Copilot(parser) => parser.flush(&mut |event| events.push(event)),
            SmokeLaneKind::Qoder(parser) => parser.flush(&mut |event| events.push(event)),
            SmokeLaneKind::Json(parser) => parser.flush(&mut |event| events.push(event)),
            SmokeLaneKind::Plain => {}
        }
        for event in &events {
            self.browser.send_event(event);
        }
    }
}

fn signal_name(status: &std::process::ExitStatus) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let signal = status.signal()?;
        let name = match signal {
            1 => "SIGHUP",
            2 => "SIGINT",
            3 => "SIGQUIT",
            6 => "SIGABRT",
            9 => "SIGKILL",
            11 => "SIGSEGV",
            13 => "SIGPIPE",
            15 => "SIGTERM",
            _ => return Some(format!("SIG{signal}")),
        };
        Some(name.to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

fn exit_to_child_exit(status: &std::process::ExitStatus) -> ChildExit {
    ChildExit {
        code: status.code().map(i64::from),
        signal: signal_name(status),
    }
}

impl AgentProbe for SpawnAgentProbe {
    fn resolve<'a>(
        &'a self,
        agent_id: &'a str,
        configured_env: &'a HashMap<String, String>,
    ) -> BoxFuture<'a, ResolutionOutcome> {
        Box::pin(async move {
            let Some(def) = crate::agents::get_agent_def(agent_id) else {
                return ResolutionOutcome::UnknownAgent;
            };
            if def.bin.is_empty() {
                return ResolutionOutcome::NotInstalled;
            }
            let launch = crate::launch::resolve_agent_launch(def, configured_env);
            let (Some(selected), Some(launch_path)) =
                (launch.selected_path.as_ref(), launch.launch_path.as_ref())
            else {
                return ResolutionOutcome::NotInstalled;
            };
            ResolutionOutcome::Ready(AgentResolution {
                agent_name: def.name.to_string(),
                selected_path: Some(selected.to_string_lossy().into_owned()),
                launch_path: Some(launch_path.to_string_lossy().into_owned()),
                path_resolved_path: launch
                    .path_resolved_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                configured_override_path: launch
                    .configured_override_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                diagnostic: launch.diagnostic.clone(),
                env: HashMap::new(),
            })
        })
    }

    fn run<'a>(
        &'a self,
        request: &'a AgentConnectionRequest,
        resolution: &'a AgentResolution,
    ) -> BoxFuture<'a, ProbeOutcome> {
        Box::pin(async move { self.run_smoke(request, resolution).await })
    }
}

impl SpawnAgentProbe {
    async fn run_smoke(
        &self,
        request: &AgentConnectionRequest,
        resolution: &AgentResolution,
    ) -> ProbeOutcome {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let Some(def) = crate::agents::get_agent_def(&request.agent_id) else {
            return ProbeOutcome::BuildArgsFailed(format!(
                "Unknown agent id: {}",
                request.agent_id
            ));
        };
        let Some(launch_path) = resolution.launch_path.clone() else {
            return ProbeOutcome::BuildArgsFailed("agent launch resolved no binary".to_string());
        };
        let temp_dir = std::env::temp_dir().join(format!(
            "od-conn-test-{}",
            std::process::id() as u64 * 1_000_003 + now_ms() as u64 % 1_000_003
        ));
        let _ = std::fs::create_dir_all(&temp_dir);
        let configured_env = agent_cli_env_for_agent(
            validate_agent_cli_env(request.agent_cli_env.as_ref()).as_ref(),
            &request.agent_id,
        );
        let mut env = self.base_env.clone();
        env.insert(
            "OD_DATA_DIR".to_string(),
            self.data_dir.to_string_lossy().into_owned(),
        );
        for (key, value) in def.env {
            env.insert((*key).to_string(), (*value).to_string());
        }
        for (key, value) in &configured_env {
            env.insert(key.clone(), value.clone());
        }
        let launch = crate::launch::resolve_agent_launch(def, &configured_env);
        env = crate::launch::apply_agent_launch_env(
            env,
            &launch.child_path_prepend,
            None,
            &[],
        );
        let caps = HashMap::new();
        let prompt_file_path = temp_dir.join("prompt.txt");
        let prompt_file = if def.prompt_via_file {
            if std::fs::write(&prompt_file_path, SMOKE_PROMPT).is_err() {
                return ProbeOutcome::BuildArgsFailed("failed to stage prompt file".to_string());
            }
            Some(prompt_file_path.to_string_lossy().into_owned())
        } else {
            None
        };
        let context = crate::agents::AgentContext {
            cwd: Some(temp_dir.to_string_lossy().into_owned()),
            platform: std::env::consts::OS.to_string(),
            ..crate::agents::AgentContext::default()
        };
        let options = crate::agents::BuildOptions {
            model: request.model.clone(),
            reasoning: request.reasoning.clone(),
            service_tier: request.service_tier.clone(),
        };
        let live = crate::agents::LiveModelCache::default();
        let dirs: Vec<String> = Vec::new();
        let input = crate::agents::BuildArgsInput {
            caps: &caps,
            options: &options,
            extra_allowed_dirs: &dirs,
            context: &context,
            live: &live,
            live_scope: None,
            prompt: SMOKE_PROMPT,
            prompt_file_path: prompt_file.as_deref(),
            image_paths: &[],
        };
        let mut args = match (def.build_args)(&input) {
            Ok(args) => args,
            Err(message) => return ProbeOutcome::BuildArgsFailed(message),
        };
        if request.agent_id == "opencode" || request.agent_id == "mimo" {
            if !args.iter().any(|arg| arg == "--pure") {
                args.push("--pure".to_string());
            }
            if !args.iter().any(|arg| arg == "--title") {
                args.push("--title".to_string());
                args.push("Connection test".to_string());
            }
        }
        // Pre-spawn auth probe (parity: `probeAgentAuthStatus`).
        let auth = crate::agent_auth::probe_agent_auth_status(
            &request.agent_id,
            def.name,
            def.auth_probe,
            std::path::Path::new(&launch_path),
            &env,
        )
        .await;
        if let Some(auth) = auth {
            if auth.status == AuthProbeStatus::Missing {
                return ProbeOutcome::AuthRequired(auth);
            }
        }
        let stdin_piped = def.prompt_via_stdin
            || def.stream_format == "acp-json-rpc"
            || def.stream_format == "dsh-profile-jsonl";
        let spawned = match crate::spawn::spawn_child(
            std::path::Path::new(&launch_path),
            &args,
            Some(&temp_dir.to_string_lossy()),
            &env,
            &[],
            stdin_piped,
        ) {
            Ok(spawned) => spawned,
            Err(err) => {
                let code = err
                    .raw_os_error()
                    .map(|code| match code {
                        2 => Some("ENOENT".to_string()),
                        _ => None,
                    })
                    .unwrap_or(None);
                return ProbeOutcome::Smoke(SmokeOutcome {
                    winner: Winner::Exit(Err(SpawnFailure {
                        message: format!("agent spawn failed: {err}"),
                        code,
                    })),
                    ..SmokeOutcome::default()
                });
            }
        };
        let mut child = spawned.child;
        if let Some(bytes) = stdin_piped.then(|| crate::spawn::stdin_bytes(def, SMOKE_PROMPT)) {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&bytes).await;
                let _ = stdin.shutdown().await;
            }
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut lane = SmokeLane::new(
            def.stream_format,
            if def.stream_format == "json-event-stream" {
                Some(def.event_parser.unwrap_or(def.id))
            } else {
                None
            },
        );
        // Drain stdout and stderr concurrently with the child.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LaneMessage>();
        if let Some(stdout) = stdout {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send(LaneMessage::Stdout(line)).is_err() {
                        return;
                    }
                }
                let _ = tx.send(LaneMessage::StdoutEnd);
            });
        } else {
            let _ = tx.send(LaneMessage::StdoutEnd);
        }
        if let Some(stderr) = stderr {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send(LaneMessage::Stderr(line)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx);
        let mut winner: Option<Winner> = None;
        let started = tokio::time::Instant::now();
        let timeout_at = started + std::time::Duration::from_millis(self.timeout_ms);
        // Parity: `scheduleTextResolution` — one `AGENT_COMPLETION_DEBOUNCE_MS`
        // window of silence after text turns a still-running child into a text
        // completion (long-lived ACP agents never exit on their own).
        let mut text_deadline: Option<tokio::time::Instant> = None;
        loop {
            let deadline = match text_deadline {
                Some(debounce) => debounce.min(timeout_at),
                None => timeout_at,
            };
            let now = tokio::time::Instant::now();
            if deadline <= now {
                if text_deadline.map(|d| d <= now).unwrap_or(false)
                    && !lane.browser.text.trim().is_empty()
                {
                    winner = Some(Winner::Text);
                } else {
                    winner.get_or_insert(Winner::Timeout);
                }
                break;
            }
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => {
                    if text_deadline.map(|d| d <= tokio::time::Instant::now()).unwrap_or(false)
                        && !lane.browser.text.trim().is_empty()
                    {
                        winner = Some(Winner::Text);
                    } else {
                        winner.get_or_insert(Winner::Timeout);
                    }
                    break;
                }
                Ok(None) => break,
                Ok(Some(message)) => match message {
                    LaneMessage::Stdout(line) => {
                        lane.feed_line(&line);
                        if let Some(error) = lane.browser.stream_error.clone() {
                            winner.get_or_insert(Winner::StreamError(error));
                        }
                        if !lane.browser.text.trim().is_empty() && winner.is_none() {
                            text_deadline = Some(
                                tokio::time::Instant::now()
                                    + std::time::Duration::from_millis(AGENT_COMPLETION_DEBOUNCE_MS),
                            );
                        }
                    }
                    LaneMessage::Stderr(line) => {
                        lane.browser.append_stderr(&line);
                        lane.browser.append_stderr("\n");
                    }
                    LaneMessage::StdoutEnd => {
                        lane.flush();
                        if winner.is_none() && !lane.browser.text.trim().is_empty() {
                            winner = Some(Winner::Text);
                        }
                        if winner.is_some() {
                            break;
                        }
                    }
                },
            }
            if let Ok(Some(status)) = child.try_wait() {
                winner.get_or_insert(Winner::Exit(Ok(exit_to_child_exit(&status))));
                break;
            }
        }
        // Parity: streaming CLI output can lag the close event.
        tokio::time::sleep(std::time::Duration::from_millis(AGENT_STDOUT_DRAIN_MS)).await;
        while let Ok(message) = rx.try_recv() {
            match message {
                LaneMessage::Stdout(line) => lane.feed_line(&line),
                LaneMessage::Stderr(line) => {
                    lane.browser.append_stderr(&line);
                    lane.browser.append_stderr("\n");
                }
                LaneMessage::StdoutEnd => {}
            }
        }
        // Parity: `claude.flush()` / `copilot.flush()` on the child's close
        // event. A trailing partial line only reaches the parser here.
        lane.flush();
        let winner = winner.unwrap_or(Winner::Timeout);
        // Parity: the `finally` block. A text winner waits out the completion
        // race (bounded); every other winner returns immediately and the child
        // is killed.
        let completion = if matches!(winner, Winner::Text) {
            match tokio::time::timeout(
                std::time::Duration::from_millis(AGENT_KILL_GRACE_MS),
                child.wait(),
            )
            .await
            {
                Ok(Ok(status)) => Some(Winner::Exit(Ok(exit_to_child_exit(&status)))),
                Ok(Err(err)) => Some(Winner::Exit(Err(SpawnFailure {
                    message: err.to_string(),
                    code: None,
                }))),
                Err(_) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    Some(Winner::Timeout)
                }
            }
        } else {
            if !matches!(winner, Winner::Exit(_)) {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            None
        };
        let antigravity_log_tail = if request.agent_id == "antigravity" {
            read_antigravity_log_tail(&env, &context)
        } else {
            None
        };
        let browser = lane.browser;
        ProbeOutcome::Smoke(SmokeOutcome {
            winner,
            completion,
            text: browser.text,
            raw_stdout: browser.raw_stdout,
            raw_stdout_tail: browser.raw_stdout_tail,
            stderr_tail: browser.stderr_tail,
            resolved_model: browser.resolved_model,
            saw_terminal_completion: browser.saw_terminal_completion,
            acp_clean_completion: false,
            acp_fatal: false,
            antigravity_log_tail,
        })
    }
}

enum LaneMessage {
    Stdout(String),
    Stderr(String),
    StdoutEnd,
}

/// Best-effort tail of agy's `--log-file` (parity: the antigravity guard).
fn read_antigravity_log_tail(
    env: &HashMap<String, String>,
    context: &crate::agents::AgentContext,
) -> Option<String> {
    let path = context.agent_log_file_path.clone().or_else(|| {
        env.get("OD_AGENT_LOG_FILE")
            .cloned()
            .filter(|path| !path.is_empty())
    })?;
    let content = std::fs::read_to_string(path).ok()?;
    if content.len() <= 8_192 {
        return Some(content);
    }
    let mut start = content.len() - 8_192;
    while start < content.len() && !content.is_char_boundary(start) {
        start += 1;
    }
    Some(content[start..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_resolution() -> AgentResolution {
        AgentResolution {
            agent_name: "Fixture Agent".to_string(),
            selected_path: Some("/usr/local/bin/fixture".to_string()),
            launch_path: Some("/usr/local/bin/fixture".to_string()),
            env: HashMap::new(),
            ..AgentResolution::default()
        }
    }

    fn resolve(outcome: &SmokeOutcome) -> Value {
        resolve_for("fixture", "Fixture Agent", outcome)
    }

    fn resolve_for(agent_id: &str, agent_name: &str, outcome: &SmokeOutcome) -> Value {
        resolve_agent_connection(agent_id, agent_name, "default", &ctx_resolution(), 0, outcome)
    }

    fn exit(code: Option<i64>, signal: Option<&str>) -> Winner {
        Winner::Exit(Ok(ChildExit {
            code,
            signal: signal.map(str::to_string),
        }))
    }

    #[test]
    fn redaction_matches_javascript_shapes() {
        assert_eq!(redact_secrets("", &[]), "");
        assert_eq!(
            redact_secrets("Authorization: Bearer sk-abc123-_./+=", &[]),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(
            redact_secrets("x-api-key: sk-abc123", &[]),
            "x-api-key: [REDACTED]"
        );
        assert_eq!(
            redact_secrets("api-key=sk-abc123", &[]),
            "api-key: [REDACTED]"
        );
        assert_eq!(
            redact_secrets("https://x.test/v1?key=sk-abc123&a=1", &[]),
            "https://x.test/v1?key=[REDACTED]&a=1"
        );
        assert_eq!(redact_secrets("token is hunter2", &["hunter2"]), "token is [REDACTED]");
        // regex metacharacters in the exact secret stay literal.
        assert_eq!(redact_secrets("a+b", &["a+b"]), "[REDACTED]");
    }

    #[test]
    fn sampling_collapses_whitespace_and_ellipsizes() {
        assert_eq!(truncate_sample("  ok \n"), "ok");
        assert_eq!(truncate_sample("a\t\tb"), "a b");
        let long = "x".repeat(200);
        let sampled = truncate_sample(&long);
        assert_eq!(sampled.chars().count(), SAMPLE_MAX_CHARS);
        assert!(sampled.ends_with('…'));
    }

    #[test]
    fn smoke_reply_and_model_error_detection() {
        assert!(is_smoke_ok_reply(" ok \n"));
        assert!(is_smoke_ok_reply("OK"));
        assert!(!is_smoke_ok_reply("okay"));
        assert!(is_likely_model_error_text("no endpoints found for model x"));
        assert!(is_likely_model_error_text("The model is not found"));
        assert!(is_likely_model_error_text("model: unknown"));
        assert!(!is_likely_model_error_text("ok"));
        assert!(!is_likely_model_error_text("invalid"));
        assert_eq!(
            smoke_failure_detail(""),
            "Provider returned a 2xx response without assistant text"
        );
        assert_eq!(
            smoke_failure_detail("hi"),
            "Expected smoke test reply \"ok\"; got \"hi\""
        );
    }

    #[test]
    fn claude_result_frame_takes_the_last_result_line() {
        let stdout = concat!(
            "{\"type\":\"assistant\"}\n",
            "{\"type\":\"result\",\"is_error\":true,\"subtype\":\"error\"}\n",
            "{\"type\":\"result\",\"result\":\"  ok  \",\"stop_reason\":\"end_turn\",\"subtype\":\"success\"}\n"
        );
        let frame = parse_claude_result_frame(stdout).expect("frame");
        assert_eq!(frame.result_text, "ok");
        assert_eq!(frame.stop_reason.as_deref(), Some("end_turn"));
        assert!(!frame.is_error);
        assert_eq!(frame.subtype.as_deref(), Some("success"));
        assert!(parse_claude_result_frame("not json\n").is_none());
        let terminal = parse_claude_result_frame(
            "{\"type\":\"result\",\"terminal_reason\":\"max_turns\"}\n",
        )
        .expect("terminal frame");
        assert_eq!(terminal.stop_reason.as_deref(), Some("max_turns"));
    }

    #[test]
    fn openeocode_helpers() {
        let stdout = concat!(
            "{\"type\":\"text\",\"part\":{\"text\":\"o\"}}\n",
            "{\"type\":\"step_start\"}\n",
            "{\"type\":\"text\",\"part\":{\"text\":\"k\"}}\n",
            "not json\n"
        );
        assert_eq!(extract_opencode_text_from_raw_stdout(stdout), "ok");
        assert!(opencode_outdated_cli_detail("Usage: opencode [options]").is_some());
        assert!(opencode_outdated_cli_detail("error: unknown option '--pure'").is_some());
        assert!(opencode_outdated_cli_detail("all good").is_none());
        let detail = opencode_provider_connectivity_detail(
            "x\nCannot connect to API: https://x.test:1234 refused",
        )
        .expect("detail");
        assert!(detail.starts_with(
            "OpenCode reported a provider connectivity failure before the connection test timed out:"
        ));
        assert!(opencode_provider_connectivity_detail("all good").is_none());
    }

    #[test]
    fn codex_guidance_shapes() {
        assert_eq!(codex_executable_guidance("codex", None, Some("/p/codex")), "");
        assert_eq!(
            codex_executable_guidance("claude", Some("/a"), Some("/b")),
            ""
        );
        assert_eq!(codex_executable_guidance("codex", Some("/a"), Some("/a")), "");
        assert!(codex_executable_guidance("codex", Some("/a"), Some("/b"))
            .starts_with(" Configured Codex path failed: /a."));
        assert_eq!(
            codex_configured_path_success_detail("/a"),
            "This test used the configured Codex path: /a."
        );
    }

    #[test]
    fn claude_diagnostics_classify_known_failures() {
        let input = ClaudeCliDiagnosticInput {
            agent_id: "claude".to_string(),
            exit_code: Some(1),
            stderr_tail: Some("Error: 401 invalid api key".to_string()),
            env: HashMap::new(),
            ..ClaudeCliDiagnosticInput::default()
        };
        let diagnostic = diagnose_claude_cli_failure(&input).expect("diagnostic");
        assert!(diagnostic.detail.contains("could not authenticate"));
        assert_eq!(diagnostic.code, None);
        assert!(diagnostic.retryable);

        let dropped = ClaudeCliDiagnosticInput {
            stderr_tail: Some("API Error: The socket connection was closed unexpectedly".to_string()),
            ..input.clone()
        };
        assert_eq!(
            diagnose_claude_cli_failure(&dropped).and_then(|d| d.code),
            Some("AGENT_CONNECTION_DROPPED".to_string())
        );

        // Default endpoint + mid-stream drop names the default endpoint.
        let dropped_default = diagnose_claude_cli_failure(&dropped).expect("dropped");
        assert!(dropped_default.detail.contains("the Anthropic API"));

        let custom = ClaudeCliDiagnosticInput {
            env: HashMap::from([(
                "ANTHROPIC_BASE_URL".to_string(),
                "http://127.0.0.1:8787".to_string(),
            )]),
            stderr_tail: Some("connect ECONNREFUSED 127.0.0.1:8787".to_string()),
            ..input.clone()
        };
        let refused = diagnose_claude_cli_failure(&custom).expect("refused");
        assert!(refused
            .message
            .contains("could not reach the configured custom Anthropic endpoint"));

        // A clean exit and a signal-only exit produce no diagnostic.
        let clean = ClaudeCliDiagnosticInput {
            exit_code: Some(0),
            ..input.clone()
        };
        assert!(diagnose_claude_cli_failure(&clean).is_none());
        let other_agent = ClaudeCliDiagnosticInput {
            agent_id: "codex".to_string(),
            ..input.clone()
        };
        assert!(diagnose_claude_cli_failure(&other_agent).is_none());
        // OpenClaude naming comes from the resolved binary basename.
        let openclaude = ClaudeCliDiagnosticInput {
            resolved_bin: Some("/opt/OpenClaude.exe".to_string()),
            exit_code: Some(1),
            stderr_tail: None,
            stdout_tail: None,
            ..input.clone()
        };
        let diagnostic = diagnose_claude_cli_failure(&openclaude).expect("openclaude");
        assert!(diagnostic.message.contains("OpenClaude exited before producing diagnostics."));
    }

    // ---- decision tree, driven by fixtures (no real agent) ----------------

    #[test]
    fn text_winner_produces_a_success_envelope() {
        let outcome = SmokeOutcome {
            winner: Winner::Text,
            completion: Some(exit(Some(0), None)),
            text: "ok\n".to_string(),
            raw_stdout: "ok\n".to_string(),
            raw_stdout_tail: "ok\n".to_string(),
            ..SmokeOutcome::default()
        };
        let response = resolve(&outcome);
        assert_eq!(response["ok"], json!(true));
        assert_eq!(response["kind"], json!("success"));
        assert_eq!(response["sample"], json!("ok"));
        assert_eq!(response["agentName"], json!("Fixture Agent"));
        assert_eq!(response["diagnostics"]["phase"], json!("connection_smoke_test"));
        assert_eq!(response["diagnostics"]["exitCode"], json!(0));
        assert_eq!(response["diagnostics"]["signal"], Value::Null);
        assert_eq!(response["diagnostics"]["binaryPath"], json!("/usr/local/bin/fixture"));
    }

    #[test]
    fn text_winner_with_model_error_reports_not_found_model() {
        let outcome = SmokeOutcome {
            winner: Winner::Text,
            completion: Some(exit(Some(1), None)),
            text: "The model gpt-x is not found".to_string(),
            ..SmokeOutcome::default()
        };
        let response = resolve(&outcome);
        assert_eq!(response["ok"], json!(false));
        assert_eq!(response["kind"], json!("not_found_model"));
        assert_eq!(
            response["detail"],
            json!("Expected smoke test reply \"ok\"; got \"The model gpt-x is not found\"")
        );
        assert_eq!(response["diagnostics"]["phase"], json!("output_parse"));
    }

    #[test]
    fn non_clean_text_without_model_error_is_a_spawn_failure() {
        let outcome = SmokeOutcome {
            winner: Winner::Text,
            completion: Some(exit(Some(1), None)),
            text: "partial answer".to_string(),
            stderr_tail: "boom".to_string(),
            ..SmokeOutcome::default()
        };
        let response = resolve(&outcome);
        assert_eq!(response["kind"], json!("agent_spawn_failed"));
        // `buffered` is non-empty, so the parse phase applies.
        assert_eq!(response["diagnostics"]["phase"], json!("output_parse"));
        let detail = response["detail"].as_str().unwrap_or_default();
        assert!(detail.contains("exit 1"), "{detail}");
        assert!(detail.contains("stderr: boom"), "{detail}");
    }

    #[test]
    fn stream_error_routes_through_auth_then_model_then_spawn() {
        let auth = SmokeOutcome {
            winner: Winner::StreamError("cursor-agent: not logged in".to_string()),
            ..SmokeOutcome::default()
        };
        let response = resolve_for("cursor-agent", "Cursor Agent", &auth);
        assert_eq!(response["kind"], json!("agent_auth_required"));
        assert_eq!(response["detail"], json!(cursor_auth_guidance()));

        let model = SmokeOutcome {
            winner: Winner::StreamError("model not found".to_string()),
            ..SmokeOutcome::default()
        };
        let response = resolve(&model);
        assert_eq!(response["kind"], json!("not_found_model"));
        assert_eq!(response["diagnostics"]["phase"], json!("output_parse"));

        let other = SmokeOutcome {
            winner: Winner::StreamError("socket closed".to_string()),
            ..SmokeOutcome::default()
        };
        let response = resolve(&other);
        assert_eq!(response["kind"], json!("agent_spawn_failed"));
        assert_eq!(response["diagnostics"]["phase"], json!("spawn"));
    }

    #[test]
    fn spawn_failure_maps_enoent_to_agent_not_installed() {
        let missing = SmokeOutcome {
            winner: Winner::Exit(Err(SpawnFailure {
                message: "spawn fixture ENOENT".to_string(),
                code: Some("ENOENT".to_string()),
            })),
            ..SmokeOutcome::default()
        };
        let response = resolve(&missing);
        assert_eq!(response["kind"], json!("agent_not_installed"));
        assert_eq!(response["diagnostics"]["phase"], json!("binary_resolution"));

        let other = SmokeOutcome {
            winner: Winner::Exit(Err(SpawnFailure {
                message: "spawn EACCES".to_string(),
                code: Some("EACCES".to_string()),
            })),
            ..SmokeOutcome::default()
        };
        let response = resolve(&other);
        assert_eq!(response["kind"], json!("agent_spawn_failed"));
        assert_eq!(response["diagnostics"]["phase"], json!("spawn"));
    }

    #[test]
    fn timeout_and_cancellation_report_timeout() {
        let outcome = SmokeOutcome {
            winner: Winner::Timeout,
            ..SmokeOutcome::default()
        };
        let response = resolve(&outcome);
        assert_eq!(response["kind"], json!("timeout"));
        assert_eq!(response["ok"], json!(false));
    }

    #[test]
    fn opencode_timeout_with_provider_detail_prefers_upstream_unavailable() {
        let outcome = SmokeOutcome {
            winner: Winner::Timeout,
            stderr_tail: "Cannot connect to API: http://x.test:1 refused".to_string(),
            ..SmokeOutcome::default()
        };
        let response = resolve_agent_connection(
            "opencode",
            "OpenCode",
            "default",
            &ctx_resolution(),
            0,
            &outcome,
        );
        assert_eq!(response["kind"], json!("upstream_unavailable"));
        assert_eq!(response["diagnostics"]["phase"], json!("connection_smoke_test"));
    }

    #[test]
    fn codex_clean_exit_with_no_text_stays_unknown() {
        let outcome = SmokeOutcome {
            winner: Winner::Exit(Ok(ChildExit {
                code: Some(0),
                signal: None,
            })),
            ..SmokeOutcome::default()
        };
        let response = resolve(&outcome);
        assert_eq!(response["kind"], json!("unknown"));
        assert_eq!(response["detail"], json!("exit 0"));
    }

    #[test]
    fn claude_late_exit_one_with_a_success_result_frame_is_a_success() {
        let outcome = SmokeOutcome {
            winner: Winner::Exit(Ok(ChildExit {
                code: Some(1),
                signal: None,
            })),
            raw_stdout: "{\"type\":\"result\",\"result\":\"ok\",\"subtype\":\"success\"}\n".to_string(),
            saw_terminal_completion: true,
            ..SmokeOutcome::default()
        };
        let response = resolve_agent_connection(
            "claude",
            "Claude Code",
            "default",
            &ctx_resolution(),
            0,
            &outcome,
        );
        assert_eq!(response["ok"], json!(true));
        assert_eq!(response["sample"], json!("ok"));
    }

    #[test]
    fn antigravity_silent_exit_falls_back_to_auth() {
        let outcome = SmokeOutcome {
            winner: Winner::Exit(Ok(ChildExit {
                code: Some(0),
                signal: None,
            })),
            ..SmokeOutcome::default()
        };
        let response = resolve_agent_connection(
            "antigravity",
            "Antigravity",
            "default",
            &ctx_resolution(),
            0,
            &outcome,
        );
        assert_eq!(response["kind"], json!("agent_auth_required"));
        assert_eq!(response["detail"], json!(antigravity_auth_guidance()));

        let quota = SmokeOutcome {
            winner: Winner::Exit(Ok(ChildExit {
                code: Some(0),
                signal: None,
            })),
            antigravity_log_tail: Some("RESOURCE_EXHAUSTED: Individual quota reached".to_string()),
            ..SmokeOutcome::default()
        };
        let response = resolve_agent_connection(
            "antigravity",
            "Antigravity",
            "default",
            &ctx_resolution(),
            0,
            &quota,
        );
        assert_eq!(response["kind"], json!("rate_limited"));
        assert_eq!(response["detail"], json!(antigravity_quota_guidance()));
    }

    #[test]
    fn opencode_recovered_text_from_raw_stdout_still_succeeds() {
        let outcome = SmokeOutcome {
            winner: Winner::Exit(Ok(ChildExit {
                code: Some(0),
                signal: None,
            })),
            raw_stdout_tail: "{\"type\":\"text\",\"part\":{\"text\":\"ok\"}}\n".to_string(),
            ..SmokeOutcome::default()
        };
        let response = resolve_agent_connection(
            "opencode",
            "OpenCode",
            "default",
            &ctx_resolution(),
            0,
            &outcome,
        );
        assert_eq!(response["ok"], json!(true));
        assert_eq!(response["sample"], json!("ok"));
    }

    #[test]
    fn opencode_outdated_cli_is_a_spawn_failure_with_the_upgrade_hint() {
        let outcome = SmokeOutcome {
            winner: Winner::Exit(Ok(ChildExit {
                code: Some(1),
                signal: None,
            })),
            stderr_tail: "Usage: opencode [options]".to_string(),
            ..SmokeOutcome::default()
        };
        let response = resolve_agent_connection(
            "opencode",
            "OpenCode",
            "default",
            &ctx_resolution(),
            0,
            &outcome,
        );
        assert_eq!(response["kind"], json!("agent_spawn_failed"));
        assert_eq!(response["detail"], json!(OPENCODE_OUTDATED_CLI_DETAIL));
        assert_eq!(response["diagnostics"]["phase"], json!("spawn"));
    }

    #[test]
    fn acp_forced_shutdown_counts_as_a_clean_completion() {
        let outcome = SmokeOutcome {
            winner: Winner::Text,
            completion: Some(exit(None, Some("SIGTERM"))),
            text: "ok".to_string(),
            saw_terminal_completion: true,
            acp_clean_completion: true,
            ..SmokeOutcome::default()
        };
        let response = resolve(&outcome);
        assert_eq!(response["ok"], json!(true));
        assert_eq!(response["diagnostics"]["exitCode"], Value::Null);
        assert_eq!(response["diagnostics"]["signal"], json!("SIGTERM"));
    }

    // ---- orchestration with a fixture probe ------------------------------

    struct FixtureProbe {
        resolution: ResolutionOutcome,
        outcome: ProbeOutcome,
    }

    impl AgentProbe for FixtureProbe {
        fn resolve<'a>(
            &'a self,
            _agent_id: &'a str,
            _configured_env: &'a HashMap<String, String>,
        ) -> BoxFuture<'a, ResolutionOutcome> {
            let resolution = self.resolution.clone();
            Box::pin(async move { resolution })
        }

        fn run<'a>(
            &'a self,
            _request: &'a AgentConnectionRequest,
            _resolution: &'a AgentResolution,
        ) -> BoxFuture<'a, ProbeOutcome> {
            let outcome = self.outcome.clone();
            Box::pin(async move { outcome })
        }
    }

    fn request() -> AgentConnectionRequest {
        AgentConnectionRequest {
            agent_id: "fixture".to_string(),
            ..AgentConnectionRequest::default()
        }
    }

    #[tokio::test]
    async fn unknown_agent_is_not_installed_with_the_id_in_the_detail() {
        let probe = FixtureProbe {
            resolution: ResolutionOutcome::UnknownAgent,
            outcome: ProbeOutcome::Smoke(SmokeOutcome::default()),
        };
        let response =
            test_agent_connection_internal(&probe, &request(), &HashMap::new()).await;
        assert_eq!(response["ok"], json!(false));
        assert_eq!(response["kind"], json!("agent_not_installed"));
        assert_eq!(response["agentName"], json!("fixture"));
        assert_eq!(response["detail"], json!("Unknown agent id: fixture"));
        assert_eq!(response["diagnostics"]["phase"], json!("binary_resolution"));
        assert_eq!(response["model"], json!("default"));
    }

    #[tokio::test]
    async fn not_installed_reports_the_def_name() {
        // `sh` stands in for a shipped def; `fixture` has no def, so use a real id.
        let probe = FixtureProbe {
            resolution: ResolutionOutcome::NotInstalled,
            outcome: ProbeOutcome::Smoke(SmokeOutcome::default()),
        };
        let mut req = request();
        req.agent_id = "claude".to_string();
        let response =
            test_agent_connection_internal(&probe, &req, &HashMap::new()).await;
        assert_eq!(response["kind"], json!("agent_not_installed"));
        assert!(response["agentName"].as_str().unwrap_or_default().len() > 1);
        assert!(response.get("detail").is_none());
    }

    #[tokio::test]
    async fn build_args_failure_uses_the_binary_resolution_phase() {
        let probe = FixtureProbe {
            resolution: ResolutionOutcome::Ready(ctx_resolution()),
            outcome: ProbeOutcome::BuildArgsFailed("bad prompt file".to_string()),
        };
        let response =
            test_agent_connection_internal(&probe, &request(), &HashMap::new()).await;
        assert_eq!(response["kind"], json!("agent_spawn_failed"));
        assert_eq!(response["detail"], json!("bad prompt file"));
        assert_eq!(response["diagnostics"]["phase"], json!("binary_resolution"));
    }

    #[tokio::test]
    async fn auth_probe_folds_its_output_into_diagnostics() {
        let probe = FixtureProbe {
            resolution: ResolutionOutcome::Ready(ctx_resolution()),
            outcome: ProbeOutcome::AuthRequired(AuthProbeResult {
                status: AuthProbeStatus::Missing,
                message: Some("Cursor Agent is not authenticated.".to_string()),
                stdout_tail: Some("out".to_string()),
                stderr_tail: Some("err".to_string()),
                exit_code: Some(1),
                signal: Some("SIGTERM".to_string()),
            }),
        };
        let response =
            test_agent_connection_internal(&probe, &request(), &HashMap::new()).await;
        assert_eq!(response["kind"], json!("agent_auth_required"));
        assert_eq!(response["detail"], json!("Cursor Agent is not authenticated."));
        assert_eq!(response["diagnostics"]["phase"], json!("binary_resolution"));
        assert_eq!(response["diagnostics"]["stdoutTail"], json!("out"));
        assert_eq!(response["diagnostics"]["stderrTail"], json!("err"));
        assert_eq!(response["diagnostics"]["exitCode"], json!(1));
        assert_eq!(response["diagnostics"]["signal"], json!("SIGTERM"));
    }

    #[tokio::test]
    async fn missing_auth_message_defaults_to_cursor_guidance() {
        let probe = FixtureProbe {
            resolution: ResolutionOutcome::Ready(ctx_resolution()),
            outcome: ProbeOutcome::AuthRequired(AuthProbeResult {
                status: AuthProbeStatus::Missing,
                message: None,
                stdout_tail: None,
                stderr_tail: None,
                exit_code: Some(0),
                signal: None,
            }),
        };
        let response =
            test_agent_connection_internal(&probe, &request(), &HashMap::new()).await;
        assert_eq!(response["detail"], json!(cursor_auth_guidance()));
    }

    #[test]
    fn agent_cli_env_prefs_filter_and_strip() {
        let raw = json!({
            "codex": {"CODEX_BIN": "/opt/codex", "NOT_ALLOWED": "x", "CODEX_HOME": ""},
            "unknown-agent": {"FOO": "bar"},
            "opencode": {"OPENCODE_BIN": "/opt/opencode"},
        });
        let prefs = validate_agent_cli_env(Some(&raw)).expect("prefs");
        assert_eq!(
            prefs["codex"],
            json!({"CODEX_BIN": "/opt/codex"})
        );
        assert!(prefs.get("unknown-agent").is_none());
        let env = agent_cli_env_for_agent(Some(&prefs), "codex");
        assert_eq!(env.get("CODEX_BIN").map(String::as_str), Some("/opt/codex"));
        // byok-opencode reads the opencode prefs.
        let env = agent_cli_env_for_agent(Some(&prefs), "byok-opencode");
        assert_eq!(env.get("OPENCODE_BIN").map(String::as_str), Some("/opt/opencode"));
        let stripped = strip_codex_bin_override(Some(prefs)).expect("stripped");
        assert!(stripped.get("codex").is_none());
        assert!(stripped.get("opencode").is_some());
        // Parity: an empty prefs map without a CODEX_BIN passes through
        // unchanged (`validateAgentCliEnv` never produces one anyway).
        assert!(strip_codex_bin_override(None).is_none());
        assert_eq!(strip_codex_bin_override(Some(Map::new())), Some(Map::new()));
        // A codex entry without CODEX_BIN also passes through.
        let no_bin = json!({"codex": {"CODEX_HOME": "/tmp/x"}});
        let prefs = validate_agent_cli_env(Some(&no_bin)).expect("prefs");
        assert!(strip_codex_bin_override(Some(prefs)).is_some());
    }

    /// Parity: the per-format lane must feed each parser the way the run
    /// pump does — a line *with* its terminator. Feeding raw lines without it
    /// made the parser buffer the whole stream into one unparseable blob, so
    /// a real smoke test reported `unknown` with exit 0 despite full output.
    #[test]
    fn smoke_lane_feeds_the_claude_parser_with_line_terminators() {
        let mut lane = SmokeLane::new("claude-stream-json", None);
        lane.feed_line(r#"{"type":"system","subtype":"init","model":"opus","session_id":"s1"}"#);
        lane.feed_line(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"ok"}]}}"#,
        );
        lane.feed_line(
            r#"{"type":"result","subtype":"success","stop_reason":"end_turn"}"#,
        );
        lane.flush();
        assert_eq!(lane.browser.text, "ok");
        assert_eq!(lane.browser.resolved_model.as_deref(), Some("opus"));
        assert!(lane.browser.saw_terminal_completion);
        // The claude lane mirrors stdout into the raw buffers.
        assert!(lane.browser.raw_stdout.contains(r#""text":"ok""#));
    }

    #[test]
    fn smoke_lane_plain_format_streams_stdout_as_text() {
        let mut lane = SmokeLane::new("plain", None);
        lane.feed_line("ok");
        lane.flush();
        assert_eq!(lane.browser.text, "ok\n");
    }

    #[test]
    fn timeout_override_respects_the_clamp() {let mut env = HashMap::new();
        env.insert(
            "OD_CONNECTION_TEST_AGENT_TIMEOUT_MS".to_string(),
            "3000000000".to_string(),
        );
        assert_eq!(agent_timeout_ms(&env), DEFAULT_AGENT_TIMEOUT_MS);
        env.insert(
            "OD_CONNECTION_TEST_AGENT_TIMEOUT_MS".to_string(),
            "1000".to_string(),
        );
        assert_eq!(agent_timeout_ms(&env), 1000);
        env.insert(
            "OD_CONNECTION_TEST_AGENT_TIMEOUT_MS".to_string(),
            "junk".to_string(),
        );
        assert_eq!(agent_timeout_ms(&env), DEFAULT_AGENT_TIMEOUT_MS);
    }
}
