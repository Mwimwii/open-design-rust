//! Bundle assembly (parity: `packages/diagnostics/src/zip.ts`).
//!
//! The TypeScript side uses JSZip; here a minimal ZIP writer emits the same
//! archive — DEFLATE (method 8) entries, a central directory and an end
//! record — so the bundle's contents and layout are what support tooling
//! expects without pulling a zip crate into the graph.

use std::io::Write;

use flate2::write::DeflateEncoder;
use flate2::Compression;
use serde_json::Value;

use super::manifest::{build_manifest, build_machine_info, DiagnosticsContext, DiagnosticsManifest, MachineInfo};
use super::redaction::{redact_json_value, RedactionOptions};
use super::sources::{
    collect_log_sources, find_crash_dumps, find_macos_crash_reports, CollectedContent, CollectedFile,
    CrashDumpLookup, CrashReportLookup, LogSource,
};
use super::time::UtcParts;

/// Prefix of the placeholder written in place of a file that could not be read.
const PLACEHOLDER_PREFIX: &str = "; file unavailable: ";

#[derive(Debug, Clone, Default)]
pub struct DiagnosticsExportInput {
    pub context: DiagnosticsContextInput,
    pub sources: Vec<LogSource>,
    pub redaction: RedactionOptions,
    /// Sanitized live snapshots unavailable from file logs (pre-run failures).
    pub summaries: Vec<(String, Value)>,
    /// When set, scan macOS crash reports matching these substrings.
    pub crash_reports: Option<CrashReportLookup>,
    /// When set, collect Chromium/Electron crash minidumps from this dir.
    pub crash_dumps: Option<CrashDumpLookup>,
}

/// The parts of a [`DiagnosticsContext`] the caller supplies; the export
/// timestamp is filled in by `build_manifest`.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticsContextInput {
    pub app: super::manifest::DiagnosticsAppInfo,
    pub source: String,
    pub namespace: Option<String>,
    pub endpoint: Option<String>,
    pub daemon_reachable: Option<bool>,
    pub extra: Option<Value>,
    pub warnings: Vec<String>,
}

impl DiagnosticsContextInput {
    fn into_context(self) -> DiagnosticsContext {
        DiagnosticsContext {
            app: self.app,
            source: self.source,
            namespace: self.namespace,
            endpoint: self.endpoint,
            daemon_reachable: self.daemon_reachable,
            extra: self.extra,
            warnings: self.warnings,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiagnosticsExportResult {
    pub zip: Vec<u8>,
    pub manifest: DiagnosticsManifest,
    pub machine_info: MachineInfo,
}

fn placeholder_for_missing(file: &CollectedFile) -> String {
    let error = file.error.as_deref().unwrap_or("unknown error");
    format!("{PLACEHOLDER_PREFIX}{error}\n")
}

/// JS `name.replace(/[^A-Za-z0-9._-]/g, '_')` for summary file names.
fn safe_summary_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn deflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(data).map_err(|err| err.to_string())?;
    encoder.finish().map_err(|err| err.to_string())
}

struct ZipEntry {
    name: String,
    /// Method 8 (deflate) unless `stored` is set.
    stored: bool,
    crc: u32,
    compressed: Vec<u8>,
    uncompressed_len: u64,
}

/// Minimal ZIP archive built from named entries.
#[derive(Default)]
pub struct ZipArchive {
    entries: Vec<ZipEntry>,
}

impl ZipArchive {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: &str, data: &[u8]) -> Result<(), String> {
        let compressed = deflate(data)?;
        // A deflate stream larger than the input is pointless; store instead,
        // exactly as JSZip does for incompressible payloads.
        let stored = compressed.len() >= data.len();
        let payload = if stored { data.to_vec() } else { compressed };
        self.entries.push(ZipEntry {
            name: name.to_string(),
            stored,
            crc: crc32fast::hash(data),
            compressed: payload,
            uncompressed_len: data.len() as u64,
        });
        Ok(())
    }

    pub fn finish(self) -> Vec<u8> {
        let (dos_date, dos_time) = UtcParts::from_system_time(std::time::SystemTime::now()).to_dos();
        let mut out: Vec<u8> = Vec::new();
        let mut directory: Vec<u8> = Vec::new();

        for entry in &self.entries {
            let offset = out.len() as u32;
            let method: u16 = if entry.stored { 0 } else { 8 };
            let name_bytes = entry.name.as_bytes();
            // Local file header.
            push_u32(&mut out, 0x0403_4b50);
            push_u16(&mut out, 20);
            push_u16(&mut out, 0);
            push_u16(&mut out, method);
            push_u16(&mut out, dos_time);
            push_u16(&mut out, dos_date);
            push_u32(&mut out, entry.crc);
            push_u32(&mut out, entry.compressed.len() as u32);
            push_u32(&mut out, entry.uncompressed_len as u32);
            push_u16(&mut out, name_bytes.len() as u16);
            push_u16(&mut out, 0);
            out.extend_from_slice(name_bytes);
            out.extend_from_slice(&entry.compressed);

            // Central directory entry.
            push_u32(&mut directory, 0x0201_4b50);
            push_u16(&mut directory, 20);
            push_u16(&mut directory, 20);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, method);
            push_u16(&mut directory, dos_time);
            push_u16(&mut directory, dos_date);
            push_u32(&mut directory, entry.crc);
            push_u32(&mut directory, entry.compressed.len() as u32);
            push_u32(&mut directory, entry.uncompressed_len as u32);
            push_u16(&mut directory, name_bytes.len() as u16);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, 0);
            push_u16(&mut directory, 0);
            push_u32(&mut directory, 0);
            push_u32(&mut directory, offset);
            directory.extend_from_slice(name_bytes);
        }

