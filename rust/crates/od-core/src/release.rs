//! Release channel identity, version parsing, and namespace layout — the Rust
//! port of `packages/release/src/index.ts`.
//!
//! The TypeScript package is the parity reference and it owns exactly one
//! thing: the release domain. Channel names, version parsing and formatting,
//! storage prefixes, release namespaces, and app identity data live here —
//! and nothing else. Like the package it ports, this module reads no files,
//! calls no GitHub/R2, spawns no build tools, and owns no workflow execution.
//!
//! The shell needs this module to answer two questions at startup: *which
//! lane is this build?* (the release channel) and *where does this build's
//! data live?* (the release namespace).

use std::cmp::Ordering;
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;

/// The product name every channel descriptor is built from (parity:
/// `PRODUCT_NAME`).
pub const PRODUCT_NAME: &str = "Open Design";

/// The namespace a build with no release channel uses (parity:
/// `DEFAULT_NAMESPACE`).
pub const DEFAULT_NAMESPACE: &str = "open-design";

/// The namespace the sidecar protocol uses when nothing else names one
/// (parity: `SIDECAR_DEFAULTS.namespace`).
pub const DEFAULT_SIDECAR_NAMESPACE: &str = "default";

/// The stable lane (parity: `RELEASE_CHANNELS.STABLE`).
pub const CHANNEL_STABLE: &str = "stable";

/// The prerelease lane (parity: `RELEASE_CHANNELS.PRERELEASE`).
pub const CHANNEL_PRERELEASE: &str = "prerelease";

/// The beta lane (parity: `RELEASE_CHANNELS.BETA`).
pub const CHANNEL_BETA: &str = "beta";

/// Names no release may claim (parity: `RESERVED_RELEASE_NAMES`).
pub const RESERVED_RELEASE_NAMES: [&str; 1] = ["local"];

/// The platform a release is published for (parity: `ReleasePlatform`).
///
/// The distinction that matters is macOS on Intel: it gets its own namespace
/// suffix, so an Intel build never writes into an arm64 build's data root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasePlatform {
    Mac,
    MacIntel,
    Win,
    Linux,
}

impl ReleasePlatform {
    /// The namespace suffix of this platform (parity:
    /// `RELEASE_PLATFORM_NAMESPACE_SUFFIXES`).
    pub fn namespace_suffix(self) -> &'static str {
        match self {
            ReleasePlatform::Mac => "",
            ReleasePlatform::MacIntel => "intel",
            ReleasePlatform::Win => "win",
            ReleasePlatform::Linux => "linux",
        }
    }

    /// The platform this build runs on.
    ///
    /// macOS on Intel is the only platform the runtime can tell apart from
    /// macOS on arm64, and only because the architectures differ.
    pub fn current() -> Self {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "x86_64") => ReleasePlatform::MacIntel,
            ("macos", _) => ReleasePlatform::Mac,
            ("windows", _) => ReleasePlatform::Win,
            _ => ReleasePlatform::Linux,
        }
    }
}

/// A release channel: the stable lane, the prerelease lane, or an exact
/// internal name (parity: `ReleaseChannel`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReleaseChannel {
    Stable,
    Prerelease,
    Internal(String),
}

impl ReleaseChannel {
    /// The channel's name (parity: the channel string itself).
    pub fn as_str(&self) -> &str {
        match self {
            ReleaseChannel::Stable => CHANNEL_STABLE,
            ReleaseChannel::Prerelease => CHANNEL_PRERELEASE,
            ReleaseChannel::Internal(name) => name,
        }
    }
}

impl fmt::Display for ReleaseChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The identity a release channel gives an installed app (parity:
/// `ReleaseInstallIdentity`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInstallIdentity {
    pub app_id: String,
    pub executable_name: String,
    pub product_name: String,
}

/// Everything a channel says about itself (parity:
/// `ReleaseChannelDescriptor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseChannelDescriptor {
    pub app_id: String,
    pub base_version_field: &'static str,
    pub channel: ReleaseChannel,
    pub counter_field: Option<&'static str>,
    pub display_label: String,
    pub github_release_enabled: bool,
    pub internal: bool,
    pub product_name: String,
    pub release_version_field: &'static str,
    pub storage_prefix: String,
}

