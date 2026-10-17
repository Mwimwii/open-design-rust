//! Log-source collection for the diagnostics bundle (parity:
//! `packages/diagnostics/src/sources.ts`).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::redaction::{redact_json_text, redact_text, RedactionOptions};

/// How a source is read: `json`/`text` decode as UTF-8 and are redacted;
/// `binary` is copied verbatim so an opaque artifact (a Chromium minidump)
/// reaches the bundle intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSourceKind {
    Json,
    Text,
    Binary,
}

/// A file to sweep into the export zip.
#[derive(Debug, Clone)]
pub struct LogSource {
    /// Relative path inside the export zip (forward slash).
    pub name: String,
    /// Absolute path on disk to read from.
    pub absolute_path: PathBuf,
    pub kind: LogSourceKind,
    /// Optional max bytes to read from the file tail; `None` reads it whole.
    pub tail_bytes: Option<u64>,
}

/// Contents to put into the zip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectedContent {
    Text(String),
    Binary(Vec<u8>),
}

/// What one source produced: redacted text, verbatim bytes, or an error.
#[derive(Debug, Clone)]
pub struct CollectedFile {
    pub name: String,
    pub absolute_path: String,
    pub content: Option<CollectedContent>,
    pub bytes: u64,
    /// Reason the file is missing or unreadable.
    pub error: Option<String>,
}

const NEWLINE_BYTE: u8 = 0x0a;

/// Drop the leading partial line of a tail window read from mid-file.
///
/// INVARIANT: every line in a truncated text source is a COMPLETE line. The
/// sources this matters most for are jsonl event logs — a byte-exact cut almost
/// always lands mid-object, so the bundle's first line would be half a JSON
/// value and any consumer streaming it through a jsonl parser would fall over.
///
/// `data` starts ONE byte before the intended cut (the guard byte): when that
/// byte is a newline the cut already begins a fresh line and only the guard
/// byte is dropped, so an aligned cut never loses a complete line.
///
/// `None` means NO complete line fits the window (a single record longer than
/// the whole tail budget); the caller surfaces that as an unreadable source
/// rather than exporting a fragment.
pub fn align_tail_to_line_start(data: &[u8]) -> Option<&[u8]> {
    if data.is_empty() {
        return Some(data);
    }
    if data[0] == NEWLINE_BYTE {
        return Some(&data[1..]);
    }
    let first_newline = data.iter().position(|byte| *byte == NEWLINE_BYTE)?;
    if first_newline + 1 >= data.len() {
        return None;
    }
    Some(&data[first_newline + 1..])
}

/// Read a whole file, or the aligned trailing window when `tail_bytes` is set.
///
/// Returns the decoded text and the byte count of what was actually read (the
/// ALIGNED length for a tail, matching the TypeScript `bytes` field).
fn read_maybe_tail(absolute_path: &Path, tail_bytes: Option<u64>) -> Result<(String, u64), String> {
    let tail = tail_bytes.filter(|value| *value > 0);
    let Some(tail) = tail else {
        let bytes = std::fs::read(absolute_path).map_err(|err| err.to_string())?;
        let len = bytes.len() as u64;
        return Ok((String::from_utf8_lossy(&bytes).into_owned(), len));
    };

    let info = std::fs::metadata(absolute_path).map_err(|err| err.to_string())?;
    let size = info.len();
    if size <= tail {
        let bytes = std::fs::read(absolute_path).map_err(|err| err.to_string())?;
        let len = bytes.len() as u64;
        return Ok((String::from_utf8_lossy(&bytes).into_owned(), len));
    }

    // Large files are not loaded whole just to slice off the tail; read the
    // trailing window straight off the descriptor.
    let mut file = File::open(absolute_path).map_err(|err| err.to_string())?;
    // One extra guard byte before the cut so `align_tail_to_line_start` can
    // tell an already line-aligned cut apart from a mid-line one.
    let start = size - tail - 1;
    file.seek(SeekFrom::Start(start)).map_err(|err| err.to_string())?;
    let mut buffer = vec![0u8; (tail + 1) as usize];
    let mut filled = 0usize;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) => return Err(err.to_string()),
        }
    }
    buffer.truncate(filled);
    let aligned = align_tail_to_line_start(&buffer).ok_or_else(|| {
        format!(
            "no complete line fits the {tail}-byte tail window (the trailing \
             record is larger than the cap); omitted rather than exporting a \
             partial record"
        )
    })?;
    let bytes = aligned.len() as u64;
    Ok((String::from_utf8_lossy(aligned).into_owned(), bytes))
}

