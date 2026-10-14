//! Agent executable launch resolution — Rust parity port of
//! `apps/daemon/src/runtimes/launch.ts` and the resolution half of
//! `apps/daemon/src/runtimes/executables.ts` (`inspectAgentExecutableResolution`,
//! `resolveAgentLaunch`, `applyAgentLaunchEnv`, `agentBinEnvKey`).
//!
//! Bounded deviation: the user-toolchain search dirs are a curated static
//! list (home override + npm-prefix env honored, 5s cache like TS) rather
//! than the full version-manager root probing/version sorting in
//! `@open-design/platform` — resolution order over the same conventional
//! locations is preserved.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::agents::AgentDef;

/// Parity: `AGENT_BIN_ENV_KEYS` (executables.ts).
pub fn agent_bin_env_key(agent_id: &str) -> Option<&'static str> {
    match agent_id {
        "amr" => Some("VELA_BIN"),
        "aider" => Some("AIDER_BIN"),
        "claude" => Some("CLAUDE_BIN"),
        "codebuddy" => Some("CODEBUDDY_BIN"),
        "codex" => Some("CODEX_BIN"),
        "copilot" => Some("COPILOT_BIN"),
        "cursor-agent" => Some("CURSOR_AGENT_BIN"),
        "deepseek" => Some("DEEPSEEK_BIN"),
        "deepseek-harness" => Some("DSH_BIN"),
        "devin" => Some("DEVIN_BIN"),
        "hermes" => Some("HERMES_BIN"),
        "kimi" => Some("KIMI_BIN"),
        "kiro" => Some("KIRO_BIN"),
        "kilo" => Some("KILO_BIN"),
        "mimo" => Some("MIMO_BIN"),
        "opencode" | "byok-opencode" => Some("OPENCODE_BIN"),
        "pi" => Some("PI_BIN"),
        "qoder" => Some("QODER_BIN"),
        "qwen" => Some("QWEN_BIN"),
        "reasonix" => Some("REASONIX_BIN"),
        "trae-cli" => Some("TRAE_CLI_BIN"),
        "vibe" => Some("VIBE_BIN"),
        _ => None,
    }
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    if !std::fs::metadata(path).map(|meta| meta.is_file()).unwrap_or(false) {
        return false;
    }
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_uppercase();
        if ext.is_empty() {
            return false;
        }
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_string());
        return pathext
            .split(';')
            .map(|value| value.trim().to_uppercase())
            .any(|value| value == format!(".{ext}"));
    }
    #[cfg(not(windows))]
    {
        true
    }
}

fn expand_home(raw: &str, home: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if value == "~" {
        return Some(home.to_string());
    }
    if let Some(rest) = value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")) {
        return Some(format!("{home}/{rest}"));
    }
    if Path::new(value).is_absolute() {
        return Some(value.to_string());
    }
    None
}

/// Absolute executable path from a configured value, or `None`.
fn executable_file_path(raw: Option<&str>, home: &str) -> Option<PathBuf> {
    let expanded = expand_home(raw?, home)?;
    let path = PathBuf::from(expanded);
    if !path.is_absolute() || !is_executable_file(&path) {
        return None;
    }
    Some(path)
}

fn detection_home(env: &HashMap<String, String>) -> String {
    if let Some(home) = env
        .get("OD_AGENT_HOME")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        return home.to_string();
    }
    env.get("HOME")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_default()
}

struct ToolchainCache {
    home: String,
    dirs: Vec<String>,
    at: Instant,
}

static TOOLCHAIN_CACHE: OnceLock<Mutex<Option<ToolchainCache>>> = OnceLock::new();

fn toolchain_cache() -> &'static Mutex<Option<ToolchainCache>> {
    TOOLCHAIN_CACHE.get_or_init(|| Mutex::new(None))
}

