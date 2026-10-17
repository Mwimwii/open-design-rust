//! The bridge's `openDesignDesktop.exportDiagnostics()` and the
//! `open-design:app-config-changed` DOM event.
//!
//! The Electron preload exposed `window.openDesignDesktop.exportDiagnostics()`
//! via a `diagnostics:export-to-file` IPC channel whose main handler (1) showed
//! a native save dialog, (2) asked the renderer to push its chat-scroll
//! capture, (3) fetched the bundle from the daemon's own
//! `GET /api/diagnostics/export`, (4) wrote it and (5) revealed the file.
//!
//! In this shell the page loads the embedded daemon over loopback, so steps 3
//! and 4 are identical — only the fetch source (`daemon_url`) and the save
//! dialog (Tauri `tauri-plugin-dialog`) move onto the host.
//!
//! Two deliberate deviations from the Electron flow, each documented:
//!
//! * **Renderer capture is fire-and-forget.** Tauri only exposes an
//!   asynchronous `WebviewWindow::eval` with no return value (unlike
//!   Electron's `executeJavaScript`), so the shell cannot *await* the
//!   renderer's `POST /api/diagnostics/chat-scroll-forensics` before fetching
//!   the bundle. The capture is triggered first, then the bundle is fetched
//!   after a short grace sleep; the in-app Export logs button already awaits
//!   its own capture before calling `openDesignDesktop.exportDiagnostics()`, so
//!   the only surface that loses the wait is a future Help-menu export that
//!   bypasses the renderer — not reached yet in this shell.
//!
//! * **`app-config-changed` is emitted, not observed.** In Electron the main
//!   process watched its own config writes and told the renderer to re-read.
//!   Here the web app mutates config through the daemon API, which is out of
//!   the shell's sight, so there is no automatic trigger — the host only
//!   provides the dispatch primitive a future config writer can call.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use super::success;
use crate::diagnostics::manifest;
use crate::shell::ShellState;

/// The daemon route the bundle is fetched from.
pub const DIAGNOSTICS_EXPORT_PATH: &str = "/api/diagnostics/export";
/// `DIAGNOSTICS_FILENAME_PREFIX`-equivalent; kept as the literal the same way
/// `open_design_diagnostics` string appears in the bridge below.
pub const DIAGNOSTICS_FILENAME_PREFIX: &str = "open-design-diagnostics";
/// The DOM CustomEvent the web app listens for
/// (`apps/web/src/App.tsx` → `open-design:app-config-changed`).
pub const APP_CONFIG_CHANGED_DOM_EVENT: &str = "open-design:app-config-changed";
/// The Tauri event that re-dispatches the DOM event (parity with the
/// Electron preload's `od:app-config-changed` IPC channel).
pub const APP_CONFIG_CHANGED_TAURI_EVENT: &str = "od:app-config-changed";

/// The expression the Electron preload evaluated to capture the chat scroll.
/// Fire-and-forget here: Tauri gives the shell no return value from `eval`.
const RENDERER_CAPTURE_EXPRESSION: &str =
    "window.__odCaptureChatScrollForensics ? window.__odCaptureChatScrollForensics() : false";
/// Grace given the fire-and-forget capture to land before the bundle fetch.
/// Local loopback, so 250 ms is ample headroom for the in-app button path.
const RENDERER_CAPTURE_GRACE_MS: u64 = 250;

/// What one export attempt produced (parity: `DesktopDiagnosticsExportResult`).
pub enum ExportOutcome {
    Saved(PathBuf),
    Cancelled,
    Failed(String),
}

impl ExportOutcome {
    /// The wire envelope `openDesignDesktop.exportDiagnostics()` resolves to.
    pub fn to_wire(&self) -> Value {
        match self {
            ExportOutcome::Saved(path) => json!({
                "ok": true,
                "path": path.to_string_lossy(),
            }),
            ExportOutcome::Cancelled => json!({ "ok": false, "cancelled": true }),
            ExportOutcome::Failed(message) => json!({
                "ok": false,
                "cancelled": false,
                "message": message,
            }),
        }
    }
}

/// `openDesignDesktop.exportDiagnostics()`.
#[tauri::command]
pub async fn host_export_diagnostics(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, ShellState>,
) -> Result<Value, String> {
    let daemon_url = state.daemon_url.clone();
    let _ = window.eval(RENDERER_CAPTURE_EXPRESSION);
    tokio::time::sleep(Duration::from_millis(RENDERER_CAPTURE_GRACE_MS)).await;
    let outcome = export_diagnostics(&app, &daemon_url).await;
    Ok(outcome.to_wire())
}

