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
}

/// Input to every `build_args` implementation.
pub struct BuildArgsInput<'a> {
    pub caps: &'a HashMap<String, bool>,
    pub options: &'a BuildOptions,
    pub extra_allowed_dirs: &'a [String],
    pub context: &'a AgentContext,
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
    pub build_args: fn(&BuildArgsInput) -> Result<Vec<String>, String>,
}

impl AgentDef {
    /// Parity: `supportsCustomModel` defaulting (`undefined === true`).
    pub fn allows_custom_model(&self) -> bool {
        self.supports_custom_model.unwrap_or(true)
    }
}

fn shipped_defs() -> Vec<AgentDef> {
    vec![claude_def(), codebuddy_def()]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn input(caps: HashMap<String, bool>, options: BuildOptions) -> (HashMap<String, bool>, BuildOptions, Vec<String>, AgentContext) {
        (
            caps,
            options,
            Vec::new(),
            AgentContext {
                platform: "linux".to_string(),
                ..AgentContext::default()
            },
        )
    }

    #[test]
    fn claude_build_args_gate_on_capabilities() {
        let def = get_agent_def("claude").expect("claude def");
        let (caps, options, dirs, ctx) = input(HashMap::new(), BuildOptions::default());
        let args = (def.build_args)(&BuildArgsInput {
            caps: &caps,
            options: &options,
            extra_allowed_dirs: &dirs,
            context: &ctx,
        })
        .expect("args");
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
        let (caps, options, dirs, ctx) = input(
            caps,
            BuildOptions {
                model: Some("claude-sonnet-5".to_string()),
                ..BuildOptions::default()
            },
        );
        let args = (def.build_args)(&BuildArgsInput {
            caps: &caps,
            options: &options,
            extra_allowed_dirs: &dirs,
            context: &ctx,
        })
        .expect("args");
        assert!(args.contains(&"--include-partial-messages".to_string()));
        assert!(args.contains(&"--thinking-display".to_string()));
        assert!(args.contains(&"--model".to_string()));
    }

    #[test]
    fn codebuddy_default_reasoning_omits_effort_flag() {
        let def = get_agent_def("codebuddy").expect("codebuddy def");
        let (caps, options, dirs, ctx) = input(
            HashMap::new(),
            BuildOptions {
                reasoning: Some("default".to_string()),
                ..BuildOptions::default()
            },
        );
        let args = (def.build_args)(&BuildArgsInput {
            caps: &caps,
            options: &options,
            extra_allowed_dirs: &dirs,
            context: &ctx,
        })
        .expect("args");
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
