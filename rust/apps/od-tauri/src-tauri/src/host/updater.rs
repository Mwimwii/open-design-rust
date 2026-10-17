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
//! Every member of the group is answered here, including the two subscriptions,
//! because `isOpenDesignHostBridge` in `packages/host/src/detection.ts` requires
//! the whole group before it reports a host at all.

use serde_json::{json, Value};

use super::bridge::platform_for;
use super::failure;

/// Parity: `unavailableUpdaterStatus()` in `apps/desktop/src/main/runtime.ts`.
///
/// The channel is the constant that function uses rather than a channel this
/// shell knows anything about: the status is inert (`supported: false`,
/// `enabled: false`), and matching the Electron shape keeps the renderer's
/// model derivation on the path it already has tests for.
pub fn unavailable_status() -> Value {
    json!({
        "arch": node_arch(),
        "capabilities": {
            "canApplyInPlace": false,
            "canDownload": false,
            "canOpenInstaller": false,
            "requiresManualInstall": false,
        },
        "channel": "beta",
        "currentVersion": "0.0.0",
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
    action: Option<String>,
    options: Option<Value>,
    labels: Option<Value>,
) -> Result<Value, String> {
    let _ = (options, labels);
    match action.as_deref().unwrap_or("status") {
        "status" | "check" | "clear-cache" | "download" | "install" => Ok(unavailable_status()),
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
        let status = unavailable_status();
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
        assert_eq!(status["currentVersion"], json!("0.0.0"));
        assert_eq!(status["error"]["code"], json!("updater-unavailable"));
        assert_eq!(status["capabilities"]["canDownload"], json!(false));
        // The platform spelling the renderer compares against, not Rust's.
        assert!(matches!(
            status["platform"].as_str(),
            Some("linux" | "darwin" | "win32")
        ));
        assert!(matches!(node_arch(), "x64" | "arm64" | "ia32" | "arm"));
    }

    #[tokio::test]
    async fn every_group_action_is_answered() {
        for action in ["status", "check", "clear-cache", "download", "install"] {
            let answer = host_updater(Some(action.to_string()), None, None)
                .await
                .expect("status actions cannot fail");
            assert!(answer.is_object(), "{action} must answer a status snapshot");
            assert_eq!(answer["supported"], json!(false));
        }
        for action in ["quit", "set-menu-labels"] {
            let answer = host_updater(Some(action.to_string()), None, None)
                .await
                .expect("action results are envelopes, not transport errors");
            assert_eq!(answer["ok"], json!(false));
            assert!(answer["reason"].is_string(), "{action} names its reason");
        }
        // An unknown action is still answered rather than left to the transport.
        let unknown = host_updater(Some("frobnicate".to_string()), None, None)
            .await
            .expect("unknown actions answer an envelope");
        assert_eq!(unknown["ok"], json!(false));
        // A missing action is the status read.
        let default = host_updater(None, None, None).await.expect("status");
        assert_eq!(default["state"], json!("unsupported"));
    }
}
