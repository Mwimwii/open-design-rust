//! Project file listing/reading — parity ports of `listFiles`,
//! `readProjectFile`, `resolveProjectFilePath`, and the `text-preview` route
//! from `apps/daemon/src/projects.ts` and
//! `apps/daemon/src/routes/project/index.ts`.
//!
//! Listing mirrors the TypeScript walk exactly: dotfiles, generated/installed
//! trees, `.artifact.json` sidecars, and symlinks never appear; entries sort
//! newest-first by mtime.

use std::cmp::Ordering;
use std::io::{self, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::project_dir::{
    assert_visible_for_imported_project, map_io, resolve_under, validate_project_path,
    ProjectPathError, ResolvedFile,
};

/// Parity: `listFiles` `since` filter and the `text-preview` route clamps.
pub const MIN_TEXT_PREVIEW_LIMIT: u64 = 1024;
pub const MAX_TEXT_PREVIEW_LIMIT: u64 = 512 * 1024;
pub const DEFAULT_TEXT_PREVIEW_LIMIT: u64 = 96 * 1024;

/// Parity: `HTML_POWERED_PREVIEW_HINT_SCAN_MAX_BYTES`.
const HTML_POWERED_PREVIEW_HINT_SCAN_MAX_BYTES: u64 = 128 * 1024 * 1024;
const POWERED_PREVIEW_CHUNK_BYTES: usize = 256 * 1024;
const POWERED_PREVIEW_TAIL_BYTES: usize = 512;

/// Parity: `IGNORED_PROJECT_DIR_NAMES` (+ the `deriveddata-` prefix rule) in
/// `apps/daemon/src/project-ignored-dirs.ts`.
const IGNORED_PROJECT_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "vendor",
    ".od",
    "debug",
    "dist",
    "build",
    ".build",
    "deriveddata",
    "target",
    ".next",
    ".nuxt",
    ".turbo",
    ".cache",
    ".output",
    "out",
    "coverage",
    ".gradle",
    ".swiftpm",
    ".tmp",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    ".ruff_cache",
];

