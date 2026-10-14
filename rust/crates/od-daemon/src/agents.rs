//! Agent runtime definitions — Rust parity port of
//! `apps/daemon/src/runtimes/defs/*`, `registry.ts`, and the model helpers in
//! `apps/daemon/src/runtimes/models.ts`.
//!
//! Each shipped adapter becomes an [`AgentDef`]: static metadata plus a
//! `build_args` function (parity for the `buildArgs` closure in each def).
//! Capability probes (`agentCapabilities` in TS) are passed in by the caller
//! as a plain map instead of read from a process-global.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Parity: `RuntimeModelOption` (`runtimes/types.ts:8`) — only the fields
/// the daemon reads when validating picks.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub default: bool,
    pub service_tier_options: Vec<ModelOption>,
    pub reasoning_options: Vec<ModelOption>,
}

impl ModelOption {
    pub fn simple(id: &str, label: &str) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            enabled: true,
            default: false,
            service_tier_options: Vec::new(),
            reasoning_options: Vec::new(),
        }
    }
}

/// Parity: `DEFAULT_MODEL_OPTION` (`runtimes/models.ts`).
pub fn default_model_option() -> ModelOption {
    ModelOption {
        id: "default".to_string(),
        label: "Default (CLI config)".to_string(),
        enabled: true,
        default: false,
        service_tier_options: Vec::new(),
        reasoning_options: Vec::new(),
    }
}

/// Parity: `RuntimeBuildOptions` (`runtimes/types.ts:25`).
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub service_tier: Option<String>,
}

/// The `RuntimeContext` fields adapter `buildArgs` implementations read
/// (parity: `runtimes/types.ts:31`). Platform comes from the caller so unit
/// tests can pin it (`NodeJS.Platform` spelling: `win32` / `darwin` / `linux`).
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    pub cwd: Option<String>,
    pub resume_session_id: Option<String>,
    pub new_session_id: Option<String>,
    pub observe_native_child_behavior: bool,
    pub disable_plugins: bool,
    pub codex_disable_plugins_env: bool,
    pub has_prior_assistant_turn: bool,
    pub platform: String,
    pub env: HashMap<String, String>,
    /// Parity: `RuntimeContext.agentLogFilePath`.
    pub agent_log_file_path: Option<String>,
    /// Parity: `RuntimeContext.antigravitySettingsPath` (test override).
    pub antigravity_settings_path: Option<String>,
    /// Home directory for adapters resolving `~/` paths.
    pub home_dir: Option<String>,
}

/// Input to every `build_args` implementation.
pub struct BuildArgsInput<'a> {
    pub caps: &'a HashMap<String, bool>,
    pub options: &'a BuildOptions,
    pub extra_allowed_dirs: &'a [String],
    pub context: &'a AgentContext,
    pub live: &'a LiveModelCache,
    pub live_scope: Option<&'a str>,
    /// The composed prompt text (only read by argv-prompt adapters).
    pub prompt: &'a str,
    /// Daemon-created temp file holding the prompt (file-prompt adapters).
    pub prompt_file_path: Option<&'a str>,
    /// Image attachment paths (adapters that forward them as flags).
    pub image_paths: &'a [String],
}

/// Parity: `RuntimeAgentDef` (`runtimes/types.ts:145`) — static subset.
/// Function-valued TS fields that need live I/O (`fetchModels`,
/// `encodeStdinPrompt`) are not ported; `listModels` parse helpers that are
/// pure (e.g. Codebuddy's help-text catalog) are ported as free functions.
pub struct AgentDef {
    pub id: &'static str,
    pub name: &'static str,
    pub bin: &'static str,
    pub fallback_bins: &'static [&'static str],
    pub version_args: &'static [&'static str],
    pub stream_format: &'static str,
    pub event_parser: Option<&'static str>,
    pub fallback_models: Vec<ModelOption>,
    pub reasoning_options: Vec<ModelOption>,
    pub prompt_via_stdin: bool,
    pub prompt_via_file: bool,
    pub prompt_input_format: Option<&'static str>,
    pub supports_image_paths: bool,
    /// `None` means "unset", which TS treats as allowed.
    pub supports_custom_model: Option<bool>,
    pub external_mcp_injection: Option<&'static str>,
    pub install_url: Option<&'static str>,
    pub docs_url: Option<&'static str>,
    pub help_args: &'static [&'static str],
    pub capability_flags: &'static [(&'static str, &'static str)],
    pub resumes_session_via_cli: bool,
    pub captures_session_id_from_stream: bool,
    pub default_model_env_var: Option<&'static str>,
    pub inactivity_timeout_ms: Option<u64>,
    pub first_output_timeout_ms: Option<u64>,
    /// Parity: `authProbe` — (argv, timeout_ms).
    pub auth_probe: Option<(&'static [&'static str], u64)>,
    /// Parity: `maxPromptArgBytes`.
    pub max_prompt_arg_bytes: Option<usize>,
    /// Parity: `mcpDiscovery`.
    pub mcp_discovery: Option<&'static str>,
    /// Parity: `acpTurnEndCompletesPrompt`.
    pub acp_turn_end_completes_prompt: bool,
    /// Parity: `acpStdioMcpRemovedInVersion`.
    pub acp_stdio_mcp_removed_in_version: Option<&'static str>,
    /// Parity: `acpMcpEnvFormat`.
    pub acp_mcp_env_format: Option<&'static str>,
    /// Parity: `versionProbeTimeoutMs`.
    pub version_probe_timeout_ms: Option<u64>,
    /// Parity: `resumesSessionViaAcpLoad`.
    pub resumes_session_via_acp_load: bool,
    /// Parity: `encodeStdinPrompt` — whole-prompt stdin encoding.
    pub encode_stdin_prompt: Option<fn(&str) -> String>,
    /// Parity: `resumesSessionViaProfileStdio`.
    pub resumes_session_via_profile_stdio: bool,
    /// Parity: `versionPolicy` — (supported versions, pattern source).
    /// Semver evaluation belongs to the detection bead.
    pub version_policy: Option<(&'static [&'static str], &'static str)>,
    /// Parity: `compatibilityProbe` argv + timeout. Probe parsing belongs
    /// to the protocol bead.
    pub compatibility_probe: Option<(&'static [&'static str], u64)>,
    /// Static spawn-env entries (parity: `env`). Runtime-computed entries
    /// (e.g. Reasonix's home dir) resolve in the spawn layer.
    pub env: &'static [(&'static str, &'static str)],
    pub build_args: fn(&BuildArgsInput) -> Result<Vec<String>, String>,
}

impl AgentDef {
    /// Parity: `supportsCustomModel` defaulting (`undefined === true`).
    pub fn allows_custom_model(&self) -> bool {
        self.supports_custom_model.unwrap_or(true)
    }
}

fn shipped_defs() -> Vec<AgentDef> {
    vec![
        claude_def(),
        codebuddy_def(),
        codex_def(),
        opencode_def(),
        cursor_agent_def(),
        copilot_def(),
        aider_def(),
        amp_def(),
        atomcode_def(),
        devin_def(),
        qoder_def(),
        qwen_def(),
        byok_opencode_def(),
        hermes_def(),
        kilo_def(),
        kimi_def(),
        kiro_def(),
        trae_cli_def(),
        vibe_def(),
        mimo_def(),
        deepseek_def(),
        reasonix_def(),
        pi_def(),
        grok_build_def(),
        deepseek_harness_def(),
        amr_def(),
        antigravity_def(),
    ]
}

static REGISTRY: OnceLock<Vec<AgentDef>> = OnceLock::new();

fn registry() -> &'static Vec<AgentDef> {
    REGISTRY.get_or_init(shipped_defs)
}

/// Parity: `getAgentDef` (`runtimes/registry.ts`).
pub fn get_agent_def(id: &str) -> Option<&'static AgentDef> {
    registry().iter().find(|def| def.id == id)
}

/// Parity: `SHIPPED_AGENT_DEFS` (`runtimes/registry.ts`).
pub fn shipped_agent_defs() -> &'static [AgentDef] {
    registry()
}

// ---- live model cache (parity: liveModelCache/liveModelOrder) --------------

/// Process-side cache of the model lists last surfaced to the UI, keyed by
/// agent id (+ optional scope). The daemon owns one; callers pass it in.
#[derive(Debug, Default)]
pub struct LiveModelCache {
    order: HashMap<String, Vec<ModelOption>>,
}

fn cache_key(agent_id: &str, scope: Option<&str>) -> String {
    match scope.map(str::trim).filter(|s| !s.is_empty()) {
        Some(scope) => format!("{agent_id}\0{scope}"),
        None => agent_id.to_string(),
    }
}

impl LiveModelCache {
    /// Parity: `rememberLiveModels`.
    pub fn remember(&mut self, agent_id: &str, models: Vec<ModelOption>, scope: Option<&str>) {
        self.order.insert(cache_key(agent_id, scope), models);
    }