/// Curated user-toolchain bin dirs (parity: the conventional locations in
/// `wellKnownUserToolchainBins`; version-manager root probing/sorting is
/// not ported). 5s cache like TS.
pub fn user_toolchain_bin_dirs(env: &HashMap<String, String>) -> Vec<String> {
    let home = detection_home(env);
    {
        let cache = toolchain_cache().lock().unwrap_or_else(|err| err.into_inner());
        if let Some(cached) = cache.as_ref() {
            if cached.home == home && cached.at.elapsed() < std::time::Duration::from_millis(5000) {
                return cached.dirs.clone();
            }
        }
    }
    let mut dirs = Vec::new();
    if let Some(prefix) = env
        .get("NPM_CONFIG_PREFIX")
        .or_else(|| env.get("npm_config_prefix"))
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        dirs.push(format!("{prefix}/bin"));
        #[cfg(windows)]
        dirs.push(prefix.to_string());
    }
    #[cfg(windows)]
    dirs.push(format!("{home}/AppData/Roaming/npm"));
    for leaf in [
        ".local/bin",
        ".vite-plus/bin",
        ".kimi-code/bin",
        ".opencode/bin",
        ".grok/bin",
        ".bun/bin",
        ".volta/bin",
        ".asdf/shims",
        "Library/pnpm",
        ".cargo/bin",
        ".npm-global/bin",
        ".npm-packages/bin",
        "go/bin",
        ".deno/bin",
    ] {
        if !home.is_empty() {
            dirs.push(format!("{home}/{leaf}"));
        }
    }
    if cfg!(not(windows)) {
        dirs.push("/opt/homebrew/bin".to_string());
        dirs.push("/usr/local/bin".to_string());
    }
    let mut cache = toolchain_cache().lock().unwrap_or_else(|err| err.into_inner());
    *cache = Some(ToolchainCache {
        home,
        dirs: dirs.clone(),
        at: Instant::now(),
    });
    dirs
}

fn resolve_path_dirs(env: &HashMap<String, String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    let path_value = env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    for dir in std::env::split_paths(&path_value)
        .map(|p| p.to_string_lossy().into_owned())
        .chain(user_toolchain_bin_dirs(env))
    {
        if dir.is_empty() || !seen.insert(dir.clone()) {
            continue;
        }
        dirs.push(dir);
    }
    dirs
}

/// Every `bin` hit on the search path, in resolution order (parity:
/// `resolveAllOnPath`).
pub fn resolve_all_on_path(bin: &str, env: &HashMap<String, String>) -> Vec<PathBuf> {
    #[cfg(windows)]
    let exts: Vec<String> = {
        let pathext = env
            .get("PATHEXT")
            .cloned()
            .or_else(|| std::env::var("PATHEXT").ok())
            .unwrap_or_else(|| ".EXE;.CMD;.BAT".to_string());
        pathext
            .split(';')
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect()
    };
    #[cfg(not(windows))]
    let exts: Vec<String> = vec![String::new()];
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for dir in resolve_path_dirs(env) {
        for ext in &exts {
            let full = PathBuf::from(format!("{dir}/{bin}{ext}"));
            let key = full.to_string_lossy().into_owned();
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            if full.exists() {
                found.push(full);
            }
        }
    }
    found
}

fn configured_executable_override(
    def: &AgentDef,
    configured_env: &HashMap<String, String>,
) -> Option<PathBuf> {
    // Prefer the explicit per-run map, then the process env.
    let key = agent_bin_env_key(def.id)?;
    let home = detection_home(configured_env);
    let raw = configured_env
        .get(key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        });
    executable_file_path(raw.as_deref(), &home)
}

fn order_path_candidates_for_agent(agent_id: &str, mut candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    if agent_id != "copilot" {
        return candidates;
    }
    // Deprioritize the VS Code bootstrap copy that cannot run headless.
    let is_bootstrap = |path: &PathBuf| {
        path.to_string_lossy()
            .replace('\\', "/")
            .to_lowercase()
            .contains("/code/user/globalstorage/github.copilot-chat/copilotcli/")
    };
    let (bootstrap, rest): (Vec<_>, Vec<_>) =
        candidates.drain(..).partition(|path| is_bootstrap(path));
    candidates = rest;
    candidates.extend(bootstrap);
    candidates
}

