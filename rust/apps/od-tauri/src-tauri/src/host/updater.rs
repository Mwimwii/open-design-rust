//! The bridge's `updater` group.
//!
//! Electron's updater is a whole subsystem: a release feed, a resumable
//! download with checksums, an installer handoff, native menu labels, and a
//! status stream. Everything that decides or moves bytes is ported here —
//! configuration, feed interpretation, the check decision, the resumable
//! download with checksum verification, the update store, the installer
//! handoff, the status stream, and the auto-check schedule — so a macOS or
//! Windows build can update itself from the same feeds the Electron host reads.
//!
//! What is not ported: the native menu labels (this shell builds no native
//! application menu, so `set-menu-labels` reports that rather than pretending
//! to relabel one), and the launcher *payload* machinery the Electron scheduler
//! uses for a silent startup update and a deferred relaunch (this shell has no
//! payload runtime and always asks the feed for the installer). Neither changes
//! whether an install completes, so `status` advertises the platform as
//! supported (see `status::CAN_APPLY_PACKAGE_LAUNCHER_UPDATES`).
//!
//! Every member of the group is answered here, including the two subscriptions,
//! because `isOpenDesignHostBridge` in `packages/host/src/detection.ts` requires
//! the whole group before it reports a host at all.

pub mod check;
pub mod config;
pub mod download;
pub mod feed;
pub mod install;
pub mod model;
pub mod scheduler;
pub mod status;
pub mod store;
pub mod support;

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use super::{failure, success};
use crate::shell::ShellState;
use config::{resolve_config, UpdaterConfig, UpdaterConfigInput};
use support::UpdateError;

/// How long a quit waits before exiting, so the command's answer reaches the
/// renderer first.
const QUIT_GRACE: Duration = Duration::from_millis(250);

/// The event the updater's status stream is published on. The injected bridge
/// subscribes to this name (`host/bridge.rs`), and the renderer's
/// `subscribeToUpdaterStatus` reaches it.
pub const STATUS_EVENT: &str = "od:updater-status";

/// How often a running transfer may report progress. Without a floor, a fast
/// transfer emits one snapshot per chunk — thousands of documents nobody can
/// read, each one crossing the IPC boundary.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// Whether an installer has been opened in this process.
///
/// An opened installer is the end of updating for the running build: the app is
/// about to be replaced, so polling the feed again can only produce a status
/// nobody acts on. The scheduler reads this to stop (parity: the Electron
/// scheduler stopping once `installResult != null`).
static INSTALLER_OPENED: AtomicBool = AtomicBool::new(false);

/// Whether this build has already handed its update to the platform installer.
pub fn installer_opened() -> bool {
    INSTALLER_OPENED.load(std::sync::atomic::Ordering::Relaxed)
}

fn note_installer_opened() {
    INSTALLER_OPENED.store(true, std::sync::atomic::Ordering::Relaxed);
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

/// Resolve the updater configuration from the build's own identity and the
/// environment (parity: the host input `updater.ts` is constructed with).
///
/// The environment is read whole here rather than filtered to the updater's
/// keys, because resolution is also how an operator overrides a single knob
/// (`OD_UPDATE_METADATA_URL` against a local feed, say) without a rebuild.
pub fn resolve(state: &ShellState) -> Result<UpdaterConfig, String> {
    resolve_config(&UpdaterConfigInput {
        app_version: Some(state.identity.app_version.clone()),
        env: std::env::vars().collect::<HashMap<_, _>>(),
        packaged: true,
        // The updater's download root lives under the namespace's data
        // directory, so an update never writes outside the tree the app owns.
        runtime_base: Some(state.data_dir.clone()),
        ..Default::default()
    })
}

fn identity(config: &UpdaterConfig) -> status::StatusIdentity<'_> {
    status::StatusIdentity {
        arch: &config.arch,
        channel: &config.channel,
        current_version: &config.current_version,
        enabled: config.enabled,
        mode: config.mode,
        platform: &config.platform,
    }
}

