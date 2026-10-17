//! The bridge's `project` group: the picker-backed flows that need a
//! desktop-minted import token.
//!
//! All three mirror one Electron transaction: show the native folder picker,
//! mint an HMAC token bound to the chosen folder, and (for the two daemon
//! flows) POST that token with the request. The renderer never names a
//! filesystem path — it receives the picked `baseDir` and the token, or a
//! structured failure.
//!
//! Shapes are the *renderer's* (`packages/host/src/protocol.ts`): the
//! normalization the Electron preload did in `normalizeProjectImportResult`
//! and friends happens here instead, so the wire shape is unit-tested in Rust
//! rather than living in the injected script.

use serde_json::{json, Map, Value};
use tauri::{State, WebviewWindow};

use crate::shell::ShellState;

use super::{failure, is_safe_project_id};

/// `project.pickWorkingDir`: pick a folder and hand back a token bound to it.
///
/// The project does not exist yet, so the token is spent later by
/// `POST /api/projects/:id/working-dir` once the renderer has created one.
#[tauri::command]
pub async fn host_pick_working_dir(
    window: WebviewWindow,
    state: State<'_, ShellState>,
) -> Result<Value, String> {
    let auth = state.desktop_auth.clone();
    let Some(base_dir) = pick_folder(&window) else {
        return Ok(canceled());
    };
    // Parity: this flow does not trim; the renderer hands the picked string
    // straight through to the daemon.
    Ok(json!({
        "ok": true,
        "baseDir": base_dir,
        "token": auth.mint_import_token(&base_dir),
    }))
}

/// `project.pickAndImport`: pick a folder and import it as a project.
#[tauri::command]
pub async fn host_pick_and_import(
    window: WebviewWindow,
    state: State<'_, ShellState>,
    init: Option<Value>,
) -> Result<Value, String> {
    let auth = state.desktop_auth.clone();
    let daemon_url = state.daemon_url.clone();
    let Some(picked) = pick_folder(&window) else {
        return Ok(canceled());
    };
    // Parity (PR #974): trim once, here, so the HMAC and the request body bind
    // to the exact string the daemon resolves. A `/tmp/foo ` selection must not
    // authorize an import of `/tmp/foo`.
    let base_dir = picked.trim().to_string();
    if base_dir.is_empty() {
        return Ok(failure("picker returned an empty path"));
    }
    let init = init.unwrap_or(Value::Null);
    let body = import_request_body(&base_dir, &init);
    let headers = import_request_headers(&init);
    match super::daemon_post_json(
        &daemon_url,
        "/api/import/folder",
        &auth.mint_import_token(&base_dir),
        body,
        headers,
    )
    .await
    {
        Ok((status, body)) => Ok(import_result(status, body)),
        Err(reason) => Ok(failure(reason)),
    }
}

/// `project.pickAndReplaceWorkingDir`: re-point an existing project at a newly
/// picked folder.
#[tauri::command]
pub async fn host_pick_and_replace_working_dir(
    window: WebviewWindow,
    state: State<'_, ShellState>,
    project_id: Option<String>,
) -> Result<Value, String> {
    let auth = state.desktop_auth.clone();
    let daemon_url = state.daemon_url.clone();
    let project_id = project_id.unwrap_or_default();
    if project_id.is_empty() {
        return Ok(failure("project id is required"));
    }
    if !is_safe_project_id(&project_id) {
        return Ok(failure("project id contains disallowed characters"));
    }
    let Some(picked) = pick_folder(&window) else {
        return Ok(canceled());
    };
    let base_dir = picked.trim().to_string();
    if base_dir.is_empty() {
        return Ok(failure("picker returned an empty path"));
    }
    let path = format!("/api/projects/{project_id}/working-dir");
    match super::daemon_post_json(
        &daemon_url,
        &path,
        &auth.mint_import_token(&base_dir),
        json!({ "baseDir": base_dir }),
        Vec::new(),
    )
    .await
    {
        Ok((status, body)) => Ok(replace_result(status, body)),
        Err(reason) => Ok(failure(reason)),
    }
}

/// The dialog cancellation envelope (parity: `{ ok: false, canceled: true }`).
fn canceled() -> Value {
    json!({ "canceled": true, "ok": false })
}