// Binaries a completed detection pass proved unlaunchable, per agent id
// (parity: `unusableExecutables` in executables.ts).
static UNUSABLE: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();

fn unusable() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    UNUSABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Parity: `rememberUnusableExecutable`.
pub fn remember_unusable_executable(agent_id: &str, resolved_path: &str) {
    unusable()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .entry(agent_id.to_string())
        .or_default()
        .insert(resolved_path.to_string());
}

/// Parity: `forgetUnusableExecutables`.
pub fn forget_unusable_executables(agent_id: &str) {
    unusable()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .remove(agent_id);
}

/// Parity: `resolveAmrOpenCodeExecutable` — the companion ladder for AMR:
/// `VELA_OPENCODE_BIN`, beside `VELA_BIN`, bundled under
/// `OD_RESOURCE_ROOT`, then PATH.
pub fn resolve_amr_opencode_executable(env: &HashMap<String, String>) -> Option<PathBuf> {
    let home = detection_home(env);
    if let Some(path) = executable_file_path(
        env.get("VELA_OPENCODE_BIN").map(String::as_str),
        &home,
    ) {
        return Some(path);
    }
    let vela = executable_file_path(env.get("VELA_BIN").map(String::as_str), &home)
        .or_else(|| std::env::var("VELA_BIN").ok().and_then(|v| executable_file_path(Some(&v), &home)))?;
    if let Some(parent) = Path::new(&vela).parent() {
        let companion = parent.join("libexec").join("opencode").join(exe_name("opencode"));
        if is_executable_file(&companion) {
            return Some(companion);
        }
    }
    None
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// Parity: `inspectAgentExecutableResolution`.
#[derive(Debug, Clone, Default)]
pub struct ExecutableResolution {
    pub configured_override_path: Option<PathBuf>,
    pub path_resolved_path: Option<PathBuf>,
    pub selected_path: Option<PathBuf>,
}

pub fn inspect_agent_executable_resolution(
    def: &AgentDef,
    configured_env: &HashMap<String, String>,
    skip_path_candidates: &[String],
) -> ExecutableResolution {
    if def.bin.is_empty() {
        return ExecutableResolution::default();
    }
    let configured_override_path = configured_executable_override(def, configured_env);
    let mut skip: HashSet<String> = skip_path_candidates.iter().cloned().collect();
    if let Some(known) = unusable()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(def.id)
    {
        skip.extend(known.iter().cloned());
    }
    let mut bins = vec![def.bin];
    bins.extend_from_slice(def.fallback_bins);
    let mut ordered: Vec<PathBuf> = Vec::new();
    for bin in bins {
        for resolved in resolve_all_on_path(bin, configured_env) {
            let key = resolved.to_string_lossy().into_owned();
            if skip.contains(&key) || ordered.contains(&resolved) {
                continue;
            }
            ordered.push(resolved);
        }
    }
    let ordered = order_path_candidates_for_agent(def.id, ordered);
    let path_resolved_path = ordered.into_iter().next();
    // Packaged AMR/byok-opencode companion ladder.
    let built_in_path = match def.id {
        "byok-opencode" => resolve_amr_opencode_executable(configured_env),
        "amr" => packaged_vela_executable(configured_env),
        _ => None,
    };
    let app_bundle_path = codex_app_bundle_executable(def, configured_env);
    let selected_path = configured_override_path
        .clone()
        .or(built_in_path)
        .or(path_resolved_path.clone())
        .or(app_bundle_path);
    ExecutableResolution {
        configured_override_path,
        path_resolved_path,
        selected_path,
    }
}

fn packaged_vela_executable(env: &HashMap<String, String>) -> Option<PathBuf> {
    let resource_root = env
        .get("OD_RESOURCE_ROOT")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("OD_RESOURCE_ROOT")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })?;
    if resolve_amr_opencode_executable(env).is_none() {
        let tree = PathBuf::from(&resource_root)
            .join("bin")
            .join("libexec")
            .join("opencode");
        let exe = tree.join(exe_name("opencode"));
        if !tree.is_dir() || !is_executable_file(&exe) {
            return None;
        }
    }
    let candidate =
        PathBuf::from(&resource_root).join("bin").join(exe_name("vela"));
    if is_executable_file(&candidate) {
        return Some(candidate);
    }
    None
}

