//! Agent authentication probing — Rust parity port of
//! `apps/daemon/src/runtimes/auth.ts` (guidance strings, failure-text
//! predicates, `classifyAgentAuthFailure`, and `probeAgentAuthStatus`).
//!
//! Deferred to a follow-up: `classifyAgentServiceFailure` and
//! `reportsToolPrincipalAuthFailure` (the STATUS_CTX attribution
//! machinery), which feed run-failure classification rather than probing.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

use crate::detect::{run_probe, ProbeOutput};

/// Parity: `AgentAuthProbeResult` (auth.ts:8).
#[derive(Debug, Clone, PartialEq)]
pub struct AuthProbeResult {
    pub status: AuthProbeStatus,
    pub message: Option<String>,
    pub stdout_tail: Option<String>,
    pub stderr_tail: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProbeStatus {
    Ok,
    Missing,
    Unknown,
}

// ---- guidance (parity: auth.ts guidance constants) -------------------------

pub fn cursor_auth_guidance() -> &'static str {
    "Cursor Agent is not authenticated. Run `cursor-agent login`, then `cursor-agent status`, and retry. For automation, ensure CURSOR_API_KEY is set in the OpenDesign process environment."
}

pub fn deepseek_auth_guidance() -> &'static str {
    "DeepSeek TUI is installed but is not authenticated. Add or verify your API key in `~/.deepseek/config.toml` as `api_key = \"...\"`, or expose DEEPSEEK_API_KEY to the OpenDesign daemon process, then retry. If OpenDesign is launched outside an interactive shell, shell rc files such as ~/.zshrc may not be loaded."
}

pub fn deepseek_harness_auth_guidance() -> &'static str {
    "DeepSeek Harness has no model API key configured. Open a terminal and run `dsh web`, then open Settings → Models and add your DeepSeek API key. Return to OpenDesign and retry. For automation, expose DEEPSEEK_API_KEY to the OpenDesign process."
}

pub fn antigravity_auth_guidance() -> &'static str {
    "Antigravity needs to sign in. The agy CLI's keyring entry has expired or been cleared, and `-p` print mode cannot complete OAuth on its own (it has no field to paste the auth code into).\n\nFix: open a terminal and run `agy` once — it will open Google sign-in in your browser, accept the redirect, and store the token in your system keyring. After you finish, return here and retry this chat. You only need to do this once; the keyring entry persists across both terminal and OpenDesign runs."
}

pub fn reasonix_auth_guidance() -> &'static str {
    "DeepSeek Reasonix is installed but is not authenticated. Add your API key in `~/.reasonix/config.json` under `apiKey`, or expose DEEPSEEK_API_KEY to the OpenDesign daemon process, then retry. If OpenDesign is launched outside an interactive shell, shell rc files such as ~/.zshrc may not be loaded."
}

pub fn claude_auth_guidance() -> &'static str {
    "Claude Code is installed but is not authenticated. Run `claude auth login` or open `claude` and complete login in a terminal, then rescan. If OpenDesign was launched outside an interactive shell, your shell rc files (e.g. ~/.zshrc) may not be loaded into its environment."
}

fn generic_auth_guidance(agent_name: &str) -> String {
    format!(
        "{agent_name} appears to be installed but is not authenticated. Sign in with the CLI in a terminal, then rescan. If OpenDesign was launched outside an interactive shell, your shell rc files (e.g. ~/.zshrc) may not be loaded into its environment."
    )
}

// ---- failure-text predicates ------------------------------------------------

fn non_blank(text: &str) -> Option<&str> {
    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

/// Parity: `isCursorAuthFailureText` (auth.ts:78).
pub fn is_cursor_auth_failure_text(text: &str) -> bool {
    let Some(value) = non_blank(text) else {
        return false;
    };
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new("(?i)authentication required|not authenticated|not logged in|unauthenticated|agent login|cursor_api_key")
            .expect("cursor auth regex")
    });
    re.is_match(value)
}

/// Parity: `isAntigravityAuthFailureText` (auth.ts:107).
pub fn is_antigravity_auth_failure_text(text: &str) -> bool {
    let Some(value) = non_blank(text) else {
        return false;
    };
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new("(?i)authentication required.*please visit|authentication timed out|authentication failed or timed out|authentication required\\. run 'agy' to log in|not logged into antigravity|accounts\\.google\\.com/o/oauth2/auth.*antigravity")
            .expect("antigravity auth regex")
    });
    re.is_match(value)
}

