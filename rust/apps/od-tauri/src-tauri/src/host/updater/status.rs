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
/// Every step the Electron host takes is ported: the feed read, the release
/// decision, the resumable download, the checksum verification, the store, and
/// the installer handoff. What is *not* ported is the status stream (so a
/// download reports its result when it finishes rather than while it runs), the
/// scheduler, and the native menu labels; none of those change whether an
/// install can be completed, so the platform may advertise itself as supported.
pub const CAN_APPLY_PACKAGE_LAUNCHER_UPDATES: bool = true;

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

/// The snapshot of a release the store holds, downloaded and verified.
///
/// `downloadPath` is what the renderer opens, and a present `installResult` is
/// how it learns the platform installer is already running — both are read off
/// this document rather than passed through the command result.
pub fn downloaded_status(
    identity: &StatusIdentity,
    release: &super::store::UpdateReleaseRef,
    artifact_path: &std::path::Path,
    install: Option<&super::install::InstallResult>,
) -> Value {
    let mut document = identity.document(
        UpdateState::Downloaded,
        identity.supported(),
        release.artifact.artifact_type.as_deref(),
    );
    document.insert("availableVersion".into(), json!(release.version));
    document.insert("artifactUrl".into(), json!(release.artifact.url));
    document.insert("artifact".into(), release.artifact.to_json());
    document.insert("checksum".into(), release.checksum.to_json());
    document.insert(
        "downloadPath".into(),
        json!(artifact_path.to_string_lossy().into_owned()),
    );
    if let Some(install) = install {
        document.insert("installResult".into(), install.to_json());
    }
    Value::Object(document)
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
    fn the_supported_platforms_advertise_the_flow_they_can_finish() {
        for build in ["darwin", "win32"] {
            let config = config(build);
            for status in [
                idle_status(&identity(&config)),
                not_available_status(&identity(&config)),
            ] {
                assert_eq!(status["supported"], json!(true), "{build}");
                assert_eq!(
                    status["capabilities"]["canDownload"],
                    json!(true),
                    "{build}"
                );
            }
        }
        // A distribution package's updates belong to its package manager.
        let linux = idle_status(&identity(&config("linux")));
        assert_eq!(linux["supported"], json!(false));
        assert_eq!(linux["capabilities"]["canDownload"], json!(false));
    }

    #[test]
    fn a_downloaded_release_names_its_path_and_can_record_the_installer() {
        let config = config("darwin");
        let candidate = select_update_candidate(&feed(), &config, false).expect("candidate");
        let release = crate::host::updater::store::UpdateReleaseRef {
            arch: candidate.arch.clone(),
            artifact: candidate.artifact.clone(),
            artifact_path: "releases/key/open-design.dmg".into(),
            checksum: candidate.checksum.clone(),
            checksum_path: "releases/key/checksum.json".into(),
            channel: candidate.channel.as_str().into(),
            downloaded_at: "2026-01-01T00:00:00.000Z".into(),
            key: "key".into(),
            metadata: candidate.metadata.clone(),
            metadata_path: "releases/key/metadata.json".into(),
            platform_key: candidate.platform_key.clone(),
            version: candidate.version.clone(),
        };
        let path = std::path::Path::new("/tmp/open-design.dmg");
        let downloaded = downloaded_status(&identity(&config), &release, path, None);
        assert_eq!(downloaded["state"], json!("downloaded"));
        assert_eq!(downloaded["availableVersion"], json!("1.2.3-beta.4"));
        assert_eq!(downloaded["downloadPath"], json!("/tmp/open-design.dmg"));
        assert_eq!(downloaded["artifact"]["type"], json!("dmg"));
        assert!(
            downloaded.get("installResult").is_none(),
            "a download is not an install"
        );

        let install = crate::host::updater::install::InstallResult {
            opened_at: "2026-01-01T00:00:01.000Z".into(),
            path: "/tmp/open-design.dmg".into(),
        };
        let installed = downloaded_status(&identity(&config), &release, path, Some(&install));
        assert_eq!(installed["state"], json!("downloaded"));
        assert_eq!(
            installed["installResult"]["path"],
            json!("/tmp/open-design.dmg")
        );
        assert_eq!(
            installed["installResult"]["openedAt"],
            json!("2026-01-01T00:00:01.000Z")
        );
        assert_eq!(
            installed["capabilities"]["canOpenInstaller"],
            json!(true),
            "an installer handoff is what this flow offers"
        );
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
