//! Project creation helpers — parity port of the pure parts of
//! `apps/daemon/src/routes/project/index.ts` (`POST /api/projects`) and
//! `apps/daemon/src/import-export-routes.ts` (`POST /api/import/folder`).
//!
//! The privileged decisions live here so the routes stay thin: which roots a
//! user may import, how linked working directories are validated, and which
//! file a freshly imported folder should open.
//!
//! DOCUMENTED GAPS
//!
//! * Workspace attribution (`x-od-workspace-*`) and skill/design-system
//!   catalog validation are not ported; the Rust daemon has no workspace
//!   identity or catalog surface yet. Ids pass through unchanged, exactly like
//!   a headerless TypeScript create.
//! * Plugin snapshot resolution is not ported, so the create response never
//!   carries `appliedPluginSnapshotId`.
//! * The sandbox-mode root refusal (`sandboxImportedProjectRootUnavailableReason`)
//!   has no counterpart: this daemon has no sandbox mode, so every import is
//!   an ordinary local import.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// System roots a project may never use (parity: `BLOCKED_CANONICAL`).
const BLOCKED_SYSTEM_DIRS: &[&str] = &["/etc", "/proc", "/sys", "/dev", "/boot"];

/// Home-relative credential directories (parity: `credentialDirs`).
const CREDENTIAL_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker"];

fn is_filesystem_root(path: &Path) -> bool {
    path.parent().is_none()
}

/// Parity: `isBlocked` from `apps/daemon/src/linked-dirs.ts`. A path is blocked
/// when it is the filesystem root, inside a blocked system directory, or an
/// ancestor of one (importing `/` is already refused separately).
pub fn is_blocked_system_dir(real_path: &Path) -> bool {
    if is_filesystem_root(real_path) {
        return true;
    }
    BLOCKED_SYSTEM_DIRS.iter().any(|blocked| {
        let blocked = Path::new(blocked);
        real_path == blocked
            || real_path.starts_with(blocked)
            || blocked.starts_with(real_path)
    })
}

/// Parity: `blockedProjectRootReason` — system and credential directories.
pub fn blocked_project_root_reason(real_path: &Path) -> Option<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| std::fs::canonicalize(home).ok());
    let in_credential_dir = home
        .as_deref()
        .map(|home| {
            CREDENTIAL_DIRS
                .iter()
                .any(|dir| real_path.starts_with(home.join(dir)))
        })
        .unwrap_or(false);
    let is_home = home.as_deref() == Some(real_path);
    if is_blocked_system_dir(real_path) || is_home || in_credential_dir {
        return Some("cannot use a system or credential directory as a project root".to_string());
    }
    None
}

/// Parity: `validateLinkedDirs` — every entry must be an absolute, existing,
/// non-blocked directory; the result is de-duplicated and canonical.
pub fn validate_linked_dirs(dirs: &Value) -> Result<Vec<String>, String> {
    let Some(items) = dirs.as_array() else {
        return Err("linkedDirs must be an array".to_string());
    };
    let mut validated = Vec::new();
    for item in items {
        let Some(dir) = item.as_str().map(str::trim).filter(|dir| !dir.is_empty()) else {
            return Err("each linked dir must be a non-empty string".to_string());
        };
        if !Path::new(dir).is_absolute() {
            return Err(format!("linked dir must be an absolute path: {dir}"));
        }
        let Ok(real_path) = std::fs::canonicalize(dir) else {
            return Err(format!(
                "directory does not exist or is not accessible: {dir}"
            ));
        };
        match std::fs::metadata(&real_path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(format!("not a directory: {dir}")),
            Err(_) => {
                return Err(format!(
                    "directory does not exist or is not accessible: {dir}"
                ))
            }
        }
        if is_blocked_system_dir(&real_path) {
            return Err(format!("system directory not allowed: {dir}"));
        }
        let canonical = real_path.to_string_lossy().into_owned();
        if !validated.contains(&canonical) {
            validated.push(canonical);
        }
    }
    Ok(validated)
}

