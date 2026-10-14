//! Agent child-process spawning — Rust parity port of the spawn layer in
//! `apps/daemon/src/runtimes/agent-process.ts`, `launch.ts`,
//! `executables.ts`, and `invocation.ts`.
//!
//! This module owns everything up to the running child: binary resolution,
//! argv construction (via [`crate::agents`]), the prompt-bytes budget guard,
//! stdin encoding, and exit classification. Wiring children into run
//! lifecycle (start/cancel/steer, stdout event mapping) lives with the
//! runs engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::agents::{AgentDef, BuildArgsInput};

/// Resolve `bin` (then `fallback_bins` in order) against `PATH`, returning
/// the first executable file found (parity: `resolveAgentBin` /
/// `resolveAgentLaunch`).
pub fn resolve_bin(bin: &str, fallback_bins: &[&str]) -> Option<PathBuf> {
    let mut candidates = vec![bin];
    candidates.extend_from_slice(fallback_bins);
    for candidate in candidates {
        if candidate.contains('/') || candidate.contains('\\') {
            let path = PathBuf::from(candidate);
            if is_executable_file(&path) {
                return Some(path);
            }
            continue;
        }
        if let Some(path) = lookup_path(candidate) {
            return Some(path);
        }
    }
    None
}

fn lookup_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            for ext in ["exe", "cmd", "bat"] {
                let with_ext = dir.join(format!("{name}.{ext}"));
                if is_executable_file(&with_ext) {
                    return Some(with_ext);
                }
            }
        }
    }
    None
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
    std::fs::metadata(path).map(|meta| meta.is_file()).unwrap_or(false)
}

/// Enforce the def's argv prompt budget (parity: the `maxPromptArgBytes`
/// check on the `/api/chat` spawn path). Returns the byte length; `Err`
/// carries the actionable message the route surfaces as an SSE error.
pub fn check_prompt_budget(def: &AgentDef, prompt_bytes: usize) -> Result<usize, String> {
    if let Some(budget) = def.max_prompt_arg_bytes {
        if prompt_bytes > budget {
            return Err(format!(
                "prompt is {prompt_bytes} bytes, over the {budget} byte argv budget for {} (reduce skills/design-system context, or use an adapter with stdin support)",
                def.id
            ));
        }
    }
    Ok(prompt_bytes)
}

/// Encode the composed prompt for the child's stdin (parity: the stdin
/// handoff in `agent-process.ts`): the def's `encodeStdinPrompt` when it
/// declares one, otherwise the raw text plus a trailing newline-less EOF.
pub fn stdin_bytes(def: &AgentDef, prompt: &str) -> Vec<u8> {
    match def.encode_stdin_prompt {
        Some(encode) => encode(prompt).into_bytes(),
        None => prompt.as_bytes().to_vec(),
    }
}

/// How the child transmitted its prompt: argv entry, piped stdin, or a
/// prompt file (parity: `promptViaStdin` / `promptViaFile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTransport {
    Argv,
    Stdin,
    File,
}

pub fn prompt_transport(def: &AgentDef) -> PromptTransport {
    if def.prompt_via_stdin {
        PromptTransport::Stdin
    } else if def.prompt_via_file {
        PromptTransport::File
    } else {
        PromptTransport::Argv
    }
}

/// A spawned agent child with piped stdio.
pub struct SpawnedChild {
    pub child: tokio::process::Child,
}

/// Spawn `bin_path` with `argv` in `cwd` (parity: `spawnAgentChild`).
/// Stdout/stderr are piped; stdin is piped when `stdin_piped` so the
/// caller can write the prompt bytes and close. `env_remove` names
/// inherited variables to strip (case-insensitive).
pub fn spawn_child(
    bin_path: &Path,
    argv: &[String],
    cwd: Option<&str>,
    env_extra: &HashMap<String, String>,
    env_remove: &[&str],
    stdin_piped: bool,
) -> std::io::Result<SpawnedChild> {
    let mut command = tokio::process::Command::new(bin_path);
    command.args(argv);
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    if stdin_piped {
        command.stdin(std::process::Stdio::piped());
    } else {
        command.stdin(std::process::Stdio::null());
    }
    if let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) {
        command.current_dir(cwd);
    }
    command.envs(env_extra);
    for name in env_remove {
        command.env_remove(name);
        // Windows inherits case-insensitively; cover the lowercase twin too.
        command.env_remove(name.to_lowercase());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Keep console windows hidden for daemon-spawned children.
        command.creation_flags(0x08000000);
    }
    Ok(SpawnedChild {
        child: command.spawn()?,
    })
}

/// Classify a waited exit (parity: the exit-code/signal mapping in
/// `run-terminal-reconciliation.ts`): `Ok(code)` for any exit code,
/// `Err` describing signal deaths.
pub fn classify_exit(status: std::process::ExitStatus) -> Result<i32, String> {
    if let Some(code) = status.code() {
        return Ok(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Err(format!("SIG{signal}"));
        }
    }
    Err("terminated by signal".to_string())
}