/// A release version, counted or stable (parity: `ParsedReleaseVersion`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReleaseVersion {
    pub base_version: String,
    pub channel: ReleaseChannel,
    /// The release counter of a counted version; `None` for the stable lane.
    pub number: Option<u64>,
    pub release_version: String,
}

/// The release domain's failures (parity: the `throw` sites of
/// `packages/release/src/index.ts`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReleaseError {
    #[error("stable release version must be x.y.z; got {value}")]
    StableVersion { value: String },
    #[error("{channel} release version must be x.y.z-{channel}.N; got {value}")]
    CountedVersion { channel: String, value: String },
    #[error("{channel} release number must be a positive integer; got {number}")]
    ReleaseNumber { channel: String, number: u64 },
}

/// Whether the value names a release channel (parity: `isReleaseChannel`).
///
/// A channel is `stable`, `prerelease`, or an exact name of one to twelve
/// lowercase letters or digits. `local` is reserved and never a channel.
pub fn is_release_channel(value: &str) -> bool {
    if RESERVED_RELEASE_NAMES.contains(&value) {
        return false;
    }
    if value == CHANNEL_STABLE || value == CHANNEL_PRERELEASE {
        return true;
    }
    exact_release_name_pattern().is_match(value)
}

/// The channel a name refers to (parity: reading a `ReleaseChannel` value).
///
/// Returns `None` for anything that is not a channel name, so a caller can
/// never build a descriptor for a name no release owns.
pub fn release_channel_from_name(value: &str) -> Option<ReleaseChannel> {
    if !is_release_channel(value) {
        return None;
    }
    match value {
        CHANNEL_STABLE => Some(ReleaseChannel::Stable),
        CHANNEL_PRERELEASE => Some(ReleaseChannel::Prerelease),
        other => Some(ReleaseChannel::Internal(other.to_string())),
    }
}

/// Everything the channel says about itself (parity:
/// `releaseChannelDescriptor`).
pub fn release_channel_descriptor(channel: &ReleaseChannel) -> ReleaseChannelDescriptor {
    match channel {
        ReleaseChannel::Stable => ReleaseChannelDescriptor {
            app_id: "io.open-design.desktop".to_string(),
            base_version_field: "baseVersion",
            channel: channel.clone(),
            counter_field: None,
            display_label: "Stable".to_string(),
            github_release_enabled: true,
            internal: false,
            product_name: PRODUCT_NAME.to_string(),
            release_version_field: "releaseVersion",
            storage_prefix: CHANNEL_STABLE.to_string(),
        },
        ReleaseChannel::Prerelease => ReleaseChannelDescriptor {
            app_id: "io.open-design.desktop.prerelease".to_string(),
            base_version_field: "baseVersion",
            channel: channel.clone(),
            counter_field: Some("releaseNumber"),
            display_label: "Prerelease".to_string(),
            github_release_enabled: false,
            internal: true,
            product_name: format!("{PRODUCT_NAME} Prerelease"),
            release_version_field: "releaseVersion",
            storage_prefix: CHANNEL_PRERELEASE.to_string(),
        },
        ReleaseChannel::Internal(name) => {
            let display_label = capitalize(name);
            ReleaseChannelDescriptor {
                app_id: format!("io.open-design.desktop.{name}"),
                base_version_field: "baseVersion",
                channel: channel.clone(),
                counter_field: Some("releaseNumber"),
                display_label: display_label.clone(),
                github_release_enabled: false,
                internal: true,
                product_name: format!("{PRODUCT_NAME} {display_label}"),
                release_version_field: "releaseVersion",
                storage_prefix: name.clone(),
            }
        }
    }
}

/// The installed identity of a channel (parity: `releaseInstallIdentity`).
pub fn release_install_identity(channel: &ReleaseChannel) -> ReleaseInstallIdentity {
    let descriptor = release_channel_descriptor(channel);
    ReleaseInstallIdentity {
        app_id: descriptor.app_id,
        executable_name: descriptor.product_name.clone(),
        product_name: descriptor.product_name,
    }
}

