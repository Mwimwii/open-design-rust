//! Namespace-scoped paths for a packaged build — the Rust port of
//! `apps/packaged/src/paths.ts` (`resolvePackagedNamespacePaths`,
//! `resolvePackagedDataRoot`) and of the namespace reader in
//! `packages/sidecar-proto/src/index.ts` (`normalizeNamespace`).
//!
//! The packaged layout is what keeps two builds of the same app apart: a
//! stable build and a beta build, or a mac build and a Windows build, own
//! separate data roots, so neither can read or corrupt the other's projects.
//! The namespace is the directory name under the namespace base root, and the
//! data root is the `data` directory inside it.
//!
//! Per `AGENTS.md` → "Daemon data directory contract", packaged code
//! resolves the *final* namespace-scoped daemon data root before spawning the
//! daemon, and the daemon receives that final root as `OD_DATA_DIR`. Nothing
//! downstream of this module may infer a data path from an app name, a
//! channel, or a namespace.

use std::path::{Path, PathBuf};

use regex::Regex;
use std::sync::OnceLock;

/// The namespace the sidecar protocol uses when nothing else names one
/// (parity: `SIDECAR_DEFAULTS.namespace`).
pub const DEFAULT_NAMESPACE: &str = "default";

/// The app key whose log directory the packaged layout keeps (parity:
/// `APP_KEYS.DESKTOP`).
pub const DESKTOP_APP_KEY: &str = "desktop";

/// The directory that holds one namespace's data root (parity: the `data`
/// segment of the packaged layout).
pub const DATA_DIR_NAME: &str = "data";

/// The directory that holds the namespace roots (parity: the `namespaces`
/// segment of the packaged layout).
pub const NAMESPACES_DIR_NAME: &str = "namespaces";

/// The namespace layout's failures (parity: `PackagedPathAccessError` and the
/// `normalizeNamespace` throws).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NamespaceError {
    #[error("namespace must not be empty")]
    Empty,
    #[error("namespace must not contain leading or trailing whitespace")]
    Whitespace,
    #[error("namespace contains unsupported characters: {value}")]
    Characters { value: String },
    #[error("namespace must not contain path separators: {value}")]
    Separators { value: String },
    #[error(
        "Open Design's packaged runtime requires OD_DATA_DIR to be an absolute path. \
         Set OD_DATA_DIR to an absolute path and relaunch Open Design."
    )]
    DataDirNotAbsolute,
    #[error(
        "Open Design's packaged runtime requires OD_DATA_DIR to target the active namespace. \
         Use an unscoped absolute base path or relaunch the matching packaged namespace."
    )]
    DataDirNamespaceMismatch { configured: String, active: String },
}

/// Every path a namespace owns (parity: `PackagedNamespacePaths`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespacePaths {
    /// The namespace's own directory.
    pub namespace_root: PathBuf,
    /// The daemon data root of this namespace.
    pub data_root: PathBuf,
    pub cache_root: PathBuf,
    pub logs_root: PathBuf,
    pub desktop_logs_root: PathBuf,
    pub desktop_log_path: PathBuf,
    pub runtime_root: PathBuf,
    pub update_root: PathBuf,
    pub user_data_root: PathBuf,
    pub session_data_root: PathBuf,
    /// The channel root — one level above the `namespaces/` parent — so the
    /// installation id survives a reset of the namespace-scoped subtree.
    pub installation_root: PathBuf,
    pub installer_observation_root: PathBuf,
    pub resource_root: PathBuf,
}

/// The namespace name, checked (parity: `normalizeNamespace`).
///
/// A namespace is one to one hundred and twenty-eight characters: it starts
/// with a letter or a digit and continues with letters, digits, dots,
/// underscores, or dashes. It never contains a path separator, so a namespace
/// can never escape its own root.
pub fn normalize_namespace(value: &str) -> Result<String, NamespaceError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(NamespaceError::Empty);
    }
    if trimmed != value {
        return Err(NamespaceError::Whitespace);
    }
    if !namespace_pattern().is_match(value) {
        return Err(NamespaceError::Characters {
            value: value.to_string(),
        });
    }
    if value.contains('/') || value.contains('\\') {
        return Err(NamespaceError::Separators {
            value: value.to_string(),
        });
    }
    Ok(value.to_string())
}

