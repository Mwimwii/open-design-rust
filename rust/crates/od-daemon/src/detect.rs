//! Agent capability detection — Rust parity port of the probe layer in
//! `apps/daemon/src/runtimes/detection.ts` (`probeCapabilities`,
//! `probeHiddenCapabilityFlags`) plus the exec helper in
//! `apps/daemon/src/runtimes/invocation.ts` (`execAgentFile`).
//!
//! Only the bounded read-only probes live here: version, help-text
//! capability scan, and hidden-flag value-rejection probes. Launch
//! resolution (`resolveAgentLaunch`), diagnostics, auth probes, model
//! catalog fetching, and the memoizing detection cache belong to later
//! beads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::agents::AgentDef;

/// A value no option can legitimately accept, forcing the CLI to validate
/// the flag before doing any work (parity: `HIDDEN_FLAG_PROBE_VALUE`).
const HIDDEN_FLAG_PROBE_VALUE: &str = "__od_capability_probe__";

/// Output of one bounded probe invocation.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// Run a short read-only metadata probe with a hard timeout (parity:
/// `execAgentFile`): neutral temp-dir cwd, SIGKILL on expiry, combined
/// output captured. `env_extra` layers over the inherited environment.
pub async fn run_probe(
    bin_path: &Path,
    args: &[String],
    env_extra: &HashMap<String, String>,
    timeout: Duration,
) -> std::io::Result<ProbeOutput> {
    let mut command = tokio::process::Command::new(bin_path);
    command
        .args(args)
        .envs(env_extra)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .current_dir(std::env::temp_dir());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_task = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(ref mut pipe) = stdout {
            let _ = pipe.read_to_end(&mut out).await;
        }
        if let Some(ref mut pipe) = stderr {
            let _ = pipe.read_to_end(&mut err).await;
        }
        (out, err)
    };
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            // Read-only metadata probes have nothing to flush: kill outright
            // rather than signaling and waiting on 'close' (parity with
            // `killSignal: 'SIGKILL'` in `execAgentFile`).
            child.start_kill().ok();
            let _ = child.wait().await;
            return Ok(ProbeOutput {
                stdout: String::new(),
                stderr: String::new(),
                success: false,
            });
        }
    };
    let (out, err) = out_task.await;
    Ok(ProbeOutput {
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        success: status.success(),
    })
}

/// First line of the version probe output (parity: the version detection
/// path; `probeCodexVersion` reads the same first line).
pub async fn probe_version(
    bin_path: &Path,
    version_args: &[&str],
    env_extra: &HashMap<String, String>,
    timeout: Duration,
) -> Option<String> {
    let args: Vec<String> = version_args.iter().map(|s| s.to_string()).collect();
    let output = run_probe(bin_path, &args, env_extra, timeout).await.ok()?;
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    let first = combined.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return None;
    }
    Some(first.to_string())
}

/// Substring-scan both help streams for each declared flag (parity:
/// `probeCapabilities`). OpenCode-class CLIs print help on stderr, so a
/// stdout-only scan would silently resolve every flag to false. A failed
/// probe yields the safe baseline (empty map); `None` means the def
/// declares no help metadata at all.
pub async fn probe_capabilities(
    def: &AgentDef,
    bin_path: &Path,
    env_extra: &HashMap<String, String>,
    timeout: Duration,
) -> Option<HashMap<String, bool>> {
    if def.help_args.is_empty() || def.capability_flags.is_empty() {
        return None;
    }
    let args: Vec<String> = def.help_args.iter().map(|s| s.to_string()).collect();
    let output = run_probe(bin_path, &args, env_extra, timeout).await.ok()?;
    let help = format!("{}\n{}", output.stdout, output.stderr);
    let mut caps = HashMap::new();
    for (flag, key) in def.capability_flags {
        caps.insert(key.to_string(), help.contains(*flag));
    }
    Some(caps)
}

/// Value-rejection probes for flags hidden from `--help` (parity:
/// `probeHiddenCapabilityFlags`). Support is recorded only on positive
/// evidence: the failure text names the flag and is not an unknown-option
/// complaint. Currently only Claude Code declares these; the def carries
/// no per-flag metadata in this port (TS keeps `hiddenCapabilityFlags`
/// on the def), so the caller passes the def's static table. Until that
/// table is ported, this covers the single known shape generically.
pub async fn probe_hidden_capabilities(
    probe_args_prefix: &[&str],
    flags: &[(&str, &str)],
    bin_path: &Path,
    env_extra: &HashMap<String, String>,
    timeout: Duration,
) -> HashMap<String, bool> {
    let mut caps = HashMap::new();
    for (flag, key) in flags {
        let mut args: Vec<String> = probe_args_prefix.iter().map(|s| s.to_string()).collect();
        args.push(flag.to_string());
        args.push(HIDDEN_FLAG_PROBE_VALUE.to_string());
        let output = run_probe(bin_path, &args, env_extra, timeout)
            .await
            .unwrap_or_default();
        let combined = format!("{}\n{}", output.stdout, output.stderr);
        caps.insert(
            key.to_string(),
            combined.contains(*flag) && !combined.contains("unknown option"),
        );
    }
    caps
}

