//! Plain-lane artifact extraction — parity port of
//! `apps/daemon/src/runtimes/plain-stream.ts`.
//!
//! On the plain/BYOK lane the agent's stdout IS the answer, so an artifact the
//! model emits inline (`<artifact identifier=… type=…>…</artifact>`) has to be
//! materialized as a project file by the daemon. This module owns the two pure
//! halves of that: reconstructing the stream from run events
//! (`plainStdoutFromRunEvents`) and pulling the artifact tags back out of it
//! (`extractPlainStreamArtifacts`), plus the naming and manifest rules the
//! persistence step needs.
//!
//! The markdown skip ranges mirror `apps/web/src/artifacts/markdown-context.ts`
//! so headless extraction sees the same fence boundaries as the browser
//! artifact parser: a tag inside a ``` fence is literal text, while a tag in a
//! tilde fence or an indented example is NOT (the web parser only recognises
//! backticks).
//!
//! DOCUMENTED DEVIATIONS
//!
//! * JS `\s` is reproduced exactly (`js_whitespace`, the `JS_WS` regex class)
//!   rather than using Rust's Unicode `White_Space`: the two disagree on
//!   `\u{0085}` (Rust yes, JS no) and `\u{FEFF}` (JS yes, Rust no).
//! * `String::to_lowercase` matches `String.prototype.toLowerCase` for every
//!   script the artifact names in practice use; a handful of exotic case pairs
//!   (Cherokee, a few Greek prosgegrammeni) would differ.
//! * Offsets are UTF-8 byte offsets, not JS UTF-16 code units. Every offset is
//!   only ever compared with, or sliced from, an offset into the same string, so
//!   the two conventions agree; `charAt`-style probes are done on `char`s.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

const OPEN_TAG: &str = "<artifact";
const CLOSE_TAG: &str = "</artifact>";

/// Parity: `MAX_ARTIFACTS_PER_RUN`.
pub const MAX_ARTIFACTS_PER_RUN: usize = 50;

/// One extracted artifact, before it becomes a project file (parity:
/// `PlainStreamArtifact`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainStreamArtifact {
    pub identifier: String,
    pub artifact_type: String,
    pub title: String,
    pub content: String,
    /// One of `.html`, `.css`, `.svg`, `.md`.
    pub extension: &'static str,
    pub file_name: String,
}

/// JS `\s`: `WhiteSpace` ∪ `LineTerminator`. Deliberately not Rust's
/// `char::is_whitespace` — see the module docblock.
const JS_WS: &str = r"[\t\n\x0B\x0C\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";

pub(crate) fn js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n'
            | '\u{000B}'
            | '\u{000C}'
            | '\r'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn js_blank(text: &str) -> bool {
    text.chars().all(js_whitespace)
}

/// Parity: `plainStdoutFromRunEvents` — only `stdout` events carry the raw
/// stream; a conversation's text deltas are the parsed view of it and would
/// double-count an artifact.
pub fn plain_stdout_from_events(events: &[Value]) -> String {
    let mut stdout = String::new();
    for record in events {
        let Some(record) = record.as_object() else {
            continue;
        };
        if record.get("event").and_then(Value::as_str) != Some("stdout") {
            continue;
        }
        let chunk = record
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| data.get("chunk"))
            .and_then(Value::as_str);
        if let Some(chunk) = chunk {
            stdout.push_str(chunk);
        }
    }
    stdout
}

/// Parity: `extractPlainStreamArtifacts`.
pub fn extract_plain_stream_artifacts(stdout: &str) -> Vec<PlainStreamArtifact> {
    if !stdout.contains(OPEN_TAG) {
        return Vec::new();
    }
    let skip_ranges = compute_markdown_skip_ranges(stdout);
    let mut artifacts = Vec::new();
    let mut from = 0usize;

    while from < stdout.len() && artifacts.len() < MAX_ARTIFACTS_PER_RUN {
        let Some(open_start) = find_next_artifact_open(stdout, from, &skip_ranges) else {
            break;
        };
        let next_open = find_next_artifact_open(stdout, open_start + OPEN_TAG.len(), &skip_ranges);
        let Some(open_end) = find_open_tag_end(stdout, open_start + OPEN_TAG.len()) else {
            from = open_start + OPEN_TAG.len();
            continue;
        };
        if let Some(next_open) = next_open {
            if next_open < open_end {
                from = next_open;
                continue;
            }
        }
        let Some(close_start) = stdout[open_end..].find(CLOSE_TAG).map(|rel| open_end + rel) else {
            from = open_start + OPEN_TAG.len();
            continue;
        };
        if let Some(next_open) = next_open {
            if next_open < close_start {
                from = next_open;
                continue;
            }
        }

        let attr_text = &stdout[open_start + OPEN_TAG.len()..open_end - 1];
        let attrs = parse_attrs(attr_text);
        let content = &stdout[open_end..close_start];
        let artifact_type = normalize_artifact_type(attrs.get("type").and_then(Value::as_str), content);
        let extension = artifact_type.as_deref().and_then(extension_for);
        if let (Some(artifact_type), Some(extension)) = (artifact_type, extension) {
            let title = attrs
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let identifier = attrs
                .get("identifier")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            artifacts.push(PlainStreamArtifact {
                file_name: format!(
                    "{}{extension}",
                    artifact_base_name_for(&identifier, &title)
                ),
                identifier,
                artifact_type,
                title,
                content: content.to_string(),
                extension,
            });
        }
        from = close_start + CLOSE_TAG.len();
    }

    artifacts
}

