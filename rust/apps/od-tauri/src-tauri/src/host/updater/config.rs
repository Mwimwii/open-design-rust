//! Updater configuration resolution (parity: `updater/config.ts`).
//!
//! Env-driven configuration for the desktop updater plus the capability
//! derivation shared with snapshot construction. Pure input→value logic: no
//! filesystem access, no updater state.
//!
//! The Electron host resolves this from a sidecar `source`; the Rust shell is
//! always the packaged app, so `packaged` replaces that input and decides the
//! default for `enabled`.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use od_core::release::{
    is_release_channel, release_channel_from_name, release_channel_from_version, ReleaseChannel,
};

use super::model::UpdateCapabilities;
use super::node_arch;

/// The release origin the default metadata URL is built from.
pub const DEFAULT_RELEASE_ORIGIN: &str = "https://releases.open-design.ai";

const BETA_POLL_INTERVAL_MS: u64 = 15 * 60 * 1000;
const STABLE_POLL_INTERVAL_MS: u64 = 6 * 60 * 60 * 1000;
const DEFAULT_POLL_INITIAL_DELAY_MS: u64 = 5_000;
const DEFAULT_POLL_BACKOFF_INITIAL_MS: u64 = 60_000;
const DEFAULT_POLL_BACKOFF_MAX_MS: u64 = 30 * 60 * 1000;

/// The environment variables the updater reads (parity: `DESKTOP_UPDATE_ENV`).
pub mod env_keys {
    pub const ARCH: &str = "OD_UPDATE_ARCH";
    pub const AUTO_CHECK: &str = "OD_UPDATE_AUTO_CHECK";
    pub const AUTO_DOWNLOAD: &str = "OD_UPDATE_AUTO_DOWNLOAD";
    pub const AUTO_OPEN: &str = "OD_UPDATE_AUTO_OPEN";
    pub const CHECK_BACKOFF_INITIAL_MS: &str = "OD_UPDATE_CHECK_BACKOFF_INITIAL_MS";
    pub const CHECK_BACKOFF_MAX_MS: &str = "OD_UPDATE_CHECK_BACKOFF_MAX_MS";
    pub const CHECK_INITIAL_DELAY_MS: &str = "OD_UPDATE_CHECK_INITIAL_DELAY_MS";
    pub const CHECK_INTERVAL_MS: &str = "OD_UPDATE_CHECK_INTERVAL_MS";
    pub const CHANNEL: &str = "OD_UPDATE_CHANNEL";
    pub const CURRENT_VERSION: &str = "OD_UPDATE_CURRENT_VERSION";
    pub const DOWNLOAD_ROOT: &str = "OD_UPDATE_DOWNLOAD_ROOT";
    pub const ENABLED: &str = "OD_UPDATE_ENABLED";
    pub const INSTALLED_VERSION: &str = "OD_UPDATE_INSTALLED_VERSION";
    pub const METADATA_URL: &str = "OD_UPDATE_METADATA_URL";
    pub const MODE: &str = "OD_UPDATE_MODE";
    pub const OPEN_DRY_RUN: &str = "OD_UPDATE_OPEN_DRY_RUN";
    pub const PLATFORM: &str = "OD_UPDATE_PLATFORM";
}

/// The update mechanism (parity: `DESKTOP_UPDATE_MODES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMode {
    /// Download the platform installer and hand off to it.
    PackageLauncher,
    /// Patch the JS bundle in place. Still unimplemented in both hosts.
    JsIncremental,
}

impl UpdateMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PackageLauncher => "package-launcher",
            Self::JsIncremental => "js-incremental",
        }
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "package-launcher" => Some(Self::PackageLauncher),
            "js-incremental" => Some(Self::JsIncremental),
            _ => None,
        }
    }
}

