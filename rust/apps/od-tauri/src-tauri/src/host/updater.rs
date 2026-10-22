//! The bridge's `updater` group.
//!
//! Electron's updater is a whole subsystem: a release feed, a resumable
//! download with checksums, an installer handoff, native menu labels, and a
//! status stream. Everything that decides or moves bytes is ported here —
//! configuration, feed interpretation, the check decision, the resumable
//! download with checksum verification, the update store, and the installer
//! handoff — so a macOS or Windows build can update itself from the same feeds
//! the Electron host reads.
//!
//! What is not ported: the status stream (a download answers when it finishes
//! rather than reporting progress while it runs), the update scheduler and its
//! deferred-launch handling, and the native menu labels. None of them changes
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
pub mod status;
pub mod store;
pub mod support;

use std::collections::HashMap;
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
fn active_release(
    config: &UpdaterConfig,
) -> Result<Option<(store::UpdateReleaseRef, std::path::PathBuf)>, UpdateError> {
    let owned = store::ensure_owned_root(&config.download_root, store::STORE_OWNER)?;
    // Cloned rather than moved: the path has to be resolved against the store
    // that holds the record.
    match owned.metadata.active.clone() {
        Some(release) => {
            let path = owned.resolve(&release.artifact_path)?;
            Ok(Some((release, path)))
        }
        None => Ok(None),
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
                    status::downloading_status(&identity, &candidate, received, total),
                );
            };
            let downloaded =
                download::download_release(&config, &candidate, &mut on_progress).await;
            Ok(match downloaded {
                Ok(release) => match active_release(&config) {
                    Ok(Some((_, path))) => {
                        status::downloaded_status(&identity, &release, &path, None)
                    }
                    Ok(None) => status::idle_status(&identity),
                    Err(error) => status::error_status(&identity, error),
                },
                Err(error) => status::error_status(&identity, error),
            })
        }
        "install" => Ok(match active_release(&config) {
            Ok(Some((release, path))) => match install::open_installer(&config.platform, &path) {
                Ok(result) => status::downloaded_status(&identity, &release, &path, Some(&result)),
                Err(error) => status::error_status(&identity, error),
            },
            Ok(None) => status::error_status(
                &identity,
                UpdateError::new(
                    "no-downloaded-release",
                    "there is no downloaded update to install",
                ),
            ),
            Err(error) => status::error_status(&identity, error),
        }),
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
}