/// The paths of one namespace (parity: `resolvePackagedNamespacePaths`).
///
/// `namespace_base_root` is the directory that holds the namespace roots
/// (the packaged layout puts it at `<user data>/namespaces`). `data_dir` is
/// the operator's `OD_DATA_DIR` override, if any: it names a *base* root, and
/// the namespace's data root lives inside it — unless it already points at
/// this namespace's data root, which is then used as it stands.
pub fn resolve_namespace_paths(
    namespace_base_root: &Path,
    namespace: &str,
    resource_root: &Path,
    data_dir: Option<&Path>,
) -> Result<NamespacePaths, NamespaceError> {
    let namespace = normalize_namespace(namespace)?;
    let namespace_root = namespace_base_root.join(&namespace);
    let data_root = resolve_data_root(namespace_base_root, &namespace, data_dir)?;
    // The channel root is the parent of the `namespaces/` directory.
    let installation_root = namespace_base_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| namespace_base_root.to_path_buf());
    Ok(NamespacePaths {
        cache_root: namespace_root.join("cache"),
        desktop_log_path: namespace_root
            .join("logs")
            .join(DESKTOP_APP_KEY)
            .join("latest.log"),
        desktop_logs_root: namespace_root.join("logs").join(DESKTOP_APP_KEY),
        data_root: data_root.clone(),
        installation_root,
        installer_observation_root: data_root.join("observations").join("installer"),
        logs_root: namespace_root.join("logs"),
        namespace_root: namespace_root.clone(),
        resource_root: resource_root.to_path_buf(),
        runtime_root: namespace_root.join("runtime"),
        session_data_root: namespace_root.join("user-data").join("session"),
        update_root: namespace_root.join("updates"),
        user_data_root: namespace_root.join("user-data"),
    })
}

/// The daemon data root of a namespace (parity: `resolvePackagedDataRoot`).
///
/// Without an override the data root is `<base>/<namespace>/data`. With one,
/// the override is a base root: the data root is `<override>/namespaces/
/// <namespace>/data`. An override that already points at
/// `namespaces/<namespace>/data` is used as it stands, and one that points at
/// *another* namespace's data root is refused rather than silently reused.
pub fn resolve_data_root(
    namespace_base_root: &Path,
    namespace: &str,
    data_dir: Option<&Path>,
) -> Result<PathBuf, NamespaceError> {
    let Some(override_root) = data_dir else {
        return Ok(namespace_base_root.join(namespace).join(DATA_DIR_NAME));
    };
    let expanded = expand_home_prefix(override_root);
    if !expanded.is_absolute() {
        return Err(NamespaceError::DataDirNotAbsolute);
    }
    match scoped_data_root_namespace(&expanded) {
        Some(scoped) if scoped != namespace => Err(NamespaceError::DataDirNamespaceMismatch {
            configured: scoped,
            active: namespace.to_string(),
        }),
        Some(_) => Ok(expanded),
        None => Ok(expanded
            .join(NAMESPACES_DIR_NAME)
            .join(namespace)
            .join(DATA_DIR_NAME)),
    }
}

/// The namespace a path is already scoped to (parity:
/// `getScopedPackagedDataRootNamespace`).
///
/// A path is scoped when its last three segments are `namespaces`,
/// `<namespace>`, `data`.
pub fn scoped_data_root_namespace(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let last = parts.len().checked_sub(1)?;
    if last < 2 {
        return None;
    }
    if parts[last - 2] != NAMESPACES_DIR_NAME || parts[last] != DATA_DIR_NAME {
        return None;
    }
    Some(parts[last - 1].clone())
}

