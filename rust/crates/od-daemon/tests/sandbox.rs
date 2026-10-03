//! Behavior matrix for the OD_SANDBOX_MODE external-root allowlist (parity
//! with `isSandboxImportedProjectRootAllowed` /
//! `assertSandboxProjectRootAvailable` in `apps/daemon/src/projects.ts`).

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use od_core::RuntimePaths;
use od_daemon::project_dir::{
    assert_sandbox_project_root_available, external_base_dir, has_external_project_root,
    is_sandbox_imported_project_root_allowed, is_sandbox_mode_enabled, project_fs_base,
    resolved_dir, ProjectDirError, SANDBOX_IMPORTED_PROJECT_UNAVAILABLE_MESSAGE, SANDBOX_MODE_ENV,
    SANDBOX_IMPORT_ALLOWED_ROOTS_ENV,
};
use od_daemon::{DaemonConfig, ProjectRow};

/// The sandbox decision reads process-wide env vars, so every test in this
/// binary serializes on one lock and restores the env when it finishes.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(SANDBOX_MODE_ENV);
        std::env::remove_var(SANDBOX_IMPORT_ALLOWED_ROOTS_ENV);
    }
}

/// Turn the sandbox on/off with an optional allowlist, holding the env lock
/// for the rest of the test. Call at most once per test.
fn sandbox(mode: Option<&str>, roots: Option<&str>) -> EnvGuard {
    let lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::remove_var(SANDBOX_MODE_ENV);
    std::env::remove_var(SANDBOX_IMPORT_ALLOWED_ROOTS_ENV);
    if let Some(mode) = mode {
        std::env::set_var(SANDBOX_MODE_ENV, mode);
    }
    if let Some(roots) = roots {
        std::env::set_var(SANDBOX_IMPORT_ALLOWED_ROOTS_ENV, roots);
    }
    EnvGuard { _lock: lock }
}

fn set_mode(value: &str) {
    std::env::set_var(SANDBOX_MODE_ENV, value);
}

fn set_roots(value: &str) {
    std::env::set_var(SANDBOX_IMPORT_ALLOWED_ROOTS_ENV, value);
}

