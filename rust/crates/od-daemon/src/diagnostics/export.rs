//! The `GET /api/diagnostics/export` handler (parity:
//! `apps/daemon/src/diagnostics-export.ts`).
//!
//! The bundle is assembled from four families of source: the shell's own app
//! logs (when the caller can name them), the per-run event captures, the native
//! agent-CLI logs, and a set of live snapshots that never reach disk. The
//! response is the zip itself, with the same content type, filename shape and
//! no-store policy the TypeScript daemon sets.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use super::agent_logs::{build_agent_cli_log_sources, build_run_event_log_sources, AgentCliLogOptions, RunEventLogOptions};
use super::browser_use::collect_default_browser_use_facts;
use super::evidence::{
    daemon_health_paths, diagnostics_evidence_paths, recent_api_failures_summary, DiagnosticsState,
};
use super::manifest::DiagnosticsAppInfo;
use super::redaction::RedactionOptions;
use super::sources::{LogSource, LogSourceKind};
use super::zip::{build_diagnostics_zip, DiagnosticsContextInput, DiagnosticsExportInput};

pub const DIAGNOSTICS_CONTENT_TYPE: &str = "application/zip";
pub const DIAGNOSTICS_FILENAME_PREFIX: &str = "open-design-diagnostics";
pub const DIAGNOSTICS_EXPORT_PATH: &str = "/api/diagnostics/export";

const TAIL_BYTES_PER_LOG: u64 = 4 * 1024 * 1024;

pub const STANDALONE_LAUNCH_WARNING: &str =
    "Daemon started without a sidecar runtime (plain `od` / standalone launch); \
     file-based logs are not captured. Re-run via `pnpm tools-dev` or the packaged \
     desktop app to include daemon/web/desktop log files in the bundle.";

pub const RUN_EVENT_CONTENT_WARNING: &str =
    "Per-run event logs may contain conversation content and artifact excerpts. \
     Review the bundle before sharing it.";

/// Where the shell's own log tree lives.
///
/// The TypeScript daemon derives this from the sidecar runtime context it was
/// launched with (`@open-design/sidecar`). The Rust daemon has no sidecar
/// integration, so the caller states the namespace root it knows instead of the
/// daemon guessing a path layout. `None` means "standalone", which produces the
/// same warning the TypeScript daemon emits for a plain `od` launch.
#[derive(Debug, Clone)]
pub struct DiagnosticsRuntime {
    pub namespace: Option<String>,
    pub source: Option<String>,
    pub mode: Option<String>,
    pub base: Option<String>,
    /// `<namespaceRoot>` — the directory that holds `logs/`.
    pub namespace_root: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct DiagnosticsExportOptions {
    pub runtime: Option<DiagnosticsRuntime>,
    /// Project root used to derive crash-report match strings.
    pub project_root: Option<PathBuf>,
    /// Directory holding per-run event logs at `<runsDir>/<runId>/events.jsonl`.
    pub runs_dir: Option<PathBuf>,
    /// OpenDesign data dir (OD_DATA_DIR).
    pub data_dir: Option<PathBuf>,
    pub diagnostics: Arc<DiagnosticsState>,
    /// Live automatic-upload status; defaults to `{ available: false }`.
    pub automatic_upload_status: Option<Value>,
}

/// `readCurrentAppVersionInfo` — the version/channel/packaged triple that both
/// the manifest and the runtime-health summary carry.
#[derive(Debug, Clone)]
pub struct AppVersionInfo {
    pub version: String,
    pub channel: String,
    pub packaged: bool,
    pub platform: String,
    pub arch: String,
}

fn clean_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `isReleaseChannel` — a non-reserved exact name matches `[a-z0-9]{1,12}`.
fn is_release_channel(value: &str) -> bool {
    value != "local"
        && (value == "stable"
            || value == "prerelease"
            || (value.len() <= 12 && value.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())))
}

