//! The bridge's `shell` group: external links and revealing a project folder.
//!
//! Both are security boundaries rather than conveniences, because the renderer
//! is the thing that supplies the argument:
//!
//! * `open_external` refuses every scheme but `http(s)` and the app's own
//!   `od://` before the OS handler ever sees it.
//! * `open_path` never accepts a path. The renderer names a *project id*; the
//!   path is derived here from the daemon's own answer, then checked against
//!   the trusted-picker rule from PR #974 (`apps/desktop`).

use std::path::{Path, PathBuf};

use serde_json::Value;
use tauri::State;

use crate::shell::ShellState;

use super::{daemon_get_project, failure, is_safe_project_id, project_metadata, success};

/// `shell.openExternal`, parity with the Electron `shell:open-external` guard.
#[tauri::command]
pub async fn open_external(app: tauri::AppHandle, url: String) -> Value {
    let parsed = match url::Url::parse(&url) {
        Ok(parsed) => parsed,
        Err(err) => return failure(err.to_string()),
    };
    match parsed.scheme() {
        "http" | "https" | "od" => {}
        scheme => return failure(format!("blocked external open for scheme \"{scheme}\"")),
    }
    use tauri_plugin_opener::OpenerExt;
    match app.opener().open_url(url, None::<&str>) {
        Ok(()) => success(),
        Err(err) => failure(err.to_string()),
    }
}

/// `shell.openPath`: reveal the named project's working directory in the OS
/// file manager.
#[tauri::command]
pub async fn host_open_path(
    app: tauri::AppHandle,
    state: State<'_, ShellState>,
    project_id: String,
) -> Result<Value, String> {
    // The borrow of `state` is dropped before the daemon call so the future
    // holds no borrow of managed state.
    let daemon_url = state.daemon_url.clone();
    Ok(open_path(&app, &daemon_url, &project_id).await)
}

async fn open_path(app: &tauri::AppHandle, daemon_url: &str, project_id: &str) -> Value {
    if project_id.is_empty() {
        return failure("project id must be a non-empty string");
    }
    if !is_safe_project_id(project_id) {
        return failure("project id contains disallowed characters");
    }
    let body = match daemon_get_project(daemon_url, project_id).await {
        Ok(body) => body,
        Err(reason) => return failure(reason),
    };
    let Some(resolved_dir) = body
        .get("resolvedDir")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return failure("daemon response did not include resolvedDir");
    };
    let project = body.get("project").cloned().unwrap_or(Value::Null);
    if let Some(reason) = trust_refusal(&project) {
        return failure(reason);
    }
    let directory = match validate_existing_directory(resolved_dir) {
        Ok(directory) => directory,
        Err(reason) => return failure(reason),
    };
    use tauri_plugin_opener::OpenerExt;
    match app
        .opener()
        .open_path(directory.to_string_lossy(), None::<&str>)
    {
        Ok(()) => success(),
        Err(err) => failure(err.to_string()),
    }
}

/// PR #974: a folder-imported project's `resolvedDir` is a user-controlled
/// location, so it may only be revealed when the daemon stamped it as having
/// come through the HMAC-gated picker. Native projects live under the
/// daemon-owned projects root and are always safe to reveal.
fn trust_refusal(project: &Value) -> Option<&'static str> {
    let metadata = project_metadata(project);
    let has_base_dir = metadata
        .as_ref()
        .and_then(|meta| meta.get("baseDir"))
        .and_then(Value::as_str)
        .is_some_and(|base_dir| !base_dir.is_empty());
    let from_trusted_picker = matches!(
        metadata
            .as_ref()
            .and_then(|meta| meta.get("fromTrustedPicker")),
        Some(Value::Bool(true))
    );
    (has_base_dir && !from_trusted_picker)
        .then_some("project did not come from the trusted picker flow")
}

/// Parity: `validateExistingDirectory` in `apps/desktop/src/main/runtime.ts`.
///
/// The `.app` rejection is load-bearing: an app bundle is a directory, and
/// `openPath` on one *launches* it instead of revealing it, so without this
/// check the bridge would be a "launch any installed application" primitive on
/// macOS.
pub fn validate_existing_directory(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("path must be a non-empty string".to_string());
    }
    if !Path::new(path).is_absolute() {
        return Err("path must be absolute".to_string());
    }
    let Ok(resolved) = std::fs::canonicalize(path) else {
        return Err("path does not exist".to_string());
    };
    let Ok(metadata) = std::fs::metadata(&resolved) else {
        return Err("path could not be stat'd".to_string());
    };
    if !metadata.is_dir() {
        return Err("path is not a directory".to_string());
    }
    if resolved
        .to_string_lossy()
        .to_ascii_lowercase()
        .ends_with(".app")
    {
        return Err("application bundles are not project directories".to_string());
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_projects_are_always_revealable() {
        assert_eq!(trust_refusal(&json!({ "id": "p1", "metadata": {} })), None);
        assert_eq!(trust_refusal(&json!({ "id": "p1" })), None);
    }

    #[test]
    fn imported_projects_need_the_trusted_picker_stamp() {
        let untrusted = json!({ "metadata": { "baseDir": "/tmp/imported" } });
        assert_eq!(
            trust_refusal(&untrusted),
            Some("project did not come from the trusted picker flow")
        );

        let trusted = json!({
            "metadata": { "baseDir": "/tmp/imported", "fromTrustedPicker": true }
        });
        assert_eq!(trust_refusal(&trusted), None);

        // A stamp that is not exactly `true` does not count.
        let falsy = json!({
            "metadata": { "baseDir": "/tmp/imported", "fromTrustedPicker": "true" }
        });
        assert!(trust_refusal(&falsy).is_some());

        // An empty baseDir is not an external root.
        assert_eq!(
            trust_refusal(&json!({ "metadata": { "baseDir": "" } })),
            None
        );
    }

    #[test]
    fn the_trusted_picker_rule_survives_the_daemons_metadata_shape() {
        // The Rust daemon serializes the raw `metadata_json` column today; the
        // rule must not become skippable because of that shape drift.
        let project = json!({ "metadata_json": "{\"baseDir\":\"/tmp/imported\"}" });
        assert_eq!(
            trust_refusal(&project),
            Some("project did not come from the trusted picker flow")
        );
    }

    #[test]
    fn directory_validation_rejects_every_non_project_path() {
        assert_eq!(
            validate_existing_directory("").unwrap_err(),
            "path must be a non-empty string"
        );
        assert_eq!(
            validate_existing_directory("relative/dir").unwrap_err(),
            "path must be absolute"
        );
        assert_eq!(
            validate_existing_directory("/definitely/not/here/od-test").unwrap_err(),
            "path does not exist"
        );

        let file = std::env::temp_dir().join("od-tauri-open-path-file.txt");
        std::fs::write(&file, b"x").expect("write temp file");
        assert_eq!(
            validate_existing_directory(&file.to_string_lossy()).unwrap_err(),
            "path is not a directory"
        );
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn directory_validation_accepts_a_real_directory_and_resolves_it() {
        let dir = std::env::temp_dir().join("od-tauri-open-path-dir");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let resolved = validate_existing_directory(&dir.to_string_lossy()).expect("valid");
        assert!(resolved.is_dir());
        assert!(resolved.is_absolute());
    }

    #[test]
    fn app_bundles_are_not_project_directories() {
        let dir = std::env::temp_dir().join("od-tauri-open-path/Safari.app");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        assert_eq!(
            validate_existing_directory(&dir.to_string_lossy()).unwrap_err(),
            "application bundles are not project directories"
        );
    }
}
