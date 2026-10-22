//! The bridge's `updater` group.
//!
//! Electron's updater is a whole subsystem: a release feed, a resumable
//! download with checksums, an installer handoff, native menu labels, and a
//! status stream. None of it is ported, so this shell answers the same
//! "unsupported" snapshot the Electron host answers when its updater is
//! disabled (`unavailableUpdaterStatus`). The renderer reads that snapshot and
//! renders no updater control at all, which is the honest outcome — better than
//! the app offering a check that cannot find anything.
//!
//! What *is* ported is the snapshot's identity: the status reports the
//! channel and the version of the build that answers it, not the placeholders
//! the Electron helper hardcodes. Those two fields are what the renderer
//! shows next to an update and what a release feed is keyed by, so a build
//! that knows its own lane should say so even while the download subsystem
//! stays unported.
//!
//! Every member of the group is answered here, including the two subscriptions,
//! because `isOpenDesignHostBridge` in `packages/host/src/detection.ts` requires
//! the whole group before it reports a host at all.

use serde_json::{json, Value};
use tauri::State;

use super::bridge::platform_for;
use super::failure;
use crate::shell::ShellState;

/// Parity: `unavailableUpdaterStatus()` in `apps/desktop/src/main/runtime.ts`.
///
/// The Electron helper fills `channel` and `currentVersion` with constants
/// (`beta`, `0.0.0`) because that host never learns its own release identity.
/// This shell does learn it — the release channel comes from the build's
/// version through `od_core::release` — so it reports the real pair. The rest
/// of the snapshot stays inert (`supported: false`, `enabled: false`), which
/// keeps the renderer's model derivation on the path it already has tests for.
pub fn unavailable_status(channel: &str, current_version: &str) -> Value {
    json!({
        "arch": node_arch(),
        "capabilities": {
            "canApplyInPlace": false,
            "canDownload": false,
            "canOpenInstaller": false,
            "requiresManualInstall": false,
        },
        "channel": channel,
        "currentVersion": current_version,
        "enabled": false,
        "error": {
            "code": "updater-unavailable",
            "message": "Desktop updater is not available.",
        },
        "mode": "package-launcher",
        "platform": platform_for(std::env::consts::OS),
        "state": "unsupported",
        "supported": false,
    })
}

/// `process.arch`'s spelling, which is what the renderer receives elsewhere.
pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// `updater.*`: one command for the whole group, dispatched on `action`.
///
/// `status`, `check`, `clear-cache`, `download` and `install` answer the status
/// snapshot the renderer expects from them. `quit` and `setMenuLabels` take the
/// action envelope, because the renderer reads them as action results.
#[tauri::command]
pub async fn host_updater(
    state: State<'_, ShellState>,
    action: Option<String>,
    options: Option<Value>,
    labels: Option<Value>,
) -> Result<Value, String> {
    let _ = (options, labels);
    match action.as_deref().unwrap_or("status") {
        "status" | "check" | "clear-cache" | "download" | "install" => Ok(unavailable_status(
            state.identity.channel.as_str(),
            &state.identity.app_version,
        )),
        "quit" => Ok(failure(
            "the desktop updater is not available in this shell, so there is no update to quit for",
        )),
        "set-menu-labels" => Ok(failure(
            "this shell has no native application menu to relabel",
        )),
        other => Ok(failure(format!("unknown updater action '{other}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_snapshot_says_unsupported_and_is_shaped_like_electrons() {
        let status = unavailable_status("beta", "0.0.0");
        for field in [
            "arch",
            "capabilities",
            "channel",
            "currentVersion",
            "enabled",
            "mode",
            "platform",
            "state",
            "supported",
        ] {
            assert!(
                status.get(field).is_some(),
                "the renderer reads `{field}` off the status"
            );
        }
        assert_eq!(status["supported"], json!(false));
        assert_eq!(status["enabled"], json!(false));
        assert_eq!(status["state"], json!("unsupported"));
        assert_eq!(status["mode"], json!("package-launcher"));
        assert_eq!(status["error"]["code"], json!("updater-unavailable"));
        assert_eq!(status["capabilities"]["canDownload"], json!(false));
        // The platform spelling the renderer compares against, not Rust's.
        assert!(matches!(
            status["platform"].as_str(),
            Some("linux" | "darwin" | "win32")
        ));
        assert!(matches!(node_arch(), "x64" | "arm64" | "ia32" | "arm"));
    }

    #[test]
    fn the_status_reports_the_builds_own_release_identity() {
        let status = unavailable_status("prerelease", "1.2.3-prerelease.4");
        assert_eq!(status["channel"], json!("prerelease"));
        assert_eq!(status["currentVersion"], json!("1.2.3-prerelease.4"));
        // The identity is the only thing that changed: the snapshot is still
        // inert, so the renderer still shows no updater control.
        assert_eq!(status["supported"], json!(false));
        assert_eq!(status["enabled"], json!(false));
        assert_eq!(status["state"], json!("unsupported"));
    }
}