/// Parity: `artifactBaseNameFor`.
pub fn artifact_base_name_for(identifier: &str, title: &str) -> String {
    let source = if !identifier.is_empty() {
        identifier
    } else if !title.is_empty() {
        title
    } else {
        "artifact"
    };
    let mut sanitized = String::new();
    let mut in_invalid_run = false;
    for ch in source.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_' {
            sanitized.push(ch);
            in_invalid_run = false;
        } else if !in_invalid_run {
            sanitized.push('-');
            in_invalid_run = true;
        }
    }
    let trimmed = sanitized.trim_matches('-');
    let sliced: String = trimmed.chars().take(60).collect();
    if sliced.is_empty() {
        "artifact".to_string()
    } else {
        sliced
    }
}

/// Parity: `reserveUniqueArtifactFileName`. The caller owns the reservation
/// set so every file in one persistence pass sees the others' picks.
pub fn reserve_unique_artifact_file_name(
    desired_name: &str,
    reserved_names: &mut HashSet<String>,
) -> Result<String, String> {
    if reserved_names.insert(desired_name.to_string()) {
        return Ok(desired_name.to_string());
    }
    let dot = desired_name.rfind('.');
    let (stem, ext) = match dot {
        Some(dot) => (&desired_name[..dot], &desired_name[dot..]),
        None => (desired_name, ""),
    };
    let mut suffix = 2u32;
    while suffix < 10_000 {
        let candidate = format!("{stem}-{suffix}{ext}");
        if reserved_names.insert(candidate.clone()) {
            return Ok(candidate);
        }
        suffix += 1;
    }
    Err(format!(
        "could not allocate artifact filename for {desired_name}"
    ))
}

/// Parity: `artifactManifestFor`.
pub fn artifact_manifest_for(artifact: &PlainStreamArtifact, entry: &str) -> Value {
    let title = if !artifact.title.is_empty() {
        artifact.title.clone()
    } else if !artifact.identifier.is_empty() {
        artifact.identifier.clone()
    } else {
        entry.to_string()
    };
    let metadata = json!({
        "identifier": artifact.identifier,
        "artifactType": artifact.artifact_type,
        "inferred": false,
    });

    match artifact.extension {
        ".html" => json!({
            "kind": "html",
            "title": title,
            "entry": entry,
            "renderer": "html",
            "status": "complete",
            "exports": ["html", "pdf", "zip"],
            "primary": true,
            "metadata": metadata,
        }),
        ".css" => json!({
            "kind": "code-snippet",
            "title": title,
            "entry": entry,
            "renderer": "code",
            "status": "complete",
            "exports": ["txt", "zip"],
            "metadata": metadata,
        }),
        ".svg" => json!({
            "kind": "svg",
            "title": title,
            "entry": entry,
            "renderer": "svg",
            "status": "complete",
            "exports": ["svg", "zip"],
            "metadata": metadata,
        }),
        _ => json!({
            "kind": "markdown-document",
            "title": title,
            "entry": entry,
            "renderer": "markdown",
            "status": "complete",
            "exports": ["md", "html", "pdf", "zip"],
            "metadata": metadata,
        }),
    }
}

/// Parity: `PLAIN_ARTIFACT_STDOUT_CAP` — the head window the run keeps for
/// artifact extraction (8 MiB).
pub const PLAIN_ARTIFACT_STDOUT_CAP: usize = 8 * 1024 * 1024;

/// Append `chunk` to the bounded head window, truncating on a UTF-8 boundary
/// when the cap is crossed. The TS original slices UTF-16 code units; Rust
/// tracks UTF-8 bytes. Both are self-consistent seam units, and the stitched
/// string is the same text either way.
pub fn append_plain_artifact_head(head: &mut String, chunk: &str) {
    if head.len() >= PLAIN_ARTIFACT_STDOUT_CAP {
        return;
    }
    head.push_str(chunk);
    if head.len() > PLAIN_ARTIFACT_STDOUT_CAP {
        let mut end = PLAIN_ARTIFACT_STDOUT_CAP;
        while end > 0 && !head.is_char_boundary(end) {
            end -= 1;
        }
        head.truncate(end);
    }
}