/// Parity: `EXT_MIME` in `apps/daemon/src/projects.ts`.
const EXT_MIME: &[(&str, &str)] = &[
    (".html", "text/html; charset=utf-8"),
    (".htm", "text/html; charset=utf-8"),
    (".css", "text/css; charset=utf-8"),
    (".js", "text/javascript; charset=utf-8"),
    (".mjs", "text/javascript; charset=utf-8"),
    (".cjs", "text/javascript; charset=utf-8"),
    (".jsx", "text/javascript; charset=utf-8"),
    (".ts", "text/typescript; charset=utf-8"),
    (".py", "text/x-python; charset=utf-8"),
    (".tsx", "text/javascript; charset=utf-8"),
    (".json", "application/json; charset=utf-8"),
    (".md", "text/markdown; charset=utf-8"),
    (".txt", "text/plain; charset=utf-8"),
    (".pdf", "application/pdf"),
    (
        ".docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        ".pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    (
        ".xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    (".svg", "image/svg+xml"),
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
    (".avif", "image/avif"),
    (".mp4", "video/mp4"),
    (".mov", "video/quicktime"),
    (".webm", "video/webm"),
    (".mp3", "audio/mpeg"),
    (".wav", "audio/wav"),
    (".m4a", "audio/mp4"),
];

/// Parity: artifact-manifest validation constants in
/// `apps/daemon/src/artifacts/manifest.ts`.
const MANIFEST_VERSION: i64 = 1;
const ALLOWED_MANIFEST_KINDS: &[&str] = &[
    "html",
    "deck",
    "react-component",
    "markdown-document",
    "svg",
    "diagram",
    "code-snippet",
    "mini-app",
    "design-system",
];
const ALLOWED_MANIFEST_RENDERERS: &[&str] = &[
    "html",
    "deck-html",
    "react-component",
    "markdown",
    "svg",
    "diagram",
    "code",
    "mini-app",
    "design-system",
];
const ALLOWED_MANIFEST_EXPORTS: &[&str] = &["html", "pdf", "zip", "jsx", "md", "svg", "txt"];
const ALLOWED_MANIFEST_STATUS: &[&str] = &["streaming", "complete", "error"];
const MAX_MANIFEST_TITLE_LENGTH: usize = 200;
const MAX_MANIFEST_PATH_LENGTH: usize = 260;
const MAX_MANIFEST_SUPPORTING_FILES: usize = 128;
const MAX_MANIFEST_METADATA_BYTES: usize = 16 * 1024;
const MAX_MANIFEST_SOURCE_SKILL_ID_LENGTH: usize = 128;
const MAX_MANIFEST_DESIGN_SYSTEM_ID_LENGTH: usize = 128;

struct ListedFile {
    entry: Value,
    mtime: f64,
}

/// Port of `listFiles`: walk the project directory, drop invisible entries,
/// sort newest-first, then apply the optional `since` cutoff.
pub fn list_files(base: &Path, since: Option<f64>) -> io::Result<Vec<Value>> {
    let mut listed = Vec::new();
    collect_files(base, "", base, &mut listed)?;
    listed.sort_by(|left, right| right.mtime.partial_cmp(&left.mtime).unwrap_or(Ordering::Equal));
    if let Some(since) = since {
        listed.retain(|file| file.mtime > since);
    }
    Ok(listed.into_iter().map(|file| file.entry).collect())
}

/// Port of `readProjectFile`: resolve inside the project sandbox, read the
/// bytes, and derive the MIME from the canonical (symlink-resolved) name.
pub fn read_project_file(
    base: &Path,
    rel: &str,
    imported: bool,
) -> Result<(Vec<u8>, String), ProjectPathError> {
    let resolved = resolve_project_file(base, rel, imported)?;
    let bytes = std::fs::read(&resolved.path).map_err(map_io)?;
    Ok((bytes, mime_for(&resolved.rel).to_string()))
}

/// Port of the `text-preview` route: bounded UTF-8 preview plus file metadata
/// and the HTML "powered preview" capability hint.
pub fn text_preview(
    base: &Path,
    rel: &str,
    imported: bool,
    limit: u64,
) -> Result<Value, ProjectPathError> {
    let resolved = resolve_project_file(base, rel, imported)?;
    let metadata = std::fs::metadata(&resolved.path).map_err(map_io)?;
    let size = metadata.len();
    let mime = mime_for(&resolved.rel);
    let kind = kind_for(&resolved.rel);
    let bytes = read_prefix(&resolved.path, size.min(limit)).map_err(map_io)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let truncated = size > bytes.len() as u64;
    let powered_preview = detect_powered_preview_hint(&resolved.path, mime, size)?;
    Ok(json!({
        "text": text,
        "truncated": truncated,
        "size": size,
        "limit": limit,
        "mime": mime,
        "kind": kind,
        "poweredPreview": powered_preview,
    }))
}

/// Parity: `rejectInternalVersionPath` — the internal version store is never
/// reachable through the public file routes.
pub fn is_project_file_version_path(raw: &str) -> bool {
    let normalized = raw.replace('\\', "/");
    normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .any(|segment| segment == ".file-versions")
}

/// Parity: the shared validate → confine sequence every read funnels through.
pub fn resolve_project_file(base: &Path, rel: &str, imported: bool) -> Result<ResolvedFile, ProjectPathError> {
    assert_visible_for_imported_project(rel, imported)?;
    let validated = validate_project_path(rel)?;
    resolve_under(base, &validated)
}

pub fn is_ignored_project_dir_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase();
    IGNORED_PROJECT_DIR_NAMES.contains(&normalized.as_str())
        || normalized.starts_with("deriveddata-")
}

/// Parity: `mimeFor`.
pub fn mime_for(name: &str) -> &'static str {
    let ext = extension_of(name);
    EXT_MIME
        .iter()
        .find(|(candidate, _)| *candidate == ext)
        .map(|(_, mime)| *mime)
        .unwrap_or("application/octet-stream")
}

/// Parity: `kindFor`.
pub fn kind_for(name: &str) -> &'static str {
    if name.ends_with(".sketch.json") {
        return "sketch";
    }
    match extension_of(name).as_str() {
        ".html" | ".htm" => "html",
        ".svg" => "sketch",
        ".png" | ".jpg" | ".jpeg" | ".gif" | ".webp" | ".avif" => {
            if name.starts_with("sketch-") {
                "sketch"
            } else {
                "image"
            }
        }
        ".mp4" | ".mov" | ".webm" => "video",
        ".mp3" | ".wav" | ".m4a" => "audio",
        ".md" | ".txt" => "text",
        ".js" | ".mjs" | ".cjs" | ".ts" | ".tsx" | ".json" | ".css" | ".py" => "code",
        ".pdf" => "pdf",
        ".docx" => "document",
        ".pptx" => "presentation",
        ".xlsx" => "spreadsheet",
        _ => "binary",
    }
}

