//! Recurring auto-check scheduling (parity: `createDesktopUpdaterScheduler` in
//! `apps/desktop/src/main/updater/scheduler.ts`).
//!
//! Pacing is separated from the loop so the two decisions that can strand a
//! user are testable without a clock or a feed: how long to wait after a poll,
//! and what to do with a delay that would spin. The loop itself is one
//! background task that polls the feed, applies `autoDownload` / `autoOpen`, and
//! publishes every result on the status stream.
//!
//! Not ported, and not portable: the startup *silent payload update*. Electron's
//! scheduler may install a `payload` artifact on the first tick without asking,
//! because it can relaunch the app through the launcher helper. This shell
//! ships one installer artifact and has no payload runtime — every check asks
//! the feed for the installer — so there is nothing to silently apply.

use std::sync::Once;
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use super::config::UpdaterConfig;
use super::model::UpdateState;
use super::{check, STATUS_EVENT};
use crate::shell::ShellState;

/// The floor a scheduled poll delay is clamped to. A zero delay would spin the
/// loop; the configuration already refuses a non-positive interval, so this is
/// the defence for a delay that is computed rather than read.
pub const MIN_SCHEDULED_POLL_DELAY_MS: u64 = 1_000;

/// What one scheduled poll resolved to, which is all the next wait depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollResult {
    /// The poll produced a snapshot without an error.
    Answered,
    /// The check or the transfer failed; the next poll backs off.
    Failed,
    /// An installer was opened, so the running build is about to be replaced.
    Installed,
}

/// A wait, clamped so a non-positive delay cannot turn the loop into a spin.
fn scheduled_delay(delay_ms: u64) -> u64 {
    if delay_ms > 0 {
        return delay_ms;
    }
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "refusing a non-positive updater poll delay; using the {MIN_SCHEDULED_POLL_DELAY_MS}ms floor"
        );
    });
    MIN_SCHEDULED_POLL_DELAY_MS
}

/// The wait before the next poll, and the failure streak it continues.
///
/// A poll that answered resets the streak and waits the channel's interval;
/// a failed one doubles the configured initial backoff per consecutive failure
/// up to the configured maximum (parity: `nextDelay`).
pub fn next_delay(result: PollResult, failures: u32, config: &UpdaterConfig) -> (u64, u32) {
    if result != PollResult::Failed {
        return (scheduled_delay(config.check_interval_ms), 0);
    }
    let failures = failures.saturating_add(1);
    let factor = 1u64 << failures.saturating_sub(1).min(63);
    let backoff = config
        .check_backoff_initial_ms
        .saturating_mul(factor)
        .min(config.check_backoff_max_ms);
    (scheduled_delay(backoff), failures)
}

/// Start the auto-check loop for this process.
///
/// Nothing happens when the updater is disabled, when `autoCheck` is off, or
/// when an installer was already opened: in each case a poll could only produce
/// a status nobody would act on.
pub fn start(app: AppHandle) {
    let Some(config) = scheduler_config(&app) else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        // The first poll is delayed: startup belongs to the window and the
        // embedded daemon, and a feed fetch must not compete with them.
        tokio::time::sleep(Duration::from_millis(scheduled_delay(
            config.check_initial_delay_ms,
        )))
        .await;

        let mut failures = 0u32;
        loop {
            let (snapshot, result) = poll_once(&app, &config).await;
            let _ = app.emit(STATUS_EVENT, snapshot);
            if result == PollResult::Installed {
                tracing::info!("updater auto-check stopped: an installer was opened");
                return;
            }
            let (delay, next_failures) = next_delay(result, failures, &config);
            failures = next_failures;
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    });
}

/// The configuration a scheduled poll runs with, or `None` when polling is off.
fn scheduler_config(app: &AppHandle) -> Option<UpdaterConfig> {
    if super::installer_opened() {
        return None;
    }
    let state = app.state::<ShellState>();
    let config = match super::resolve(&state) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "updater auto-check is off: the configuration did not resolve");
            return None;
        }
    };
    // Parity: `shouldAutoCheck`.
    (config.enabled && config.auto_check).then_some(config)
}

