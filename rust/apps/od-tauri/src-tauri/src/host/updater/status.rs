//! The updater status snapshot (parity: `updater.ts`'s `snapshot()` and
//! `runtime.ts`'s `unavailableUpdaterStatus()`).
//!
//! One document, built from the build's own identity plus whatever the last
//! check concluded. The renderer derives its whole model from these fields
//! (`apps/web/src/lib/updater.ts`), so the shape is the contract: a field that
//! is missing reads as "the host has no opinion", and a capability that is
//! `true` must be one the host can actually deliver.

use od_core::release::ReleaseChannel;
use serde_json::{json, Map, Value};

use super::config::{capabilities_for, is_supported_package_launcher_platform, UpdateMode};
use super::feed::UpdateCandidate;
use super::model::{object_map, UpdateState};
use super::support::UpdateError;

/// Whether this build can carry a package-launcher update through to the end.
///
/// The feed read and the release decision are ported; the resumable download,
/// the checksum verification, and the installer handoff are not. A status
/// snapshot must not advertise a capability the host cannot deliver, so the
/// platform reports `supported: false` — and the renderer accordingly renders
/// no updater control — until this flips with the download port. `check` still
/// answers the truth about the feed; the snapshot just does not promise an
/// install it cannot perform.
pub const CAN_APPLY_PACKAGE_LAUNCHER_UPDATES: bool = false;

/// The facts every status document reports about the build answering it.
#[derive(Debug, Clone)]
pub struct StatusIdentity<'a> {
    pub arch: &'a str,
    pub channel: &'a ReleaseChannel,
    pub current_version: &'a str,
    pub enabled: bool,
    pub mode: UpdateMode,
    pub platform: &'a str,
}

impl<'a> StatusIdentity<'a> {
    /// Whether this build's platform has a usable update path at all.
    ///
    /// Both halves matter: a mode this host does not implement, and a platform
    /// whose updates belong to another owner (a distribution package
    /// manager), are unsupported for different reasons and neither is an error.
    pub fn supported(&self) -> bool {
        CAN_APPLY_PACKAGE_LAUNCHER_UPDATES
            && self.mode == UpdateMode::PackageLauncher
            && is_supported_package_launcher_platform(self.platform)
    }

    fn document(
        &self,
        state: UpdateState,
        supported: bool,
        artifact_type: Option<&str>,
    ) -> Map<String, Value> {
        object_map(json!({
            "arch": self.arch,
            "capabilities": capabilities_for(self.mode, self.platform, supported, artifact_type)
                .to_json(),
            "channel": self.channel.as_str(),
            "currentVersion": self.current_version,
            "enabled": self.enabled,
            "mode": self.mode.as_str(),
            "platform": self.platform,
            "state": state.as_str(),
            "supported": supported,
        }))
    }
}

fn with_error(mut document: Map<String, Value>, error: UpdateError) -> Value {
    document.insert("error".into(), error.to_json());
    Value::Object(document)
}

/// The snapshot of a host with no update path: nothing to check, nothing to
/// promise (parity: `unavailableUpdaterStatus`).
pub fn unsupported_status(identity: &StatusIdentity, error: UpdateError) -> Value {
    let document = identity.document(UpdateState::Unsupported, false, None);
    with_error(document, error)
}

/// The snapshot before anything has been checked, or after a check that found
/// nothing newer.
pub fn idle_status(identity: &StatusIdentity) -> Value {
    Value::Object(identity.document(UpdateState::Idle, identity.supported(), None))
}

/// The snapshot of a finished check that found nothing newer.
pub fn not_available_status(identity: &StatusIdentity) -> Value {
    Value::Object(identity.document(UpdateState::NotAvailable, identity.supported(), None))
}

/// The snapshot of a finished check that found a newer release.
///
/// `artifactUrl` repeats `artifact.url` because the renderer's prompt key and
/// the "open the release page" affordance read the flat field first.
pub fn available_status(identity: &StatusIdentity, candidate: &UpdateCandidate) -> Value {
    let mut document = identity.document(
        UpdateState::Available,
        identity.supported(),
        Some(candidate.artifact_type_name()),
    );
    document.insert("availableVersion".into(), json!(candidate.version));
    document.insert("artifactUrl".into(), json!(candidate.artifact.url));
    document.insert("artifact".into(), candidate.artifact.to_json());
    document.insert("checksum".into(), candidate.checksum.to_json());
    Value::Object(document)
}

