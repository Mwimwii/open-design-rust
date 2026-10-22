//! The bridge's `updater` group.
//!
//! Electron's updater is a whole subsystem: a release feed, a resumable
//! download with checksums, an installer handoff, native menu labels, and a
//! status stream. The Rust shell has ported the front half — the configuration
//! the updater decides with, the release feed's interpretation, and the
//! `check` decision — and none of the install half yet.
//!
//! What that means for a person running the app: the host can read a release
//! feed and tell you a newer version exists, but it still reports
//! `supported: false` (see `status::CAN_APPLY_PACKAGE_LAUNCHER_UPDATES`), so
//! the renderer keeps rendering no updater control. The alternative — showing a
//! download button that cannot finish — is the one outcome a status snapshot
//! must never produce.
//!
//! Every member of the group is answered here, including the two subscriptions,
//! because `isOpenDesignHostBridge` in `packages/host/src/detection.ts` requires
//! the whole group before it reports a host at all.

pub mod check;
pub mod config;
pub mod feed;
pub mod model;
pub mod status;
pub mod support;

use std::collections::HashMap;

use serde_json::Value;
use tauri::State;

use super::failure;
use crate::shell::ShellState;
use config::{resolve_config, UpdaterConfig, UpdaterConfigInput};

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
        // The updater's default download root lives under the namespace's data
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
                support::UpdateError::new(
                    "updater-unavailable",
                    "Desktop updater is not available.",
                )
            }),
        ),
        _ => status::error_status(
            identity,
            outcome.error.unwrap_or_else(|| {
                support::UpdateError::new("update-check-failed", "the update check failed")
            }),
        ),
    }
}

/// `updater.*`: one command for the whole group, dispatched on `action`.
///
/// `status` and `check` answer the status snapshot the renderer expects from
/// them. The three install-side actions answer with the reason they cannot run
/// yet instead of pretending to work; `quit` and `setMenuLabels` take the
/// action envelope, because the renderer reads them as action results.
#[tauri::command]
pub async fn host_updater(
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
            None => status::idle_status(&identity),
        }),
        "check" => Ok(outcome_status(&identity, check::check(&config).await)),
        "download" => Ok(status::error_status(
            &identity,
            support::UpdateError::new(
                "update-download-unavailable",
                "this build can see a newer release but cannot download it yet",
            ),
        )),
        "install" => Ok(status::error_status(
            &identity,
            support::UpdateError::new(
                "update-install-unavailable",
                "this build cannot hand an update to an installer yet",
            ),
        )),
        "clear-cache" => Ok(status::error_status(
            &identity,
            support::UpdateError::new(
                "update-store-unavailable",
                "this build keeps no update store yet",
            ),
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
    use serde_json::json;

    #[test]
    fn the_arch_spelling_is_the_one_the_renderer_compares() {
        assert!(matches!(node_arch(), "x64" | "arm64" | "ia32" | "arm"));
    }

    #[test]
    fn a_download_or_install_request_is_refused_by_name() {
        // The install half is not ported, and the refusal has to say which
        // half rather than blaming the platform or the feed.
        let config = config::resolve_config(&config::UpdaterConfigInput {
            env: [
                ("OD_UPDATE_PLATFORM".to_string(), "darwin".to_string()),
                ("OD_UPDATE_CHANNEL".to_string(), "beta".to_string()),
                ("OD_UPDATE_CURRENT_VERSION".to_string(), "1.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            packaged: true,
            ..Default::default()
        })
        .expect("config");
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
    }
}