/// The channel a version belongs to (parity: `releaseChannelFromVersion`).
///
/// A counted version names its channel (`1.2.3-beta.4`); a development
/// version names it in the middle (`1.2.3-beta-internal.4`) and still
/// inherits the matching updater feed. A plain `x.y.z` version names no
/// channel at all — it is the stable lane's own version shape, but a build
/// that carries one is not a published release.
pub fn release_channel_from_version(version: Option<&str>) -> Option<ReleaseChannel> {
    let version = version?;
    if version.is_empty() {
        return None;
    }
    if let Some(captures) = counted_version_pattern().captures(version) {
        if let Some(name) = captures.get(1) {
            if let Some(channel) = release_channel_from_name(name.as_str()) {
                return Some(channel);
            }
        }
    }
    if beta_fallback_pattern().is_match(version) {
        return release_channel_from_name(CHANNEL_BETA);
    }
    if prerelease_fallback_pattern().is_match(version) {
        return release_channel_from_name(CHANNEL_PRERELEASE);
    }
    None
}

/// The channel a namespace belongs to (parity: `releaseChannelFromNamespace`).
///
/// The default namespace is the stable lane; a `release-<channel>` namespace
/// names its channel; anything else names none.
pub fn release_channel_from_namespace(namespace: &str) -> Option<ReleaseChannel> {
    release_channel_from_namespace_or(namespace, DEFAULT_NAMESPACE)
}

/// The channel a namespace belongs to, with an explicit default namespace
/// (parity: `releaseChannelFromNamespace(namespace, defaultNamespace)`).
pub fn release_channel_from_namespace_or(
    namespace: &str,
    default_namespace: &str,
) -> Option<ReleaseChannel> {
    if namespace == default_namespace
        || is_release_channel_namespace(namespace, &ReleaseChannel::Stable)
    {
        return Some(ReleaseChannel::Stable);
    }
    let captures = namespace_pattern().captures(namespace)?;
    let name = captures.get(1)?.as_str();
    release_channel_from_name(name)
}

/// Whether the namespace is the channel's namespace (parity:
/// `isReleaseChannelNamespace`).
pub fn is_release_channel_namespace(namespace: &str, channel: &ReleaseChannel) -> bool {
    let pattern = channel_namespace_pattern(channel);
    pattern.is_match(namespace)
}

/// The namespace a channel's build writes into (parity: `releaseNamespace`).
pub fn release_namespace(channel: &ReleaseChannel, platform: ReleasePlatform) -> String {
    let suffix = platform.namespace_suffix();
    if suffix.is_empty() {
        format!("release-{channel}")
    } else {
        format!("release-{channel}-{suffix}")
    }
}

/// A release version, counted or stable (parity: `parseReleaseVersion`).
pub fn parse_release_version(
    value: &str,
    channel: &ReleaseChannel,
) -> Result<ParsedReleaseVersion, ReleaseError> {
    match channel {
        ReleaseChannel::Stable => {
            if stable_version_pattern().is_match(value) {
                return Ok(ParsedReleaseVersion {
                    base_version: value.to_string(),
                    channel: channel.clone(),
                    number: None,
                    release_version: value.to_string(),
                });
            }
            Err(ReleaseError::StableVersion {
                value: value.to_string(),
            })
        }
        counted => {
            let pattern = counted_channel_version_pattern(counted);
            if let Some(captures) = pattern.captures(value) {
                let base_version = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
                let number = captures
                    .get(2)
                    .and_then(|m| m.as_str().parse::<u64>().ok())
                    .unwrap_or_default();
                if number >= 1 {
                    return Ok(ParsedReleaseVersion {
                        base_version: base_version.to_string(),
                        channel: counted.clone(),
                        number: Some(number),
                        release_version: value.to_string(),
                    });
                }
            }
            Err(ReleaseError::CountedVersion {
                channel: counted.as_str().to_string(),
                value: value.to_string(),
            })
        }
    }
}

/// The counted version of a release (parity: `formatReleaseVersion`).
pub fn format_release_version(
    channel: &str,
    base_version: &str,
    number: u64,
) -> Result<String, ReleaseError> {
    if number < 1 {
        return Err(ReleaseError::ReleaseNumber {
            channel: channel.to_string(),
            number,
        });
    }
    Ok(format!("{base_version}-{channel}.{number}"))
}

