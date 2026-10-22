//! IPC commands the shell itself owns — process facts about this app, not the
//! host-bridge surface (which lives in `host::*`).

use serde::Serialize;
use tauri::State;

use crate::shell::ShellState;

#[derive(Debug, Serialize)]
pub struct DaemonInfo {
    pub url: String,
    pub version: String,
    pub data_dir: String,
    pub web_dist: Option<String>,
    /// The release channel this build belongs to.
    pub channel: String,
    /// The namespace this build writes into.
    pub namespace: String,
}

/// The version this build carries — the bundle's version, which is what the
/// installer and the About surface show.
#[tauri::command]
pub fn app_version(state: State<'_, ShellState>) -> String {
    state.identity.app_version.clone()
}

#[tauri::command]
pub fn daemon_info(state: State<'_, ShellState>) -> DaemonInfo {
    DaemonInfo {
        url: state.daemon_url.clone(),
        version: state.identity.app_version.clone(),
        data_dir: state.data_dir.display().to_string(),
        web_dist: state
            .web_dist
            .as_ref()
            .map(|path| path.display().to_string()),
        channel: state.identity.channel.as_str().to_string(),
        namespace: state.identity.namespace.clone(),
    }
}

/// Live health probe against the embedded daemon (`GET /api/health`).
#[tauri::command]
pub async fn daemon_health(state: State<'_, ShellState>) -> Result<serde_json::Value, String> {
    let url = format!("{}/api/health", state.daemon_url);
    let response = reqwest::get(&url).await.map_err(|err| err.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("daemon health returned HTTP {status}"));
    }
    response.json().await.map_err(|err| err.to_string())
}

/// Native folder picker for importing a workspace (parity:
/// `pick-and-import-workspace-context` in the Electron shell).
///
/// Superseded by the bridge's `project.*` group, which mints the import token
/// the daemon's import flow requires; kept until that group lands.
#[tauri::command]
pub async fn pick_workspace_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let picked = app.dialog().file().blocking_pick_folder();
    Ok(picked
        .and_then(|path| path.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned()))
}
