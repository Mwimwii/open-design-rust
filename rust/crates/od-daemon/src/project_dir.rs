//! Project directory resolution and confined path joins — parity ports of
//! `resolveProjectDir`, `hasExternalProjectRoot`, `validateProjectPath`,
//! `assertVisibleForImportedProject`, and `resolveSafeReal` from
//! `apps/daemon/src/projects.ts`.
//!
//! Every path arriving from HTTP funnels through [`validate_project_path`] +
//! [`resolve_under`] so a `path` parameter can never escape the project
//! directory, including via symlinks that point outside it.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::DaemonConfig;
use crate::storage::{self, ProjectRow};

/// Segments no project-relative path may contain (parity:
/// `RESERVED_PROJECT_FILE_SEGMENTS`).
const RESERVED_PROJECT_FILE_SEGMENTS: &[&str] = &[".file-versions", ".live-artifacts"];

/// Parity: `SANDBOX_MODE_ENV`.
pub const SANDBOX_MODE_ENV: &str = "OD_SANDBOX_MODE";

/// Parity: `SANDBOX_IMPORT_ALLOWED_ROOTS_ENV`.
pub const SANDBOX_IMPORT_ALLOWED_ROOTS_ENV: &str = "OD_SANDBOX_IMPORT_ALLOWED_ROOTS";

/// Parity: `SANDBOX_IMPORTED_PROJECT_UNAVAILABLE_MESSAGE`.
pub const SANDBOX_IMPORTED_PROJECT_UNAVAILABLE_MESSAGE: &str = "Imported-folder projects are not available in OD_SANDBOX_MODE unless their root is under OD_SANDBOX_IMPORT_ALLOWED_ROOTS.";

/// Parity: `SANDBOX_IMPORT_ALLOWED_ROOTS_INVALID_MESSAGE` + the ` Got: <root>`
/// suffix `configuredSandboxImportRoots` appends.
const SANDBOX_IMPORT_ALLOWED_ROOTS_INVALID_MESSAGE: &str =
    "OD_SANDBOX_IMPORT_ALLOWED_ROOTS entries must be absolute paths.";

/// Parity: the message `isSandboxModeEnabled` throws for an unrecognized
/// `OD_SANDBOX_MODE` value (the falsy set joins with a trailing ", " because
/// its last member is the empty string).
const SANDBOX_MODE_INVALID_MESSAGE: &str =
    "OD_SANDBOX_MODE must be one of 1, true, yes, on or 0, false, no, off, ";

/// Why a project directory could not be resolved. Mapped to HTTP responses by
/// the route layer.
#[derive(Debug, thiserror::Error)]
pub enum ProjectDirError {
    /// Sandbox refusal or a malformed sandbox env value (parity: thrown from
    /// `isSandboxModeEnabled` / `SandboxImportedProjectError`) → HTTP 400.
    #[error("{0}")]
    Rejected(String),

    /// `invalid project id: <id>` from `resolveProjectDir` → HTTP 500
    /// (`PROJECT_DIR_UNRESOLVED`), the route mapping this crate already uses.
    #[error("{0}")]
    Unresolved(String),
}

/// Managed dir = `<data root>/projects/<id>`; imported projects use their
/// external `metadata.baseDir`. Port of `resolveProjectDir`: the sandbox
/// allowlist is consulted before any directory is handed out.
pub fn resolved_dir(config: &DaemonConfig, project: &ProjectRow) -> Result<String, ProjectDirError> {
    assert_sandbox_project_root_available(project)?;
    if let Some(base_dir) = external_base_dir(project)? {
        return Ok(base_dir);
    }
    if !storage::is_safe_id(&project.id) {
        return Err(ProjectDirError::Unresolved(format!(
            "invalid project id: {}",
            project.id
        )));
    }
    Ok(config.paths.projects_dir().join(&project.id).to_string_lossy().into_owned())
}