/// Parity: `detectEntryFile` — prefer `index.html`, then the first HTML file.
pub fn detect_entry_file(dir: &Path) -> Option<String> {
    if dir.join("index.html").is_file() {
        return Some("index.html".to_string());
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().map(|kind| kind.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".html") || lower.ends_with(".htm") {
            return Some(name);
        }
    }
    None
}

/// Parity: `ORCHESTRATOR_WORKSPACE_KIND`.
const ORCHESTRATOR_WORKSPACE_KIND: &str = "scratch";

/// Parity: `ORCHESTRATOR_WRITEBACK`.
const ORCHESTRATOR_WRITEBACK: &str = "external";

/// Parity: `ORCHESTRATOR_WORKSPACE_KEYS`.
const ORCHESTRATOR_WORKSPACE_KEYS: &[&str] = &[
    "kind",
    "sourceLabel",
    "sourceRef",
    "baseRevision",
    "writeback",
];

/// The parsed `orchestratorWorkspace` document (parity:
/// `OrchestratorWorkspace`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorWorkspace {
    pub kind: String,
    pub writeback: String,
    pub source_label: Option<String>,
    pub source_ref: Option<String>,
    pub base_revision: Option<String>,
}

impl OrchestratorWorkspace {
    /// The metadata document the project row stores.
    pub fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("kind".to_string(), Value::String(self.kind.clone()));
        object.insert(
            "writeback".to_string(),
            Value::String(self.writeback.clone()),
        );
        if let Some(label) = &self.source_label {
            object.insert("sourceLabel".to_string(), Value::String(label.clone()));
        }
        if let Some(reference) = &self.source_ref {
            object.insert("sourceRef".to_string(), Value::String(reference.clone()));
        }
        if let Some(revision) = &self.base_revision {
            object.insert(
                "baseRevision".to_string(),
                Value::String(revision.clone()),
            );
        }
        Value::Object(object)
    }
}