/// The `x.y.z` triple of a version (parity: `parseReleaseBaseVersion`).
pub fn parse_release_base_version(value: &str) -> Option<[u64; 3]> {
    let captures = base_version_pattern().captures(value)?;
    let mut parts = [0u64; 3];
    for (index, slot) in parts.iter_mut().enumerate() {
        *slot = captures.get(index + 1)?.as_str().parse().ok()?;
    }
    Some(parts)
}

/// The order of two base versions (parity: `compareReleaseBaseVersions`).
pub fn compare_release_base_versions(left: [u64; 3], right: [u64; 3]) -> Ordering {
    for index in 0..3 {
        match left[index].cmp(&right[index]) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// The first letter of a name, capitalized (parity: the display label of an
/// exact release channel).
fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn exact_release_name_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^[a-z0-9]{1,12}$").expect("exact release name pattern"))
}

fn counted_version_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^\d+\.\d+\.\d+-([a-z0-9]{1,12})\.\d+$")
            .expect("counted release version pattern")
    })
}

fn beta_fallback_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)(?:^|[-.])beta(?:[-.]|$)").expect("beta channel fallback pattern")
    })
}

fn prerelease_fallback_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)(?:^|[-.])prerelease(?:[-.]|$)")
            .expect("prerelease channel fallback pattern")
    })
}

fn namespace_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^release-([a-z0-9]{1,12})(?:$|[-_.])").expect("release namespace pattern")
    })
}

fn channel_namespace_pattern(channel: &ReleaseChannel) -> Regex {
    Regex::new(&format!(
        r"(?i)^release-{}(?:$|[-_.])",
        regex::escape(channel.as_str())
    ))
    .expect("release channel namespace pattern")
}

fn stable_version_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^\d+\.\d+\.\d+$").expect("stable release version pattern"))
}

fn counted_channel_version_pattern(channel: &ReleaseChannel) -> Regex {
    Regex::new(&format!(
        r"^(\d+\.\d+\.\d+)-{}\.(\d+)$",
        regex::escape(channel.as_str())
    ))
    .expect("counted channel release version pattern")
}