/// The snapshot of a check that could not answer.
pub fn error_status(identity: &StatusIdentity, error: UpdateError) -> Value {
    let document = identity.document(UpdateState::Error, identity.supported(), None);
    with_error(document, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::updater::config::{resolve_config, UpdaterConfigInput};
    use crate::host::updater::feed::select_update_candidate;
    use std::collections::HashMap;

    fn config(platform: &str) -> crate::host::updater::config::UpdaterConfig {
        let env: HashMap<String, String> = [
            ("OD_UPDATE_PLATFORM".to_string(), platform.to_string()),
            // The feed's `mac` key describes Apple silicon; ask for it
            // explicitly instead of inheriting the test host's arch.
            ("OD_UPDATE_ARCH".to_string(), "arm64".to_string()),
            ("OD_UPDATE_CHANNEL".to_string(), "beta".to_string()),
            ("OD_UPDATE_CURRENT_VERSION".to_string(), "1.0.0".to_string()),
        ]
        .into_iter()
        .collect();
        resolve_config(&UpdaterConfigInput {
            env,
            packaged: true,
            ..Default::default()
        })
        .expect("config")
    }

    fn identity(config: &crate::host::updater::config::UpdaterConfig) -> StatusIdentity<'_> {
        StatusIdentity {
            arch: &config.arch,
            channel: &config.channel,
            current_version: &config.current_version,
            enabled: config.enabled,
            mode: config.mode,
            platform: &config.platform,
        }
    }

    fn feed() -> Map<String, Value> {
        json!({
            "channel": "beta",
            "releaseVersion": "1.2.3-beta.4",
            "platforms": {
                "mac": {
                    "enabled": true,
                    "artifacts": {"dmg": {"url": "https://example.test/a.dmg"}},
                },
            },
        })
        .as_object()
        .cloned()
        .expect("object")
    }

    #[test]
    fn every_snapshot_carries_the_fields_the_renderer_reads() {
        for build in ["darwin", "win32", "linux"] {
            let config = config(build);
            for status in [
                idle_status(&identity(&config)),
                not_available_status(&identity(&config)),
                unsupported_status(
                    &identity(&config),
                    UpdateError::new("updater-unavailable", "not available"),
                ),
                error_status(
                    &identity(&config),
                    UpdateError::new("metadata-unreachable", "offline"),
                ),
            ] {
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
                        "{build}: the renderer reads `{field}` off the status"
                    );
                }
                assert_eq!(status["platform"], json!(build));
                assert_eq!(status["channel"], json!("beta"));
                assert_eq!(status["currentVersion"], json!("1.0.0"));
            }
        }
    }

    #[test]
    fn an_unported_download_path_is_not_advertised() {
        // The feed read is ported; the download is not. A supported snapshot
        // would invite a click the host cannot serve.
        if !CAN_APPLY_PACKAGE_LAUNCHER_UPDATES {
            for build in ["darwin", "win32"] {
                let config = config(build);
                let status = idle_status(&identity(&config));
                assert_eq!(status["supported"], json!(false), "{build}");
                assert_eq!(
                    status["capabilities"]["canDownload"],
                    json!(false),
                    "{build}"
                );
                assert_eq!(status["state"], json!("idle"), "{build}");
            }
        }
    }

    #[test]
    fn an_available_release_names_its_version_artifact_and_checksum() {
        let config = config("darwin");
        let candidate = select_update_candidate(&feed(), &config, false).expect("candidate");
        let status = available_status(&identity(&config), &candidate);
        assert_eq!(status["state"], json!("available"));
        assert_eq!(status["availableVersion"], json!("1.2.3-beta.4"));
        assert_eq!(status["artifactUrl"], json!("https://example.test/a.dmg"));
        assert_eq!(status["artifact"]["type"], json!("dmg"));
        assert_eq!(status["artifact"]["platformKey"], json!("mac"));
        assert_eq!(status["checksum"]["algorithm"], json!("sha256"));
    }

    #[test]
    fn an_error_snapshot_carries_the_code_and_message() {
        let config = config("darwin");
        let status = error_status(
            &identity(&config),
            UpdateError::new("metadata-unreachable", "connection refused"),
        );
        assert_eq!(status["state"], json!("error"));
        assert_eq!(status["error"]["code"], json!("metadata-unreachable"));
        assert_eq!(status["error"]["message"], json!("connection refused"));
        assert_eq!(status["channel"], json!("beta"));
    }

    #[test]
    fn the_unsupported_snapshot_says_why_and_promises_nothing() {
        let config = config("linux");
        let status = unsupported_status(
            &identity(&config),
            UpdateError::new("updater-unavailable", "Desktop updater is not available."),
        );
        assert_eq!(status["state"], json!("unsupported"));
        assert_eq!(status["supported"], json!(false));
        assert_eq!(status["capabilities"]["canDownload"], json!(false));
        assert_eq!(
            status["capabilities"]["requiresManualInstall"],
            json!(false)
        );
        assert_eq!(status["error"]["code"], json!("updater-unavailable"));
    }
}