/// Filesystem form of [`resolved_dir`] for the project file routes.
pub fn project_fs_base(
    config: &DaemonConfig,
    project: &ProjectRow,
) -> Result<PathBuf, ProjectDirError> {
    Ok(PathBuf::from(resolved_dir(config, project)?))
}

/// `metadata.baseDir` when it names an absolute external root this process may
/// actually use — parity: `usesExternalProjectRoot` followed by
/// `path.normalize(metadata.baseDir)`.
pub fn external_base_dir(project: &ProjectRow) -> Result<Option<String>, ProjectDirError> {
    let metadata = project_metadata(project);
    let Some(base_dir) = metadata.as_ref().and_then(|m| m.get("baseDir")).and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let normalized = normalize_path(base_dir);
    if !Path::new(&normalized).is_absolute() {
        return Ok(None);
    }
    if is_orchestrator_scratch_workspace(metadata.as_ref()) {
        return Ok(Some(normalized));
    }
    if !is_sandbox_mode_enabled()? {
        return Ok(Some(normalized));
    }
    if is_sandbox_imported_project_root_allowed(base_dir)? {
        Ok(Some(normalized))
    } else {
        Err(ProjectDirError::Rejected(
            SANDBOX_IMPORTED_PROJECT_UNAVAILABLE_MESSAGE.to_string(),
        ))
    }
}

/// Whether `metadata.baseDir` names an absolute external workspace root
/// (parity: `hasExternalProjectRoot`). Imported-folder visibility rules key
/// off this metadata check alone, independent of the sandbox allowlist that
/// [`external_base_dir`] consults.
pub fn has_external_project_root(project: &ProjectRow) -> bool {
    let Some(metadata) = project_metadata(project) else {
        return false;
    };
    has_external_metadata_root(&metadata)
}

/// Port of `assertSandboxProjectRootAvailable`. Evaluated in the same order as
/// the TypeScript condition so a malformed `OD_SANDBOX_MODE` still throws for
/// projects that are not imported folders at all.
pub fn assert_sandbox_project_root_available(project: &ProjectRow) -> Result<(), ProjectDirError> {
    if !is_sandbox_mode_enabled()? {
        return Ok(());
    }
    let Some(metadata) = project_metadata(project) else {
        return Ok(());
    };
    if !has_external_metadata_root(&metadata) {
        return Ok(());
    }
    if is_orchestrator_scratch_workspace(Some(&metadata)) {
        return Ok(());
    }
    let Some(base_dir) = metadata.get("baseDir").and_then(Value::as_str) else {
        return Ok(());
    };
    if is_sandbox_imported_project_root_allowed(base_dir)? {
        return Ok(());
    }
    Err(ProjectDirError::Rejected(
        SANDBOX_IMPORTED_PROJECT_UNAVAILABLE_MESSAGE.to_string(),
    ))
}

/// `projects.metadata_json` parsed the way `normalizeProject` does: absent or
/// unparsable metadata is indistinguishable from none.
fn project_metadata(project: &ProjectRow) -> Option<Value> {
    serde_json::from_str(project.metadata_json.as_deref()?).ok()
}

fn has_external_metadata_root(metadata: &Value) -> bool {
    metadata
        .get("baseDir")
        .and_then(Value::as_str)
        .is_some_and(|base_dir| Path::new(&normalize_path(base_dir)).is_absolute())
}

/// Port of `isSandboxModeEnabled`: unset means off, the two documented value
/// sets are honored, everything else throws.
pub fn is_sandbox_mode_enabled() -> Result<bool, ProjectDirError> {
    let Ok(raw) = std::env::var(SANDBOX_MODE_ENV) else {
        return Ok(false);
    };
    let value = raw.trim().to_lowercase();
    match value.as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        _ => Err(ProjectDirError::Rejected(
            SANDBOX_MODE_INVALID_MESSAGE.to_string(),
        )),
    }
}