/// Parity: `codexAppBundleExecutable` — macOS Codex.app bundle fallback.
fn codex_app_bundle_executable(
    def: &AgentDef,
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    if def.id != "codex" || !cfg!(target_os = "macos") {
        return None;
    }
    let home = detection_home(env);
    let suffix = ["Codex.app", "Contents", "Resources", "codex"];
    let mut candidates = vec![suffix
        .iter()
        .fold(PathBuf::from(&home).join("Applications"), |path, part| {
            path.join(part)
        })];
    if env.get("OD_AGENT_HOME").is_none() {
        candidates.insert(
            0,
            suffix
                .iter()
                .fold(PathBuf::from("/Applications"), |path, part| {
                    path.join(part)
                }),
        );
    }
    for candidate in candidates {
        if let Some(resolved) =
            executable_file_path(candidate.to_str(), &home)
        {
            return Some(resolved);
        }
    }
    None
}

/// Parity: `resolveAgentLaunch` (+ codex-native upgrade).
#[derive(Debug, Clone)]
pub struct AgentLaunch {
    pub configured_override_path: Option<PathBuf>,
    pub path_resolved_path: Option<PathBuf>,
    pub selected_path: Option<PathBuf>,
    pub launch_path: Option<PathBuf>,
    pub launch_kind: &'static str,
    pub child_path_prepend: Vec<String>,
    pub diagnostic: Option<String>,
}

pub fn resolve_agent_launch(
    def: &AgentDef,
    configured_env: &HashMap<String, String>,
) -> AgentLaunch {
    let resolution = inspect_agent_executable_resolution(def, configured_env, &[]);
    let Some(selected) = resolution.selected_path.clone() else {
        return AgentLaunch {
            configured_override_path: resolution.configured_override_path,
            path_resolved_path: resolution.path_resolved_path,
            selected_path: None,
            launch_path: None,
            launch_kind: "selected",
            child_path_prepend: Vec::new(),
            diagnostic: None,
        };
    };
    let mut child_path_prepend = Vec::new();
    if let Some(parent) = selected.parent() {
        child_path_prepend.push(parent.to_string_lossy().into_owned());
    }
    if def.id != "codex" {
        return AgentLaunch {
            configured_override_path: resolution.configured_override_path,
            path_resolved_path: resolution.path_resolved_path,
            selected_path: Some(selected.clone()),
            launch_path: Some(selected),
            launch_kind: "selected",
            child_path_prepend,
            diagnostic: None,
        };
    }
    let native = try_resolve_codex_native_binary(&selected);
    child_path_prepend.extend(native.child_path_prepend);
    let native_path = native.path;
    AgentLaunch {
        configured_override_path: resolution.configured_override_path,
        path_resolved_path: resolution.path_resolved_path,
        selected_path: Some(selected.clone()),
        launch_path: Some(native_path.clone().unwrap_or(selected)),
        launch_kind: if native_path.is_some() {
            "codex-native"
        } else {
            "selected"
        },
        child_path_prepend,
        diagnostic: native.diagnostic,
    }
}

struct CodexNative {
    path: Option<PathBuf>,
    child_path_prepend: Vec<String>,
    diagnostic: Option<String>,
}

