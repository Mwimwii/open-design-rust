//! The update store: one directory tree the updater owns, plus the record of
//! what it holds (parity: `updater/store.ts`).
//!
//! The store exists so a download can be interrupted without corrupting an
//! installed app: artifacts land in `staging/<cycle>`, are verified there, and
//! only then are promoted into `releases/<key>`. The sentinel file is what
//! makes deletion safe — `clear-cache` (and any future purge) may only remove a
//! tree this updater proved it owns.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::config::resolve_path;
use super::model::{ArtifactSnapshot, ChecksumAlgorithm, ChecksumSnapshot};
use super::support::UpdateError;

/// The marker that proves a directory belongs to this updater (parity:
/// `OWNERSHIP_SENTINEL`).
pub const OWNERSHIP_SENTINEL: &str = ".open-design-updater-root.json";
/// The owner name the sentinel records.
pub const STORE_OWNER: &str = "open-design-updater";
pub const STORE_METADATA_FILE: &str = "metadata.json";
pub const RELEASES_DIR: &str = "releases";
pub const STAGING_DIR: &str = "staging";
pub const DOWNLOADS_DIR: &str = "downloads";
pub const BACK_DIR: &str = ".back";
pub const HELPERS_DIR: &str = "helpers";
pub const STATE_DIR: &str = "state";
pub const UPDATE_ROOT_VERSION: u32 = 1;
pub const STORE_METADATA_VERSION: u32 = 1;

/// A downloaded release the store holds (parity: `UpdateReleaseRef`).
///
/// Paths are relative to the store root, so the record survives moving or
/// renaming the tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReleaseRef {
    pub arch: String,
    pub artifact: ArtifactSnapshot,
    pub artifact_path: String,
    pub checksum: ChecksumSnapshot,
    pub checksum_path: String,
    pub channel: String,
    pub downloaded_at: String,
    pub key: String,
    pub metadata: Map<String, Value>,
    pub metadata_path: String,
    pub platform_key: String,
    pub version: String,
}

/// The store's record of itself (parity: `UpdateStoreMetadata`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<UpdateReleaseRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    pub version: u32,
}

impl Default for StoreMetadata {
    fn default() -> Self {
        Self {
            active: None,
            last_checked_at: None,
            version: STORE_METADATA_VERSION,
        }
    }
}

/// The directories inside an update root (parity:
/// `DesktopUpdaterStoreLayout`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLayout {
    pub back_root: PathBuf,
    pub downloads_root: PathBuf,
    pub helpers_root: PathBuf,
    pub metadata_path: PathBuf,
    pub ownership_sentinel_path: PathBuf,
    pub releases_root: PathBuf,
    pub root: PathBuf,
    pub staging_root: PathBuf,
    pub state_root: PathBuf,
}

impl StoreLayout {
    /// The root-level entry a path lives under (parity: `rootEntryForPath`).
    pub fn root_entry_for(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let first = relative.components().next()?;
        Some(first.as_os_str().to_string_lossy().into_owned())
    }

    /// Whether a root-level entry is part of the layout (parity:
    /// `isAllowedRootEntry`).
    pub fn is_allowed_root_entry(&self, name: &str) -> bool {
        [
            &self.back_root,
            &self.downloads_root,
            &self.helpers_root,
            &self.ownership_sentinel_path,
            &self.releases_root,
            &self.staging_root,
            &self.state_root,
            &self.metadata_path,
        ]
        .into_iter()
        .any(|path| self.root_entry_for(path).as_deref() == Some(name))
    }

    /// The subdirectories the store owns and expects to exist.
    pub fn owned_directories(&self) -> [&PathBuf; 6] {
        [
            &self.releases_root,
            &self.staging_root,
            &self.downloads_root,
            &self.back_root,
            &self.helpers_root,
            &self.state_root,
        ]
    }
}