/// Read one source, redacting text/json and copying binary verbatim.
pub fn collect_log_source(source: &LogSource, opts: &RedactionOptions) -> CollectedFile {
    let absolute_path = source.absolute_path.to_string_lossy().into_owned();
    let failure = |error: String| CollectedFile {
        name: source.name.clone(),
        absolute_path: absolute_path.clone(),
        content: None,
        bytes: 0,
        error: Some(error),
    };
    match source.kind {
        LogSourceKind::Binary => match std::fs::read(&source.absolute_path) {
            Ok(bytes) => {
                let len = bytes.len() as u64;
                CollectedFile {
                    name: source.name.clone(),
                    absolute_path,
                    content: Some(CollectedContent::Binary(bytes)),
                    bytes: len,
                    error: None,
                }
            }
            Err(err) => failure(err.to_string()),
        },
        LogSourceKind::Text | LogSourceKind::Json => {
            match read_maybe_tail(&source.absolute_path, source.tail_bytes) {
                Ok((text, bytes)) => {
                    let redacted = if source.kind == LogSourceKind::Json {
                        redact_json_text(&text, opts)
                    } else {
                        redact_text(&text, opts)
                    };
                    CollectedFile {
                        name: source.name.clone(),
                        absolute_path,
                        content: Some(CollectedContent::Text(redacted)),
                        bytes,
                        error: None,
                    }
                }
                Err(err) => failure(err),
            }
        }
    }
}

pub fn collect_log_sources(sources: &[LogSource], opts: &RedactionOptions) -> Vec<CollectedFile> {
    sources.iter().map(|source| collect_log_source(source, opts)).collect()
}

const DEFAULT_CRASH_DIRS_DARWIN: &[&str] = &["/Library/Logs/DiagnosticReports"];

/// Which macOS diagnostic reports to sweep (parity: `CrashReportLookup`).
#[derive(Debug, Clone, Default)]
pub struct CrashReportLookup {
    /// Filenames must contain at least one of these substrings (case-insensitive).
    pub match_substrings: Vec<String>,
    pub within_days: Option<u64>,
    pub max_reports: Option<usize>,
    /// Override base directories to scan.
    pub search_dirs: Option<Vec<PathBuf>>,
    /// Home directory to derive `~/Library/Logs/DiagnosticReports` from.
    pub home_dir: Option<PathBuf>,
}

/// macOS crash reports matching the lookup, newest first. On every other
/// platform this is empty, exactly like the TypeScript guard.
pub fn find_macos_crash_reports(lookup: &CrashReportLookup) -> Vec<LogSource> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs((lookup.within_days.unwrap_or(7)) * 24 * 60 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let max = lookup.max_reports.unwrap_or(20);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(search) = &lookup.search_dirs {
        dirs.extend(search.iter().cloned());
    } else {
        if let Some(home) = &lookup.home_dir {
            dirs.push(home.join("Library/Logs/DiagnosticReports"));
        }
        dirs.extend(DEFAULT_CRASH_DIRS_DARWIN.iter().map(PathBuf::from));
    }
    let needles: Vec<String> = lookup
        .match_substrings
        .iter()
        .map(|entry| entry.to_lowercase())
        .collect();

    let mut found: Vec<(PathBuf, SystemTime, String)> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_lowercase();
            if !needles.iter().any(|needle| lower.contains(needle)) {
                continue;
            }
            let absolute_path = dir.join(&name);
            let Ok(info) = std::fs::metadata(&absolute_path) else {
                continue;
            };
            if !info.is_file() {
                continue;
            }
            let Ok(mtime) = info.modified() else {
                continue;
            };
            if mtime < cutoff {
                continue;
            }
            found.push((absolute_path, mtime, name));
        }
    }
    found.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    found
        .into_iter()
        .take(max)
        .map(|(absolute_path, _, name)| LogSource {
            name: format!("crash-reports/{name}"),
            absolute_path,
            kind: LogSourceKind::Text,
            tail_bytes: None,
        })
        .collect()
}

/// Which crash minidumps to sweep (parity: `CrashDumpLookup`).
#[derive(Debug, Clone)]
pub struct CrashDumpLookup {
    /// Root crashDumps directory (Electron's `app.getPath('crashDumps')`).
    pub dir: PathBuf,
    pub within_days: Option<u64>,
    pub max_dumps: Option<usize>,
}

