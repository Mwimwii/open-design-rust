//! Agent authentication probing — Rust parity port of
//! `apps/daemon/src/runtimes/auth.ts` (guidance strings, failure-text
//! predicates, `classifyAgentAuthFailure`, `classifyAgentServiceFailure`,
//! `reportsToolPrincipalAuthFailure`, and `probeAgentAuthStatus`).

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

pub fn antigravity_quota_guidance() -> &'static str {
    "Antigravity returned \"RESOURCE_EXHAUSTED: Individual quota reached\" for the current model. Each Antigravity model (Gemini 3 Pro / Flash, Claude 4.6, GPT-OSS) has its own quota.\n\nFix: open `agy` in a terminal and use its Switch Model picker (the menu at the bottom of the TUI) to pick a model with available quota, then retry here. OpenDesign uses whatever model you pick in agy's TUI when the Settings model picker is left on \"Default\". Quotas reset automatically on Antigravity's schedule."
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

// ---- service-failure classification (parity: auth.ts STATUS_CTX block) ------

/// Model-service failure classes that map a CLI agent's raw error text to a
/// structured API error code (parity: `AgentServiceFailureCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentServiceFailureCode {
    AgentAuthRequired,
    RateLimited,
    UpstreamUnavailable,
}

impl AgentServiceFailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentServiceFailureCode::AgentAuthRequired => "AGENT_AUTH_REQUIRED",
            AgentServiceFailureCode::RateLimited => "RATE_LIMITED",
            AgentServiceFailureCode::UpstreamUnavailable => "UPSTREAM_UNAVAILABLE",
        }
    }
}

/// A bare HTTP status number is too noisy to trust on its own — agent stderr
/// is full of unrelated numbers (`line 500`, `read 502 bytes`, `exit code
/// 401`). Only treat one as a signal when it carries explicit HTTP-status
/// context. `code` alone is NOT enough; it only counts when qualified
/// (status/error/response code) or immediately followed by `:`/`=`/`#`.
///
/// The TS source writes `\bcode(?=\s*[:=#])`; `regex` has no lookahead, so the
/// separator is consumed here instead (`\bcode\s*[:=#]`), which is equivalent
/// because the whole context is followed by `[\s:=#-]*` anyway.
const STATUS_CTX: &str = concat!(
    r"(?:\bhttp(?:[ /]?[0-9](?:\.[0-9])?)?\b",
    r"|\b(?:status|error|response)(?:[ _-]?code)?\b",
    r"|\bcode\s*[:=#]",
    r"|\b(?:server|http)[ _-]?error\b)",
    r"[\s:=#-]*"
);

/// Parity: `AGENT_AUTH_FAILURE_RE` — a missing, invalid, or expired credential.
fn agent_auth_failure_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            concat!(
                r"(?i)(\b(unauthor(?:ized|ised)|authenticat(?:e|ed|ion)",
                r"|invalid[ _-]?(?:api[ _-]?)?key|incorrect api key|no api key|x-api-key",
                r"|missing[ _-]?credentials?|not (?:authenticated|logged[ _-]?in)",
                r"|please (?:sign|log)[ _-]?in|oauth token (?:has )?expired|session expired",
                r"|credentials? (?:are )?(?:missing|invalid|required))\b",
                r"|/login\b|{}401\b)"
            ),
            STATUS_CTX
        ))
        .expect("agent auth failure regex")
    })
}

/// Parity: `AGENT_RATE_FAILURE_RE` — quota / rate limit / billing balance.
fn agent_rate_failure_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            concat!(
                r"(?i)(\b(rate[ _-]?limit|too many requests|quota",
                r"|insufficient[ _-]?(?:quota|balance|credit|funds)",
                r"|credit balance is too low|exceeded your current quota|usage limit",
                r"|session limit|limit reached|billing (?:hard )?limit)\b",
                r"|{}429\b)"
            ),
            STATUS_CTX
        ))
        .expect("agent rate failure regex")
    })
}

/// Parity: `AGENT_UPSTREAM_FAILURE_RE` — overloaded / 5xx / unavailable.
fn agent_upstream_failure_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            concat!(
                r"(?i)(\b(overloaded(?:_error)?|service (?:is )?(?:temporarily )?unavailable",
                r"|bad gateway|gateway timeout|internal server error",
                r"|upstream (?:error|unavailable)|provider (?:error|unavailable)",
                r"|temporarily unavailable|model is currently overloaded|5xx)\b",
                r"|{}5[0-9][0-9]\b",
                r"|\b5[0-9][0-9]\s+(?:bad gateway|service unavailable|internal server error|gateway timeout))"
            ),
            STATUS_CTX
        ))
        .expect("agent upstream failure regex")
    })
}