fn base_version_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"^(\d+)\.(\d+)\.(\d+)$").expect("release base version pattern"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_counted_release_versions() {
        assert_eq!(
            format_release_version("prerelease", "1.2.3", 4).expect("format"),
            "1.2.3-prerelease.4"
        );
        let parsed = parse_release_version("1.2.3-prerelease.4", &ReleaseChannel::Prerelease)
            .expect("parse");
        assert_eq!(parsed.base_version, "1.2.3");
        assert_eq!(parsed.channel, ReleaseChannel::Prerelease);
        assert_eq!(parsed.number, Some(4));
        assert_eq!(parsed.release_version, "1.2.3-prerelease.4");
    }

    #[test]
    fn parses_and_compares_stable_base_versions() {
        assert_eq!(parse_release_base_version("1.2.3"), Some([1, 2, 3]));
        assert_eq!(parse_release_base_version("1.2"), None);
        assert_eq!(
            compare_release_base_versions([1, 2, 4], [1, 2, 3]),
            Ordering::Greater
        );
        assert_eq!(
            compare_release_base_versions([1, 2, 3], [1, 2, 3]),
            Ordering::Equal
        );
        assert_eq!(
            compare_release_base_versions([1, 2, 3], [1, 3, 0]),
            Ordering::Less
        );
    }

    #[test]
    fn centralizes_release_identity_and_namespace_derivation() {
        let descriptor = release_channel_descriptor(&ReleaseChannel::Prerelease);
        assert_eq!(descriptor.product_name, "Open Design Prerelease");
        assert_eq!(
            release_install_identity(&ReleaseChannel::Prerelease),
            ReleaseInstallIdentity {
                app_id: "io.open-design.desktop.prerelease".to_string(),
                executable_name: "Open Design Prerelease".to_string(),
                product_name: "Open Design Prerelease".to_string(),
            }
        );
        assert_eq!(
            release_namespace(&ReleaseChannel::Prerelease, ReleasePlatform::Mac),
            "release-prerelease"
        );
        assert_eq!(
            release_namespace(&ReleaseChannel::Prerelease, ReleasePlatform::Win),
            "release-prerelease-win"
        );
        assert_eq!(
            release_namespace(&ReleaseChannel::Prerelease, ReleasePlatform::MacIntel),
            "release-prerelease-intel"
        );
        let internal = release_channel_descriptor(&ReleaseChannel::Internal("qa2".to_string()));
        assert_eq!(internal.app_id, "io.open-design.desktop.qa2");
        assert_eq!(
            internal.channel,
            ReleaseChannel::Internal("qa2".to_string())
        );
        assert_eq!(internal.product_name, "Open Design Qa2");
        assert_eq!(internal.storage_prefix, "qa2");
    }

    #[test]
    fn limits_exact_names_to_one_to_twelve_lowercase_letters_or_digits() {
        assert!(is_release_channel("beta"));
        assert!(is_release_channel("qa1234567890"));
        assert!(!is_release_channel("local"));
        assert!(!is_release_channel("qa-2"));
        assert!(!is_release_channel("Beta"));
        assert!(!is_release_channel("qa12345678901"));
    }

    #[test]
    fn infers_release_channels_from_versions_and_namespaces() {
        assert_eq!(
            release_channel_from_version(Some("1.2.3-beta.1")),
            Some(ReleaseChannel::Internal("beta".to_string()))
        );
        assert_eq!(
            release_channel_from_version(Some("1.2.3-beta-internal.1")),
            Some(ReleaseChannel::Internal("beta".to_string()))
        );
        assert_eq!(
            release_channel_from_version(Some("1.2.3-prerelease.1")),
            Some(ReleaseChannel::Prerelease)
        );
        assert_eq!(
            release_channel_from_namespace("release-preview-linux"),
            Some(ReleaseChannel::Internal("preview".to_string()))
        );
        assert_eq!(
            release_channel_from_namespace("open-design"),
            Some(ReleaseChannel::Stable)
        );
        assert_eq!(release_channel_from_namespace("beta-local-flow"), None);
        assert_eq!(release_channel_from_namespace("release-local"), None);
    }

    #[test]
    fn a_plain_version_names_no_channel() {
        // A plain `x.y.z` is the stable lane's version shape, but a build
        // that carries one is not a published release: its namespace is the
        // default one, not a release namespace.
        assert_eq!(release_channel_from_version(Some("1.2.3")), None);
        assert_eq!(release_channel_from_version(Some("")), None);
        assert_eq!(release_channel_from_version(None), None);
        // ... and it parses as a stable release version.
        let parsed =
            parse_release_version("1.2.3", &ReleaseChannel::Stable).expect("stable version");
        assert_eq!(parsed.base_version, "1.2.3");
        assert_eq!(parsed.number, None);
    }

    #[test]
    fn a_stable_version_must_be_a_triple() {
        assert!(parse_release_version("1.2", &ReleaseChannel::Stable).is_err());
        assert!(parse_release_version("1.2.3-beta.1", &ReleaseChannel::Stable).is_err());
        assert!(parse_release_version("1.2.3-beta.1", &ReleaseChannel::Prerelease).is_err());
        assert!(parse_release_version("1.2.3-prerelease.0", &ReleaseChannel::Prerelease).is_err());
    }

    #[test]
    fn a_release_number_is_positive() {
        assert!(format_release_version("prerelease", "1.2.3", 0).is_err());
        assert_eq!(
            format_release_version("prerelease", "1.2.3", 1).expect("format"),
            "1.2.3-prerelease.1"
        );
    }

    #[test]
    fn the_channel_namespace_is_case_insensitive() {
        assert!(is_release_channel_namespace(
            "release-stable-linux",
            &ReleaseChannel::Stable
        ));
        assert!(is_release_channel_namespace(
            "RELEASE-STABLE",
            &ReleaseChannel::Stable
        ));
        assert!(!is_release_channel_namespace(
            "release-prerelease",
            &ReleaseChannel::Stable
        ));
    }
}