fn project_with_metadata(metadata: Option<serde_json::Value>) -> ProjectRow {
    ProjectRow {
        id: "p1".to_string(),
        name: "Demo".to_string(),
        skill_id: None,
        design_system_id: None,
        pending_prompt: None,
        metadata_json: metadata.map(|value| value.to_string()),
        custom_instructions: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn imported(base_dir: &str) -> ProjectRow {
    project_with_metadata(Some(serde_json::json!({ "baseDir": base_dir })))
}

fn tmp(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "od-daemon-sandbox-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.to_string_lossy().into_owned()
}

fn test_config() -> DaemonConfig {
    let data_root = std::env::temp_dir().join(format!(
        "od-daemon-sandbox-config-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&data_root);
    let paths = RuntimePaths::resolve(&data_root).expect("data dir");
    DaemonConfig {
        paths,
        bind_host: "127.0.0.1".to_string(),
        port: 0,
        api_token: None,
        api_auth_disabled: false,
        web_dist: None,
    }
}

// ---- OD_SANDBOX_MODE parsing ------------------------------------------------

#[test]
fn sandbox_mode_is_off_when_unset_and_parses_both_value_sets() {
    let guard = sandbox(None, None);
    assert!(!is_sandbox_mode_enabled().unwrap());
    for value in ["1", "true", "TRUE", " yes ", "on", "On"] {
        set_mode(value);
        assert!(is_sandbox_mode_enabled().unwrap(), "truthy: {value:?}");
    }
    for value in ["0", "false", "no", "off", "", "   "] {
        set_mode(value);
        assert!(!is_sandbox_mode_enabled().unwrap(), "falsy: {value:?}");
    }
    drop(guard);
}

#[test]
fn sandbox_mode_rejects_unrecognized_values_before_reading_the_allowlist() {
    let _guard = sandbox(Some("maybe"), None);
    let err = is_sandbox_mode_enabled().unwrap_err();
    assert!(matches!(err, ProjectDirError::Rejected(_)));
    assert_eq!(
        err.to_string(),
        "OD_SANDBOX_MODE must be one of 1, true, yes, on or 0, false, no, off, "
    );
    let err = is_sandbox_imported_project_root_allowed("/tmp").unwrap_err();
    assert_eq!(
        err.to_string(),
        "OD_SANDBOX_MODE must be one of 1, true, yes, on or 0, false, no, off, "
    );
}

// ---- allowlist containment --------------------------------------------------

#[test]
fn sandbox_off_allows_every_root() {
    let _guard = sandbox(None, None);
    assert!(is_sandbox_imported_project_root_allowed("/anything/goes").unwrap());
    assert!(is_sandbox_imported_project_root_allowed("/anything/goes").unwrap());
}

#[test]
fn sandbox_on_denies_without_an_allowlist() {
    let _guard = sandbox(Some("1"), None);
    assert!(!is_sandbox_imported_project_root_allowed("/tmp/project").unwrap());

    set_roots("");
    assert!(!is_sandbox_imported_project_root_allowed("/tmp/project").unwrap());

    set_roots("   ");
    assert!(!is_sandbox_imported_project_root_allowed("/tmp/project").unwrap());
}

#[test]
fn sandbox_on_accepts_only_roots_under_an_allowed_entry() {
    let allowed = tmp("allowed");
    let sibling = tmp("sibling");
    let separator = if cfg!(windows) { ';' } else { ':' };

    let _guard = sandbox(Some("true"), Some(&allowed));

    // The allowed root itself.
    assert!(is_sandbox_imported_project_root_allowed(&allowed).unwrap());
    // A descendant of it.
    let nested = format!("{allowed}/projects/demo");
    assert!(is_sandbox_imported_project_root_allowed(&nested).unwrap());
    // A trailing separator is not a different path.
    assert!(is_sandbox_imported_project_root_allowed(&format!("{allowed}/")).unwrap());

    // A parent of the allowed entry.
    let parent = Path::new(&allowed)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());
    assert!(!is_sandbox_imported_project_root_allowed(&parent).unwrap());
    // A sibling directory.
    assert!(!is_sandbox_imported_project_root_allowed(&sibling).unwrap());
    // A path that only shares a prefix with the allowed entry.
    assert!(!is_sandbox_imported_project_root_allowed(&format!("{allowed}-extra")).unwrap());

    // Several entries, `:` separated; every listed root opens its own subtree.
    set_roots(&format!("{sibling}{separator}{allowed}"));
    assert!(is_sandbox_imported_project_root_allowed(&nested).unwrap());
    assert!(is_sandbox_imported_project_root_allowed(&format!("{sibling}/nested")).unwrap());
    assert!(!is_sandbox_imported_project_root_allowed(&format!("{sibling}-extra/nested")).unwrap());
}

#[test]
fn sandbox_on_rejects_relative_allowlist_entries() {
    let _guard = sandbox(Some("1"), Some("relative/root:/tmp/ok"));
    let err = is_sandbox_imported_project_root_allowed("/tmp/ok").unwrap_err();
    let message = err.to_string();
    assert!(
        message.starts_with("OD_SANDBOX_IMPORT_ALLOWED_ROOTS entries must be absolute paths."),
        "unexpected message: {message}"
    );
    assert!(message.ends_with(" Got: relative/root"), "{message}");
}

#[test]
fn sandbox_allowlist_resolves_paths_before_comparing() {
    let allowed = tmp("symlink-allowed");
    let outside = tmp("symlink-outside");
    std::fs::create_dir_all(format!("{outside}/secret")).expect("secret");
    std::os::unix::fs::symlink(&outside, format!("{allowed}/link")).expect("symlink");

    let _guard = sandbox(Some("1"), Some(&allowed));

    // The allowlist entry itself is followed.
    assert!(is_sandbox_imported_project_root_allowed(&allowed).unwrap());
    // The symlink target is NOT inside the allowlist, so a path that walks
    // through it is refused even though the literal prefix matches.
    assert!(!is_sandbox_imported_project_root_allowed(&format!("{allowed}/link/secret")).unwrap());
    // An allowlist entry expressed as a symlink resolves to the real root, so
    // the real root — and its descendants — become the allowed set.
    set_roots(&format!("{allowed}/link"));
    assert!(is_sandbox_imported_project_root_allowed(&outside).unwrap());
    assert!(is_sandbox_imported_project_root_allowed(&format!("{outside}/nested")).unwrap());
    assert!(!is_sandbox_imported_project_root_allowed(&allowed).unwrap());
}

// ---- external_base_dir / assert_sandbox_project_root_available -------------

#[test]
fn external_root_is_handed_out_when_the_sandbox_is_off() {
    let _guard = sandbox(None, Some("/nothing"));
    let project = imported("/tmp/some-imported-folder");
    assert!(has_external_project_root(&project));
    assert_eq!(
        external_base_dir(&project).unwrap().as_deref(),
        Some("/tmp/some-imported-folder")
    );
    assert!(assert_sandbox_project_root_available(&project).is_ok());
}

#[test]
fn external_root_is_refused_when_the_sandbox_is_on_and_unlisted() {
    let _guard = sandbox(Some("1"), Some("/somewhere/else"));
    let project = imported("/tmp/some-imported-folder");
    let err = external_base_dir(&project).unwrap_err();
    assert!(matches!(err, ProjectDirError::Rejected(_)));
    assert_eq!(
        err.to_string(),
        SANDBOX_IMPORTED_PROJECT_UNAVAILABLE_MESSAGE
    );

    let err = assert_sandbox_project_root_available(&project).unwrap_err();
    assert_eq!(
        err.to_string(),
        SANDBOX_IMPORTED_PROJECT_UNAVAILABLE_MESSAGE
    );
}

#[test]
fn external_root_is_handed_out_when_the_sandbox_is_on_and_listed() {
    let allowed = tmp("listed");
    let base = format!("{allowed}/demo");
    std::fs::create_dir_all(&base).expect("base");
    let _guard = sandbox(Some("yes"), Some(&allowed));

    let project = imported(&base);
    assert_eq!(
        external_base_dir(&project).unwrap().as_deref(),
        Some(base.as_str())
    );
    assert!(assert_sandbox_project_root_available(&project).is_ok());
}

#[test]
fn managed_projects_ignore_the_allowlist_entirely() {
    let _guard = sandbox(Some("1"), None);
    let project = project_with_metadata(None);
    assert!(!has_external_project_root(&project));
    assert_eq!(external_base_dir(&project).unwrap(), None);
    assert!(assert_sandbox_project_root_available(&project).is_ok());

    let named = project_with_metadata(Some(serde_json::json!({"name": "no base dir"})));
    assert!(external_base_dir(&named).unwrap().is_none());
    assert!(assert_sandbox_project_root_available(&named).is_ok());

    let config = test_config();
    let dir = resolved_dir(&config, &project).unwrap();
    assert!(dir.ends_with("/p1"), "{dir}");
    assert!(project_fs_base(&config, &project).unwrap().ends_with("p1"));
}

#[test]
fn relative_base_dir_is_not_an_external_root() {
    let _guard = sandbox(Some("1"), None);
    let project = imported("relative/folder");
    assert!(!has_external_project_root(&project));
    assert_eq!(external_base_dir(&project).unwrap(), None);
    assert!(assert_sandbox_project_root_available(&project).is_ok());
}

#[test]
fn malformed_metadata_reads_as_no_metadata() {
    let _guard = sandbox(Some("1"), None);
    let mut project = project_with_metadata(None);
    project.metadata_json = Some("{ not json".to_string());
    assert!(!has_external_project_root(&project));
    assert_eq!(external_base_dir(&project).unwrap(), None);
    assert!(assert_sandbox_project_root_available(&project).is_ok());
}

#[test]
fn orchestrator_scratch_workspaces_bypass_the_allowlist() {
    let _guard = sandbox(Some("1"), None);
    fn scratch_project(extra: serde_json::Value) -> ProjectRow {
        let mut workspace = serde_json::Map::new();
        workspace.insert("kind".to_string(), serde_json::json!("scratch"));
        if let serde_json::Value::Object(extra) = extra {
            for (key, value) in extra {
                workspace.insert(key, value);
            }
        }
        project_with_metadata(Some(serde_json::json!({
            "baseDir": "/tmp/orchestrator-scratch",
            "orchestratorWorkspace": serde_json::Value::Object(workspace),
        })))
    }

    let project = scratch_project(serde_json::json!({"kind": "scratch"}));
    assert!(has_external_project_root(&project));
    assert_eq!(
        external_base_dir(&project).unwrap().as_deref(),
        Some("/tmp/orchestrator-scratch")
    );
    assert!(assert_sandbox_project_root_available(&project).is_ok());

    // `kind` must be exactly "scratch".
    let wrong_kind = scratch_project(serde_json::json!({"kind": "persistent"}));
    assert!(matches!(
        assert_sandbox_project_root_available(&wrong_kind),
        Err(ProjectDirError::Rejected(_))
    ));

    // Unknown keys disqualify the object.
    let unknown_key = scratch_project(serde_json::json!({"nope": 1}));
    assert!(matches!(
        assert_sandbox_project_root_available(&unknown_key),
        Err(ProjectDirError::Rejected(_))
    ));

    // `writeback` must be absent, null, or "external".
    let writeback = scratch_project(serde_json::json!({"writeback": "internal"}));
    assert!(matches!(
        assert_sandbox_project_root_available(&writeback),
        Err(ProjectDirError::Rejected(_))
    ));
    let external = scratch_project(serde_json::json!({"writeback": "external"}));
    assert!(assert_sandbox_project_root_available(&external).is_ok());
    let null_writeback = scratch_project(serde_json::json!({"writeback": null}));
    assert!(assert_sandbox_project_root_available(&null_writeback).is_ok());
}

#[test]
fn invalid_project_ids_still_resolve_to_an_error() {
    let _guard = sandbox(None, None);
    let config = test_config();
    let mut project = project_with_metadata(None);
    project.id = "../escape".to_string();
    let err = resolved_dir(&config, &project).unwrap_err();
    assert!(matches!(err, ProjectDirError::Unresolved(_)));
    assert_eq!(err.to_string(), "invalid project id: ../escape");
}
