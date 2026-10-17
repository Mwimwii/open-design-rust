//! Per-run event logs and native agent-CLI logs for the diagnostics bundle
//! (parity: `packages/diagnostics/src/agent-logs.ts`).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::sources::{LogSource, LogSourceKind};

pub const DEFAULT_RUN_EVENT_TAIL_BYTES: u64 = 2 * 1024 * 1024;
pub const DEFAULT_MAX_RUN_EVENT_LOGS: usize = 20;
/// CLI session logs can be large (Codex's `codex-tui.log` grows into the
/// hundreds of MB), so keep a generous-but-bounded tail that still captures
/// the final error frame.
pub const DEFAULT_AGENT_LOG_TAIL_BYTES: u64 = 2 * 1024 * 1024;
pub const DEFAULT_MAX_FILES_PER_AGENT: usize = 3;

/// `^[A-Za-z0-9._-]+$` — a directory or file entry that is safe to place in the
/// zip path without traversal or exotic characters.
fn is_safe_dir_entry(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn stat_info(path: &Path) -> Option<(SystemTime, bool)> {
    let info = std::fs::metadata(path).ok()?;
    let mtime = info.modified().ok()?;
    Some((mtime, info.is_file()))
}

#[derive(Debug, Clone, Default)]
pub struct RunEventLogOptions {
    pub max_runs: Option<usize>,
    pub tail_bytes: Option<u64>,
}

/// Collect the most recent per-run event logs (`<runsDir>/<runId>/events.jsonl`).
///
/// These are the daemon's capture of each agent CLI's own stdout/stderr for a
/// run — the richest agent-agnostic signal for "the run failed / produced
/// nothing".
pub fn build_run_event_log_sources(
    runs_dir: Option<&Path>,
    options: &RunEventLogOptions,
) -> Vec<LogSource> {
    let Some(runs_dir) = runs_dir else {
        return Vec::new();
    };
    let max_runs = options.max_runs.unwrap_or(DEFAULT_MAX_RUN_EVENT_LOGS);
    let tail_bytes = options.tail_bytes.unwrap_or(DEFAULT_RUN_EVENT_TAIL_BYTES);
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return Vec::new();
    };

    let mut candidates: Vec<(String, PathBuf, SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().is_dir() || !is_safe_dir_entry(&name) {
            continue;
        }
        let absolute_path = entry.path().join("events.jsonl");
        let Some((mtime, is_file)) = stat_info(&absolute_path) else {
            continue;
        };
        if !is_file {
            continue;
        }
        candidates.push((name, absolute_path, mtime));
    }

    candidates.sort_by_key(|entry| std::cmp::Reverse(entry.2));
    candidates
        .into_iter()
        .take(max_runs)
        .map(|(run_id, absolute_path, _)| LogSource {
            name: format!("runs/{run_id}/events.jsonl"),
            absolute_path,
            kind: LogSourceKind::Text,
            tail_bytes: Some(tail_bytes),
        })
        .collect()
}

/// The newest `*.log` files in one directory, most recent first.
fn latest_log_files_in(dir: &Path, max: usize) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<(String, PathBuf, SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".log") || !is_safe_dir_entry(&name) {
            continue;
        }
        let absolute_path = dir.join(&name);
        let Some((mtime, is_file)) = stat_info(&absolute_path) else {
            continue;
        };
        if !is_file {
            continue;
        }
        candidates.push((name, absolute_path, mtime));
    }
    candidates.sort_by_key(|entry| std::cmp::Reverse(entry.2));
    candidates
        .into_iter()
        .take(max)
        .map(|(name, absolute_path, _)| (name, absolute_path))
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct AgentCliLogOptions {
    /// User home directory; CLI configs/logs live under here.
    pub home_dir: Option<String>,
    /// OpenDesign data dir (OD_DATA_DIR); fallback location of the AMR
    /// OpenCode home.
    pub data_dir: Option<String>,
    /// Effective AMR OpenCode home (the resolved `OPENCODE_TEST_HOME`).
    pub amr_open_code_home: Option<String>,
    /// Effective Claude config home (`CLAUDE_CONFIG_DIR`).
    pub claude_config_dir: Option<String>,
    /// Effective Codex home (`CODEX_HOME`).
    pub codex_home: Option<String>,
    /// `XDG_DATA_HOME`, used for OpenCode's data dir resolution.
    pub xdg_data_home: Option<String>,
    pub max_files_per_agent: Option<usize>,
    pub tail_bytes: Option<u64>,
}