/// The snapshot of a check that answered.
fn outcome_status(identity: &status::StatusIdentity<'_>, outcome: check::CheckOutcome) -> Value {
    match outcome.state {
        model::UpdateState::Available => match outcome.candidate {
            Some(candidate) => status::available_status(identity, &candidate),
            // Selection only reports `available` with a candidate; if that ever
            // stops being true, an idle snapshot is the safe answer.
            None => status::idle_status(identity),
        },
        model::UpdateState::NotAvailable => status::not_available_status(identity),
        model::UpdateState::Unsupported => status::unsupported_status(
            identity,
            outcome.error.unwrap_or_else(|| {
                UpdateError::new("updater-unavailable", "Desktop updater is not available.")
            }),
        ),
        _ => status::error_status(
            identity,
            outcome.error.unwrap_or_else(|| {
                UpdateError::new("update-check-failed", "the update check failed")
            }),
        ),
    }
}

/// The store's active release, resolved to a path inside the store.
///
/// A release this build already runs is not an update. After the external
/// installer hands off and the app relaunches onto that version, the store's
/// record is left over from the run that downloaded it: reporting it would keep
/// offering the version already running, and `install` would open the same
/// installer again on every relaunch. Such a record is dropped here (parity:
/// `loadActiveRelease` refusing `active.version <= currentVersion` while the
/// store is restored at startup).
fn active_release(
    config: &UpdaterConfig,
) -> Result<Option<(store::UpdateReleaseRef, std::path::PathBuf)>, UpdateError> {
    let owned = store::ensure_owned_root(&config.download_root, store::STORE_OWNER)?;
    let Some(release) = owned.metadata.active.clone() else {
        return Ok(None);
    };
    if feed::compare_versions(&release.version, &config.current_version) != Ordering::Greater {
        // The `currentVersion` exception the Electron host keeps is its
        // launcher reinstall gate; this shell has no launcher runtime, so a
        // release that is not strictly newer is always stale here.
        clear_applied_release(&owned);
        return Ok(None);
    }
    // Cloned rather than moved: the path has to be resolved against the store
    // that holds the record.
    let path = owned.resolve(&release.artifact_path)?;
    Ok(Some((release, path)))
}

/// Forget the store's active release because this build already runs it.
///
/// A failed write is logged, not returned: the caller only wanted to know
/// whether an update is pending, and reporting a store error instead would
/// hide a status the renderer can still act on. The next read retries.
fn clear_applied_release(owned: &store::OwnedRoot) {
    let metadata = store::StoreMetadata {
        active: None,
        ..owned.metadata.clone()
    };
    if let Err(error) = store::write_metadata(&owned.layout, &metadata) {
        tracing::warn!(
            code = %error.code,
            message = %error.message,
            "could not clear the applied update from the store"
        );
    }
}

/// Fetch the candidate's release, reporting progress on the status stream.
///
/// `Ok` carries the downloaded snapshot and `Err` an error snapshot; both are
/// answers the renderer can render, which is why the failure is not a `String`.
/// The unattended path in `scheduler` reads the same distinction to decide
/// whether the next poll backs off.
async fn download_candidate(
    app: &AppHandle,
    config: &UpdaterConfig,
    identity: &status::StatusIdentity<'_>,
    candidate: &feed::UpdateCandidate,
) -> Result<Value, Value> {
    let mut last_emit = std::time::Instant::now()
        .checked_sub(PROGRESS_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);
    let mut on_progress = |received: u64, total: Option<u64>| {
        if last_emit.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        last_emit = std::time::Instant::now();
        let _ = app.emit(
            STATUS_EVENT,
            status::downloading_status(identity, candidate, received, total),
        );
    };
    match download::download_release(config, candidate, &mut on_progress).await {
        Ok(release) => match active_release(config) {
            Ok(Some((_, path))) => Ok(status::downloaded_status(identity, &release, &path, None)),
            Ok(None) => Ok(status::idle_status(identity)),
            Err(error) => Err(status::error_status(identity, error)),
        },
        Err(error) => Err(status::error_status(identity, error)),
    }
}