/// Port of `isSandboxImportedProjectRootAllowed`.
pub fn is_sandbox_imported_project_root_allowed(
    project_root: &str,
) -> Result<bool, ProjectDirError> {
    if !is_sandbox_mode_enabled()? {
        return Ok(true);
    }
    let candidate = canonicalize_path_for_containment(project_root);
    Ok(sandbox_import_allowed_root_paths()?
        .iter()
        .any(|root| is_path_inside_dir(root, &candidate)))
}

/// Port of `configuredSandboxImportRoots` + the `canonicalizePathForContainment`
/// pass `sandboxImportAllowedRoots` maps it through.
fn sandbox_import_allowed_root_paths() -> Result<Vec<String>, ProjectDirError> {
    let Ok(raw) = std::env::var(SANDBOX_IMPORT_ALLOWED_ROOTS_ENV) else {
        return Ok(Vec::new());
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let delimiter = if cfg!(windows) { ';' } else { ':' };
    let roots: Vec<&str> = raw
        .split(delimiter)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if let Some(relative) = roots
        .iter()
        .find(|root| !Path::new(&normalize_path(root)).is_absolute())
    {
        return Err(ProjectDirError::Rejected(format!(
            "{SANDBOX_IMPORT_ALLOWED_ROOTS_INVALID_MESSAGE} Got: {relative}"
        )));
    }
    Ok(roots
        .into_iter()
        .map(canonicalize_path_for_containment)
        .collect())
}

/// `path.normalize` then `fs.realpathSync.native`, falling back to the
/// normalized path when the target does not exist yet.
fn canonicalize_path_for_containment(value: &str) -> String {
    let normalized = normalize_path(value);
    match std::fs::canonicalize(&normalized) {
        Ok(real) => real.to_string_lossy().into_owned(),
        Err(_) => normalized,
    }
}

/// Parity: `isPathInsideDir`.
fn is_path_inside_dir(root: &str, candidate: &str) -> bool {
    let relative = path_relative(root, candidate);
    relative.is_empty() || (!relative.starts_with("..") && !Path::new(&relative).is_absolute())
}

/// Port of Node's posix `path.relative`: both inputs resolve against the
/// working directory, normalize, then the answer is the segment walk between
/// them.
fn path_relative(from: &str, to: &str) -> String {
    let from = resolve_path(from);
    let to = resolve_path(to);
    if from == to {
        return String::new();
    }
    let from_parts = path_segments(&from);
    let to_parts = path_segments(&to);
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts: Vec<&str> = vec![".."; from_parts.len() - common];
    parts.extend_from_slice(&to_parts[common..]);
    parts.join("/")
}

/// Port of Node's posix `path.resolve`: relative inputs resolve against the
/// working directory and the result is normalized with the trailing separator
/// dropped (`path.resolve('/a/b/') === '/a/b'`).
fn resolve_path(value: &str) -> String {
    let joined = if Path::new(value).is_absolute() {
        value.to_string()
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        format!("{}/{}", cwd.to_string_lossy(), value)
    };
    let normalized = normalize_path(&joined);
    match normalized.strip_suffix('/') {
        Some("") => "/".to_string(),
        Some(stripped) => stripped.to_string(),
        None => normalized,
    }
}

fn path_segments(value: &str) -> Vec<&str> {
    value.split('/').filter(|part| !part.is_empty()).collect()
}

/// Port of Node's posix `path.normalize`.
fn normalize_path(value: &str) -> String {
    if value.is_empty() {
        return ".".to_string();
    }
    let absolute = value.starts_with('/');
    let trailing = value.ends_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in value.split('/') {
        match segment {
            "" | "." => {}
            ".." => match segments.last() {
                Some(last) if *last != ".." => {
                    segments.pop();
                }
                _ if !absolute => segments.push(".."),
                _ => {}
            },
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    if absolute {
        if joined.is_empty() {
            return "/".to_string();
        }
        return if trailing && !joined.is_empty() {
            format!("/{joined}/")
        } else {
            format!("/{joined}")
        };
    }
    if joined.is_empty() {
        return if trailing { "./".to_string() } else { ".".to_string() };
    }
    if trailing {
        format!("{joined}/")
    } else {
        joined
    }
}

/// Port of `isOrchestratorScratchWorkspace`: a parsed
/// `metadata.orchestratorWorkspace` object whose `kind` is `scratch`.
fn is_orchestrator_scratch_workspace(metadata: Option<&Value>) -> bool {
    let Some(value) = metadata.and_then(|m| m.get("orchestratorWorkspace")) else {
        return false;
    };
    if value.is_null() {
        return false;
    }
    let Some(record) = value.as_object() else {
        return false;
    };
    const KEYS: [&str; 5] = ["kind", "sourceLabel", "sourceRef", "baseRevision", "writeback"];
    if record.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return false;
    }
    if string_field(record.get("kind")) != Some("scratch") {
        return false;
    }
    if !record.get("writeback").is_none_or(Value::is_null)
        && string_field(record.get("writeback")) != Some("external")
    {
        return false;
    }
    ["sourceLabel", "sourceRef", "baseRevision"]
        .iter()
        .all(|key| {
            record.get(*key).is_none_or(Value::is_null) || string_field(record.get(*key)).is_some()
        })
}

/// Parity: `stringField` from `workspace-contract.ts` — trimmed non-empty
/// strings only.
fn string_field(value: Option<&Value>) -> Option<&str> {
    let text = value?.as_str()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Why a project-relative path was refused. Mapped to HTTP responses by the
/// route layer; messages never carry absolute paths.
#[derive(Debug, thiserror::Error)]
pub enum ProjectPathError {
    /// `validateProjectPath` rejection; message carried verbatim.
    #[error("{0}")]
    Invalid(&'static str),

    /// Hidden (dot-prefixed) segment in an imported-folder project.
    #[error("hidden path segments are not accessible in imported folders")]
    Hidden,

    /// The canonical path left the project directory through a symlink.
    #[error("path escapes project dir via symlink")]
    Escape,

    /// The target does not exist.
    #[error("file not found")]
    NotFound,

    /// Any other I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Port of `validateProjectPath`: forward slashes only, no absolute or
/// drive-qualified paths, no `.`/`..` segments, no reserved segments.
pub fn validate_project_path(raw: &str) -> Result<String, ProjectPathError> {
    if raw.trim().is_empty() {
        return Err(ProjectPathError::Invalid("invalid file name"));
    }
    let normalized = raw.replace('\\', "/");
    let drive_qualified = normalized
        .as_bytes()
        .first()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && normalized.as_bytes().get(1) == Some(&b':');
    if raw.contains('\0') || drive_qualified || normalized.starts_with('/') {
        return Err(ProjectPathError::Invalid("invalid file name"));
    }
    let parts: Vec<&str> = normalized.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|part| matches!(*part, "." | "..")) {
        return Err(ProjectPathError::Invalid("invalid file name"));
    }
    if parts
        .iter()
        .any(|part| RESERVED_PROJECT_FILE_SEGMENTS.contains(part))
    {
        return Err(ProjectPathError::Invalid("reserved project path"));
    }
    Ok(parts.join("/"))
}

/// Port of `assertVisibleForImportedProject`: imported (external `baseDir`)
/// projects refuse every hidden segment so reads cannot reach credential
/// dotfiles outside the app's managed data.
pub fn assert_visible_for_imported_project(name: &str, imported: bool) -> Result<(), ProjectPathError> {
    if !imported {
        return Ok(());
    }
    let normalized = name.replace('\\', "/");
    if normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .any(|segment| segment.starts_with('.'))
    {
        return Err(ProjectPathError::Hidden);
    }
    Ok(())
}

/// A project path resolved against the canonical (symlink-free) project root.
pub struct ResolvedFile {
    /// Canonical project directory.
    pub base: PathBuf,
    /// Canonical target path (symlinks resolved).
    pub path: PathBuf,
    /// Target relative to [`ResolvedFile::base`], forward slashes.
    pub rel: String,
}

/// Symlink-aware containment check (parity: `resolveSafeReal`): canonicalize
/// both the base and the joined candidate, then require the canonical target
/// to stay under the canonical base so descendant symlinks cannot escape the
/// project tree.
pub fn resolve_under(base: &Path, rel: &str) -> Result<ResolvedFile, ProjectPathError> {
    let base_real = std::fs::canonicalize(base).map_err(map_io)?;
    let candidate = base_real.join(rel);
    let real = std::fs::canonicalize(&candidate).map_err(map_io)?;
    if !real.starts_with(&base_real) {
        return Err(ProjectPathError::Escape);
    }
    let rel_real = real
        .strip_prefix(&base_real)
        .map(|value| value.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| rel.to_string());
    Ok(ResolvedFile {
        base: base_real,
        path: real,
        rel: rel_real,
    })
}

/// Missing paths are `NotFound` (→ 404); everything else stays an I/O error
/// (→ 400), matching the TypeScript routes' ENOENT/other split.
pub fn map_io(err: std::io::Error) -> ProjectPathError {
    if err.kind() == std::io::ErrorKind::NotFound {
        ProjectPathError::NotFound
    } else {
        ProjectPathError::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_matches_typescript_rules() {
        assert_eq!(validate_project_path("index.html").unwrap(), "index.html");
        assert_eq!(validate_project_path("a/b\\c.html").unwrap(), "a/b/c.html");
        assert!(matches!(validate_project_path(""), Err(ProjectPathError::Invalid(_))));
        assert!(matches!(validate_project_path("   "), Err(ProjectPathError::Invalid(_))));
        assert!(matches!(validate_project_path(".."), Err(ProjectPathError::Invalid(_))));
        assert!(matches!(
            validate_project_path("../../etc/passwd"),
            Err(ProjectPathError::Invalid(_))
        ));
        assert!(matches!(
            validate_project_path("/etc/passwd"),
            Err(ProjectPathError::Invalid(_))
        ));
        assert!(matches!(
            validate_project_path("C:\\windows\\system32"),
            Err(ProjectPathError::Invalid(_))
        ));
        assert!(matches!(
            validate_project_path("a\0b"),
            Err(ProjectPathError::Invalid(_))
        ));
        assert!(matches!(
            validate_project_path(".file-versions/x"),
            Err(ProjectPathError::Invalid("reserved project path"))
        ));
        assert!(matches!(
            validate_project_path(".live-artifacts/x"),
            Err(ProjectPathError::Invalid("reserved project path"))
        ));
    }

    #[test]
    fn imported_projects_reject_hidden_segments() {
        assert!(assert_visible_for_imported_project(".ssh/id_rsa", true).is_err());
        assert!(assert_visible_for_imported_project("a/.env", true).is_err());
        assert!(assert_visible_for_imported_project("a/.env", false).is_ok());
        assert!(assert_visible_for_imported_project("dir/file.html", true).is_ok());
    }

    #[test]
    fn resolve_under_confines_to_canonical_base() {
        let root = std::env::temp_dir().join(format!("od-project-dir-{}", std::process::id()));
        let base = root.join("project");
        let outside = root.join("outside");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(base.join("inside.txt"), "in").unwrap();
        std::fs::write(outside.join("secret.txt"), "out").unwrap();

        let resolved = resolve_under(&base, "inside.txt").expect("inside file");
        assert_eq!(resolved.rel, "inside.txt");
        assert!(resolved.path.ends_with("inside.txt"));

        assert!(matches!(
            resolve_under(&base, "missing.txt"),
            Err(ProjectPathError::NotFound)
        ));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret.txt"), base.join("link.txt")).unwrap();
            std::os::unix::fs::symlink(&outside, base.join("linkdir")).unwrap();
            assert!(matches!(
                resolve_under(&base, "link.txt"),
                Err(ProjectPathError::Escape)
            ));
            assert!(matches!(
                resolve_under(&base, "linkdir/secret.txt"),
                Err(ProjectPathError::Escape)
            ));
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
