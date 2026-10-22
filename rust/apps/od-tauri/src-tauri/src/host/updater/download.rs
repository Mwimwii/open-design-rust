//! Fetching and verifying a release artifact (parity: `downloadCopyAndClear`
//! plus the verify/promote half of `downloadUpdate` in `updater.ts`).
//!
//! The order matters and is the whole safety argument: download into
//! `staging/<cycle>/`, hash the bytes that actually landed, compare them to the
//! digest the feed named, and only then let the release become visible by
//! renaming the staging directory into `releases/<key>/`. A crash at any point
//! leaves a partial tree the next attempt overwrites.
//!
//! Resumption is per-file: an interrupted transfer leaves a partial artifact,
//! and the next attempt asks the server for the remainder (`Range`) instead of
//! starting over. A server that ignores the range simply rewrites the file.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256, Sha512};

use super::config::UpdaterConfig;
use super::feed::{artifact_file_name, parse_checksum_text, release_key, UpdateCandidate};
use super::model::{ChecksumAlgorithm, ChecksumSnapshot};
use super::store::{
    contains_path, ensure_owned_root, timestamp_now, write_json, write_metadata, ResolvedChecksum,
    UpdateReleaseRef,
};
use super::support::UpdateError;

/// How many times one artifact transfer is attempted before the download is
/// reported failed (parity: `ARTIFACT_DOWNLOAD_MAX_ATTEMPTS`).
pub const DOWNLOAD_MAX_ATTEMPTS: u32 = 3;

/// The read size used to hash a downloaded artifact.
const HASH_BUFFER_SIZE: usize = 64 * 1024;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The digest a feed declared, fetching the checksum file when the feed only
/// pointed at one (parity: `resolveChecksum`).
pub async fn resolve_checksum(
    client: &reqwest::Client,
    checksum: &ChecksumSnapshot,
) -> Result<ResolvedChecksum, UpdateError> {
    if let Some(value) = checksum.value.clone() {
        return Ok(ResolvedChecksum {
            algorithm: checksum.algorithm,
            url: checksum.url.clone(),
            value: value.to_ascii_lowercase(),
        });
    }
    let Some(url) = checksum.url.clone() else {
        return Err(UpdateError::new(
            "checksum-unavailable",
            "release metadata does not include a checksum for this artifact",
        ));
    };
    let response = client.get(&url).send().await.map_err(|error| {
        UpdateError::new(
            "checksum-unavailable",
            format!("checksum file request failed: {error}"),
        )
    })?;
    if !response.status().is_success() {
        return Err(UpdateError::new(
            "checksum-unavailable",
            format!(
                "checksum file request failed with HTTP {}",
                response.status().as_u16()
            ),
        ));
    }
    let text = response.text().await.map_err(|error| {
        UpdateError::new(
            "checksum-unavailable",
            format!("checksum file could not be read: {error}"),
        )
    })?;
    let value = parse_checksum_text(&text, checksum.algorithm)
        .map_err(|message| UpdateError::new("checksum-unavailable", message))?;
    Ok(ResolvedChecksum {
        algorithm: checksum.algorithm,
        url: Some(url),
        value,
    })
}

/// The digest of a file on disk, read in chunks so a large artifact does not
/// have to fit in memory.
pub fn hash_file(path: &Path, algorithm: ChecksumAlgorithm) -> Result<String, UpdateError> {
    let mut file = File::open(path).map_err(|error| {
        UpdateError::new(
            "download-failed",
            format!("downloaded artifact could not be read: {error}"),
        )
    })?;
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    match algorithm {
        ChecksumAlgorithm::Sha256 => {
            let mut hasher = Sha256::new();
            loop {
                let read = file.read(&mut buffer).map_err(|error| {
                    UpdateError::new(
                        "download-failed",
                        format!("downloaded artifact could not be read: {error}"),
                    )
                })?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            Ok(hex(&hasher.finalize()))
        }
        ChecksumAlgorithm::Sha512 => {
            let mut hasher = Sha512::new();
            loop {
                let read = file.read(&mut buffer).map_err(|error| {
                    UpdateError::new(
                        "download-failed",
                        format!("downloaded artifact could not be read: {error}"),
                    )
                })?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            Ok(hex(&hasher.finalize()))
        }
    }
}

/// One transfer attempt, appending to whatever a previous attempt left.
///
/// `Ok(bytes)` is the artifact's size on disk after the attempt.
async fn transfer_once(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
) -> Result<u64, String> {
    let existing = fs::metadata(destination)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut request = client.get(url);
    if existing > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={existing}-"));
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| format!("artifact request failed: {error}"))?;
    let status = response.status();

    let resuming = existing > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut written = existing;
    let mut file = if resuming {
        OpenOptions::new()
            .append(true)
            .open(destination)
            .map_err(|error| format!("artifact could not be opened for resume: {error}"))?
    } else if status.is_success() {
        written = 0;
        File::create(destination)
            .map_err(|error| format!("artifact could not be created: {error}"))?
    } else {
        return Err(format!(
            "artifact request failed with HTTP {}",
            status.as_u16()
        ));
    };

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("artifact transfer failed: {error}"))?
    {
        file.write_all(&chunk)
            .map_err(|error| format!("artifact could not be written: {error}"))?;
        written += chunk.len() as u64;
    }
    file.sync_all()
        .map_err(|error| format!("artifact could not be written: {error}"))?;
    Ok(written)
}