/// Parity: the finalize-time stitch of the head window and the event tail.
/// Returns the windows to extract from: one when the windows overlap or are
/// contiguous, two when a true gap separates them. Byte offsets are used
/// consistently for `head` and `tail`, so the seam removes exactly the
/// double-covered span.
pub fn plain_stdout_windows(head: &str, tail: &str, total_bytes: u64) -> Vec<String> {
    if head.is_empty() {
        return vec![tail.to_string()];
    }
    let head_len = head.len() as u64;
    let tail_start = total_bytes.saturating_sub(tail.len() as u64);
    if tail_start <= head_len {
        let skip = (head_len - tail_start) as usize;
        let skip = skip.min(tail.len());
        let mut stitched = String::with_capacity(head.len() + tail.len() - skip);
        stitched.push_str(head);
        stitched.push_str(&tail[skip..]);
        vec![stitched]
    } else {
        vec![head.to_string(), tail.to_string()]
    }
}

/// One persisted artifact (parity: `PersistedPlainStreamArtifact`).
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedPlainArtifact {
    pub identifier: String,
    pub artifact_type: String,
    pub title: String,
    pub name: String,
    pub file: Value,
}

/// Parity: `persistPlainStreamArtifactList` — reserve a unique file name per
/// artifact against the project's current inventory, then write the file and
/// its manifest sidecar. `overwrite: false` makes a name collision a hard
/// error rather than a silent replacement.
pub fn persist_plain_stream_artifact_list(
    base: &std::path::Path,
    artifacts: &[PlainStreamArtifact],
    existing_names: &[String],
) -> Result<Vec<PersistedPlainArtifact>, crate::project_files::WriteProjectFileError> {
    if artifacts.is_empty() {
        return Ok(Vec::new());
    }
    let mut reserved: HashSet<String> = existing_names.iter().cloned().collect();
    let mut persisted = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let name = reserve_unique_artifact_file_name(&artifact.file_name, &mut reserved)
            .map_err(|message| {
                crate::project_files::WriteProjectFileError::Io(std::io::Error::other(message))
            })?;
        let manifest = artifact_manifest_for(artifact, &name);
        let file = crate::project_files::write_project_file(
            base,
            &name,
            artifact.content.as_bytes(),
            false,
            Some(&manifest),
        )?;
        persisted.push(PersistedPlainArtifact {
            identifier: artifact.identifier.clone(),
            artifact_type: artifact.artifact_type.clone(),
            title: artifact.title.clone(),
            name,
            file,
        });
    }
    Ok(persisted)
}

/// Parity: the `TYPE_TO_EXTENSION` table.
fn extension_for(artifact_type: &str) -> Option<&'static str> {
    Some(match artifact_type {
        "html" | "text/html" => ".html",
        "css" | "text/css" => ".css",
        "svg" | "image/svg+xml" => ".svg",
        "markdown" | "md" | "text/markdown" | "text/x-markdown" => ".md",
        _ => return None,
    })
}

/// Parity: `normalizeArtifactType`.
fn normalize_artifact_type(raw_type: Option<&str>, content: &str) -> Option<String> {
    let normalized = raw_type.map(|raw| raw.trim().to_lowercase());
    if let Some(normalized) = normalized
        .as_deref()
        .filter(|normalized| !normalized.is_empty())
    {
        if extension_for(normalized).is_some() {
            return Some(normalized.to_string());
        }
    }
    if normalized.as_deref().is_none_or(str::is_empty) && html_document_re().is_match(content) {
        return Some("text/html".to_string());
    }
    None
}

/// Parity: `parseAttrs` — a `key="value"` / `key='value'` scan where the last
/// occurrence of a duplicate key wins, as `RegExp.exec` in a loop would.
fn parse_attrs(raw: &str) -> Map<String, Value> {
    let mut attrs = Map::new();
    for captures in attr_re().captures_iter(raw) {
        let name = captures
            .get(1)
            .map(|value| value.as_str().to_string())
            .unwrap_or_default();
        let value = captures
            .get(2)
            .or_else(|| captures.get(3))
            .map(|value| value.as_str().to_string())
            .unwrap_or_default();
        attrs.insert(name, Value::String(value));
    }
    attrs
}

/// Parity: `findNextArtifactOpen`.
fn find_next_artifact_open(
    text: &str,
    from: usize,
    skip_ranges: &[(usize, usize)],
) -> Option<usize> {
    let mut current = from;
    while current < text.len() {
        let idx = text[current..].find(OPEN_TAG).map(|rel| current + rel)?;
        if !is_real_artifact_open_at(text, idx) || range_contains(skip_ranges, idx) {
            current = idx + OPEN_TAG.len();
            continue;
        }
        return Some(idx);
    }
    None
}

