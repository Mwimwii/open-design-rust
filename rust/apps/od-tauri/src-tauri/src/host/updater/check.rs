//! The `check` action: read the release feed and decide whether it offers this
//! build something newer (parity: `checkForCandidate` in `updater.ts`).
//!
//! The split is deliberate: [`fetch_feed`] is the only IO, [`interpret_feed`]
//! is the whole decision and takes an already-parsed document, and [`check`]
//! is the two joined. That way the decision — channel mismatch, unpublished
//! platform, "not newer" — is testable without a feed server.

use std::cmp::Ordering;
use std::time::Duration;

use serde_json::{Map, Value};

use super::config::{is_supported_package_launcher_platform, UpdateMode, UpdaterConfig};
use super::feed::{compare_versions, select_update_candidate_with_fallback, UpdateCandidate};
use super::model::UpdateState;
use super::support::UpdateError;

/// How long a feed request may take before the check calls it unreachable.
///
/// A check is a background courtesy, never a startup dependency, so a slow
/// mirror is reported rather than waited out.
pub const FEED_TIMEOUT: Duration = Duration::from_secs(20);

/// What one check concluded.
///
/// `candidate` is present only for `available`; its absence for any other state
/// is what stops a later step from downloading something nobody selected.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckOutcome {
    pub state: UpdateState,
    pub candidate: Option<UpdateCandidate>,
    pub error: Option<UpdateError>,
}

impl CheckOutcome {
    fn refused(state: UpdateState, error: UpdateError) -> Self {
        Self {
            state,
            candidate: None,
            error: Some(error),
        }
    }

    fn found(candidate: UpdateCandidate) -> Self {
        Self {
            state: UpdateState::Available,
            candidate: Some(candidate),
            error: None,
        }
    }

    fn current() -> Self {
        Self {
            state: UpdateState::NotAvailable,
            candidate: None,
            error: None,
        }
    }
}

/// The refusal a check can answer without touching the network.
///
/// A disabled updater and a platform whose updates belong to someone else are
/// both answered here, so neither spends a request to learn what the build
/// already knows.
pub fn preflight(config: &UpdaterConfig) -> Option<CheckOutcome> {
    if !config.enabled {
        return Some(CheckOutcome::refused(
            UpdateState::Unsupported,
            UpdateError::new("updater-disabled", "Desktop updater is disabled."),
        ));
    }
    if config.mode != UpdateMode::PackageLauncher {
        return Some(CheckOutcome::refused(
            UpdateState::Unsupported,
            UpdateError::new(
                "update-mode-not-implemented",
                "js-incremental updates are not implemented yet",
            ),
        ));
    }
    if !is_supported_package_launcher_platform(&config.platform) {
        return Some(CheckOutcome::refused(
            UpdateState::Unsupported,
            UpdateError::new(
                "unsupported-platform",
                "package-launcher updates are currently supported on macOS and Windows only",
            ),
        ));
    }
    None
}

/// Read a release feed (parity: `fetchJson`).
///
/// The `Err` text reaches the renderer as the `metadata-unreachable` message, so
/// it names the failure the reader can act on rather than an error code they
/// cannot.
pub async fn fetch_feed(url: &str) -> Result<Map<String, Value>, String> {
    let client = reqwest::Client::builder()
        .timeout(FEED_TIMEOUT)
        .build()
        .map_err(|error| format!("release metadata client could not start: {error}"))?;
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| format!("release metadata request failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "release metadata request failed with HTTP {}",
            status.as_u16()
        ));
    }
    let body = response
        .text()
        .await
        .map_err(|error| format!("release metadata could not be read: {error}"))?;
    match serde_json::from_str::<Value>(&body) {
        Ok(Value::Object(metadata)) => Ok(metadata),
        Ok(_) => Err("release metadata is not a JSON object".to_string()),
        Err(error) => Err(format!("release metadata is not JSON: {error}")),
    }
}

/// Decide what a feed document means for this build (parity: the body of
/// `checkForCandidate` after the fetch).
///
/// This shell never prefers a launcher payload — it ships one self-contained
/// bundle — so the fallback is asked for the installer directly. The
/// reinstall floor is not consulted either: it exists to protect a launcher
/// runtime this build does not have.
pub fn interpret_feed(metadata: &Map<String, Value>, config: &UpdaterConfig) -> CheckOutcome {
    match select_update_candidate_with_fallback(metadata, config, false) {
        Err(rejection) => CheckOutcome::refused(rejection.state, rejection.error),
        Ok(candidate) => {
            if compare_versions(&candidate.version, &config.current_version) == Ordering::Greater {
                CheckOutcome::found(candidate)
            } else {
                // Not newer is not an error: a feed that still lists this
                // build (or an older one) means there is nothing to do.
                CheckOutcome::current()
            }
        }
    }
}