/// The store layout under a root (parity: `resolveDesktopUpdaterStoreLayout`).
pub fn resolve_store_layout(root: &Path) -> StoreLayout {
    let root = resolve_path(root);
    StoreLayout {
        back_root: root.join(BACK_DIR),
        downloads_root: root.join(DOWNLOADS_DIR),
        helpers_root: root.join(HELPERS_DIR),
        metadata_path: root.join(STORE_METADATA_FILE),
        ownership_sentinel_path: root.join(OWNERSHIP_SENTINEL),
        releases_root: root.join(RELEASES_DIR),
        staging_root: root.join(STAGING_DIR),
        state_root: root.join(STATE_DIR),
        root,
    }
}

/// Whether `path` stays inside `root` without `..` components (parity:
/// `containsPath`).
pub fn contains_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative
        .components()
        .all(|component| !matches!(component, Component::ParentDir | Component::RootDir))
}

/// A store root this updater has proved it owns.
#[derive(Debug, Clone)]
pub struct OwnedRoot {
    pub layout: StoreLayout,
    pub metadata: StoreMetadata,
}

impl OwnedRoot {
    /// An absolute path inside the store, refused if it would escape.
    pub fn resolve(&self, relative: &str) -> Result<PathBuf, UpdateError> {
        let path = self.layout.root.join(relative);
        if !contains_path(&self.layout.root, &path) {
            return Err(UpdateError::new(
                "update-store-invalid-shape",
                format!("resolved path escaped the update store: {relative}"),
            ));
        }
        Ok(path)
    }
}

fn shape_error(message: impl Into<String>) -> UpdateError {
    UpdateError::new("update-store-invalid-shape", message)
}

fn read_sentinel(path: &Path) -> Result<Option<Map<String, Value>>, UpdateError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(shape_error(format!("ownership marker unreadable: {error}"))),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(marker)) => Ok(Some(marker)),
        _ => Err(shape_error("ownership marker is not a JSON object")),
    }
}

fn directory_is_empty(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

/// Open (creating if needed) the store under `root` (parity:
/// `ensureOwnedUpdateRoot`).
///
/// Ownership is proven, not assumed: an existing tree without the marker is
/// someone else's directory, and a marker from a newer store version is a tree
/// this build must not touch.
pub fn ensure_owned_root(root: &Path, owner: &str) -> Result<OwnedRoot, UpdateError> {
    let layout = resolve_store_layout(root);
    fs::create_dir_all(&layout.root).map_err(|error| {
        UpdateError::new(
            "update-root-unwritable",
            format!("update root could not be created: {error}"),
        )
    })?;
    if fs::symlink_metadata(&layout.root)
        .map(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
        .unwrap_or(true)
    {
        return Err(UpdateError::new(
            "update-root-not-owned",
            format!(
                "update root is not an owned directory: {}",
                layout.root.display()
            ),
        ));
    }

    match read_sentinel(&layout.ownership_sentinel_path)? {
        Some(marker) => {
            if marker.get("version").and_then(Value::as_u64) != Some(u64::from(UPDATE_ROOT_VERSION))
            {
                return Err(UpdateError::new(
                    "update-root-version-mismatch",
                    format!(
                        "update root has an unsupported ownership marker version at {}",
                        layout.ownership_sentinel_path.display()
                    ),
                ));
            }
        }
        None => {
            if !directory_is_empty(&layout.root) {
                return Err(UpdateError::new(
                    "update-root-not-owned",
                    format!(
                        "update root is not empty and has no OpenDesign updater ownership marker: {}",
                        layout.root.display()
                    ),
                ));
            }
            let marker = serde_json::json!({
                "createdAt": timestamp_now(),
                "owner": owner,
                "version": UPDATE_ROOT_VERSION,
            });
            write_json(&layout.ownership_sentinel_path, &marker)?;
        }
    }

    if let Ok(entries) = fs::read_dir(&layout.root) {
        let unexpected: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| !layout.is_allowed_root_entry(name))
            .collect();
        if !unexpected.is_empty() {
            return Err(shape_error(format!(
                "update store contains unexpected root entries: {}",
                unexpected.join(", ")
            )));
        }
    }

    for directory in layout.owned_directories() {
        ensure_plain_directory(&layout.root, directory)?;
    }

    let metadata = read_metadata(&layout.metadata_path)?;
    Ok(OwnedRoot { layout, metadata })
}

fn ensure_plain_directory(root: &Path, directory: &Path) -> Result<(), UpdateError> {
    match fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(directory)
                .map_err(|error| shape_error(format!("update store directory failed: {error}")))?;
            Ok(())
        }
        Err(error) => Err(shape_error(format!(
            "update store entry unreadable: {error}"
        ))),
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(shape_error(format!(
                    "update store entry {} must be a plain directory",
                    directory.display()
                )));
            }
            if !contains_path(root, directory) {
                return Err(shape_error(format!(
                    "update store entry {} escapes update root",
                    directory.display()
                )));
            }
            Ok(())
        }
    }
}

