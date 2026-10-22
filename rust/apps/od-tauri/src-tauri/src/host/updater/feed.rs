//! Release-feed interpretation (parity: `updater/feed.ts`).
//!
//! Metadata parsing, per-channel version resolution, and artifact selection
//! with the payload-to-installer fallback — the decisions that turn a feed
//! document into "here is the release this build could install", or into the
//! reason it cannot.
//!
//! The feed is the shared contract between the Electron host and this shell;
//! the two differ only in what they ask it for. The Electron host asks for a
//! launcher payload when it has a usable launcher runtime and an installer
//! otherwise, and this shell always asks for the installer artifact: it ships
//! one self-contained bundle with no launcher payload runtime, so replacing
//! the package is its only update mechanism.

use std::cmp::Ordering;

use od_core::release::{is_release_channel, release_channel_from_name, ReleaseChannel};
use serde_json::{Map, Value};

use super::config::UpdaterConfig;
use super::model::{ArtifactSnapshot, ChecksumAlgorithm, ChecksumSnapshot, UpdateState};
use super::support::{number_field, object_field, string_field, UpdateError};

/// A release this build can download (parity: `UpdateCandidate`).
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateCandidate {
    pub arch: String,
    pub artifact: ArtifactSnapshot,
    pub checksum: ChecksumSnapshot,
    pub channel: ReleaseChannel,
    pub metadata: Map<String, Value>,
    pub platform_key: String,
    pub version: String,
}

impl UpdateCandidate {
    /// The artifact's declared type, or the snapshot's fallback spelling.
    pub fn artifact_type_name(&self) -> &str {
        self.artifact.artifact_type.as_deref().unwrap_or("artifact")
    }
}

/// Why a candidate could not be selected (parity: the `ok: false` arm).
///
/// The state is part of the refusal because it is an answer: an unpublished
/// platform for a build that has the mechanism is a configuration mistake
/// (`error`), while a platform with no package-launcher path at all is simply
/// `unsupported`.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRejection {
    pub error: UpdateError,
    pub state: UpdateState,
}

/// The artifact key/type a feed is asked for (parity: the
/// `selectedPackageLauncherArtifact` return value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSelection {
    pub artifact_key: &'static str,
    pub artifact_type: &'static str,
    pub description: &'static str,
    pub platform_key: String,
}

/// A path segment safe for a file name (parity: `sanitizePathSegment`).
///
/// Every run of unsafe characters becomes one `-`, and the result never starts
/// or ends with one; nothing left means the caller gets `update` rather than an
/// empty segment.
pub fn sanitize_path_segment(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    let mut inserted_dash = false;
    for character in value.chars() {
        let safe = character.is_ascii_alphanumeric()
            || character == '.'
            || character == '_'
            || character == '-';
        if safe {
            sanitized.push(character);
            inserted_dash = false;
        } else if !inserted_dash {
            sanitized.push('-');
            inserted_dash = true;
        }
    }
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "update".to_string()
    } else {
        trimmed.to_string()
    }
}