/// `path.extname` semantics: extension of the last path segment, empty for
/// dotfiles with no further dot.
fn extension_of(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rfind('.') {
        Some(index) if index > 0 => base[index..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn collect_files(dir: &Path, rel_dir: &str, project_root: &Path, out: &mut Vec<ListedFile>) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let rel = if rel_dir.is_empty() {
            name.to_string()
        } else {
            format!("{rel_dir}/{name}")
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if file_type.is_dir() {
            if is_ignored_project_dir_name(&name) {
                continue;
            }
            collect_files(&dir.join(name.as_ref()), &rel, project_root, out)?;
            continue;
        }
        // Parity: `Dirent.isFile()` — symlinks (and anything else) are skipped.
        if !file_type.is_file() {
            continue;
        }
        if name.ends_with(".artifact.json") {
            continue;
        }
        let full = dir.join(name.as_ref());
        let metadata = match std::fs::metadata(&full) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let mtime = system_time_ms(modified);
        let manifest = read_manifest_for_path(project_root, &rel);
        out.push(ListedFile {
            entry: file_entry(&rel, project_root, &metadata, mtime, manifest),
            mtime,
        });
    }
    Ok(())
}

fn file_entry(
    rel: &str,
    project_root: &Path,
    metadata: &std::fs::Metadata,
    mtime: f64,
    manifest: Option<Value>,
) -> Value {
    let mut entry = Map::new();
    entry.insert("name".to_string(), json!(rel));
    entry.insert("path".to_string(), json!(rel));
    entry.insert(
        "localPath".to_string(),
        json!(project_root.join(rel).to_string_lossy()),
    );
    entry.insert("type".to_string(), json!("file"));
    entry.insert("size".to_string(), json!(metadata.len()));
    entry.insert("mtime".to_string(), json!(mtime));
    entry.insert("kind".to_string(), json!(kind_for(rel)));
    entry.insert("mime".to_string(), json!(mime_for(rel)));
    if let Some(manifest) = &manifest {
        if let Some(kind) = manifest.get("kind") {
            entry.insert("artifactKind".to_string(), kind.clone());
        }
        entry.insert("artifactManifest".to_string(), manifest.clone());
    }
    Value::Object(entry)
}

fn system_time_ms(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64() * 1000.0,
        Err(err) => -err.duration().as_secs_f64() * 1000.0,
    }
}

fn read_prefix(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = Vec::new();
    if limit > 0 {
        (&mut file).take(limit).read_to_end(&mut buffer)?;
    }
    Ok(buffer)
}

// --- artifact manifests (list entries only) ---------------------------------

/// Parity: `readManifestForPath` — sidecar first, legacy inference fallback,
/// `null` for ignored subtrees and Vite dev entry HTML.
fn read_manifest_for_path(project_root: &Path, rel: &str) -> Option<Value> {
    if rel
        .split('/')
        .filter(|segment| !segment.is_empty())
        .any(is_ignored_project_dir_name)
    {
        return None;
    }
    if is_vite_dev_html_entry(project_root, rel) {
        return None;
    }
    if let Ok(raw) = std::fs::read_to_string(project_root.join(format!("{rel}.artifact.json"))) {
        if let Some(manifest) = parse_sidecar_manifest(&raw, rel) {
            return Some(manifest);
        }
    }
    infer_legacy_manifest(rel)
}

/// Parity: `inferLegacyManifest`.
fn infer_legacy_manifest(entry: &str) -> Option<Value> {
    let lower = entry.to_ascii_lowercase();
    let exports = match extension_of(&lower).as_str() {
        ".html" | ".htm" => {
            let is_deck = lower.contains("deck") || lower.contains("slides") || lower.contains("pitch");
            let kind = if is_deck { "deck" } else { "html" };
            let renderer = if is_deck { "deck-html" } else { "html" };
            json!({
                "version": MANIFEST_VERSION,
                "kind": kind,
                "title": entry,
                "entry": entry,
                "renderer": renderer,
                "status": "complete",
                "exports": ["html", "pdf", "zip"],
                "metadata": { "inferred": true },
            })
        }
        ".md" => json!({
            "version": MANIFEST_VERSION,
            "kind": "markdown-document",
            "title": entry,
            "entry": entry,
            "renderer": "markdown",
            "status": "complete",
            "exports": ["md", "html", "pdf", "zip"],
            "metadata": { "inferred": true },
        }),
        ".svg" => json!({
            "version": MANIFEST_VERSION,
            "kind": "svg",
            "title": entry,
            "entry": entry,
            "renderer": "svg",
            "status": "complete",
            "exports": ["svg", "zip"],
            "metadata": { "inferred": true },
        }),
        _ => return None,
    };
    Some(exports)
}

/// Parity: `parsePersistedManifest` + `validateArtifactManifestInput`. A
/// daemon-written sidecar already carries the sanitized shape, so a validated
/// pass-through is byte-compatible for every manifest the daemon produces.
fn parse_sidecar_manifest(raw: &str, entry_fallback: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let object = parsed.as_object()?;
    if object.get("version")?.as_i64()? != MANIFEST_VERSION {
        return None;
    }
    let kind = object.get("kind")?.as_str()?;
    if !ALLOWED_MANIFEST_KINDS.contains(&kind) {
        return None;
    }
    let renderer = object.get("renderer")?.as_str()?;
    if !ALLOWED_MANIFEST_RENDERERS.contains(&renderer) {
        return None;
    }
    let exports = object.get("exports")?.as_array()?;
    if exports.is_empty() {
        return None;
    }
    for export in exports {
        if !ALLOWED_MANIFEST_EXPORTS.contains(&export.as_str()?) {
            return None;
        }
    }
    if let Some(status) = object.get("status") {
        if !ALLOWED_MANIFEST_STATUS.contains(&status.as_str()?) {
            return None;
        }
    }
    let entry = object
        .get("entry")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(entry_fallback);
    if !is_valid_manifest_path(entry) {
        return None;
    }
    if let Some(primary) = object.get("primary") {
        let valid = primary.as_bool() == Some(true)
            || primary.as_str().is_some_and(is_valid_manifest_path);
        if !valid {
            return None;
        }
    }
    if let Some(supporting) = object.get("supportingFiles") {
        let files = supporting.as_array()?;
        if files.len() > MAX_MANIFEST_SUPPORTING_FILES {
            return None;
        }
        for file in files {
            if !file.as_str().is_some_and(is_valid_manifest_path) {
                return None;
            }
        }
    }
    if let Some(title) = object.get("title") {
        let title = title.as_str()?;
        if title.is_empty() || title.len() > MAX_MANIFEST_TITLE_LENGTH {
            return None;
        }
    }
    if let Some(source_skill_id) = object.get("sourceSkillId") {
        let value = source_skill_id.as_str()?;
        if value.len() > MAX_MANIFEST_SOURCE_SKILL_ID_LENGTH {
            return None;
        }
    }
    if let Some(design_system_id) = object.get("designSystemId") {
        if !design_system_id.is_null() {
            let value = design_system_id.as_str()?;
            if value.len() > MAX_MANIFEST_DESIGN_SYSTEM_ID_LENGTH {
                return None;
            }
        }
    }
    if let Some(metadata) = object.get("metadata") {
        if !metadata.is_object() {
            return None;
        }
        let serialized = serde_json::to_string(metadata).ok()?;
        if serialized.len() > MAX_MANIFEST_METADATA_BYTES {
            return None;
        }
    }
    Some(parsed)
}

/// Parity: `validateSupportingPath`.
fn is_valid_manifest_path(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_MANIFEST_PATH_LENGTH || value.contains('\0') {
        return false;
    }
    let normalized = value.replace('\\', "/");
    if normalized.starts_with('/') {
        return false;
    }
    let drive_qualified = normalized
        .as_bytes()
        .first()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && normalized.as_bytes().get(1) == Some(&b':');
    if drive_qualified || normalized.contains("..") {
        return false;
    }
    normalized.split('/').any(|part| !part.is_empty())
}

/// Parity: `isViteDevHtmlEntry` — Vite dev-entry HTML never carries an
/// artifact manifest.
fn is_vite_dev_html_entry(project_dir_path: &Path, safe_name: &str) -> bool {
    if !is_index_html_name(safe_name) {
        return false;
    }
    let Ok(body) = std::fs::read(project_dir_path.join(safe_name)) else {
        return false;
    };
    if !has_vite_dev_module_script(&body) {
        return false;
    }
    for candidate in [
        "vite.config.js",
        "vite.config.mjs",
        "vite.config.cjs",
        "vite.config.ts",
        "vite.config.mts",
        "vite.config.cts",
    ] {
        match std::fs::metadata(project_dir_path.join(candidate)) {
            Ok(metadata) => {
                if metadata.is_file() {
                    return true;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    let Ok(raw) = std::fs::read_to_string(project_dir_path.join("package.json")) else {
        return false;
    };
    let Ok(package) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    json_truthy(package.get("dependencies").and_then(|deps| deps.get("vite")))
        || json_truthy(package.get("devDependencies").and_then(|deps| deps.get("vite")))
}

fn is_index_html_name(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    ["index.html", "index.htm"]
        .iter()
        .any(|suffix| match lower.strip_suffix(suffix) {
            Some(prefix) => prefix.is_empty() || prefix.ends_with('/'),
            None => false,
        })
}

/// JS truthiness for the `package.json` vite dependency probe.
fn json_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(values)) => !values.is_empty(),
    }
}

// --- "powered preview" capability hint --------------------------------------

/// Parity: `detectPoweredPreviewHint`. Scans HTML in 256 KiB chunks (keeping
/// the last 512 bytes as carry) for cross-origin-isolation signals.
fn detect_powered_preview_hint(
    path: &Path,
    mime: &str,
    size: u64,
) -> Result<Value, ProjectPathError> {
    if !mime_is_html(mime) {
        return Ok(json!({"required": false, "scannedBytes": 0, "complete": true}));
    }
    let scan_limit = size.min(HTML_POWERED_PREVIEW_HINT_SCAN_MAX_BYTES);
    let mut file = std::fs::File::open(path).map_err(map_io)?;
    let mut scanned: u64 = 0;
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = vec![0_u8; POWERED_PREVIEW_CHUNK_BYTES];
    while scanned < scan_limit {
        let want = (scan_limit - scanned).min(POWERED_PREVIEW_CHUNK_BYTES as u64) as usize;
        let read = file.read(&mut chunk[..want]).map_err(map_io)?;
        if read == 0 {
            break;
        }
        scanned += read as u64;
        let mut sample = Vec::with_capacity(tail.len() + read);
        sample.extend_from_slice(&tail);
        sample.extend_from_slice(&chunk[..read]);
        if html_has_powered_preview_signal(&sample) {
            return Ok(json!({
                "required": true,
                "scannedBytes": scanned,
                "complete": scanned >= size,
            }));
        }
        let keep = sample.len().min(POWERED_PREVIEW_TAIL_BYTES);
        tail = sample[sample.len() - keep..].to_vec();
    }
    Ok(json!({
        "required": false,
        "scannedBytes": scanned,
        "complete": scanned >= size,
    }))
}

fn mime_is_html(mime: &str) -> bool {
    mime.strip_prefix("text/html")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(';'))
}

/// Parity: `htmlHasPoweredPreviewSignal`, hand-compiled (no regex crate in
/// the workspace dependency set).
fn html_has_powered_preview_signal(source: &[u8]) -> bool {
    has_word_literal(source, b"SharedArrayBuffer")
        || has_new_call(source, b"Worker")
        || has_new_call(source, b"SharedWorker")
        || has_word_call(source, b"importScripts")
        || has_webassembly_stream(source)
        || has_dot_wasm(source)
        || has_getcontext_webgl2(source)
        || has_word_literal(source, b"OffscreenCanvas")
        || has_navigator_gpu(source)
}

/// `\bnew\s+<name>\s*\(`
fn has_new_call(source: &[u8], name: &[u8]) -> bool {
    let mut from = 0;
    while let Some(index) = find_literal(source, b"new", from, true) {
        from = index + 1;
        if !boundary_before(source, index) {
            continue;
        }
        let mut cursor = index + 3;
        let whitespace_start = cursor;
        while cursor < source.len() && is_js_space(source[cursor]) {
            cursor += 1;
        }
        if cursor == whitespace_start || !matches_literal(source, cursor, name, true) {
            continue;
        }
        let mut after = cursor + name.len();
        while after < source.len() && is_js_space(source[after]) {
            after += 1;
        }
        if source.get(after) == Some(&b'(') {
            return true;
        }
    }
    false
}

/// `\b<word>\s*\(`
fn has_word_call(source: &[u8], word: &[u8]) -> bool {
    let mut from = 0;
    while let Some(index) = find_literal(source, word, from, true) {
        from = index + 1;
        if !boundary_before(source, index) {
            continue;
        }
        let mut cursor = index + word.len();
        while cursor < source.len() && is_js_space(source[cursor]) {
            cursor += 1;
        }
        if source.get(cursor) == Some(&b'(') {
            return true;
        }
    }
    false
}

/// `\bWebAssembly\s*\.\s*(instantiateStreaming|compileStreaming)\b`
fn has_webassembly_stream(source: &[u8]) -> bool {
    let mut from = 0;
    while let Some(index) = find_literal(source, b"WebAssembly", from, true) {
        from = index + 1;
        if !boundary_before(source, index) {
            continue;
        }
        let mut cursor = index + 11;
        while cursor < source.len() && is_js_space(source[cursor]) {
            cursor += 1;
        }
        if source.get(cursor) != Some(&b'.') {
            continue;
        }
        cursor += 1;
        while cursor < source.len() && is_js_space(source[cursor]) {
            cursor += 1;
        }
        for name in [b"instantiateStreaming".as_slice(), b"compileStreaming".as_slice()] {
            if matches_literal(source, cursor, name, true)
                && boundary_after(source, cursor + name.len())
            {
                return true;
            }
        }
    }
    false
}

/// `\.wasm\b`
fn has_dot_wasm(source: &[u8]) -> bool {
    let mut from = 0;
    while let Some(index) = find_literal(source, b".wasm", from, true) {
        from = index + 1;
        if boundary_after(source, index + 5) {
            return true;
        }
    }
    false
}

/// `getContext\s*\(\s*["'\`]webgl2["'\`]`
fn has_getcontext_webgl2(source: &[u8]) -> bool {
    let mut from = 0;
    while let Some(index) = find_literal(source, b"getContext", from, true) {
        from = index + 1;
        let mut cursor = index + 10;
        while cursor < source.len() && is_js_space(source[cursor]) {
            cursor += 1;
        }
        if source.get(cursor) != Some(&b'(') {
            continue;
        }
        cursor += 1;
        while cursor < source.len() && is_js_space(source[cursor]) {
            cursor += 1;
        }
        if !source.get(cursor).copied().is_some_and(is_quote) {
            continue;
        }
        cursor += 1;
        if !matches_literal(source, cursor, b"webgl2", true) {
            continue;
        }
        cursor += 6;
        if source.get(cursor).copied().is_some_and(is_quote) {
            return true;
        }
    }
    false
}

/// `\bnavigator\s*\.\s*gpu\b`
fn has_navigator_gpu(source: &[u8]) -> bool {
    let mut from = 0;
    while let Some(index) = find_literal(source, b"navigator", from, true) {
        from = index + 1;
        if !boundary_before(source, index) {
            continue;
        }
        let mut cursor = index + 9;
        while cursor < source.len() && is_js_space(source[cursor]) {
            cursor += 1;
        }
        if source.get(cursor) != Some(&b'.') {
            continue;
        }
        cursor += 1;
        while cursor < source.len() && is_js_space(source[cursor]) {
            cursor += 1;
        }
        if matches_literal(source, cursor, b"gpu", true) && boundary_after(source, cursor + 3) {
            return true;
        }
    }
    false
}

/// `/<script\b[^>]*\btype=["']module["'][^>]*\bsrc=["']\/src\//i`
fn has_vite_dev_module_script(body: &[u8]) -> bool {
    let mut from = 0;
    while let Some(index) = find_literal(body, b"<script", from, true) {
        from = index + 1;
        if !boundary_after(body, index + 7) {
            continue;
        }
        if module_script_with_src(body, index + 7) {
            return true;
        }
    }
    false
}

fn module_script_with_src(body: &[u8], from: usize) -> bool {
    let mut index = from;
    while index < body.len() && body[index] != b'>' {
        if boundary_before(body, index)
            && matches_literal(body, index, b"type=", true)
            && body.get(index + 5).copied().is_some_and(is_quote)
        {
            let module_start = index + 6;
            if matches_literal(body, module_start, b"module", true)
                && body.get(module_start + 6).copied().is_some_and(is_quote)
                && src_attribute_matches(body, module_start + 7)
            {
                return true;
            }
        }
        index += 1;
    }
    false
}

/// `[^>]*\bsrc=["']\/src\/`
fn src_attribute_matches(body: &[u8], from: usize) -> bool {
    let mut index = from;
    while index < body.len() && body[index] != b'>' {
        if boundary_before(body, index)
            && matches_literal(body, index, b"src=", true)
            && body.get(index + 4).copied().is_some_and(is_quote)
            && matches_literal(body, index + 5, b"/src/", true)
        {
            return true;
        }
        index += 1;
    }
    false
}

// --- tiny byte-pattern helpers (stand-ins for the JS regexes) ---------------

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_js_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_quote(byte: u8) -> bool {
    matches!(byte, b'\'' | b'"' | b'`')
}

fn boundary_before(source: &[u8], index: usize) -> bool {
    index == 0 || !is_word_byte(source[index - 1])
}

fn boundary_after(source: &[u8], index: usize) -> bool {
    index >= source.len() || !is_word_byte(source[index])
}

fn matches_literal(haystack: &[u8], at: usize, needle: &[u8], case_insensitive: bool) -> bool {
    haystack
        .get(at..at + needle.len())
        .is_some_and(|slice| {
            slice
                .iter()
                .zip(needle)
                .all(|(left, right)| {
                    if case_insensitive {
                        left.eq_ignore_ascii_case(right)
                    } else {
                        left == right
                    }
                })
        })
}

fn find_literal(haystack: &[u8], needle: &[u8], from: usize, case_insensitive: bool) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let last = haystack.len() - needle.len();
    (from..=last).find(|&index| matches_literal(haystack, index, needle, case_insensitive))
}