/// `od:app-config-changed` → listeners re-dispatch the DOM event, mirroring
/// the Electron preload's `ipcRenderer.on('od:app-config-changed', ...)`.
#[tauri::command]
pub fn host_notify_app_config_changed(window: WebviewWindow) -> Result<Value, String> {
    window
        .emit(APP_CONFIG_CHANGED_TAURI_EVENT, ())
        .map_err(|err| err.to_string())?;
    Ok(success())
}

/// Fetch the bundle from the daemon's own export endpoint, exactly as
/// `fetchDiagnosticsBundle` does in the Electron shell.
pub async fn fetch_diagnostics_bundle(base_url: &str) -> Result<Vec<u8>, String> {
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        DIAGNOSTICS_EXPORT_PATH
    );
    let response = reqwest::get(&url)
        .await
        .map_err(|err| format!("daemon fetch failed: {err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "GET {DIAGNOSTICS_EXPORT_PATH} failed with HTTP {status}"
        ));
    }
    response
        .bytes()
        .await
        .map(Vec::from)
        .map_err(|err| format!("daemon fetch failed: {err}"))
}

/// The native save dialog for a bare filename, parity with the Electron host:
/// a bare `defaultPath` (never forced into a folder), a zip filter and the
/// title "Export OpenDesign diagnostics".
fn choose_save_path(app: &AppHandle, filename: &str) -> Option<PathBuf> {
    app.dialog()
        .file()
        .set_title("Export OpenDesign diagnostics")
        .set_file_name(filename)
        .add_filter("Zip archive", &["zip"])
        // `blocking_save_file` runs off the async command thread (as
        // `host_print_pdf` does); it must not block the main thread.
        .blocking_save_file()
        .and_then(|file| file.into_path().ok())
}

/// Reveal the saved file in the OS file manager, best-effort.
fn reveal_file(app: &AppHandle, path: &std::path::Path) {
    let _ = app.opener().reveal_item_in_dir(path);
}

/// The export flow minus the Tauri command envelope: dialog → fetch → write →
/// reveal. `daemon_url` and the dialog come from the shell; everything else is
/// pure enough to reason about and is covered by the tests below.
pub async fn export_diagnostics(app: &AppHandle, daemon_url: &str) -> ExportOutcome {
    let filename = manifest::diagnostics_file_name(DIAGNOSTICS_FILENAME_PREFIX);
    let Some(path) = choose_save_path(app, &filename) else {
        return ExportOutcome::Cancelled;
    };
    let bytes = match fetch_diagnostics_bundle(daemon_url).await {
        Ok(bytes) => bytes,
        Err(message) => return ExportOutcome::Failed(message),
    };
    if let Err(err) = std::fs::write(&path, &bytes) {
        return ExportOutcome::Failed(format!("failed to write {path:?}: {err}"));
    }
    reveal_file(app, &path);
    ExportOutcome::Saved(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_maps_to_the_absent_envelope() {
        let wire = ExportOutcome::Cancelled.to_wire();
        assert_eq!(wire["ok"], json!(false));
        assert_eq!(wire["cancelled"], json!(true));
        assert!(wire.get("message").is_none());
        assert!(wire.get("path").is_none());
    }

    #[test]
    fn failed_maps_to_a_presented_failure() {
        let wire = ExportOutcome::Failed("boom".to_string()).to_wire();
        assert_eq!(wire["ok"], json!(false));
        assert_eq!(wire["cancelled"], json!(false));
        assert_eq!(wire["message"], json!("boom"));
        assert!(wire.get("path").is_none());
    }

    #[test]
    fn saved_maps_to_a_path() {
        let wire = ExportOutcome::Saved(PathBuf::from("/tmp/x.zip")).to_wire();
        assert_eq!(wire["ok"], json!(true));
        assert_eq!(wire["path"], json!("/tmp/x.zip"));
        assert!(wire.get("cancelled").is_none());
    }

    #[test]
    fn outcome_keys_match_the_web_app_contract() {
        // `apps/web/src/components/ExportDiagnosticsButton.tsx` reads `ok`,
        // `cancelled` and `message`; the bridge must never leak the
        // `{ok:false, reason}` shape used by the `__od__` group.
        let cancelled = serde_json::to_string(&ExportOutcome::Cancelled.to_wire()).unwrap();
        assert!(cancelled.contains("\"cancelled\":true"));
        assert!(!cancelled.contains("reason"));
        let failed = serde_json::to_string(&ExportOutcome::Failed("x".to_string()).to_wire()).unwrap();
        assert!(failed.contains("\"cancelled\":false") && failed.contains("\"message\":\"x\""));
        assert!(!failed.contains("reason"));
    }
}
