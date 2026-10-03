//! IPC commands exposed to the React UI (`invoke(...)`).

use serde::Serialize;
use tauri::State;

use crate::shell::{web_dist, ShellState};

#[derive(Debug, Serialize)]
pub struct DaemonInfo {
    pub url: String,
    pub version: String,
    pub data_dir: String,
    pub web_dist: Option<String>,
}

#[tauri::command]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
pub fn daemon_info(state: State<'_, ShellState>) -> DaemonInfo {
    DaemonInfo {
        url: state.daemon_url.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_dir: state.data_dir.display().to_string(),
        web_dist: web_dist().map(|path| path.display().to_string()),
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
/// `pick-and-import-workspace-context` in the Electron shell). The blocking
/// variant is the documented pattern for async commands (runs off the main
/// thread).
#[tauri::command]
pub async fn pick_workspace_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let picked = app.dialog().file().blocking_pick_folder();
    Ok(picked
        .and_then(|path| path.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned()))
}

/// Guarded external open: only `http(s)` and the app's own `od://` scheme
/// reach the OS handler (parity: the Electron external-open guard).
#[tauri::command]
pub async fn open_external(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|err| err.to_string())?;
    match parsed.scheme() {
        "http" | "https" | "od" => {}
        scheme => return Err(format!("blocked external open for scheme \"{scheme}\"")),
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener().open_url(url, None::<&str>).map_err(|err| err.to_string())
}