/// Collect Chromium/Electron crash minidumps as BINARY sources from the root
/// dir plus `completed/`, `pending/` and `reports/`, newest first.
pub fn find_crash_dumps(lookup: &CrashDumpLookup) -> Vec<LogSource> {
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(lookup.within_days.unwrap_or(14) * 24 * 60 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let max = lookup.max_dumps.unwrap_or(10);
    let dirs = [
        lookup.dir.clone(),
        lookup.dir.join("completed"),
        lookup.dir.join("pending"),
        lookup.dir.join("reports"),
    ];
    let mut found: Vec<(PathBuf, SystemTime, String)> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().ends_with(".dmp") {
                continue;
            }
            let absolute_path = dir.join(&name);
            let Ok(info) = std::fs::metadata(&absolute_path) else {
                continue;
            };
            if !info.is_file() {
                continue;
            }
            let Ok(mtime) = info.modified() else {
                continue;
            };
            if mtime < cutoff {
                continue;
            }
            found.push((absolute_path, mtime, name));
        }
    }
    found.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    found
        .into_iter()
        .take(max)
        .map(|(absolute_path, _, name)| LogSource {
            name: format!("crash-dumps/{name}"),
            absolute_path,
            kind: LogSourceKind::Binary,
            tail_bytes: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Bytes that do NOT round-trip through UTF-8 decode/encode, so a
    /// corrupting text path (decode → redact → re-encode) is detectable.
    const RAW_DUMP_BYTES: [u8; 11] = [
        0x4d, 0x44, 0x4d, 0x50, 0x00, 0xff, 0xfe, 0x80, 0x81, 0x00, 0x0a,
    ];

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-diag-sources-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
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

    #[test]
    fn aligns_a_tail_window_to_a_line_start() {
        // Guard byte is a newline: only the guard byte is dropped.
        assert_eq!(align_tail_to_line_start(b"\nfull line\n"), Some(&b"full line\n"[..]));
        // Mid-line cut: the first partial line is dropped.
        assert_eq!(align_tail_to_line_start(b"partial\nfull\n"), Some(&b"full\n"[..]));
        // No complete line fits.
        assert_eq!(align_tail_to_line_start(b"partial"), None);
        assert_eq!(align_tail_to_line_start(b"partial\n"), None);
        assert_eq!(align_tail_to_line_start(b""), Some(&b""[..]));
    }

    #[test]
    fn collects_text_and_redacts_it() {
        let dir = temp_dir("text");
        let path = dir.join("daemon.log");
        fs::write(&path, "GET /api?token=abc123 ok\n").unwrap();
        let collected = collect_log_source(&text_source("logs/daemon/latest.log", path), &RedactionOptions::default());
        assert_eq!(collected.name, "logs/daemon/latest.log");
        assert!(collected.error.is_none());
        match collected.content {
            Some(CollectedContent::Text(text)) => assert!(text.contains("token=[REDACTED]"), "{text}"),
            other => panic!("unexpected content: {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_only_the_aligned_tail_window() {
        let dir = temp_dir("tail");
        let path = dir.join("big.log");
        fs::write(&path, "aaaa\nbbbb\ncccc\n").unwrap();
        let source = LogSource {
            name: "logs/big.log".into(),
            absolute_path: path,
            kind: LogSourceKind::Text,
            tail_bytes: Some(11),
        };
        let collected = collect_log_source(&source, &RedactionOptions::default());
        match collected.content {
            // The 12-byte window (tail + guard) starts one byte before the cut
            // and lands mid-line, so aligning drops that partial line and keeps
            // the two complete ones.
            Some(CollectedContent::Text(text)) => assert_eq!(text, "bbbb\ncccc\n"),
            other => panic!("unexpected content: {other:?}"),
        }
        assert_eq!(collected.bytes, 10);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_missing_files_as_unreadable_with_zero_bytes() {
        let dir = temp_dir("missing");
        let collected = collect_log_source(
            &text_source("logs/missing.log", dir.join("no-such.log")),
            &RedactionOptions::default(),
        );
        assert!(collected.content.is_none());
        assert_eq!(collected.bytes, 0);
        assert!(collected.error.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_sources_are_copied_verbatim() {
        let dir = temp_dir("binary");
        let path = dir.join("crash.dmp");
        fs::write(&path, RAW_DUMP_BYTES).unwrap();
        let collected = collect_log_source(
            &LogSource {
                name: "crash-dumps/crash.dmp".into(),
                absolute_path: path,
                kind: LogSourceKind::Binary,
                tail_bytes: None,
            },
            &RedactionOptions::default(),
        );
        match collected.content {
            Some(CollectedContent::Binary(bytes)) => assert_eq!(bytes, RAW_DUMP_BYTES.to_vec()),
            other => panic!("unexpected content: {other:?}"),
        }
        assert_eq!(collected.bytes, RAW_DUMP_BYTES.len() as u64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_dumps_come_from_the_root_and_completed_dir() {
        let dir = temp_dir("dumps");
        fs::write(dir.join("a.dmp"), RAW_DUMP_BYTES).unwrap();
        fs::create_dir_all(dir.join("completed")).unwrap();
        fs::write(dir.join("completed").join("b.dmp"), RAW_DUMP_BYTES).unwrap();
        fs::write(dir.join("notes.txt"), "not a dump").unwrap();
        let mut names: Vec<String> = find_crash_dumps(&CrashDumpLookup {
            dir: dir.clone(),
            within_days: None,
            max_dumps: None,
        })
        .into_iter()
        .map(|source| source.name)
        .collect();
        names.sort();
        assert_eq!(names, vec!["crash-dumps/a.dmp", "crash-dumps/b.dmp"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_crash_dump_dir_is_empty_not_an_error() {
        let dir = temp_dir("dumps-missing");
        assert!(find_crash_dumps(&CrashDumpLookup {
            dir: dir.join("does-not-exist"),
            within_days: None,
            max_dumps: None,
        })
        .is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_dumps_are_capped_at_max_dumps() {
        let dir = temp_dir("dumps-cap");
        for index in 0..5 {
            fs::write(dir.join(format!("d{index}.dmp")), RAW_DUMP_BYTES).unwrap();
        }
        let sources = find_crash_dumps(&CrashDumpLookup {
            dir: dir.clone(),
            within_days: None,
            max_dumps: Some(2),
        });
        assert_eq!(sources.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