/// Delete every release the store holds and forget the active one (parity:
/// `clearUpdateCache`).
///
/// Only the layout's own directories are emptied, and the ownership marker
/// stays: clearing a cache must never turn a proved store into an unclaimed
/// directory, or the next run would refuse the tree it just emptied.
pub fn clear_cached_releases(root: &Path) -> Result<(), UpdateError> {
    let owned = ensure_owned_root(root, STORE_OWNER)?;
    for directory in [
        &owned.layout.releases_root,
        &owned.layout.staging_root,
        &owned.layout.downloads_root,
    ] {
        clear_directory(directory)?;
    }
    let mut metadata = owned.metadata;
    metadata.active = None;
    write_metadata(&owned.layout, &metadata)
}

fn clear_directory(directory: &Path) -> Result<(), UpdateError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(shape_error(format!(
                "update store cache could not be read: {error}"
            )))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            shape_error(format!("update store cache could not be read: {error}"))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            shape_error(format!("update store cache could not be read: {error}"))
        })?;
        let cleared = if file_type.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        cleared.map_err(|error| {
            shape_error(format!(
                "update store cache entry {} could not be removed: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

/// Read the store's metadata, treating a missing or unreadable record as empty
/// (parity: `readStoreMetadata`, which returns `null`).
///
/// Losing the record is recoverable — it only names the release that *was*
/// downloaded, and the feed can offer it again — so a corrupt file costs a
/// re-download rather than a wedged updater.
pub fn read_metadata(path: &Path) -> Result<StoreMetadata, UpdateError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoreMetadata::default())
        }
        Err(error) => {
            return Err(shape_error(format!(
                "update store metadata unreadable: {error}"
            )))
        }
    };
    match serde_json::from_slice::<StoreMetadata>(&bytes) {
        Ok(metadata) if metadata.version == STORE_METADATA_VERSION => Ok(metadata),
        // A record this build cannot read is a record it must not trust.
        _ => Ok(StoreMetadata::default()),
    }
}

/// Write the store's metadata atomically (parity: `writeStoreMetadata`).
///
/// The record is what makes a half-promoted release visible, so it is written
/// to a sibling and renamed: a crash leaves either the old record or the new
/// one, never a truncated one.
pub fn write_metadata(layout: &StoreLayout, metadata: &StoreMetadata) -> Result<(), UpdateError> {
    if let Some(parent) = layout.metadata_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| shape_error(format!("update store metadata failed: {error}")))?;
    }
    let encoded = serde_json::to_vec_pretty(metadata)
        .map_err(|error| shape_error(format!("update store metadata failed: {error}")))?;
    let temporary = layout.metadata_path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| shape_error(format!("update store metadata failed: {error}")))?;
        file.write_all(&encoded)
            .map_err(|error| shape_error(format!("update store metadata failed: {error}")))?;
        file.sync_all()
            .map_err(|error| shape_error(format!("update store metadata failed: {error}")))?;
    }
    fs::rename(&temporary, &layout.metadata_path)
        .map_err(|error| shape_error(format!("update store metadata failed: {error}")))
}