/// Parity: `isRealArtifactOpenAt` — `<artifactfoo` is prose about the tag, not
/// a tag.
fn is_real_artifact_open_at(text: &str, idx: usize) -> bool {
    text[idx + OPEN_TAG.len()..]
        .chars()
        .next()
        .is_some_and(js_whitespace)
}

/// Parity: `findOpenTagEnd` — quote-aware, so a `>` inside an attribute value
/// does not end the tag.
fn find_open_tag_end(text: &str, from: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (offset, ch) in text[from..].char_indices() {
        let index = from + offset;
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch == '>' {
            return Some(index + 1);
        }
    }
    None
}

/// Parity: `computeMarkdownSkipRanges`.
fn compute_markdown_skip_ranges(text: &str) -> Vec<(usize, usize)> {
    let fences = compute_markdown_fence_ranges(text);
    let mut ranges = fences.clone();
    ranges.extend(compute_markdown_inline_code_ranges(text, &fences));
    ranges
}

/// Parity: `computeMarkdownFenceRanges` — an unterminated open fence runs to
/// the end of the stream, which is what makes a truncated ``` block safe.
fn compute_markdown_fence_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0usize;
    let mut fence_start: Option<usize> = None;
    while pos < text.len() {
        let eol = text[pos..].find('\n').map(|rel| pos + rel);
        let line_end = eol.unwrap_or(text.len());
        let line = &text[pos..line_end];
        let line_has_newline = eol.is_some();

        if fence_start.is_none() {
            if line_has_newline && fence_open_re().is_match(line) {
                fence_start = Some(pos);
            }
        } else if line_has_newline && fence_close_re().is_match(line) {
            ranges.push((fence_start.expect("open fence"), eol.expect("newline") + 1));
            fence_start = None;
        }

        if !line_has_newline {
            break;
        }
        pos = eol.expect("newline") + 1;
    }
    if let Some(start) = fence_start {
        ranges.push((start, text.len()));
    }
    ranges
}

/// Parity: `computeMarkdownInlineCodeRanges` — backtick runs only pair inside
/// the same block, and never inside a fence.
fn compute_markdown_inline_code_ranges(
    text: &str,
    fence_ranges: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (start, end) in compute_markdown_inline_block_ranges(text, fence_ranges) {
        let mut i = start;
        while i < end {
            let Some(idx) = text[i..]
                .find('`')
                .map(|rel| i + rel)
                .filter(|idx| *idx < end)
            else {
                break;
            };
            let tick_count = count_backtick_run(text, idx);
            let close =
                find_matching_backtick_run(text, idx + tick_count, tick_count, fence_ranges, end);
            let Some(close) = close else {
                i = idx + tick_count;
                continue;
            };
            ranges.push((idx, close + tick_count));
            i = close + tick_count;
        }
    }
    ranges
}

/// Parity: `computeMarkdownInlineBlockRanges` — headings and list items are
/// their own one-line block, and a blank line ends the block.
fn compute_markdown_inline_block_ranges(
    text: &str,
    fence_ranges: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0usize;
    let mut block_start: Option<usize> = None;

    while pos < text.len() {
        if let Some(fence_range) = range_containing(fence_ranges, pos) {
            close_block(&mut block_start, pos, &mut ranges);
            pos = fence_range.1;
            continue;
        }

        let eol = text[pos..].find('\n').map(|rel| pos + rel);
        let line_end = eol.unwrap_or(text.len());
        let line = &text[pos..line_end];

        if js_blank(line) {
            close_block(&mut block_start, pos, &mut ranges);
        } else if heading_re().is_match(line)
            || ul_item_re().is_match(line)
            || ol_item_re().is_match(line)
        {
            close_block(&mut block_start, pos, &mut ranges);
            ranges.push((pos, line_end));
        } else if block_start.is_none() {
            block_start = Some(pos);
        }

        pos = match eol {
            Some(eol) => eol + 1,
            None => text.len(),
        };
    }

    close_block(&mut block_start, text.len(), &mut ranges);
    ranges
}

fn close_block(block_start: &mut Option<usize>, idx: usize, ranges: &mut Vec<(usize, usize)>) {
    if let Some(start) = block_start.take() {
        if idx > start {
            ranges.push((start, idx));
        }
    }
}

/// Parity: `countBacktickRun`.
fn count_backtick_run(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0usize;
    while from + count < bytes.len() && bytes[from + count] == b'`' {
        count += 1;
    }
    count
}

/// Parity: `findMatchingBacktickRun`.
fn find_matching_backtick_run(
    text: &str,
    from: usize,
    tick_count: usize,
    fence_ranges: &[(usize, usize)],
    until: usize,
) -> Option<usize> {
    let mut i = from;
    while i < until {
        let idx = text[i..]
            .find('`')
            .map(|rel| i + rel)
            .filter(|idx| *idx < until)?;
        if range_contains(fence_ranges, idx) {
            i = idx + 1;
            continue;
        }
        let candidate_count = count_backtick_run(text, idx);
        if candidate_count == tick_count {
            return Some(idx);
        }
        i = idx + candidate_count;
    }
    None
}