        let directory_offset = out.len() as u32;
        out.extend_from_slice(&directory);
        let directory_size = directory.len() as u32;
        let count = self.entries.len() as u16;

        // End of central directory.
        push_u32(&mut out, 0x0605_4b50);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, count);
        push_u16(&mut out, count);
        push_u32(&mut out, directory_size);
        push_u32(&mut out, directory_offset);
        push_u16(&mut out, 0);
        out
    }
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// One entry read back out of an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntryBytes {
    pub name: String,
    pub data: Vec<u8>,
}

/// Read an archive this module wrote back into named entries.
///
/// Deliberately minimal: it walks the local file headers and inflates DEFLATE
/// entries, which is exactly the shape [`ZipArchive`] produces. It exists so
/// callers (the export route's tests, `od diagnostics export`) can verify a
/// bundle without a second zip implementation in the graph.
pub fn read_zip_entries(zip: &[u8]) -> Result<Vec<ZipEntryBytes>, String> {
    let eocd = zip
        .windows(4)
        .rposition(|window| window == [0x50, 0x4b, 0x05, 0x06])
        .ok_or_else(|| "end of central directory not found".to_string())?;
    if eocd + 12 > zip.len() {
        return Err("truncated end of central directory".to_string());
    }
    let total = u16::from_le_bytes([zip[eocd + 10], zip[eocd + 11]]) as usize;
    let mut out = Vec::with_capacity(total);
    let mut cursor = 0usize;
    for _ in 0..total {
        if cursor + 30 > zip.len() || zip[cursor..cursor + 4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(format!("expected a local file header at {cursor}"));
        }
        let method = u16::from_le_bytes([zip[cursor + 8], zip[cursor + 9]]);
        let compressed_len =
            u32::from_le_bytes([zip[cursor + 18], zip[cursor + 19], zip[cursor + 20], zip[cursor + 21]]) as usize;
        let name_len = u16::from_le_bytes([zip[cursor + 26], zip[cursor + 27]]) as usize;
        let extra_len = u16::from_le_bytes([zip[cursor + 28], zip[cursor + 29]]) as usize;
        let name_start = cursor + 30;
        let name_end = name_start + name_len;
        if name_end > zip.len() {
            return Err("truncated entry name".to_string());
        }
        let name = String::from_utf8(zip[name_start..name_end].to_vec())
            .map_err(|err| format!("entry name is not utf-8: {err}"))?;
        let data_start = name_end + extra_len;
        let data_end = data_start + compressed_len;
        if data_end > zip.len() {
            return Err(format!("truncated payload for {name}"));
        }
        let payload = &zip[data_start..data_end];
        let data = match method {
            0 => payload.to_vec(),
            8 => {
                let mut decoder = flate2::read::DeflateDecoder::new(payload);
                let mut buffer = Vec::new();
                std::io::Read::read_to_end(&mut decoder, &mut buffer)
                    .map_err(|err| format!("inflate {name}: {err}"))?;
                buffer
            }
            other => return Err(format!("unsupported compression method {other} for {name}")),
        };
        out.push(ZipEntryBytes { name, data });
        cursor = data_end;
    }
    Ok(out)
}

