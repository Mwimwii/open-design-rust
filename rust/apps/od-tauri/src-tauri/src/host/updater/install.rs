//! Handing a verified artifact to the platform's installer (parity:
//! `openInstaller` in `updater.ts`).
//!
//! This host does not replace its own bundle. It opens the artifact it just
//! verified and lets the platform do what it does: mount the DMG so a person can
//! drag the app over the old one, or run the Windows installer. Both paths
//! require the app to exit first, which is why `quit` exists as its own action.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::store::timestamp_now;
use super::support::UpdateError;

/// What an install handoff recorded (parity:
/// `DesktopUpdateStatusSnapshot["installResult"]`).
///
/// The renderer treats a present `installResult` as "the installer is open" and
/// stops offering the install action, so the record only exists after the
/// platform accepted the artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallResult {
    pub opened_at: String,
    pub path: String,
}

impl InstallResult {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("an install result serializes")
    }
}

/// Open the platform installer for a verified artifact.
///
/// The artifact is opened by path, never by URL: what runs here must be the
/// bytes this process hashed.
pub fn open_installer(platform: &str, artifact: &Path) -> Result<InstallResult, UpdateError> {
    if !artifact.is_file() {
        return Err(UpdateError::new(
            "install-artifact-missing",
            format!("the downloaded artifact is gone: {}", artifact.display()),
        ));
    }
    let command = match platform {
        "darwin" => Command::new("open").arg(artifact).status(),
        "win32" => Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(artifact)
            .status(),
        _ => {
            return Err(UpdateError::new(
                "unsupported-platform",
                "installer handoff is implemented on macOS and Windows only",
            ))
        }
    };
    match command {
        Ok(status) if status.success() => Ok(InstallResult {
            opened_at: timestamp_now(),
            path: artifact.to_string_lossy().into_owned(),
        }),
        Ok(status) => Err(UpdateError::new(
            "install-open-failed",
            format!("the platform installer exited with {status}"),
        )),
        Err(error) => Err(UpdateError::new(
            "install-open-failed",
            format!("the platform installer could not be opened: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "od-update-install-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn a_missing_artifact_is_refused_before_anything_runs() {
        let root = scratch("missing");
        let error = open_installer("darwin", &root.join("absent.dmg")).expect_err("missing");
        assert_eq!(error.code, "install-artifact-missing");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_platform_without_a_handoff_says_so() {
        let root = scratch("linux");
        let artifact = root.join("OpenDesign.AppImage");
        fs::write(&artifact, b"not a real artifact").expect("artifact");
        let error = open_installer("linux", &artifact).expect_err("linux");
        assert_eq!(error.code, "unsupported-platform");
        assert!(error.message.contains("macOS and Windows"));
        let _ = fs::remove_dir_all(&root);
    }
}