fn range_containing(ranges: &[(usize, usize)], idx: usize) -> Option<(usize, usize)> {
    ranges
        .iter()
        .find(|(start, end)| idx >= *start && idx < *end)
        .copied()
}

fn range_contains(ranges: &[(usize, usize)], idx: usize) -> bool {
    ranges
        .iter()
        .any(|(start, end)| idx >= *start && idx < *end)
}

/// Parity: `looksLikeHtmlDocument` (`/^\s*(?:<!doctype\s+html\b|<html\b)/i`).
fn html_document_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?i)^{JS_WS}*(?:<!doctype{JS_WS}+html\b|<html\b)"
        ))
        .expect("html document regex")
    })
}

/// Parity: `FENCE_OPEN_RE` (`/^```(\w[\w+-]*)?\s*$/`).
fn fence_open_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"^```(?:[A-Za-z0-9_][A-Za-z0-9_+\-]*)?{JS_WS}*$"
        ))
        .expect("fence open regex")
    })
}

/// Parity: `FENCE_CLOSE_RE` (`/^```\s*$/`).
fn fence_close_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"^```{JS_WS}*$")).expect("fence close regex"))
}

/// Parity: `HEADING_RE` (`/^#{1,4}\s+/`).
fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"^#{{1,4}}{JS_WS}+")).expect("heading regex"))
}

/// Parity: `UL_ITEM_RE` (`/^\s*[-*+]\s+/`).
fn ul_item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"^{JS_WS}*[-*+]{JS_WS}+")).expect("ul item regex"))
}

/// Parity: `OL_ITEM_RE` (`/^\s*\d+\.\s+/`).
fn ol_item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"^{JS_WS}*[0-9]+\.{JS_WS}+")).expect("ol item regex"))
}

/// Parity: the `parseAttrs` scan regex.
fn attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"([A-Za-z_:][A-Za-z0-9_:.\-]*)\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("attr regex")
    })
}

#[cfg(test)]
mod plain_stream_tests {
    use super::*;

    fn names(artifacts: &[PlainStreamArtifact]) -> Vec<&str> {
        artifacts.iter().map(|a| a.file_name.as_str()).collect()
    }

    /// Parity: `plain-stream.test.ts:14`.
    #[test]
    fn extracts_and_names_artifact_tags_from_plain_stdout() {
        let stdout = concat!(
            "Here is the result:\n",
            r#"<artifact identifier="flowmind-landing" type="text/html" title="FlowMind Landing">"#,
            "<!doctype html><html><body><h1>FlowMind</h1></body></html>",
            "</artifact>",
            "\nDone.",
        );
        let artifacts = extract_plain_stream_artifacts(stdout);
        assert_eq!(artifacts.len(), 1);
        let artifact = &artifacts[0];
        assert_eq!(artifact.file_name, "flowmind-landing.html");
        assert_eq!(artifact.identifier, "flowmind-landing");
        assert_eq!(artifact.artifact_type, "text/html");
        assert_eq!(artifact.title, "FlowMind Landing");
        assert_eq!(artifact.extension, ".html");
        assert_eq!(
            artifact.content,
            "<!doctype html><html><body><h1>FlowMind</h1></body></html>"
        );
        assert_eq!(
            artifact_manifest_for(artifact, "flowmind-landing.html"),
            json!({
                "kind": "html",
                "title": "FlowMind Landing",
                "entry": "flowmind-landing.html",
                "renderer": "html",
                "status": "complete",
                "exports": ["html", "pdf", "zip"],
                "primary": true,
                "metadata": {
                    "identifier": "flowmind-landing",
                    "artifactType": "text/html",
                    "inferred": false,
                },
            })
        );
    }

    /// Parity: `plain-stream.test.ts:62`.
    #[test]
    fn no_artifact_tags_yield_nothing() {
        assert!(extract_plain_stream_artifacts("plain answer with no file output").is_empty());
        assert!(extract_plain_stream_artifacts("").is_empty());
        assert!(extract_plain_stream_artifacts("a <artifact-free>answer</artifact-free>").is_empty());
    }