/// Write any JSON document atomically.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), UpdateError> {
    let encoded = serde_json::to_vec_pretty(value)
        .map_err(|error| shape_error(format!("update store write failed: {error}")))?;
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| shape_error(format!("update store write failed: {error}")))?;
        file.write_all(&encoded)
            .map_err(|error| shape_error(format!("update store write failed: {error}")))?;
        file.sync_all()
            .map_err(|error| shape_error(format!("update store write failed: {error}")))?;
    }
    fs::rename(&temporary, path)
        .map_err(|error| shape_error(format!("update store write failed: {error}")))
}

/// The current time as the ISO-8601 UTC string the store records (parity:
/// `new Date().toISOString()`).
pub fn timestamp_now() -> String {
    timestamp_from(SystemTime::now())
}
fn timestamp_from(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs() as i64;
    let millis = since_epoch.subsec_millis();
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60
    )
}

/// Days since the Unix epoch to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The checksum record a store keeps next to a verified artifact (parity:
/// `ResolvedChecksumSnapshot`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedChecksum {
    pub algorithm: ChecksumAlgorithm,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "od-update-store-{name}-{}-{:?}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn a_fresh_root_gets_the_marker_and_its_directories() {
        let root = scratch("fresh");
        let owned = ensure_owned_root(&root, "open-design-updater").expect("owned root");
        for directory in [
            RELEASES_DIR,
            STAGING_DIR,
            DOWNLOADS_DIR,
            BACK_DIR,
            HELPERS_DIR,
            STATE_DIR,
        ] {
            assert!(
                owned.layout.root.join(directory).is_dir(),
                "{directory} must exist"
            );
        }
        let marker: Value = serde_json::from_slice(
            &fs::read(&owned.layout.ownership_sentinel_path).expect("marker"),
        )
        .expect("json");
        assert_eq!(marker["version"], serde_json::json!(UPDATE_ROOT_VERSION));
        assert_eq!(marker["owner"], serde_json::json!("open-design-updater"));
        assert!(marker["createdAt"]
            .as_str()
            .is_some_and(|value| value.ends_with('Z')));
        assert_eq!(owned.metadata, StoreMetadata::default());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reopening_keeps_the_record_and_the_marker() {
        let root = scratch("reopen");
        let owned = ensure_owned_root(&root, "open-design-updater").expect("first open");
        let metadata = StoreMetadata {
            last_checked_at: Some("2026-01-01T00:00:00.000Z".into()),
            ..StoreMetadata::default()
        };
        write_metadata(&owned.layout, &metadata).expect("write");

        let reopened = ensure_owned_root(&root, "open-design-updater").expect("second open");
        assert_eq!(
            reopened.metadata.last_checked_at.as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_someone_else_owns_is_refused() {
        let root = scratch("foreign");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("their-file.txt"), b"not ours").expect("file");
        let error = ensure_owned_root(&root, "open-design-updater").expect_err("foreign root");
        assert_eq!(error.code, "update-root-not-owned");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_marker_from_another_store_version_is_refused() {
        let root = scratch("version");
        let owned = ensure_owned_root(&root, "open-design-updater").expect("owned root");
        fs::write(
            &owned.layout.ownership_sentinel_path,
            br#"{"version": 99, "owner": "open-design-updater"}"#,
        )
        .expect("marker");
        let error = ensure_owned_root(&root, "open-design-updater").expect_err("version");
        assert_eq!(error.code, "update-root-version-mismatch");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unexpected_root_entry_is_a_shape_error() {
        let root = scratch("shape");
        let owned = ensure_owned_root(&root, "open-design-updater").expect("owned root");
        fs::write(owned.layout.root.join("stray.txt"), b"?").expect("stray");
        let error = ensure_owned_root(&root, "open-design-updater").expect_err("shape");
        assert_eq!(error.code, "update-store-invalid-shape");
        assert!(error.message.contains("stray.txt"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_record_reads_as_empty_instead_of_wedging_the_updater() {
        let root = scratch("corrupt");
        let owned = ensure_owned_root(&root, "open-design-updater").expect("owned root");
        fs::write(&owned.layout.metadata_path, b"{ not json").expect("corrupt");
        let reopened = ensure_owned_root(&root, "open-design-updater").expect("reopen");
        assert_eq!(reopened.metadata, StoreMetadata::default());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn contained_paths_resolve_and_escapes_are_refused() {
        let root = scratch("escape");
        let owned = ensure_owned_root(&root, "open-design-updater").expect("owned root");
        assert_eq!(
            owned
                .resolve("releases/key/OpenDesign.dmg")
                .expect("inside"),
            owned.layout.root.join("releases/key/OpenDesign.dmg")
        );
        let error = owned.resolve("../outside.bin").expect_err("escape");
        assert_eq!(error.code, "update-store-invalid-shape");
        assert!(!contains_path(&owned.layout.root, Path::new("/etc/passwd")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_release_record_round_trips_with_relative_paths() {
        let reference = sample_reference();
        let metadata = StoreMetadata {
            active: Some(reference.clone()),
            ..StoreMetadata::default()
        };
        let encoded = serde_json::to_value(&metadata).expect("encode");
        assert_eq!(
            encoded["active"]["artifactPath"],
            serde_json::json!(reference.artifact_path)
        );
        assert_eq!(encoded["active"]["channel"], serde_json::json!("beta"));
        let decoded: StoreMetadata = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded.active, Some(reference));
    }

    #[test]
    fn clearing_the_cache_empties_the_releases_and_forgets_the_active_one() {
        let root = scratch("clear");
        let owned = ensure_owned_root(&root, STORE_OWNER).expect("owned root");
        let release_dir = owned
            .layout
            .releases_root
            .join("1.2.3-mac-arm64-abcdef123456");
        fs::create_dir_all(&release_dir).expect("release");
        fs::write(release_dir.join("artifact.dmg"), b"bytes").expect("artifact");
        fs::create_dir_all(owned.layout.staging_root.join("cycle")).expect("staging");
        let metadata = StoreMetadata {
            active: Some(sample_reference()),
            ..StoreMetadata::default()
        };
        write_metadata(&owned.layout, &metadata).expect("metadata");

        clear_cached_releases(&root).expect("clear");

        assert_eq!(
            fs::read_dir(&owned.layout.releases_root)
                .expect("releases")
                .count(),
            0,
            "releases are gone"
        );
        assert_eq!(
            fs::read_dir(&owned.layout.staging_root)
                .expect("staging")
                .count(),
            0,
            "staging is gone"
        );
        assert!(
            owned.layout.ownership_sentinel_path.is_file(),
            "the store still proves it owns the tree"
        );
        let reopened = ensure_owned_root(&root, STORE_OWNER).expect("reopen");
        assert_eq!(reopened.metadata.active, None);
        let _ = fs::remove_dir_all(&root);
    }

    fn sample_reference() -> UpdateReleaseRef {
        UpdateReleaseRef {
            arch: "arm64".into(),
            artifact: ArtifactSnapshot {
                name: Some("OpenDesign.dmg".into()),
                platform_key: Some("mac".into()),
                size: Some(10),
                artifact_type: Some("dmg".into()),
                url: "https://example.test/OpenDesign.dmg".into(),
            },
            artifact_path: "releases/key/OpenDesign.dmg".into(),
            checksum: ChecksumSnapshot {
                algorithm: ChecksumAlgorithm::Sha256,
                url: None,
                value: Some("a".repeat(64)),
            },
            checksum_path: "releases/key/checksum.json".into(),
            channel: "beta".into(),
            downloaded_at: "2026-01-01T00:00:00.000Z".into(),
            key: "1.2.3-mac-arm64-abcdef123456".into(),
            metadata: Map::new(),
            metadata_path: "releases/key/metadata.json".into(),
            platform_key: "mac".into(),
            version: "1.2.3-beta.4".into(),
        }
    }

    #[test]
    fn timestamps_are_iso_utc() {
        let epoch = UNIX_EPOCH + std::time::Duration::from_millis(0);
        assert_eq!(timestamp_from(epoch), "1970-01-01T00:00:00.000Z");
        let leap = UNIX_EPOCH + std::time::Duration::from_millis(1_709_164_800_123);
        assert_eq!(timestamp_from(leap), "2024-02-29T00:00:00.123Z");
    }
}