/// Daemon-owned environment layered over the inherited process env
/// (parity: the portable subset of `createAgentRuntimeEnv` +
/// `createOpenDesignToolEnv` in `apps/daemon/src/server.ts`):
/// `OD_DATA_DIR` (the data-root truth source agents must inherit),
/// the def's static `env` entries, and the run-scoped project identity.
/// Deferred until the backing pieces exist in Rust: `OD_DAEMON_URL` /
/// `OD_TOOL_TOKEN` (no token broker yet), `OD_NODE_BIN` / `OD_BIN` (no `od`
/// CLI port), sandbox/proxy env, workspace scope, and MCP server injection.
pub fn agent_env(
    data_dir: &Path,
    def: &crate::agents::AgentDef,
    project_id: Option<&str>,
    project_dir: Option<&str>,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "OD_DATA_DIR".to_string(),
        data_dir.to_string_lossy().into_owned(),
    );
    for (key, value) in def.env {
        env.insert(key.to_string(), value.to_string());
    }
    if let Some(project_id) = project_id.filter(|id| !id.is_empty()) {
        env.insert("OD_PROJECT_ID".to_string(), project_id.to_string());
    }
    if let Some(project_dir) = project_dir.filter(|dir| !dir.is_empty()) {
        env.insert("OD_PROJECT_DIR".to_string(), project_dir.to_string());
    }
    env
}

/// Convenience wrapper: build argv through the def and resolve the binary
/// in one call. `build` errors (e.g. missing prompt file) and unresolvable
/// binaries both surface as `Err` so the route can fail the run.
pub fn prepare_spawn(
    def: &AgentDef,
    input: &BuildArgsInput,
) -> Result<(PathBuf, Vec<String>), String> {
    let bin_path = resolve_bin(def.bin, def.fallback_bins)
        .ok_or_else(|| {
            format!(
                "agent binary not found: Agent \"{}\" (`{}`) is not installed or not on PATH. Install it and refresh the agent list (GET /api/agents) before retrying.",
                def.name, def.bin
            )
        })?;
    let argv = (def.build_args)(input)?;
    Ok((bin_path, argv))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::get_agent_def;

    #[test]
    fn resolve_bin_finds_path_entries_and_prefers_primary() {
        // `sh` exists on every unix test host; windows hosts resolve via PATHEXT.
        let found = resolve_bin("sh", &[]);
        #[cfg(unix)]
        assert!(found.is_some(), "sh should resolve on PATH");
        #[cfg(not(unix))]
        let _ = found;
        assert!(resolve_bin("definitely-not-a-real-binary-xyz", &[]).is_none());
    }

    #[test]
    fn prompt_budget_guards_argv_adapters() {
        let def = get_agent_def("aider").expect("aider def");
        assert!(check_prompt_budget(def, 10).is_ok());
        let err = check_prompt_budget(def, 30_001).expect_err("over budget");
        assert!(err.contains("stdin support"), "{err}");
        let claude = get_agent_def("claude").expect("claude def");
        assert!(check_prompt_budget(claude, usize::MAX).is_ok());
    }

    #[test]
    fn stdin_encoding_uses_def_encoder() {
        let agy = get_agent_def("antigravity").expect("antigravity def");
        let bytes = stdin_bytes(agy, "hi");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.ends_with('\n'), "{text}");
        assert!(text.contains("\"event\":\"user\""), "{text}");
        let qwen = get_agent_def("qwen").expect("qwen def");
        assert_eq!(stdin_bytes(qwen, "hi"), b"hi");
    }

    #[test]
    fn prompt_transport_matches_def_flags() {
        assert_eq!(
            prompt_transport(get_agent_def("claude").expect("def")),
            PromptTransport::Stdin
        );
        assert_eq!(
            prompt_transport(get_agent_def("atomcode").expect("def")),
            PromptTransport::File
        );
        assert_eq!(
            prompt_transport(get_agent_def("aider").expect("def")),
            PromptTransport::Argv
        );
    }

    #[test]
    fn agent_env_carries_data_dir_def_env_and_project_identity() {
        use std::path::Path;
        let def = get_agent_def("reasonix").expect("reasonix def");
        let env = agent_env(
            Path::new("/data/od"),
            def,
            Some("p1"),
            Some("/work/p1"),
        );
        assert_eq!(env.get("OD_DATA_DIR").map(String::as_str), Some("/data/od"));
        assert_eq!(env.get("OD_PROJECT_ID").map(String::as_str), Some("p1"));
        assert_eq!(env.get("OD_PROJECT_DIR").map(String::as_str), Some("/work/p1"));
        assert!(
            env.get("REASONIX_ACP_SYSTEM_APPEND")
                .map(|v| v.contains("OpenDesign integration"))
                .unwrap_or(false),
            "def env entries must be forwarded"
        );
        let bare = agent_env(Path::new("/data/od"), def, None, None);
        assert!(!bare.contains_key("OD_PROJECT_ID"));
        assert!(!bare.contains_key("OD_PROJECT_DIR"));
    }

    #[test]
    fn classify_exit_passes_codes_through() {
        use std::os::unix::process::ExitStatusExt;
        let ok = std::process::ExitStatus::from_raw(0);
        assert_eq!(classify_exit(ok), Ok(0));
        let failed = std::process::ExitStatus::from_raw(3 << 8);
        assert_eq!(classify_exit(failed), Ok(3));
    }
}