/// Hand the store's active release to the platform installer.
///
/// The `bool` is whether the installer opened: that is the point after which
/// this build stops updating itself, so the scheduler stops polling.
fn install_active(config: &UpdaterConfig, identity: &status::StatusIdentity<'_>) -> (Value, bool) {
    match active_release(config) {
        Ok(Some((release, path))) => match install::open_installer(&config.platform, &path) {
            Ok(result) => {
                note_installer_opened();
                (
                    status::downloaded_status(identity, &release, &path, Some(&result)),
                    true,
                )
            }
            Err(error) => (status::error_status(identity, error), false),
        },
        Ok(None) => (
            status::error_status(
                identity,
                UpdateError::new(
                    "no-downloaded-release",
                    "there is no downloaded update to install",
                ),
            ),
            false,
        ),
        Err(error) => (status::error_status(identity, error), false),
    }
}

/// `updater.*`: one command for the whole group, dispatched on `action`.
///
/// `status`, `check`, `download`, `install`, and `clear-cache` answer the status
/// snapshot the renderer expects from them. `quit` and `setMenuLabels` take the
/// action envelope, because the renderer reads them as action results.
#[tauri::command]
pub async fn host_updater(
    app: AppHandle,
    state: State<'_, ShellState>,
    action: Option<String>,
    options: Option<Value>,
    labels: Option<Value>,
) -> Result<Value, String> {
    let _ = (options, labels);
    let config = resolve(&state)?;
    let identity = identity(&config);
    match action.as_deref().unwrap_or("status") {
        "status" => Ok(match check::preflight(&config) {
            Some(refusal) => status::unsupported_status(
                &identity,
                refusal.error.expect("a refusal carries an error"),
            ),
            None => match active_release(&config) {
                Ok(Some((release, path))) => {
                    status::downloaded_status(&identity, &release, &path, None)
                }
                Ok(None) => status::idle_status(&identity),
                Err(error) => status::error_status(&identity, error),
            },
        }),
        "check" => Ok(outcome_status(&identity, check::check(&config).await)),
        "download" => {
            // A download always re-reads the feed: the artifact is chosen by
            // the same decision that reported it available, so nothing can be
            // fetched that a check would not have offered.
            let outcome = check::check(&config).await;
            let Some(candidate) = outcome.candidate.clone() else {
                return Ok(outcome_status(&identity, outcome));
            };
            Ok(download_candidate(&app, &config, &identity, &candidate)
                .await
                .unwrap_or_else(|snapshot| snapshot))
        }
        "install" => Ok(install_active(&config, &identity).0),
        "clear-cache" => Ok(match store::clear_cached_releases(&config.download_root) {
            Ok(()) => status::idle_status(&identity),
            Err(error) => status::error_status(&identity, error),
        }),
        "quit" => {
            // The renderer calls this after handing an installer to the
            // platform; on Windows the installer cannot replace a running
            // binary, so the app has to go. The answer is sent first, then the
            // process exits from a detached thread.
            std::thread::spawn(move || {
                std::thread::sleep(QUIT_GRACE);
                app.exit(0);
            });
            Ok(success())
        }
        "set-menu-labels" => Ok(failure(
            "this shell has no native application menu to relabel",
        )),
        other => Ok(failure(format!("unknown updater action '{other}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> UpdaterConfig {
        config::resolve_config(&config::UpdaterConfigInput {
            env: [
                ("OD_UPDATE_PLATFORM".to_string(), "darwin".to_string()),
                ("OD_UPDATE_ARCH".to_string(), "arm64".to_string()),
                ("OD_UPDATE_CHANNEL".to_string(), "beta".to_string()),
                ("OD_UPDATE_CURRENT_VERSION".to_string(), "1.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            packaged: true,
            ..Default::default()
        })
        .expect("config")
    }

    #[test]
    fn the_arch_spelling_is_the_one_the_renderer_compares() {
        assert!(matches!(node_arch(), "x64" | "arm64" | "ia32" | "arm"));
    }

    #[test]
    fn a_check_outcome_maps_onto_the_snapshot_the_renderer_expects() {
        let config = config();
        let identity = identity(&config);
        assert_eq!(
            outcome_status(
                &identity,
                check::CheckOutcome {
                    state: model::UpdateState::Available,
                    candidate: None,
                    error: None,
                },
            )["state"],
            json!("idle"),
            "an available state without a candidate must not invent one"
        );
        assert_eq!(
            outcome_status(
                &identity,
                check::CheckOutcome {
                    state: model::UpdateState::Error,
                    candidate: None,
                    error: None,
                },
            )["error"]["code"],
            json!("update-check-failed")
        );
        assert_eq!(
            outcome_status(
                &identity,
                check::CheckOutcome {
                    state: model::UpdateState::Unsupported,
                    candidate: None,
                    error: None,
                },
            )["state"],
            json!("unsupported")
        );
        assert_eq!(
            outcome_status(
                &identity,
                check::CheckOutcome {
                    state: model::UpdateState::NotAvailable,
                    candidate: None,
                    error: None,
                },
            )["state"],
            json!("not-available")
        );
    }

    #[test]
    fn a_store_that_holds_nothing_reports_idle_rather_than_a_stale_release() {
        let root = std::env::temp_dir().join(format!(
            "od-updater-active-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        let mut config = config();
        config.download_root = root.clone();
        assert!(
            active_release(&config).expect("store").is_none(),
            "a fresh store holds no release"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    /// A store holding one release at `version`, with the artifact on disk.
    fn store_holding(root: &std::path::Path, version: &str) -> UpdaterConfig {
        let owned = store::ensure_owned_root(root, store::STORE_OWNER).expect("store");
        std::fs::create_dir_all(root.join("releases/key")).expect("releases dir");
        std::fs::write(root.join("releases/key/OpenDesign.dmg"), b"artifact").expect("artifact");
        let mut metadata = owned.metadata.clone();
        metadata.active = Some(store::UpdateReleaseRef {
            arch: "arm64".into(),
            artifact: model::ArtifactSnapshot {
                name: Some("OpenDesign.dmg".into()),
                platform_key: Some("mac".into()),
                size: Some(8),
                artifact_type: Some("dmg".into()),
                url: "https://example.test/OpenDesign.dmg".into(),
            },
            artifact_path: "releases/key/OpenDesign.dmg".into(),
            checksum: model::ChecksumSnapshot {
                algorithm: model::ChecksumAlgorithm::Sha256,
                url: None,
                value: Some("a".repeat(64)),
            },
            checksum_path: "releases/key/checksum.json".into(),
            channel: "beta".into(),
            downloaded_at: "2026-01-01T00:00:00.000Z".into(),
            key: "1.2.3-mac-arm64-abcdef123456".into(),
            metadata: serde_json::Map::new(),
            metadata_path: "releases/key/metadata.json".into(),
            platform_key: "mac".into(),
            version: version.into(),
        });
        store::write_metadata(&owned.layout, &metadata).expect("write metadata");
        let mut config = config();
        config.download_root = root.to_path_buf();
        config
    }

    fn scratch_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "od-updater-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ))
    }

    #[test]
    fn a_release_this_build_already_runs_is_not_offered_again() {
        let root = scratch_root("applied");
        // `config()` runs 1.0.0; the store's release is the one already applied.
        let config = store_holding(&root, "1.0.0");
        assert!(
            active_release(&config).expect("store").is_none(),
            "the running version must not be offered as an update"
        );
        let reopened = store::ensure_owned_root(&root, store::STORE_OWNER).expect("reopen");
        assert_eq!(
            reopened.metadata.active, None,
            "the stale record is cleared, not just ignored, so the UI cannot revive it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_older_release_left_in_the_store_is_dropped_too() {
        let root = scratch_root("older");
        let config = store_holding(&root, "0.9.0");
        assert!(active_release(&config).expect("store").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_newer_release_in_the_store_stays_actionable() {
        let root = scratch_root("newer");
        let config = store_holding(&root, "1.1.0");
        let active = active_release(&config).expect("store").expect("release");
        assert_eq!(active.0.version, "1.1.0");
        assert!(active.1.ends_with("OpenDesign.dmg"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