    /// Parity: `getRememberedLiveModels`.
    pub fn get(&self, agent_id: &str, scope: Option<&str>) -> &[ModelOption] {
        self.order
            .get(&cache_key(agent_id, scope))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

// ---- model helpers (parity: runtimes/models.ts) ----------------------------

fn find_fallback_model(def: &AgentDef, model_id: &str) -> Option<ModelOption> {
    def.fallback_models.iter().find(|m| m.id == model_id).cloned()
}

/// Parity: `findKnownModel` — live list first, then the static fallback.
pub fn find_known_model(
    def: &AgentDef,
    model_id: Option<&str>,
    live: &LiveModelCache,
    scope: Option<&str>,
) -> Option<ModelOption> {
    let model_id = model_id.filter(|id| !id.is_empty())?;
    if let Some(model) = live.get(def.id, scope).iter().find(|m| m.id == model_id).cloned() {
        return Some(model);
    }
    find_fallback_model(def, model_id)
}

/// Parity: `isKnownModel`.
pub fn is_known_model(
    def: &AgentDef,
    model_id: Option<&str>,
    live: &LiveModelCache,
    scope: Option<&str>,
) -> bool {
    find_known_model(def, model_id, live, scope).is_some()
}

/// Parity: `isKnownServiceTier`.
pub fn is_known_service_tier(
    def: &AgentDef,
    model_id: Option<&str>,
    service_tier: Option<&str>,
    live: &LiveModelCache,
    scope: Option<&str>,
) -> bool {
    let tier = service_tier.unwrap_or_default();
    if tier.is_empty() || tier == "default" {
        return false;
    }
    find_known_model(def, model_id, live, scope)
        .map(|model| model.service_tier_options.iter().any(|t| t.id == tier))
        .unwrap_or(false)
}

/// Parity: `isKnownReasoningEffort`.
pub fn is_known_reasoning_effort(
    def: &AgentDef,
    model_id: Option<&str>,
    effort: Option<&str>,
    live: &LiveModelCache,
    scope: Option<&str>,
) -> bool {
    let effort = effort.unwrap_or_default();
    if effort.is_empty() {
        return false;
    }
    if let Some(options) = find_known_model(def, model_id, live, scope)
        .and_then(|model| {
            if model.reasoning_options.is_empty() {
                None
            } else {
                Some(model.reasoning_options)
            }
        })
        .or_else(|| {
            if def.reasoning_options.is_empty() {
                None
            } else {
                Some(def.reasoning_options.clone())
            }
        })
    {
        return options.iter().any(|option| option.id == effort);
    }
    false
}

/// Parity: `resolveModelForServiceTier`.
pub fn resolve_model_for_service_tier(
    def: &AgentDef,
    model_id: Option<&str>,
    service_tier: Option<&str>,
    live: &LiveModelCache,
    scope: Option<&str>,
) -> Option<String> {
    let tier = service_tier.unwrap_or_default();
    if tier.is_empty() || tier == "default" {
        return model_id.map(str::to_string);
    }
    if is_known_service_tier(def, model_id, service_tier, live, scope) {
        return model_id.map(str::to_string);
    }
    if let Some(id) = model_id.filter(|id| *id != "default") {
        return Some(id.to_string());
    }
    let mut seen = HashSet::new();
    let candidates = live
        .get(def.id, scope)
        .iter()
        .chain(def.fallback_models.iter());
    for candidate in candidates {
        if candidate.id.is_empty() || candidate.id == "default" || !seen.insert(candidate.id.clone()) {
            continue;
        }
        if is_known_service_tier(def, Some(&candidate.id), service_tier, live, scope) {
            return Some(candidate.id.clone());
        }
    }
    model_id.map(str::to_string)
}

/// Parity: `resolveModelForAgent`.
pub fn resolve_model_for_agent(
    def: &AgentDef,
    resolved: Option<&str>,
    env: &HashMap<String, String>,
    live: &LiveModelCache,
    scope: Option<&str>,
) -> Option<String> {
    if let Some(id) = resolved {
        if id != "default" {
            return Some(id.to_string());
        }
        return Some(id.to_string());
    }
    if let Some(var) = def.default_model_env_var {
        if let Some(raw) = env.get(var).map(|v| v.trim()).filter(|v| !v.is_empty()) {
            return Some(raw.to_string());
        }
    }
    if def.fallback_models.iter().any(|m| m.id == "default") {
        return resolved.map(str::to_string);
    }
    let live_models = live.get(def.id, scope);
    if let Some(id) = resolve_default_model_from_options(live_models) {
        return Some(id);
    }
    if def.fallback_models.is_empty() {
        return resolved.map(str::to_string);
    }
    resolve_default_model_from_options(&def.fallback_models).or_else(|| resolved.map(str::to_string))
}

fn resolve_default_model_from_options(models: &[ModelOption]) -> Option<String> {
    let mut first: Option<&ModelOption> = None;
    for model in models {
        if model.id.is_empty() || !model.enabled {
            continue;
        }
        if first.is_none() {
            first = Some(model);
        }
        if model.default {
            return Some(model.id.clone());
        }
    }
    first.map(|m| m.id.clone())
}

/// Parity: `sanitizeCustomModel` (`runtimes/models.ts:227`).
pub fn sanitize_custom_model(id: Option<&str>) -> Option<String> {
    let trimmed = id?.trim();
    if trimmed.is_empty() || trimmed.len() > 200 {
        return None;
    }
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return None,
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | ':' | '@' | '-'))
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Parity: `clampCodexReasoning` (`runtimes/defs/shared.ts`).
pub fn clamp_codex_reasoning(model_id: Option<&str>, effort: Option<&str>) -> Option<String> {
    let effort = effort.filter(|e| !e.is_empty())?;
    let raw = model_id.unwrap_or_default().trim().to_string();
    let id = raw.rsplit('/').next().unwrap_or("");
    let late_family = id.is_empty()
        || id == "default"
        || ["gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5"]
            .iter()
            .any(|prefix| id.starts_with(prefix));
    if late_family && effort == "minimal" {
        return Some("low".to_string());
    }
    if id == "gpt-5.1" && effort == "xhigh" {
        return Some("high".to_string());
    }
    if id == "gpt-5.1-codex-mini" {
        if effort == "high" || effort == "xhigh" {
            return Some("high".to_string());
        }
        return Some("medium".to_string());
    }
    Some(effort.to_string())
}

/// Parity: `parseLineSeparatedModels` (`runtimes/defs/shared.ts`).
pub fn parse_line_separated_models(stdout: &str) -> Vec<ModelOption> {
    let mut seen = HashSet::new();
    let mut out = vec![default_model_option()];
    for line in stdout.lines() {
        let id = line.trim();
        if id.is_empty() || id.starts_with('#') || !seen.insert(id.to_string()) {
            continue;
        }
        out.push(ModelOption::simple(id, id));
    }
    out
}

// ---- defs ------------------------------------------------------------------

fn non_empty_dirs(dirs: &[String]) -> Vec<&String> {
    dirs.iter().filter(|d| !d.is_empty()).collect()
}

/// Parity: `claudeAgentDef` (`runtimes/defs/claude.ts`).
fn claude_def() -> AgentDef {
    AgentDef {
        id: "claude",
        name: "Claude Code",
        bin: "claude",
        fallback_bins: &["openclaude"],
        version_args: &["--version"],
        stream_format: "claude-stream-json",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("sonnet", "Sonnet (alias)"),
            ModelOption::simple("opus", "Opus (alias)"),
            ModelOption::simple("haiku", "Haiku (alias)"),
            ModelOption::simple("fable", "Fable (alias)"),
            ModelOption::simple("claude-opus-5", "claude-opus-5"),
            ModelOption::simple("claude-sonnet-5", "claude-sonnet-5"),
            ModelOption::simple("claude-fable-5", "claude-fable-5"),
            ModelOption::simple("claude-opus-4-5", "claude-opus-4-5"),
            ModelOption::simple("claude-sonnet-4-5", "claude-sonnet-4-5"),
            ModelOption::simple("claude-haiku-4-5", "claude-haiku-4-5"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: Some("stream-json"),
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: Some("claude-mcp-json"),
        install_url: None,
        docs_url: None,
        help_args: &["-p", "--help"],
        capability_flags: &[
            ("--include-partial-messages", "partialMessages"),
            ("--forward-subagent-text", "forwardSubagentText"),
            ("--agents", "customAgents"),
            ("--add-dir", "addDir"),
        ],
        resumes_session_via_cli: true,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: Some((&["auth", "status"], 5000)),
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let caps = input.caps;
            let mut args = vec![
                "-p".to_string(),
                "--input-format".to_string(),
                "stream-json".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ];
            if caps.get("partialMessages") == Some(&true) {
                args.push("--include-partial-messages".to_string());
            }
            if caps.get("thinkingDisplay") == Some(&true) {
                args.push("--thinking-display".to_string());
                args.push("summarized".to_string());
            }
            if input.context.observe_native_child_behavior {
                if caps.get("forwardSubagentText") != Some(&true) {
                    return Err(
                        "Claude native Child behavior observation requires advertised --forward-subagent-text support.".to_string(),
                    );
                }
                args.push("--forward-subagent-text".to_string());
            }
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            let dirs = non_empty_dirs(input.extra_allowed_dirs);
            if !dirs.is_empty() && caps.get("addDir") != Some(&false) {
                args.push("--add-dir".to_string());
                for dir in dirs {
                    args.push(dir.clone());
                }
            }
            match (
                input.context.resume_session_id.as_deref().filter(|s| !s.is_empty()),
                input.context.new_session_id.as_deref().filter(|s| !s.is_empty()),
            ) {
                (Some(resume), _) => {
                    args.push("--resume".to_string());
                    args.push(resume.to_string());
                }
                (None, Some(new)) => {
                    args.push("--session-id".to_string());
                    args.push(new.to_string());
                }
                (None, None) => {}
            }
            args.push("--permission-mode".to_string());
            args.push("bypassPermissions".to_string());
            Ok(args)
        },
    }
}

/// Parity: `codebuddyAgentDef` (`runtimes/defs/codebuddy.ts:65`).
fn codebuddy_def() -> AgentDef {
    AgentDef {
        id: "codebuddy",
        name: "Codebuddy Code",
        bin: "codebuddy",
        fallback_bins: &["cbc"],
        version_args: &["--version"],
        stream_format: "claude-stream-json",
        event_parser: None,
        fallback_models: vec![default_model_option()],
        reasoning_options: ["default", "minimal", "low", "medium", "high", "xhigh", "max"]
            .iter()
            .map(|id| {
                ModelOption::simple(
                    id,
                    match *id {
                        "default" => "Default",
                        "minimal" => "Minimal",
                        "low" => "Low",
                        "medium" => "Medium",
                        "high" => "High",
                        "xhigh" => "XHigh",
                        "max" => "Max",
                        _ => id,
                    },
                )
            })
            .collect(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: Some("stream-json"),
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: Some("claude-mcp-json"),
        install_url: Some("https://www.codebuddy.cn"),
        docs_url: Some("https://www.codebuddy.cn/docs/workbuddy/Overview"),
        help_args: &["-p", "--help"],
        capability_flags: &[
            ("--include-partial-messages", "partialMessages"),
            ("--add-dir", "addDir"),
        ],
        resumes_session_via_cli: true,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let caps = input.caps;
            let mut args = vec![
                "-p".to_string(),
                "--input-format".to_string(),
                "stream-json".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ];
            if caps.get("partialMessages") == Some(&true) {
                args.push("--include-partial-messages".to_string());
            }
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            if let Some(reasoning) = input.options.reasoning.as_deref().filter(|r| *r != "default") {
                args.push("--effort".to_string());
                args.push(reasoning.to_string());
            }
            let dirs = non_empty_dirs(input.extra_allowed_dirs);
            if !dirs.is_empty() && caps.get("addDir") != Some(&false) {
                args.push("--add-dir".to_string());
                for dir in dirs {
                    args.push(dir.clone());
                }
            }
            match (
                input.context.resume_session_id.as_deref().filter(|s| !s.is_empty()),
                input.context.new_session_id.as_deref().filter(|s| !s.is_empty()),
            ) {
                (Some(resume), _) => {
                    args.push("--resume".to_string());
                    args.push(resume.to_string());
                }
                (None, Some(new)) => {
                    args.push("--session-id".to_string());
                    args.push(new.to_string());
                }
                (None, None) => {}
            }
            args.push("--permission-mode".to_string());
            args.push("bypassPermissions".to_string());
            Ok(args)
        },
    }
}
// ---- codex (parity: runtimes/defs/codex.ts, exec-json transport) ----------
// The app-server transport override (`withCodexTransport`) belongs to the
// spawn bead: it needs version probing + launch resolution.

/// Parity: `CODEX_SHELL_ENVIRONMENT_INCLUDE_KEYS` (codex.ts:131).
const CODEX_SHELL_ENV_INCLUDE_KEYS: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "TMP", "TEMP",
    "LANG", "LC_ALL", "TERM", "COLORTERM", "SYSTEMROOT", "COMSPEC",
    "PATHEXT", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "HOMEDRIVE",
    "HOMEPATH", "ELECTRON_RUN_AS_NODE", "OD_BIN", "OD_HYPERFRAMES_BIN",
    "OD_NODE_BIN", "OD_DAEMON_URL", "OD_TOOL_TOKEN", "OD_DATA_DIR",
    "OD_PROJECT_ID", "OD_PROJECT_DIR", "OD_WORKSPACE_ID",
    "OD_WORKSPACE_MEMBER_ID", "OD_TASK_INPUT_DIR",
];