/// One poll: check, then apply the unattended-update preferences.
///
/// `autoDownload` is on by default in the host configuration, so an available
/// release is fetched without waiting for the renderer; `autoOpen` is off by
/// default, so opening the installer remains an explicit choice.
async fn poll_once(app: &AppHandle, config: &UpdaterConfig) -> (Value, PollResult) {
    let identity = super::identity(config);
    let outcome = check::check(config).await;
    let mut failed = outcome.state == UpdateState::Error || outcome.error.is_some();
    let candidate = outcome.candidate.clone();
    let mut snapshot = super::outcome_status(&identity, outcome);

    if let Some(candidate) = candidate.filter(|_| config.auto_download) {
        match super::download_candidate(app, config, &identity, &candidate).await {
            Ok(downloaded) => {
                snapshot = downloaded;
                if config.auto_open {
                    let (installed, opened) = super::install_active(config, &identity);
                    snapshot = installed;
                    if opened {
                        return (snapshot, PollResult::Installed);
                    }
                    failed = true;
                }
            }
            Err(error) => {
                snapshot = error;
                failed = true;
            }
        }
    }

    (
        snapshot,
        if failed {
            PollResult::Failed
        } else {
            PollResult::Answered
        },
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(env: &[(&str, &str)]) -> UpdaterConfig {
        let mut environment: HashMap<String, String> = [
            ("OD_UPDATE_PLATFORM".to_string(), "darwin".to_string()),
            ("OD_UPDATE_ARCH".to_string(), "arm64".to_string()),
            ("OD_UPDATE_CHANNEL".to_string(), "beta".to_string()),
            ("OD_UPDATE_CURRENT_VERSION".to_string(), "1.0.0".to_string()),
            (
                "OD_UPDATE_CHECK_INTERVAL_MS".to_string(),
                "60000".to_string(),
            ),
            (
                "OD_UPDATE_CHECK_BACKOFF_INITIAL_MS".to_string(),
                "10000".to_string(),
            ),
            (
                "OD_UPDATE_CHECK_BACKOFF_MAX_MS".to_string(),
                "90000".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        for (key, value) in env {
            environment.insert((*key).to_string(), (*value).to_string());
        }
        super::super::config::resolve_config(&super::super::config::UpdaterConfigInput {
            env: environment,
            packaged: true,
            ..Default::default()
        })
        .expect("config")
    }

    #[test]
    fn an_answered_poll_waits_the_interval_and_clears_the_streak() {
        let config = config(&[]);
        assert_eq!(
            next_delay(PollResult::Answered, 4, &config),
            (60_000, 0),
            "a healthy poll must not inherit a previous streak"
        );
    }

    #[test]
    fn failures_double_the_initial_backoff_up_to_the_maximum() {
        let config = config(&[]);
        // 10s, 20s, 40s, 80s, then clamped at 90s.
        let mut failures = 0u32;
        let mut delays = Vec::new();
        for _ in 0..6 {
            let (delay, next) = next_delay(PollResult::Failed, failures, &config);
            delays.push(delay);
            failures = next;
        }
        assert_eq!(delays, vec![10_000, 20_000, 40_000, 80_000, 90_000, 90_000]);
        assert_eq!(failures, 6);
    }

    #[test]
    fn a_huge_streak_saturates_instead_of_overflowing() {
        let config = config(&[]);
        assert_eq!(
            next_delay(PollResult::Failed, u32::MAX, &config).0,
            90_000,
            "the shift must saturate, not panic or wrap"
        );
    }

    #[test]
    fn a_non_positive_delay_falls_back_to_the_floor() {
        assert_eq!(scheduled_delay(0), MIN_SCHEDULED_POLL_DELAY_MS);
        assert_eq!(scheduled_delay(500), 500, "a positive delay is kept as-is");
    }

    #[test]
    fn a_completed_install_is_not_a_failure_streak() {
        assert_eq!(
            next_delay(PollResult::Installed, 3, &config(&[])),
            (60_000, 0)
        );
    }
}