fn pretty(value: &Value) -> Vec<u8> {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| "null".to_string())
        .into_bytes()
}

/// Collect every source, build the manifest and machine info, and emit the zip.
pub fn build_diagnostics_zip(input: DiagnosticsExportInput) -> Result<DiagnosticsExportResult, String> {
    let redaction = input.redaction.clone();
    let mut sources = input.sources;

    if let Some(lookup) = &input.crash_reports {
        sources.extend(find_macos_crash_reports(lookup));
    }
    if let Some(lookup) = &input.crash_dumps {
        sources.extend(find_crash_dumps(lookup));
    }

    let collected = collect_log_sources(&sources, &redaction);
    let context = input.context.into_context();
    let manifest = build_manifest(&context, &collected);
    let machine_info = build_machine_info(redaction.username.as_deref());

    let mut archive = ZipArchive::new();
    for file in &collected {
        let payload: Vec<u8> = match &file.content {
            Some(CollectedContent::Text(text)) => text.as_bytes().to_vec(),
            Some(CollectedContent::Binary(bytes)) => bytes.clone(),
            None => placeholder_for_missing(file).into_bytes(),
        };
        archive.add(&file.name, &payload)?;
    }
    archive.add(
        "summary/manifest.json",
        &pretty(&redact_json_value(
            &serde_json::to_value(&manifest).map_err(|err| err.to_string())?,
            &redaction,
        )),
    )?;
    archive.add(
        "summary/machine-info.json",
        &pretty(&redact_json_value(
            &serde_json::to_value(&machine_info).map_err(|err| err.to_string())?,
            &redaction,
        )),
    )?;
    for (name, value) in &input.summaries {
        archive.add(
            &format!("summary/{}", safe_summary_name(name)),
            &pretty(&redact_json_value(value, &redaction)),
        )?;
    }

    Ok(DiagnosticsExportResult {
        zip: archive.finish(),
        manifest,
        machine_info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::manifest::DiagnosticsAppInfo;
    use crate::diagnostics::sources::LogSourceKind;
    use std::path::PathBuf;

    /// Name/content pairs for an archive, via the module's own reader, so the
    /// assertions run against real archive bytes rather than the in-memory
    /// list that produced them.
    fn read_entries(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        read_zip_entries(zip)
            .expect("read archive")
            .into_iter()
            .map(|entry| (entry.name, entry.data))
            .collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-diag-zip-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn text_source(name: &str, path: PathBuf) -> LogSource {
        LogSource {
            name: name.to_string(),
            absolute_path: path,
            kind: LogSourceKind::Text,
            tail_bytes: None,
        }
    }

    fn read_json(entries: &[(String, Vec<u8>)], name: &str) -> Value {
        let (_, bytes) = entries
            .iter()
            .find(|(entry, _)| entry == name)
            .unwrap_or_else(|| panic!("{name} missing from {entries:?}"));
        serde_json::from_slice(bytes).expect("json entry")
    }

    #[test]
    fn packages_logs_with_a_redacted_manifest_and_machine_info() {
        let dir = temp_dir("basic");
        let log = dir.join("daemon.log");
        std::fs::write(&log, "GET /api?token=abc123 ok\n").unwrap();

        let result = build_diagnostics_zip(DiagnosticsExportInput {
            context: DiagnosticsContextInput {
                app: DiagnosticsAppInfo {
                    name: "open-design".into(),
                    version: Some("1.2.3".into()),
                    channel: None,
                    packaged: Some(false),
                },
                source: "test".into(),
                namespace: Some("default".into()),
                ..Default::default()
            },
            sources: vec![text_source("logs/daemon/latest.log", log)],
            redaction: RedactionOptions { username: Some("alice".into()) },
            ..Default::default()
        })
        .expect("zip");

        let entries = read_entries(&result.zip);
        let (_, log_bytes) = entries
            .iter()
            .find(|(name, _)| name == "logs/daemon/latest.log")
            .expect("log entry");
        let log_text = String::from_utf8(log_bytes.clone()).unwrap();
        assert!(log_text.contains("token=[REDACTED]"), "{log_text}");

        let manifest = read_json(&entries, "summary/manifest.json");
        assert_eq!(manifest["app"]["name"], serde_json::json!("open-design"));
        assert_eq!(manifest["namespace"], serde_json::json!("default"));
        assert_eq!(manifest["files"][0]["name"], serde_json::json!("logs/daemon/latest.log"));
        assert_eq!(manifest["warnings"], serde_json::json!([]));

        let machine = read_json(&entries, "summary/machine-info.json");
        assert!(machine["platform"].is_string());
        assert_eq!(machine["hostname"], serde_json::json!("[REDACTED]"));
        assert_eq!(machine["username"], serde_json::json!("[REDACTED]"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summaries_are_redacted_and_pretty_printed() {
        let result = build_diagnostics_zip(DiagnosticsExportInput {
            context: DiagnosticsContextInput {
                app: DiagnosticsAppInfo {
                    name: "open-design".into(),
                    ..Default::default()
                },
                source: "test".into(),
                ..Default::default()
            },
            summaries: vec![(
                "runtime-health.json".into(),
                serde_json::json!({
                    "amr": { "sessionState": "reauth_required", "credentialRevision": "safe-digest" },
                    "note": "/Users/alice/private",
                }),
            )],
            redaction: RedactionOptions { username: Some("alice".into()) },
            ..Default::default()
        })
        .expect("zip");

        let entries = read_entries(&result.zip);
        let (_, bytes) = entries
            .iter()
            .find(|(name, _)| name == "summary/runtime-health.json")
            .expect("summary entry");
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains("\"sessionState\": \"reauth_required\""), "{text}");
        assert!(text.contains("/Users/<USER>/private"), "{text}");
        assert!(!text.contains("/Users/alice"), "{text}");
    }

    #[test]
    fn a_source_name_with_path_separators_cannot_escape_the_summary_dir() {
        let result = build_diagnostics_zip(DiagnosticsExportInput {
            context: DiagnosticsContextInput {
                app: DiagnosticsAppInfo {
                    name: "open-design".into(),
                    ..Default::default()
                },
                source: "test".into(),
                ..Default::default()
            },
            summaries: vec![("a b/c.json".into(), serde_json::json!({ "ok": true }))],
            ..Default::default()
        })
        .expect("zip");
        let entries = read_entries(&result.zip);
        assert!(entries.iter().any(|(name, _)| name == "summary/a_b_c.json"));
    }

    #[test]
    fn an_unreadable_source_gets_a_placeholder_and_a_warning() {
        let dir = temp_dir("placeholder");
        let result = build_diagnostics_zip(DiagnosticsExportInput {
            context: DiagnosticsContextInput {
                app: DiagnosticsAppInfo {
                    name: "open-design".into(),
                    ..Default::default()
                },
                source: "test".into(),
                ..Default::default()
            },
            sources: vec![text_source("logs/missing.log", dir.join("no-such.log"))],
            ..Default::default()
        })
        .expect("zip");

        let entries = read_entries(&result.zip);
        let (_, bytes) = entries
            .iter()
            .find(|(name, _)| name == "logs/missing.log")
            .expect("placeholder entry");
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains("file unavailable"), "{text}");
        assert_eq!(result.manifest.warnings.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_dumps_reach_the_zip_intact() {
        let dir = temp_dir("dumps");
        const RAW: [u8; 11] = [0x4d, 0x44, 0x4d, 0x50, 0x00, 0xff, 0xfe, 0x80, 0x81, 0x00, 0x0a];
        std::fs::write(dir.join("renderer.dmp"), RAW).unwrap();

        let result = build_diagnostics_zip(DiagnosticsExportInput {
            context: DiagnosticsContextInput {
                app: DiagnosticsAppInfo {
                    name: "open-design".into(),
                    packaged: Some(true),
                    ..Default::default()
                },
                source: "test".into(),
                ..Default::default()
            },
            crash_dumps: Some(CrashDumpLookup {
                dir: dir.clone(),
                within_days: None,
                max_dumps: None,
            }),
            ..Default::default()
        })
        .expect("zip");

        let entries = read_entries(&result.zip);
        let (_, bytes) = entries
            .iter()
            .find(|(name, _)| name == "crash-dumps/renderer.dmp")
            .expect("dump entry");
        assert_eq!(bytes, &RAW.to_vec());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