/// Read the feed and interpret it (parity: `checkForCandidate`).
pub async fn check(config: &UpdaterConfig) -> CheckOutcome {
    if let Some(refusal) = preflight(config) {
        return refusal;
    }
    match fetch_feed(&config.metadata_url).await {
        Ok(metadata) => interpret_feed(&metadata, config),
        Err(message) => CheckOutcome::refused(
            UpdateState::Error,
            UpdateError::new("metadata-unreachable", message),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::updater::config::{resolve_config, UpdaterConfigInput};
    use serde_json::json;
    use std::collections::HashMap;

    fn config(platform: &str, current_version: &str) -> UpdaterConfig {
        let env: HashMap<String, String> = [
            ("OD_UPDATE_PLATFORM".to_string(), platform.to_string()),
            // Apple silicon, so the feed's `mac` key is the one this build asks
            // for; the arch is an input, not an accident of the test host.
            ("OD_UPDATE_ARCH".to_string(), "arm64".to_string()),
            ("OD_UPDATE_CHANNEL".to_string(), "beta".to_string()),
            (
                "OD_UPDATE_CURRENT_VERSION".to_string(),
                current_version.to_string(),
            ),
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

    fn feed(version: &str) -> Map<String, Value> {
        json!({
            "channel": "beta",
            "releaseVersion": version,
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
    fn a_newer_release_is_available_and_names_the_candidate() {
        let outcome = interpret_feed(&feed("1.2.3-beta.4"), &config("darwin", "1.2.3-beta.3"));
        assert_eq!(outcome.state, UpdateState::Available);
        let candidate = outcome.candidate.expect("a candidate");
        assert_eq!(candidate.version, "1.2.3-beta.4");
        assert_eq!(candidate.artifact.url, "https://example.test/a.dmg");
        assert!(outcome.error.is_none());
    }

    #[test]
    fn the_same_or_an_older_release_is_not_an_update() {
        for current in ["1.2.3-beta.4", "1.2.3-beta.9", "1.3.0"] {
            let outcome = interpret_feed(&feed("1.2.3-beta.4"), &config("darwin", current));
            assert_eq!(
                outcome.state,
                UpdateState::NotAvailable,
                "current {current}"
            );
            assert!(outcome.candidate.is_none());
            assert!(outcome.error.is_none());
        }
    }

    #[test]
    fn a_feed_for_another_channel_is_an_error_a_person_can_act_on() {
        let mut wrong_channel = feed("9.9.9");
        wrong_channel.insert("channel".into(), json!("stable"));
        let outcome = interpret_feed(&wrong_channel, &config("darwin", "1.0.0"));
        assert_eq!(outcome.state, UpdateState::Error);
        let error = outcome.error.expect("an error");
        assert_eq!(error.code, "metadata-channel-mismatch");
        assert!(error.message.contains("stable"));
        assert!(
            outcome.candidate.is_none(),
            "a refusal yields nothing to download"
        );
    }

    #[test]
    fn preflight_answers_disabled_and_foreign_platforms_without_a_request() {
        let disabled = config("darwin", "1.0.0");
        let mut disabled = disabled;
        disabled.enabled = false;
        let refusal = preflight(&disabled).expect("disabled");
        assert_eq!(refusal.state, UpdateState::Unsupported);
        assert_eq!(refusal.error.expect("error").code, "updater-disabled");

        let linux = preflight(&config("linux", "1.0.0")).expect("linux");
        assert_eq!(linux.state, UpdateState::Unsupported);
        assert_eq!(linux.error.expect("error").code, "unsupported-platform");

        let incremental = {
            let mut config = config("darwin", "1.0.0");
            config.mode = UpdateMode::JsIncremental;
            config
        };
        assert_eq!(
            preflight(&incremental)
                .expect("incremental")
                .error
                .expect("error")
                .code,
            "update-mode-not-implemented"
        );

        assert!(preflight(&config("darwin", "1.0.0")).is_none());
        assert!(preflight(&config("win32", "1.0.0")).is_none());
    }

    #[test]
    fn a_feed_that_omits_the_platform_is_a_refusal_not_a_crash() {
        let mut missing = feed("9.9.9");
        missing["platforms"]
            .as_object_mut()
            .expect("platforms")
            .remove("mac");
        let outcome = interpret_feed(&missing, &config("darwin", "1.0.0"));
        assert_eq!(outcome.state, UpdateState::Error);
        assert_eq!(outcome.error.expect("error").code, "no-compatible-artifact");
    }
}
