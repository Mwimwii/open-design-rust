//! The updater's wire model (parity: the `DesktopUpdate*` snapshot types in
//! `packages/sidecar-proto/src/index.ts`).
//!
//! These are the shapes the renderer reads, so they are defined once here and
//! shared by the feed, the status builder, and the store. The store persists
//! some of them verbatim, which is why they round-trip through serde in the
//! feed's camelCase spelling rather than an internal one.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// The checksum algorithm a feed may declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChecksumAlgorithm {
    Sha256,
    Sha512,
}

impl ChecksumAlgorithm {
    /// The digest's hex length.
    pub fn hex_length(self) -> usize {
        match self {
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }
}

/// An artifact a feed offers (parity: `DesktopUpdateArtifactSnapshot`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The artifact's mechanism (`dmg`, `installer`, `payload`), which decides
    /// whether an update can be applied in place.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
    pub url: String,
}

/// A checksum a feed declares (parity: `DesktopUpdateChecksumSnapshot`).
///
/// `value` and `url` are alternatives: a feed either inlines the digest or
/// points at a checksum file the updater fetches after download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecksumSnapshot {
    pub algorithm: ChecksumAlgorithm,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl ChecksumSnapshot {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("a checksum snapshot serializes")
    }
}

impl ArtifactSnapshot {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("an artifact snapshot serializes")
    }
}

/// The states a status snapshot may report (parity: `DESKTOP_UPDATE_STATES`).
///
/// The transition states (`checking`, `installing`) are not variants: a command
/// answers with its result, so nothing would ever observe them. `downloading`
/// is a variant because the status stream reports it while the transfer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateState {
    Available,
    Downloaded,
    Downloading,
    Error,
    Idle,
    NotAvailable,
    Unsupported,
}

impl UpdateState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Downloaded => "downloaded",
            Self::Downloading => "downloading",
            Self::Error => "error",
            Self::Idle => "idle",
            Self::NotAvailable => "not-available",
            Self::Unsupported => "unsupported",
        }
    }
}

/// The `capabilities` block of a status snapshot (parity:
/// `DesktopUpdateCapabilitySet`).
///
/// The renderer branches its affordances on these four booleans rather than on
/// the state, so a host that can see a release but not fetch it says exactly
/// that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateCapabilities {
    pub can_apply_in_place: bool,
    pub can_download: bool,
    pub can_open_installer: bool,
    pub requires_manual_install: bool,
}

impl UpdateCapabilities {
    pub fn to_json(self) -> Value {
        json!({
            "canApplyInPlace": self.can_apply_in_place,
            "canDownload": self.can_download,
            "canOpenInstaller": self.can_open_installer,
            "requiresManualInstall": self.requires_manual_install,
        })
    }
}

/// A JSON object with named fields, as a map.
pub fn object_map(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("a json! object literal")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_serialize_in_the_camel_case_wire_shape() {
        let artifact = ArtifactSnapshot {
            name: Some("OpenDesign-1.2.3.dmg".into()),
            platform_key: Some("mac".into()),
            size: Some(42),
            artifact_type: Some("dmg".into()),
            url: "https://example.test/a.dmg".into(),
        };
        assert_eq!(artifact.to_json()["platformKey"], json!("mac"));
        assert_eq!(artifact.to_json()["type"], json!("dmg"));

        let bare = ArtifactSnapshot {
            name: None,
            platform_key: None,
            size: None,
            artifact_type: None,
            url: "https://example.test/a.bin".into(),
        };
        let encoded = bare.to_json();
        assert!(
            encoded.get("name").is_none(),
            "an absent field stays absent"
        );
        assert_eq!(encoded["url"], json!("https://example.test/a.bin"));

        let checksum = ChecksumSnapshot {
            algorithm: ChecksumAlgorithm::Sha512,
            url: None,
            value: Some("f".repeat(128)),
        };
        assert_eq!(checksum.to_json()["algorithm"], json!("sha512"));
        assert_eq!(checksum.to_json()["value"].as_str().unwrap().len(), 128);
    }

    #[test]
    fn states_use_the_wire_spellings() {
        assert_eq!(UpdateState::NotAvailable.as_str(), "not-available");
        assert_eq!(UpdateState::Unsupported.as_str(), "unsupported");
        assert_eq!(UpdateState::Available.as_str(), "available");
        assert_eq!(UpdateState::Downloaded.as_str(), "downloaded");
        assert_eq!(UpdateState::Downloading.as_str(), "downloading");
        assert_eq!(UpdateState::Idle.as_str(), "idle");
        assert_eq!(UpdateState::Error.as_str(), "error");
    }

    #[test]
    fn capabilities_serialize_with_the_renderers_field_names() {
        let capabilities = UpdateCapabilities {
            can_apply_in_place: false,
            can_download: true,
            can_open_installer: true,
            requires_manual_install: true,
        };
        let encoded = capabilities.to_json();
        assert_eq!(encoded["canDownload"], json!(true));
        assert_eq!(encoded["canApplyInPlace"], json!(false));
        assert_eq!(encoded["canOpenInstaller"], json!(true));
        assert_eq!(encoded["requiresManualInstall"], json!(true));
    }
}