fn has_word_literal(source: &[u8], word: &[u8]) -> bool {
    let mut from = 0;
    while let Some(index) = find_literal(source, word, from, true) {
        from = index + 1;
        if boundary_before(source, index) && boundary_after(source, index + word.len()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-project-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn mime_and_kind_match_typescript_tables() {
        assert_eq!(mime_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(mime_for("a/b/notes.md"), "text/markdown; charset=utf-8");
        assert_eq!(mime_for("photo.png"), "image/png");
        assert_eq!(mime_for("archive.bin"), "application/octet-stream");
        assert_eq!(kind_for("index.html"), "html");
        assert_eq!(kind_for("deck-slide.HTM"), "html");
        assert_eq!(kind_for("pitch-deck.html"), "html");
        assert_eq!(kind_for("chart.svg"), "sketch");
        assert_eq!(kind_for("sketch-photo.png"), "sketch");
        assert_eq!(kind_for("photo.png"), "image");
        assert_eq!(kind_for("app.tsx"), "code");
        assert_eq!(kind_for("design.sketch.json"), "sketch");
        assert_eq!(kind_for("model.xyz"), "binary");
    }

    #[test]
    fn list_skips_hidden_ignored_sidecars_and_symlinks() {
        let base = temp_dir("list");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::create_dir_all(base.join("node_modules")).unwrap();
        std::fs::write(base.join("index.html"), "<html></html>").unwrap();
        std::fs::write(base.join(".hidden"), "nope").unwrap();
        std::fs::write(base.join("index.html.artifact.json"), "{}").unwrap();
        std::fs::write(base.join("sub/note.md"), "# note").unwrap();
        std::fs::write(base.join("node_modules/skip.js"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("index.html"), base.join("link.html")).unwrap();

        let files = list_files(&base, None).expect("list");
        let names: Vec<&str> = files
            .iter()
            .map(|file| file.get("name").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(names, vec!["index.html", "sub/note.md"]);

        let first = &files[0];
        assert_eq!(first["type"], "file");
        assert_eq!(first["path"], "index.html");
        assert_eq!(first["kind"], "html");
        assert_eq!(first["mime"], "text/html; charset=utf-8");
        assert_eq!(first["artifactKind"], "html");
        assert!(first["localPath"].as_str().unwrap().ends_with("/index.html"));
        assert!(first.get("artifactManifest").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn since_filters_older_entries() {
        let base = temp_dir("since");
        std::fs::write(base.join("old.html"), "old").unwrap();
        std::fs::write(base.join("new.html"), "new").unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_000);
        let new = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_100_000);
        std::fs::File::options()
            .write(true)
            .open(base.join("old.html"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(base.join("new.html"))
            .unwrap()
            .set_modified(new)
            .unwrap();

        let all = list_files(&base, None).expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["name"], "new.html");
        assert_eq!(all[1]["name"], "old.html");
        assert_eq!(all[1]["mtime"], 1_700_000_000_000.0_f64);

        let recent = list_files(&base, Some(1_700_000_000_500.0)).expect("since");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0]["name"], "new.html");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn text_preview_clamps_limit_and_reports_metadata() {
        let base = temp_dir("preview");
        let body = "x".repeat(4000);
        std::fs::write(base.join("page.html"), &body).unwrap();

        let preview = text_preview(&base, "page.html", false, 2048).expect("preview");
        assert_eq!(preview["truncated"], true);
        assert_eq!(preview["size"], 4000);
        assert_eq!(preview["limit"], 2048);
        assert_eq!(preview["text"].as_str().unwrap().len(), 2048);
        assert_eq!(preview["mime"], "text/html; charset=utf-8");
        assert_eq!(preview["kind"], "html");
        assert_eq!(preview["poweredPreview"]["required"], false);
        assert_eq!(preview["poweredPreview"]["scannedBytes"], 4000);
        assert_eq!(preview["poweredPreview"]["complete"], true);

        let full = text_preview(&base, "page.html", false, 8192).expect("preview");
        assert_eq!(full["truncated"], false);
        assert_eq!(full["text"].as_str().unwrap().len(), 4000);

        assert!(matches!(
            text_preview(&base, "missing.html", false, 1024),
            Err(ProjectPathError::NotFound)
        ));
        assert!(matches!(
            text_preview(&base, "../secret", false, 1024),
            Err(ProjectPathError::Invalid(_))
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn powered_preview_signals_match_typescript_regexes() {
        assert!(html_has_powered_preview_signal(b"const sab = new SharedArrayBuffer(8);"));
        assert!(html_has_powered_preview_signal(b"new Worker('w.js')"));
        assert!(html_has_powered_preview_signal(b"new  SharedWorker ( \"w.js\" )"));
        assert!(!html_has_powered_preview_signal(b"new WorkerX(1)"));
        assert!(html_has_powered_preview_signal(b"self.importScripts('a.js')"));
        assert!(html_has_powered_preview_signal(b"WebAssembly . instantiateStreaming(m)"));
        assert!(html_has_powered_preview_signal(b"WebAssembly.compileStreaming(m)"));
        assert!(!html_has_powered_preview_signal(b"WebAssembly.compileStreamingX(m)"));
        assert!(html_has_powered_preview_signal(b"fetch(m.moduleUrl).wasm"));
        assert!(html_has_powered_preview_signal(b"file.wasm.bin"));
        assert!(html_has_powered_preview_signal(b"canvas.getContext('webgl2')"));
        assert!(!html_has_powered_preview_signal(b"canvas.getContext('2d')"));
        assert!(html_has_powered_preview_signal(b"const c = new OffscreenCanvas(1, 2)"));
        assert!(html_has_powered_preview_signal(b"navigator . gpu"));
        assert!(!html_has_powered_preview_signal(b"navigator.gpuX"));
        assert!(!html_has_powered_preview_signal(b"<p>hello</p>"));
    }

    #[test]
    fn vite_dev_module_script_detection() {
        assert!(has_vite_dev_module_script(
            br#"<script type="module" src="/src/main.ts"></script>"#
        ));
        assert!(has_vite_dev_module_script(
            br#"<script type='module' src='/src/x.ts'>"#
        ));
        assert!(!has_vite_dev_module_script(
            br#"<script src="/src/main.ts" type="module">"#
        ));
        assert!(!has_vite_dev_module_script(
            br#"<script type="module" src="/assets/main.js">"#
        ));
        assert!(!has_vite_dev_module_script(
            br#"<script type="text/javascript" src="/src/main.ts">"#
        ));
    }

    #[test]
    fn sidecar_manifest_validation() {
        let good = r#"{"version":1,"kind":"html","title":"a.html","entry":"a.html","renderer":"html","status":"complete","exports":["html","pdf","zip"],"metadata":{"inferred":true}}"#;
        let manifest = parse_sidecar_manifest(good, "a.html").expect("valid sidecar");
        assert_eq!(manifest["kind"], "html");

        let wrong_version = good.replace("\"version\":1", "\"version\":2");
        assert!(parse_sidecar_manifest(&wrong_version, "a.html").is_none());
        let bad_kind = good.replace("\"kind\":\"html\"", "\"kind\":\"nope\"");
        assert!(parse_sidecar_manifest(&bad_kind, "a.html").is_none());
        let traversal = good.replace("\"entry\":\"a.html\"", "\"entry\":\"../x.html\"");
        assert!(parse_sidecar_manifest(&traversal, "a.html").is_none());

        assert!(infer_legacy_manifest("docs/readme.md").is_some());
        assert!(infer_legacy_manifest("logo.svg").is_some());
        assert!(
            infer_legacy_manifest("sales-pitch.html").unwrap()["kind"] == "deck"
        );
        assert!(infer_legacy_manifest("plain.css").is_none());
    }

    #[test]
    fn file_version_paths_are_detected() {
        assert!(is_project_file_version_path(".file-versions/index.html"));
        assert!(is_project_file_version_path("sub/.file-versions/x"));
        assert!(!is_project_file_version_path("index.html"));
        assert!(!is_project_file_version_path(".live-artifacts/x"));
    }
}