/// Parity: `isDeepSeekAuthFailureText` (auth.ts:129).
pub fn is_deepseek_auth_failure_text(text: &str) -> bool {
    let Some(value) = non_blank(text) else {
        return false;
    };
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new("(?i)\\b(?:MISSING_CREDENTIAL|DSH_PROVIDER_AUTH_FAILED)\\b|KEY=<your-key>|api_key\\s*=\\s*[\"']<your-key>[\"']")
            .expect("deepseek auth regex")
    });
    if re.is_match(value) {
        return true;
    }
    static CONFIG_RE: OnceLock<Regex> = OnceLock::new();
    let config_re = CONFIG_RE.get_or_init(|| {
        Regex::new("(?i)~/\\.deepseek/config\\.toml").expect("deepseek config regex")
    });
    static KEY_RE: OnceLock<Regex> = OnceLock::new();
    let key_re = KEY_RE.get_or_init(|| {
        Regex::new("(?i)api[_ -]?key|KEY=").expect("deepseek key regex")
    });
    if config_re.is_match(value) && key_re.is_match(value) {
        return true;
    }
    static ENV_RE: OnceLock<Regex> = OnceLock::new();
    let env_re = ENV_RE.get_or_init(|| Regex::new("DEEPSEEK_API_KEY").expect("deepseek env regex"));
    static ENV_CTX_RE: OnceLock<Regex> = OnceLock::new();
    let env_ctx_re = ENV_CTX_RE.get_or_init(|| {
        Regex::new("(?i)auth|api[_ -]?key|missing|not set|required|unauthorized")
            .expect("deepseek env ctx regex")
    });
    env_re.is_match(value) && env_ctx_re.is_match(value)
}

/// Parity: `isReasonixAuthFailureText` (auth.ts:193).
pub fn is_reasonix_auth_failure_text(text: &str) -> bool {
    let Some(value) = non_blank(text) else {
        return false;
    };
    static CONFIG_RE: OnceLock<Regex> = OnceLock::new();
    let config_re = CONFIG_RE.get_or_init(|| {
        Regex::new("(?i)~/\\.reasonix/config\\.json").expect("reasonix config regex")
    });
    static KEY_RE: OnceLock<Regex> = OnceLock::new();
    let key_re = KEY_RE.get_or_init(|| {
        Regex::new("(?i)api[_ -]?key|missing|not set|required|unauthorized|invalid")
            .expect("reasonix key regex")
    });
    if config_re.is_match(value) && key_re.is_match(value) {
        return true;
    }
    static ENV_RE: OnceLock<Regex> = OnceLock::new();
    let env_re = ENV_RE.get_or_init(|| Regex::new("DEEPSEEK_API_KEY").expect("reasonix env regex"));
    static AUTH_RE: OnceLock<Regex> = OnceLock::new();
    let auth_re = AUTH_RE.get_or_init(|| {
        Regex::new("(?i)auth|missing|not set|required|unauthorized|invalid")
            .expect("reasonix auth regex")
    });
    env_re.is_match(value) && auth_re.is_match(value)
}

/// Parity: `isClaudeAuthFailureText` (auth.ts:206).
pub fn is_claude_auth_failure_text(text: &str) -> bool {
    let Some(value) = non_blank(text) else {
        return false;
    };
    // A parsed `authenticated: true` / `loggedIn: true` (object or raw
    // text) is authoritative proof of login.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(value) {
        if parsed.get("authenticated") == Some(&serde_json::Value::Bool(true))
            || parsed.get("loggedIn") == Some(&serde_json::Value::Bool(true))
        {
            return false;
        }
        if parsed.get("authenticated") == Some(&serde_json::Value::Bool(false))
            || parsed.get("loggedIn") == Some(&serde_json::Value::Bool(false))
        {
            return true;
        }
    }
    static TRUE_RE: OnceLock<Regex> = OnceLock::new();
    let true_re = TRUE_RE.get_or_init(|| {
        Regex::new("(?i)\"authenticated\"\\s*:\\s*true|\"loggedIn\"\\s*:\\s*true")
            .expect("claude true regex")
    });
    if true_re.is_match(value) {
        return false;
    }
    static FALSE_RE: OnceLock<Regex> = OnceLock::new();
    let false_re = FALSE_RE.get_or_init(|| {
        Regex::new("(?i)\"authenticated\"\\s*:\\s*false|\"loggedIn\"\\s*:\\s*false|not authenticated|not logged[ _-]?in|authentication required|please (?:sign|log)[ _-]?in")
            .expect("claude false regex")
    });
    false_re.is_match(value)
}