/// Parity: `codexOpenDesignShellEnvironmentArgs` (codex.ts:173).
pub fn codex_shell_environment_args() -> Vec<String> {
    let include_only = CODEX_SHELL_ENV_INCLUDE_KEYS
        .iter()
        .map(|key| format!("\"{key}\""))
        .collect::<Vec<_>>()
        .join(",");
    vec![
        "-c".to_string(),
        "allow_login_shell=false".to_string(),
        "-c".to_string(),
        "shell_environment_policy.inherit=\"all\"".to_string(),
        "-c".to_string(),
        "shell_environment_policy.ignore_default_excludes=true".to_string(),
        "-c".to_string(),
        format!("shell_environment_policy.include_only=[{include_only}]"),
    ]
}

/// Parity: `codexReasoningSummaryArgs` (codex.ts:216).
pub fn codex_reasoning_summary_args() -> Vec<String> {
    vec!["-c".to_string(), "model_reasoning_summary=\"detailed\"".to_string()]
}

/// Parity: `codexUpdatePlanToolArgs` (codex.ts:248).
pub fn codex_update_plan_tool_args() -> Vec<String> {
    vec!["-c".to_string(), "tools.update_plan.enabled=true".to_string()]
}

/// Parity: `codexNeedsDangerFullAccessSandbox` (codex.ts:250).
pub fn codex_needs_danger_full_access(platform: &str, env: &HashMap<String, String>) -> bool {
    if env.get("OD_CODEX_SANDBOX").map(|v| v.trim()) == Some("danger-full-access") {
        return true;
    }
    if platform == "win32" {
        return true;
    }
    env.get("WSL_DISTRO_NAME").map(|v| !v.trim().is_empty()).unwrap_or(false)
}

fn codex_fallback_models() -> Vec<ModelOption> {
    let priority = ModelOption::simple("priority", "Fast");
    let mut gpt_5_5 = ModelOption::simple("gpt-5.5", "gpt-5.5");
    gpt_5_5.service_tier_options = vec![priority];
    vec![
        default_model_option(),
        gpt_5_5,
        ModelOption::simple("gpt-5.4", "gpt-5.4"),
        ModelOption::simple("gpt-5.4-mini", "gpt-5.4-mini"),
        ModelOption::simple("gpt-5.3-codex", "gpt-5.3-codex"),
        ModelOption::simple("gpt-5.1", "gpt-5.1"),
        ModelOption::simple("gpt-5.1-codex-mini", "gpt-5.1-codex-mini"),
        ModelOption::simple("gpt-5-codex", "gpt-5-codex"),
        ModelOption::simple("gpt-5", "gpt-5"),
        ModelOption::simple("o3", "o3"),
        ModelOption::simple("o4-mini", "o4-mini"),
    ]
}