/// Parity: `stringField` — a trimmed, non-empty string or nothing.
fn string_field(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Parity: `parseOrchestratorWorkspace` — the scratch-workspace contract the
/// import and working-dir routes validate.
///
/// `None` in means "no orchestrator workspace" (the field is absent or null);
/// `Some(Err(message))` is the 400 the route answers with.
pub fn parse_orchestrator_workspace(
    value: Option<&Value>,
) -> Result<Option<OrchestratorWorkspace>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(record) = value.as_object() else {
        return Err("orchestratorWorkspace must be an object when provided".to_string());
    };
    if let Some(unknown) = record
        .keys()
        .find(|key| !ORCHESTRATOR_WORKSPACE_KEYS.contains(&key.as_str()))
    {
        return Err(format!(
            "orchestratorWorkspace contains unsupported field: {unknown}"
        ));
    }
    if string_field(record.get("kind")).as_deref() != Some(ORCHESTRATOR_WORKSPACE_KIND) {
        return Err("orchestratorWorkspace.kind must be \"scratch\"".to_string());
    }
    if let Some(writeback) = record.get("writeback") {
        if !writeback.is_null()
            && string_field(Some(writeback)).as_deref() != Some(ORCHESTRATOR_WRITEBACK)
        {
            return Err("orchestratorWorkspace.writeback must be \"external\"".to_string());
        }
    }
    for key in ["sourceLabel", "sourceRef", "baseRevision"] {
        if let Some(field) = record.get(key) {
            if !field.is_null() && string_field(Some(field)).is_none() {
                return Err(format!(
                    "orchestratorWorkspace.{key} must be a non-empty string when provided"
                ));
            }
        }
    }
    Ok(Some(OrchestratorWorkspace {
        kind: ORCHESTRATOR_WORKSPACE_KIND.to_string(),
        writeback: ORCHESTRATOR_WRITEBACK.to_string(),
        source_label: string_field(record.get("sourceLabel")),
        source_ref: string_field(record.get("sourceRef")),
        base_revision: string_field(record.get("baseRevision")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn system_and_credential_roots_are_refused() {
        assert!(is_blocked_system_dir(Path::new("/etc")));
        assert!(is_blocked_system_dir(Path::new("/etc/nginx")));
        assert!(is_blocked_system_dir(Path::new("/")));
        assert!(!is_blocked_system_dir(Path::new("/home/user/work")));
        assert_eq!(
            blocked_project_root_reason(Path::new("/proc")),
            Some("cannot use a system or credential directory as a project root".to_string())
        );
    }

    #[test]
    fn linked_dirs_must_be_absolute_existing_directories() {
        assert_eq!(
            validate_linked_dirs(&json!("nope")),
            Err("linkedDirs must be an array".to_string())
        );
        assert!(validate_linked_dirs(&json!([""])).is_err());
        assert_eq!(
            validate_linked_dirs(&json!(["relative/dir"])),
            Err("linked dir must be an absolute path: relative/dir".to_string())
        );
        assert!(validate_linked_dirs(&json!(["/definitely/not/here"])).is_err());
        assert_eq!(
            validate_linked_dirs(&json!(["/etc"])),
            Err("system directory not allowed: /etc".to_string())
        );
        let dir = std::env::temp_dir().join(format!("od-linked-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let validated =
            validate_linked_dirs(&json!([dir.to_string_lossy(), dir.to_string_lossy()]))
                .expect("valid");
        assert_eq!(validated.len(), 1, "duplicates collapse");
    }

    #[test]
    fn entry_file_prefers_index_then_the_first_html_file() {
        let dir = std::env::temp_dir().join(format!("od-entry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(detect_entry_file(&dir), None);
        std::fs::write(dir.join("about.html"), b"<html></html>").expect("write");
        assert_eq!(detect_entry_file(&dir).as_deref(), Some("about.html"));
        std::fs::write(dir.join("index.html"), b"<html></html>").expect("write");
        assert_eq!(detect_entry_file(&dir).as_deref(), Some("index.html"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_orchestrator_workspace_contract_is_enforced() {
        assert_eq!(parse_orchestrator_workspace(None), Ok(None));
        assert_eq!(parse_orchestrator_workspace(Some(&Value::Null)), Ok(None));

        let parsed = parse_orchestrator_workspace(Some(&json!({
            "kind": "scratch",
            "writeback": "external",
            "sourceLabel": "  deck  ",
            "sourceRef": "file-1",
        })))
        .expect("valid")
        .expect("present");
        assert_eq!(parsed.kind, "scratch");
        assert_eq!(parsed.writeback, "external");
        assert_eq!(parsed.source_label.as_deref(), Some("deck"));
        assert_eq!(parsed.source_ref.as_deref(), Some("file-1"));
        assert_eq!(parsed.base_revision, None);

        assert_eq!(
            parse_orchestrator_workspace(Some(&json!("scratch"))),
            Err("orchestratorWorkspace must be an object when provided".to_string())
        );
        assert_eq!(
            parse_orchestrator_workspace(Some(&json!({ "kind": "team" }))),
            Err("orchestratorWorkspace.kind must be \"scratch\"".to_string())
        );
        assert_eq!(
            parse_orchestrator_workspace(Some(&json!({
                "kind": "scratch",
                "writeback": "internal",
            }))),
            Err("orchestratorWorkspace.writeback must be \"external\"".to_string())
        );
        assert_eq!(
            parse_orchestrator_workspace(Some(&json!({
                "kind": "scratch",
                "sourceLabel": 7,
            }))),
            Err("orchestratorWorkspace.sourceLabel must be a non-empty string when provided"
                .to_string())
        );
        assert_eq!(
            parse_orchestrator_workspace(Some(&json!({
                "kind": "scratch",
                "unknown": "x",
            }))),
            Err("orchestratorWorkspace contains unsupported field: unknown".to_string())
        );
    }
}