/// Parity: `TOOL_INVOCATION_MARKER` — the agent saying a tool it ran failed.
fn tool_invocation_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)\btool[_ -](?:error|use|call|result)\b",
            r"|\btool\s+[A-Za-z0-9_./-]+\s+(?:failed|error)\b",
            r"|\bmcp_auth_required\b|\b(?:mcp|connector|plugin)\b"
        ))
        .expect("tool invocation marker regex")
    })
}

/// Parity: `PROGRAM_DIAGNOSTIC_PREFIX` — the Unix `progname: message`
/// convention plus npm's `npm ERR!` variant.
fn program_diagnostic_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[ \t]*([A-Za-z][A-Za-z0-9_.+-]{0,31})(?::[ \t]|[ \t]ERR!)")
            .expect("program diagnostic prefix regex")
    })
}

/// Parity: `SEVERITY_LABELS` — a label says how bad a line is, not who wrote it.
fn severity_labels() -> &'static std::collections::HashSet<&'static str> {
    static SET: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "err", "error", "errors", "warn", "warning", "fatal", "panic", "critical", "crit",
            "note", "notice", "info", "debug", "trace", "verbose",
        ]
        .into_iter()
        .collect()
    })
}

/// Parity: `OWN_AGENT_COMMAND_NAMES` — every command name Open Design's own
/// agent CLIs answer to (each shipped adapter's `id` and its `bin`). A row the
/// text reports about a name in here is the agent speaking as itself, not a
/// tool inside it. Kept as a literal because the adapter registry imports this
/// module.
pub fn own_agent_command_names() -> &'static std::collections::HashSet<&'static str> {
    static SET: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "agy", "aider", "amp", "amr", "antigravity", "atomcode", "byok-opencode", "claude",
            "codebuddy", "codex", "copilot", "cursor-agent", "deepseek", "deepseek-harness",
            "devin", "dsh", "grok", "grok-build", "hermes", "kilo", "kimi", "kiro", "kiro-cli",
            "mimo", "opencode", "opencode-cli", "pi", "qoder", "qodercli", "qwen", "reasonix",
            "trae-cli", "traecli", "vela", "vibe", "vibe-acp",
        ]
        .into_iter()
        .collect()
    })
}

/// True when a gateway's own provider-credential code says the PLATFORM's
/// upstream credentials failed (parity:
/// `reportsPlatformProviderCredentialFault`, vela-errors.ts:183).
///
/// Rust's `regex` has no lookbehind, so the `(?<![\w.-])…(?![\w.-])` boundary
/// is checked by hand.
pub fn reports_platform_provider_credential_fault(text: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)upstream_provider_(?:unauthenticated|forbidden)")
            .expect("vela credential code regex")
    });
    let boundary = |ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'));
    for found in re.find_iter(text) {
        let before_ok = text[..found.start()].chars().next_back().is_none_or(boundary);
        let after_ok = text[found.end()..].chars().next().is_none_or(boundary);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// True when the auth failure `text` reports belongs to a TOOL the agent ran,
/// rather than to the agent's own credential (parity:
/// `reportsToolPrincipalAuthFailure`, auth.ts:437).
///
/// Scoped per line: an unrelated `npm ERR!` elsewhere in a long stderr tail
/// must not vouch for the agent's own 401 on another line.
pub fn reports_tool_principal_auth_failure(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    for raw_line in text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if !agent_auth_failure_re().is_match(line) {
            continue;
        }
        if tool_invocation_marker_re().is_match(line) {
            return true;
        }
        let Some(captures) = program_diagnostic_prefix_re().captures(line) else {
            continue;
        };
        let speaker = captures
            .get(1)
            .map(|capture| capture.as_str().to_lowercase())
            .unwrap_or_default();
        if severity_labels().contains(speaker.as_str()) {
            continue;
        }
        if agent_auth_failure_re().is_match(&speaker) {
            continue;
        }
        if own_agent_command_names().contains(speaker.as_str()) {
            continue;
        }
        let prefix_end = captures.get(0).map(|capture| capture.end()).unwrap_or(0);
        if agent_auth_failure_re().is_match(&line[prefix_end..]) {
            return true;
        }
    }
    false
}

/// The model-service failure class implied by an agent's combined
/// stdout/stderr/error text, or `None` for an ordinary process failure
/// (parity: `classifyAgentServiceFailure`, auth.ts:465).
///
/// Auth is checked before rate/upstream so a `401` is never misread as `5xx`.
pub fn classify_agent_service_failure(text: &str) -> Option<AgentServiceFailureCode> {
    if text.trim().is_empty() {
        return None;
    }
    // A self-identifying machine code outranks a class read off prose.
    if reports_platform_provider_credential_fault(text) {
        return Some(AgentServiceFailureCode::UpstreamUnavailable);
    }
    // The auth class is a claim about THIS agent's credential. A report that
    // names a different holder (a tool the agent ran) answers a different
    // question, so it does not reach the auth branch. Rate and upstream stay
    // global: a tool that hit a 503 or 429 still classifies.
    if !reports_tool_principal_auth_failure(text) && agent_auth_failure_re().is_match(text) {
        return Some(AgentServiceFailureCode::AgentAuthRequired);
    }
    if agent_rate_failure_re().is_match(text) {
        return Some(AgentServiceFailureCode::RateLimited);
    }
    if agent_upstream_failure_re().is_match(text) {
        return Some(AgentServiceFailureCode::UpstreamUnavailable);
    }
    None
}