/// Everything the updater decides with (parity: `DesktopUpdaterConfig`).
///
/// The launcher paths are carried because the Electron host reads its
/// physically installed outer package through them; the Rust shell ships one
/// self-contained bundle, so they stay `None` and the reinstall gate falls
/// through to the pattern it keeps for an unreadable outer version.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdaterConfig {
    pub arch: String,
    pub auto_check: bool,
    pub auto_download: bool,
    pub auto_open: bool,
    pub check_backoff_initial_ms: u64,
    pub check_backoff_max_ms: u64,
    pub check_initial_delay_ms: u64,
    pub check_interval_ms: u64,
    pub channel: ReleaseChannel,
    pub current_version: String,
    pub download_root: PathBuf,
    pub enabled: bool,
    pub installed_version_override: Option<String>,
    pub installer_observation_root: Option<PathBuf>,
    pub launcher_launch_path: Option<PathBuf>,
    pub launcher_root: Option<PathBuf>,
    pub launcher_payload_extractor_path: Option<PathBuf>,
    pub launcher_runtime_path: Option<PathBuf>,
    pub metadata_url: String,
    pub mode: UpdateMode,
    pub namespace: Option<String>,
    pub open_dry_run: bool,
    pub platform: String,
    pub runtime_base: PathBuf,
}

/// The inputs the host supplies; the environment overrides them
/// (parity: `DesktopUpdaterConfigInput`).
#[derive(Debug, Clone, Default)]
pub struct UpdaterConfigInput {
    pub app_version: Option<String>,
    pub arch: Option<String>,
    pub current_version: Option<String>,
    pub download_root: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub installer_observation_root: Option<PathBuf>,
    pub launcher_launch_path: Option<PathBuf>,
    pub launcher_payload_extractor_path: Option<PathBuf>,
    pub launcher_root: Option<PathBuf>,
    pub launcher_runtime_path: Option<PathBuf>,
    pub mode: Option<UpdateMode>,
    pub namespace: Option<String>,
    pub packaged: bool,
    pub platform: Option<String>,
    pub runtime_base: Option<PathBuf>,
}

fn env_value<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

/// A boolean env value (parity: `isTruthyEnv`).
///
/// Absent, `1`/`0`, `true`/`false`, `yes`/`no`; anything else is a
/// configuration error rather than a silent default.
fn truthy_env(env: &HashMap<String, String>, key: &str) -> Result<Option<bool>, String> {
    let Some(value) = env_value(env, key) else {
        return Ok(None);
    };
    match value {
        "1" | "true" | "yes" => Ok(Some(true)),
        "0" | "false" | "no" => Ok(Some(false)),
        other => Err(format!(
            "boolean env value must be one of 1/0/true/false/yes/no, got {other}"
        )),
    }
}

fn duration_env(env: &HashMap<String, String>, key: &str, fallback: u64) -> Result<u64, String> {
    let Some(value) = env_value(env, key) else {
        return Ok(fallback);
    };
    let parsed: f64 = value
        .parse()
        .map_err(|_| format!("{key} must be a non-negative number of milliseconds"))?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(format!(
            "{key} must be a non-negative number of milliseconds"
        ));
    }
    Ok(parsed as u64)
}

fn positive_duration_env(
    env: &HashMap<String, String>,
    key: &str,
    fallback: u64,
) -> Result<u64, String> {
    let parsed = duration_env(env, key, fallback)?;
    if parsed == 0 {
        return Err(format!("{key} must be greater than 0 milliseconds"));
    }
    Ok(parsed)
}

/// Make a path absolute and lexically clean (parity: `node:path`'s `resolve`).
///
/// Lexical on purpose: the updater resolves roots that do not exist yet, and
/// canonicalizing would need them on disk.
pub fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut cleaned = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `resolve` pops a real directory, but never above the root.
                if !cleaned.pop() {
                    cleaned.push(component.as_os_str());
                }
            }
            other => cleaned.push(other.as_os_str()),
        }
    }
    cleaned
}

fn has_null_byte(path: &Path) -> bool {
    path.to_string_lossy().contains('\0')
}

/// A download root the updater may create: absolute, no NUL.
fn normalize_download_root(value: &Path) -> Result<PathBuf, String> {
    if has_null_byte(value) {
        return Err("update download root must not contain null bytes".to_string());
    }
    if !value.is_absolute() {
        return Err(format!(
            "update download root must be absolute: {}",
            value.display()
        ));
    }
    Ok(resolve_path(value))
}