    /// Parity: `plain-stream.test.ts:79` — a typeless html body is inferred, and
    /// a colliding name is reserved a suffix.
    #[test]
    fn infers_html_without_a_type_and_avoids_filename_collisions() {
        let stdout = concat!(
            r#"<artifact identifier="landing" title="Landing">"#,
            "<!doctype html><html><body>New</body></html>",
            "</artifact>",
        );
        let artifacts = extract_plain_stream_artifacts(stdout);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].artifact_type, "text/html");
        assert_eq!(artifacts[0].file_name, "landing.html");

        let mut reserved: HashSet<String> = HashSet::new();
        reserved.insert("landing.html".to_string());
        assert_eq!(
            reserve_unique_artifact_file_name("landing.html", &mut reserved).unwrap(),
            "landing-2.html"
        );
        assert_eq!(
            reserve_unique_artifact_file_name("landing.html", &mut reserved).unwrap(),
            "landing-3.html"
        );
        // A fresh name is taken as-is.
        assert_eq!(
            reserve_unique_artifact_file_name("brand.css", &mut reserved).unwrap(),
            "brand.css"
        );
        // Extensionless names still get a suffix.
        assert_eq!(
            reserve_unique_artifact_file_name(
                "notes",
                &mut HashSet::from(["notes".to_string()])
            )
            .unwrap(),
            "notes-2"
        );
    }

    /// Parity: `plain-stream.test.ts:118`.
    #[test]
    fn ignores_bare_artifact_tags() {
        let artifacts = extract_plain_stream_artifacts(
            [
                "<artifact><!doctype html><html><body>Bare</body></html></artifact>",
                r#"<artifact identifier="real" type="text/html"><!doctype html><html><body>Real</body></html></artifact>"#,
            ]
            .join("\n")
            .as_str(),
        );
        assert_eq!(names(&artifacts), vec!["real.html"]);
        assert_eq!(artifacts[0].content, "<!doctype html><html><body>Real</body></html>");
    }

    /// Parity: `plain-stream.test.ts:133`.
    #[test]
    fn maps_supported_types_to_stable_project_file_names() {
        let artifacts = extract_plain_stream_artifacts(
            [
                r#"<artifact identifier="theme" type="text/css">body { color: red; }</artifact>"#,
                r#"<artifact identifier="logo" type="image/svg+xml"><svg /></artifact>"#,
                r#"<artifact identifier="brief" type="text/markdown"># Brief</artifact>"#,
            ]
            .join("\n")
            .as_str(),
        );
        assert_eq!(names(&artifacts), vec!["theme.css", "logo.svg", "brief.md"]);
        let types: Vec<&str> = artifacts.iter().map(|a| a.artifact_type.as_str()).collect();
        assert_eq!(types, vec!["text/css", "image/svg+xml", "text/markdown"]);
        // Short forms and the md aliases map to the same extensions.
        assert_eq!(extension_for("html"), Some(".html"));
        assert_eq!(extension_for("md"), Some(".md"));
        assert_eq!(extension_for("text/x-markdown"), Some(".md"));
        assert_eq!(extension_for("application/json"), None);
    }

    /// Parity: `plain-stream.test.ts:149`.
    #[test]
    fn reconstructs_only_plain_stdout_events_and_ignores_literal_code_fence_examples() {
        let events = vec![
            json!({"event": "agent", "data": {"type": "text_delta", "delta": "<artifact type=\"text/html\">no</artifact>"}}),
            json!({"event": "stdout", "data": {"chunk": "```html\n<artifact type=\"text/html\">example</artifact>\n```\n"}}),
            json!({"event": "stdout", "data": {"chunk": "<artifact type=\"text/html\"><!doctype html><html></html></artifact>"}}),
        ];
        let stdout = plain_stdout_from_events(&events);
        assert!(stdout.starts_with("```html\n"));
        let artifacts = extract_plain_stream_artifacts(&stdout);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].content, "<!doctype html><html></html>");
    }

    /// Parity: `plain-stream.test.ts:166` — an indented example still counts
    /// (the web parser only recognises an unindented ``` line).
    #[test]
    fn extracts_artifact_tags_inside_indented_backtick_examples() {
        let artifacts = extract_plain_stream_artifacts(
            [
                "- Literal example:\n",
                "   ```html\n",
                "   <artifact type=\"text/html\"><!doctype html><html><body>Example</body></html></artifact>\n",
                r#"<artifact identifier="real" type="text/html"><!doctype html><html><body>Real</body></html></artifact>"#,
            ]
            .join("")
            .as_str(),
        );
        assert_eq!(names(&artifacts), vec!["artifact.html", "real.html"]);
        assert_eq!(artifacts[0].content, "<!doctype html><html><body>Example</body></html>");
        assert_eq!(artifacts[1].content, "<!doctype html><html><body>Real</body></html>");
    }

    /// Parity: `plain-stream.test.ts:184` — tilde fences are not fences to the
    /// web parser, so their contents are still extracted.
    #[test]
    fn extracts_artifact_tags_inside_tilde_fences() {
        let artifacts = extract_plain_stream_artifacts(
            [
                "~~~html\n",
                "<artifact type=\"text/html\"><!doctype html><html><body>Tilde</body></html></artifact>\n",
                "~~~\n",
                r#"<artifact identifier="real" type="text/html"><!doctype html><html><body>Real</body></html></artifact>"#,
            ]
            .join("")
            .as_str(),
        );
        assert_eq!(names(&artifacts), vec!["artifact.html", "real.html"]);
        assert_eq!(artifacts[0].content, "<!doctype html><html><body>Tilde</body></html>");
    }

    /// Parity: `plain-stream.test.ts:202`.
    #[test]
    fn ignores_artifact_tags_inside_inline_markdown_code_spans() {
        let artifacts = extract_plain_stream_artifacts(
            [
                "Use `<artifact identifier=\"example\" type=\"text/html\">example</artifact>` to emit HTML.\n",
                r#"<artifact identifier="real" type="text/html"><!doctype html><html><body>Real</body></html></artifact>"#,
            ]
            .join("")
            .as_str(),
        );
        assert_eq!(names(&artifacts), vec!["real.html"]);
    }

    /// Parity: `plain-stream.test.ts:216`.
    #[test]
    fn resyncs_past_prose_artifact_openers() {
        let artifacts = extract_plain_stream_artifacts(
            [
                "Use <artifact type=\"text/html\"> in prose before emitting the real artifact.\n",
                r#"<artifact identifier="real" type="text/html">"#,
                "<!doctype html><html><body>Real</body></html>",
                "</artifact>",
            ]
            .join("")
            .as_str(),
        );
        assert_eq!(names(&artifacts), vec!["real.html"]);
        assert_eq!(artifacts[0].content, "<!doctype html><html><body>Real</body></html>");
    }

    /// Parity: `plain-stream.test.ts:230`.
    #[test]
    fn resyncs_past_malformed_artifact_openers() {
        let artifacts = extract_plain_stream_artifacts(
            [
                "Malformed protocol example: <artifact type=\"text/html\"\n",
                r#"<artifact identifier="real" type="text/html">"#,
                "<!doctype html><html><body>Real</body></html>",
                "</artifact>",
            ]
            .join("")
            .as_str(),
        );
        assert_eq!(names(&artifacts), vec!["real.html"]);
    }

    /// Parity: `plain-stream.test.ts:245` — an unmatched backtick does not run
    /// past its own block.
    #[test]
    fn does_not_let_unmatched_backticks_hide_artifact_tags_in_later_paragraphs() {
        let artifacts = extract_plain_stream_artifacts(
            [
                "Intro with a stray ` backtick.",
                "",
                r#"<artifact identifier="real" type="text/html"><!doctype html><html><body>Real</body></html></artifact>"#,
                "",
                "Another stray ` backtick later.",
            ]
            .join("\n")
            .as_str(),
        );
        assert_eq!(names(&artifacts), vec!["real.html"]);
    }

    /// The artifact budget is per run, not per stream.
    #[test]
    fn stops_after_the_per_run_artifact_budget() {
        let unit = r#"<artifact type="text/html"><!doctype html><html></html></artifact>"#;
        let stdout = unit.repeat(MAX_ARTIFACTS_PER_RUN + 5);
        assert_eq!(
            extract_plain_stream_artifacts(&stdout).len(),
            MAX_ARTIFACTS_PER_RUN
        );
    }

    /// Parity: `artifactBaseNameFor` — sanitised, capped, and never empty.
    #[test]
    fn artifact_base_names_are_sanitised_capped_and_never_empty() {
        assert_eq!(artifact_base_name_for("FlowMind Landing", ""), "flowmind-landing");
        assert_eq!(artifact_base_name_for("", "My Design System"), "my-design-system");
        assert_eq!(artifact_base_name_for("", ""), "artifact");
        assert_eq!(artifact_base_name_for("!!!", ""), "artifact");
        assert_eq!(artifact_base_name_for("a/b\\c", ""), "a-b-c");
        assert_eq!(artifact_base_name_for("__keep__", ""), "__keep__");
        assert_eq!(artifact_base_name_for("--trim--", ""), "trim");
        let long = artifact_base_name_for(&"x".repeat(80), "");
        assert_eq!(long.len(), 60);
        assert!(long.chars().all(|ch| ch == 'x'));
    }

    /// Parity: `artifactManifestFor` for the non-html extensions.
    #[test]
    fn manifests_match_the_artifact_kind() {
        let css = extract_plain_stream_artifacts(
            r#"<artifact identifier="theme" type="text/css">body {}</artifact>"#,
        );
        let manifest = artifact_manifest_for(&css[0], "theme.css");
        assert_eq!(manifest["kind"], "code-snippet");
        assert_eq!(manifest["renderer"], "code");
        assert_eq!(manifest["exports"], json!(["txt", "zip"]));
        assert_eq!(manifest["metadata"]["artifactType"], "text/css");

        let svg = extract_plain_stream_artifacts(
            r#"<artifact identifier="logo" type="image/svg+xml"><svg /></artifact>"#,
        );
        assert_eq!(artifact_manifest_for(&svg[0], "logo.svg")["kind"], "svg");

        // A title-less, identifier-less artifact falls back to the entry name.
        let bare =
            extract_plain_stream_artifacts(r#"<artifact type="text/html"><!doctype html></artifact>"#);
        let manifest = artifact_manifest_for(&bare[0], "artifact.html");
        assert_eq!(manifest["title"], "artifact.html");
    }

    /// Parity: `plainStdoutFromRunEvents` ignores malformed records.
    #[test]
    fn plain_stdout_ignores_malformed_records() {
        let events = vec![
            json!("not an object"),
            json!({"event": "stdout"}),
            json!({"event": "stdout", "data": {"chunk": 42}}),
            json!({"data": {"chunk": "no event"}}),
            json!({"event": "stdout", "data": {"chunk": "kept"}}),
        ];
        assert_eq!(plain_stdout_from_events(&events), "kept");
    }

    #[test]
    fn open_tag_end_is_quote_aware() {
        // The `>` inside the quoted value does not end the tag.
        assert_eq!(find_open_tag_end(r#" type="a>b""#, 0), None);
        assert_eq!(find_open_tag_end(r#" type="a>b">"#, 0), Some(12));
        assert_eq!(find_open_tag_end(r#" type='a>b'>"#, 0), Some(12));
        // A quote that never closes hides the terminator entirely.
        assert_eq!(find_open_tag_end(r#" type="a>b'"#, 0), None);
        assert_eq!(find_open_tag_end(" type=", 0), None);
        assert_eq!(find_open_tag_end(">", 0), Some(1));
    }

    #[test]
    fn stdout_windows_stitch_overlap_and_split_gaps() {
        // Whole stream already fits the head window: tail is a suffix of it.
        assert_eq!(
            plain_stdout_windows("abcdef", "ef", 6),
            vec!["abcdef".to_string()]
        );
        // Tail covers the head exactly.
        assert_eq!(
            plain_stdout_windows("abcdef", "abcdef", 6),
            vec!["abcdef".to_string()]
        );
        // Overlap: the head keeps the first 6 bytes of a 10-byte stream.
        assert_eq!(
            plain_stdout_windows("abcdef", "cdefghij", 10),
            vec!["abcdefghij".to_string()]
        );
        // Gap: the tail starts after the head ends, so both windows extract.
        assert_eq!(
            plain_stdout_windows("abc", "ghij", 10),
            vec!["abc".to_string(), "ghij".to_string()]
        );
        // Empty head falls back to the tail.
        assert_eq!(
            plain_stdout_windows("", "ghij", 10),
            vec!["ghij".to_string()]
        );
    }

    #[test]
    fn head_window_caps_on_a_char_boundary() {
        let mut head = String::new();
        append_plain_artifact_head(&mut head, "ok");
        assert_eq!(head, "ok");
        let big = "é".repeat(PLAIN_ARTIFACT_STDOUT_CAP);
        append_plain_artifact_head(&mut head, &big);
        assert!(head.len() <= PLAIN_ARTIFACT_STDOUT_CAP);
        assert!(head.is_char_boundary(head.len()));
        // Further chunks are ignored once the cap is reached.
        append_plain_artifact_head(&mut head, "more");
        assert!(head.len() <= PLAIN_ARTIFACT_STDOUT_CAP);
    }

    #[test]
    fn persists_artifacts_with_unique_names_and_manifests() {
        let base = std::env::temp_dir().join(format!(
            "od-plain-persist-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let artifacts = extract_plain_stream_artifacts(concat!(
            r#"<artifact identifier="hero" type="text/html" title="Hero">"#,
            "<html><body>one</body></html>",
            "</artifact>",
            r#"<artifact identifier="hero" type="text/html" title="Hero">"#,
            "<html><body>two</body></html>",
            "</artifact>",
        ));
        assert_eq!(artifacts.len(), 2);

        let persisted =
            persist_plain_stream_artifact_list(&base, &artifacts, &["hero.html".to_string()])
                .expect("persist");
        assert_eq!(
            persisted.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["hero-2.html", "hero-3.html"]
        );
        for artifact in &persisted {
            let body = std::fs::read_to_string(base.join(&artifact.name)).expect("body");
            assert!(body.contains("html"));
            let manifest: Value = serde_json::from_str(
                &std::fs::read_to_string(base.join(format!("{}.artifact.json", artifact.name)))
                    .expect("manifest"),
            )
            .expect("manifest json");
            assert_eq!(manifest["version"], json!(1));
            assert_eq!(manifest["kind"], json!("html"));
            assert_eq!(manifest["entry"], json!(artifact.name));
            assert!(manifest["createdAt"].as_str().is_some());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn js_whitespace_matches_javascript_semantics() {
        assert!(js_whitespace('\u{FEFF}'));
        assert!(js_whitespace('\u{2028}'));
        assert!(js_whitespace(' '));
        assert!(!js_whitespace('\u{0085}'));
        assert!(!js_whitespace('a'));
    }
}