/// Show the native folder picker, parented to the app window so the dialog is
/// window-modal and owns Escape (parity: `showDirectoryPickerForSender`).
///
/// `None` means the user cancelled. Electron additionally passed
/// `createDirectory` and `dontAddToRecent`, which have no Tauri equivalent;
/// the picker is a directory picker either way.
fn pick_folder(window: &WebviewWindow) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    window
        .dialog()
        .file()
        .blocking_pick_folder()
        .and_then(|path| path.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
}

/// Parity: the `requestBody` of `pickAndImportFolder`. `name` is included only
/// when it is not null; `skillId`/`designSystemId` are included whenever the
/// renderer sent the key at all (the preload passes `null` through).
fn import_request_body(base_dir: &str, init: &Value) -> Value {
    let mut body = Map::new();
    body.insert("baseDir".to_string(), json!(base_dir));
    for key in ["name", "skillId", "designSystemId"] {
        let Some(value) = init.get(key) else {
            continue;
        };
        if key == "name" && value.is_null() {
            continue;
        }
        body.insert(key.to_string(), value.clone());
    }
    Value::Object(body)
}

/// Parity: the workspace-context headers `pickAndImportFolder` forwards so the
/// import is attributed to the workspace the renderer is acting in. Absent
/// fields are skipped rather than stringified as `"undefined"`.
fn import_request_headers(init: &Value) -> Vec<(String, String)> {
    let Some(context) = init.get("workspaceContext").filter(|value| !value.is_null()) else {
        return Vec::new();
    };
    let mut headers = Vec::new();
    for (header, key) in [
        ("x-od-workspace-id", "workspaceId"),
        ("x-od-workspace-type", "workspaceType"),
        ("x-od-workspace-member-id", "workspaceMemberId"),
        ("x-od-workspace-role", "role"),
        ("x-od-workspace-lifecycle-state", "lifecycleState"),
        ("x-od-workspace-member-status", "memberStatus"),
    ] {
        if let Some(value) = context.get(key).and_then(Value::as_str) {
            headers.push((header.to_string(), value.to_string()));
        }
    }
    for (header, key) in [
        ("x-od-workspace-can-share-projects", "canShareProjects"),
        ("x-od-workspace-can-write-synced-files", "canWriteSyncedFiles"),
    ] {
        if let Some(value) = context
            .get("permissions")
            .and_then(|permissions| permissions.get(key))
            .and_then(Value::as_bool)
        {
            headers.push((header.to_string(), value.to_string()));
        }
    }
    headers
}

/// Parity: `normalizeProjectImportResult`. A daemon error body rides along as
/// `details`, because the renderer surfaces it.
fn import_result(status: u16, body: Value) -> Value {
    if !(200..300).contains(&status) {
        let mut envelope = json!({
            "ok": false,
            "reason": format!("daemon returned HTTP {status}"),
        });
        if !body.is_null() {
            envelope["details"] = body;
        }
        return envelope;
    }
    let project_id = body
        .get("project")
        .and_then(|project| project.get("id"))
        .and_then(Value::as_str);
    let conversation_id = body.get("conversationId").and_then(Value::as_str);
    let entry_file = body.get("entryFile");
    let has_entry_file = entry_file.is_some_and(|value| value.is_string() || value.is_null());
    match (project_id, conversation_id, has_entry_file) {
        (Some(project_id), Some(conversation_id), true) => json!({
            "ok": true,
            "projectId": project_id,
            "conversationId": conversation_id,
            "entryFile": entry_file.cloned().unwrap_or(Value::Null),
        }),
        _ => failure("daemon import response did not include host project identifiers"),
    }
}