fn normalize_optional_root(value: Option<&Path>, label: &str) -> Result<Option<PathBuf>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if has_null_byte(value) {
        return Err(format!("{label} must not contain null bytes"));
    }
    if !value.is_absolute() {
        return Err(format!("{label} must be absolute: {}", value.display()));
    }
    Ok(Some(resolve_path(value)))
}

fn normalize_optional_non_empty(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The channel a version names, falling back to stable
/// (parity: `defaultChannelForVersion`).
pub fn default_channel_for_version(version: &str) -> ReleaseChannel {
    release_channel_from_version(Some(version))
        .unwrap_or_else(|| release_channel_from_name("stable").expect("stable is a channel"))
}

fn default_poll_interval_ms(channel: &ReleaseChannel) -> u64 {
    if channel.as_str() == "stable" {
        STABLE_POLL_INTERVAL_MS
    } else {
        BETA_POLL_INTERVAL_MS
    }
}

fn default_metadata_url(channel: &ReleaseChannel) -> String {
    format!(
        "{DEFAULT_RELEASE_ORIGIN}/{}/latest/metadata.json",
        channel.as_str()
    )
}

/// Resolve the updater configuration, with the environment winning over the
/// input (parity: `resolveDesktopUpdaterConfig`).
pub fn resolve_config(input: &UpdaterConfigInput) -> Result<UpdaterConfig, String> {
    let env = &input.env;

    let mode = match env_value(env, env_keys::MODE) {
        Some(value) => UpdateMode::from_name(value)
            .ok_or_else(|| format!("unsupported desktop update mode: {value}"))?,
        None => input.mode.unwrap_or(UpdateMode::PackageLauncher),
    };

    let enabled = truthy_env(env, env_keys::ENABLED)?.unwrap_or(input.packaged);

    let runtime_base = resolve_path(
        input
            .runtime_base
            .as_deref()
            .unwrap_or_else(|| Path::new(".")),
    );

    let download_root = match env_value(env, env_keys::DOWNLOAD_ROOT) {
        Some(value) => normalize_download_root(Path::new(value))?,
        None => normalize_download_root(
            &input
                .download_root
                .clone()
                .unwrap_or_else(|| runtime_base.join("updates")),
        )?,
    };

    let current_version = env_value(env, env_keys::CURRENT_VERSION)
        .map(str::to_string)
        .or_else(|| input.current_version.clone())
        .or_else(|| input.app_version.clone())
        .unwrap_or_else(|| "0.0.0".to_string());

    let channel = match env_value(env, env_keys::CHANNEL) {
        Some(value) if is_release_channel(value) => {
            release_channel_from_name(value).expect("checked as a channel")
        }
        Some(value) => return Err(format!("unsupported desktop update channel: {value}")),
        None => default_channel_for_version(&current_version),
    };

    let installed_version_override =
        normalize_optional_non_empty(env_value(env, env_keys::INSTALLED_VERSION));

    let installer_observation_root = normalize_optional_root(
        input.installer_observation_root.as_deref(),
        "installer observation root",
    )?;
    let launcher_launch_path = input.launcher_launch_path.clone();
    let launcher_root = normalize_optional_root(input.launcher_root.as_deref(), "launcher root")?;
    let launcher_payload_extractor_path = normalize_optional_root(
        input.launcher_payload_extractor_path.as_deref(),
        "launcher payload extractor path",
    )?;
    let launcher_runtime_path = normalize_optional_root(
        input.launcher_runtime_path.as_deref(),
        "launcher runtime path",
    )?;
    let namespace = normalize_optional_non_empty(input.namespace.as_deref());

    let arch = env_value(env, env_keys::ARCH)
        .map(str::to_string)
        .or_else(|| input.arch.clone())
        .unwrap_or_else(|| node_arch().to_string());

    Ok(UpdaterConfig {
        arch,
        auto_check: truthy_env(env, env_keys::AUTO_CHECK)?.unwrap_or(enabled),
        auto_download: truthy_env(env, env_keys::AUTO_DOWNLOAD)?.unwrap_or(true),
        auto_open: truthy_env(env, env_keys::AUTO_OPEN)?.unwrap_or(false),
        check_backoff_initial_ms: positive_duration_env(
            env,
            env_keys::CHECK_BACKOFF_INITIAL_MS,
            DEFAULT_POLL_BACKOFF_INITIAL_MS,
        )?,
        check_backoff_max_ms: positive_duration_env(
            env,
            env_keys::CHECK_BACKOFF_MAX_MS,
            DEFAULT_POLL_BACKOFF_MAX_MS,
        )?,
        check_initial_delay_ms: duration_env(
            env,
            env_keys::CHECK_INITIAL_DELAY_MS,
            DEFAULT_POLL_INITIAL_DELAY_MS,
        )?,
        check_interval_ms: positive_duration_env(
            env,
            env_keys::CHECK_INTERVAL_MS,
            default_poll_interval_ms(&channel),
        )?,
        channel: channel.clone(),
        current_version,
        download_root,
        enabled,
        installed_version_override,
        installer_observation_root,
        launcher_launch_path,
        launcher_root,
        launcher_payload_extractor_path,
        launcher_runtime_path,
        metadata_url: env_value(env, env_keys::METADATA_URL)
            .map(str::to_string)
            .unwrap_or_else(|| default_metadata_url(&channel)),
        mode,
        namespace,
        open_dry_run: truthy_env(env, env_keys::OPEN_DRY_RUN)?.unwrap_or(false),
        platform: env_value(env, env_keys::PLATFORM)
            .map(str::to_string)
            .or_else(|| input.platform.clone())
            .unwrap_or_else(|| {
                super::super::bridge::platform_for(std::env::consts::OS).to_string()
            }),
        runtime_base,
    })
}

/// Whether the platform has a package-launcher update path
/// (parity: `isSupportedPackageLauncherPlatform`).
pub fn is_supported_package_launcher_platform(platform: &str) -> bool {
    platform == "darwin" || platform == "win32"
}

/// The `capabilities` block a status snapshot implies
/// (parity: `capabilitiesFor`).
pub fn capabilities_for(
    mode: UpdateMode,
    platform: &str,
    supported: bool,
    artifact_type: Option<&str>,
) -> UpdateCapabilities {
    let package_launcher = mode == UpdateMode::PackageLauncher
        && is_supported_package_launcher_platform(platform)
        && supported;
    let payload_update = artifact_type == Some("payload");
    let has_selected_artifact = artifact_type.is_some_and(|value| !value.is_empty());
    let manual_installer = package_launcher && (!has_selected_artifact || !payload_update);
    UpdateCapabilities {
        can_apply_in_place: package_launcher && payload_update,
        can_download: package_launcher,
        can_open_installer: manual_installer,
        requires_manual_install: manual_installer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn input(pairs: &[(&str, &str)]) -> UpdaterConfigInput {
        UpdaterConfigInput {
            env: env(pairs),
            packaged: true,
            ..Default::default()
        }
    }

    #[test]
    fn defaults_are_the_packaged_beta_lane() {
        let config = resolve_config(&UpdaterConfigInput {
            app_version: Some("1.2.3-beta.4".to_string()),
            packaged: true,
            ..Default::default()
        })
        .expect("config");
        assert_eq!(config.channel.as_str(), "beta");
        assert_eq!(config.current_version, "1.2.3-beta.4");
        assert!(matches!(
            config.platform.as_str(),
            "linux" | "darwin" | "win32"
        ));
        assert!(config.enabled);
        assert!(config.auto_check);
        assert!(config.auto_download);
        assert!(!config.auto_open);
        assert_eq!(config.mode, UpdateMode::PackageLauncher);
        assert_eq!(config.check_interval_ms, BETA_POLL_INTERVAL_MS);
        assert_eq!(
            config.metadata_url,
            "https://releases.open-design.ai/beta/latest/metadata.json"
        );
        assert!(config.download_root.ends_with("updates"));
    }

    #[test]
    fn a_stable_build_polls_six_hourly_and_an_unpackaged_build_stays_off() {
        let config = resolve_config(&UpdaterConfigInput {
            app_version: Some("1.2.3".to_string()),
            packaged: false,
            ..Default::default()
        })
        .expect("config");
        assert_eq!(config.channel.as_str(), "stable");
        assert!(!config.enabled);
        assert!(!config.auto_check);
        assert_eq!(config.check_interval_ms, STABLE_POLL_INTERVAL_MS);

        // A version that names no channel still lands on stable.
        let bare = resolve_config(&UpdaterConfigInput {
            app_version: Some("totally-unparsable".to_string()),
            packaged: true,
            ..Default::default()
        })
        .expect("config");
        assert_eq!(bare.channel.as_str(), "stable");
    }

    #[test]
    fn the_environment_overrides_every_knob() {
        let config = resolve_config(&input(&[
            (env_keys::ARCH, "arm64"),
            (env_keys::AUTO_CHECK, "0"),
            (env_keys::AUTO_DOWNLOAD, "no"),
            (env_keys::AUTO_OPEN, "yes"),
            (env_keys::CHANNEL, "prerelease"),
            (env_keys::CURRENT_VERSION, "2.0.0-prerelease.1"),
            (env_keys::ENABLED, "0"),
            (env_keys::INSTALLED_VERSION, " 1.0.0 "),
            (env_keys::METADATA_URL, "https://example.test/feed.json"),
            (env_keys::OPEN_DRY_RUN, "1"),
            (env_keys::PLATFORM, "darwin"),
            (env_keys::CHECK_INTERVAL_MS, "9000"),
        ]))
        .expect("config");
        assert_eq!(config.arch, "arm64");
        assert!(!config.auto_check);
        assert!(!config.auto_download);
        assert!(config.auto_open);
        assert_eq!(config.channel.as_str(), "prerelease");
        assert_eq!(config.current_version, "2.0.0-prerelease.1");
        assert!(!config.enabled);
        assert_eq!(config.installed_version_override.as_deref(), Some("1.0.0"));
        assert_eq!(config.metadata_url, "https://example.test/feed.json");
        assert!(config.open_dry_run);
        assert_eq!(config.platform, "darwin");
        assert_eq!(config.check_interval_ms, 9000);
    }

    #[test]
    fn bad_values_are_refused_instead_of_defaulted() {
        for (key, value, expected) in [
            (env_keys::ENABLED, "maybe", "boolean env value"),
            (
                env_keys::MODE,
                "hot-patch",
                "unsupported desktop update mode",
            ),
            (
                env_keys::CHANNEL,
                "Moon Shot",
                "unsupported desktop update channel",
            ),
            (env_keys::CHECK_INTERVAL_MS, "0", "greater than 0"),
            (env_keys::CHECK_INITIAL_DELAY_MS, "-1", "non-negative"),
            (env_keys::CHECK_INITIAL_DELAY_MS, "soon", "non-negative"),
        ] {
            let error = resolve_config(&input(&[(key, value)])).expect_err(key);
            assert!(
                error.contains(expected),
                "{key}={value} should mention {expected:?}, got {error:?}"
            );
        }
        let relative = resolve_config(&input(&[(env_keys::DOWNLOAD_ROOT, "relative/dir")]))
            .expect_err("relative root");
        assert!(relative.contains("must be absolute"), "{relative}");
    }

    #[test]
    fn capabilities_follow_the_platform_and_the_artifact() {
        let dmg = capabilities_for(UpdateMode::PackageLauncher, "darwin", true, Some("dmg"));
        assert!(dmg.can_download);
        assert!(!dmg.can_apply_in_place);
        assert!(dmg.requires_manual_install);

        let payload =
            capabilities_for(UpdateMode::PackageLauncher, "darwin", true, Some("payload"));
        assert!(payload.can_apply_in_place);
        assert!(!payload.requires_manual_install);

        let linux = capabilities_for(UpdateMode::PackageLauncher, "linux", true, Some("dmg"));
        assert!(!linux.can_download);
        assert!(!linux.can_open_installer);

        let unsupported = capabilities_for(UpdateMode::PackageLauncher, "darwin", false, None);
        assert!(!unsupported.can_download);

        let incremental =
            capabilities_for(UpdateMode::JsIncremental, "darwin", true, Some("payload"));
        assert!(!incremental.can_download);
    }

    #[test]
    fn paths_resolve_lexically_without_touching_disk() {
        let resolved = resolve_path(Path::new("./a/./b/../c"));
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("a/c"), "{resolved:?}");
    }
}
