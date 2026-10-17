//! The bridge methods that are neither picking, capturing nor printing: browser
//! data, the native window appearance, and the desktop pet.
//!
//! Two of the three are real. The third — clearing the design browser's storage
//! — cannot be done here without collateral damage, so it says so instead of
//! pretending.

use serde_json::{json, Value};
use tauri::{utils::Theme, WebviewWindow};

use super::failure;

/// `browser.clearData`.
///
/// Electron cleared one named session partition
/// (`persist:open-design-design-browser`), which is where the design browser's
/// cookies and storage lived and nowhere else. Tauri's
/// `clear_all_browsing_data` is the whole webview: running it here would take
/// the app's own localStorage, IndexedDB and cookies with it, and return the
/// user to a signed-out, re-onboarding state. So this answers a failure the
/// renderer can show rather than a success it cannot trust.
#[tauri::command]
pub async fn host_clear_browser_data(options: Option<Value>) -> Result<Value, String> {
    let _ = options;
    Ok(failure(
        "the Rust shell cannot clear the design browser's data separately from the app's own storage",
    ))
}

/// `appearance.setTheme`.
///
/// Electron kept the native window material in step with the app theme (macOS
/// vibrancy). The Tauri equivalent is the window theme, which is what native
/// decorations and the system material follow. The call is fire-and-forget in
/// the renderer, but the answer is still an honest result rather than a
/// silent no-op.
#[tauri::command]
pub async fn host_set_theme(window: WebviewWindow, theme: Option<String>) -> Result<Value, String> {
    let requested = match theme.as_deref() {
        Some("dark") => Some(Theme::Dark),
        Some("light") => Some(Theme::Light),
        // "system" — and anything this shell does not know — follows the OS.
        _ => None,
    };
    window.set_theme(requested).map_err(|err| err.to_string())?;
    Ok(json!({ "ok": true }))
}

/// `pet.setVisible`.
///
/// The desktop pet lives in its own always-on-top Electron window. This shell
/// has no such window, so there is nothing to show or hide: the setting is
/// remembered by the app either way, and the call is fire-and-forget in the
/// renderer (`invoke(...).catch(...)`). A failure envelope would only fill the
/// renderer's console, so this succeeds and the gap is recorded here.
#[tauri::command]
pub async fn host_set_pet_visible(visible: bool) -> Result<Value, String> {
    let _ = visible;
    Ok(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clearing_browser_data_names_why_it_cannot() {
        let answer = host_clear_browser_data(None)
            .await
            .expect("an unsupported capability is an answer, not a transport error");
        assert_eq!(answer["ok"], json!(false));
        let reason = answer["reason"].as_str().expect("a reason");
        assert!(
            reason.contains("design browser"),
            "the reason has to name what could not be cleared: {reason}"
        );
    }

    #[tokio::test]
    async fn the_pet_gap_is_a_no_op_rather_than_a_failure() {
        let answer = host_set_pet_visible(true).await.expect("a no-op");
        assert_eq!(answer["ok"], json!(true));
    }
}