// ---- tailored classifier ----------------------------------------------------

/// Agents with bespoke auth-failure matching (parity: `TAILORED_AUTH_AGENTS`).
pub fn has_tailored_auth_classifier(agent_id: &str) -> bool {
    matches!(
        agent_id,
        "claude" | "cursor-agent" | "deepseek" | "deepseek-harness" | "antigravity" | "reasonix"
    )
}

/// Parity: `classifyAgentAuthFailure` (auth.ts:219). `None` from a tailored
/// classifier is authoritative ("authenticated") and must not be
/// second-guessed by the generic matcher.
pub fn classify_agent_auth_failure(agent_id: &str, text: &str) -> Option<AuthProbeResult> {
    let missing = |message: String| {
        Some(AuthProbeResult {
            status: AuthProbeStatus::Missing,
            message: Some(message),
            stdout_tail: None,
            stderr_tail: None,
            exit_code: None,
            signal: None,
        })
    };
    match agent_id {
        "claude" if is_claude_auth_failure_text(text) => missing(claude_auth_guidance().to_string()),
        "cursor-agent" if is_cursor_auth_failure_text(text) => {
            missing(cursor_auth_guidance().to_string())
        }
        "deepseek" if is_deepseek_auth_failure_text(text) => {
            missing(deepseek_auth_guidance().to_string())
        }
        "deepseek-harness" if is_deepseek_auth_failure_text(text) => {
            missing(deepseek_harness_auth_guidance().to_string())
        }
        "antigravity" if is_antigravity_auth_failure_text(text) => {
            missing(antigravity_auth_guidance().to_string())
        }
        "reasonix" if is_reasonix_auth_failure_text(text) => {
            missing(reasonix_auth_guidance().to_string())
        }
        _ => None,
    }
}

/// Parity: `normalizeDeepSeekHarnessFailure` (auth.ts:172). Never coerces a
/// non-string `message` to text (avoids `[object Object]` leaks).
pub fn normalize_deepseek_harness_failure(
    code: Option<&str>,
    message: Option<serde_json::Value>,
    nested_code: Option<&str>,
    nested_message: Option<serde_json::Value>,
) -> (String, Option<String>, bool) {
    let code = code
        .filter(|c| !c.trim().is_empty())
        .or_else(|| nested_code.filter(|c| !c.trim().is_empty()))
        .unwrap_or("AGENT_EXECUTION_FAILED")
        .to_string();
    let message = message
        .and_then(|v| v.as_str().map(str::to_string))
        .or_else(|| nested_message.and_then(|v| v.as_str().map(str::to_string)));
    let auth_required =
        is_deepseek_auth_failure_text(&format!("{code}\n{}", message.as_deref().unwrap_or("")));
    let message = if auth_required {
        Some(deepseek_harness_auth_guidance().to_string())
    } else {
        message.or_else(|| Some("DeepSeek Harness profile error.".to_string()))
    };
    (code, message, auth_required)
}

// ---- probe runner ------------------------------------------------------------

fn has_non_empty_env(env: &HashMap<String, String>, keys: &[&str]) -> bool {
    keys.iter().any(|key| {
        env.get(*key).map(|v| !v.trim().is_empty()).unwrap_or(false)
    })
}

fn has_claude_enterprise_provider_auth(env: &HashMap<String, String>) -> bool {
    ["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX"]
        .iter()
        .any(|key| env.get(*key).map(|v| v == "1").unwrap_or(false))
}

/// Parity: `hasProbeSatisfyingAuth` (auth.ts:563) — API-key / enterprise
/// short-circuits that skip the probe spawn entirely.
fn has_probe_satisfying_auth(agent_id: &str, env: &HashMap<String, String>) -> bool {
    match agent_id {
        "codex" => has_non_empty_env(env, &["CODEX_API_KEY", "OPENAI_API_KEY"]),
        "claude" => {
            has_non_empty_env(env, &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"])
                || has_claude_enterprise_provider_auth(env)
        }
        _ => false,
    }
}

/// Read the selected Codex provider's `env_key` from `config.toml`
/// (parity: `readCodexProviderEnvKey` in codex-config-normalize.ts).
/// Returns the key name when set (value presence is checked by the caller).
fn read_codex_provider_env_key(env: &HashMap<String, String>) -> Option<String> {
    let home = env
        .get("CODEX_HOME")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| format!("{home}/.codex"))
        })?;
    let toml = std::fs::read_to_string(format!("{home}/config.toml")).ok()?;
    extract_codex_provider_env_key(&toml)
}