/// Parity: `parseCodexDebugModels` (codex.ts:60).
pub fn parse_codex_debug_models(stdout: &str) -> Option<Vec<ModelOption>> {
    let parsed: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let models = if let Some(arr) = parsed.as_array() {
        arr.clone()
    } else {
        parsed.get("models")?.as_array()?.clone()
    };
    let mut out = vec![default_model_option()];
    let mut seen = HashSet::new();
    seen.insert("default".to_string());
    for raw in &models {
        let obj = raw.as_object()?;
        if obj.get("visibility").and_then(|v| v.as_str()) == Some("hidden") {
            continue;
        }
        let id = obj
            .get("slug")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                obj.get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("")
            .to_string();
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        let label = obj
            .get("display_name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                obj.get("name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or(&id)
            .to_string();
        out.push(ModelOption::simple(&id, &label));
    }
    if out.len() > 1 { Some(out) } else { None }
}

/// Parity: `codexAgentDef` (`runtimes/defs/codex.ts:256`).
fn codex_def() -> AgentDef {
    AgentDef {
        id: "codex",
        name: "Codex CLI",
        bin: "codex",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "json-event-stream",
        event_parser: Some("codex"),
        fallback_models: codex_fallback_models(),
        reasoning_options: ["default", "none", "minimal", "low", "medium", "high", "xhigh"]
            .iter()
            .map(|id| ModelOption::simple(id, match *id {
                "default" => "Default",
                "none" => "None",
                "minimal" => "Minimal",
                "low" => "Low",
                "medium" => "Medium",
                "high" => "High",
                "xhigh" => "XHigh",
                _ => id,
            }))
            .collect(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: true,
        captures_session_id_from_stream: true,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: Some((&["login", "status"], 5000)),
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let ctx = input.context;
            let needs_danger = codex_needs_danger_full_access(&ctx.platform, &ctx.env);
            let resume = ctx.resume_session_id.as_deref().filter(|s| !s.is_empty());
            let sandbox_args: Vec<String> = if needs_danger {
                if resume.is_some() {
                    vec!["-c".to_string(), "sandbox_mode=\"danger-full-access\"".to_string()]
                } else {
                    vec!["--sandbox".to_string(), "danger-full-access".to_string()]
                }
            } else if resume.is_some() {
                vec![
                    "-c".to_string(),
                    "sandbox_mode=\"workspace-write\"".to_string(),
                    "-c".to_string(),
                    "sandbox_workspace_write.network_access=true".to_string(),
                ]
            } else {
                vec![
                    "--sandbox".to_string(),
                    "workspace-write".to_string(),
                    "-c".to_string(),
                    "sandbox_workspace_write.network_access=true".to_string(),
                ]
            };
            let mut args = if resume.is_some() {
                vec![
                    "exec".to_string(),
                    "resume".to_string(),
                    "--json".to_string(),
                    "--skip-git-repo-check".to_string(),
                ]
            } else {
                vec![
                    "exec".to_string(),
                    "--json".to_string(),
                    "--skip-git-repo-check".to_string(),
                ]
            };
            args.extend(sandbox_args);
            if ctx.disable_plugins || ctx.codex_disable_plugins_env {
                args.push("--disable".to_string());
                args.push("plugins".to_string());
            }
            args.extend(codex_shell_environment_args());
            args.extend(codex_reasoning_summary_args());
            args.extend(codex_update_plan_tool_args());
            if resume.is_none() {
                if let Some(cwd) = ctx.cwd.as_deref().filter(|c| !c.is_empty()) {
                    args.push("-C".to_string());
                    args.push(cwd.to_string());
                }
                for dir in non_empty_dirs(input.extra_allowed_dirs) {
                    args.push("--add-dir".to_string());
                    args.push(dir.clone());
                }
            }
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            if let Some(reasoning) = input.options.reasoning.as_deref().filter(|r| *r != "default") {
                let effort = clamp_codex_reasoning(input.options.model.as_deref(), Some(reasoning))
                    .unwrap_or_else(|| reasoning.to_string());
                args.push("-c".to_string());
                args.push(format!("model_reasoning_effort=\"{effort}\""));
            }
            if let Some(tier) = input.options.service_tier.as_deref().filter(|t| *t != "default") {
                args.push("-c".to_string());
                args.push(format!("service_tier=\"{tier}\""));
            }
            if let Some(resume) = resume {
                args.push(resume.to_string());
            }
            Ok(args)
        },
    }
}

// ---- opencode (parity: runtimes/defs/opencode.ts) --------------------------

fn opencode_fallback_models() -> Vec<ModelOption> {
    vec![
        default_model_option(),
        ModelOption::simple("anthropic/claude-sonnet-4-5", "anthropic/claude-sonnet-4-5"),
        ModelOption::simple("openai/gpt-5.6-sol", "openai/gpt-5.6-sol"),
        ModelOption::simple("openai/gpt-5.6-terra", "openai/gpt-5.6-terra"),
        ModelOption::simple("openai/gpt-5.6-luna", "openai/gpt-5.6-luna"),
        ModelOption::simple("openai/gpt-5", "openai/gpt-5"),
        ModelOption::simple("google/gemini-2.5-pro", "google/gemini-2.5-pro"),
    ]
}

fn is_valid_opencode_variant_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_opencode_model_id(id: &str) -> bool {
    let Some((provider, model)) = id.split_once('/') else {
        return false;
    };
    if provider.is_empty() || model.is_empty() {
        return false;
    }
    let part_ok = |part: &str| {
        let mut chars = part.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphanumeric() => {}
            _ => return false,
        }
        part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    // provider half allows `[A-Za-z0-9._-]*` after the first char; model half
    // additionally allows `/`, `:`, `@`.
    if !part_ok(provider) {
        return false;
    }
    let mut chars = model.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    model.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '@')
    })
}

/// Parity: `parseOpenCodeModels` (opencode.ts:72). Only verbose metadata can
/// advertise reasoning variants; plain ids are accepted as a fallback.
pub fn parse_opencode_models(stdout: &str) -> Option<Vec<ModelOption>> {
    let lines: Vec<&str> = stdout.lines().collect();
    let mut models = vec![default_model_option()];
    let mut seen = HashSet::new();
    let mut index = 0;
    while index < lines.len() {
        let id = lines[index].trim();
        if !is_opencode_model_id(id) || !seen.insert(id.to_string()) {
            index += 1;
            continue;
        }
        // Optional verbose metadata object starting on the next line.
        let mut variant_ids: Vec<String> = Vec::new();
        if lines.get(index + 1).map(|l| l.trim_start().starts_with('{')).unwrap_or(false) {
            let mut buffer = String::new();
            let mut end = index;
            for (offset, line) in lines.iter().enumerate().skip(index + 1) {
                buffer.push_str(line);
                buffer.push('\n');
                if !line.trim_end().ends_with('}') {
                    continue;
                }
                if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&buffer) {
                    if let Some(serde_json::Value::Object(variants)) = map.get("variants") {
                        variant_ids = variants
                            .keys()
                            .filter(|v| is_valid_opencode_variant_id(v))
                            .cloned()
                            .collect();
                    }
                    end = offset;
                    break;
                }
            }
            index = index.max(end);
        }
        let mut option = ModelOption::simple(id, id);
        if !variant_ids.is_empty() {
            let mut reasoning = vec![ModelOption::simple("default", "Default")];
            for variant in &variant_ids {
                reasoning.push(ModelOption::simple(variant, variant));
            }
            option.reasoning_options = reasoning;
        }
        models.push(option);
        index += 1;
    }
    if models.len() > 1 { Some(models) } else { None }
}

/// Parity: `opencodeAgentDef` (`runtimes/defs/opencode.ts:125`).
fn opencode_def() -> AgentDef {
    AgentDef {
        id: "opencode",
        name: "OpenCode",
        bin: "opencode-cli",
        fallback_bins: &["opencode"],
        version_args: &["--version"],
        stream_format: "json-event-stream",
        event_parser: Some("opencode"),
        fallback_models: opencode_fallback_models(),
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: Some("opencode-env-content"),
        install_url: None,
        docs_url: None,
        help_args: &["run", "--help"],
        capability_flags: &[("--dangerously-skip-permissions", "skipPermissions")],
        resumes_session_via_cli: true,
        captures_session_id_from_stream: true,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec!["run".to_string(), "--format".to_string(), "json".to_string()];
            if input.caps.get("skipPermissions") == Some(&true) {
                args.push("--dangerously-skip-permissions".to_string());
            }
            if let Some(cwd) = input.context.cwd.as_deref().filter(|c| !c.is_empty()) {
                args.push("--dir".to_string());
                args.push(cwd.to_string());
            }
            if let Some(resume) = input.context.resume_session_id.as_deref().filter(|s| !s.is_empty()) {
                args.push("-s".to_string());
                args.push(resume.to_string());
            }
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("-m".to_string());
                args.push(model.to_string());
            }
            if let (Some(model), Some(variant)) = (
                input.options.model.as_deref(),
                input.options.reasoning.as_deref(),
            ) {
                if model != "default" && variant != "default" {
                    let known = input.live.get("opencode", input.live_scope).iter().any(|m| {
                        m.id == model
                            && m.reasoning_options.iter().any(|o| o.id == variant)
                    });
                    if known {
                        args.push("--variant".to_string());
                        args.push(variant.to_string());
                    }
                }
            }
            Ok(args)
        },
    }
}


// ---- cursor-agent (parity: runtimes/defs/cursor-agent.ts) -----------------

/// Parity: `parseCursorAgentModels` (cursor-agent.ts:7).
pub fn parse_cursor_agent_models(stdout: &str) -> Option<Vec<ModelOption>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed.to_lowercase().contains("no models available") {
        return None;
    }
    let mut out = vec![default_model_option()];
    let mut seen = HashSet::new();
    seen.insert("default".to_string());
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let lower = line.to_lowercase();
        if lower == "available models" || lower == "models" {
            continue;
        }
        let (id, label) = match line.find(" - ") {
            Some(pos) => (line[..pos].trim(), line[pos + 3..].trim()),
            None => (line, line),
        };
        if id.is_empty() || !is_model_id_token(id) || !seen.insert(id.to_string()) {
            continue;
        }
        let label = if label.is_empty() { id } else { label };
        out.push(ModelOption::simple(id, label));
    }
    if out.len() > 1 { Some(out) } else { None }
}

fn is_model_id_token(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    id.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | ':' | '@' | '-')
    })
}

/// Parity: `cursorAgentDef` (`runtimes/defs/cursor-agent.ts:33`).
fn cursor_agent_def() -> AgentDef {
    AgentDef {
        id: "cursor-agent",
        name: "Cursor Agent",
        bin: "cursor-agent",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "json-event-stream",
        event_parser: Some("cursor-agent"),
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("auto", "auto"),
            ModelOption::simple("sonnet-4", "sonnet-4"),
            ModelOption::simple("sonnet-4-thinking", "sonnet-4-thinking"),
            ModelOption::simple("gpt-5", "gpt-5"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &["--help"],
        capability_flags: &[("--trust", "trust")],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: Some((&["status"], 5000)),
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--stream-partial-output".to_string(),
                "--force".to_string(),
            ];
            if input.caps.get("trust") == Some(&true) {
                args.push("--trust".to_string());
            }
            if let Some(cwd) = input.context.cwd.as_deref().filter(|c| !c.is_empty()) {
                args.push("--workspace".to_string());
                args.push(cwd.to_string());
            }
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            Ok(args)
        },
    }
}

// ---- copilot (parity: runtimes/defs/copilot.ts) ----------------------------

/// Parity: `copilotAgentDef` (`runtimes/defs/copilot.ts:13`).
fn copilot_def() -> AgentDef {
    AgentDef {
        id: "copilot",
        name: "GitHub Copilot CLI",
        bin: "copilot",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "copilot-stream-json",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("claude-sonnet-4.6", "Claude Sonnet 4.6"),
            ModelOption::simple("gpt-5.2", "GPT-5.2"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: Some(30 * 60 * 1000),
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec![
                "--allow-all-tools".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
            ];
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            for dir in non_empty_dirs(input.extra_allowed_dirs) {
                args.push("--add-dir".to_string());
                args.push(dir.clone());
            }
            Ok(args)
        },
    }
}