/// Parity: `normalizeProjectReplaceWorkingDirResult`.
fn replace_result(status: u16, body: Value) -> Value {
    if !(200..300).contains(&status) {
        let mut envelope = json!({
            "ok": false,
            "reason": format!("daemon returned HTTP {status}"),
        });
        if !body.is_null() {
            envelope["details"] = body;
        }
        return envelope;
    }
    let Some(base_dir) = body.get("baseDir").and_then(Value::as_str) else {
        return failure("daemon working-dir response did not include baseDir");
    };
    let entry_file = body
        .get("entryFile")
        .filter(|value| value.is_string() || value.is_null())
        .cloned()
        .unwrap_or(Value::Null);
    json!({ "ok": true, "baseDir": base_dir, "entryFile": entry_file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::desktop_auth::{sign_import_token, DesktopAuth};

    #[test]
    fn import_body_carries_only_the_keys_the_renderer_sent() {
        assert_eq!(
            import_request_body("/tmp/p", &json!({})),
            json!({ "baseDir": "/tmp/p" })
        );
        assert_eq!(
            import_request_body("/tmp/p", &json!({ "name": "Deck", "skillId": null })),
            json!({ "baseDir": "/tmp/p", "name": "Deck", "skillId": null })
        );
        // `name: null` is the one key the Electron host drops.
        assert_eq!(
            import_request_body("/tmp/p", &json!({ "name": null })),
            json!({ "baseDir": "/tmp/p" })
        );
    }

    #[test]
    fn workspace_headers_are_forwarded_without_the_undefined_ones() {
        let init = json!({
            "workspaceContext": {
                "workspaceId": "ws-1",
                "workspaceType": "team",
                "role": "owner",
                "permissions": { "canShareProjects": true, "canWriteSyncedFiles": false }
            }
        });
        let headers = import_request_headers(&init);
        assert!(headers.contains(&("x-od-workspace-id".to_string(), "ws-1".to_string())));
        assert!(headers.contains(&("x-od-workspace-role".to_string(), "owner".to_string())));
        assert!(headers.contains(&(
            "x-od-workspace-can-share-projects".to_string(),
            "true".to_string()
        )));
        assert!(headers.contains(&(
            "x-od-workspace-can-write-synced-files".to_string(),
            "false".to_string()
        )));
        // Fields the renderer did not send produce no header at all.
        assert!(!headers.iter().any(|(name, _)| name.contains("member")));
    }

    #[test]
    fn no_workspace_context_means_no_headers() {
        assert!(import_request_headers(&json!({})).is_empty());
        assert!(import_request_headers(&json!({ "workspaceContext": null })).is_empty());
    }

    #[test]
    fn a_successful_import_is_the_renderer_shape() {
        let body = json!({
            "project": { "id": "p1" },
            "conversationId": "c1",
            "entryFile": "index.html"
        });
        assert_eq!(
            import_result(200, body),
            json!({ "ok": true, "projectId": "p1", "conversationId": "c1", "entryFile": "index.html" })
        );
        // `entryFile: null` is allowed; a missing conversation is not.
        assert_eq!(
            import_result(
                201,
                json!({ "project": { "id": "p1" }, "conversationId": "c1", "entryFile": null })
            ),
            json!({ "ok": true, "projectId": "p1", "conversationId": "c1", "entryFile": null })
        );
        assert_eq!(
            import_result(200, json!({ "project": { "id": "p1" } }))["ok"],
            json!(false)
        );
    }

    #[test]
    fn a_failed_import_carries_the_status_and_the_daemon_body() {
        let envelope = import_result(503, json!({ "error": { "code": "DESKTOP_AUTH_PENDING" } }));
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["reason"], json!("daemon returned HTTP 503"));
        assert_eq!(envelope["details"]["error"]["code"], json!("DESKTOP_AUTH_PENDING"));
    }

    #[test]
    fn a_failed_replace_that_lost_its_base_dir_is_a_failure() {
        assert_eq!(
            replace_result(200, json!({ "project": { "id": "p1" } })),
            json!({
                "ok": false,
                "reason": "daemon working-dir response did not include baseDir"
            })
        );
        assert_eq!(
            replace_result(200, json!({ "baseDir": "/tmp/next", "entryFile": "a.html" })),
            json!({ "ok": true, "baseDir": "/tmp/next", "entryFile": "a.html" })
        );
    }

    #[test]
    fn the_minted_token_verifies_against_its_own_inputs() {
        // The same computation the daemon performs, done here so the token the
        // shell hands the renderer is checked against the published format.
        let auth = DesktopAuth::generate();
        let token = auth.mint_import_token("/tmp/project");
        let fields: Vec<&str> = token.split('~').collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(
            token,
            sign_import_token(auth.secret(), "/tmp/project", fields[0], fields[1])
        );
        assert_ne!(
            token.split('~').nth(2),
            sign_import_token(auth.secret(), "/tmp/other", fields[0], fields[1])
                .split('~')
                .nth(2)
        );
    }
}