/// Probe the capability map for an already-resolved binary (help scan
/// plus the Claude hidden-flag probe). Shared by [`detect_agent`] and the
/// run spawn path, which resolves synchronously and probes async.
pub async fn probe_caps(
    def: &crate::agents::AgentDef,
    bin_path: &Path,
    env_extra: &HashMap<String, String>,
) -> HashMap<String, bool> {
    let mut caps = probe_capabilities(def, bin_path, env_extra, Duration::from_millis(5_000))
        .await
        .unwrap_or_default();
    // Claude Code's hidden `--thinking-display` flag (parity: the
    // `hiddenCapabilityFlags` block on the TS def, kept inline here until
    // the per-def hidden-flag table is ported).
    if def.id == "claude" {
        let hidden = probe_hidden_capabilities(
            &["-p"],
            &[("--thinking-display", "thinkingDisplay")],
            bin_path,
            env_extra,
            Duration::from_millis(5_000),
        )
        .await;
        caps.extend(hidden);
    }
    caps
}

/// The detected state of one agent: resolved binary, version line, and
/// capability map (parity: the per-agent detection result).
#[derive(Debug, Clone)]
pub struct DetectedAgent {
    pub bin_path: PathBuf,
    pub version: Option<String>,
    pub caps: HashMap<String, bool>,
}

/// Resolve + probe one def end to end. `None` when no candidate binary
/// resolves (the caller fails the dependent run as AGENT_UNAVAILABLE).
pub async fn detect_agent(
    def: &AgentDef,
    env_extra: &HashMap<String, String>,
) -> Option<DetectedAgent> {
    let bin_path = crate::spawn::resolve_bin(def.bin, def.fallback_bins)?;
    let probe_timeout =
        Duration::from_millis(def.version_probe_timeout_ms.unwrap_or(5_000));
    let version = probe_version(bin_path.as_path(), def.version_args, env_extra, probe_timeout).await;
    let caps = probe_caps(def, bin_path.as_path(), env_extra).await;
    Some(DetectedAgent {
        bin_path,
        version,
        caps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shim_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-detect-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("shim dir");
        dir
    }

    fn write_shim(dir: &Path, name: &str, script: &str) -> PathBuf {
        let shim = dir.join(name);
        // An orphaned sleeper from an interrupted run can hold the script
        // busy (ETXTBSY); retry briefly rather than failing the suite.
        let mut attempts = 0;
        loop {
            match std::fs::write(&shim, script) {
                Ok(()) => break,
                Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 20 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(err) => panic!("shim script: {err}"),
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&shim).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&shim, perms).expect("perms");
        }
        shim
    }

    #[tokio::test]
    async fn version_probe_reads_first_line() {
        let dir = shim_dir("version");
        let bin = write_shim(&dir, "fake", "#!/bin/sh\necho 'v1.2.3'\necho 'noise'\n");
        let version = probe_version(
            &bin,
            &["--version"],
            &HashMap::new(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(version.as_deref(), Some("v1.2.3"));
    }

    #[tokio::test]
    async fn capability_scan_reads_both_streams() {
        let dir = shim_dir("caps");
        // Help on stderr (the OpenCode shape): a stdout-only scan would miss it.
        let bin = write_shim(&dir, "fake", "#!/bin/sh\necho '--trust' >&2\n");
        let def = crate::agents::get_agent_def("cursor-agent").expect("def");
        let caps = probe_capabilities(def, &bin, &HashMap::new(), Duration::from_secs(5))
            .await
            .expect("caps");
        assert_eq!(caps.get("trust"), Some(&true));
    }

    #[tokio::test]
    async fn hidden_flag_needs_positive_evidence() {
        let dir = shim_dir("hidden");
        // Knows the flag: names it while rejecting the value.
        let knowing = write_shim(
            &dir,
            "knowing",
            "#!/bin/sh\necho \"option '--thinking-display' argument '__od_capability_probe__' is invalid\" >&2\nexit 2\n",
        );
        // Doesn't know the flag: unknown-option complaint.
        let naive = write_shim(
            &dir,
            "naive",
            "#!/bin/sh\necho \"unknown option '--thinking-display'\" >&2\nexit 1\n",
        );
        let flags = [("--thinking-display", "thinkingDisplay")];
        let caps = probe_hidden_capabilities(&["-p"], &flags, &knowing, &HashMap::new(), Duration::from_secs(5)).await;
        assert_eq!(caps.get("thinkingDisplay"), Some(&true));
        let caps = probe_hidden_capabilities(&["-p"], &flags, &naive, &HashMap::new(), Duration::from_secs(5)).await;
        assert_eq!(caps.get("thinkingDisplay"), Some(&false));
    }

    #[tokio::test]
    async fn probe_timeout_kills_hanging_children() {
        let dir = shim_dir("timeout");
        let bin = write_shim(&dir, "hang", "#!/bin/sh\nsleep 30\n");
        let started = std::time::Instant::now();
        let output = run_probe(&bin, &[], &HashMap::new(), Duration::from_millis(300))
            .await
            .expect("probe");
        assert!(started.elapsed() < Duration::from_secs(10), "probe hung");
        assert!(!output.success);
    }
}