/// Parity: `classifyProbedAuthFailure` (auth.ts:579) — agents with a tailored
/// classifier use only that (`None` === authenticated); everyone else falls
/// back to the generic HTTP/text classifier.
fn classify_probed_auth_failure(
    classifier_id: &str,
    agent_name: &str,
    text: &str,
) -> Option<AuthProbeResult> {
    if has_tailored_auth_classifier(classifier_id) {
        return classify_agent_auth_failure(classifier_id, text);
    }
    if classify_agent_service_failure(text) == Some(AgentServiceFailureCode::AgentAuthRequired) {
        return Some(AuthProbeResult {
            status: AuthProbeStatus::Missing,
            message: Some(generic_auth_guidance(agent_name)),
            stdout_tail: None,
            stderr_tail: None,
            exit_code: None,
            signal: None,
        });
    }
    None
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
            success,
            exit_code,
        }) => {
            let output = format!("{stdout}\n{stderr}");
            // Parity: the tailored classifier is authoritative for its agents
            // (a `None` there means "authenticated"); everyone else falls back
            // to the generic HTTP/text classifier.
            if let Some(failure) =
                classify_probed_auth_failure(agent_id, agent_name, &output)
            {
                let mut failure = with_probe_tails(failure, &stdout, &stderr);
                failure.exit_code = if success { Some(0) } else { exit_code };
                return Some(failure);
            }
            if success {
                return Some(AuthProbeResult {
                    status: AuthProbeStatus::Ok,
                    message: None,
                    stdout_tail: None,
                    stderr_tail: None,
                    exit_code: None,
                    signal: None,
                });
            }
            // A non-zero probe exit the classifier cannot name is reported as
            // unverifiable, never as a healthy install.
            let mut result = AuthProbeResult {
                status: AuthProbeStatus::Unknown,
                message: Some(format!(
                    "{agent_name} authentication status could not be verified with `{}`.",
                    args.join(" ")
                )),
                stdout_tail: None,
                stderr_tail: None,
                exit_code,
                signal: None,
            };
            result = with_probe_tails(result, &stdout, &stderr);
            Some(result)
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

    // Parity: apps/daemon/tests/runtimes/service-failure-classification.test.ts.
    #[test]
    fn service_failure_classification_matches_typescript() {
        let classify = |text: &str| classify_agent_service_failure(text).map(|c| c.as_str());

        // The official DeepSeek Harness missing-credential failure.
        assert_eq!(
            classify("dsh: MISSING_CREDENTIAL: llm-deepseek: no API key for provider route \"deepseek-official\"; export DEEPSEEK_API_KEY"),
            Some("AGENT_AUTH_REQUIRED")
        );

        for text in [
            "Error: 401 {\"type\":\"authentication_error\",\"message\":\"invalid x-api-key\"}",
            "Incorrect API key provided: sk-***. ",
            "Please run /login to authenticate.",
            "Unauthorized: OAuth token has expired",
        ] {
            assert_eq!(classify(text), Some("AGENT_AUTH_REQUIRED"), "{text}");
        }

        for text in [
            "Error: 429 Too Many Requests",
            "rate_limit_error: rate limit exceeded",
            "You exceeded your current quota, please check your plan and billing details.",
            "Your credit balance is too low to access the Anthropic API.",
            "insufficient_quota",
        ] {
            assert_eq!(classify(text), Some("RATE_LIMITED"), "{text}");
        }

        for text in [
            "Error: 529 {\"type\":\"overloaded_error\"}",
            "Service temporarily unavailable (503)",
            "Bad gateway",
            "The model is currently overloaded. Please try again later.",
            "HTTP 500 from provider",
            "status 503",
            "server error 502",
            "502 Bad Gateway",
        ] {
            assert_eq!(classify(text), Some("UPSTREAM_UNAVAILABLE"), "{text}");
        }

        assert_eq!(classify("HTTP 401 Unauthorized"), Some("AGENT_AUTH_REQUIRED"));
        assert_eq!(classify("status code 429"), Some("RATE_LIMITED"));
        // Auth is checked first, so a 401 beside a 503 is still auth.
        assert_eq!(
            classify("401 unauthorized — also saw a 503 earlier"),
            Some("AGENT_AUTH_REQUIRED")
        );
    }

    /// R-053: a gateway's own provider-credential code is a service outage, not
    /// the caller having signed out.
    #[test]
    fn platform_provider_credential_codes_are_upstream_not_auth() {
        for text in [
            "API request failed with status 500: upstream_provider_unauthenticated",
            "[code=upstream_provider_unauthenticated] Upstream provider credentials are missing or invalid.",
            "[code=upstream_provider_forbidden] Upstream provider rejected access for the configured credentials.",
            "json-rpc id 4: opencode event stream: {\"properties\":{\"error\":{\"data\":{\"message\":\"\\\"[code=upstream_provider_unauthenticated] Upstream provider credentials are missing or invalid.\\\"\"}}}}",
        ] {
            assert_eq!(
                classify_agent_service_failure(text).map(|c| c.as_str()),
                Some("UPSTREAM_UNAVAILABLE"),
                "{text}"
            );
        }

        // The reverse guard: the caller's own bare `unauthenticated` code stays auth.
        for text in [
            "API request failed with status 401: unauthenticated",
            "API request failed with status 401: invalid_api_key",
            "API request failed with status 401: missing_api_key",
            "HTTP 401 Unauthorized",
            "Error: 401 {\"type\":\"authentication_error\",\"message\":\"invalid x-api-key\"}",
        ] {
            assert_eq!(
                classify_agent_service_failure(text).map(|c| c.as_str()),
                Some("AGENT_AUTH_REQUIRED"),
                "{text}"
            );
        }

        // A longer identifier that merely contains the code is not the code.
        assert_eq!(
            classify_agent_service_failure("code=upstream_provider_unauthenticatedx")
                .map(|c| c.as_str()),
            None
        );
    }

    /// A tool inside the agent reporting its OWN credential failure must not
    /// become the agent's sign-in prompt.
    #[test]
    fn tool_principal_auth_failures_are_not_the_agents_credential() {
        assert!(reports_tool_principal_auth_failure(
            "gh: authentication required: run gh auth login"
        ));
        // An unrelated tool line does not vouch for the agent's own 401 on
        // another line.
        assert!(!reports_tool_principal_auth_failure(
            "Error: 401 unauthorized\nnpm ERR! code ELIFECYCLE\nnpm ERR! errno 1"
        ));
        // Our own CLI speaking as itself is not a foreign tool.
        assert!(!reports_tool_principal_auth_failure(
            "dsh: MISSING_CREDENTIAL: no API key for provider route"
        ));
        // A severity label or a status reason phrase is a label, not a speaker.
        assert!(!reports_tool_principal_auth_failure(
            "Error: list Link models: API request failed with status 401: invalid_api_key"
        ));
        // A tool's own auth failure is not the agent's, so it does not reach the
        // auth branch — and there is no rate/upstream signal either.
        assert_eq!(
            classify_agent_service_failure("gh: authentication required: run gh auth login")
                .map(|c| c.as_str()),
            None
        );
        // The tool's own 401 is not auth, but an unrelated 429 still is a rate limit.
        assert_eq!(
            classify_agent_service_failure("gh: authentication required\nHTTP 429 Too Many Requests")
                .map(|c| c.as_str()),
            Some("RATE_LIMITED")
        );
    }

    #[test]
    fn service_failure_returns_none_for_ordinary_failures_and_bare_numbers() {
        for text in [
            "",
            "spawn ENOENT",
            "Segmentation fault (core dumped)",
            "TypeError: cannot read properties of undefined",
            "Compiled 500 modules in 503ms; read 502 bytes at line 529",
            "Build failed at line 500 (exit code 1)",
            "Processed 4290 rows, 401 skipped, took 4290ms",
            "wrote 502 files",
            "exit code 401",
            "process exited with code 429",
            "command failed: exit code 503",
            "child process exited with code 500",
        ] {
            assert!(
                classify_agent_service_failure(text).is_none(),
                "{text}"
            );
        }
    }

    /// The generic (non-tailored) probe path now classifies auth instead of
    /// always reporting `unknown` (auth.ts `classifyProbedAuthFailure`).
    #[tokio::test]
    async fn generic_probe_classifies_auth_via_the_service_classifier() {
        let dir = std::env::temp_dir().join(format!("od-auth-probe3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let bin = dir.join("aider");
        std::fs::write(&bin, "#!/bin/sh\necho 'HTTP 401 Unauthorized' >&2\nexit 1\n")
            .expect("shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).expect("perms");
        }
        let result = probe_agent_auth_status(
            "aider",
            "Aider",
            Some((&["status"], 5_000)),
            &bin,
            &HashMap::new(),
        )
        .await
        .expect("result");
        assert_eq!(result.status, AuthProbeStatus::Missing);
        assert_eq!(result.exit_code, Some(1));
        assert!(result
            .message
            .unwrap_or_default()
            .contains("appears to be installed"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
