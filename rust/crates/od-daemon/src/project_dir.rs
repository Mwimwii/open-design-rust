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

/// Managed dir = `<data root>/projects/<id>`; imported projects use their
/// external `metadata.baseDir`. Port of `resolveProjectDir` (sandbox-mode
/// allowlist rules are a follow-up, tracked in beads).
pub fn resolved_dir(config: &DaemonConfig, project: &ProjectRow) -> Result<String, String> {
    if let Some(base_dir) = external_base_dir(project) {
        return Ok(base_dir);
    }
    if !storage::is_safe_id(&project.id) {
        return Err(format!("invalid project id: {}", project.id));
    }
    Ok(config.paths.projects_dir().join(&project.id).to_string_lossy().into_owned())
}

/// Filesystem form of [`resolved_dir`] for the project file routes.
pub fn project_fs_base(config: &DaemonConfig, project: &ProjectRow) -> Result<PathBuf, String> {
    Ok(PathBuf::from(resolved_dir(config, project)?))
}

/// `metadata.baseDir` is the external workspace root when present. The
/// TypeScript `usesExternalProjectRoot` additionally consults sandbox
/// allowlists; in sandbox mode we conservatively stay on the managed root.
pub fn external_base_dir(project: &ProjectRow) -> Option<String> {
    let metadata: Value = serde_json::from_str(project.metadata_json.as_deref()?).ok()?;
    let base_dir = metadata.get("baseDir")?.as_str()?.trim();
    if base_dir.is_empty() {
        return None;
    }
    let sandbox = matches!(
        std::env::var("OD_SANDBOX_MODE").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    );
    if sandbox {
        // TODO(sandbox): port `isSandboxImportedProjectRootAllowed`.
        return None;
    }
    Some(base_dir.to_string())
}

/// Whether `metadata.baseDir` names an absolute external workspace root
/// (parity: `hasExternalProjectRoot`). Imported-folder visibility rules key
/// off this metadata check alone, independent of the sandbox allowlist that
/// [`external_base_dir`] consults.
pub fn has_external_project_root(project: &ProjectRow) -> bool {
    let Some(raw) = project.metadata_json.as_deref() else {
        return false;
    };
    let Ok(metadata) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    let Some(base_dir) = metadata.get("baseDir").and_then(Value::as_str) else {
        return false;
    };
    Path::new(base_dir).is_absolute()
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