fn extract_codex_provider_env_key(toml: &str) -> Option<String> {
    // Selected provider from the top-level `model_provider = "..."` line.
    let mut provider: Option<String> = None;
    for line in toml.lines() {
        let cleaned = strip_toml_comment(line);
        let stripped = cleaned.trim();
        if let Some(value) = match_quoted_value(stripped, "model_provider") {
            provider = Some(value);
        }
    }
    let provider = provider?;
    // Walk `[model_providers.<provider>]` for its `env_key`.
    let mut in_table = false;
    for line in toml.lines() {
        let cleaned = strip_toml_comment(line);
        let stripped = cleaned.trim();
        if stripped.starts_with('[') {
            in_table = table_header_name(stripped)
                .map(|name| name == provider)
                .unwrap_or(false);
            continue;
        }
        if in_table {
            if let Some(value) = match_quoted_value(stripped, "env_key") {
                return Some(value);
            }
        }
    }
    None
}

fn strip_toml_comment(line: &str) -> String {
    let mut out = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in line.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => break,
            _ => {}
        }
        out.push(ch);
    }
    out
}

fn table_header_name(line: &str) -> Option<String> {
    // `[model_providers."name"]`, `[model_providers.'name']`, or bare.
    let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
    let dotted = inner.strip_prefix("model_providers.")?.trim();
    if let Some(name) = dotted.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(name.to_string());
    }
    if let Some(name) = dotted.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Some(name.to_string());
    }
    if dotted.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        return Some(dotted.to_string());
    }
    None
}

fn match_quoted_value(line: &str, key: &str) -> Option<String> {
    let (lhs, rhs) = line.split_once('=')?;
    if lhs.trim() != key {
        return None;
    }
    let rhs = rhs.trim();
    if let Some(inner) = rhs.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(inner.to_string());
    }
    if let Some(inner) = rhs.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Some(inner.to_string());
    }
    None
}

const PROBE_TAIL_BYTES: usize = 400;

fn tail_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() > PROBE_TAIL_BYTES {
        let start = trimmed.len() - PROBE_TAIL_BYTES;
        return Some(trimmed[start..].to_string());
    }
    Some(trimmed.to_string())
}

fn with_probe_tails(
    mut base: AuthProbeResult,
    stdout_text: &str,
    stderr_text: &str,
) -> AuthProbeResult {
    base.stdout_tail = tail_string(stdout_text);
    base.stderr_tail = tail_string(stderr_text);
    base
}