struct ComparableVersion {
    nums: [u64; 3],
    pre: Vec<String>,
}

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn number_part(value: Option<&&str>) -> u64 {
    value
        .filter(|value| is_digits(value))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// A version in the shape the release protocol compares
/// (parity: `parseComparableLauncherVersion`).
fn parse_comparable_version(value: &str) -> ComparableVersion {
    let trimmed = value.trim();
    let without_prefix = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    let cleaned = without_prefix.split('+').next().unwrap_or("");

    // `x.y.z.nightly.N` is the nightly shape, ordered as a prerelease of that
    // triple rather than as a fourth numeric field.
    let parts: Vec<&str> = cleaned.split('.').collect();
    if parts.len() == 5
        && parts[3].eq_ignore_ascii_case("nightly")
        && [0, 1, 2, 4].iter().all(|index| is_digits(parts[*index]))
    {
        return ComparableVersion {
            nums: [
                number_part(Some(&parts[0])),
                number_part(Some(&parts[1])),
                number_part(Some(&parts[2])),
            ],
            pre: vec!["nightly".to_string(), parts[4].to_string()],
        };
    }

    let (core, prerelease) = match cleaned.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (cleaned, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    ComparableVersion {
        nums: [
            number_part(numbers.first()),
            number_part(numbers.get(1)),
            number_part(numbers.get(2)),
        ],
        pre: match prerelease {
            Some(value) if !value.is_empty() => value.split('.').map(str::to_string).collect(),
            _ => Vec::new(),
        },
    }
}

fn compare_identifier(left: &str, right: &str) -> Ordering {
    let left_number = is_digits(left).then(|| left.parse::<u64>().unwrap_or(0));
    let right_number = is_digits(right).then(|| right.parse::<u64>().unwrap_or(0));
    match (left_number, right_number) {
        (Some(left), Some(right)) => left.cmp(&right),
        // Numeric identifiers always sort before alphanumeric ones.
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// The order of two release versions (parity: `compareLauncherVersions`).
///
/// Base triple first, then SemVer-style prerelease ordering: a release without
/// a prerelease is newer than one with it, numeric identifiers compare
/// numerically, and a shorter identifier list is older.
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let left = parse_comparable_version(left);
    let right = parse_comparable_version(right);
    for index in 0..3 {
        match left.nums[index].cmp(&right.nums[index]) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    match (left.pre.is_empty(), right.pre.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            for index in 0..left.pre.len().max(right.pre.len()) {
                match (left.pre.get(index), right.pre.get(index)) {
                    (None, _) => return Ordering::Less,
                    (_, None) => return Ordering::Greater,
                    (Some(left), Some(right)) => match compare_identifier(left, right) {
                        Ordering::Equal => continue,
                        other => return other,
                    },
                }
            }
            Ordering::Equal
        }
    }
}

/// The channel a feed claims (parity: `metadataChannel`).
pub fn metadata_channel(metadata: &Map<String, Value>) -> Option<ReleaseChannel> {
    let channel = string_field(metadata, "channel")?;
    is_release_channel(&channel)
        .then(|| release_channel_from_name(&channel))
        .flatten()
}

/// The version a feed offers for a channel (parity:
/// `releaseVersionForChannel`).
///
/// `releaseVersion` is the canonical field; the per-channel field is the older
/// spelling the feeds still carry, and a feed that sets only one of them is
/// still a feed.
pub fn release_version_for_channel(
    metadata: &Map<String, Value>,
    channel: &ReleaseChannel,
) -> Option<String> {
    let canonical = string_field(metadata, "releaseVersion");
    let fallback = match channel.as_str() {
        "beta" => string_field(metadata, "betaVersion"),
        "prerelease" => string_field(metadata, "prereleaseVersion"),
        "stable" => string_field(metadata, "stableVersion"),
        _ => None,
    };
    canonical.or(fallback)
}

/// The feed's platform key for a macOS build (parity:
/// `selectedMacPlatformKey`).
pub fn selected_mac_platform_key(arch: &str) -> String {
    if arch == "x64" {
        "macIntel".to_string()
    } else {
        "mac".to_string()
    }
}

/// The feed's platform key for a Windows build (parity:
/// `selectedWinPlatformKey`).
pub fn selected_win_platform_key(arch: &str) -> String {
    match arch {
        "x64" => "win".to_string(),
        "arm64" => "winArm64".to_string(),
        "ia32" => "winIa32".to_string(),
        other => format!("win-{}", sanitize_path_segment(other)),
    }
}

/// The artifact a package-launcher update asks a feed for (parity:
/// `selectedPackageLauncherArtifact`).
///
/// `None` means the platform has no package-launcher update path at all:
/// Linux ships as a distribution package, where the platform's own package
/// manager owns upgrades, and inventing a self-updater there would fight it.
pub fn selected_package_launcher_artifact(
    platform: &str,
    arch: &str,
    prefer_payload: bool,
) -> Option<ArtifactSelection> {
    match platform {
        "darwin" => {
            let platform_key = selected_mac_platform_key(arch);
            Some(if prefer_payload {
                ArtifactSelection {
                    artifact_key: "payload",
                    artifact_type: "payload",
                    description: "mac launcher payload",
                    platform_key,
                }
            } else {
                ArtifactSelection {
                    artifact_key: "dmg",
                    artifact_type: "dmg",
                    description: "mac DMG",
                    platform_key,
                }
            })
        }
        "win32" => {
            let platform_key = selected_win_platform_key(arch);
            Some(if prefer_payload {
                ArtifactSelection {
                    artifact_key: "payload",
                    artifact_type: "payload",
                    description: "Windows launcher payload",
                    platform_key,
                }
            } else {
                ArtifactSelection {
                    artifact_key: "installer",
                    artifact_type: "installer",
                    description: "Windows installer",
                    platform_key,
                }
            })
        }
        _ => None,
    }
}

fn reject(state: UpdateState, code: &str, message: impl Into<String>) -> CandidateRejection {
    CandidateRejection {
        error: UpdateError::new(code, message),
        state,
    }
}

/// Select the release a feed offers this build (parity:
/// `selectUpdateCandidate`).
///
/// Every refusal names its reason, because the status snapshot shows it: a
/// feed that published a stable release while this build watches beta is an
/// error a person can act on; an artifact the feed never published is the same
/// answer but a different cause.
pub fn select_update_candidate(
    metadata: &Map<String, Value>,
    config: &UpdaterConfig,
    prefer_payload: bool,
) -> Result<UpdateCandidate, CandidateRejection> {
    // The mode check runs before anything else: an unimplemented mechanism is
    // not a feed problem, and fetching a feed to report that would be noise.
    if config.mode == super::config::UpdateMode::JsIncremental {
        return Err(reject(
            UpdateState::Unsupported,
            "update-mode-not-implemented",
            "js-incremental updates are not implemented yet",
        ));
    }

    let Some(selection) =
        selected_package_launcher_artifact(&config.platform, &config.arch, prefer_payload)
    else {
        return Err(reject(
            UpdateState::Unsupported,
            "unsupported-platform",
            "package-launcher updates are currently supported on macOS and Windows only",
        ));
    };

    let Some(channel) = metadata_channel(metadata) else {
        return Err(reject(
            UpdateState::Error,
            "metadata-channel-unsupported",
            "release metadata does not include a supported update channel",
        ));
    };
    if channel != config.channel {
        return Err(reject(
            UpdateState::Error,
            "metadata-channel-mismatch",
            format!(
                "release metadata channel {channel} does not match configured update channel {}",
                config.channel
            ),
        ));
    }

    let Some(platforms) = object_field(metadata, "platforms") else {
        return Err(reject(
            UpdateState::Error,
            "metadata-missing-platforms",
            "release metadata does not include platform artifacts",
        ));
    };
    let platform_key = selection.platform_key;
    let enabled = object_field(platforms, &platform_key)
        .filter(|platform| platform.get("enabled").and_then(Value::as_bool) == Some(true));
    let Some(platform) = enabled else {
        return Err(reject(
            UpdateState::Error,
            "no-compatible-artifact",
            format!("release metadata does not include an enabled {platform_key} artifact"),
        ));
    };

    let Some(version) = release_version_for_channel(metadata, &config.channel) else {
        return Err(reject(
            UpdateState::Error,
            "metadata-missing-version",
            format!(
                "release metadata does not include a {} update version",
                config.channel
            ),
        ));
    };

    let artifact_record = object_field(platform, "artifacts")
        .and_then(|artifacts| object_field(artifacts, selection.artifact_key));
    let url = artifact_record.and_then(|record| string_field(record, "url"));
    let Some(artifact_record) = artifact_record.filter(|_| url.is_some()) else {
        return Err(reject(
            UpdateState::Error,
            "no-compatible-artifact",
            format!(
                "release metadata does not include a {} artifact for {platform_key}",
                selection.description
            ),
        ));
    };
    let url = url.expect("filtered on a present url");

    let artifact = ArtifactSnapshot {
        name: string_field(artifact_record, "name"),
        platform_key: Some(platform_key.clone()),
        size: number_field(artifact_record, "size").map(|size| size.max(0.0) as u64),
        artifact_type: Some(selection.artifact_type.to_string()),
        url,
    };

    // sha512 outranks sha256 when a feed publishes both: the stronger digest
    // is the one the publisher intended to be checked.
    let sha256 = string_field(artifact_record, "sha256")
        .or_else(|| string_field(artifact_record, "sha256Digest"));
    let sha512 = string_field(artifact_record, "sha512")
        .or_else(|| string_field(artifact_record, "sha512Digest"));
    let checksum = match sha512 {
        Some(value) => ChecksumSnapshot {
            algorithm: ChecksumAlgorithm::Sha512,
            url: None,
            value: Some(value),
        },
        None => ChecksumSnapshot {
            algorithm: ChecksumAlgorithm::Sha256,
            url: string_field(artifact_record, "sha256Url"),
            value: sha256,
        },
    };

    Ok(UpdateCandidate {
        arch: string_field(platform, "arch").unwrap_or_else(|| config.arch.clone()),
        artifact,
        checksum,
        channel: config.channel.clone(),
        metadata: metadata.clone(),
        platform_key,
        version,
    })
}

/// Select a candidate, preferring the launcher payload and falling back to the
/// installer when the feed has none (parity:
/// `selectUpdateCandidateWithFallback`).
///
/// Only "no such artifact" falls back. Every other refusal is about the
/// release or the build, not the mechanism, so retrying the installer would
/// report a different error for the same cause.
pub fn select_update_candidate_with_fallback(
    metadata: &Map<String, Value>,
    config: &UpdaterConfig,
    prefer_payload: bool,
) -> Result<UpdateCandidate, CandidateRejection> {
    if !prefer_payload {
        return select_update_candidate(metadata, config, false);
    }
    match select_update_candidate(metadata, config, true) {
        Ok(candidate) => Ok(candidate),
        Err(rejection) if rejection.error.code == "no-compatible-artifact" => {
            select_update_candidate(metadata, config, false)
        }
        Err(rejection) => Err(rejection),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::updater::config::{resolve_config, UpdaterConfigInput};
    use std::collections::HashMap;

    fn config(platform: &str, arch: &str, channel: &str) -> UpdaterConfig {
        let env: HashMap<String, String> = [
            ("OD_UPDATE_PLATFORM".to_string(), platform.to_string()),
            ("OD_UPDATE_ARCH".to_string(), arch.to_string()),
            ("OD_UPDATE_CHANNEL".to_string(), channel.to_string()),
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

    fn feed() -> Map<String, Value> {
        serde_json::json!({
            "channel": "beta",
            "releaseVersion": "1.2.3-beta.4",
            "platforms": {
                "mac": {
                    "arch": "arm64",
                    "enabled": true,
                    "artifacts": {
                        "dmg": {
                            "url": "https://example.test/OpenDesign-1.2.3-beta.4.dmg",
                            "name": "OpenDesign-1.2.3-beta.4.dmg",
                            "size": 120,
                            "sha256": "a".repeat(64),
                        },
                        "payload": {
                            "url": "https://example.test/OpenDesign-1.2.3-beta.4.zip",
                            "name": "OpenDesign-1.2.3-beta.4.zip",
                        },
                    },
                },
                "win": {
                    "arch": "x64",
                    "enabled": true,
                    "artifacts": {
                        "installer": {
                            "url": "https://example.test/OpenDesign-Setup-1.2.3-beta.4.exe",
                            "sha512": "b".repeat(128),
                            "sha256Url": "https://example.test/sha256.txt",
                        },
                    },
                },
                "macIntel": {"enabled": false},
            },
        })
        .as_object()
        .expect("object")
        .clone()
    }

    #[test]
    fn a_mac_feed_selects_the_dmg_and_its_checksum() {
        let candidate = select_update_candidate(&feed(), &config("darwin", "arm64", "beta"), false)
            .expect("candidate");
        assert_eq!(candidate.version, "1.2.3-beta.4");
        assert_eq!(candidate.platform_key, "mac");
        assert_eq!(candidate.arch, "arm64");
        assert_eq!(candidate.artifact_type_name(), "dmg");
        assert_eq!(candidate.artifact.size, Some(120));
        assert_eq!(
            candidate.artifact.name.as_deref(),
            Some("OpenDesign-1.2.3-beta.4.dmg")
        );
        assert_eq!(candidate.checksum.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(
            candidate.checksum.value.as_deref(),
            Some("a".repeat(64).as_str())
        );
        assert_eq!(candidate.channel.as_str(), "beta");

        // Intel asks for its own platform key, which this feed left disabled.
        let intel = select_update_candidate(&feed(), &config("darwin", "x64", "beta"), false)
            .expect_err("macIntel is disabled");
        assert_eq!(intel.error.code, "no-compatible-artifact");
        assert_eq!(intel.state, UpdateState::Error);
        assert!(intel.error.message.contains("macIntel"));
    }

    #[test]
    fn a_windows_feed_selects_the_installer_and_prefers_sha512() {
        let candidate = select_update_candidate(&feed(), &config("win32", "x64", "beta"), false)
            .expect("candidate");
        assert_eq!(candidate.platform_key, "win");
        assert_eq!(candidate.artifact_type_name(), "installer");
        assert_eq!(candidate.checksum.algorithm, ChecksumAlgorithm::Sha512);
        assert_eq!(
            candidate.checksum.value.as_deref(),
            Some("b".repeat(128).as_str())
        );
        assert_eq!(
            candidate.checksum.url, None,
            "a digest needs no checksum URL"
        );
    }

    #[test]
    fn the_payload_fallback_lands_on_the_installer_when_the_feed_has_no_payload() {
        let windows =
            select_update_candidate_with_fallback(&feed(), &config("win32", "x64", "beta"), true)
                .expect("fallback");
        assert_eq!(windows.artifact_type_name(), "installer");

        let mac = select_update_candidate_with_fallback(
            &feed(),
            &config("darwin", "arm64", "beta"),
            true,
        )
        .expect("payload");
        assert_eq!(mac.artifact_type_name(), "payload");
    }

    #[test]
    fn refusals_carry_the_state_and_the_reason() {
        let linux = select_update_candidate(&feed(), &config("linux", "x64", "beta"), false)
            .expect_err("linux");
        assert_eq!(linux.state, UpdateState::Unsupported);
        assert_eq!(linux.error.code, "unsupported-platform");

        let stable = select_update_candidate(&feed(), &config("darwin", "arm64", "stable"), false)
            .expect_err("channel mismatch");
        assert_eq!(stable.error.code, "metadata-channel-mismatch");
        assert!(stable.error.message.contains("beta"));
        assert!(stable.error.message.contains("stable"));

        let mut wrong_channel = feed();
        // A name no release channel can own (spaces are not a channel spelling).
        wrong_channel.insert("channel".into(), serde_json::json!("Moon Shot"));
        assert_eq!(
            select_update_candidate(&wrong_channel, &config("darwin", "arm64", "beta"), false)
                .expect_err("unsupported channel")
                .error
                .code,
            "metadata-channel-unsupported"
        );

        let mut no_platforms = feed();
        no_platforms.remove("platforms");
        assert_eq!(
            select_update_candidate(&no_platforms, &config("darwin", "arm64", "beta"), false)
                .expect_err("no platforms")
                .error
                .code,
            "metadata-missing-platforms"
        );

        let mut no_version = feed();
        no_version.remove("releaseVersion");
        assert_eq!(
            select_update_candidate(&no_version, &config("darwin", "arm64", "beta"), false)
                .expect_err("no version")
                .error
                .code,
            "metadata-missing-version"
        );

        let mut no_artifact = feed();
        no_artifact["platforms"]["mac"]["artifacts"]
            .as_object_mut()
            .expect("artifacts")
            .remove("dmg");
        assert_eq!(
            select_update_candidate(&no_artifact, &config("darwin", "arm64", "beta"), false)
                .expect_err("no dmg")
                .error
                .code,
            "no-compatible-artifact"
        );

        let mut empty_url = feed();
        empty_url["platforms"]["mac"]["artifacts"]["dmg"]["url"] = serde_json::json!("");
        assert_eq!(
            select_update_candidate(&empty_url, &config("darwin", "arm64", "beta"), false)
                .expect_err("empty url")
                .error
                .code,
            "no-compatible-artifact"
        );
    }

    #[test]
    fn an_incremental_mode_is_refused_before_the_feed_is_read() {
        let mut incremental = config("darwin", "arm64", "beta");
        incremental.mode = super::super::config::UpdateMode::JsIncremental;
        let rejection = select_update_candidate(&feed(), &incremental, false).expect_err("mode");
        assert_eq!(rejection.state, UpdateState::Unsupported);
        assert_eq!(rejection.error.code, "update-mode-not-implemented");
    }

    #[test]
    fn the_older_per_channel_version_fields_still_resolve() {
        let mut metadata = feed();
        metadata.remove("releaseVersion");
        metadata.insert("betaVersion".into(), serde_json::json!("9.9.9-beta.1"));
        let candidate =
            select_update_candidate(&metadata, &config("darwin", "arm64", "beta"), false)
                .expect("candidate");
        assert_eq!(candidate.version, "9.9.9-beta.1");
    }

    #[test]
    fn versions_order_by_triple_then_prerelease() {
        assert_eq!(compare_versions("1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(compare_versions("1.2.4", "1.2.3"), Ordering::Greater);
        assert_eq!(compare_versions("1.2.3", "1.10.0"), Ordering::Less);
        assert_eq!(compare_versions("v1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(compare_versions("1.2.3+build.9", "1.2.3"), Ordering::Equal);
        assert_eq!(compare_versions("1.2.3-beta.4", "1.2.3"), Ordering::Less);
        assert_eq!(compare_versions("1.2.3", "1.2.3-beta.4"), Ordering::Greater);
        assert_eq!(
            compare_versions("1.2.3-beta.10", "1.2.3-beta.9"),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.2.3-beta", "1.2.3-beta.1"),
            Ordering::Less
        );
        assert_eq!(
            compare_versions("1.2.3-alpha", "1.2.3-beta"),
            Ordering::Less
        );
        // The nightly shape is a prerelease of its triple, not a fourth number.
        assert_eq!(compare_versions("1.2.3.nightly.7", "1.2.3"), Ordering::Less);
        assert_eq!(
            compare_versions("1.2.3.nightly.7", "1.2.3.nightly.8"),
            Ordering::Less
        );
        // An unparsable version has no base triple, so it never looks newer.
        assert_eq!(compare_versions("garbage", "0.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("garbage", "1.0.0"), Ordering::Less);
    }

    #[test]
    fn platform_keys_match_the_feeds_spelling() {
        assert_eq!(selected_mac_platform_key("x64"), "macIntel");
        assert_eq!(selected_mac_platform_key("arm64"), "mac");
        assert_eq!(selected_win_platform_key("x64"), "win");
        assert_eq!(selected_win_platform_key("arm64"), "winArm64");
        assert_eq!(selected_win_platform_key("ia32"), "winIa32");
        assert_eq!(selected_win_platform_key("riscv 64"), "win-riscv-64");
        assert!(selected_package_launcher_artifact("linux", "x64", false).is_none());
        assert_eq!(
            selected_package_launcher_artifact("darwin", "arm64", false)
                .expect("mac")
                .artifact_key,
            "dmg"
        );
    }

    #[test]
    fn path_segments_are_sanitized_for_file_names() {
        assert_eq!(
            sanitize_path_segment("1.2.3-beta.4+build/7"),
            "1.2.3-beta.4-build-7"
        );
        assert_eq!(sanitize_path_segment("--weird--"), "weird");
        assert_eq!(sanitize_path_segment("///"), "update");
        assert_eq!(sanitize_path_segment("a  b"), "a-b");
    }
}