/// A leading `~`, `$HOME`, or `${HOME}` becomes the home directory (parity:
/// `expandHomePrefix`).
pub fn expand_home_prefix(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    let home = home_dir();
    let rest = if raw == "~" || raw == "$HOME" || raw == "${HOME}" {
        Some("")
    } else if let Some(rest) = raw.strip_prefix("~/") {
        Some(rest)
    } else if let Some(rest) = raw.strip_prefix("$HOME/") {
        Some(rest)
    } else if let Some(rest) = raw.strip_prefix("${HOME}/") {
        Some(rest)
    } else {
        None
    };
    match (home, rest) {
        (Some(home), Some(rest)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// The user's home directory.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }
}

fn namespace_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("namespace pattern")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_root() -> PathBuf {
        PathBuf::from("/home/user/Library/Application Support/Open Design/namespaces")
    }

    #[test]
    fn a_namespace_must_be_a_path_safe_name() {
        assert_eq!(normalize_namespace("default").expect("default"), "default");
        assert_eq!(
            normalize_namespace("release-beta-win").expect("release namespace"),
            "release-beta-win"
        );
        assert_eq!(normalize_namespace(""), Err(NamespaceError::Empty));
        assert_eq!(
            normalize_namespace(" default"),
            Err(NamespaceError::Whitespace)
        );
        assert!(matches!(
            normalize_namespace("-leading-dash"),
            Err(NamespaceError::Characters { .. })
        ));
        assert!(matches!(
            normalize_namespace("with/slash"),
            Err(NamespaceError::Characters { .. })
        ));
        assert!(matches!(
            normalize_namespace(&"a".repeat(129)),
            Err(NamespaceError::Characters { .. })
        ));
    }

    #[test]
    fn the_data_root_lives_inside_the_namespace() {
        let paths = resolve_namespace_paths(
            &base_root(),
            "release-beta",
            Path::new("/app/resources"),
            None,
        )
        .expect("paths");
        assert_eq!(
            paths.data_root,
            base_root().join("release-beta").join("data")
        );
        assert_eq!(paths.namespace_root, base_root().join("release-beta"));
        assert_eq!(paths.cache_root, base_root().join("release-beta/cache"));
        assert_eq!(paths.logs_root, base_root().join("release-beta/logs"));
        assert_eq!(
            paths.desktop_log_path,
            base_root().join("release-beta/logs/desktop/latest.log")
        );
        assert_eq!(paths.update_root, base_root().join("release-beta/updates"));
        // The channel root is the parent of the namespaces directory.
        assert_eq!(
            paths.installation_root,
            PathBuf::from("/home/user/Library/Application Support/Open Design")
        );
    }

    #[test]
    fn an_override_names_a_base_root() {
        let paths = resolve_namespace_paths(
            &base_root(),
            "default",
            Path::new("/app/resources"),
            Some(Path::new("/var/data")),
        )
        .expect("paths");
        assert_eq!(
            paths.data_root,
            PathBuf::from("/var/data/namespaces/default/data")
        );
    }

    #[test]
    fn an_override_already_scoped_to_the_namespace_is_used_as_it_stands() {
        let paths = resolve_namespace_paths(
            &base_root(),
            "default",
            Path::new("/app/resources"),
            Some(Path::new("/var/data/namespaces/default/data")),
        )
        .expect("paths");
        assert_eq!(
            paths.data_root,
            PathBuf::from("/var/data/namespaces/default/data")
        );
    }

    #[test]
    fn an_override_scoped_to_another_namespace_is_refused() {
        let error = resolve_namespace_paths(
            &base_root(),
            "default",
            Path::new("/app/resources"),
            Some(Path::new("/var/data/namespaces/release-beta/data")),
        )
        .expect_err("mismatch");
        assert_eq!(
            error,
            NamespaceError::DataDirNamespaceMismatch {
                configured: "release-beta".to_string(),
                active: "default".to_string(),
            }
        );
    }

    #[test]
    fn a_relative_override_is_refused() {
        let error = resolve_namespace_paths(
            &base_root(),
            "default",
            Path::new("/app/resources"),
            Some(Path::new("relative/data")),
        )
        .expect_err("relative");
        assert_eq!(error, NamespaceError::DataDirNotAbsolute);
    }

    #[test]
    fn the_scoped_namespace_is_read_off_the_path_tail() {
        assert_eq!(
            scoped_data_root_namespace(Path::new("/var/data/namespaces/default/data")),
            Some("default".to_string())
        );
        assert_eq!(
            scoped_data_root_namespace(Path::new("/var/data/namespaces/default")),
            None
        );
        assert_eq!(scoped_data_root_namespace(Path::new("/var/data")), None);
    }
}