// ---- small adapters batch -------------------------------------------------
// Parity sources: runtimes/defs/aider.ts, amp.ts, atomcode.ts, devin.ts,
// qoder.ts, qwen.ts, byok-opencode.ts.

/// Node `path.isAbsolute` spelling for both platforms (TS filters
/// extra dirs/attachments to absolute paths before forwarding).
fn is_absolute_path(path: &str) -> bool {
    if path.starts_with('/') {
        return true;
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    path.starts_with("\\\\")
}

/// Parity: `aiderAgentDef` (`runtimes/defs/aider.ts:6`).
fn aider_def() -> AgentDef {
    AgentDef {
        id: "aider",
        name: "Aider",
        bin: "aider",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "plain",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("sonnet", "sonnet"),
            ModelOption::simple("gpt-4o", "gpt-4o"),
            ModelOption::simple("deepseek/deepseek-chat", "deepseek/deepseek-chat"),
            ModelOption::simple("gemini/gemini-2.0-flash", "gemini/gemini-2.0-flash"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: false,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: Some("https://aider.chat/docs/install.html"),
        docs_url: Some("https://aider.chat"),
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: Some(30_000),
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec![
                "--yes-always".to_string(),
                "--no-pretty".to_string(),
                "--no-git".to_string(),
                "--no-auto-commits".to_string(),
                "--no-suggest-shell-commands".to_string(),
                "--no-show-model-warnings".to_string(),
            ];
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            args.push("--message".to_string());
            args.push(input.prompt.to_string());
            Ok(args)
        },
    }
}

/// Parity: `ampAgentDef` (`runtimes/defs/amp.ts:40`).
fn amp_def() -> AgentDef {
    AgentDef {
        id: "amp",
        name: "Amp",
        bin: "amp",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "claude-stream-json",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("smart", "Smart (mode)"),
            ModelOption::simple("deep", "Deep (mode)"),
            ModelOption::simple("rush", "Rush (mode)"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: Some(false),
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec![
                "-x".to_string(),
                "--stream-json".to_string(),
                "--dangerously-allow-all".to_string(),
            ];
            if let Some(mode) = input.options.model.as_deref().filter(|m| *m != "default") {
                if matches!(mode, "deep" | "smart" | "rush") {
                    args.push("--mode".to_string());
                    args.push(mode.to_string());
                }
            }
            Ok(args)
        },
    }
}

/// Parity: `atomcodeAgentDef` (`runtimes/defs/atomcode.ts:37`).
fn atomcode_def() -> AgentDef {
    AgentDef {
        id: "atomcode",
        name: "AtomCode CLI",
        bin: "atomcode",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "plain",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("claude-sonnet-4-6", "Claude Sonnet 4.6"),
            ModelOption::simple("claude-sonnet-4-5", "Claude Sonnet 4.5"),
            ModelOption::simple("gpt-5.2", "GPT-5.2"),
            ModelOption::simple("glm-5.2", "GLM-5.2"),
            ModelOption::simple("deepseek-v4", "DeepSeek V4"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: false,
        prompt_via_file: true,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: Some("https://atomcode.atomgit.com/docs/en/quickstart.html"),
        docs_url: Some("https://atomcode.atomgit.com/docs/en/index.html"),
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let Some(path) = input.prompt_file_path.filter(|p| !p.is_empty()) else {
                return Err("atomcode requires runtimeContext.promptFilePath".to_string());
            };
            let mut args = vec![
                "--prompt-file".to_string(),
                path.to_string(),
                "-y".to_string(),
            ];
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            Ok(args)
        },
    }
}

/// Parity: `devinAgentDef` (`runtimes/defs/devin.ts:5`).
fn devin_def() -> AgentDef {
    AgentDef {
        id: "devin",
        name: "Devin for Terminal",
        bin: "devin",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "acp-json-rpc",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("adaptive", "adaptive"),
            ModelOption::simple("swe", "swe"),
            ModelOption::simple("opus", "opus"),
            ModelOption::simple("sonnet", "sonnet"),
            ModelOption::simple("codex", "codex"),
            ModelOption::simple("gpt", "gpt"),
            ModelOption::simple("gemini", "gemini"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: false,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: Some("acp-merge"),
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |_input| {
            Ok(vec![
                "--permission-mode".to_string(),
                "dangerous".to_string(),
                "--respect-workspace-trust".to_string(),
                "false".to_string(),
                "acp".to_string(),
            ])
        },
    }
}

/// Parity: `qoderAgentDef` (`runtimes/defs/qoder.ts:6`).
fn qoder_def() -> AgentDef {
    AgentDef {
        id: "qoder",
        name: "Qoder CLI",
        bin: "qodercli",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "qoder-stream-json",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("lite", "Lite"),
            ModelOption::simple("efficient", "Efficient"),
            ModelOption::simple("auto", "Auto"),
            ModelOption::simple("performance", "Performance"),
            ModelOption::simple("ultimate", "Ultimate"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--yolo".to_string(),
            ];
            if let Some(cwd) = input.context.cwd.as_deref().filter(|c| !c.is_empty()) {
                args.push("-w".to_string());
                args.push(cwd.to_string());
            }
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            for dir in input.extra_allowed_dirs.iter().filter(|d| is_absolute_path(d)) {
                args.push("--add-dir".to_string());
                args.push(dir.clone());
            }
            for path in input.image_paths.iter().filter(|p| is_absolute_path(p)) {
                args.push("--attachment".to_string());
                args.push(path.clone());
            }
            Ok(args)
        },
    }
}

/// Parity: `qwenAgentDef` (`runtimes/defs/qwen.ts:21`).
fn qwen_def() -> AgentDef {
    AgentDef {
        id: "qwen",
        name: "Qwen Code",
        bin: "qwen",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "plain",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("qwen3-coder-plus", "qwen3-coder-plus"),
            ModelOption::simple("qwen3-coder-flash", "qwen3-coder-flash"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec!["--yolo".to_string()];
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            Ok(args)
        },
    }
}

/// Parity: `opencodeByokModelId` (`runtimes/byok-opencode.ts:35`).
pub fn opencode_byok_model_id(model: Option<&str>) -> Option<String> {
    let trimmed = model.unwrap_or_default().trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("default") {
        return None;
    }
    if trimmed.starts_with("open-design-byok/") {
        return Some(trimmed.to_string());
    }
    Some(format!("open-design-byok/{trimmed}"))
}

/// Parity: `byokOpenCodeAgentDef` (`runtimes/defs/byok-opencode.ts:11`).
fn byok_opencode_def() -> AgentDef {
    AgentDef {
        id: "byok-opencode",
        name: "BYOK OpenCode",
        bin: "opencode-cli",
        fallback_bins: &["opencode"],
        version_args: &["--version"],
        stream_format: "json-event-stream",
        event_parser: Some("opencode"),
        fallback_models: vec![default_model_option()],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: Some(true),
        external_mcp_injection: Some("opencode-env-content"),
        install_url: None,
        docs_url: None,
        help_args: &["run", "--help"],
        capability_flags: &[("--dangerously-skip-permissions", "skipPermissions")],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec!["run".to_string(), "--format".to_string(), "json".to_string()];
            if let Some(cwd) = input.context.cwd.as_deref().filter(|c| !c.is_empty()) {
                args.push("--dir".to_string());
                args.push(cwd.to_string());
            }
            if input.caps.get("skipPermissions") == Some(&true) {
                args.push("--dangerously-skip-permissions".to_string());
            }
            if let Some(model) = opencode_byok_model_id(input.options.model.as_deref()) {
                args.push("-m".to_string());
                args.push(model);
            }
            Ok(args)
        },
    }
}


// ---- ACP adapters batch ---------------------------------------------------
// Parity sources: runtimes/defs/hermes.ts, kilo.ts, kimi.ts, kiro.ts,
// trae-cli.ts, vibe.ts, mimo.ts. `fetchModels` (live ACP probing) belongs
// to the spawn/detection bead; fallback catalogs and argv are ported here.

macro_rules! acp_def {
    (
        $id:expr, $name:expr, $bin:expr, $models:expr, $argv:expr,
        $mcp_injection:expr, $mcp_discovery:expr, $turn_end:expr,
        $stdio_removed:expr, $probe_timeout:expr, $parser:expr
    ) => {
        AgentDef {
            id: $id,
            name: $name,
            bin: $bin,
            fallback_bins: &[],
            version_args: &["--version"],
            stream_format: "acp-json-rpc",
            event_parser: $parser,
            fallback_models: $models,
            reasoning_options: Vec::new(),
            prompt_via_stdin: false,
            prompt_via_file: false,
            prompt_input_format: None,
            supports_image_paths: false,
            supports_custom_model: None,
            external_mcp_injection: $mcp_injection,
            install_url: None,
            docs_url: None,
            help_args: &[],
            capability_flags: &[],
            resumes_session_via_cli: false,
            captures_session_id_from_stream: false,
            default_model_env_var: None,
            inactivity_timeout_ms: None,
            first_output_timeout_ms: None,
            auth_probe: None,
            max_prompt_arg_bytes: None,
            mcp_discovery: $mcp_discovery,
            acp_turn_end_completes_prompt: $turn_end,
            acp_stdio_mcp_removed_in_version: $stdio_removed,
            acp_mcp_env_format: None,
            version_probe_timeout_ms: $probe_timeout,
            resumes_session_via_acp_load: false,
            encode_stdin_prompt: None,
            resumes_session_via_profile_stdio: false,
            version_policy: None,
            compatibility_probe: None,
            env: &[],
            build_args: |_input| Ok($argv.iter().map(|s| s.to_string()).collect()),
        }
    };
}