fn release_channel_from_version(version: &str) -> Option<String> {
    if version.is_empty() {
        return None;
    }
    // ^\d+\.\d+\.\d+-([a-z0-9]{1,12})\.\d+$
    let parts: Vec<&str> = version.splitn(2, '-').collect();
    if parts.len() == 2 {
        let head = parts[0];
        let head_parts: Vec<&str> = head.split('.').collect();
        if head_parts.len() == 3 && head_parts.iter().all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit())) {
            let tail: Vec<&str> = parts[1].split('.').collect();
            if tail.len() >= 2 {
                let name = tail[0];
                if !name.is_empty()
                    && name.len() <= 12
                    && name.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
                    && is_release_channel(name)
                {
                    return Some(name.to_string());
                }
            }
        }
    }
    // Development builds such as `beta-internal.N` still inherit the beta feed.
    let lower = version.to_lowercase();
    if lower.contains("beta") {
        return Some("beta".to_string());
    }
    if lower.contains("prerelease") {
        return Some("prerelease".to_string());
    }
    None
}

fn infer_release_channel_from_version(version: &str) -> Option<String> {
    if let Some(channel) = release_channel_from_version(version) {
        return Some(channel);
    }
    let head = version.split_once('-')?.1;
    let name = head.split('.').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// `isPackagedRuntime` — an explicit resources path wins, otherwise the
/// executable's location decides.
pub fn is_packaged_runtime(exec_path: Option<&str>, resources_path: Option<&str>) -> bool {
    if resources_path.is_some() {
        return true;
    }
    let Some(exec_path) = exec_path else {
        return false;
    };
    let normalized = exec_path.replace('\\', "/").to_lowercase();
    match std::env::consts::OS {
        "macos" => normalized.contains("/contents/resources/"),
        "windows" => normalized.contains("/resources/") || normalized.contains("/app.asar"),
        "linux" => {
            normalized.contains("/usr/share/")
                || normalized.contains("/opt/")
                || normalized.contains("/resources/")
        }
        _ => normalized.contains("/resources/") || normalized.contains("/app.asar"),
    }
}

pub fn resolve_app_version_info(env: &HashMap<String, String>) -> AppVersionInfo {
    let exec_path = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    let resources_path = env.get("OD_RESOURCES_PATH").cloned().or_else(|| clean_env("OD_RESOURCES_PATH"));
    let packaged = is_packaged_runtime(exec_path.as_deref(), resources_path.as_deref());
    let version = env
        .get("OD_APP_VERSION")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let channel = env
        .get("OD_RELEASE_CHANNEL")
        .or_else(|| env.get("OD_APP_CHANNEL"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| infer_release_channel_from_version(&version))
        .unwrap_or_else(|| if packaged { "stable" } else { "development" }.to_string());
    AppVersionInfo {
        version,
        channel,
        packaged,
        platform: match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        }
        .to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

fn safe_username() -> Option<String> {
    for key in ["USER", "LOGNAME", "USERNAME"] {
        if let Some(value) = clean_env(key) {
            return Some(value);
        }
    }
    None
}

/// The user's home directory (`os.homedir()`).
pub fn home_dir() -> Option<String> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    clean_env(key).or_else(|| clean_env("HOME"))
}

/// The environment keys the diagnostics collectors read back per agent.
#[derive(Debug, Clone, Default)]
pub struct ResolvedDiagnosticsAgentEnvironment {
    pub amr_home: Option<String>,
    pub amr_open_code_home: Option<String>,
    pub amr_configured_env: Value,
    pub claude_config_dir: Option<String>,
    pub codex_home: Option<String>,
    pub open_code_xdg_data_home: Option<String>,
}

/// Build the effective child env for one agent the way the spawn layer does: the
/// daemon environment, the resolved data root, and — for AMR only — the default
/// private OpenCode home. Reads back only the keys the collectors use.
fn agent_effective_env(
    agent_id: &str,
    process_env: &HashMap<String, String>,
    data_dir: Option<&Path>,
) -> HashMap<String, String> {
    let mut env = process_env.clone();
    if let Some(data_dir) = data_dir {
        env.insert("OD_DATA_DIR".to_string(), data_dir.to_string_lossy().into_owned());
    }
    if let Some(def) = crate::agents::get_agent_def(agent_id) {
        for (key, value) in def.env {
            env.insert(key.to_string(), value.to_string());
        }
    }
    if agent_id == "amr" {
        let missing = env
            .get("OPENCODE_TEST_HOME")
            .map(|value| value.trim().is_empty())
            .unwrap_or(true);
        if missing {
            if let Some(data_dir) = env
                .get("OD_DATA_DIR")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
            {
                env.insert(
                    "OPENCODE_TEST_HOME".to_string(),
                    PathBuf::from(data_dir)
                        .join("amr")
                        .join("opencode-home")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    env
}

fn clean_env_value(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Resolve agent diagnostics inputs through the same Settings → spawn-env
/// helpers the launcher uses, so login status and log discovery line up with
/// the environment a real run gets. Empty values on failure, exactly like the
/// TypeScript helper.
pub fn resolve_diagnostics_agent_environment(
    data_dir: Option<&Path>,
    process_env: &HashMap<String, String>,
) -> ResolvedDiagnosticsAgentEnvironment {
    let amr = agent_effective_env("amr", process_env, data_dir);
    let claude = agent_effective_env("claude", process_env, data_dir);
    let codex = agent_effective_env("codex", process_env, data_dir);
    let opencode = agent_effective_env("opencode", process_env, data_dir);
    ResolvedDiagnosticsAgentEnvironment {
        amr_home: clean_env_value(&amr, "AMR_HOME"),
        amr_open_code_home: clean_env_value(&amr, "OPENCODE_TEST_HOME"),
        amr_configured_env: json!({}),
        claude_config_dir: clean_env_value(&claude, "CLAUDE_CONFIG_DIR"),
        codex_home: clean_env_value(&codex, "CODEX_HOME"),
        // OpenCode resolves its data/log dir from XDG_DATA_HOME; sandbox mode
        // rewrites that, so read the EFFECTIVE value rather than the host's.
        open_code_xdg_data_home: clean_env_value(&opencode, "XDG_DATA_HOME"),
    }
}

/// Whether an optional log source should be listed at all.
///
/// ENOENT is the ordinary "this launcher never produced one" answer and must
/// drop the entry silently — tools-dev appends to latest.log and never rotates,
/// so listing a phantom would stamp a placeholder into every dev bundle. Any
/// OTHER access failure means the file is THERE but unreachable, so the source
/// stays listed and collection records the real error.
fn should_list_optional_source(path: &Path) -> bool {
    match std::fs::File::open(path) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// The daemon/web/desktop log tree under `<namespaceRoot>/logs/<app>`.
fn build_sidecar_log_sources(runtime: Option<&DiagnosticsRuntime>) -> Vec<LogSource> {
    let Some(runtime) = runtime else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    for app in ["daemon", "web", "desktop"] {
        let logs_dir = runtime.namespace_root.join("logs").join(app);
        let latest = logs_dir.join("latest.log");
        sources.push(LogSource {
            name: format!("logs/{app}/latest.log"),
            absolute_path: latest.clone(),
            kind: LogSourceKind::Text,
            tail_bytes: Some(TAIL_BYTES_PER_LOG),
        });
        // The packaged launcher truncates latest.log on every start and rotates
        // the prior session aside as previous.log. After an incident-triggered
        // relaunch that rotated file IS the incident-time log, so bundle it
        // whenever it exists; non-rotating launchers never produce one.
        let previous = logs_dir.join("previous.log");
        if should_list_optional_source(&previous) {
            sources.push(LogSource {
                name: format!("logs/{app}/previous.log"),
                absolute_path: previous,
                kind: LogSourceKind::Text,
                tail_bytes: Some(TAIL_BYTES_PER_LOG),
            });
        }
        // Only the desktop runs a renderer that writes renderer.log and the
        // GPU/system snapshot; daemon and web are pure headless services, so
        // listing those files only produces missing-file noise.
        if app == "desktop" {
            sources.push(LogSource {
                name: format!("logs/{app}/renderer.log"),
                absolute_path: logs_dir.join("renderer.log"),
                kind: LogSourceKind::Text,
                tail_bytes: Some(TAIL_BYTES_PER_LOG),
            });
            sources.push(LogSource {
                name: format!("logs/{app}/gpu-info.json"),
                absolute_path: logs_dir.join("gpu-info.json"),
                kind: LogSourceKind::Json,
                tail_bytes: None,
            });
        }
    }
    sources
}

/// The desktop relocates Electron's crashDumps to `<logs/desktop>/crashes` so
/// the minidumps live inside the same log tree this export collects.
fn resolve_desktop_crash_dumps_dir(runtime: Option<&DiagnosticsRuntime>) -> Option<PathBuf> {
    runtime.map(|runtime| runtime.namespace_root.join("logs").join("desktop").join("crashes"))
}

fn current_process_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

/// Build the response for `GET /api/diagnostics/export`.
pub async fn create_diagnostics_export(options: DiagnosticsExportOptions) -> Response {
    let process_env = current_process_env();
    let version_info = resolve_app_version_info(&process_env);
    let home = home_dir();
    let agent_environment = resolve_diagnostics_agent_environment(options.data_dir.as_deref(), &process_env);
    let browser_use = collect_default_browser_use_facts();
    let run_event_sources =
        build_run_event_log_sources(options.runs_dir.as_deref(), &RunEventLogOptions::default());

    let mut sources = build_sidecar_log_sources(options.runtime.as_ref());
    sources.extend(run_event_sources.iter().cloned());
    sources.extend(build_agent_cli_log_sources(&AgentCliLogOptions {
        home_dir: home.clone(),
        data_dir: options.data_dir.as_ref().map(|path| path.to_string_lossy().into_owned()),
        amr_open_code_home: agent_environment.amr_open_code_home.clone(),
        claude_config_dir: agent_environment.claude_config_dir.clone(),
        codex_home: agent_environment.codex_home.clone(),
        xdg_data_home: agent_environment
            .open_code_xdg_data_home
            .clone()
            .or_else(|| clean_env("XDG_DATA_HOME")),
        ..Default::default()
    }));

    options.diagnostics.evidence.refresh(&process_env);

    if let Some(data_dir) = &options.data_dir {
        let (current, previous) = diagnostics_evidence_paths(data_dir);
        for (name, absolute_path) in [("latest", current), ("previous", previous)] {
            if should_list_optional_source(&absolute_path) {
                sources.push(LogSource {
                    name: format!("logs/diagnostics/environment-evidence.{name}.json"),
                    absolute_path,
                    kind: LogSourceKind::Json,
                    tail_bytes: Some(256 * 1024),
                });
            }
        }
        let (health_current, health_previous) = daemon_health_paths(data_dir);
        for (name, absolute_path) in [("latest", health_current), ("previous", health_previous)] {
            if should_list_optional_source(&absolute_path) {
                sources.push(LogSource {
                    name: format!("logs/diagnostics/daemon-health.{name}.json"),
                    absolute_path,
                    kind: LogSourceKind::Json,
                    tail_bytes: Some(256 * 1024),
                });
            }
        }
    }

    let username = safe_username();
    let crash_dumps_dir = resolve_desktop_crash_dumps_dir(options.runtime.as_ref());

    // Surface "expected-but-empty" so a reader can tell a collection gap apart
    // from "no runs happened".
    let mut warnings: Vec<String> = Vec::new();
    if options.runtime.is_none() {
        warnings.push(STANDALONE_LAUNCH_WARNING.to_string());
    }
    if !run_event_sources.is_empty() {
        warnings.push(RUN_EVENT_CONTENT_WARNING.to_string());
    }
    if let Some(runs_dir) = &options.runs_dir {
        if run_event_sources.is_empty() {
            warnings.push(format!(
                "No per-run event logs found under {}. Either no chat \
                 runs have executed in this data dir, or run-event persistence is \
                 disabled (server.ts createChatRunService runsLogDir).",
                runs_dir.display()
            ));
        }
    }

    let summaries = vec![
        (
            "automatic-log-upload.json".to_string(),
            options
                .automatic_upload_status
                .clone()
                .unwrap_or_else(|| json!({ "available": false })),
        ),
        (
            "environment-evidence.json".to_string(),
            options.diagnostics.evidence.snapshot(),
        ),
        // Always written, even when nothing was posted, so an empty slot reads
        // as a stated fact instead of a missing file.
        (
            super::evidence::CHAT_SCROLL_FORENSICS_SUMMARY_FILE.to_string(),
            chat_scroll_forensics_summary(&options, &version_info),
        ),
        (
            "recent-api-failures.json".to_string(),
            recent_api_failures_summary(&options.diagnostics.api_failures),
        ),
        (
            "runtime-health.json".to_string(),
            runtime_health_summary(&run_event_sources, &agent_environment),
        ),
    ];

    let input = DiagnosticsExportInput {
        context: DiagnosticsContextInput {
            app: DiagnosticsAppInfo {
                name: "open-design".to_string(),
                version: Some(version_info.version.clone()),
                channel: Some(version_info.channel.clone()),
                packaged: Some(version_info.packaged),
            },
            source: "daemon-http".to_string(),
            namespace: options.runtime.as_ref().and_then(|runtime| runtime.namespace.clone()),
            endpoint: None,
            daemon_reachable: None,
            extra: Some(json!({
                "runtimeAvailable": options.runtime.is_some(),
                "sourceTag": options.runtime.as_ref().and_then(|runtime| runtime.source.clone()),
                "mode": options.runtime.as_ref().and_then(|runtime| runtime.mode.clone()),
                "base": options.runtime.as_ref().and_then(|runtime| runtime.base.clone()),
                "projectRoot": options.project_root.as_ref().map(|path| path.to_string_lossy().into_owned()),
                "browserUse": browser_use,
            })),
            warnings,
        },
        sources,
        redaction: RedactionOptions { username },
        summaries,
        crash_reports: Some(crash_report_lookup(options.project_root.as_deref(), home.as_deref())),
        crash_dumps: crash_dumps_dir.map(|dir| super::sources::CrashDumpLookup {
            dir,
            within_days: Some(14),
            max_dumps: Some(10),
        }),
    };

    match build_diagnostics_zip(input) {
        Ok(result) => {
            let filename = super::manifest::diagnostics_file_name(DIAGNOSTICS_FILENAME_PREFIX);
            let mut response = Response::new(Body::from(result.zip));
            *response.status_mut() = StatusCode::OK;
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(DIAGNOSTICS_CONTENT_TYPE),
            );
            if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
                headers.insert(header::CONTENT_DISPOSITION, value);
            }
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": "DIAGNOSTICS_EXPORT_FAILED", "message": message })),
        )
            .into_response(),
    }
}

fn chat_scroll_forensics_summary(options: &DiagnosticsExportOptions, version_info: &AppVersionInfo) -> Value {
    let mut summary = options.diagnostics.chat_scroll.summary();
    summary["app"] = json!({
        "version": version_info.version,
        "channel": version_info.channel,
        "packaged": version_info.packaged,
        "platform": version_info.platform,
        "arch": version_info.arch,
    });
    summary
}

fn runtime_health_summary(run_event_sources: &[LogSource], _agent_environment: &ResolvedDiagnosticsAgentEnvironment) -> Value {
    let runs_present = !run_event_sources.is_empty();
    json!({
        "daemon": { "reachable": true },
        // Vela's login status is not ported into the Rust daemon; the
        // TypeScript handler itself answers with this shape when the status
        // call throws.
        "amr": { "error": "vela login status is not available in the Rust daemon" },
        "coverage": {
            "runEventsPresent": runs_present,
            "note": if runs_present {
                "Per-run events were included."
            } else {
                "The failure may have happened before a run was created; inspect daemon logs and AMR session state."
            },
        },
    })
}

/// Restrict crash reports to OpenDesign's own process names: a generic
/// "Electron" substring would sweep up any other Electron app's reports.
fn crash_report_lookup(project_root: Option<&Path>, home: Option<&str>) -> super::sources::CrashReportLookup {
    super::sources::CrashReportLookup {
        match_substrings: vec!["Open Design".to_string(), "open-design".to_string()],
        within_days: Some(7),
        max_reports: Some(10),
        search_dirs: None,
        home_dir: home
            .map(PathBuf::from)
            .or_else(|| project_root.map(PathBuf::from)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn release_channels_are_inferred_from_versions() {
        assert_eq!(infer_release_channel_from_version("1.2.3-beta.4"), Some("beta".to_string()));
        assert_eq!(infer_release_channel_from_version("1.2.3-prerelease.9"), Some("prerelease".to_string()));
        assert_eq!(infer_release_channel_from_version("1.2.3"), None);
        assert_eq!(infer_release_channel_from_version("1.2.3-custom.7"), Some("custom".to_string()));
        assert_eq!(infer_release_channel_from_version("beta-internal.3"), Some("beta".to_string()));
    }

    #[test]
    fn packaged_runtime_follows_the_exec_path() {
        assert!(is_packaged_runtime(None, Some("/r")));
        assert!(is_packaged_runtime(Some("/usr/share/OpenDesign/OpenDesign"), None));
        assert!(is_packaged_runtime(Some("/opt/OpenDesign/x"), None));
        assert!(!is_packaged_runtime(Some("/home/me/target/debug/od-tauri"), None));
        assert!(!is_packaged_runtime(None, None));
    }

    #[test]
    fn app_version_info_prefers_the_environment() {
        let info = resolve_app_version_info(&env_of(&[
            ("OD_APP_VERSION", "1.2.3-beta.4"),
            ("OD_RELEASE_CHANNEL", "beta"),
        ]));
        assert_eq!(info.version, "1.2.3-beta.4");
        assert_eq!(info.channel, "beta");
        // Falls back to the crate version and an inferred channel.
        let fallback = resolve_app_version_info(&env_of(&[]));
        assert_eq!(fallback.version, env!("CARGO_PKG_VERSION"));
        assert!(!fallback.channel.is_empty());
    }

    #[test]
    fn amr_open_code_home_defaults_to_the_data_dir() {
        let resolved = resolve_diagnostics_agent_environment(Some(Path::new("/data")), &env_of(&[]));
        assert_eq!(resolved.amr_open_code_home.as_deref(), Some("/data/amr/opencode-home"));
        // An explicit override wins.
        let resolved = resolve_diagnostics_agent_environment(
            Some(Path::new("/data")),
            &env_of(&[("OPENCODE_TEST_HOME", "/custom")]),
        );
        assert_eq!(resolved.amr_open_code_home.as_deref(), Some("/custom"));
        let resolved = resolve_diagnostics_agent_environment(None, &env_of(&[]));
        assert_eq!(resolved.amr_open_code_home, None);
    }

    #[test]
    fn agent_environment_reads_the_effective_cli_homes() {
        let resolved = resolve_diagnostics_agent_environment(
            None,
            &env_of(&[
                ("CLAUDE_CONFIG_DIR", "/c"),
                ("CODEX_HOME", "/x"),
                ("XDG_DATA_HOME", "/xdg"),
                ("AMR_HOME", "/amr"),
            ]),
        );
        assert_eq!(resolved.claude_config_dir.as_deref(), Some("/c"));
        assert_eq!(resolved.codex_home.as_deref(), Some("/x"));
        assert_eq!(resolved.open_code_xdg_data_home.as_deref(), Some("/xdg"));
        assert_eq!(resolved.amr_home.as_deref(), Some("/amr"));
    }

    #[test]
    fn sidecar_log_sources_list_only_existing_previous_logs() {
        let dir = std::env::temp_dir().join(format!("od-diag-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logs = dir.join("logs");
        std::fs::create_dir_all(logs.join("daemon")).unwrap();
        std::fs::write(logs.join("daemon").join("previous.log"), "old").unwrap();
        std::fs::create_dir_all(logs.join("desktop")).unwrap();

        let runtime = DiagnosticsRuntime {
            namespace: Some("default".into()),
            source: Some("tools-dev".into()),
            mode: Some("runtime".into()),
            base: Some(dir.to_string_lossy().into_owned()),
            namespace_root: dir.clone(),
        };
        let sources = build_sidecar_log_sources(Some(&runtime));
        let names: Vec<String> = sources.iter().map(|source| source.name.clone()).collect();
        assert!(names.contains(&"logs/daemon/latest.log".to_string()));
        assert!(names.contains(&"logs/daemon/previous.log".to_string()));
        assert!(!names.contains(&"logs/web/previous.log".to_string()));
        assert!(names.contains(&"logs/desktop/renderer.log".to_string()));
        assert!(names.contains(&"logs/desktop/gpu-info.json".to_string()));
        assert!(build_sidecar_log_sources(None).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn optional_sources_only_stay_listed_for_real_access_failures() {
        let dir = std::env::temp_dir().join(format!("od-diag-optional-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("there.log"), "x").unwrap();
        assert!(should_list_optional_source(&dir.join("there.log")));
        assert!(!should_list_optional_source(&dir.join("missing.log")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_dump_dir_follows_the_desktop_log_tree() {
        let runtime = DiagnosticsRuntime {
            namespace: None,
            source: None,
            mode: None,
            base: None,
            namespace_root: PathBuf::from("/ns"),
        };
        assert_eq!(
            resolve_desktop_crash_dumps_dir(Some(&runtime)),
            Some(PathBuf::from("/ns/logs/desktop/crashes"))
        );
        assert_eq!(resolve_desktop_crash_dumps_dir(None), None);
    }
}
