//! The diagnostics bundle's manifest and machine-info documents (parity:
//! `packages/diagnostics/src/manifest.ts`).
//!
//! Every optional field is serialized by OMISSION, never as an explicit
//! `null`: `JSON.stringify` drops `undefined` keys, and the bundle is read by
//! support tooling that treats a present-null differently from an absent key.

use serde::Serialize;
use serde_json::Value;

use super::sources::CollectedFile;
use super::time::UtcParts;

#[derive(Debug, Clone, Default, Serialize)]
pub struct DiagnosticsAppInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packaged: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct DiagnosticsContext {
    pub app: DiagnosticsAppInfo,
    /// Which surface triggered the export (`daemon-http`, `desktop-ipc`).
    pub source: String,
    /// Active runtime namespace, when known.
    pub namespace: Option<String>,
    /// Daemon URL or other endpoint the UI was connected to.
    pub endpoint: Option<String>,
    /// Whether the host process could reach the daemon when triggering.
    pub daemon_reachable: Option<bool>,
    /// Free-form flags or notes, JSON-stringified into the manifest.
    pub extra: Option<Value>,
    /// Upstream warnings to merge into the manifest's `warnings` array.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    pub name: String,
    pub absolute_path: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsManifest {
    pub exported_at: String,
    pub app: DiagnosticsAppInfo,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_reachable: Option<bool>,
    pub files: Vec<ManifestFile>,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineInfo {
    pub hostname: String,
    pub platform: String,
    pub release: String,
    pub arch: String,
    #[serde(rename = "type")]
    pub os_type: String,
    pub total_memory_bytes: u64,
    pub node_version: String,
    pub pid: u32,
    pub ppid: u32,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// Build the manifest from the supplied context and what each source produced.
/// An unreadable file contributes both a `files` entry (with its error) and a
/// matching `warnings` sentence.
pub fn build_manifest(context: &DiagnosticsContext, files: &[CollectedFile]) -> DiagnosticsManifest {
    let mut warnings = context.warnings.clone();
    for file in files {
        if let Some(error) = &file.error {
            warnings.push(format!("{}: {error}", file.name));
        }
    }
    DiagnosticsManifest {
        exported_at: super::time::now_iso_millis(),
        app: context.app.clone(),
        source: context.source.clone(),
        namespace: context.namespace.clone(),
        endpoint: context.endpoint.clone(),
        daemon_reachable: context.daemon_reachable,
        files: files
            .iter()
            .map(|file| ManifestFile {
                name: file.name.clone(),
                absolute_path: file.absolute_path.clone(),
                bytes: file.bytes,
                error: file.error.clone(),
            })
            .collect(),
        warnings,
        extra: context.extra.clone(),
    }
}

/// The `process.platform` / `os.type()` spellings of this host.
fn platform_name() -> String {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
    .to_string()
}

fn os_type() -> String {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows_NT",
        other => other,
    }
    .to_string()
}

/// `os.release()` — best-effort without a libc binding: the kernel release
/// string is readable from `/proc` on Linux, and `unknown` elsewhere.
fn os_release() -> String {
    if let Ok(text) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "unknown".to_string()
}

/// `os.totalmem()` — `/proc/meminfo` on Linux, zero elsewhere.
fn total_memory_bytes() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

/// Parent process id. The Rust standard library exposes no such call, so this
/// is a tiny FFI shim on unix (the same pattern `od-core` uses for `geteuid`).
#[cfg(unix)]
fn parent_pid() -> u32 {
    extern "C" {
        fn getppid() -> i32;
    }
    let pid = unsafe { getppid() };
    if pid < 0 {
        0
    } else {
        pid as u32
    }
}

#[cfg(not(unix))]
fn parent_pid() -> u32 {
    0
}

/// `nodeVersion` reported by the TypeScript daemon. The Rust daemon is not a
/// Node process, so the field stays a string with an explicit "no Node runtime"
/// marker rather than silently claiming a version.
const NO_NODE_RUNTIME: &str = "unknown";

pub fn build_machine_info(username: Option<&str>) -> MachineInfo {
    MachineInfo {
        // Never publish the operator's hostname or username.
        hostname: "[REDACTED]".to_string(),
        platform: platform_name(),
        release: os_release(),
        arch: std::env::consts::ARCH.to_string(),
        os_type: os_type(),
        total_memory_bytes: total_memory_bytes(),
        node_version: NO_NODE_RUNTIME.to_string(),
        pid: std::process::id(),
        ppid: parent_pid(),
        cwd: std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        username: username.map(|_| "[REDACTED]".to_string()),
    }
}

/// `open-design-diagnostics-2026-10-17T13-00-47Z.zip` (parity:
/// `diagnosticsFileName` — colons and dots become dashes and the millisecond
/// part is dropped).
pub fn diagnostics_file_name(prefix: &str) -> String {
    diagnostics_file_name_at(prefix, std::time::SystemTime::now())
}

pub fn diagnostics_file_name_at(prefix: &str, now: std::time::SystemTime) -> String {
    let iso = UtcParts::from_system_time(now).to_iso_millis();
    let dashed = iso.replace([':', '.'], "-");
    // `-\d{3}Z$` → `Z` (the milliseconds just became `-465Z`).
    let stem = dashed
        .strip_suffix('Z')
        .map(|head| {
            let bytes = head.as_bytes();
            if bytes.len() > 4
                && bytes[bytes.len() - 4] == b'-'
                && bytes[bytes.len() - 3..].iter().all(u8::is_ascii_digit)
            {
                format!("{}Z", &head[..head.len() - 4])
            } else {
                dashed.clone()
            }
        })
        .unwrap_or(dashed);
    format!("{prefix}-{stem}.zip")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn manifest_omits_absent_optional_fields() {
        let context = DiagnosticsContext {
            app: DiagnosticsAppInfo {
                name: "open-design".into(),
                version: None,
                channel: None,
                packaged: None,
            },
            source: "test".into(),
            namespace: None,
            endpoint: None,
            daemon_reachable: None,
            extra: None,
            warnings: vec![],
        };
        let manifest = build_manifest(&context, &[]);
        let value = serde_json::to_value(&manifest).unwrap();
        assert_eq!(value["app"], json!({ "name": "open-design" }));
        assert!(value.get("namespace").is_none());
        assert!(value.get("endpoint").is_none());
        assert!(value.get("daemonReachable").is_none());
        assert!(value.get("extra").is_none());
        assert_eq!(value["warnings"], json!([]));
        assert!(value.get("exportedAt").is_some());
    }

    #[test]
    fn unreadable_files_warn_and_stay_listed() {
        let context = DiagnosticsContext {
            app: DiagnosticsAppInfo {
                name: "open-design".into(),
                version: Some("1.2.3".into()),
                channel: None,
                packaged: Some(false),
            },
            source: "test".into(),
            namespace: Some("default".into()),
            endpoint: None,
            daemon_reachable: None,
            extra: None,
            warnings: vec![],
        };
        let files = vec![CollectedFile {
            name: "logs/missing.log".into(),
            absolute_path: "/tmp/no-such.log".into(),
            content: None,
            bytes: 0,
            error: Some("ENOENT: no such file or directory".into()),
        }];
        let manifest = build_manifest(&context, &files);
        let value = serde_json::to_value(&manifest).unwrap();
        assert_eq!(value["app"]["name"], json!("open-design"));
        assert_eq!(value["namespace"], json!("default"));
        assert_eq!(value["files"][0]["name"], json!("logs/missing.log"));
        assert_eq!(value["files"][0]["bytes"], json!(0));
        assert!(value["files"][0]["error"].is_string());
        assert_eq!(value["warnings"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn machine_info_redacts_identity() {
        let info = build_machine_info(Some("alice"));
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["hostname"], json!("[REDACTED]"));
        assert_eq!(value["username"], json!("[REDACTED]"));
        assert!(value["platform"].is_string());
        assert!(value["nodeVersion"].is_string());
        let without = serde_json::to_value(build_machine_info(None)).unwrap();
        assert!(without.get("username").is_none());
    }

    #[test]
    fn file_name_matches_the_iso_stamp_treatment() {
        let now = std::time::UNIX_EPOCH + Duration::from_millis(1_792_242_047_465);
        assert_eq!(
            diagnostics_file_name_at("open-design-diagnostics", now),
            "open-design-diagnostics-2026-10-17T13-00-47Z.zip"
        );
    }
}