/// Parity: `hermesAgentDef` (`runtimes/defs/hermes.ts:5`).
fn hermes_def() -> AgentDef {
    acp_def!(
        "hermes", "Hermes", "hermes",
        vec![
            default_model_option(),
            ModelOption::simple("grok-4.3", "grok-4.3 (xAI · default)"),
            ModelOption::simple("grok-4.20-reasoning", "grok-4.20-reasoning (xAI · deep)"),
            ModelOption::simple("grok-4.20-0309-non-reasoning", "grok-4.20-non-reasoning (xAI · fast)"),
            ModelOption::simple("grok-4.20-multi-agent-0309", "grok-4.20-multi-agent (xAI · orchestration)"),
            ModelOption::simple("openai-codex:gpt-5.5", "gpt-5.5 (openai-codex:gpt-5.5)"),
            ModelOption::simple("openai-codex:gpt-5.4", "gpt-5.4 (openai-codex:gpt-5.4)"),
            ModelOption::simple("openai-codex:gpt-5.4-mini", "gpt-5.4-mini (openai-codex:gpt-5.4-mini)"),
        ],
        &["acp", "--accept-hooks"],
        Some("acp-merge"), Some("mature-acp"), false, None, None, None
    )
}

/// Parity: `kiloAgentDef` (`runtimes/defs/kilo.ts:5`).
fn kilo_def() -> AgentDef {
    acp_def!(
        "kilo", "Kilo", "kilo", vec![default_model_option()], &["acp"],
        Some("acp-merge"), None, false, None, None, None
    )
}

/// Parity: `kimiAgentDef` (`runtimes/defs/kimi.ts:5`).
fn kimi_def() -> AgentDef {
    acp_def!(
        "kimi", "Kimi CLI", "kimi",
        vec![
            default_model_option(),
            ModelOption::simple("kimi-k2-turbo-preview", "kimi-k2-turbo-preview"),
            ModelOption::simple("moonshot-v1-8k", "moonshot-v1-8k"),
            ModelOption::simple("moonshot-v1-32k", "moonshot-v1-32k"),
        ],
        &["acp"],
        Some("acp-merge"), Some("mature-acp"), false, Some("0.37.0"), None, None
    )
}

/// Parity: `kiroAgentDef` (`runtimes/defs/kiro.ts:5`).
fn kiro_def() -> AgentDef {
    acp_def!(
        "kiro", "Kiro CLI", "kiro-cli", vec![default_model_option()], &["acp"],
        Some("acp-merge"), None, true, None, None, None
    )
}

/// Parity: `traeCliAgentDef` (`runtimes/defs/trae-cli.ts:5`).
fn trae_cli_def() -> AgentDef {
    acp_def!(
        "trae-cli", "Trae CLI", "traecli", vec![default_model_option()],
        &["acp", "serve", "--yolo"],
        Some("acp-merge"), Some("mature-acp"), false, None, Some(10_000), None
    )
}

/// Parity: `vibeAgentDef` (`runtimes/defs/vibe.ts:5`).
fn vibe_def() -> AgentDef {
    acp_def!(
        "vibe", "Mistral Vibe CLI", "vibe-acp", vec![default_model_option()], &[] as &[&str],
        Some("acp-merge"), None, false, None, None, None
    )
}

/// Parity: `mimoAgentDef` (`runtimes/defs/mimo.ts:5`).
fn mimo_def() -> AgentDef {
    AgentDef {
        id: "mimo",
        name: "MiMo Code",
        bin: "mimo",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "json-event-stream",
        event_parser: Some("opencode"),
        fallback_models: vec![default_model_option()],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: Some("mimo-env-content"),
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec!["run".to_string(), "--format".to_string(), "json".to_string()];
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            Ok(args)
        },
    }
}


// ---- misc adapters batch --------------------------------------------------
// Parity sources: runtimes/defs/deepseek.ts, reasonix.ts, pi.ts,
// grok-build.ts, deepseek-harness.ts.

/// Parity: `deepseekAgentDef` (`runtimes/defs/deepseek.ts:6`).
fn deepseek_def() -> AgentDef {
    AgentDef {
        id: "deepseek",
        name: "DeepSeek TUI",
        bin: "deepseek",
        fallback_bins: &["codewhale"],
        version_args: &["--version"],
        stream_format: "plain",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("deepseek-v4-pro", "deepseek-v4-pro"),
            ModelOption::simple("deepseek-v4-flash", "deepseek-v4-flash"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: false,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: Some(30_000),
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec!["exec".to_string(), "--auto".to_string()];
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            args.push(input.prompt.to_string());
            Ok(args)
        },
    }
}

const REASONIX_DESIGN_INSTRUCTIONS: &str = "# OpenDesign integration — MUST follow

You are running inside OpenDesign, a design tool. The user message contains
design context (system prompt, skill instructions, design system tokens).
Follow these rules:

1. **Output format**: Wrap your HTML output in <artifact> tags:
   <artifact>
   <!DOCTYPE html>
   <html>...</html>
   </artifact>

2. **Design system**: The user message includes a design system with colors,
   typography, spacing, and component patterns. Apply them consistently.

3. **Skill workflow**: The user message includes a skill (SKILL.md) with
   specific workflow instructions. Follow the skill's steps in order.

