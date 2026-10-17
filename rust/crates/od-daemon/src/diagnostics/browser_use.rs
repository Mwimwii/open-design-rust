//! Browser-use discovery facts for the diagnostics bundle (`extra.browserUse`).
//! Parity: `apps/daemon/src/browser/browser-use-diagnostics.ts`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

const BROWSER_USE_REGISTRY_BASENAME: &str = "codex-browser-use";
const DEFAULT_STALE_THRESHOLD_MS: i64 = 10 * 60 * 1000;

pub fn browser_use_registry_path(tmp_dir: &Path) -> PathBuf {
    tmp_dir.join(BROWSER_USE_REGISTRY_BASENAME)
}

fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as i64,
        Err(_) => 0,
    }
}

#[cfg(unix)]
fn is_socket(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::FileTypeExt;
    entry
        .file_type()
        .map(|kind| kind.is_socket())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_socket(_entry: &std::fs::DirEntry) -> bool {
    false
}

/// Probe the in-app browser backend registry: how many sockets exist, how many
/// are stale, and whether this session already registered one.
pub fn collect_browser_use_discovery_facts(
    registry_path: &Path,
    now: i64,
    current_session_id: Option<&str>,
    stale_threshold_ms: i64,
) -> Value {
    let entries = match std::fs::read_dir(registry_path) {
        Ok(entries) => entries,
        Err(error) => {
            let code = error.raw_os_error();
            let category = if error.kind() == std::io::ErrorKind::NotFound || code == Some(2) {
                "registry-missing"
            } else {
                "registry-unreadable"
            };
            return json!({
                "registryPath": registry_path.to_string_lossy(),
                "registryExists": false,
                "socketCount": 0,
                "candidateCount": 0,
                "staleCount": 0,
                "currentSessionIdPresent": current_session_id.map(|_| false),
                "probeFailureCategory": category,
                "staleThresholdMs": stale_threshold_ms,
            });
        }
    };

    let mut socket_count = 0i64;
    let mut candidate_count = 0i64;
    let mut stale_count = 0i64;
    let mut newest_mtime = 0i64;
    let mut current_session_id_present = current_session_id.map(|_| false);

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(is_socket(&entry) || name.ends_with(".sock")) {
            continue;
        }
        socket_count += 1;
        candidate_count += 1;
        if let Some(session) = current_session_id {
            if name.contains(session) {
                current_session_id_present = Some(true);
            }
        }
        match std::fs::metadata(entry.path()).and_then(|info| info.modified()) {
            Ok(mtime) => {
                let millis = mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as i64)
                    .unwrap_or(0);
                newest_mtime = newest_mtime.max(millis);
                if now - millis > stale_threshold_ms {
                    stale_count += 1;
                }
            }
            Err(_) => stale_count += 1,
        }
    }

    let mut facts = json!({
        "registryPath": registry_path.to_string_lossy(),
        "registryExists": true,
        "socketCount": socket_count,
        "candidateCount": candidate_count,
        "staleCount": stale_count,
        "currentSessionIdPresent": current_session_id_present,
        "probeFailureCategory": "not-probed",
        "staleThresholdMs": stale_threshold_ms,
    });
    if newest_mtime > 0 {
        facts["newestSocketAgeMs"] = json!((now - newest_mtime).max(0));
    }
    facts
}

/// Facts for this process's temp directory, exactly as the handler asks for.
pub fn collect_default_browser_use_facts() -> Value {
    let tmp = std::env::temp_dir();
    collect_browser_use_discovery_facts(
        &browser_use_registry_path(&tmp),
        now_ms(),
        None,
        DEFAULT_STALE_THRESHOLD_MS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-diag-browseruse-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_missing_registry_reports_registry_missing() {
        let dir = temp_dir("missing");
        let facts = collect_browser_use_discovery_facts(&dir.join("nope"), now_ms(), None, DEFAULT_STALE_THRESHOLD_MS);
        assert_eq!(facts["registryExists"], json!(false));
        assert_eq!(facts["probeFailureCategory"], json!("registry-missing"));
        assert_eq!(facts["currentSessionIdPresent"], json!(null));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sockets_are_counted_and_old_ones_are_stale() {
        let dir = temp_dir("sockets");
        std::fs::write(dir.join("a.sock"), b"x").unwrap();
        std::fs::write(dir.join("codex-browser-use-session-1.sock"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        let facts = collect_browser_use_discovery_facts(
            &dir,
            now_ms(),
            Some("session-1"),
            DEFAULT_STALE_THRESHOLD_MS,
        );
        assert_eq!(facts["registryExists"], json!(true));
        assert_eq!(facts["socketCount"], json!(2));
        assert_eq!(facts["candidateCount"], json!(2));
        // Both were just written, so neither is stale.
        assert_eq!(facts["staleCount"], json!(0));
        assert_eq!(facts["currentSessionIdPresent"], json!(true));
        assert!(facts["newestSocketAgeMs"].is_number());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_registry_reports_zero_counts() {
        let dir = temp_dir("empty");
        let facts = collect_browser_use_discovery_facts(&dir, now_ms(), Some("s"), DEFAULT_STALE_THRESHOLD_MS);
        assert_eq!(facts["registryExists"], json!(true));
        assert_eq!(facts["socketCount"], json!(0));
        assert_eq!(facts["currentSessionIdPresent"], json!(false));
        assert!(facts.get("newestSocketAgeMs").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