/// Collect the native on-disk logs of the coding-agent CLIs OpenDesign drives:
/// Claude Code, Codex, OpenCode and AMR's private OpenCode runtime.
///
/// Deliberately excluded: secret-bearing files. Only known *log* directories
/// are swept (`*.log` files) — never `~/.amr/config.json`, `~/.codex/auth.json`
/// or session transcripts.
pub fn build_agent_cli_log_sources(options: &AgentCliLogOptions) -> Vec<LogSource> {
    let Some(home) = options.home_dir.as_deref().map(str::trim).filter(|home| !home.is_empty()) else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let max_files = options.max_files_per_agent.unwrap_or(DEFAULT_MAX_FILES_PER_AGENT);
    let tail_bytes = options.tail_bytes.unwrap_or(DEFAULT_AGENT_LOG_TAIL_BYTES);

    let nonempty = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };

    let open_code_user_log_dir = match nonempty(&options.xdg_data_home) {
        Some(xdg) => xdg.join("opencode").join("log"),
        None => home.join(".local").join("share").join("opencode").join("log"),
    };
    let claude_dir = nonempty(&options.claude_config_dir).unwrap_or_else(|| home.join(".claude"));
    let codex_home = nonempty(&options.codex_home).unwrap_or_else(|| home.join(".codex"));

    let mut agent_dirs: Vec<(&str, PathBuf)> = vec![
        ("claude", claude_dir),
        ("codex", codex_home.join("log")),
        ("opencode", open_code_user_log_dir),
    ];

    let amr_home = nonempty(&options.amr_open_code_home);
    let amr_open_code_home = amr_home.or_else(|| {
        nonempty(&options.data_dir).map(|data_dir| data_dir.join("amr").join("opencode-home"))
    });
    if let Some(amr_open_code_home) = amr_open_code_home {
        agent_dirs.push((
            "amr",
            amr_open_code_home
                .join(".local")
                .join("share")
                .join("opencode")
                .join("log"),
        ));
    }

    let mut sources = Vec::new();
    for (agent, dir) in agent_dirs {
        for (name, absolute_path) in latest_log_files_in(&dir, max_files) {
            sources.push(LogSource {
                name: format!("agent-cli-logs/{agent}/{name}"),
                absolute_path,
                kind: LogSourceKind::Text,
                tail_bytes: Some(tail_bytes),
            });
        }
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-diag-agentlogs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn touch(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn touch_at(path: &Path, secs_since_epoch: u64, content: &str) {
        touch(path, content);
        let when = std::time::UNIX_EPOCH + Duration::from_secs(secs_since_epoch);
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    #[test]
    fn run_event_logs_are_empty_without_a_runs_dir() {
        assert!(build_run_event_log_sources(None, &RunEventLogOptions::default()).is_empty());
        let dir = temp_dir("no-runs");
        assert!(build_run_event_log_sources(
            Some(&dir.join("missing")),
            &RunEventLogOptions::default()
        )
        .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_event_logs_come_newest_first_and_skip_dirs_without_events() {
        let dir = temp_dir("run-events");
        let runs = dir.join("runs");
        touch_at(&runs.join("run-a").join("events.jsonl"), 1_767_225_600, "a");
        touch_at(&runs.join("run-b").join("events.jsonl"), 1_767_312_000, "b");
        std::fs::create_dir_all(runs.join("run-empty")).unwrap();

        let sources = build_run_event_log_sources(
            Some(&runs),
            &RunEventLogOptions { max_runs: Some(5), tail_bytes: None },
        );
        let names: Vec<String> = sources.iter().map(|source| source.name.clone()).collect();
        assert_eq!(names, vec!["runs/run-b/events.jsonl", "runs/run-a/events.jsonl"]);
        for source in &sources {
            assert_eq!(source.kind, LogSourceKind::Text);
            assert!(source.tail_bytes.unwrap_or(0) > 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_event_logs_skip_unsafe_directory_names() {
        let dir = temp_dir("run-unsafe");
        let runs = dir.join("runs");
        touch(&runs.join("safe-run_1.2").join("events.jsonl"), "safe");
        touch(&runs.join("unsafe run").join("events.jsonl"), "space");
        touch(&runs.join("unsafe:run").join("events.jsonl"), "colon");

        let sources = build_run_event_log_sources(
            Some(&runs),
            &RunEventLogOptions { max_runs: Some(5), tail_bytes: None },
        );
        let names: Vec<String> = sources.iter().map(|source| source.name.clone()).collect();
        assert_eq!(names, vec!["runs/safe-run_1.2/events.jsonl"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_cli_logs_cover_every_known_home_and_never_sweep_secrets() {
        let dir = temp_dir("agent-logs");
        let home = dir.join("home");
        let data_dir = dir.join("data");
        touch(&home.join(".claude").join("daemon.log"), "x");
        touch(&home.join(".codex").join("log").join("codex-tui.log"), "x");
        touch(
            &home
                .join(".local")
                .join("share")
                .join("opencode")
                .join("log")
                .join("2026-01-01T00.log"),
            "x",
        );
        touch(
            &data_dir
                .join("amr")
                .join("opencode-home")
                .join(".local")
                .join("share")
                .join("opencode")
                .join("log")
                .join("2026-01-02T00.log"),
            "x",
        );
        // Secret-bearing files outside *.log dirs must NOT be swept.
        touch(&home.join(".codex").join("auth.json"), "secret");
        touch(&home.join(".amr").join("config.json"), "secret");

        let sources = build_agent_cli_log_sources(&AgentCliLogOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            data_dir: Some(data_dir.to_string_lossy().into_owned()),
            ..Default::default()
        });
        let names: Vec<String> = sources.iter().map(|source| source.name.clone()).collect();
        for expected in [
            "agent-cli-logs/claude/daemon.log",
            "agent-cli-logs/codex/codex-tui.log",
            "agent-cli-logs/opencode/2026-01-01T00.log",
            "agent-cli-logs/amr/2026-01-02T00.log",
        ] {
            assert!(names.contains(&expected.to_string()), "{expected} missing from {names:?}");
        }
        assert!(!names.iter().any(|name| name.contains("auth.json")));
        assert!(!names.iter().any(|name| name.contains("config.json")));
        for source in &sources {
            assert!(source.name.ends_with(".log"));
            assert!(source.tail_bytes.unwrap_or(0) > 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_home_overrides_beat_the_defaults() {
        let dir = temp_dir("agent-overrides");
        let home = dir.join("home");
        let data_dir = dir.join("data");
        let override_home = dir.join("custom-amr-home");
        let claude_dir = dir.join("custom-claude");
        let codex_home = dir.join("custom-codex");
        touch(
            &data_dir
                .join("amr")
                .join("opencode-home")
                .join(".local")
                .join("share")
                .join("opencode")
                .join("log")
                .join("default.log"),
            "x",
        );
        touch(
            &override_home
                .join(".local")
                .join("share")
                .join("opencode")
                .join("log")
                .join("override.log"),
            "x",
        );
        touch(&home.join(".claude").join("daemon.log"), "x");
        touch(&home.join(".codex").join("log").join("codex-tui.log"), "x");
        touch(&claude_dir.join("cost-tracker.log"), "x");
        touch(&codex_home.join("log").join("session.log"), "x");

        let sources = build_agent_cli_log_sources(&AgentCliLogOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            data_dir: Some(data_dir.to_string_lossy().into_owned()),
            amr_open_code_home: Some(override_home.to_string_lossy().into_owned()),
            claude_config_dir: Some(claude_dir.to_string_lossy().into_owned()),
            codex_home: Some(codex_home.to_string_lossy().into_owned()),
            ..Default::default()
        });
        let names: Vec<String> = sources.iter().map(|source| source.name.clone()).collect();
        assert!(names.contains(&"agent-cli-logs/amr/override.log".to_string()));
        assert!(!names.contains(&"agent-cli-logs/amr/default.log".to_string()));
        assert!(names.contains(&"agent-cli-logs/claude/cost-tracker.log".to_string()));
        assert!(names.contains(&"agent-cli-logs/codex/session.log".to_string()));
        assert!(!names.contains(&"agent-cli-logs/claude/daemon.log".to_string()));
        assert!(!names.contains(&"agent-cli-logs/codex/codex-tui.log".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn xdg_data_home_relocates_the_opencode_log_dir() {
        let dir = temp_dir("agent-xdg");
        let home = dir.join("home");
        let xdg = dir.join("xdg");
        touch(&xdg.join("opencode").join("log").join("session.log"), "x");

        let sources = build_agent_cli_log_sources(&AgentCliLogOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            xdg_data_home: Some(xdg.to_string_lossy().into_owned()),
            ..Default::default()
        });
        assert!(sources
            .iter()
            .any(|source| source.name == "agent-cli-logs/opencode/session.log"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_home_dir_yields_nothing() {
        assert!(build_agent_cli_log_sources(&AgentCliLogOptions {
            home_dir: Some(String::new()),
            ..Default::default()
        })
        .is_empty());
        assert!(build_agent_cli_log_sources(&AgentCliLogOptions::default()).is_empty());
    }

    #[test]
    fn agent_logs_are_capped_per_agent_most_recent_first() {
        let dir = temp_dir("agent-cap");
        let home = dir.join("home");
        for index in 0..5 {
            touch_at(
                &home.join(".claude").join(format!("f{index}.log")),
                1_700_000_000 + index * 60,
                "x",
            );
        }
        let sources = build_agent_cli_log_sources(&AgentCliLogOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            max_files_per_agent: Some(2),
            ..Default::default()
        });
        let names: Vec<String> = sources.iter().map(|source| source.name.clone()).collect();
        assert_eq!(
            names,
            vec![
                "agent-cli-logs/claude/f4.log".to_string(),
                "agent-cli-logs/claude/f3.log".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