4. **No code fences**: Do NOT wrap HTML in ```html code fences.
   Output raw HTML inside <artifact> tags only.

5. **Single file**: Output a complete, self-contained HTML file with all
   CSS and JS inline. No external dependencies.

6. **Language**: Match the language of the user's prompt.";

/// Parity: `reasonixAgentDef` (`runtimes/defs/reasonix.ts:46`).
/// `REASONIX_HOME` resolution is spawn-layer work; the static system-prompt
/// append is carried here.
fn reasonix_def() -> AgentDef {
    AgentDef {
        id: "reasonix",
        name: "DeepSeek Reasonix",
        bin: "reasonix",
        fallback_bins: &["dsnix"],
        version_args: &["--version"],
        stream_format: "acp-json-rpc",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("deepseek-v4-pro", "deepseek-v4-pro"),
            ModelOption::simple("deepseek-v4-flash", "deepseek-v4-flash"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: false,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: Some("acp-merge"),
        install_url: Some("https://github.com/esengine/DeepSeek-Reasonix"),
        docs_url: Some("https://esengine.github.io/DeepSeek-Reasonix/"),
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: Some("mature-acp"),
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: Some("map"),
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[("REASONIX_ACP_SYSTEM_APPEND", REASONIX_DESIGN_INSTRUCTIONS)],
        build_args: |_input| Ok(vec!["acp".to_string()]),
    }
}

/// Parity: `piAgentDef` (`runtimes/defs/pi.ts:10`).
fn pi_def() -> AgentDef {
    AgentDef {
        id: "pi",
        name: "Pi",
        bin: "pi",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "pi-rpc",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("anthropic/claude-sonnet-4-5", "Claude Sonnet 4.5 (anthropic)"),
            ModelOption::simple("anthropic/claude-opus-4-5", "Claude Opus 4.5 (anthropic)"),
            ModelOption::simple("openai/gpt-5", "GPT-5 (openai)"),
            ModelOption::simple("openai/o4-mini", "o4-mini (openai)"),
            ModelOption::simple("google/gemini-2.5-pro", "Gemini 2.5 Pro (google)"),
            ModelOption::simple("google/gemini-2.5-flash", "Gemini 2.5 Flash (google)"),
        ],
        reasoning_options: ["default", "off", "minimal", "low", "medium", "high", "xhigh"]
            .iter()
            .map(|id| ModelOption::simple(id, match *id {
                "default" => "Default",
                "off" => "Off",
                "minimal" => "Minimal",
                "low" => "Low",
                "medium" => "Medium",
                "high" => "High",
                "xhigh" => "XHigh",
                _ => id,
            }))
            .collect(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: true,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: Some(15_000),
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let mut args = vec!["--mode".to_string(), "rpc".to_string()];
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            if let Some(reasoning) = input.options.reasoning.as_deref().filter(|r| *r != "default") {
                args.push("--thinking".to_string());
                args.push(reasoning.to_string());
            }
            for dir in input.extra_allowed_dirs.iter().filter(|d| is_absolute_path(d)) {
                args.push("--append-system-prompt".to_string());
                args.push(dir.clone());
            }
            Ok(args)
        },
    }
}

/// Parity: `parseGrokBuildModels` (grok-build.ts:8).
pub fn parse_grok_build_models(stdout: &str) -> Vec<ModelOption> {
    let mut seen = HashSet::new();
    let mut out = vec![default_model_option()];
    for raw_line in stdout.lines() {
        let mut line = raw_line.trim().trim_start_matches(['*', '-']).trim();
        if let Some(stripped) = line.strip_suffix("(default)") {
            line = stripped.trim();
        }
        let lower = line.to_lowercase();
        if !lower.starts_with("grok-") {
            continue;
        }
        let rest = &line["grok-".len()..];
        let mut chars = rest.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphanumeric() => {}
            _ => continue,
        }
        if !rest.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            continue;
        }
        // TS regex is case-insensitive on the whole id; keep the original
        // spelling but dedupe case-sensitively like the TS Set.
        if !seen.insert(line.to_string()) {
            continue;
        }
        out.push(ModelOption::simple(line, line));
    }
    out
}

fn grok_model_supports_reasoning_effort(model: Option<&str>) -> bool {
    match model {
        None => false,
        Some(m) if m == "default" || m == "grok-build" => false,
        Some(m) => m.to_lowercase().contains("reasoning"),
    }
}

/// Parity: `grokBuildAgentDef` (`runtimes/defs/grok-build.ts:50`).
fn grok_build_def() -> AgentDef {
    AgentDef {
        id: "grok-build",
        name: "Grok Build",
        bin: "grok",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "plain",
        event_parser: None,
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("grok-build", "grok-build (xAI · default)"),
            ModelOption::simple("grok-4.3", "grok-4.3 (xAI)"),
            ModelOption::simple("grok-4.20-reasoning", "grok-4.20-reasoning (xAI · deep)"),
            ModelOption::simple("grok-4.20-non-reasoning", "grok-4.20-non-reasoning (xAI · fast)"),
            ModelOption::simple("grok-4.20-multi-agent", "grok-4.20-multi-agent (xAI · orchestration)"),
        ],
        reasoning_options: ["low", "medium", "high", "xhigh", "max"]
            .iter()
            .map(|id| ModelOption::simple(id, id))
            .collect(),
        prompt_via_stdin: false,
        prompt_via_file: true,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: None,
        external_mcp_injection: None,
        install_url: Some("https://x.ai/cli"),
        docs_url: Some("https://x.ai/cli"),
        help_args: &["-p", "--help"],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            let Some(path) = input.prompt_file_path.filter(|p| !p.is_empty()) else {
                return Err("grok-build requires runtimeContext.promptFilePath".to_string());
            };
            let mut args = vec![
                "--prompt-file".to_string(),
                path.to_string(),
                "--no-plan".to_string(),
                "--always-approve".to_string(),
            ];
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            if input.options.reasoning.is_some()
                && grok_model_supports_reasoning_effort(input.options.model.as_deref())
            {
                args.push("--effort".to_string());
                args.push(input.options.reasoning.clone().unwrap_or_default());
            }
            Ok(args)
        },
    }
}

/// Parity: `parseDeepSeekHarnessVersion` (deepseek-harness.ts:44):
/// optional `v`, three numeric parts, optional prerelease/build metadata.
pub fn parse_deepseek_harness_version(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let rest = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let no_build = rest.split('+').next().unwrap_or("");
    if no_build.contains('+') {
        return None;
    }
    // Split off a single prerelease suffix: numeric core must be exactly N.N.N.
    let (core, pre) = match no_build.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (no_build, None),
    };
    // A prerelease suffix must not contain another bare separator issue:
    // validate dot-separated identifiers loosely (non-empty, alnum + . -).
    if let Some(pre) = pre {
        let main_pre = pre.split('+').next().unwrap_or("");
        if main_pre.is_empty()
            || !main_pre.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return None;
        }
    }
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(rest.to_string())
}

/// Parity: `deepseekHarnessAgentDef` (`runtimes/defs/deepseek-harness.ts:46`).
fn deepseek_harness_def() -> AgentDef {
    AgentDef {
        id: "deepseek-harness",
        name: "DeepSeek Harness",
        bin: "dsh",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "dsh-profile-jsonl",
        event_parser: None,
        fallback_models: vec![default_model_option()],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: Some(false),
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: true,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: true,
        version_policy: Some((
            &["0.1.0-rc.8", "0.1.1-rc.2", "0.1.2-rc.1"],
            "^0\\.1\\.\\d+$|^0\\.1\\.0-rc\\.(?:[6-9]|[1-9]\\d+)$|^0\\.1\\.1-rc\\.\\d+$|^0\\.1\\.2-rc\\.\\d+$",
        )),
        compatibility_probe: Some((&["--profile", "open-design", "--probe"], 10_000)),
        env: &[],
        build_args: |_input| {
            Ok(vec![
                "--profile".to_string(),
                "open-design".to_string(),
                "--stdio".to_string(),
            ])
        },
    }
}

// ---- amr + antigravity (parity: runtimes/defs/amr.ts, antigravity.ts) -----
// `parseVelaModelJson` (price metadata) and all live `fetchModels` probing
// belong to the detection bead.

/// Parity: `normalizeVelaModelId` (amr.ts:48).
pub fn normalize_vela_model_id(raw_id: &str) -> Option<String> {
    let trimmed = raw_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_provider = trimmed.strip_prefix("vela/").unwrap_or(trimmed);
    let without_prefix = without_provider.strip_prefix("public_model_").unwrap_or(without_provider);
    if without_prefix.is_empty() {
        return None;
    }
    let lower = without_prefix.to_lowercase();
    if lower == "deepseek_v3_2" || lower == "deepseek-v3-2" {
        return Some("deepseek-v3.2".to_string());
    }
    if lower == "kimi_k2_6" {
        return Some("kimi-k2.6".to_string());
    }
    if lower == "kimi_k2_7_code" {
        return Some("kimi-k2.7-code".to_string());
    }
    if lower == "glm_5_1" {
        return Some("glm-5.1".to_string());
    }
    if lower == "glm_5" {
        return Some("glm-5".to_string());
    }
    if let Some(versioned) = normalize_known_vela_version_id(without_prefix) {
        return Some(versioned);
    }
    Some(without_prefix.replace('_', "-"))
}

fn normalize_known_vela_version_id(raw_id: &str) -> Option<String> {
    let lower = raw_id.to_lowercase();
    // claude_<family>_<major>_<minor><suffix>
    for prefix in ["claude_haiku", "claude_opus", "claude_sonnet", "claude-haiku", "claude-opus", "claude-sonnet"] {
        let sep = if prefix.contains('_') { '_' } else { '-' };
        if lower.starts_with(prefix) {
            let rest = &raw_id[prefix.len()..];
            let rest = rest.strip_prefix(sep).or_else(|| rest.strip_prefix(if sep == '_' { '-' } else { '_' }))?;
            let (major, minor, suffix) = split_numeric_tail(rest)?;
            let family = prefix.replace('-', "_").replace("claude_", "");
            return Some(format!("claude-{family}-{major}.{minor}{}", suffix.replace('_', "-")));
        }
    }
    for (name, dash) in [("gpt", "gpt"), ("gemini", "gemini")] {
        for sep in ['_', '-'] {
            if lower.starts_with(&format!("{name}{sep}")) {
                let rest = &raw_id[name.len() + 1..];
                let (major, minor, suffix) = split_numeric_tail(rest)?;
                return Some(format!("{dash}-{major}.{minor}{}", suffix.replace('_', "-")));
            }
        }
    }
    for sep in ['_', '-'] {
        if lower.starts_with(&format!("minimax{sep}m")) {
            let rest = &raw_id[8..];
            let (major, minor, suffix) = split_numeric_tail(rest)?;
            return Some(format!("minimax-m{major}.{minor}{}", suffix.replace('_', "-")));
        }
    }
    None
}

fn split_numeric_tail(rest: &str) -> Option<(String, String, String)> {
    // `<major><sep><minor><suffix>` where sep is `_` or `-`.
    let sep_pos = rest.find(|c| ['_', '-'].contains(&c))?;
    let major = rest[..sep_pos].to_string();
    let tail = &rest[sep_pos + 1..];
    let minor_len = tail.chars().take_while(|c| c.is_ascii_digit()).count();
    if major.is_empty() || !major.chars().all(|c| c.is_ascii_digit()) || minor_len == 0 {
        return None;
    }
    let minor = tail[..minor_len].to_string();
    let suffix = tail[minor_len..].to_string();
    Some((major, minor, suffix))
}

/// Parity: `isVelaChatCapableModelId` (amr.ts:107).
pub fn is_vela_chat_capable_model_id(model_id: &str) -> bool {
    let id = normalize_vela_model_id(model_id)
        .unwrap_or_else(|| model_id.trim().to_string())
        .to_lowercase();
    for prefix in [
        "gpt-image-",
        "nano-banana-",
        "seedream-",
        "seedance-",
        "doubao-seedance-",
        "veo-",
        "imagen-",
    ] {
        if id.starts_with(prefix) {
            return false;
        }
    }
    true
}

fn order_amr_chat_models(mut models: Vec<ModelOption>) -> Vec<ModelOption> {
    // Preferred ids first (stable), the rest by id — parity with the TS
    // comparator except for locale collation (byte order here).
    let rank = |id: &str| match id {
        "deepseek-v4-flash" => 0,
        "deepseek-v4-pro" => 1,
        _ => 2,
    };
    models.sort_by(|a, b| {
        rank(&a.id).cmp(&rank(&b.id)).then_with(|| {
            if rank(&a.id) == 2 {
                a.id.cmp(&b.id)
            } else {
                std::cmp::Ordering::Equal
            }
        })
    });
    models
}

/// Parity: `parseVelaModels` (amr.ts:117, line-based form).
pub fn parse_vela_models(stdout: &str) -> Vec<ModelOption> {
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let raw_id = trimmed.split_whitespace().next().unwrap_or("");
        if raw_id.is_empty() {
            continue;
        }
        let Some(id) = normalize_vela_model_id(raw_id) else {
            continue;
        };
        if !seen.insert(id.clone()) || !is_vela_chat_capable_model_id(&id) {
            continue;
        }
        models.push(ModelOption::simple(&id, &id));
    }
    order_amr_chat_models(models)
}

/// Parity: `amrAgentDef` (`runtimes/defs/amr.ts:667`).
fn amr_def() -> AgentDef {
    AgentDef {
        id: "amr",
        name: "AMR",
        bin: "vela",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "acp-json-rpc",
        event_parser: None,
        fallback_models: Vec::new(),
        reasoning_options: Vec::new(),
        prompt_via_stdin: false,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: true,
        supports_custom_model: Some(false),
        external_mcp_injection: None,
        install_url: None,
        docs_url: None,
        help_args: &[],
        capability_flags: &[],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: Some("VELA_DEFAULT_MODEL"),
        inactivity_timeout_ms: Some(30 * 60 * 1000),
        first_output_timeout_ms: Some(2 * 60 * 1000),
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: true,
        encode_stdin_prompt: None,
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |_input| Ok(vec!["agent".to_string(), "run".to_string()]),
    }
}

// ---- antigravity (parity: runtimes/defs/antigravity.ts) --------------------

/// Parity: `encodeAntigravityStdinPrompt` (antigravity.ts:37).
pub fn encode_antigravity_stdin_prompt(prompt: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({ "event": "user", "message": { "content": prompt } })
    )
}

/// Parity: `writeAntigravityModelSelection` (antigravity.ts:68). Returns the
/// path written so callers can assert on it in tests.
pub fn write_antigravity_model_selection(
    label: &str,
    settings_path: Option<&str>,
    home_dir: Option<&str>,
) -> std::io::Result<std::path::PathBuf> {
    let path = match settings_path.filter(|p| !p.is_empty()) {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let home = home_dir
                .map(str::to_string)
                .or_else(|| std::env::var("HOME").ok())
                .unwrap_or_default();
            std::path::Path::new(&home)
                .join(".gemini")
                .join("antigravity-cli")
                .join("settings.json")
        }
    };
    let mut existing: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    existing.insert(
        "model".to_string(),
        serde_json::Value::String(label.to_string()),
    );
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(
        &path,
        format!("{}\n", serde_json::Value::Object(existing)),
    )?;
    Ok(path)
}

/// Parity: `antigravityAgentDef` (`runtimes/defs/antigravity.ts:196`).
fn antigravity_def() -> AgentDef {
    AgentDef {
        id: "antigravity",
        name: "Antigravity",
        bin: "agy",
        fallback_bins: &[],
        version_args: &["--version"],
        stream_format: "json-event-stream",
        event_parser: Some("antigravity"),
        fallback_models: vec![
            default_model_option(),
            ModelOption::simple("Gemini 3.1 Pro (High)", "Gemini 3.1 Pro (High)"),
            ModelOption::simple("Gemini 3.1 Pro (Low)", "Gemini 3.1 Pro (Low)"),
            ModelOption::simple("Gemini 3.5 Flash (High)", "Gemini 3.5 Flash (High)"),
            ModelOption::simple("Gemini 3.5 Flash (Medium)", "Gemini 3.5 Flash (Medium)"),
            ModelOption::simple("Gemini 3.5 Flash (Low)", "Gemini 3.5 Flash (Low)"),
            ModelOption::simple("Claude Sonnet 4.6 (Thinking)", "Claude Sonnet 4.6 (Thinking)"),
            ModelOption::simple("Claude Opus 4.6 (Thinking)", "Claude Opus 4.6 (Thinking)"),
            ModelOption::simple("GPT-OSS 120B (Medium)", "GPT-OSS 120B (Medium)"),
        ],
        reasoning_options: Vec::new(),
        prompt_via_stdin: true,
        prompt_via_file: false,
        prompt_input_format: None,
        supports_image_paths: false,
        supports_custom_model: Some(false),
        external_mcp_injection: None,
        install_url: Some("https://antigravity.google/cli"),
        docs_url: Some("https://antigravity.google/docs/cli-overview"),
        help_args: &["--help"],
        capability_flags: &[("--dangerously-skip-permissions", "skipPermissions")],
        resumes_session_via_cli: false,
        captures_session_id_from_stream: false,
        default_model_env_var: None,
        inactivity_timeout_ms: None,
        first_output_timeout_ms: None,
        auth_probe: None,
        max_prompt_arg_bytes: None,
        mcp_discovery: None,
        acp_turn_end_completes_prompt: false,
        acp_stdio_mcp_removed_in_version: None,
        acp_mcp_env_format: None,
        version_probe_timeout_ms: None,
        resumes_session_via_acp_load: false,
        encode_stdin_prompt: Some(encode_antigravity_stdin_prompt),
        resumes_session_via_profile_stdio: false,
        version_policy: None,
        compatibility_probe: None,
        env: &[],
        build_args: |input| {
            if let Some(model) = input.options.model.as_deref().filter(|m| *m != "default") {
                let settings = input.context.antigravity_settings_path.as_deref();
                write_antigravity_model_selection(model, settings, input.context.home_dir.as_deref())
                    .map_err(|e| e.to_string())?;
            }
            let mut args = Vec::new();
            if let Some(log) = input.context.agent_log_file_path.as_deref().filter(|l| !l.is_empty()) {
                args.push("--log-file".to_string());
                args.push(log.to_string());
            }
            if input.caps.get("skipPermissions") == Some(&true) {
                args.push("--dangerously-skip-permissions".to_string());
            }
            args.push("--input-format".to_string());
            args.push("stream-json".to_string());
            args.push("--output-format".to_string());
            args.push("stream-json".to_string());
            Ok(args)
        },
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        caps: HashMap<String, bool>,
        options: BuildOptions,
    ) -> (
        HashMap<String, bool>,
        BuildOptions,
        Vec<String>,
        AgentContext,
        LiveModelCache,
    ) {
        (
            caps,
            options,
            Vec::new(),
            AgentContext {
                platform: "linux".to_string(),
                ..AgentContext::default()
            },
            LiveModelCache::default(),
        )
    }

    fn args_input<'a>(
        caps: &'a HashMap<String, bool>,
        options: &'a BuildOptions,
        dirs: &'a [String],
        context: &'a AgentContext,
        live: &'a LiveModelCache,
    ) -> BuildArgsInput<'a> {
        BuildArgsInput {
            caps,
            options,
            extra_allowed_dirs: dirs,
            context,
            live,
            live_scope: None,
            prompt: "",
            prompt_file_path: None,
            image_paths: &[],
        }
    }

    #[test]
    fn claude_build_args_gate_on_capabilities() {
        let def = get_agent_def("claude").expect("claude def");
        let (caps, options, dirs, ctx, live) = input(HashMap::new(), BuildOptions::default());
        let args = (def.build_args)(&args_input(&caps, &options, &dirs, &ctx, &live)).expect("args");
        assert_eq!(
            args,
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "bypassPermissions",
            ]
        );
        let mut caps = HashMap::new();
        caps.insert("partialMessages".to_string(), true);
        caps.insert("thinkingDisplay".to_string(), true);
        let (caps, options, dirs, ctx, live) = input(
            caps,
            BuildOptions {
                model: Some("claude-sonnet-5".to_string()),
                ..BuildOptions::default()
            },
        );
        let args = (def.build_args)(&args_input(&caps, &options, &dirs, &ctx, &live)).expect("args");
        assert!(args.contains(&"--include-partial-messages".to_string()));
        assert!(args.contains(&"--thinking-display".to_string()));
        assert!(args.contains(&"--model".to_string()));
    }

    #[test]
    fn codebuddy_default_reasoning_omits_effort_flag() {
        let def = get_agent_def("codebuddy").expect("codebuddy def");
        let (caps, options, dirs, ctx, live) = input(
            HashMap::new(),
            BuildOptions {
                reasoning: Some("default".to_string()),
                ..BuildOptions::default()
            },
        );
        let args = (def.build_args)(&args_input(&caps, &options, &dirs, &ctx, &live)).expect("args");
        assert!(!args.contains(&"--effort".to_string()));
    }

    #[test]
    fn model_helpers_match_typescript() {
        assert_eq!(sanitize_custom_model(Some("claude-sonnet-5")), Some("claude-sonnet-5".to_string()));
        assert_eq!(sanitize_custom_model(Some("--evil")), None);
        assert_eq!(sanitize_custom_model(Some("has space")), None);
        let def = get_agent_def("claude").expect("claude def");
        let live = LiveModelCache::default();
        assert!(is_known_model(def, Some("sonnet"), &live, None));
        assert!(!is_known_model(def, Some("nope"), &live, None));
        assert_eq!(
            clamp_codex_reasoning(Some("gpt-5.1"), Some("xhigh")),
            Some("high".to_string())
        );
        assert_eq!(
            clamp_codex_reasoning(Some("gpt-5.4"), Some("minimal")),
            Some("low".to_string())
        );
        let models = parse_line_separated_models("a\n# comment\n\na\nb\n");
        assert_eq!(models.len(), 3);
    }
}