/// Download, verify, and promote one candidate's artifact (parity:
/// `downloadUpdate`'s download/verify/promote half).
///
/// Returns the store record; the caller writes it into the store's metadata,
/// which is what makes the release visible to `install`.
pub async fn download_release(
    config: &UpdaterConfig,
    candidate: &UpdateCandidate,
) -> Result<UpdateReleaseRef, UpdateError> {
    let owned = ensure_owned_root(&config.download_root, "open-design-updater")?;
    let layout = owned.layout.clone();

    let cycle = format!(
        "{}-{}",
        timestamp_now().replace([':', '.'], "-"),
        std::process::id()
    );
    let staging = layout.staging_root.join(&cycle);
    if !contains_path(&layout.root, &staging) {
        return Err(UpdateError::new(
            "download-path-escaped",
            "resolved update staging path escaped the update root",
        ));
    }
    fs::create_dir_all(&staging).map_err(|error| {
        UpdateError::new(
            "download-failed",
            format!("update staging directory could not be created: {error}"),
        )
    })?;

    let file_name = artifact_file_name(candidate);
    let target: PathBuf = staging.join(&file_name);
    if !contains_path(&layout.root, &target) {
        let _ = fs::remove_dir_all(&staging);
        return Err(UpdateError::new(
            "download-path-escaped",
            "resolved update download path escaped the update root",
        ));
    }

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| {
            UpdateError::new(
                "download-failed",
                format!("update download client could not start: {error}"),
            )
        })?;

    let checksum = match resolve_checksum(&client, &candidate.checksum).await {
        Ok(checksum) => checksum,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    let mut last_error = String::new();
    for _ in 0..DOWNLOAD_MAX_ATTEMPTS {
        match transfer_once(&client, &candidate.artifact.url, &target).await {
            Ok(_) => {
                last_error.clear();
                break;
            }
            Err(message) => last_error = message,
        }
    }
    if !last_error.is_empty() {
        let _ = fs::remove_dir_all(&staging);
        return Err(UpdateError::new("download-failed", last_error));
    }

    let digest = match hash_file(&target, checksum.algorithm) {
        Ok(digest) => digest,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if !digest.eq_ignore_ascii_case(&checksum.value) {
        let _ = fs::remove_dir_all(&staging);
        return Err(UpdateError::new(
            "checksum-mismatch",
            "downloaded update checksum did not match release metadata",
        ));
    }

    let key = release_key(candidate, &checksum.value);
    let release_dir = layout.releases_root.join(&key);
    if !contains_path(&layout.root, &release_dir) {
        let _ = fs::remove_dir_all(&staging);
        return Err(UpdateError::new(
            "download-path-escaped",
            "resolved release path escaped the update root",
        ));
    }
    write_json(&staging.join("metadata.json"), &candidate.metadata)?;
    write_json(&staging.join("checksum.json"), &checksum)?;
    // A previous attempt may have promoted the same key; the artifact on disk is
    // this one, re-verified above, so replacing it is safe.
    if release_dir.exists() {
        fs::remove_dir_all(&release_dir).map_err(|error| {
            UpdateError::new(
                "release-promote-failed",
                format!("previous release could not be replaced: {error}"),
            )
        })?;
    }
    fs::rename(&staging, &release_dir).map_err(|error| {
        UpdateError::new(
            "release-promote-failed",
            format!("verified release could not be promoted: {error}"),
        )
    })?;

    let relative = |path: &Path| {
        path.strip_prefix(&layout.root)
            .map(|relative| relative.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned())
    };
    let reference = UpdateReleaseRef {
        arch: candidate.arch.clone(),
        artifact: candidate.artifact.clone(),
        artifact_path: relative(&release_dir.join(&file_name)),
        checksum: ChecksumSnapshot {
            algorithm: checksum.algorithm,
            url: checksum.url.clone(),
            value: Some(checksum.value.clone()),
        },
        checksum_path: relative(&release_dir.join("checksum.json")),
        channel: candidate.channel.as_str().to_string(),
        downloaded_at: timestamp_now(),
        key,
        metadata: candidate.metadata.clone(),
        metadata_path: relative(&release_dir.join("metadata.json")),
        platform_key: candidate.platform_key.clone(),
        version: candidate.version.clone(),
    };

    let mut metadata = owned.metadata;
    metadata.active = Some(reference.clone());
    metadata.last_checked_at = Some(timestamp_now());
    write_metadata(&layout, &metadata)?;
    Ok(reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::updater::config::{
        resolve_config, UpdaterConfigInput, DEFAULT_RELEASE_ORIGIN,
    };
    use crate::host::updater::feed::select_update_candidate;
    use crate::host::updater::store::StoreMetadata;
    use serde_json::json;
    use std::collections::HashMap;

    fn config(download_root: &Path) -> UpdaterConfig {
        let env: HashMap<String, String> = [
            ("OD_UPDATE_PLATFORM".to_string(), "darwin".to_string()),
            ("OD_UPDATE_ARCH".to_string(), "arm64".to_string()),
            ("OD_UPDATE_CHANNEL".to_string(), "beta".to_string()),
            ("OD_UPDATE_CURRENT_VERSION".to_string(), "1.0.0".to_string()),
        ]
        .into_iter()
        .collect();
        resolve_config(&UpdaterConfigInput {
            download_root: Some(download_root.to_path_buf()),
            env,
            packaged: true,
            ..Default::default()
        })
        .expect("config")
    }

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "od-update-download-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn candidate(digest: &str) -> UpdateCandidate {
        let metadata = json!({
            "channel": "beta",
            "releaseVersion": "1.2.3-beta.4",
            "platforms": {
                "mac": {
                    "enabled": true,
                    "artifacts": {"dmg": {
                        "url": "https://example.test/OpenDesign.dmg",
                        "name": "OpenDesign.dmg",
                        "sha256": digest,
                    }},
                },
            },
        });
        select_update_candidate(
            metadata.as_object().expect("object"),
            &config(&scratch("cfg")),
            false,
        )
        .expect("candidate")
    }

    #[test]
    fn an_inline_digest_needs_no_second_request() {
        let checksum = ChecksumSnapshot {
            algorithm: ChecksumAlgorithm::Sha256,
            url: None,
            value: Some("AB".repeat(32)),
        };
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let resolved = runtime
            .block_on(async { resolve_checksum(&reqwest::Client::new(), &checksum).await })
            .expect("resolved");
        assert_eq!(resolved.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(
            resolved.value,
            "ab".repeat(32),
            "digests compare lowercased"
        );
    }

    #[test]
    fn a_checksum_with_neither_a_digest_nor_a_url_is_refused() {
        let checksum = ChecksumSnapshot {
            algorithm: ChecksumAlgorithm::Sha256,
            url: None,
            value: None,
        };
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let error = runtime
            .block_on(async { resolve_checksum(&reqwest::Client::new(), &checksum).await })
            .expect_err("no checksum");
        assert_eq!(error.code, "checksum-unavailable");
        assert!(error.message.contains("checksum"));
    }

    #[test]
    fn hashing_a_file_matches_the_known_digest() {
        let root = scratch("hash");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("artifact.bin");
        fs::write(&path, b"open design").expect("write");
        assert_eq!(
            hash_file(&path, ChecksumAlgorithm::Sha256).expect("digest"),
            format!("{:x}", Sha256::digest(b"open design"))
        );
        assert_eq!(
            hash_file(&path, ChecksumAlgorithm::Sha512).expect("digest"),
            format!("{:x}", Sha512::digest(b"open design"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_artifact_is_a_download_failure_not_a_panic() {
        let root = scratch("missing");
        fs::create_dir_all(&root).expect("root");
        let error = hash_file(&root.join("absent.bin"), ChecksumAlgorithm::Sha256)
            .expect_err("missing file");
        assert_eq!(error.code, "download-failed");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_candidate_names_the_file_and_key_the_store_will_use() {
        let digest = "a".repeat(64);
        let candidate = candidate(&digest);
        assert_eq!(
            artifact_file_name(&candidate),
            "open-design-1.2.3-beta.4-mac-arm64-dmg.dmg"
        );
        assert!(release_key(&candidate, &digest).starts_with("1.2.3-beta.4-mac-arm64-"));
        assert_eq!(
            StoreMetadata::default().version,
            crate::host::updater::store::STORE_METADATA_VERSION
        );
        assert!(DEFAULT_RELEASE_ORIGIN.contains("open-design.ai"));
    }

    /// Serve one artifact over loopback and return its address, so the whole
    /// download → verify → promote path runs without a feed server.
    fn serve_once(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 2048];
                let _ = stream.read(&mut buffer);
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (format!("http://{address}/artifact.dmg"), server)
    }

    fn candidate_at(config: &UpdaterConfig, url: &str, digest: &str) -> UpdateCandidate {
        let metadata = json!({
            "channel": "beta",
            "releaseVersion": "1.2.3-beta.4",
            "platforms": {
                "mac": {
                    "enabled": true,
                    "artifacts": {"dmg": {
                        "url": url,
                        "name": "OpenDesign.dmg",
                        "sha256": digest,
                    }},
                },
            },
        });
        select_update_candidate(metadata.as_object().expect("object"), config, false)
            .expect("candidate")
    }

    #[test]
    fn a_download_is_verified_promoted_and_recorded_as_the_active_release() {
        let body = b"open design artifact bytes".to_vec();
        let digest = format!("{:x}", Sha256::digest(&body));
        let (url, server) = serve_once(body.clone());
        let root = scratch("promote");
        let config = config(&root);
        let candidate = candidate_at(&config, &url, &digest);

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let release = runtime
            .block_on(download_release(&config, &candidate))
            .expect("download");
        server.join().expect("server");

        assert_eq!(release.version, "1.2.3-beta.4");
        assert_eq!(release.channel, "beta");
        assert_eq!(
            release.artifact_path,
            format!(
                "releases/{}/open-design-1.2.3-beta.4-mac-arm64-dmg.dmg",
                release.key
            )
        );
        let artifact = root.join(&release.artifact_path);
        assert_eq!(fs::read(&artifact).expect("artifact"), body);

        let owned = crate::host::updater::store::ensure_owned_root(
            &root,
            crate::host::updater::store::STORE_OWNER,
        )
        .expect("store");
        assert_eq!(owned.metadata.active.expect("active").key, release.key);
        assert_eq!(
            fs::read_dir(&owned.layout.staging_root)
                .expect("staging")
                .count(),
            0,
            "a promoted release leaves nothing staged"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_checksum_mismatch_discards_the_whole_download() {
        let body = b"tampered bytes".to_vec();
        // A digest for different bytes: what the feed promised is not what the
        // server sent.
        let digest = format!("{:x}", Sha256::digest(b"the real artifact"));
        let (url, server) = serve_once(body);
        let root = scratch("mismatch");
        let config = config(&root);
        let candidate = candidate_at(&config, &url, &digest);

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let error = runtime
            .block_on(download_release(&config, &candidate))
            .expect_err("mismatch");
        server.join().expect("server");

        assert_eq!(error.code, "checksum-mismatch");
        let owned = crate::host::updater::store::ensure_owned_root(
            &root,
            crate::host::updater::store::STORE_OWNER,
        )
        .expect("store");
        assert_eq!(
            fs::read_dir(&owned.layout.releases_root)
                .expect("releases")
                .count(),
            0,
            "an unverified artifact is never promoted"
        );
        assert_eq!(
            fs::read_dir(&owned.layout.staging_root)
                .expect("staging")
                .count(),
            0,
            "the failed cycle is removed"
        );
        assert_eq!(owned.metadata.active, None);
        let _ = fs::remove_dir_all(&root);
    }
}