fn codex_target_triple() -> String {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return "aarch64-apple-darwin".to_string();
    }
    if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        return "x86_64-apple-darwin".to_string();
    }
    if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        return "aarch64-unknown-linux-musl".to_string();
    }
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return "x86_64-unknown-linux-musl".to_string();
    }
    if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        return "aarch64-pc-windows-msvc".to_string();
    }
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        return "x86_64-pc-windows-msvc".to_string();
    }
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn codex_package_suffix() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn try_resolve_codex_native_binary(wrapper: &Path) -> CodexNative {
    let triple = codex_target_triple();
    let suffix = codex_package_suffix();
    let mut roots = Vec::new();
    let mut current = wrapper.parent().map(Path::to_path_buf);
    while let Some(dir) = current {
        roots.push(dir.clone());
        current = dir.parent().map(Path::to_path_buf);
    }
    if let Ok(real) = std::fs::canonicalize(wrapper) {
        let mut current = real.parent().map(Path::to_path_buf);
        while let Some(dir) = current {
            if !roots.contains(&dir) {
                roots.push(dir.clone());
            }
            current = dir.parent().map(Path::to_path_buf);
        }
    }
    for root in &roots {
        let scoped = root.join("node_modules").join("@openai");
        let mut package_dirs = vec![scoped.join(format!("codex-{suffix}"))];
        if let Ok(entries) = std::fs::read_dir(&scoped) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    if let Some(name) = entry.file_name().to_str() {
                        if name.starts_with("codex-") {
                            package_dirs.push(scoped.join(name));
                        }
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        for dir in package_dirs {
            if !seen.insert(dir.clone()) {
                continue;
            }
            let vendor_path = dir.join("vendor").join(&triple).join("path");
            let candidates = [
                dir.join("vendor").join(&triple).join("codex").join("codex"),
                dir.join("vendor").join(&triple).join("codex").join("codex.exe"),
                dir.join("codex"),
                dir.join("bin").join("codex"),
                dir.join("vendor").join("codex"),
                dir.join("codex.exe"),
                dir.join("bin").join("codex.exe"),
            ];
            for candidate in candidates {
                if is_executable_file(&candidate) {
                    let prepend = if vendor_path.is_dir() {
                        vec![vendor_path.to_string_lossy().into_owned()]
                    } else {
                        Vec::new()
                    };
                    return CodexNative {
                        path: Some(candidate),
                        child_path_prepend: prepend,
                        diagnostic: None,
                    };
                }
            }
        }
    }
    if !looks_like_codex_node_wrapper(wrapper) {
        return CodexNative {
            path: None,
            child_path_prepend: Vec::new(),
            diagnostic: None,
        };
    }
    CodexNative {
        path: None,
        child_path_prepend: Vec::new(),
        diagnostic: Some(format!(
            "Codex native binary was not found for {suffix}/{triple}; falling back to wrapper {}. Set CODEX_BIN to a native Codex binary if this wrapper cannot launch from a GUI environment.",
            wrapper.display()
        )),
    }
}

fn looks_like_codex_node_wrapper(path: &Path) -> bool {
    let Ok(contents) = std::fs::read(path) else {
        return false;
    };
    let head_len = contents.len().min(64_000);
    let head = String::from_utf8_lossy(&contents[..head_len]).to_lowercase();
    head.contains("node") || head.contains("@openai/codex") || head.contains("codex-")
}

/// Parity: `applyAgentLaunchEnv` — guarantee node/shim dirs at the front
/// of PATH and toolchain dirs at the end (deduped, case-aware key on
/// Windows). `node_bin_dir` defaults to the current exe's directory.
pub fn apply_agent_launch_env(
    mut env: HashMap<String, String>,
    child_path_prepend: &[String],
    node_bin_dir: Option<&str>,
    append_path_dirs: &[String],
) -> HashMap<String, String> {
    let mut to_prepend: Vec<String> = Vec::new();
    if let Some(dir) = node_bin_dir.filter(|d| !d.is_empty()) {
        to_prepend.push(dir.to_string());
    }
    to_prepend.extend(child_path_prepend.iter().cloned());
    if to_prepend.is_empty() && append_path_dirs.is_empty() {
        return env;
    }
    let path_key = env
        .keys()
        .find(|key| key.eq_ignore_ascii_case("path"))
        .cloned()
        .unwrap_or_else(|| "PATH".to_string());
    #[cfg(windows)]
    let delimiter = ';';
    #[cfg(not(windows))]
    let delimiter = ':';
    let existing = env.get(&path_key).cloned().unwrap_or_default();
    let mut seen = HashSet::new();
    let mut merged: Vec<String> = Vec::new();
    for entry in to_prepend
        .iter()
        .map(String::as_str)
        .chain(existing.split(delimiter).filter(|e| !e.is_empty()))
        .chain(append_path_dirs.iter().map(String::as_str))
    {
        let normalized = entry.trim_end_matches(['/', '\\']).to_string();
        #[cfg(windows)]
        let normalized = normalized.to_lowercase();
        if seen.insert(normalized) {
            merged.push(entry.to_string());
        }
    }
    env.insert(path_key, merged.join(&delimiter.to_string()));
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::get_agent_def;

    fn test_env() -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        env.insert("HOME".to_string(), "/tmp/od-launch-test-home".to_string());
        env
    }

    fn write_exe(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("perms");
        }
        path
    }

    #[test]
    fn configured_override_wins_over_path() {
        let dir = std::env::temp_dir().join(format!("od-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let custom = write_exe(&dir.join("custom"), "cursor-agent");
        let on_path = write_exe(&dir.join("onpath"), "cursor-agent");
        let mut env = test_env();
        env.insert(
            "PATH".to_string(),
            format!("{}/onpath:/usr/bin:/bin", dir.display()),
        );
        env.insert(
            "CURSOR_AGENT_BIN".to_string(),
            custom.to_string_lossy().into_owned(),
        );
        let def = get_agent_def("cursor-agent").expect("def");
        let resolution = inspect_agent_executable_resolution(def, &env, &[]);
        assert_eq!(resolution.configured_override_path, Some(custom.clone()));
        assert_eq!(resolution.path_resolved_path, Some(on_path));
        assert_eq!(resolution.selected_path, Some(custom));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unusable_paths_are_skipped() {
        let dir = std::env::temp_dir().join(format!("od-launch-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = write_exe(&dir.join("a"), "qwen");
        let second = write_exe(&dir.join("b"), "qwen");
        let mut env = test_env();
        env.insert(
            "PATH".to_string(),
            format!("{}/a:{}/b", dir.display(), dir.display()),
        );
        let def = get_agent_def("qwen").expect("def");
        remember_unusable_executable("qwen", &first.to_string_lossy());
        let resolution = inspect_agent_executable_resolution(def, &env, &[]);
        assert_eq!(resolution.path_resolved_path, Some(second.clone()));
        forget_unusable_executables("qwen");
        let resolution = inspect_agent_executable_resolution(def, &env, &[]);
        assert_eq!(resolution.path_resolved_path, Some(first));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_env_prepends_without_clobbering() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        let out = apply_agent_launch_env(env, &["/shim".to_string()], Some("/node"), &[]);
        assert_eq!(
            out.get("PATH").map(String::as_str),
            Some("/node:/shim:/usr/bin:/bin")
        );
    }

    #[test]
    fn codex_native_walk_finds_vendored_binary() {
        let dir = std::env::temp_dir().join(format!("od-launch-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let wrapper = write_exe(&dir, "codex");
        let triple = codex_target_triple();
        let vendor = dir.join("node_modules").join("@openai").join("codex-linux");
        let native = vendor.join("vendor").join(&triple).join("codex").join("codex");
        std::fs::create_dir_all(native.parent().expect("parent")).expect("dirs");
        std::fs::write(&native, "#!/bin/sh\nexit 0\n").expect("native");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&native).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&native, perms).expect("perms");
        }
        let found = try_resolve_codex_native_binary(&wrapper);
        assert_eq!(found.path, Some(native));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