/// Run an adapter's declared auth probe and classify the result (parity:
/// `probeAgentAuthStatus`, auth.ts:597). `None` when the def declares no
/// probe — those agents are never actively probed.
pub async fn probe_agent_auth_status(
    agent_id: &str,
    agent_name: &str,
    auth_probe: Option<(&[&str], u64)>,
    resolved_bin: &std::path::Path,
    env: &HashMap<String, String>,
) -> Option<AuthProbeResult> {
    let (args, timeout_ms) = auth_probe?;
    if has_probe_satisfying_auth(agent_id, env) {
        return Some(AuthProbeResult {
            status: AuthProbeStatus::Ok,
            message: None,
            stdout_tail: None,
            stderr_tail: None,
            exit_code: None,
            signal: None,
        });
    }
    // Codex custom providers authenticate via a provider-specific env key
    // even when `codex login status` (a ChatGPT-login check) fails.
    if agent_id == "codex" {
        if let Some(key) = read_codex_provider_env_key(env) {
            if has_non_empty_env(env, &[key.as_str()]) {
                return Some(AuthProbeResult {
                    status: AuthProbeStatus::Ok,
                    message: None,
                    stdout_tail: None,
                    stderr_tail: None,
                    exit_code: None,
                    signal: None,
                });
            }
        }
    }
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let timeout = std::time::Duration::from_millis(timeout_ms);
    match run_probe(resolved_bin, &argv, env, timeout).await {
        Ok(ProbeOutput {
            stdout,
            stderr,
            success: _,
        }) => {
            let output = format!("{stdout}\n{stderr}");
            if has_tailored_auth_classifier(agent_id) {
                match classify_agent_auth_failure(agent_id, &output) {
                    Some(failure) => Some(with_probe_tails(failure, &stdout, &stderr)),
                    None => Some(AuthProbeResult {
                        status: AuthProbeStatus::Ok,
                        message: None,
                        stdout_tail: None,
                        stderr_tail: None,
                        exit_code: None,
                        signal: None,
                    }),
                }
            } else {
                // Generic fallback for adapters without a tailored
                // classifier: unknown either way without the service-failure
                // classifier (follow-up bead).
                Some(with_probe_tails(
                    AuthProbeResult {
                        status: AuthProbeStatus::Unknown,
                        message: Some(generic_auth_guidance(agent_name)),
                        stdout_tail: None,
                        stderr_tail: None,
                        exit_code: None,
                        signal: None,
                    },
                    &stdout,
                    &stderr,
                ))
            }
        }
        Err(err) => Some(AuthProbeResult {
            status: AuthProbeStatus::Unknown,
            message: Some(format!("auth probe failed to spawn: {err}")),
            stdout_tail: None,
            stderr_tail: None,
            exit_code: None,
            signal: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates_match_documented_shapes() {
        assert!(is_cursor_auth_failure_text("Error: authentication required"));
        assert!(is_cursor_auth_failure_text("CURSOR_API_KEY is missing"));
        assert!(!is_cursor_auth_failure_text("all good"));
        assert!(!is_cursor_auth_failure_text(""));
        assert!(is_antigravity_auth_failure_text(
            "Error: authentication required. Run 'agy' to log in, then retry."
        ));
        assert!(!is_antigravity_auth_failure_text("done"));
        assert!(is_deepseek_auth_failure_text("DSH_PROVIDER_AUTH_FAILED"));
        assert!(is_deepseek_auth_failure_text("see ~/.deepseek/config.toml api_key"));
        assert!(!is_deepseek_auth_failure_text("ok"));
        assert!(is_reasonix_auth_failure_text(
            "missing api_key in ~/.reasonix/config.json"
        ));
        assert!(!is_reasonix_auth_failure_text("ok"));
        assert!(is_claude_auth_failure_text("please log in"));
        assert!(is_claude_auth_failure_text(r#"{"authenticated":false}"#));
        assert!(!is_claude_auth_failure_text(r#"{"authenticated":true}"#));
        assert!(!is_claude_auth_failure_text("ok"));
    }

    #[test]
    fn tailored_classifier_is_authoritative() {
        // Null from a tailored classifier means authenticated — never falls
        // through to generic matching.
        assert!(classify_agent_auth_failure("cursor-agent", "all good").is_none());
        let failure = classify_agent_auth_failure("cursor-agent", "not logged in")
            .expect("failure");
        assert_eq!(failure.status, AuthProbeStatus::Missing);
        assert!(failure.message.unwrap_or_default().contains("cursor-agent login"));
    }

    #[test]
    fn codex_provider_env_key_parses() {
        let toml = "model_provider = \"azure\"\n[model_providers.azure]\nenv_key = \"AZURE_OPENAI_API_KEY\"\n";
        assert_eq!(
            extract_codex_provider_env_key(toml).as_deref(),
            Some("AZURE_OPENAI_API_KEY")
        );
        assert_eq!(extract_codex_provider_env_key("nope = 1\n"), None);
    }

    #[test]
    fn tails_truncate() {
        assert_eq!(tail_string("  hi  ").as_deref(), Some("hi"));
        assert_eq!(tail_string("   ").as_deref(), None);
        let long = "x".repeat(500);
        assert_eq!(tail_string(&long).map(|s| s.len()), Some(400));
    }

    #[tokio::test]
    async fn probe_runner_reports_ok_for_clean_output() {
        let dir = std::env::temp_dir().join(format!("od-auth-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let bin = dir.join("cursor-agent");
        std::fs::write(&bin, "#!/bin/sh\necho '{\"loggedIn\":true}'\n").expect("shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).expect("perms");
        }
        let result = probe_agent_auth_status(
            "cursor-agent",
            "Cursor Agent",
            Some((&["status"], 5_000)),
            &bin,
            &HashMap::new(),
        )
        .await
        .expect("result");
        // `loggedIn: true` is authoritative proof of login.
        assert_eq!(result.status, AuthProbeStatus::Ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn probe_runner_reports_missing_for_auth_failure() {
        let dir = std::env::temp_dir().join(format!("od-auth-probe2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let bin = dir.join("cursor-agent");
        std::fs::write(&bin, "#!/bin/sh\necho 'not logged in' >&2\nexit 1\n").expect("shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).expect("perms");
        }
        let result = probe_agent_auth_status(
            "cursor-agent",
            "Cursor Agent",
            Some((&["status"], 5_000)),
            &bin,
            &HashMap::new(),
        )
        .await
        .expect("result");
        assert_eq!(result.status, AuthProbeStatus::Missing);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
